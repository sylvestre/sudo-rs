use std::collections::VecDeque;
use std::ffi::c_int;
use std::fs::File;
use std::io;
use std::process::{exit, Command};

use signal_hook::consts::*;

use crate::exec::event::{EventClosure, EventDispatcher, StopReason};
use crate::exec::use_pty::monitor::exec_monitor;
use crate::exec::{cond_fmt, opt_fmt, signal_fmt};
use crate::exec::{
    io_util::{retry_while_interrupted, was_interrupted},
    use_pty::backchannel::{BackchannelPair, MonitorMessage, ParentBackchannel, ParentMessage},
    ExitReason,
};
use crate::log::{dev_error, dev_info, dev_warn};
use crate::system::signal::{SignalAction, SignalHandler};
use crate::system::term::{tcgetpgrp, Pty, UserTerm};
use crate::system::wait::{waitpid, WaitError, WaitOptions};
use crate::system::{chown, fork, Group, User};
use crate::system::{getpgid, interface::ProcessId, signal::SignalInfo};

use super::pipe::Pipe;

pub(crate) fn exec_pty(
    sudo_pid: ProcessId,
    mut command: Command,
    mut user_tty: UserTerm,
) -> io::Result<(ExitReason, Box<dyn FnOnce()>)> {
    // Allocate a pseudoterminal.
    let pty = get_pty()?;

    // Create backchannels to communicate with the monitor.
    let mut backchannels = BackchannelPair::new().map_err(|err| {
        dev_error!("unable to create backchannel: {err}");
        err
    })?;

    // We don't want to receive SIGTTIN/SIGTTOU
    // FIXME: why?
    if let Err(err) = SignalHandler::with_action(SIGTTIN, SignalAction::Ignore) {
        dev_error!("unable to set handler for SIGTTIN: {err}");
    }
    if let Err(err) = SignalHandler::with_action(SIGTTOU, SignalAction::Ignore) {
        dev_error!("unable to set handler for SIGTTOU: {err}");
    }

    // FIXME (ogsudo): Initialize the policy plugin's session here by calling
    // `policy_init_session`.
    // FIXME (ogsudo): initializes ttyblock sigset here by calling `init_ttyblock`

    // Fetch the parent process group so we can signals to it.

    // FIXME: ogsudo never handles this error explicitly.
    let parent_pgrp = getpgid(0).unwrap_or(-1);

    // Set all the IO streams for the command to the follower side of the pty.
    let clone_follower = || {
        pty.follower.try_clone().map_err(|err| {
            dev_error!("cannot clone pty follower: {err}");
            err
        })
    };

    command.stdin(clone_follower()?);
    command.stdout(clone_follower()?);
    command.stderr(clone_follower()?);

    let mut dispatcher = EventDispatcher::<ParentClosure>::new()?;

    //  Read from `/dev/tty` and write to the leader if not in the background.
    let tty_pipe = Pipe::new();
    dispatcher.set_read_callback(&user_tty, |parent, _| {
        parent.tty_pipe.on_read(&mut parent.user_tty)
    });
    dispatcher.set_write_callback(&pty.leader, |parent, _| {
        parent.tty_pipe.on_write(&mut parent.pty_leader)
    });

    // Read from the leader and write to `/dev/tty`.
    let pty_pipe = Pipe::new();
    dispatcher.set_read_callback(&pty.leader, |parent, _| {
        parent.pty_pipe.on_read(&mut parent.pty_leader)
    });
    dispatcher.set_write_callback(&user_tty, |parent, _| {
        parent.pty_pipe.on_write(&mut parent.user_tty)
    });

    // Check if we are the foreground process
    let mut foreground = tcgetpgrp(&user_tty).is_ok_and(|tty_pgrp| tty_pgrp == parent_pgrp);
    dev_info!(
        "sudo is runnning in the {}",
        cond_fmt(foreground, "foreground", "background")
    );

    // FIXME: maybe all these boolean flags should be on a dedicated type.

    // Whether we're running on a pipeline
    let pipeline = false;
    // Whether the command should be executed in the background (this is not the `-b` flag)
    let exec_bg = false;
    // Whether the user's terminal is in raw mode or not.
    let mut _term_raw = false;

    // FIXME (ogsudo): Do some extra setup if any of the IO streams are not a tty and logging is
    // enabled or if sudo is running in background.

    // Copy terminal settings from `/dev/tty` to the pty.
    if let Err(err) = user_tty.copy_to(&pty.follower) {
        dev_error!("cannot copy terminal settings to pty: {err}");
        foreground = false;
    }

    // Start in raw mode unless we're part of a pipeline or backgrounded.
    if foreground && !pipeline && !exec_bg && user_tty.term_raw(false).is_ok() {
        _term_raw = true;
    }

    // enabled or if sudo is running in background.
    // FIXME: it would be better if we didn't create the dispatcher before the fork and managed
    // to block all the signals here instead.

    let monitor_pid = fork().map_err(|err| {
        dev_error!("unable to fork monitor process: {err}");
        err
    })?;

    if monitor_pid == 0 {
        // Close the file descriptors that we don't access
        drop(pty.leader);
        drop(backchannels.parent);

        // Unregister all the handlers so `exec_monitor` can register new ones for the monitor
        // process.
        dispatcher.unregister_handlers();

        // If `exec_monitor` returns, it means we failed to execute the command somehow.
        if let Err(err) = exec_monitor(pty.follower, command, &mut backchannels.monitor) {
            match err.try_into() {
                Ok(msg) => {
                    if let Err(err) = backchannels.monitor.send(&msg) {
                        dev_error!("unable to send status to parent: {err}");
                    }
                }
                Err(err) => dev_warn!("execution error {err:?} cannot be send over backchannel"),
            }
        }
        // FIXME: drop everything before calling `exit`.
        exit(1)
    }

    // Close the file descriptors that we don't access
    drop(pty.follower);
    drop(backchannels.monitor);

    // Send green light to the monitor after closing the follower.
    retry_while_interrupted(|| backchannels.parent.send(&MonitorMessage::ExecCommand)).map_err(
        |err| {
            dev_error!("unable to send green light to monitor: {err}");
            err
        },
    )?;

    let closure = ParentClosure::new(
        monitor_pid,
        sudo_pid,
        backchannels.parent,
        user_tty,
        tty_pipe,
        pty.leader.into(),
        pty_pipe,
        &mut dispatcher,
    );

    // FIXME (ogsudo): Restore the signal handlers here.

    // FIXME (ogsudo): Retry if `/dev/tty` is revoked.
    let exit_reason = closure.run(&mut dispatcher)?;

    Ok((exit_reason, Box::new(move || drop(dispatcher))))
}

