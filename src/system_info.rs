//! Host and per-process resource sampling via `sysinfo` (cross-platform).
//!
//! One reusable [`System`] instance (no per-cycle reallocation). All values are
//! in BYTES on every OS. Per-PID RSS is the foundation for `workers_capacity`
//! in the Phase 1 SLO controller. Scans are blocking; callers wrap them in
//! `tokio::task::block_in_place`.

use std::collections::HashMap;
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

    /// RSS (bytes) of the given worker PIDs, sampled with a SINGLE process
    /// refresh. Dead/missing PIDs are simply absent from the map.
    pub fn worker_rss_batch(&mut self, pids: &[u32]) -> HashMap<u32, u64> {
        if pids.is_empty() {
            return HashMap::new();
        }
        let sysinfo_pids: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&sysinfo_pids), true);
        let mut out = HashMap::with_capacity(pids.len());
        for &p in pids {
            if let Some(proc) = self.sys.process(Pid::from_u32(p)) {
                out.insert(p, proc.memory());
            }
        }
        out
    }
}

impl Default for ResourceProbe {
    fn default() -> Self {
        Self::new()
    }
}
