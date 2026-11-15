use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::txid_body::*;
fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("txid-body");
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn append_and_get_roundtrip() {
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    assert_eq!(t.count(), 0);
    let a = [1u8; 32];
    let b = [2u8; 32];
    t.append_batch(0, &[a, b]).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get(Fk(1)).unwrap(), a);
    assert_eq!(t.get(Fk(2)).unwrap(), b);
    assert_eq!(t.get_range(1, 2).unwrap(), vec![a, b]);
    drop(t);
    let t2 = TxidBody::open(&dir).unwrap();
    assert_eq!(t2.count(), 2);
    assert_eq!(t2.get(Fk(2)).unwrap(), b);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Held pool session must fill identity pages via `pread_batch_on_session`
/// (not one-shot `pread_at`). SQE lens prove the session path ran.
#[test]
fn page_grouped_on_pool_session_pushes_sqes() {
    use rbitcoin_store::uring_session::{SessionKind, UringSession};
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    let a = [0x11u8; 32];
    let b = [0x22u8; 32];
    t.append_batch(0, &[a, b]).unwrap();
    let mut session = UringSession::try_open_kind(SessionKind::Pool, 32).expect("pool");
    let (map, pages) = t
        .get_many_page_grouped_on_session(&[Fk(1), Fk(2)], &mut session)
        .expect("session identity");
    session.drain_all().unwrap();
    assert_eq!(pages, 1);
    assert_eq!(map.get(&1), Some(&a));
    assert_eq!(map.get(&2), Some(&b));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn page_grouped_poisoned_held_session_is_fail_closed() {
    use rbitcoin_store::uring_session::{SessionKind, UringSession};
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    let a = [0x11u8; 32];
    t.append_batch(0, &[a]).unwrap();
    let mut session = UringSession::try_open_kind(SessionKind::Pool, 32).expect("pool");
    session.poison();
    let err = t
        .get_many_page_grouped_on_session(&[Fk(1)], &mut session)
        .expect_err("poisoned identity pread");
    assert!(
        err.to_string().contains("poisoned") || err.to_string().contains("io_uring"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn entry_offset_layout() {
    assert_eq!(TxidBody::entry_offset(1).unwrap(), 32);
    assert_eq!(TxidBody::entry_offset(2).unwrap(), 64);
    assert!(TxidBody::entry_offset(0).is_err());
    assert_eq!(TXID_ENTRIES_PER_PAGE, 128);
    assert_eq!(TxidBody::entry_page(1).unwrap(), 0);
    assert_eq!(TxidBody::entry_page(TXID_ENTRIES_PER_PAGE + 1).unwrap(), 1);
}

#[test]
fn truncate_get_many_logical_and_errors() {
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    let a = [0xAAu8; 32];
    let b = [0xBBu8; 32];
    let c = [0xCCu8; 32];
    t.append_batch(0, &[a, b, c]).unwrap();
    assert!(t.logical_len() >= 32 * 3);
    assert!(t.get_many(&[]).unwrap().is_empty());
    let many = t.get_many(&[Fk(1), Fk(2), Fk(99), Fk(3)]).unwrap();
    assert_eq!(many[0], Some(a));
    assert_eq!(many[1], Some(b));
    assert_eq!(many[2], None);
    assert_eq!(many[3], Some(c));
    assert!(matches!(t.get(Fk(99)), Err(StoreError::NotFound)));
    t.truncate_to_count(2).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get(Fk(2)).unwrap(), b);
    assert!(matches!(t.get(Fk(3)), Err(StoreError::NotFound)));
    t.truncate_to_count(2).unwrap();
    assert!(matches!(
        t.truncate_to_count(5),
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        t.append_batch(0, &[[0u8; 32]]),
        Err(StoreError::Corrupt(_))
    ));
    t.append_batch(2, &[]).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Page-grouped multi-get matches serial `get` for dense + sparse fks and
/// issues ≤1 bulk read per OS page touched.
#[test]
fn get_many_page_grouped_matches_serial_and_caps_pages() {
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    // 300 entries → spans multiple 128-entry pages.
    let mut all = Vec::with_capacity(300);
    for i in 0..300u32 {
        let mut x = [0u8; 32];
        x[0..4].copy_from_slice(&i.to_le_bytes());
        all.push(x);
    }
    t.append_batch(0, &all).unwrap();

    let dense: Vec<Fk> = (1u64..=10).map(Fk).collect();
    let (map_d, pages_d) = t.get_many_page_grouped(&dense).unwrap();
    let serial_d = t.get_many_serial(&dense).unwrap();
    assert_eq!(map_d, serial_d);
    assert_eq!(pages_d, 1, "dense same-page fks must be one bulk read");

    // Sparse across pages: fk 1 (page0), 129 (page1), 257 (page2).
    let sparse = vec![Fk(1), Fk(129), Fk(257), Fk(1)]; // dup ok
    let (map_s, pages_s) = t.get_many_page_grouped(&sparse).unwrap();
    let serial_s = t.get_many_serial(&sparse).unwrap();
    assert_eq!(map_s, serial_s);
    assert_eq!(pages_s, 3, "three distinct pages → three bulk reads");
    let many = t.get_many(&sparse).unwrap();
    assert_eq!(many[0], Some(all[0]));
    assert_eq!(many[1], Some(all[128]));
    assert_eq!(many[2], Some(all[256]));
    assert_eq!(many[3], Some(all[0]));

    let cluster: Vec<Fk> = [130u64, 140, 150].into_iter().map(Fk).collect();
    let (_m, pages_c) = t.get_many_page_grouped(&cluster).unwrap();
    assert_eq!(pages_c, 1);

    let (empty_map, p0) = t.get_many_page_grouped(&[Fk(9999), Fk(0)]).unwrap();
    assert!(empty_map.is_empty());
    assert_eq!(p0, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn get_range_typed_rows_and_past_count() {
    let dir = tmp();
    let t = TxidBody::create(&dir).unwrap();
    let a = [0x31u8; 32];
    let b = [0x32u8; 32];
    let c = [0x33u8; 32];
    t.append_batch(0, &[a, b, c]).unwrap();
    assert_eq!(t.get_range(1, 3).unwrap(), vec![a, b, c]);
    assert_eq!(t.get_range(2, 2).unwrap(), vec![b]);
    assert!(t.get_range(3, 1).unwrap().is_empty());
    assert!(matches!(t.get_range(1, 4), Err(StoreError::NotFound)));
    assert!(matches!(t.get_range(0, 1), Err(StoreError::NotFound)));
    let _ = std::fs::remove_dir_all(&dir);
}