fn get_pty() -> io::Result<Pty> {
    let tty_gid = Group::from_name("tty")
        .unwrap_or(None)
        .map(|group| group.gid);

    let pty = Pty::open().map_err(|err| {
        dev_error!("unable to allocate pty: {err}");
        err
    })?;
    // FIXME: Test this
    chown(&pty.path, User::effective_uid(), tty_gid).map_err(|err| {
        dev_error!("unable to change owner for pty: {err}");
        err
    })?;

    Ok(pty)
}

struct ParentClosure {
    // The monitor PID.
    //
    /// This is `Some` iff the process is still running.
    monitor_pid: Option<ProcessId>,
    sudo_pid: ProcessId,
    command_pid: Option<ProcessId>,
    backchannel: ParentBackchannel,
    user_tty: UserTerm,
    tty_pipe: Pipe<UserTerm, File>,
    pty_leader: File,
    pty_pipe: Pipe<File, UserTerm>,
    message_queue: VecDeque<MonitorMessage>,
}

impl ParentClosure {
    #[allow(clippy::too_many_arguments)]
    fn new(
        monitor_pid: ProcessId,
        sudo_pid: ProcessId,
        backchannel: ParentBackchannel,
        user_tty: UserTerm,
        tty_pipe: Pipe<UserTerm, File>,
        pty_leader: File,
        pty_pipe: Pipe<File, UserTerm>,
        dispatcher: &mut EventDispatcher<Self>,
    ) -> Self {
        dispatcher.set_read_callback(&backchannel, |parent, dispatcher| {
            parent.on_message_received(dispatcher)
        });

        // Check for queued messages only when the backchannel can be written so we can send
        // messages to the monitor process without blocking.
        dispatcher.set_write_callback(&backchannel, |parent, dispatcher| {
            parent.check_message_queue(dispatcher)
        });

        Self {
            monitor_pid: Some(monitor_pid),
            sudo_pid,
            command_pid: None,
            backchannel,
            user_tty,
            tty_pipe,
            pty_leader,
            pty_pipe,
            message_queue: VecDeque::new(),
        }
    }

