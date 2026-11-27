//! A Git child, its process group, and its pipes have one lifetime.

use crate::stop::HardCancellation;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

use crate::blocking::{
    start_blocking_operation, BlockingOperation, OffloadError, OperationHandle, StartedOperations,
};

pub(crate) struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// A process result is usable only after group shutdown and pipe closure were confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEnd {
    Exited,
    TimedOut,
    Cancelled,
    Unconfirmed,
}

impl Output {
    pub(crate) fn end(&self) -> ProcessEnd {
        if self.error.is_some() {
            ProcessEnd::Unconfirmed
        } else if self.cancelled {
            ProcessEnd::Cancelled
        } else if self.timed_out {
            ProcessEnd::TimedOut
        } else {
            ProcessEnd::Exited
        }
    }
}

/// Retains the completion of exactly one invocation across interrupted waits.
pub(crate) struct Running {
    stop: watch::Sender<bool>,
    operation: Completion,
}

enum Completion {
    Independent(BlockingOperation<Output>),
    Retained(Arc<OperationHandle<Output>>),
}

struct StopOnDrop {
    stop: watch::Sender<bool>,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.stop.send(true);
        }
    }
}

impl Running {
    /// Abandoning this wait requests shutdown, but retains its completion slot.
    pub(crate) async fn wait(&mut self) -> Result<Output, OffloadError> {
        let mut guard = StopOnDrop {
            stop: self.stop.clone(),
            armed: true,
        };
        let result = match &mut self.operation {
            Completion::Independent(operation) => operation.wait().await,
            Completion::Retained(handle) => handle
                .take_result()
                .await
                .expect("process completion already taken"),
        };
        guard.armed = false;
        result
    }

    /// Retain the completion slot when cancellation requests shutdown. A lost
    /// await also requests shutdown through the guard in `wait`.
    pub(crate) async fn wait_with_cancel(
        &mut self,
        cancel: Option<&HardCancellation>,
    ) -> Result<Output, OffloadError> {
        if let Some(cancel) = cancel {
            tokio::select! {
                biased;
                result = self.wait() => return result,
                _ = cancel.cancelled() => {}
            }
            self.stop();
        }
        self.wait().await
    }

