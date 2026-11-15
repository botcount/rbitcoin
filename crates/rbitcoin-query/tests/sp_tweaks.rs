use rbitcoin_primitives::Fk;
use rbitcoin_primitives::Height;
use rbitcoin_query::sp_tweaks::*;
use rbitcoin_query::Query;
use rbitcoin_query::*;
use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

use rbitcoin_query::testutil::FixtureChain;
fn tmp_q() -> (rbitcoin_query::testutil::TempDir, Query) {
    rbitcoin_query::testutil::tiny_query_labeled("sptweaks")
}

#[test]
fn split_store_puts_blockfilter_and_sp_tweaks_on_cold() {
    let root = rbitcoin_query::testutil::TempDir::labeled("cold-indexes").unwrap();
    let hot = root.path().join("hot");
    let cold = root.path().join("cold");
    let layout = rbitcoin_store::StoreLayout::tiny(&hot).with_cold_dir(&cold);
    let q = Query::open_or_create_layout(layout.clone()).unwrap();
    q.set_block_filter_index(true).unwrap();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    assert!(cold.join("blockfilter.idx").is_dir());
    assert!(cold.join("blockfilter.body").is_dir());
    assert!(cold.join("sp_tweaks.idx").is_dir());
    assert!(cold.join("sp_tweaks.body").is_dir());
    assert!(!hot.join("blockfilter.idx").exists());
    assert!(!hot.join("blockfilter.body").exists());
    assert!(!hot.join("sp_tweaks.idx").exists());
    assert!(!hot.join("sp_tweaks.body").exists());
    drop(q);
    let q = Query::open_or_create_layout(layout).unwrap();
    assert_eq!(q.sptweaks_next_height(), Some(Height(0)));
    q.set_block_filter_index(true).unwrap();
    assert!(!hot.join("blockfilter.idx").exists());
    assert!(cold.join("blockfilter.idx").join("meta").is_file());
}

