use rbitcoin_store::bdz::BdzMphf;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::tx_head_mphf::*;
use std::path::PathBuf;

use rbitcoin_store::uring_session::{with_drain_hard_cap, IoCtx, SessionKind, UringSession};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rbitcoin-tx-mphf-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn index_batch_held_session_submits_g_pages() {
    let dir = tmp("held");
    std::fs::create_dir_all(&dir).unwrap();
    let keys: Vec<u64> = (0..200u64).map(|i| i * 17 + 3).collect();
    let ram = BdzMphf::build(&keys).unwrap();
    let p = dir.join("t.mphf");
    ram.write_packed_to(&p).unwrap();
    let fd = BdzMphf::read_packed_from(&p).unwrap();
    let serial = fd.index_batch(&keys[..8], &mut IoCtx::none()).unwrap();
    let mut session = UringSession::try_open_kind(SessionKind::Pool, 32).expect("pool");
    let mut ctx = IoCtx::held(&mut session);
    let batch = fd.index_batch(&keys[..8], &mut ctx).unwrap();
    session.drain_all().unwrap();
    assert_eq!(batch, serial);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Held-session leftover `KIND_BULK_PREAD` must not be harvested as a BDZ
/// g-page CQE (`bdz g page bad slot`). A new wave cannot drain a previous
/// frame's SQEs, so it fails closed instead.
#[test]
fn index_batch_held_session_rejects_foreign_kind_leftover() {
    use std::io::Write;
    let dir = tmp("held-leftover");
    std::fs::create_dir_all(&dir).unwrap();
    let keys: Vec<u64> = (0..200u64).map(|i| i * 17 + 3).collect();
    let ram = BdzMphf::build(&keys).unwrap();
    let p = dir.join("t.mphf");
    ram.write_packed_to(&p).unwrap();
    let fd_mphf = BdzMphf::read_packed_from(&p).unwrap();

    let leftover_path = dir.join("leftover.bin");
    let mut leftover_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&leftover_path)
        .unwrap();
    leftover_file.write_all(&[0xAAu8; 8]).unwrap();
    leftover_file.sync_all().unwrap();
    let leftover_fd = rbitcoin_store::io_handle::IoHandle::from_file(&leftover_file);

    let mut session = UringSession::try_open_kind(SessionKind::Pool, 32).expect("pool");
    session.begin_batch().unwrap();
    let mut leftover_buf = [0u8; 8];
    let ud = rbitcoin_store::uring_session::pack_ud(
        rbitcoin_store::uring_session::KIND_BULK_PREAD,
        session.epoch(),
        0,
    );
    // SAFETY: `leftover_buf` lives until the explicit drain below.
    unsafe { session.push_pread(leftover_fd, 0, &mut leftover_buf, ud) }.unwrap();
    session.submit().unwrap();
    assert!(
        session.in_flight() > 0,
        "foreign SQE must still be pending when BDZ starts"
    );

    with_drain_hard_cap(std::time::Duration::from_millis(200), || {
        let mut ctx = IoCtx::held(&mut session);
        let err = fd_mphf.index_batch(&keys[..8], &mut ctx).unwrap_err();
        assert!(
            matches!(&err, StoreError::Corrupt(msg) if msg.contains("undrained")),
            "held index_batch must reject leftover, got {err:?}"
        );
        assert!(session.is_poisoned());
    });
    // The rejected index_batch never reached stream_g_pages' own drain.
    // Drain the real pread while the buffer and file are still alive.
    session
        .drain_all()
        .expect("drain the leftover pread before the buffer dies");
    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tx_head_mphf_open_is_header_only() {
    let dir = tmp("open");
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("000000");
    let pairs: Vec<(u64, u32)> = (1..64u32)
        .map(|i| (u64::from(i).wrapping_mul(0x9e37_79b9_7f4a_7c15), i))
        .collect();
    let h = TxHeadMphf::write(&base, &pairs).unwrap();
    assert_eq!(h.mphf.g_bytes_resident(), 0);
    assert!(!rel_path(&base).is_file());
    assert_eq!(&std::fs::read(mphf_path(&base)).unwrap()[0..4], b"BDZ2");
    let slots = h.slots_for(&[pairs[0].0]).unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0], pairs[0].1 - 1);
    let rels = h
        .read_rels_batch(&slots, &mut rbitcoin_store::IoCtx::none())
        .unwrap();
    assert_eq!(rels[0][0], pairs[0].1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_assigned_pairs_unique_and_bip30_newest() {
    assert!(group_assigned_pairs(&mut []).unwrap().keys.is_empty());
    assert!(matches!(
        group_assigned_pairs(&mut [(1, 0)]),
        Err(StoreError::Corrupt(_))
    ));
    let mut pairs = vec![(10u64, 2u32), (20, 1), (10, 5), (10, 3), (30, 4)];
    let g = group_assigned_pairs(&mut pairs).unwrap();
    assert_eq!(g.modulus, 5);
    let mut got: Vec<(u64, u32)> = g
        .keys
        .iter()
        .copied()
        .zip(g.values.iter().copied())
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(10, 4), (20, 0), (30, 3)]);
    assert_eq!(g.mlt.get(&4), Some(&vec![3, 2]));
    assert_eq!(g.mlt.len(), 1);
}

#[test]
fn tx_head_mphf_unlinks_leftover_rel_and_bip30_mlt() {
    let dir = tmp("leftover-rel");
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("000000");
    let key = 0xB1B0_u64;
    let pairs = vec![(key, 1u32), (0x2222, 2), (key, 3)];
    std::fs::write(rel_path(&base), [1u8, 2, 3, 4]).unwrap();
    let h = TxHeadMphf::write(&base, &pairs).unwrap();
    assert!(!rel_path(&base).is_file());
    let slots = h.slots_for(&[key]).unwrap();
    assert_eq!(slots, vec![2]);
    let rels = h
        .read_rels_batch(&slots, &mut rbitcoin_store::IoCtx::none())
        .unwrap();
    assert_eq!(rels[0], vec![3, 1]);
    std::fs::write(rel_path(&base), [9u8; 4]).unwrap();
    let h2 = TxHeadMphf::open(&base).unwrap();
    assert!(!rel_path(&base).is_file());
    assert_eq!(
        h2.read_rels_batch(&slots, &mut rbitcoin_store::IoCtx::none())
            .unwrap()[0],
        vec![3, 1]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mlt_reads_slot_rels_and_refuses_a_short_header() {
    let dir = tmp("mlt-fields");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("base.mlt");
    let mut m = std::collections::HashMap::new();
    m.insert(0x0102_0304u32, vec![0x1111_1111u32, 0x2222_2222]);
    write_mlt(&path, &m).unwrap();
    let got = read_mlt(&path).unwrap();
    assert_eq!(
        got.get(&0x0102_0304).map(|v| v.as_slice()),
        Some([0x1111_1111u32, 0x2222_2222].as_slice())
    );
    std::fs::write(&path, [1u8, 2, 3]).unwrap();
    match read_mlt(&path) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("short"), "{msg}"),
        other => panic!("{other:?}"),
    }
    std::fs::write(&path, 1u32.to_le_bytes()).unwrap();
    match read_mlt(&path) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("truncated"), "{msg}"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