    fn run(mut self, dispatcher: &mut EventDispatcher<Self>) -> io::Result<ExitReason> {
        match dispatcher.event_loop(&mut self) {
            StopReason::Break(err) | StopReason::Exit(ParentExit::Backchannel(err)) => Err(err),
            StopReason::Exit(ParentExit::Command(exit_reason)) => Ok(exit_reason),
        }
    }

    /// Read an event from the backchannel and return the event if it should break the event loop.
    fn on_message_received(&mut self, dispatcher: &mut EventDispatcher<Self>) {
        match self.backchannel.recv() {
            // Not an actual error, we can retry later.
            Err(err) if was_interrupted(&err) => {}
            // Failed to read command status. This means that something is wrong with the socket
            // and we should stop.
            Err(err) => {
                // If we get EOF the monitor exited or was killed
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    dev_info!("parent received EOF from backchannel");
                    dispatcher.set_exit(err.into());
                } else {
                    dev_error!("could not receive message from monitor: {err}");
                    if !dispatcher.got_break() {
                        dispatcher.set_break(err);
                    }
                }
            }
            Ok(event) => {
                match event {
                    // Received the PID of the command. This means that the command is already
                    // executing.
                    ParentMessage::CommandPid(pid) => {
                        dev_info!("received command PID ({pid}) from monitor");
                        self.command_pid = pid.into();
                    }
                    // The command terminated or the monitor was not able to spawn it. We should stop
                    // either way.
                    ParentMessage::CommandExit(code) => {
                        dev_info!("command exited with status code {code}");
                        dispatcher.set_exit(ExitReason::Code(code).into());
                    }
                    ParentMessage::CommandSignal(signal) => {
                        // FIXME: this isn't right as the command has not exited if the signal is
                        // not a termination one. However, doing this makes us fail an ignored
                        // compliance test instead of hanging forever.
                        dev_info!("command was terminated by {}", signal_fmt(signal));
                        dispatcher.set_exit(ExitReason::Signal(signal).into());
                    }
                    ParentMessage::IoError(code) => {
                        let err = io::Error::from_raw_os_error(code);
                        dev_info!("received error ({code}) for monitor: {err}");
                        dispatcher.set_break(err);
                    }
                    ParentMessage::ShortRead => {
                        dev_info!("received short read error for monitor");
                        dispatcher.set_break(io::ErrorKind::UnexpectedEof.into());
                    }
                }
            }
        }
    }

    /// Decides if the signal sent by the process with `signaler_pid` PID is self-terminating.
    ///
    /// A signal is self-terminating if `signaler_pid`:
    /// - is the same PID of the command, or
    /// - is in the process group of the command and either sudo or the command is the leader.
    fn is_self_terminating(&self, signaler_pid: ProcessId) -> bool {
        if signaler_pid != 0 {
            if Some(signaler_pid) == self.command_pid {
                return true;
            }

            if let Ok(signaler_pgrp) = getpgid(signaler_pid) {
                if Some(signaler_pgrp) == self.command_pid || signaler_pgrp == self.sudo_pid {
                    return true;
                }
            }
        }

        false
    }

    /// Schedule sending a signal event to the monitor using the backchannel.
    ///
    /// The signal message will be sent once the backchannel is ready to be written.
    fn schedule_signal(&mut self, signal: c_int) {
        dev_info!("scheduling message with {} for monitor", signal_fmt(signal));
        self.message_queue.push_back(MonitorMessage::Signal(signal));
    }

    /// Send the first message in the event queue using the backchannel, if any.
    ///
    /// Calling this function will block until the backchannel can be written.
    fn check_message_queue(&mut self, dispatcher: &mut EventDispatcher<Self>) {
        if let Some(msg) = self.message_queue.front() {
            dev_info!("sending message {msg:?} to monitor over backchannel");
            match self.backchannel.send(msg) {
                // The event was sent, remove it from the queue
                Ok(()) => {
                    self.message_queue.pop_front().unwrap();
                }
                // The other end of the socket is gone, we should exit.
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                    dev_error!("broken pipe while writing to monitor over backchannel");
                    // FIXME: maybe we need a different event for backchannel errors.
                    dispatcher.set_break(err);
                }
                // Non critical error, we can retry later.
                Err(_) => {}
            }
        }
    }

    /// Handle changes to the monitor status.
    fn handle_sigchld(&mut self, monitor_pid: ProcessId) {
        const OPTS: WaitOptions = WaitOptions::new().all().untraced().no_hang();

        let status = loop {
            match waitpid(monitor_pid, OPTS) {
                Err(WaitError::Io(err)) if was_interrupted(&err) => {}
                // This only happens if we receive `SIGCHLD` but there's no status update from the
                // monitor.
                Err(WaitError::Io(_err)) => dev_info!("parent could not wait for monitor: {_err}"),
                // This only happens if the monitor exited and any process already waited for the monitor.
                Err(WaitError::NotReady) => dev_info!("monitor process without status update"),
                Ok((_pid, status)) => break status,
            }
        };

        if let Some(_code) = status.exit_status() {
            dev_info!("monitor ({monitor_pid}) exited with status code {_code}");
            self.monitor_pid = None;
        } else if let Some(_signal) = status.term_signal() {
            dev_info!(
                "monitor ({monitor_pid}) was terminated by {}",
                signal_fmt(_signal)
            );
            self.monitor_pid = None;
        } else if let Some(_signal) = status.stop_signal() {
            // FIXME: we should stop too.
            dev_info!(
                "monitor ({monitor_pid}) was stopped by {}",
                signal_fmt(_signal)
            );
        } else if status.did_continue() {
            dev_info!("monitor ({monitor_pid}) continued execution");
        } else {
            dev_warn!("unexpected wait status for monitor ({monitor_pid})")
        }
    }
}

