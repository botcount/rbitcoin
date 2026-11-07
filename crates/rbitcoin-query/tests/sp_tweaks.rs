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
    assert!(
        q.set_prune_seqsigwit(true).is_err(),
        "prune under tweaks stays refused"
    );
    q.set_sptweaks_enabled(false, Height(4)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    q.set_sptweaks_enabled(true, Height(4)).unwrap();

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

fn header(h: u32, prev_fk: Fk, prev_hash: Option<[u8; 32]>) -> HeaderRecord {
    rbitcoin_query::testutil::header_with_merkle_marker(h, prev_fk, prev_hash, 0xec)
}
