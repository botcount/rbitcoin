use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::FILE_HEADER_LEN;
use rbitcoin_store::scripthash_head::*;
use rbitcoin_store::scripthash_layout::{ShHeadValue, SH_HEAD_SLOT_SIZE};
use std::time::Instant;

#[test]
fn prefix_shard_of_high_bits_contiguous() {
    // 16 shards: first nibble of byte 0.
    assert_eq!(prefix_shard_of(&[0x00; 32], 16), 0);
    assert_eq!(prefix_shard_of(&[0x0f; 32], 16), 0);
    let mut k = [0u8; 32];
    k[0] = 0x10;
    assert_eq!(prefix_shard_of(&k, 16), 1);
    k[0] = 0x1f;
    assert_eq!(prefix_shard_of(&k, 16), 1);
    k[0] = 0xf0;
    assert_eq!(prefix_shard_of(&k, 16), 15);
    k[0] = 0xff;
    assert_eq!(prefix_shard_of(&k, 16), 15);
    // Lex order of first byte maps to non-decreasing shard ids.
    let mut prev = 0usize;
    for b in 0u16..=255 {
        k[0] = b as u8;
        let s = prefix_shard_of(&k, 16);
        assert!(s >= prev, "b={b:#x} shard={s} prev={prev}");
        prev = s;
    }
    assert_eq!(prefix_shard_of(&[0xab; 32], 1), 0);
}

#[test]
fn head_insert_get_clear() {
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-shhead-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
    let h = ScriptHashHead::create_with_slots(&path, 64).unwrap();
    let mut key = [0u8; 32];
    key[0] = 7;
    let val = ShHeadValue::inline_one(Fk(42));
    h.insert(&key, &val).unwrap();
    assert_eq!(h.get(&key).unwrap().unwrap(), val);
    assert!(h.clear_key(&key).unwrap());
    assert!(h.get(&key).unwrap().is_none());
    // Sidecar tracks occupied; reopen without full scan path for tiny files.
    drop(h);
    let h2 = ScriptHashHead::open(&path).unwrap();
    assert!(h2.is_known_empty() || h2.occupied() >= 1); // soft-clear keeps slot
    assert!(h2.get(&key).unwrap().is_none());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
}

#[test]
fn open_large_without_occ_skips_scan_not_empty() {
    // Body just over OCC_SCAN_BYTE_CAP → skip full scan when .occ missing.
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-shhead-large-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
    // body = slots * 32 B; need body > 16 MiB ⇒ slots > 16MiB/32 = 524_288.
    let min_slots = (OCC_SCAN_BYTE_CAP / SH_HEAD_SLOT_SIZE as u64) + 1;
    let slots = min_slots.next_power_of_two();
    assert!(slots * SH_HEAD_SLOT_SIZE as u64 > OCC_SCAN_BYTE_CAP);
    let h = ScriptHashHead::create_with_slots(&path, slots).unwrap();
    assert!(h.is_known_empty());
    let mut key = [0u8; 32];
    key[0] = 0xab;
    h.insert(&key, &ShHeadValue::inline_one(Fk(1))).unwrap();
    assert_eq!(h.occupied(), 1);
    drop(h);
    // Drop sidecar only — reopen must not scan body and must not claim empty.
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
    let t0 = Instant::now();
    let h2 = ScriptHashHead::open(&path).unwrap();
    let open_ms = t0.elapsed().as_millis();
    assert!(
        !h2.is_known_empty(),
        "missing .occ on large head must not report empty"
    );
    assert!(
        open_ms < 2_000,
        "open without .occ must skip full scan (took {open_ms}ms)"
    );
    // Lookups still work (probe, not occupancy).
    assert_eq!(h2.get(&key).unwrap().unwrap().inline_fks(), vec![Fk(1)]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
}

#[test]
fn scripthash_head_insert_many_full_without_rehash() {
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-shhead-full-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let h = ScriptHashHead::create_with_slots(&path, 8).unwrap();
    h.insert_many(&[]).unwrap();
    let mut k0 = [0u8; 32];
    k0[0] = 1;
    h.insert(&k0, &ShHeadValue::Empty).unwrap();
    assert!(!h.clear_key(&k0).unwrap());

    let mut n = 0u64;
    loop {
        let mut key = [0u8; 32];
        key[0..8].copy_from_slice(&n.to_le_bytes());
        match h.insert(&key, &ShHeadValue::inline_one(Fk(n + 1))) {
            Ok(()) => n += 1,
            Err(StoreError::Corrupt(SH_HEAD_FULL)) => break,
            Err(e) => panic!("unexpected {e}"),
        }
        assert!(n < 16, "must not grow 8-slot ingest OA");
    }
    let mut seen = 0u64;
    h.for_each_occupied(|_full, val| {
        assert!(!val.is_empty());
        seen += 1;
        Ok(())
    })
    .unwrap();
    assert!(seen >= 1);
    h.flush().unwrap();
    h.flush_async().unwrap();
    drop(h);
    let h2 = ScriptHashHead::open(&path).unwrap();
    let mut key = [0u8; 32];
    key[0..8].copy_from_slice(&5u64.to_le_bytes());
    assert!(h2.get(&key).unwrap().is_some());
    {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len((FILE_HEADER_LEN + 3) as u64)
            .unwrap();
    }
    assert!(matches!(
        ScriptHashHead::open(&path),
        Err(StoreError::Corrupt(_))
    ));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sh_head_one_key_roundtrip() {
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-shhead-one-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
    let h = ScriptHashHead::create_with_slots(&path, 64).unwrap();
    let mut key = [0u8; 32];
    key[0] = 3;
    h.insert_many(&[
        (key, ShHeadValue::inline_one(Fk(19))),
        (key, ShHeadValue::inline_one(Fk(20))),
    ])
    .unwrap();
    assert_eq!(
        h.get(&key).unwrap().unwrap(),
        ShHeadValue::inline_one(Fk(20))
    );
    assert_eq!(h.occupied(), 1);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
}

#[test]
fn occ_sidecar_reads_the_count_and_ignores_short() {
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-shocc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
    store_occ_sidecar(&path, 17).unwrap();
    assert_eq!(load_occ_sidecar(&path), Some(17));
    std::fs::write(occ_sidecar_path(&path), b"SHOCC001").unwrap();
    assert_eq!(load_occ_sidecar(&path), None);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(occ_sidecar_path(&path));
}
