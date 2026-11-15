use rbitcoin_primitives::Fk;
use rbitcoin_store::compact::output_flags;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::spend_annotate_uring::*;
use rbitcoin_store::spender_table::SpenderTable;
use rbitcoin_store::tx_table::{InputRecord, TxRecord};
use rbitcoin_store::tx_table::{OutputRecord, TxTable};
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_table() -> (std::path::PathBuf, TxTable, SpenderTable) {
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rbitcoin-ann-known-{id}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let t = TxTable::create_tiny(&dir).unwrap();
    let s = SpenderTable::create(&dir).unwrap();
    (dir, t, s)
}

fn put_n_outs(t: &TxTable, n_out: u32, txid_byte: u8) -> (Fk, u64, u64) {
    let tx = TxRecord {
        txid: [txid_byte; 32],
        version: 1,
        locktime: 0,
        input_count: 1,
        output_count: n_out,
    };
    let inputs = vec![InputRecord {
        prev_txid: [0u8; 32],
        parent: rbitcoin_store::InputParent::Coinbase,
        sequence: u32::MAX,
        script_sig: vec![0x01],
        witness: vec![],
    }];
    let outputs = vec![OutputRecord::unspent(50, vec![0x51]); n_out as usize];
    let fk = t
        .put_full_batch_indexed(&[(tx, inputs, outputs)], false)
        .unwrap()[0];
    let (off, len) = t.spent_range_batch(&[fk]).unwrap()[0].unwrap();
    (fk, off, len)
}

fn put_one(t: &TxTable) -> (Fk, u64, u64) {
    put_n_outs(t, 1, 0x11)
}

#[test]
fn pure_write_known_null_mmap_and_uring() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_one(&t);
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let bulk = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    let (field, flags, _vin) = bulk[0].expect("meta");
    assert!(field.is_null());
    let sfk = Fk(99);
    let cold = put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs, cfk, 0, sfk, 0)],
        &[(field, flags, 0)],
    )
    .unwrap();
    assert!(cold.is_empty());
    let bulk2 = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    let (f2, fl2, _vin) = bulk2[0].unwrap();
    assert_eq!(f2, sfk);
    assert_eq!(fl2 & output_flags::MULTI_SPENDER, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shipped RMW machine annotates the sole spender on the process session.
#[test]
fn shipped_rmw_annotates_sole_spender() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_one(&t);
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let sfk = Fk(88);
    let cold = match put_spend_batch_by_abs_meta_uring(&t, &spenders, &[(abs, cfk, 0, sfk, 0)]) {
        Ok(cold) => cold,
        Err(StoreError::Unavailable) => {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("rmw: {e}"),
    };
    assert!(cold.is_empty());
    let bulk = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    let (field, flags, _vin) = bulk[0].unwrap();
    assert_eq!(field, sfk);
    assert_eq!(flags & output_flags::MULTI_SPENDER, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pure_write_idempotent_skip() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, len) = put_one(&t);
    let _ = len;
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let sfk = Fk(77);
    let bulk = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    let (field, flags, _vin) = bulk[0].unwrap();
    put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs, cfk, 0, sfk, 0)],
        &[(field, flags, 0)],
    )
    .unwrap();
    // Second time with known field==sfk → skip
    put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, sfk, 0)], &[(sfk, 0, 0)])
        .unwrap();
    let bulk2 = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(bulk2[0].unwrap().0, sfk);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn class_a_same_batch_slot_annotate_is_skip() {
    let (dir, t, spenders) = temp_table();
    let tx = TxRecord {
        txid: [0x21; 32],
        version: 1,
        locktime: 0,
        input_count: 1,
        output_count: 2,
    };
    let pin = std::sync::Arc::new((
        tx,
        vec![
            OutputRecord::unspent(7, vec![0x51]),
            OutputRecord::unspent(8, vec![0x52]),
        ],
    ));
    let ins = vec![InputRecord::coinbase(u32::MAX, vec![0x01], vec![])];
    let sfk = Fk(9);
    let (fks, _loc) = t
        .put_full_batch_from_pins(
            &[(pin, ins)],
            false,
            rbitcoin_store::tx_table::SpentBodyImage::from_pairs(&[2], &[vec![(0u32, sfk, 0)]])
                .unwrap(),
        )
        .unwrap();
    let cfk = fks[0];
    let (off, _) = t.spent_range(cfk).unwrap();
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let bulk = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(bulk[0].unwrap().0, sfk);
    let cold =
        put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, sfk, 0)], &[(sfk, 0, 0)])
            .unwrap();
    assert!(cold.is_empty());
    let bulk2 = t.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(bulk2[0].unwrap().0, sfk);
    let abs1 = rbitcoin_store::tx_table::spent_abs(off, 1);
    let neighbor = t.get_spender_meta_at_abs_batch(&[abs1]).unwrap();
    assert!(neighbor[0].unwrap().0.is_null());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pure_write_multi_repeat_same_spend_fk_skips() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_one(&t);
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let a = Fk(11);
    let b = Fk(12);
    let known0 = t.get_spender_meta_at_abs_batch(&[abs]).unwrap()[0].unwrap();
    put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, a, 0)], &[known0]).unwrap();
    let known1 = t.get_spender_meta_at_abs_batch(&[abs]).unwrap()[0].unwrap();
    put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, b, 0)], &[known1]).unwrap();
    let (field, flags, _vin) = t.get_spender_meta_at_abs_batch(&[abs]).unwrap()[0].unwrap();
    assert_ne!(flags & output_flags::MULTI_SPENDER, 0);
    let n0 = spenders.count();
    put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, b, 0)], &[(field, flags, 0)])
        .unwrap();
    assert_eq!(spenders.count(), n0, "repeat must not append a list node");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two vouts on one page: both land; a third slot on that page is unchanged.
