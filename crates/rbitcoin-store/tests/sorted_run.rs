use rbitcoin_store::sorted_run::*;

#[test]
fn workers_for_free_ram_1gib_head_is_not_sh_1_5gib() {
    const HEAD: u64 = rbitcoin_store::tx_table::TX_HEAD_REBUILD_WORKER_FREE_RAM_BYTES;
    assert_eq!(workers_for_free_ram(8, 0, HEAD), 1);
    assert_eq!(workers_for_free_ram(8, HEAD.saturating_sub(1), HEAD), 1);
    assert_eq!(workers_for_free_ram(8, HEAD, HEAD), 1);
    assert_eq!(workers_for_free_ram(8, 2 * HEAD, HEAD), 2);
    assert_eq!(workers_for_free_ram(8, 3 * HEAD, HEAD), 3);
    assert_eq!(workers_for_free_ram(4, 20 * HEAD, HEAD), 4);
    assert_eq!(workers_for_free_ram(8, 3 * (1 << 30) / 2, HEAD), 1);
    assert_eq!(workers_for_free_ram(8, 2 * (1 << 30), HEAD), 2);
}

#[test]
fn mem_available_parses_proc_meminfo() {
    let text = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    3145728 kB\nBuffers:          200000 kB\n";
    assert_eq!(mem_available_from_meminfo(text), Some(3145728 * 1024));
    assert_eq!(mem_available_from_meminfo("MemTotal: 1 kB\n"), None);
}

#[test]
fn darwin_vm_pages_count_as_free_ram() {
    assert_eq!(mem_available_from_darwin_vm(0, 10, 10), None);
    assert_eq!(mem_available_from_darwin_vm(4096, 0, 0), Some(0));
    assert_eq!(mem_available_from_darwin_vm(4096, 1, 0), Some(4096));
    assert_eq!(mem_available_from_darwin_vm(16384, 2, 2), Some(4 * 16384));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn mem_available_bytes_is_some_on_linux_macos_windows() {
    // A cgroup at its limit reports 0 (`cgroup_usage_over_limit_leaves_zero_headroom`).
    let n = mem_available_bytes().expect("Linux MemAvailable / Darwin vm / Windows AvailPhys");
    let w = workers_for_free_ram(logical_cpus(), n, 2 << 30);
    assert!((1..=256).contains(&w));
    assert!(w <= logical_cpus());
    let label = free_gib_label();
    assert!(label == "?" || label.parse::<f64>().is_ok(), "{label}");
}

const GIB: u64 = 1 << 30;

fn cgroup_level(root: &std::path::Path, rel: &str, max: &str, high: &str, current: u64) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).expect("cgroup dir");
    std::fs::write(dir.join("memory.max"), format!("{max}\n")).expect("memory.max");
    std::fs::write(dir.join("memory.high"), format!("{high}\n")).expect("memory.high");
    std::fs::write(dir.join("memory.current"), format!("{current}\n")).expect("memory.current");
}

const SERVICE: &str = "0::/system.slice/rbitcoin.service\n";

#[test]
fn cgroup_memory_max_below_mem_available_limits_headroom() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-max").expect("tmp");
    cgroup_level(&tmp.path, "system.slice", "max", "max", 5 * GIB);
    let max = (4 * GIB).to_string();
    cgroup_level(&tmp.path, "system.slice/rbitcoin.service", &max, "max", GIB);
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), Some(3 * GIB));
}

#[test]
fn cgroup_all_max_is_unlimited() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-unl").expect("tmp");
    cgroup_level(&tmp.path, "system.slice", "max", "max", GIB);
    cgroup_level(
        &tmp.path,
        "system.slice/rbitcoin.service",
        "max",
        "max",
        GIB,
    );
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), None);
}

#[test]
fn cgroup_ancestor_tighter_than_leaf_wins() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-anc").expect("tmp");
    let slice = (6 * GIB).to_string();
    cgroup_level(&tmp.path, "system.slice", &slice, "max", 5 * GIB);
    let leaf = (16 * GIB).to_string();
    cgroup_level(
        &tmp.path,
        "system.slice/rbitcoin.service",
        &leaf,
        "max",
        2 * GIB,
    );
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), Some(GIB));
}

#[test]
fn cgroup_memory_high_tighter_than_max_wins() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-high").expect("tmp");
    let max = (8 * GIB).to_string();
    let high = (3 * GIB).to_string();
    cgroup_level(&tmp.path, "system.slice/rbitcoin.service", &max, &high, GIB);
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), Some(2 * GIB));
}

#[test]
fn cgroup_usage_over_limit_leaves_zero_headroom() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-over").expect("tmp");
    let max = (2 * GIB).to_string();
    cgroup_level(
        &tmp.path,
        "system.slice/rbitcoin.service",
        &max,
        "max",
        3 * GIB,
    );
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), Some(0));
}

/// After IBD the page cache fills a `MemoryMax=` cgroup. The kernel reclaims
/// it before the OOM killer runs, so it is headroom.
#[test]
fn cgroup_file_cache_counts_as_headroom() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-cache").expect("tmp");
    let max = (7 * GIB).to_string();
    let leaf = "system.slice/rbitcoin.service";
    cgroup_level(&tmp.path, leaf, &max, "max", 7 * GIB - (64 << 20));
    let stat = format!(
        "anon {}\nfile {}\nactive_file {}\ninactive_file {}\nshmem 0\n",
        2 * GIB,
        5 * GIB,
        GIB,
        4 * GIB
    );
    std::fs::write(tmp.path.join(leaf).join("memory.stat"), stat).expect("memory.stat");
    assert_eq!(
        cgroup_headroom_bytes(&tmp.path, SERVICE),
        Some(5 * GIB + (64 << 20))
    );
}

#[test]
fn cgroup_namespace_root_limit_counts() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-ns").expect("tmp");
    let max = (2 * GIB).to_string();
    cgroup_level(&tmp.path, "", &max, "max", GIB / 2);
    assert_eq!(
        cgroup_headroom_bytes(&tmp.path, "0::/\n"),
        Some(GIB + GIB / 2)
    );
}

#[test]
fn cgroup_missing_files_or_v1_falls_back() {
    let tmp = rbitcoin_store::testutil::TempDir::labeled("cg-miss").expect("tmp");
    assert_eq!(cgroup_headroom_bytes(&tmp.path, SERVICE), None);
    let v1 = "12:memory:/system.slice/rbitcoin.service\n1:name=systemd:/x\n";
    assert_eq!(cgroup_headroom_bytes(&tmp.path, v1), None);
    let max = (2 * GIB).to_string();
    cgroup_level(&tmp.path, "", &max, "max", GIB);
    assert_eq!(cgroup_headroom_bytes(&tmp.path, "0::/../escape\n"), None);
}
