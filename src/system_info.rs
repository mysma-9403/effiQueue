//! Host and per-process resource sampling via `sysinfo` (cross-platform).
//!
//! One reusable [`System`] instance (no per-cycle reallocation). All values are
//! in BYTES on every OS. Per-PID RSS is the foundation for `workers_capacity`
//! in the Phase 1 SLO controller.

use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Host memory/swap snapshot, in bytes.
pub struct SystemData {
    pub memory_used: u64,
    pub memory_total: u64,
    pub total_swap: u64,
    pub used_swap: u64,
}

/// Reusable resource prober — holds a single `System` across cycles.
pub struct ResourceProbe {
    sys: System,
}

impl ResourceProbe {
    pub fn new() -> Self {
        let sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::everything())
                .with_processes(ProcessRefreshKind::nothing().with_memory()),
        );
        Self { sys }
    }

    /// Host memory and swap, in bytes.
    pub fn host_memory(&mut self) -> SystemData {
        self.sys.refresh_memory();
        SystemData {
            memory_used: self.sys.used_memory(),
            memory_total: self.sys.total_memory(),
            total_swap: self.sys.total_swap(),
            used_swap: self.sys.used_swap(),
        }
    }

    /// RSS of a single worker (bytes), if the process is alive.
    pub fn worker_rss(&mut self, pid: u32) -> Option<u64> {
        let p = Pid::from_u32(pid);
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&[p]), true);
        self.sys.process(p).map(|proc| proc.memory())
    }
}

impl Default for ResourceProbe {
    fn default() -> Self {
        Self::new()
    }
}