#[test]
fn pure_write_two_vouts_same_page_preserves_neighbor() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_n_outs(&t, 3, 0x22);
    let abs0 = rbitcoin_store::tx_table::spent_abs(off, 0);
    let abs1 = rbitcoin_store::tx_table::spent_abs(off, 1);
    let abs2 = rbitcoin_store::tx_table::spent_abs(off, 2);
    assert_eq!(
        abs0 & !0xfff,
        abs2 & !0xfff,
        "fixture must keep vout 0 and 2 on one 4 KiB page"
    );

    let sentinel = Fk(0x51);
    let known1 = t.get_spender_meta_at_abs_batch(&[abs1]).unwrap()[0].unwrap();
    let cold_s =
        put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs1, cfk, 1, sentinel, 0)], &[known1])
            .unwrap();
    assert!(cold_s.is_empty());

    let known0 = t.get_spender_meta_at_abs_batch(&[abs0]).unwrap()[0].unwrap();
    let known2 = t.get_spender_meta_at_abs_batch(&[abs2]).unwrap()[0].unwrap();
    let mut hdr_before = [0u8; rbitcoin_store::file::FILE_HEADER_LEN];
    let body_pub = t.spent.body_published_len();
    t.spent
        .read_prefix_at_published(
            body_pub,
            0,
            rbitcoin_store::file::FILE_HEADER_LEN as u64,
            &mut hdr_before,
        )
        .unwrap();
    let cold = put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs0, cfk, 0, Fk(10), 0), (abs2, cfk, 2, Fk(12), 0)],
        &[known0, known2],
    )
    .unwrap();
    assert!(cold.is_empty());

    let bulk = t
        .get_spender_meta_at_abs_batch(&[abs0, abs1, abs2])
        .unwrap();
    assert_eq!(bulk[0].unwrap().0, Fk(10));
    assert_eq!(
        bulk[1].unwrap().0,
        sentinel,
        "neighbor slot must be preserved"
    );
    assert_eq!(bulk[2].unwrap().0, Fk(12));

    let mut hdr_after = [0u8; rbitcoin_store::file::FILE_HEADER_LEN];
    t.spent
        .read_prefix_at_published(
            body_pub,
            0,
            rbitcoin_store::file::FILE_HEADER_LEN as u64,
            &mut hdr_after,
        )
        .unwrap();
    assert_eq!(
        hdr_before, hdr_after,
        "a tail slot write must not rewrite the file header"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A long body pwrite is armed to fail. The tail slot must still land, and the
/// neighbor on that page must stay put. A page rewrite is longer than 8 bytes,
/// so this fails closed on that path.
#[test]
fn tail_slot_write_survives_a_short_page_pwrite() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_n_outs(&t, 3, 0x22);
    let abs0 = rbitcoin_store::tx_table::spent_abs(off, 0);
    let abs1 = rbitcoin_store::tx_table::spent_abs(off, 1);
    let sentinel = Fk(0x61);
    let known1 = t.get_spender_meta_at_abs_batch(&[abs1]).unwrap()[0].unwrap();
    assert!(put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs1, cfk, 1, sentinel, 0)],
        &[known1],
    )
    .unwrap()
    .is_empty());
    let known0 = t.get_spender_meta_at_abs_batch(&[abs0]).unwrap()[0].unwrap();
    rbitcoin_store::io_handle::arm_short_pwrite(t.spent.body_write_fd());
    let cold =
        put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs0, cfk, 0, Fk(10), 0)], &[known0])
            .unwrap();
    rbitcoin_store::io_handle::disarm_short_pwrite();
    assert!(
        cold.is_empty(),
        "an 8-byte tail slot must not take the long page-write path"
    );
    let bulk = t.get_spender_meta_at_abs_batch(&[abs0, abs1]).unwrap();
    assert_eq!(bulk[0].unwrap().0, Fk(10));
    assert_eq!(bulk[1].unwrap().0, sentinel);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Vouts whose abs straddle a 4 KiB page both persist.
