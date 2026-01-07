use std::fmt::Debug;

use serde::{Deserialize, Serialize};
use sysinfo::{RefreshKind, System};

/// (CPU_USAGE, RAM_USAGE)
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct SystemLoad(u8, u8);

impl SystemLoad {
    pub fn cpu(&self) -> u8 {
        self.0
    }
    pub fn ram(&self) -> u8 {
        self.1
    }
}

impl Debug for SystemLoad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemLoad")
            .field("CPU", &format!("{}%", &self.cpu()))
            .field("RAM", &format!("{}%", &self.ram()))
            .finish()
    }
}

/// Average of cpu usage of all cores
///
/// returns u8 with range 0-100
async fn avg_cpu_usage(sys: &mut System) -> u8 {
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;

    sys.refresh_cpu_all();

    let mut usage = 0f32;
    let mut n = 0u32;

    for cpu in sys.cpus() {
        n += 1;
        usage += (cpu.cpu_usage() - usage) / n as f32
    }

    usage.clamp(0.0, 100.0) as u8
}

fn memory_usage(sys: &mut System) -> u8 {
    sys.refresh_memory();

    let (used, total) = if let Some(l) = sys.cgroup_limits() {
        (l.rss as f32, l.total_memory as f32)
    } else {
        (sys.used_memory() as f32, sys.total_memory() as f32)
    };

    ((used / total) * 100.0) as u8
}

pub async fn system_load() -> SystemLoad {
    let mut sys = System::new_with_specifics(RefreshKind::everything().without_processes());

    let cpu = avg_cpu_usage(&mut sys).await;
    let ram = memory_usage(&mut sys);

    SystemLoad(cpu, ram)
}
