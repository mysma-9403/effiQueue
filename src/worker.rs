//! Worker lifecycle: spawn processes directly by argv (no shell, no generated
//! `.sh`), keep the [`Child`] handle so identity is by PID, and stop workers
//! through the platform abstraction. Foundation for the Phase 1 SLO controller
//! (per-PID RSS in `spawn_rss`, exit reaping for autorestart).

use crate::platform::{self, StopOutcome};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A single tracked worker process.
pub struct TrackedWorker {
    /// Logical index (drives `%(process_num)02d` and the display name).
    pub id: u32,
    /// OS process id (`Child::id()`).
    pub pid: u32,
    /// Display name (from `process_name`) — logs/metrics only, never discovery.
    pub name: String,
    /// Retained handle — the source of PID + kill, dropped in the old code.
    pub child: Child,
    /// RSS (bytes) sampled shortly after spawn. Foundation for Phase 1. TODO Phase 1.
    #[allow(dead_code)]
    pub spawn_rss: Option<u64>,
    /// When the worker started. Foundation for age/backoff. TODO Phase 1.
    #[allow(dead_code)]
    pub started_at: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("empty command line after shell-words parsing")]
    EmptyCommand,
    #[error("failed to parse 'command': {0}")]
    Parse(String),
    #[error("failed to spawn process: {0}")]
    Spawn(#[source] std::io::Error),
}

impl TrackedWorker {
    /// Spawn a worker. By default parses `command` into argv and runs it
    /// directly; with `shell = true` wraps it in `sh -c` / `cmd /C`.
    pub fn spawn(
        id: u32,
        command: &str,
        name_template: &str,
        shell: bool,
    ) -> Result<Self, WorkerError> {
        let expanded = crate::config::expand_process_num(command, id);
        let mut cmd = if shell {
            #[cfg(unix)]
            {
                let mut c = Command::new("sh");
                c.arg("-c").arg(&expanded);
                c
            }
            #[cfg(windows)]
            {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(&expanded);
                c
            }
        } else {
            let argv =
                shell_words::split(&expanded).map_err(|e| WorkerError::Parse(e.to_string()))?;
            let (prog, rest) = argv.split_first().ok_or(WorkerError::EmptyCommand)?;
            let mut c = Command::new(prog);
            c.args(rest);
            c
        };
        let child = cmd
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(WorkerError::Spawn)?;
        let pid = child.id();
        let name = crate::config::expand_process_num(name_template, id);
        Ok(TrackedWorker {
            id,
            pid,
            name,
            child,
            spawn_rss: None,
            started_at: Instant::now(),
        })
    }

    /// Request a graceful stop. Unix: `SIGTERM`, poll until `drain_timeout`,
    /// then `SIGKILL`. Windows: wait for self-exit until `drain_timeout`, then
    /// hard-kill (best effort).
    pub async fn request_stop(&mut self, drain_timeout: Duration) -> StopOutcome {
        if self.poll_exited().is_some() {
            return StopOutcome::AlreadyExited;
        }
        // Unix: SIGTERM asks the worker to finish the current message and exit.
        // Windows: no-op — the worker must self-exit (e.g. `--time-limit`).
        let _ = platform::signal_terminate(self.pid);
        #[cfg(windows)]
        tracing::warn!(
            pid = self.pid,
            "graceful drain is not available on Windows; after drain_timeout Child.kill() (TerminateProcess) is used — in-flight messages may be lost"
        );

        let deadline = Instant::now() + drain_timeout;
        while Instant::now() < deadline {
            if self.poll_exited().is_some() {
                return StopOutcome::Drained;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let outcome = if platform::force_kill(self.pid, &mut self.child).is_ok() {
            StopOutcome::Killed
        } else {
            StopOutcome::Error
        };
        let _ = self.child.wait();
        outcome
    }

    /// Non-blocking check whether the worker has already exited.
    pub fn poll_exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

/// The set of live workers for one program. Identity/count come from this
/// registry, never from scanning process names.
pub struct WorkerPool {
    workers: Vec<TrackedWorker>,
    next_id: u32,
    command: String,
    name_template: String,
    shell: bool,
}

impl WorkerPool {
    pub fn new(command: String, name_template: String, shell: bool) -> Self {
        Self {
            workers: Vec::new(),
            next_id: 0,
            command,
            name_template,
            shell,
        }
    }

    /// Number of live workers (registry length, not a name scan).
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// PIDs of all live workers (basis for per-PID RSS sampling).
    pub fn pids(&self) -> Vec<u32> {
        self.workers.iter().map(|w| w.pid).collect()
    }

    /// Scale up by one. Returns the new worker's PID.
    pub fn spawn_one(&mut self) -> Result<u32, WorkerError> {
        let id = self.next_id;
        let w = TrackedWorker::spawn(id, &self.command, &self.name_template, self.shell)?;
        let pid = w.pid;
        tracing::info!(worker_id = id, name = %w.name, pid, "started worker");
        self.next_id += 1;
        self.workers.push(w);
        Ok(pid)
    }

    /// Scale down by one (graceful).
    pub async fn stop_one(&mut self, drain_timeout: Duration) -> Option<StopOutcome> {
        let mut w = self.workers.pop()?;
        let outcome = w.request_stop(drain_timeout).await;
        tracing::info!(worker_id = w.id, pid = w.pid, ?outcome, "stopped worker");
        Some(outcome)
    }

    /// Remove workers that exited on their own (crash or self-exit).
    /// Foundation for autorestart (Phase 1).
    pub fn reap_exited(&mut self) -> Vec<(u32, std::process::ExitStatus)> {
        let mut dead = Vec::new();
        let mut i = 0;
        while i < self.workers.len() {
            if let Some(status) = self.workers[i].poll_exited() {
                let w = self.workers.remove(i);
                dead.push((w.id, status));
            } else {
                i += 1;
            }
        }
        dead
    }

    /// Gracefully stop every worker (used on daemon shutdown).
    pub async fn shutdown_all(&mut self, drain_timeout: Duration) {
        while let Some(mut w) = self.workers.pop() {
            w.request_stop(drain_timeout).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_is_error() {
        assert!(matches!(
            TrackedWorker::spawn(0, "   ", "n", false),
            Err(WorkerError::EmptyCommand)
        ));
    }

    #[tokio::test]
    async fn spawn_and_request_stop_sleeper() {
        #[cfg(unix)]
        let cmd = "sleep 30";
        #[cfg(windows)]
        let cmd = "cmd /C timeout /T 30 /NOBREAK";

        let mut pool = WorkerPool::new(cmd.to_string(), "w_%(process_num)02d".to_string(), false);
        pool.spawn_one().unwrap();
        assert_eq!(pool.len(), 1);
        let outcome = pool.stop_one(Duration::from_millis(300)).await;
        assert!(matches!(
            outcome,
            Some(StopOutcome::Killed) | Some(StopOutcome::Drained)
        ));
        assert_eq!(pool.len(), 0);
    }
}
