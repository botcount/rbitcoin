use rbitcoin_query::sh_builder::*;
use rbitcoin_store::Store;

use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn durable_sh_inclusion_floor_prefers_hwm() {
    assert_eq!(durable_sh_inclusion_floor(0, 50), 50);
    assert_eq!(durable_sh_inclusion_floor(99, 50), 99);
}

#[test]
fn force_rebuild_resets_seal_and_clears_runs() {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-sh-force-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open_or_create_tiny(&dir).unwrap();
    let b = ShRunBuilder::new(&dir);
    let runs_dir = dir.join("scripthash.runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    store_seal(&runs_dir, 1_400_000_000).unwrap();
    b.refresh_seal();
    assert_eq!(b.sealed_max_create_fk(), 1_400_000_000);
    std::fs::write(runs_dir.join("000001.run"), b"leftover").unwrap();
    assert!(b.on_disk_run_count() > 0);

    b.prepare_force_full_rebuild(&store).unwrap();
    assert_eq!(b.sealed_max_create_fk(), 0);
    assert_eq!(b.on_disk_run_count(), 0);
    assert!(store.scripthash.head_is_empty());
    assert_eq!(store.scripthash.entry_count(), 0);
    assert_eq!(store.scripthash.include_hwm(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_seal_reads_eight_and_ignores_short() {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-seal-width-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(load_seal(&dir), 0);
    store_seal(&dir, 64).unwrap();
    assert_eq!(load_seal(&dir), 64);
    std::fs::write(seal_path(&dir), [1u8, 2, 3]).unwrap();
    assert_eq!(load_seal(&dir), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