#[test]
fn put_and_load_noop_when_disabled() {
    let (dir, q) = tmp_q();
    assert!(!q.sptweaks_enabled());
    assert!(q.sptweaks_next_height().is_none());
    q.put_sp_tweaks_block(Height(0), Fk(1), &[None]).unwrap();
    q.truncate_sp_tweaks_through_tip(None).unwrap();
    assert!(q.load_thin_tweaks(Height(0)).unwrap().is_none());
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    assert!(q.sptweaks_enabled());
    assert_eq!(q.sptweaks_origin(), Height(0));
    assert_eq!(q.sptweaks_next_height(), Some(Height(0)));
    assert!(q.load_thin_tweaks(Height(0)).unwrap().is_none());
    // Not next height is a no-op.
    q.put_sp_tweaks_block(Height(3), Fk(1), &[None]).unwrap();
    assert_eq!(q.sptweaks_next_height(), Some(Height(0)));
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn put_sp_tweaks_rejects_header_fk_mismatch() {
    let (dir, q) = tmp_q();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let h0 = header(0, Fk::NULL, None);
    let fk0 = q
        .connect_block(
            Height(0),
            &h0,
            &[TxApply {
                tx: TxRecord {
                    txid: [1u8; 32],
                    version: 1,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
            }],
        )
        .unwrap();
    let err = q
        .put_sp_tweaks_block(Height(0), Fk(fk0.0.wrapping_add(1)), &[None])
        .unwrap_err();
    assert!(
        format!("{err}").contains("sp_tweaks put header is not confirmed tip"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thin_tweak_range_limits_default_eligible_cap() {
    let d = ThinTweakRangeLimits::default();
    assert_eq!(d.max_heights, 128);
    assert_eq!(d.max_eligible, 16384);
    assert!(!d.cut_through);
}

#[test]
fn wave_join_is_dense_packed_vs_sparse() {
    assert!(wave_join_is_dense(4, 10, 13));
    assert!(wave_join_is_dense(1, 10, 13));
    assert!(!wave_join_is_dense(1, 10, 14));
    assert!(!wave_join_is_dense(0, 10, 13));
    assert!(wave_join_is_dense(2, 100, 107));
    assert!(!wave_join_is_dense(2, 100, 108));
    assert!(!wave_join_is_dense(1, 20, 10));
}

#[test]
fn thin_tweak_body_corrupt_strings_are_distinct() {
    let missing = require_thin_body_range(None).unwrap_err();
    let empty = require_thin_body_range(Some((8, 0))).unwrap_err();
    let ok = require_thin_body_range(Some((8, 16))).unwrap();
    assert_eq!(ok, (8, 16));
    let m = format!("{missing}");
    let e = format!("{empty}");
    assert!(m.contains("body missing"), "{m}");
    assert!(e.contains("body empty"), "{e}");
    assert_ne!(m, e);
}

fn connect_one(
    q: &Query,
    h: u32,
    prev: Fk,
    prev_hash: Option<[u8; 32]>,
    p2tr: bool,
) -> (Fk, [u8; 32]) {
    let header = header(h, prev, prev_hash);
    let script = if p2tr {
        let mut s = rbitcoin_primitives::P2TR_PREFIX.to_vec();
        s.extend_from_slice(&[0x11; 32]);
        s
    } else {
        vec![0x51]
    };
    let fk = q
        .connect_block(
            Height(h),
            &header,
            &[TxApply {
                tx: TxRecord {
                    txid: [h as u8; 32],
                    version: 1,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                outputs: vec![OutputRecord::unspent(50_0000_0000, script)],
            }],
        )
        .unwrap();
    (fk, header.hash)
}

/// Prune and tweaks together only when the index starts before its origin,
/// or already covers every dropped height. A sealed row still serves.
#[test]
fn prune_seqsigwit_and_sp_tweaks_follow_taproot() {
    let (dir, q) = tmp_q();
    q.set_sptweaks_enabled(true, Height(4)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();

    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    let mut at_origin = Fk::NULL;
    for h in 0..=4 {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, h == 4);
        prev = fk;
        prev_hash = Some(hash);
        if h == 4 {
            at_origin = fk;
        }
    }
    let tweak = [0x02; 33];
    q.put_sp_tweaks_blocks(&[(Height(4), at_origin, vec![Some(tweak)])])
        .unwrap();
    assert_eq!(q.sptweaks_next_height(), Some(Height(5)));
    q.set_pruneheight(Some(Height(4))).unwrap();
    let rows = q.load_thin_tweaks(Height(4)).unwrap().expect("sealed row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tweak, tweak);
    q.set_sptweaks_enabled(true, Height(4)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, q) = tmp_q();
    let _ = connect_one(&q, 0, Fk::NULL, None, false);
    q.set_prune_seqsigwit(true).unwrap();
    q.set_pruneheight(Some(Height(0))).unwrap();
    let err = q.set_sptweaks_enabled(true, Height(0)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("prune-seqsigwit"), "{msg}");
    assert!(msg.contains("taproot"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing is dropped until the tip reaches the keep window. A restart whose
/// tweak watermark is still behind that tip has the witnesses in
/// `seqsigwit.window` and must get through the startup gate.
#[test]
fn pruned_restart_allows_a_tweak_gap_before_anything_is_dropped() {
    let (dir, q) = tmp_q();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    for h in 0..3 {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, false);
        prev = fk;
        prev_hash = Some(hash);
    }
    assert_eq!(q.tip_height(), Some(Height(2)));
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), None);
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.sp_tweaks
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .truncate_keep_slots(0)
        .unwrap();
    assert!(q.sptweaks_next_height().unwrap().0 <= 2);
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tip 287 drops nothing, so adding prune beside a not-yet-enabled index is
/// safe. Tip 288 drops genesis. An index that does not already cover height 0
/// is refused before that cut.
#[test]
fn adding_prune_matches_the_height_that_will_be_dropped() {
    let (dir, q) = tmp_q();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    let keep = Query::SEQSIGWIT_KEEP_HEIGHTS;
    for h in 0..keep {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, false);
        prev = fk;
        prev_hash = Some(hash);
    }
    assert_eq!(q.tip_height(), Some(Height(keep - 1)));
    q.refuse_adding_prune_with_tweaks(Height(0)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, q) = tmp_q();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    for h in 0..=keep {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, false);
        prev = fk;
        prev_hash = Some(hash);
    }
    assert_eq!(q.tip_height(), Some(Height(keep)));
    let before = q.store().txs.seqsigwit_loc.count();
    assert!(before > 0);
    let err = q.refuse_adding_prune_with_tweaks(Height(0)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("taproot"), "{msg}");
    assert_eq!(q.store().txs.seqsigwit_loc.count(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The tweak index already covers the chain. Turning prune on drops
/// historical witnesses and still serves the sealed row from `sp_tweaks`
/// and `txout`. An index that does not cover the height about to be dropped
/// is refused before the stem cut.
#[test]
fn adding_prune_after_the_tweak_index_is_built_keeps_serving() {
    let keep = Query::SEQSIGWIT_KEEP_HEIGHTS;
    let (dir, q) = tmp_q();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    let mut headers = Vec::with_capacity((keep + 1) as usize);
    for h in 0..=keep {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, h == 0);
        prev = fk;
        prev_hash = Some(hash);
        headers.push(fk);
    }
    assert_eq!(q.tip_height(), Some(Height(keep)));
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let tweak = [0x02; 33];
    for (h, fk) in headers.iter().copied().enumerate() {
        let row = if h == 0 {
            vec![Some(tweak)]
        } else {
            vec![None]
        };
        q.put_sp_tweaks_block(Height(h as u32), fk, &row).unwrap();
    }
    assert_eq!(q.sptweaks_next_height(), Some(Height(keep + 1)));
    // A restart has not loaded the flag yet. The files are the index.
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    let before = q.store().txs.seqsigwit_loc.count();
    q.refuse_adding_prune_with_tweaks(Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    assert!(q.store().txs.seqsigwit_loc.count() < before);
    assert!(!q.store().path().join("seqsigwit.window/0.bin").exists());
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let rows = q.load_thin_tweaks(Height(0)).unwrap().expect("sealed row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tweak, tweak);
    assert!(!rows[0].p2tr.is_empty());
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, q) = tmp_q();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    for h in 0..=keep {
        let (fk, hash) = connect_one(&q, h, prev, prev_hash, false);
        prev = fk;
        prev_hash = Some(hash);
    }
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    let before = q.store().txs.seqsigwit_loc.count();
    let err = q.refuse_adding_prune_with_tweaks(Height(0)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does not cover"), "{msg}");
    assert!(msg.contains("sp_tweaks.idx"), "{msg}");
    assert!(
        msg.contains("run without --prune-seqsigwit"),
        "prune is not on yet, so the catch-up is the remedy: {msg}"
    );
    assert!(!msg.contains("taproot"), "{msg}");
    assert_eq!(q.store().txs.seqsigwit_loc.count(), before);
    let err = q.set_prune_seqsigwit(true).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does not cover"), "{msg}");
    assert_eq!(q.store().txs.seqsigwit_loc.count(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prune is already on, so this call is a later start. A tweak index that
/// no longer covers the persisted prune height must not make it fail, and
/// it must not cut the stem again. Turning tweaks back on still refuses.
#[test]
fn already_pruned_restart_ignores_a_tweak_hole() {
    let keep = Query::SEQSIGWIT_KEEP_HEIGHTS;
    let (dir, q) = tmp_q();
    let (fk, hash) = connect_one(&q, 0, Fk::NULL, None, false);
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), None);
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    // No rows. Next stays at the origin. The files remain after the flag drops.
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    let mut prev = fk;
    let mut prev_hash = Some(hash);
    for h in 1..=keep {
        let (next_fk, next_hash) = connect_one(&q, h, prev, prev_hash, false);
        prev = next_fk;
        prev_hash = Some(next_hash);
    }
    assert_eq!(q.tip_height(), Some(Height(keep)));
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    assert_eq!(q.sptweaks_next_height(), Some(Height(0)));
    let before = q.store().txs.seqsigwit_loc.count();
    q.set_prune_seqsigwit(true).unwrap();
    assert_eq!(q.store().txs.seqsigwit_loc.count(), before);
    assert_eq!(q.pruneheight(), Some(Height(0)));
    let err = q.set_sptweaks_enabled(true, Height(0)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does not cover"), "{msg}");
    assert!(msg.contains("delete store/sp_tweaks.idx"), "{msg}");
    assert!(
        !msg.contains("run without --prune-seqsigwit"),
        "prune is already on, so the unpruned catch-up cannot run: {msg}"
    );
    assert!(!msg.contains("taproot"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A caught-up index still has 288 blocks of slack. Omitting `--sp-tweaks`
/// on a pruned datadir reports that slack, and reports when the index is
/// already past the prune height.
#[test]
fn omitting_sp_tweaks_reports_blocks_left_before_the_index_is_stranded() {
    let caught_up = sp_tweaks_omit_warning(1001, Some(1000), Some(712));
    assert_eq!(caught_up, SpTweaksOmitWarning::BlocksLeft(288));
    assert!(
        caught_up.log_line().contains("288 more blocks"),
        "{}",
        caught_up.log_line()
    );
    assert_eq!(
        sp_tweaks_omit_warning(6, Some(10), Some(5)),
        SpTweaksOmitWarning::BlocksLeft(0)
    );
    assert_eq!(
        sp_tweaks_omit_warning(5, Some(300), Some(5)),
        SpTweaksOmitWarning::Stranded
    );
    assert_eq!(
        sp_tweaks_omit_warning(0, None, None),
        SpTweaksOmitWarning::BlocksLeft(288)
    );

    let (dir, q) = tmp_q();
    let _ = connect_one(&q, 0, Fk::NULL, None, false);
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), None);
    assert_eq!(
        q.sp_tweaks_prune_omit_warning(),
        Some(SpTweaksOmitWarning::BlocksLeft(287))
    );
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    assert_eq!(q.sp_tweaks_prune_omit_warning(), None);
    q.set_sptweaks_enabled(false, Height(0)).unwrap();
    q.set_pruneheight(Some(Height(0))).unwrap();
    assert_eq!(
        q.sp_tweaks_prune_omit_warning(),
        Some(SpTweaksOmitWarning::Stranded)
    );
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, q) = tmp_q();
    q.set_prune_seqsigwit(true).unwrap();
    assert_eq!(q.sp_tweaks_prune_omit_warning(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

fn header(h: u32, prev_fk: Fk, prev_hash: Option<[u8; 32]>) -> HeaderRecord {
    rbitcoin_query::testutil::header_with_merkle_marker(h, prev_fk, prev_hash, 0xec)
}

/// Two P2TR transactions share one `{height}.bin`. Repair reads that file
/// once, and a RAM hit does not touch it.
#[cfg(debug_assertions)]
#[test]
fn pruned_tweak_repair_reads_each_spill_once() {
    let (dir, q) = tmp_q();
    q.set_prune_seqsigwit(true).unwrap();
    let header = header(0, Fk::NULL, None);
    let p2tr = {
        let mut s = rbitcoin_primitives::P2TR_PREFIX.to_vec();
        s.extend_from_slice(&[0x11; 32]);
        s
    };
    let tx = |mark: u8| TxApply {
        tx: TxRecord {
            txid: [mark; 32],
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(u32::MAX, vec![mark], vec![])],
        outputs: vec![OutputRecord::unspent(50_0000_0000, p2tr.clone())],
    };
    q.connect_block(Height(0), &header, &[tx(0xcb), tx(0xcd)])
        .unwrap();
    q.clear_seqsigwit_ram_window();
    let heights = q.index_heights(0, 0, Some(0)).unwrap();
    assert_eq!(heights.len(), 1);
    rbitcoin_query::arm_spill_read_trace();
    let window = q.read_index_window(&heights).unwrap();
    assert_eq!(rbitcoin_query::take_spill_reads(), 1);
    assert!(window.blocks[0].txs.iter().all(|t| t.inputs.is_some()));
    let _ = std::fs::remove_dir_all(&dir);
}