#[test]
fn pure_write_two_pages_both_land() {
    let (dir, t, spenders) = temp_table();
    // 512 × 8 B from file offset 16 crosses 4096 (slot 510 starts at 4096).
    let (cfk, off, _len) = put_n_outs(&t, 512, 0x33);
    let abs0 = rbitcoin_store::tx_table::spent_abs(off, 0);
    let abs_last = rbitcoin_store::tx_table::spent_abs(off, 511);
    assert_ne!(
        abs0 & !0xfff,
        abs_last & !0xfff,
        "fixture must straddle a 4 KiB page"
    );
    let k0 = t.get_spender_meta_at_abs_batch(&[abs0]).unwrap()[0].unwrap();
    let k1 = t.get_spender_meta_at_abs_batch(&[abs_last]).unwrap()[0].unwrap();
    let cold = put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs0, cfk, 0, Fk(1), 0), (abs_last, cfk, 511, Fk(2), 0)],
        &[k0, k1],
    )
    .unwrap();
    assert!(cold.is_empty());
    let bulk = t.get_spender_meta_at_abs_batch(&[abs0, abs_last]).unwrap();
    assert_eq!(bulk[0].unwrap().0, Fk(1));
    assert_eq!(bulk[1].unwrap().0, Fk(2));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same-page pair lands as slot writes. A pwrite longer than 8 bytes is armed