enum ParentExit {
    /// Error while reading from the backchannel.
    Backchannel(io::Error),
    /// The command exited.
    Command(ExitReason),
}

impl From<io::Error> for ParentExit {
    fn from(err: io::Error) -> Self {
        Self::Backchannel(err)
    }
}

impl From<ExitReason> for ParentExit {
    fn from(reason: ExitReason) -> Self {
        Self::Command(reason)
    }
}

impl EventClosure for ParentClosure {
    type Break = io::Error;
    type Exit = ParentExit;

    fn on_signal(&mut self, info: SignalInfo, _dispatcher: &mut EventDispatcher<Self>) {
        dev_info!(
            "parent received{} {} from {}",
            opt_fmt(info.is_user_signaled(), " user signaled"),
            signal_fmt(info.signal()),
            info.pid()
        );

        let Some(monitor_pid) = self.monitor_pid else {
            dev_info!("monitor was terminated, ignoring signal");
            return;
        };

        match info.signal() {
            SIGCHLD => self.handle_sigchld(monitor_pid),
            // FIXME: check `resume_terminal`
            SIGCONT => {}
            // FIXME: check `sync_ttysize`
            SIGWINCH => {}
            // Skip the signal if it was sent by the user and it is self-terminating.
            _ if info.is_user_signaled() && self.is_self_terminating(info.pid()) => {}
            // FIXME: check `send_command_status`
            signal => self.schedule_signal(signal),
        }
    }
}
