use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::FILE_HEADER_LEN;
use rbitcoin_store::spender_table::*;

#[test]
fn append_get_chain() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-spender-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let t = SpenderTable::create(&dir).unwrap();
    assert!(dir.join("spent.ovf").exists(), "schema 17 name");
    assert!(
        !dir.join("spenders.body").exists(),
        "legacy filename must not be created"
    );
    let a = t.append(Fk(10), 3, Fk::NULL).unwrap();
    let b = t.append(Fk(11), 4, a).unwrap();
    assert_eq!(t.get(a).unwrap(), (Fk(10), 3, Fk::NULL));
    assert_eq!(t.get(b).unwrap(), (Fk(11), 4, a));
    assert_eq!(t.count(), 2);
    assert!(matches!(
        t.append(Fk::NULL, 0, Fk::NULL),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(t.get(Fk::NULL), Err(StoreError::InvalidFk)));
    assert!(matches!(t.get(Fk(99)), Err(StoreError::NotFound)));
    t.flush().unwrap();
    t.flush_async().unwrap();
    drop(t);
    let t = SpenderTable::open(&dir).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get(a).unwrap(), (Fk(10), 3, Fk::NULL));
    let dir2 = dir.with_extension("empty");
    let _ = std::fs::remove_dir_all(&dir2);
    std::fs::create_dir_all(&dir2).unwrap();
    let t2 = SpenderTable::open(&dir2).unwrap();
    assert_eq!(t2.count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn open_rejects_bad_body_size() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-spender-bad-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let t = SpenderTable::create(&dir).unwrap();
    t.append(Fk(1), 0, Fk::NULL).unwrap();
    drop(t);
    // Shrink below HWM so open clamps logical_len to a non-multiple of 16.
    let body = dir.join("spent.ovf");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&body)
        .unwrap()
        .set_len((FILE_HEADER_LEN + 3) as u64)
        .unwrap();
    assert!(matches!(
        SpenderTable::open(&dir),
        Err(StoreError::Corrupt(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_renames_leftover_spenders_body_to_spent_ovf() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-spender-legacy-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    {
        let t = SpenderTable::create(&dir).unwrap();
        t.append(Fk(7), 9, Fk::NULL).unwrap();
        t.flush().unwrap();
    }
    std::fs::rename(dir.join("spent.ovf"), dir.join("spenders.body")).unwrap();
    assert!(!dir.join("spent.ovf").exists());
    let t = SpenderTable::open(&dir).unwrap();
    assert!(dir.join("spent.ovf").exists());
    assert!(!dir.join("spenders.body").exists());
    assert_eq!(t.count(), 1);
    assert_eq!(t.get(Fk(1)).unwrap(), (Fk(7), 9, Fk::NULL));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_fk_or_vin_overflow_is_corrupt() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-spender-ovf-cap-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let t = SpenderTable::create(&dir).unwrap();
    match t.append(Fk(1u64 << 40), 0, Fk::NULL) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("u40"), "{m}"),
        other => panic!("expected u40 Corrupt, got {other:?}"),
    }
    match t.append(Fk(1), 1u32 << 16, Fk::NULL) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("u16"), "{m}"),
        other => panic!("expected u16 Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spender_get_reads_the_packed_record() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-spender-fields-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let t = SpenderTable::create(&dir).unwrap();
    let next = Fk(0x1111_2222_3333_4444);
    let id = t.append(Fk(0x0102_0304), 0x0107, next).unwrap();
    assert_eq!(t.get(id).unwrap(), (Fk(0x0102_0304), 0x0107, next));
    match t.get(Fk(99)) {
        Err(StoreError::NotFound) => {}
        other => panic!("missing spender must be NotFound, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
