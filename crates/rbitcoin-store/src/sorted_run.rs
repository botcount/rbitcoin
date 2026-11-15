//! Worker cap from free RAM. Used by `tx.head` rebuild and scripthash extract.
//!
//! The sorted-run codec is gone. Tip materialize does not write `scripthash.runs`.
//! Store open unlinks leftover names in that directory except `SEAL`.

/// Cap workers at 1 per `per_worker` free bytes (floor 1, clamp 1..=256 vs CPUs).
pub fn workers_for_free_ram(cpus: usize, free_bytes: u64, per_worker: u64) -> usize {
    let cpus = cpus.clamp(1, 256);
    if per_worker == 0 {
        return 1;
    }
    let ram_cap = (free_bytes / per_worker) as usize;
    cpus.min(ram_cap.clamp(1, 256))
}

/// `MemAvailable` from `/proc/meminfo` text (kB → bytes).
#[cfg(any(debug_assertions, target_os = "linux"))]
#[doc(hidden)]
pub fn mem_available_from_meminfo(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemAvailable:") else {
            continue;
        };
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        return Some(kb.saturating_mul(1024));
    }
    None
}

/// Darwin reclaimable pages (`free_count + inactive_count`) × page size.
/// Speculative pages are already inside `free_count`.
#[cfg(any(debug_assertions, target_os = "macos"))]
#[doc(hidden)]
pub fn mem_available_from_darwin_vm(
    page_size: u64,
    free_count: u64,
    inactive_count: u64,
) -> Option<u64> {
    if page_size == 0 {
        return None;
    }
    Some(
        free_count
            .saturating_add(inactive_count)
            .saturating_mul(page_size),
    )
}

#[cfg(target_os = "macos")]
#[doc(hidden)]
pub fn mem_available_from_darwin_host() -> Option<u64> {
    let page_size = {
        // SAFETY: `sysconf` reads the process page size. It takes no pointer.
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n <= 0 {
            return None;
        }
        n as u64
    };
    // SAFETY: the zeroed struct is the `host_statistics64` out-buffer filled below.
    let mut vm = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `vm` and `count` are locals. `mach_host_self` is this process.
    let kr = unsafe {
        // SAFETY: process host port; HOST_VM_INFO64 writes into this stack vm_statistics64.
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut vm as *mut _ as libc::host_info64_t,
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return None;
    }
    mem_available_from_darwin_vm(
        page_size,
        u64::from(vm.free_count),
        u64::from(vm.inactive_count),
    )
}

#[cfg(windows)]
#[repr(C)]
#[allow(dead_code)]
#[doc(hidden)]
pub struct MemoryStatusEx {
    pub dw_length: u32,
    pub dw_memory_load: u32,
    pub ull_total_phys: u64,
    pub ull_avail_phys: u64,
    pub ull_total_page_file: u64,
    pub ull_avail_page_file: u64,
    pub ull_total_virtual: u64,
    pub ull_avail_virtual: u64,
    pub ull_avail_extended_virtual: u64,
}

#[cfg(windows)]
extern "system" {
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
}

#[cfg(windows)]
#[doc(hidden)]
pub fn mem_available_from_windows_host() -> Option<u64> {
    let mut st = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: dw_length is size_of Self; the API fills the remaining fields.
    if unsafe { GlobalMemoryStatusEx(&mut st) } == 0 {
        return None;
    }
    Some(st.ull_avail_phys)
}

/// Bytes the cgroup v2 hierarchy still lets this process charge.
///
/// `proc_self_cgroup` is `/proc/self/cgroup` text; its `0::<path>` line is
/// resolved under `cgroup_root` (the v2 mount). Each level from that cgroup up
/// to `cgroup_root` caps at `min(memory.max, memory.high)` less
/// `memory.current` without its reclaimable file pages (`memory.stat`).
/// `None` when no level sets a limit or the v2 files are absent (v1, hybrid).
#[cfg(any(debug_assertions, target_os = "linux"))]
#[doc(hidden)]
pub fn cgroup_headroom_bytes(cgroup_root: &std::path::Path, proc_self_cgroup: &str) -> Option<u64> {
    let rel = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim()
        .trim_start_matches('/');
    let rel = std::path::Path::new(rel);
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let leaf = cgroup_root.join(rel);
    leaf.ancestors()
        .take(rel.components().count() + 1)
        .filter_map(cgroup_level_headroom)
        .min()
}

#[cfg(any(debug_assertions, target_os = "linux"))]
fn cgroup_level_headroom(dir: &std::path::Path) -> Option<u64> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let limit = ["memory.max", "memory.high"]
        .into_iter()
        .filter_map(|name| read(name)?.trim().parse::<u64>().ok())
        .min()?;
    let current: u64 = read("memory.current")?.trim().parse().ok()?;
    // `memory.current` counts the page cache, which the kernel reclaims
    // before the OOM killer runs. `file` would also count shmem. No
    // `memory.stat` counts as nothing to reclaim.
    let reclaimable: u64 = read("memory.stat").map_or(0, |stat| {
        stat.lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(key, _)| matches!(*key, "active_file" | "inactive_file"))
            .filter_map(|(_, v)| v.trim().parse::<u64>().ok())
            .sum()
    });
    Some(limit.saturating_sub(current.saturating_sub(reclaimable)))
}

/// Memory this process can still allocate.
///
/// Linux: `MemAvailable`, capped by cgroup v2 headroom
/// (`cgroup_headroom_bytes`) so `MemoryMax=` / `docker --memory` bound the
/// worker count. Darwin: free+inactive pages. Windows: `ullAvailPhys`.
/// Other OS: `None` (worker cap falls back to 1).
pub fn mem_available_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let host = mem_available_from_meminfo(&text)?;
        let cgroup = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|t| cgroup_headroom_bytes(std::path::Path::new("/sys/fs/cgroup"), &t));
        Some(cgroup.map_or(host, |c| host.min(c)))
    }
    #[cfg(target_os = "macos")]
    {
        mem_available_from_darwin_host()
    }
    #[cfg(windows)]
    {
        return mem_available_from_windows_host();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// [`mem_available_bytes`] as a log label (`"12.3"` GiB, or `"?"` when unknown).
pub fn free_gib_label() -> String {
    mem_available_bytes()
        .map(|b| format!("{:.1}", b as f64 / (1u64 << 30) as f64))
        .unwrap_or_else(|| "?".into())
}

pub fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 256)
}
