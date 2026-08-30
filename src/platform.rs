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

/// Signal a worker's whole process group, falling back to the single process.
///
/// Workers are spawned as group leaders (`process_group(0)`), so the negated PID
/// addresses the worker *and* anything it forked — the `sh` wrapper under
/// `shell = true`, or a runtime that spawns helpers.
#[cfg(unix)]
fn signal_group(pid: u32, signal: nix::sys::signal::Signal) -> std::io::Result<()> {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(-(pid as i32)), signal) {
        Ok(()) => Ok(()),
        // No such group (the worker never became a leader) — target the process.
        Err(_) => kill(Pid::from_raw(pid as i32), signal).map_err(std::io::Error::other),
    }
}

/// Ask a worker to terminate gracefully (Unix: `SIGTERM`). Does not wait.
#[cfg(unix)]
pub fn signal_terminate(pid: u32) -> std::io::Result<()> {
    signal_group(pid, nix::sys::signal::Signal::SIGTERM)
}

/// Force-kill a worker (Unix: `SIGKILL`), falling back to `Child::kill()`.
#[cfg(unix)]
pub fn force_kill(pid: u32, child: &mut Child) -> std::io::Result<()> {
    match signal_group(pid, nix::sys::signal::Signal::SIGKILL) {
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