    pub(crate) fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts one blocking process operation without tying its lifetime to a wait.
pub(crate) fn start(
    command: Command,
    timeout: Duration,
    payload: Option<Vec<u8>>,
    cancel: Option<HardCancellation>,
) -> Running {
    let (stop, receiver) = watch::channel(false);
    let operation = start_blocking_operation(move || {
        run_with_stop(
            command,
            timeout,
            Some(&receiver),
            cancel.as_ref(),
            payload.as_deref(),
        )
    });
    Running {
        stop,
        operation: Completion::Independent(operation),
    }
}

/// Admit a process and retain its stop request and completion before awaiting it.
pub(crate) fn start_owned(
    command: Command,
    timeout: Duration,
    payload: Option<Vec<u8>>,
    owner: &StartedOperations,
    cancel: Option<HardCancellation>,
) -> Result<Running, &'static str> {
    let (stop, receiver) = watch::channel(false);
    let shutdown = stop.clone();
    // With no runtime the work may finish inline before start returns its
    // handle. Hold the handoff lock while admitting it and transfer the mark
    // either here or from the worker after the handle becomes available.
    let handoff = Arc::new(Mutex::new(None::<Arc<OperationHandle<Output>>>));
    let mut slot = handoff.lock().unwrap_or_else(|error| error.into_inner());
    let uncertain = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_handoff = Arc::clone(&handoff);
    let worker_uncertain = Arc::clone(&uncertain);
    let handle = owner.start(
        move || {
            let output = run_with_stop(
                command,
                timeout,
                Some(&receiver),
                cancel.as_ref(),
                payload.as_deref(),
            );
            if output.end() == ProcessEnd::Unconfirmed {
                worker_uncertain.store(true, std::sync::atomic::Ordering::Release);
                if let Ok(guard) = worker_handoff.try_lock() {
                    if let Some(handle) = guard.as_ref() {
                        handle.mark_unconfirmed();
                    }
                }
            }
            output
        },
        Some(Box::new(move || {
            let _ = shutdown.send(true);
        })),
    )?;
    let handle = Arc::new(handle);
    *slot = Some(Arc::clone(&handle));
    if uncertain.load(std::sync::atomic::Ordering::Acquire) {
        handle.mark_unconfirmed();
    }
    drop(slot);
    Ok(Running {
        stop,
        operation: Completion::Retained(handle),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
    use std::time::Instant;

    const POLL: Duration = Duration::from_millis(5);
    const CLEANUP_LIMIT: Duration = Duration::from_secs(2);

    fn vanished(error: &std::io::Error) -> bool {
        error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
    }

    struct Owner {
        child: Child,
        pid: i32,
        input: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        reaped: bool,
    }

    impl Owner {
        fn signal(&self, signal: i32) -> std::io::Result<()> {
            // A child auto-reaped by an inherited SIGCHLD disposition no
            // longer reserves its numeric group ID. Refuse to signal it.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { libc::kill(-self.pid, signal) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        }

        fn exited(&self) -> std::io::Result<bool> {
            let stat = match std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
                Ok(stat) => stat,
                Err(error) if vanished(&error) => {
                    // An inherited SIGCHLD disposition may auto-reap a short
                    // child. Its numeric group ID is no longer safe to signal;
                    // absence is usable only if no member remains to stop.
                    return match live_members(self.pid)? {
                        false => Ok(true),
                        true => Err(std::io::Error::other(
                            "Git leader disappeared while its process group remained active",
                        )),
                    };
                }
                Err(error) => return Err(error),
            };
            let fields = stat.rsplit_once(") ").ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid leader stat")
            })?;
            Ok(matches!(
                fields.1.split_whitespace().next(),
                Some("Z" | "X")
            ))
        }

        fn reap(&mut self) -> std::io::Result<Option<i32>> {
            let status = self.child.wait();
            if status.is_ok() {
                self.reaped = true;
            }
            status.map(|status| status.code())
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.signal(libc::SIGKILL);
                let _ = self.child.wait();
            }
        }
    }

    fn nonblocking(fd: i32) -> std::io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain<R: Read>(pipe: &mut Option<R>, bytes: &mut Vec<u8>) -> std::io::Result<()> {
        let Some(reader) = pipe.as_mut() else {
            return Ok(());
        };
        let mut buffer = [0; 16384];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    *pipe = None;
                    return Ok(());
                }
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    // A continuous producer cannot monopolize this loop:
                    // the caller must still probe cancellation and timeout.
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    // Linux /proc reports the state and process group of each member. Zombie
    // descendants have stopped executing; a live member must not be mistaken
    // for a stopped process merely because the leader exited or a pipe closed.
    fn live_members(group: i32) -> std::io::Result<bool> {
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().parse::<i32>().is_err() {
                continue;
            }
            let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error),
            };
            let fields = stat
                .rsplit_once(") ")
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid process stat")
                })?
                .1;
            let mut fields = fields.split_whitespace();
            let state = fields.next();
            let _parent = fields.next();
            let process_group = fields.next().and_then(|id| id.parse::<i32>().ok());
            if process_group == Some(group) && !matches!(state, Some("Z" | "X")) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn run(
        mut command: Command,
        timeout: Duration,
        local_stop: Option<&watch::Receiver<bool>>,
        cancel: Option<&HardCancellation>,
        payload: Option<&[u8]>,
    ) -> Output {
        if cancel.is_some_and(HardCancellation::is_cancelled) {
            return Output {
                code: None,
                stdout: vec![],
                stderr: vec![],
                timed_out: false,
                cancelled: true,
                error: None,
            };
        }
        command.stdin(if payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.process_group(0);
        // Reset inherited signal state before exec so Git can release locks on
        // SIGTERM even when the parent process blocked or ignored the signal.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                let mut mask = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGTERM);
                libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
                Ok(())
            });
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Output {
                    code: None,
                    stdout: vec![],
                    stderr: vec![],
                    timed_out: false,
                    cancelled: false,
                    error: Some(error.to_string()),
                }
            }
        };
        let mut owner = Owner {
            pid: child.id() as i32,
            child,
            input: None,
            stdout: None,
            stderr: None,
            reaped: false,
        };
        owner.input = owner.child.stdin.take();
        owner.stdout = owner.child.stdout.take();
        owner.stderr = owner.child.stderr.take();
        let mut output = Output {
            code: None,
            stdout: vec![],
            stderr: vec![],
            timed_out: false,
            cancelled: false,
            error: None,
        };
        for fd in owner
            .input
            .as_ref()
            .map(|p| p.as_raw_fd())
            .into_iter()
            .chain(owner.stdout.as_ref().map(|p| p.as_raw_fd()))
            .chain(owner.stderr.as_ref().map(|p| p.as_raw_fd()))
        {
            if let Err(error) = nonblocking(fd) {
                output.error = Some(format!("Git pipe setup failed: {error}"));
                break;
            }
        }
        let started = Instant::now();
        let mut offset = 0;
        let mut leader_exited = false;
        while output.error.is_none() {
            if local_stop.is_some_and(|rx| *rx.borrow())
                || cancel.is_some_and(HardCancellation::is_cancelled)
            {
                output.cancelled = true;
                break;
            }
            if started.elapsed() >= timeout {
                output.timed_out = true;
                break;
            }
            if let Err(error) = drain(&mut owner.stdout, &mut output.stdout)
                .and_then(|()| drain(&mut owner.stderr, &mut output.stderr))
            {
                output.error = Some(format!("Git pipe read failed: {error}"));
                break;
            }
            if let Some(pipe) = owner.input.as_mut() {
                let data = payload.unwrap_or_default();
                if offset == data.len() {
                    owner.input = None;
                } else {
                    match pipe.write(&data[offset..]) {
                        Ok(n) => offset += n,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                            owner.input = None
                        }
                        Err(error) => {
                            output.error = Some(format!("Git pipe write failed: {error}"));
                            break;
                        }
                    }
                }
            }
            match owner.exited() {
                Ok(true) => {
                    leader_exited = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    output.error = Some(format!("Git status probe failed: {error}"));
                    break;
                }
            }
            if local_stop.is_some_and(|rx| *rx.borrow())
                || cancel.is_some_and(HardCancellation::is_cancelled)
            {
                output.cancelled = true;
                break;
            }
            if started.elapsed() >= timeout {
                output.timed_out = true;
                break;
            }
            std::thread::sleep(POLL);
        }
        owner.input = None;
        // Observe the leader before signalling the group. A short Git command
        // can exit between the last loop probe and shutdown; keep that fact
        // while its unreaped PID still anchors the group identity.
        if !leader_exited && output.error.is_none() {
            match owner.exited() {
                Ok(exited) => leader_exited = exited,
                Err(error) => {
                    output.error = Some(format!("Git shutdown probe failed: {error}"));
                }
            }
        }
        if output.timed_out || output.cancelled {
            if let Err(error) = owner.signal(libc::SIGTERM) {
                output
                    .error
                    .get_or_insert_with(|| format!("Git termination failed: {error}"));
            }
            std::thread::sleep(crate::git::exec::TERMINATION_GRACE);
        }
        // A normally exited Git with no live descendants needs no signal.
        // In particular, do not signal a group whose only member is the
        // unreaped leader: there is nothing to stop, and the leader's status
        // is already observed. Keep the leader unreaped for the group probe.
        let active = match live_members(owner.pid) {
            Ok(active) => active,
            Err(error) => {
                output
                    .error
                    .get_or_insert_with(|| format!("Git shutdown probe failed: {error}"));
                true
            }
        };
        if active {
            if let Err(error) = owner.signal(libc::SIGKILL) {
                output
                    .error
                    .get_or_insert_with(|| format!("Git group kill failed: {error}"));
            }
        }
        let cleanup = Instant::now();
        loop {
            if let Err(error) = drain(&mut owner.stdout, &mut output.stdout)
                .and_then(|()| drain(&mut owner.stderr, &mut output.stderr))
            {
                output
                    .error
                    .get_or_insert_with(|| format!("Git pipe read failed: {error}"));
                break;
            }
            // An observed zombie cannot resume execution. Keep the observation
            // while the group is stopped; the leader is reaped only afterwards.
            let leader = if leader_exited {
                Ok(true)
            } else {
                owner.exited()
            };
            let stopped = match (leader, live_members(owner.pid)) {
                (Ok(true), Ok(false)) => true,
                (Err(error), _) | (_, Err(error)) => {
                    output
                        .error
                        .get_or_insert_with(|| format!("Git shutdown probe failed: {error}"));
                    break;
                }
                _ => false,
            };
            if stopped && owner.stdout.is_none() && owner.stderr.is_none() {
                break;
            }
            if cleanup.elapsed() >= CLEANUP_LIMIT {
                output.error.get_or_insert_with(|| {
                    "Git process group or output pipes remained active after shutdown".into()
                });
                break;
            }
            std::thread::sleep(POLL);
        }
        match owner.reap() {
            Ok(code) => output.code = code,
            Err(error) => {
                output
                    .error
                    .get_or_insert_with(|| format!("Git reap failed: {error}"));
            }
        }
        output
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn continuous_output_does_not_starve_the_deadline() {
        let started = Instant::now();
        let output = run(
            shell("while :; do printf 'continuous output\\n'; done"),
            Duration::from_millis(100),
            None,
            None,
        );
        assert!(output.timed_out, "{:?}", output.error);
        assert!(output.error.is_none(), "{:?}", output.error);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_exited_leader_does_not_hide_a_descendant_with_closed_pipes() {
        let started = Instant::now();
        let output = run(
            shell("sleep 30 </dev/null >/dev/null 2>&1 & exit 0"),
            Duration::from_secs(2),
            None,
            None,
        );
        assert_eq!(output.code, Some(0));
        assert!(output.error.is_none(), "{:?}", output.error);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_lost_wait_stops_the_process_and_retains_its_completion() {
        let source = crate::stop::StopSource::new();
        let token = source.token();
        let mut running = start(
            shell("sleep 30"),
            Duration::from_secs(30),
            None,
            Some(token.hard_cancellation()),
        );
        {
            let wait = running.wait();
            tokio::pin!(wait);
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut wait)
                .await
                .is_err());
        }
        let output = tokio::time::timeout(Duration::from_secs(5), running.wait())
            .await
            .expect("shutdown must complete")
            .expect("the operation must report completion");
        assert!(output.cancelled, "{:?}", output.error);
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(token.state(), crate::stop::StopState::Running);
        assert!(!token.hard_cancellation().is_cancelled());
    }

    #[tokio::test]
    async fn owner_keeps_a_lost_process_wait_and_stops_descendants() {
        let owner = StartedOperations::default();
        let mut running = start_owned(
            shell("sleep 30 </dev/null >/dev/null 2>&1 & wait"),
            Duration::from_secs(30),
            None,
            &owner,
            None,
        )
        .unwrap();
        {
            let wait = running.wait();
            tokio::pin!(wait);
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut wait)
                .await
                .is_err());
        }
        drop(running);
        owner.close();
        let summary = tokio::time::timeout(Duration::from_secs(5), owner.wait())
            .await
            .expect("descendants must stop");
        assert_eq!(summary.total, 1);
        assert_eq!(summary.unconfirmed, 0);
        assert!(owner.start(|| 1, None).is_err());
    }

    #[tokio::test]
    async fn unconfirmed_process_result_survives_a_lost_caller() {
        let owner = StartedOperations::default();
        let mut command = shell("exit 0");
        command.current_dir("/a/directory/that/does/not/exist");
        let running = start_owned(command, Duration::from_secs(1), None, &owner, None).unwrap();
        drop(running);
        owner.close();
        let summary = owner.wait().await;
        assert_eq!(summary.total, 1);
        assert_eq!(summary.unconfirmed, 1);
    }
}

pub(crate) fn run(
    command: Command,
    timeout: Duration,
    cancel: Option<&HardCancellation>,
    payload: Option<&[u8]>,
) -> Output {
    run_with_stop(command, timeout, None, cancel, payload)
}

fn run_with_stop(
    command: Command,
    timeout: Duration,
    local_stop: Option<&watch::Receiver<bool>>,
    cancel: Option<&HardCancellation>,
    payload: Option<&[u8]>,
) -> Output {
    #[cfg(target_os = "linux")]
    {
        linux::run(command, timeout, local_stop, cancel, payload)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, timeout, local_stop, cancel, payload);
        Output {
            code: None,
            stdout: vec![],
            stderr: vec![],
            timed_out: false,
            cancelled: false,
            error: Some("Git process-group shutdown cannot be confirmed on this platform".into()),
        }
    }
}
