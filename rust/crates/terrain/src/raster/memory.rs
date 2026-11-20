//! Memory budget for decoded terrain windows shared by all open GeoTIFFs.

use std::sync::OnceLock;

/// Overrides the budget in MiB. Zero disables retention, so each sampling batch
/// decodes the windows it touches.
const BUDGET_ENV: &str = "RF_PLANNER_TERRAIN_CACHE_MB";
/// Used only when neither the override nor the host memory size is available.
const FALLBACK_BYTES: u64 = 1 << 30;

/// Half of the physical or container memory limit, unless overridden.
pub(super) fn resident_budget_bytes() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let bytes = std::env::var(BUDGET_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|mib| mib.saturating_mul(1 << 20))
            .or_else(|| memory_limit_bytes().map(|limit| limit / 2))
            .unwrap_or(FALLBACK_BYTES);
        usize::try_from(bytes).unwrap_or(usize::MAX)
    })
}

/// Physical memory, reduced to the cgroup limit inside containers.
#[cfg(target_os = "linux")]
fn memory_limit_bytes() -> Option<u64> {
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?
        .saturating_mul(1024);
    // cgroup v2 reports "max" when unlimited; v1 reports a very large number.
    let container = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .find_map(|path| {
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    });
    Some(container.map_or(total, |limit| limit.min(total)))
}

#[cfg(windows)]
fn memory_limit_bytes() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_physical: u64,
        available_physical: u64,
        total_page_file: u64,
        available_page_file: u64,
        total_virtual: u64,
        available_virtual: u64,
        available_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    let mut status = MemoryStatusEx {
        length: size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_physical: 0,
        available_physical: 0,
        total_page_file: 0,
        available_page_file: 0,
        total_virtual: 0,
        available_virtual: 0,
        available_extended_virtual: 0,
    };
    // SAFETY: the buffer is a valid MEMORYSTATUSEX with its length field set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    (ok && status.total_physical > 0).then_some(status.total_physical)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn memory_limit_bytes() -> Option<u64> {
    None
}
