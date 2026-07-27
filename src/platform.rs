//! Platform-specific process control. The ONLY place with OS `#[cfg]` for signals.
//!
//! Unix: graceful SIGTERM then SIGKILL. Windows has no SIGTERM, so graceful
//! drain is best-effort — we rely on the worker self-exiting (e.g. Symfony
//! Messenger `--time-limit`) and, as a last resort, `Child::kill()`
//! (`TerminateProcess`), which may lose an in-flight message.

use std::process::Child;

/// Canonical outcome of a stop request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The worker exited on its own within the grace period.
    Drained,
    /// The worker had to be force-killed after the grace period.
    Killed,
    /// The worker was already gone before we asked it to stop.
    AlreadyExited,
    /// Killing failed.
    Error,
}

/// Ask a worker to terminate gracefully (Unix: `SIGTERM`). Does not wait.
#[cfg(unix)]
pub fn signal_terminate(pid: u32) -> std::io::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM).map_err(std::io::Error::other)
}

/// Force-kill a worker (Unix: `SIGKILL`), falling back to `Child::kill()`.
#[cfg(unix)]
pub fn force_kill(pid: u32, child: &mut Child) -> std::io::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(_) => child.kill(),
    }
}

/// Windows has no `SIGTERM`: graceful terminate is a no-op. The worker must
/// stop itself (e.g. via `--time-limit`); otherwise `force_kill` is used.
#[cfg(windows)]
pub fn signal_terminate(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

/// Windows: the only available mechanism is `Child::kill()` (`TerminateProcess`),
/// which is NOT graceful.
#[cfg(windows)]
pub fn force_kill(_pid: u32, child: &mut Child) -> std::io::Result<()> {
    child.kill()
}
