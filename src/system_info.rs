// W pliku src/system_info.rs

use sysinfo::{System, Component, Disk, NetworkData};

pub struct SystemData {
    pub memory_used: u64,
    pub memory_total: u64,
    pub total_swap: u64,
    pub used_swap: u64,
}

impl SystemData {
    pub fn new() -> Self {
        let mut sys = System::new_all(); // Tworzy nową instancję `System` i zbiera informacje o wszystkim.
        sys.refresh_all(); // Odświeża informacje o systemie.

        let memory_used = sys.used_memory();
        let memory_total = sys.total_memory();
        let total_swap = sys.total_swap();
        let used_swap = sys.used_swap();

        SystemData {
            memory_used,
            memory_total,
            total_swap,
            used_swap
        }
    }
}
