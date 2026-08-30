//! Worker lifecycle: spawn processes directly by argv (no shell, no generated
//! `.sh`), keep the [`Child`] handle so identity is by PID, and stop workers
//! through the platform abstraction. Foundation for the Phase 1 SLO controller
//! (per-PID RSS in `spawn_rss`, exit reaping for autorestart).

use crate::platform::{self, StopOutcome};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A worker that exits sooner than this is treated as a crash, not a self-exit.
const MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(10);
/// Consecutive crashes tolerated before spawning is backed off.
const CRASHES_BEFORE_BACKOFF: u32 = 3;
/// Ceiling on the crash-loop back-off.
const MAX_SPAWN_BACKOFF: Duration = Duration::from_secs(300);

/// A single tracked worker process.
pub struct TrackedWorker {
    /// Logical index (drives `%(process_num)02d` and the display name).
    pub id: u32,
    /// OS process id (`Child::id()`).
    pub pid: u32,
    /// Display name (from `process_name`) — logs/metrics only, never discovery.
    pub name: String,
    /// Retained handle — the source of PID + kill.
    pub child: Child,
    /// When the worker started — drives crash-loop detection.
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
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        // Put the worker in its own process group so a stop reaches the whole
        // tree. Without this, `shell = true` only ever signals the `sh`, and any
        // worker that forks leaves children behind.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn().map_err(WorkerError::Spawn)?;
        let pid = child.id();
        let name = crate::config::expand_process_num(name_template, id);
        Ok(TrackedWorker {
            id,
            pid,
            name,
            child,
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

/// A worker that exited on its own, with enough context to judge whether it
/// crashed or finished cleanly.
#[derive(Debug)]
pub struct ExitedWorker {
    pub id: u32,
    pub pid: u32,
    pub status: std::process::ExitStatus,
    pub uptime: Duration,
    /// Exited faster than [`MIN_HEALTHY_UPTIME`] — treated as a crash.
    pub crashed: bool,
}

/// The set of live workers for one program. Identity/count come from this
/// registry, never from scanning process names.
pub struct WorkerPool {
    workers: Vec<TrackedWorker>,
    next_id: u32,
    command: String,
    name_template: String,
    shell: bool,
    /// Consecutive short-lived exits — drives the crash-loop back-off.
    consecutive_crashes: u32,
    /// Spawning is suppressed until this instant.
    blocked_until: Option<Instant>,
}

impl WorkerPool {
    pub fn new(command: String, name_template: String, shell: bool) -> Self {
        Self {
            workers: Vec::new(),
            next_id: 0,
            command,
            name_template,
            shell,
            consecutive_crashes: 0,
            blocked_until: None,
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

    /// Whether spawning is currently allowed. A command that dies on startup
    /// would otherwise be respawned once per tick forever.
    pub fn can_spawn(&self) -> bool {
        match self.blocked_until {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    /// How much longer spawning stays suppressed, if it does.
    pub fn spawn_backoff_remaining(&self) -> Option<Duration> {
        self.blocked_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    /// Scale up by one. Returns the new worker's PID, or `None` while the
    /// crash-loop back-off is active.
    pub fn spawn_one(&mut self) -> Result<Option<u32>, WorkerError> {
        if !self.can_spawn() {
            return Ok(None);
        }
        let id = self.next_id;
        let w = TrackedWorker::spawn(id, &self.command, &self.name_template, self.shell)?;
        let pid = w.pid;
        tracing::info!(worker_id = id, name = %w.name, pid, "started worker");
        self.next_id += 1;
        self.workers.push(w);
        Ok(Some(pid))
    }

    /// Remove the newest worker from the registry and hand it to the caller.
    ///
    /// The pool's count drops immediately while the caller drains the process in
    /// the background, so a slow drain never stalls the control loop.
    pub fn detach_one(&mut self) -> Option<TrackedWorker> {
        self.workers.pop()
    }

    /// Remove workers that exited on their own and update crash-loop state.
    pub fn reap_exited(&mut self) -> Vec<ExitedWorker> {
        let mut dead = Vec::new();
        let mut i = 0;
        while i < self.workers.len() {
            if let Some(status) = self.workers[i].poll_exited() {
                let w = self.workers.remove(i);
                let uptime = w.started_at.elapsed();
                dead.push(ExitedWorker {
                    id: w.id,
                    pid: w.pid,
                    status,
                    uptime,
                    crashed: uptime < MIN_HEALTHY_UPTIME,
                });
            } else {
                i += 1;
            }
        }
        for w in &dead {
            self.record_exit(w.crashed);
        }
        dead
    }

    /// Track consecutive crashes and arm an exponential back-off once a command
    /// looks broken rather than merely unlucky.
    fn record_exit(&mut self, crashed: bool) {
        if !crashed {
            self.consecutive_crashes = 0;
            self.blocked_until = None;
            return;
        }
        self.consecutive_crashes = self.consecutive_crashes.saturating_add(1);
        if self.consecutive_crashes < CRASHES_BEFORE_BACKOFF {
            return;
        }
        let exponent = (self.consecutive_crashes - CRASHES_BEFORE_BACKOFF).min(10);
        let backoff = MIN_HEALTHY_UPTIME
            .saturating_mul(1u32 << exponent)
            .min(MAX_SPAWN_BACKOFF);
        self.blocked_until = Some(Instant::now() + backoff);
        tracing::warn!(
            consecutive_crashes = self.consecutive_crashes,
            backoff_s = backoff.as_secs(),
            command = %self.command,
            "worker command keeps exiting immediately; backing off before respawning"
        );
    }

    /// Gracefully stop every worker (used on daemon shutdown). Workers are
    /// drained concurrently, so shutdown costs one `drain_timeout`, not N.
    pub async fn shutdown_all(&mut self, drain_timeout: Duration) {
        let mut set = tokio::task::JoinSet::new();
        for mut w in std::mem::take(&mut self.workers) {
            set.spawn(async move {
                let outcome = w.request_stop(drain_timeout).await;
                tracing::info!(worker_id = w.id, pid = w.pid, ?outcome, "stopped worker");
            });
        }
        while set.join_next().await.is_some() {}
    }
}

/// Drain a detached worker in the background.
pub async fn drain_detached(mut w: TrackedWorker, drain_timeout: Duration) {
    let outcome = w.request_stop(drain_timeout).await;
    tracing::info!(worker_id = w.id, pid = w.pid, ?outcome, "stopped worker");
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

    fn sleeper_pool() -> WorkerPool {
        #[cfg(unix)]
        let cmd = "sleep 30";
        #[cfg(windows)]
        let cmd = "cmd /C timeout /T 30 /NOBREAK";
        WorkerPool::new(cmd.to_string(), "w_%(process_num)02d".to_string(), false)
    }

    #[tokio::test]
    async fn spawn_and_request_stop_sleeper() {
        let mut pool = sleeper_pool();
        pool.spawn_one().unwrap().expect("spawn allowed");
        assert_eq!(pool.len(), 1);
        let w = pool.detach_one().expect("worker detached");
        // Detaching drops the count immediately; the drain happens off the loop.
        assert_eq!(pool.len(), 0);
        let mut w = w;
        let outcome = w.request_stop(Duration::from_millis(300)).await;
        assert!(matches!(
            outcome,
            StopOutcome::Killed | StopOutcome::Drained
        ));
    }

    #[tokio::test]
    async fn shutdown_all_drains_every_worker() {
        let mut pool = sleeper_pool();
        for _ in 0..3 {
            pool.spawn_one().unwrap().expect("spawn allowed");
        }
        assert_eq!(pool.len(), 3);
        pool.shutdown_all(Duration::from_millis(200)).await;
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[tokio::test]
    async fn crash_looping_command_gets_backed_off() {
        // `false` exits immediately, so every spawn counts as a crash.
        #[cfg(unix)]
        let cmd = "false";
        #[cfg(windows)]
        let cmd = "cmd /C exit 1";
        let mut pool = WorkerPool::new(cmd.to_string(), "w".to_string(), false);

        for _ in 0..CRASHES_BEFORE_BACKOFF {
            assert!(pool.can_spawn(), "should still be allowed to spawn");
            pool.spawn_one().unwrap().expect("spawn allowed");
            // Give the process a moment to die, then reap it.
            tokio::time::sleep(Duration::from_millis(120)).await;
            let dead = pool.reap_exited();
            assert_eq!(dead.len(), 1);
            assert!(dead[0].crashed, "a sub-10s exit must count as a crash");
        }

        assert!(
            !pool.can_spawn(),
            "back-off must engage after repeat crashes"
        );
        assert!(pool.spawn_backoff_remaining().is_some());
        // A blocked spawn is not an error — it reports that nothing started.
        assert_eq!(pool.spawn_one().unwrap(), None);
        assert_eq!(pool.len(), 0);
    }

    #[tokio::test]
    async fn a_healthy_exit_clears_the_crash_counter() {
        let mut pool = sleeper_pool();
        pool.spawn_one().unwrap().expect("spawn allowed");
        // Forge the history: two crashes recorded, then a clean long-lived exit.
        pool.record_exit(true);
        pool.record_exit(true);
        assert_eq!(pool.consecutive_crashes, 2);
        pool.record_exit(false);
        assert_eq!(pool.consecutive_crashes, 0);
        assert!(pool.can_spawn());
        pool.shutdown_all(Duration::from_millis(200)).await;
    }
}