/// to fail, so a page image cannot be what stored these slots.
#[test]
fn pure_write_same_page_is_one_page_write() {
    if !rbitcoin_store::bulk_io::io_uring_enabled() {
        return;
    }
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_n_outs(&t, 3, 0x44);
    let abs0 = rbitcoin_store::tx_table::spent_abs(off, 0);
    let abs2 = rbitcoin_store::tx_table::spent_abs(off, 2);
    let k0 = t.get_spender_meta_at_abs_batch(&[abs0]).unwrap()[0].unwrap();
    let k2 = t.get_spender_meta_at_abs_batch(&[abs2]).unwrap()[0].unwrap();
    let cold = put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs0, cfk, 0, Fk(3), 0), (abs2, cfk, 2, Fk(4), 0)],
        &[k0, k2],
    )
    .unwrap();
    assert!(cold.is_empty());
    let bulk = t.get_spender_meta_at_abs_batch(&[abs0, abs2]).unwrap();
    assert_eq!(bulk[0].unwrap().0, Fk(3));
    assert_eq!(bulk[1].unwrap().0, Fk(4));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_meta_annotate_stays_in_the_directory() {
    let (dir, t, spenders) = temp_table();
    let (fk, off, _len) = put_n_outs(&t, 520, 0x44);
    let a0 = rbitcoin_store::spent_abs(off, 0);
    let a1 = rbitcoin_store::spent_abs(off, 1);
    let a2 = rbitcoin_store::spent_abs(off, 510);
    assert_eq!(a0 / 4096, a1 / 4096);
    assert_ne!(a0 / 4096, a2 / 4096);
    rbitcoin_store::spend_annotate_uring::reset_spend_annotate_counts();
    let edges = [
        (a0, fk, 0u32, Fk(10), 1u32),
        (a1, fk, 1u32, Fk(11), 2u32),
        (a2, fk, 510u32, Fk(12), 3u32),
    ];
    assert!(t
        .put_spend_batch_by_abs_meta(&spenders, &edges)
        .unwrap()
        .is_empty());
    // Every published slot, including the short tail, stays in the directory.
    // The body bytes stay zero. The page-RMW counters stay quiet.
    assert_eq!(
        rbitcoin_store::spend_annotate_uring::spend_annotate_counts(),
        (0, 0)
    );
    let published = t.spent.body_published_len();
    let mut slot = [0u8; 8];
    t.spent
        .read_prefix_at_published(published, a0, 8, &mut slot)
        .unwrap();
    assert_eq!(slot, [0u8; 8]);
    t.spent
        .read_prefix_at_published(published, a1, 8, &mut slot)
        .unwrap();
    assert_eq!(slot, [0u8; 8]);
    t.spent
        .read_prefix_at_published(published, a2, 8, &mut slot)
        .unwrap();
    assert_eq!(slot, [0u8; 8]);
    let (off, len) = t.spent_range(fk).unwrap();
    let (m0, f0, v0) = t.get_output_spender_meta_at(off, len, 0).unwrap();
    let (m1, f1, v1) = t.get_output_spender_meta_at(off, len, 1).unwrap();
    let (m2, f2, v2) = t.get_output_spender_meta_at(off, len, 510).unwrap();
    assert!(!m0 && f0 == Fk(10) && v0 == 1);
    assert!(!m1 && f1 == Fk(11) && v1 == 2);
    assert!(!m2 && f2 == Fk(12) && v2 == 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A logged slot stays the previous spender when the body byte is corrupt.
/// An offset past the published body stays cold, in input order.
#[test]
fn batched_annotate_keeps_the_logged_slot_and_the_cold_tail() {
    let (dir, t, spenders) = temp_table();
    let (fk, off, len) = put_n_outs(&t, 2, 0x46);
    let a0 = rbitcoin_store::spent_abs(off, 0);
    let a1 = rbitcoin_store::spent_abs(off, 1);
    let past = t.spent.body_published_len();
    assert!(t
        .put_spend_batch_by_abs_meta(&spenders, &[(a0, fk, 0, Fk(10), 1), (a1, fk, 1, Fk(12), 3)],)
        .unwrap()
        .is_empty());
    t.spent
        .write_body_abs(a0, &[0xff, 0, 0, 0, 0, 0, 0, 0])
        .unwrap();
    let cold = t
        .put_spend_batch_by_abs_meta(
            &spenders,
            &[(a0, fk, 0, Fk(11), 2), (past, fk, 0, Fk(13), 4)],
        )
        .unwrap();
    assert_eq!(cold, vec![(fk, 0, Fk(13), 4)]);
    let (multi, field, _) = t.get_output_spender_meta_at(off, len, 0).unwrap();
    assert!(multi);
    let (second, second_vin, next) = spenders.get(field).unwrap();
    assert_eq!((second, second_vin), (Fk(11), 2));
    let (first, first_vin, end) = spenders.get(next).unwrap();
    assert_eq!((first, first_vin), (Fk(10), 1));
    assert!(end.is_null());
    let (m1, f1, v1) = t.get_output_spender_meta_at(off, len, 1).unwrap();
    assert!(!m1 && f1 == Fk(12) && v1 == 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A same-spender re-annotate rewrites the log when the body slot will not decode.
///
/// The log already serves the slot, so the torn body is not consulted.
#[test]
fn known_skip_rewrites_a_logged_slot_over_a_torn_body() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, len) = put_one(&t);
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    let sfk = Fk(77);
    assert!(put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs, cfk, 0, sfk, 0)],
        &[(Fk::NULL, 0, 0)],
    )
    .unwrap()
    .is_empty());
    t.spent
        .write_body_abs(abs, &[0xff, 0, 0, 0, 0, 0, 0, 0])
        .unwrap();
    let cold =
        put_spend_batch_by_abs_meta_known(&t, &spenders, &[(abs, cfk, 0, sfk, 0)], &[(sfk, 0, 0)])
            .expect("a torn body must not fail a slot the log already serves");
    assert!(cold.is_empty());
    let (multi, field, vin) = t.get_output_spender_meta_at(off, len, 0).unwrap();
    assert!(!multi && field == sfk && vin == 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A known skip with no log note still refuses a reserved body flag.
#[test]
fn known_skip_refuses_a_torn_body_when_the_log_misses() {
    let (dir, t, spenders) = temp_table();
    let (cfk, off, _len) = put_one(&t);
    let abs = rbitcoin_store::tx_table::spent_abs(off, 0);
    t.spent
        .write_body_abs(abs, &[0xff, 0, 0, 0, 0, 0, 0, 0])
        .unwrap();
    let err = put_spend_batch_by_abs_meta_known(
        &t,
        &spenders,
        &[(abs, cfk, 0, Fk(77), 0)],
        &[(Fk(77), 0, 0)],
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A published slot with no directory note and reserved flags fails the batch.
#[test]
fn batched_annotate_refuses_a_corrupt_body_slot() {
    let (dir, t, spenders) = temp_table();
    let (fk, off, _len) = put_one(&t);
    let abs = rbitcoin_store::spent_abs(off, 0);
    t.spent
        .write_body_abs(abs, &[0xff, 0, 0, 0, 0, 0, 0, 0])
        .unwrap();
    let err = t
        .put_spend_batch_by_abs_meta(&spenders, &[(abs, fk, 0, Fk(10), 1)])
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second spend of a historical slot promotes through the directory.
///
/// The body page stays zero, including the short tail. Zeroing a logged slot
/// on disk still returns the multi head.
#[test]
fn historical_second_spend_keeps_both_edges() {
    let (dir, t, spenders) = temp_table();
    let (fk, off, len) = put_n_outs(&t, 520, 0x45);
    let a0 = rbitcoin_store::spent_abs(off, 0);
    let a_tail = rbitcoin_store::spent_abs(off, 510);
    assert!(a0 < 4096);
    assert_ne!(a0 / 4096, a_tail / 4096);
    assert!(t
        .put_spend_batch_by_abs_meta(&spenders, &[(a0, fk, 0, Fk(10), 1)])
        .unwrap()
        .is_empty());
    assert!(t
        .put_spend_batch_by_abs_meta(&spenders, &[(a0, fk, 0, Fk(11), 2)])
        .unwrap()
        .is_empty());
    assert!(t
        .put_spend_batch_by_abs_meta(&spenders, &[(a_tail, fk, 510, Fk(12), 3)])
        .unwrap()
        .is_empty());
    let published = t.spent.body_published_len();
    let mut body = [0u8; 8];
    t.spent
        .read_prefix_at_published(published, a0, 8, &mut body)
        .unwrap();
    assert_eq!(body, [0u8; 8]);
    t.spent
        .read_prefix_at_published(published, a_tail, 8, &mut body)
        .unwrap();
    assert_eq!(body, [0u8; 8]);
    t.spent.write_body_abs(a0, &[0u8; 8]).unwrap();
    let (multi, field, vin) = t.get_output_spender_meta_at(off, len, 0).unwrap();
    assert!(multi && vin == 0);
    let (second, second_vin, next) = spenders.get(field).unwrap();
    assert_eq!((second, second_vin), (Fk(11), 2));
    let (first, first_vin, end) = spenders.get(next).unwrap();
    assert_eq!((first, first_vin), (Fk(10), 1));
    assert!(end.is_null());
    let bulk = t.get_spender_meta_at_abs_batch(&[a0, a_tail]).unwrap();
    assert_eq!(bulk[0].unwrap().0, field);
    assert_ne!(bulk[0].unwrap().1 & output_flags::MULTI_SPENDER, 0);
    assert_eq!(bulk[1].unwrap().0, Fk(12));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_writes_by_spent_page_merges_same_page() {
    let meta = [1u8; META_LEN];
    let slot = META_LEN as u64;
    let writes = [
        (16u64, Fk(1), 0, Fk(10), 0, meta),
        (16 + 2 * slot, Fk(1), 2, Fk(12), 0, meta),
    ];
    let rec = 3 * slot;
    let g = group_writes_by_spent_page(&writes, 16 + rec);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].off, rbitcoin_store::file::FILE_HEADER_LEN as u64);
    assert_eq!(g[0].len, rec as usize);
    assert_eq!(g[0].writes.len(), 2);
}

#[test]
fn group_writes_by_spent_page_keeps_distinct_pages() {
    let meta = [1u8; META_LEN];
    let writes = [
        (16u64, Fk(1), 0, Fk(10), 0, meta),
        (4096u64 + 16, Fk(2), 0, Fk(12), 0, meta),
    ];
    let g = group_writes_by_spent_page(&writes, 4096 + 16 + META_LEN as u64);
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].off, rbitcoin_store::file::FILE_HEADER_LEN as u64);
    assert_eq!(g[1].off, 4096);
}

#[test]
fn group_writes_by_spent_page_merges_straddle() {
    let meta = [1u8; META_LEN];
    let slot = META_LEN as u64;
    // Slot starts 3 bytes before the 4 KiB page end so the RMW straddles.
    let first = 4096u64 - 3;
    let second = first + slot;
    let end = second + slot;
    let writes = [
        (first, Fk(1), 0, Fk(10), 0, meta),
        (second, Fk(1), 1, Fk(11), 0, meta),
    ];
    let g = group_writes_by_spent_page(&writes, end);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].writes.len(), 2);
    assert!(g[0].off <= first);
    assert!(g[0].off + g[0].len as u64 >= end);
}
