//! Bounded output and cancellation ownership for already-authorized commands.
//!
//! Callers retain command construction, environment, sandbox and policy checks.
//! Like the runtime ShellTool, Unix children get their own session/process group;
//! cancellation kills that group while the leader is still owned. Other targets
//! retain Tokio's direct-child kill-on-drop guarantee.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::Instant;

const POST_EXIT_DRAIN: Duration = Duration::from_millis(250);
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowBehavior {
    /// Finish the command, discarding bytes beyond the capture budgets.
    Drain,
    /// Stop read-only producers when their capture budget is exhausted.
    Terminate,
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    /// Optional bound on captured stdout lines, used by search producers.
    pub stdout_lines: Option<usize>,
    pub overflow: OverflowBehavior,
}

#[derive(Debug)]
pub enum ProcessOutcome {
    Exited(ExitStatus),
    TimedOut,
    OutputLimit,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub outcome: ProcessOutcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

struct ProcessOwner {
    child: Child,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessOwner {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe. The post-fork hook does
            // not allocate or lock; it only detaches the child before exec.
            unsafe {
                command.as_std_mut().pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self {
            #[cfg(unix)]
            pgid: child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0),
            child,
        })
    }

    fn disarm_group(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: this positive PGID was created by setsid in the owned
            // child. It is cleared as soon as wait reaps the leader, so this
            // guard never signals a group using a reaped/recycled leader PID.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    async fn terminate_and_reap(&mut self) {
        self.terminate();
        // A stuck OS wait must not turn an execution timeout into another
        // unbounded wait. Tokio retains its kill-on-drop/reaper fallback.
        let _ = tokio::time::timeout(KILL_REAP_TIMEOUT, self.child.wait()).await;
        self.disarm_group();
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    lines: usize,
    truncated: bool,
}

impl Capture {
    fn append(&mut self, chunk: &[u8], byte_limit: usize, line_limit: Option<usize>) -> bool {
        let mut take = chunk.len().min(byte_limit.saturating_sub(self.bytes.len()));
        let mut line_limit_reached = false;
        if let Some(limit) = line_limit {
            let remaining = limit.saturating_sub(self.lines);
            if remaining == 0 {
                take = 0;
                line_limit_reached = true;
            } else if let Some((index, _)) = chunk[..take]
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .nth(remaining - 1)
            {
                take = index + 1;
                line_limit_reached = true;
            }
        }
        self.lines += chunk[..take].iter().filter(|byte| **byte == b'\n').count();
        self.bytes.extend_from_slice(&chunk[..take]);
        self.truncated |= take < chunk.len() || line_limit_reached;
        self.truncated
    }
}

async fn read_pipe<R: AsyncRead + Unpin>(
    reader: &mut Option<R>,
    buffer: &mut [u8],
) -> io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buffer).await,
        None => std::future::pending().await,
    }
}

async fn drain_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Execute a command with bounded captures. Dropping this future drops both
/// pipe readers and the process owner; no detached reader task survives it.
pub async fn run_bounded(
    command: &mut Command,
    limits: ProcessLimits,
) -> io::Result<ProcessOutput> {
    let mut owner = ProcessOwner::spawn(command)?;
    let mut stdout = owner.child.stdout.take();
    let mut stderr = owner.child.stderr.take();
    let mut stdout_capture = Capture::default();
    let mut stderr_capture = Capture::default();
    let mut stdout_buffer = [0_u8; 8192];
    let mut stderr_buffer = [0_u8; 8192];
    let timeout = tokio::time::sleep(limits.timeout);
    tokio::pin!(timeout);
    let mut status = None;
    let mut post_exit_deadline = None;

    let outcome = loop {
        if let Some(status) = status
            && stdout.is_none()
            && stderr.is_none()
        {
            break ProcessOutcome::Exited(status);
        }
        tokio::select! {
            result = owner.child.wait(), if status.is_none() => {
                status = Some(result?);
                // A normal exit may leave descendants holding the pipes. Bound
                // their drain, but do not signal a possibly recycled leader PID.
                owner.disarm_group();
                post_exit_deadline = Some(Instant::now() + POST_EXIT_DRAIN);
            }
            read = read_pipe(&mut stdout, &mut stdout_buffer) => {
                let read = read?;
                if read == 0 {
                    stdout = None;
                } else if stdout_capture.append(
                    &stdout_buffer[..read], limits.stdout_bytes, limits.stdout_lines,
                ) && limits.overflow == OverflowBehavior::Terminate {
                    owner.terminate_and_reap().await;
                    break ProcessOutcome::OutputLimit;
                }
            }
            read = read_pipe(&mut stderr, &mut stderr_buffer) => {
                let read = read?;
                if read == 0 {
                    stderr = None;
                } else if stderr_capture.append(
                    &stderr_buffer[..read], limits.stderr_bytes, None,
                ) && limits.overflow == OverflowBehavior::Terminate {
                    owner.terminate_and_reap().await;
                    break ProcessOutcome::OutputLimit;
                }
            }
            _ = &mut timeout, if status.is_none() => {
                owner.terminate_and_reap().await;
                break ProcessOutcome::TimedOut;
            }
            _ = drain_deadline(post_exit_deadline), if status.is_some() => {
                stdout_capture.truncated |= stdout.is_some();
                stderr_capture.truncated |= stderr.is_some();
                // The status branch above established Some; avoid relying on
                // that invariant with an unwrap in this resource owner.
                if let Some(status) = status {
                    break ProcessOutcome::Exited(status);
                }
            }
        }
    };

    Ok(ProcessOutput {
        outcome,
        stdout: stdout_capture.bytes,
        stderr: stderr_capture.bytes,
        stdout_truncated: stdout_capture.truncated,
        stderr_truncated: stderr_capture.truncated,
    })
}

#[cfg(all(test, unix))]
mod tests;
