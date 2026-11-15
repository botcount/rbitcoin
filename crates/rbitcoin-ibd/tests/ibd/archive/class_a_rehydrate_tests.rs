use bitcoin::absolute::LockTime;
use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use rbitcoin_ibd::ibd::archive::{
    rehydrate_block_queue_into_confirm, rehydrate_class_a_into_body_queue,
};
use rbitcoin_ibd::ibd::confirm::ConfirmFeed;
use rbitcoin_ibd::ibd::progress::claim_ready;
use rbitcoin_ibd::ibd::state::IbdWorkState;
use rbitcoin_primitives::Fk;

use bitcoin::script::ScriptBuf;
use bitcoin::transaction::Version as TxVersion;
use bitcoin::{
    Amount, Block, CompactTarget, OutPoint, Sequence, Target, Transaction, TxIn, TxOut, Witness,
};

use rbitcoin_query::testutil::FixtureChain;

fn mine(prev: BlockHash, time: u32, height: u32) -> Block {
    let mut ss = if height == 0 {
        vec![0x00]
    } else {
        rbitcoin_consensus::bip34_height_script(height)
    };
    while ss.len() < 2 {
        ss.push(0x00);
    }
    let coinbase = Transaction {
        version: TxVersion::ONE,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(ss),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    let bits = CompactTarget::from_consensus(0x207f_ffff);
    let header = Header {
        version: Version::from_consensus(4),
        prev_blockhash: prev,
        merkle_root: bitcoin::TxMerkleNode::from_byte_array([0u8; 32]),
        time,
        bits,
        nonce: 0,
    };
    let mut block = Block {
        header,
        txdata: vec![coinbase],
    };
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    let target = Target::from_compact(bits);
    for nonce in 0..u32::MAX {
        block.header.nonce = nonce;
        if block.header.validate_pow(target).is_ok() {
            break;
        }
    }
    block
}

#[test]
fn class_a_rehydrate_max_zero_is_zero() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("ca-max0");
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let feed = ConfirmFeed::new();
    assert_eq!(
        rehydrate_class_a_into_body_queue(&hub, &mut st, &feed, 0).unwrap(),
        0
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn class_a_rehydrate_makes_tip_plus_one_claim_ready() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("ca-rehydrate");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let time = 1_300_000_000u32;
    let mut tip = gen;
    let mut hashes = Vec::new();
    // Tip stays at genesis; Class A via commit_class_a_only (not public archive_block).
    for h in 1u32..=3 {
        let b = mine(tip, time + h * 600, h);
        hub.ensure_header(&b.header).unwrap();
        let (rec, txs) =
            rbitcoin_query::prepare_block_for_archive(&hub.query, &hub.params, &b).unwrap();
        hub.query.commit_class_a_only(&rec, &txs).unwrap();
        hashes.push(b.block_hash());
        tip = b.block_hash();
    }

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    rbitcoin_ibd::ibd::path::seed_work_path_from_store(&mut st, &hub);
    assert!(
        st.body.is_known_archived(&hashes[0]),
        "seed must mark Class A known"
    );
    assert!(
        !hub.query.block_queue_has_height(1),
        "BQ empty before rehydrate"
    );
    assert!(
        !claim_ready(&hub, &mut st.body, 1, &hashes[0]),
        "Class A alone is not claim-ready"
    );

    let feed = ConfirmFeed::new();
    let n = rehydrate_class_a_into_body_queue(&hub, &mut st, &feed, 32).unwrap();
    assert_eq!(n, 3, "contiguous Class A prefix should all rehydrate");
    for (i, h) in hashes.iter().enumerate() {
        let ht = (i as u32) + 1;
        assert!(
            hub.query.block_queue_has_height(ht),
            "BQ must hold height {ht}"
        );
        assert!(
            st.body.is_pending(h),
            "pending after Class A rehydrate h={ht}"
        );
        assert!(
            claim_ready(&hub, &mut st.body, ht, h),
            "claim_ready after Class A rehydrate h={ht}"
        );
    }
    assert_eq!(feed.size_snap().0, 3);

    // Idempotent: second call counts already-queued as ready, no error.
    let n2 = rehydrate_class_a_into_body_queue(&hub, &mut st, &feed, 32).unwrap();
    assert_eq!(n2, 3);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bq_rehydrate_residue_keep_drop_gap_and_unknown() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("bq-rehydrate");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let time = 1_300_000_000u32;
    let mut prev = gen;
    let mut hashes = Vec::new();
    let mut headers = Vec::new();
    let mut fks = Vec::new();
    for h in 1u32..=4 {
        let b = mine(prev, time + h * 600, h);
        fks.push(hub.ensure_header_fk(&b.header).unwrap());
        hashes.push(b.block_hash());
        headers.push(b.header);
        prev = b.block_hash();
    }

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let feed = ConfirmFeed::new();
    assert_eq!(
        rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap(),
        0,
        "empty queue"
    );

    hub.query
        .block_queue_enqueue(0, gen.to_byte_array(), 0, b"stale")
        .unwrap();
    hub.query
        .block_queue_enqueue(1, hashes[0].to_byte_array(), 0, b"")
        .unwrap();
    st.body.mark_archived(hashes[1]);
    hub.query
        .block_queue_enqueue(2, hashes[1].to_byte_array(), fks[1].0, b"wire2")
        .unwrap();
    hub.note_confirmed_tip(&[(3, hashes[2])], &[headers[2]])
        .unwrap();
    assert!(
        hub.has_block(&hashes[2]),
        "stale confirmed-set must not dequeue above-tip wire"
    );
    assert_eq!(hub.tip_height(), Some(0));
    hub.query
        .block_queue_enqueue(3, hashes[2].to_byte_array(), fks[2].0, b"wire3")
        .unwrap();
    let unk = BlockHash::from_byte_array([0x11; 32]);
    hub.query
        .block_queue_enqueue(u32::MAX, unk.to_byte_array(), 0, b"unk")
        .unwrap();
    for (i, h) in hashes.iter().enumerate() {
        st.record_height(*h, (i as u32) + 1);
    }

    let n = rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap();
    assert_eq!(n, 2, "ready heights are 2 and 3");
    assert!(
        !hub.query.block_queue_has_height(0),
        "drop residue at confirmed tip"
    );
    assert!(
        !hub.query.block_queue_has_height(1),
        "empty payload dequeue"
    );
    assert!(hub.query.block_queue_has_height(2), "keep known-archived");
    assert!(
        hub.query.block_queue_has_height(3),
        "keep has_block above tip"
    );
    assert!(
        hub.query.block_queue_has_height(u32::MAX),
        "unknown height stays queued"
    );
    assert!(st.body.is_pending(&hashes[1]));
    assert!(st.body.is_pending(&hashes[2]));
    assert!(
        claim_ready(&hub, &mut st.body, 2, &hashes[1]),
        "kept BQ is claim-ready"
    );
    assert!(st.body.is_missing(&unk), "unknown height marked missing");
    assert!(
        st.body.is_missing(&hashes[0]),
        "tip+1 gap marked missing for densify"
    );
    assert_eq!(st.header_fks.get(&hashes[1]).copied(), Some(fks[1]));
    assert_eq!(feed.size_snap().0, 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bq_rehydrate_no_tip_keeps_height_zero() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("bq-rehydrate-notip");
    assert!(hub.tip_height().is_none());
    let h = BlockHash::from_byte_array([0x22; 32]);
    hub.query
        .block_queue_enqueue(0, h.to_byte_array(), 1, b"gen")
        .unwrap();
    let mut st = IbdWorkState::new(Vec::new(), None, None);
    let feed = ConfirmFeed::new();
    let n = rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap();
    assert_eq!(n, 0, "a height with no stored parent is not offered");
    assert!(hub.query.block_queue_has_height(0));
    assert!(st.body.is_pending(&h));
    assert_eq!(st.header_fks.get(&h).copied(), Some(Fk(1)));
    assert_eq!(feed.size_snap().0, 0);

    hub.query.block_queue_dequeue_height(0).unwrap();
    let merkle = [0x44u8; 32];
    let prev = [0u8; 32];
    let hash = rbitcoin_store::block_header_hash(1, &prev, &merkle, 1_300_000_000, 0x207f_ffff, 0);
    let fk = hub
        .query
        .put_header(&rbitcoin_store::HeaderRecord {
            prev_fk: Fk::NULL,
            version: 1,
            timestamp: 1_300_000_000,
            bits: 0x207f_ffff,
            nonce: 0,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        })
        .unwrap();
    hub.query
        .block_queue_enqueue(0, hash, fk.0, b"gen-body")
        .unwrap();
    let mut st = IbdWorkState::new(Vec::new(), None, None);
    let n = rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap();
    assert_eq!(n, 1, "a pre-tip genesis body with a null parent is offered");
    assert_eq!(feed.size_snap().0, 1);
    assert_eq!(
        st.height_to_hash.get(&0).map(|h| h.to_byte_array()),
        Some(hash)
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bq_rehydrate_does_not_offer_a_competing_leftover() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("bq-rehydrate-fork");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let tip = mine(gen, 1_300_010_000, 1);
    hub.accept_block(tip.clone()).unwrap();
    let linear = mine(tip.block_hash(), 1_300_010_600, 2);
    let sib = mine(gen, 1_300_010_100, 1);
    hub.ensure_header_fk(&sib.header).unwrap();
    let fork_child = mine(sib.block_hash(), 1_300_010_700, 2);
    let child_fk = hub.ensure_header_fk(&fork_child.header).unwrap();
    hub.query
        .block_queue_enqueue(
            2,
            fork_child.block_hash().to_byte_array(),
            child_fk.0,
            b"fork",
        )
        .unwrap();

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    st.record_height(linear.block_hash(), 2);
    st.note_path_prev(linear.block_hash(), tip.block_hash());
    let feed = ConfirmFeed::new();
    let n = rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap();
    assert_eq!(
        n, 0,
        "a leftover that does not extend the tip is not offered"
    );
    assert_eq!(feed.size_snap().0, 0);
    assert_eq!(
        st.height_to_hash.get(&2),
        Some(&linear.block_hash()),
        "the competing leftover must not take the tip+1 slot"
    );
    assert_eq!(
        st.path_prev.get(&fork_child.block_hash()).copied(),
        Some(sib.block_hash())
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A gap height that is only pending is not marked missing.
#[test]
fn bq_gap_pending_only_stays_pending() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("bq-gap-pending");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let time = 1_300_000_000u32;
    let mut prev = gen;
    let mut hashes = Vec::new();
    let mut fks = Vec::new();
    for h in 1u32..=3 {
        let b = mine(prev, time + h * 600, h);
        fks.push(hub.ensure_header_fk(&b.header).unwrap());
        hashes.push(b.block_hash());
        prev = b.block_hash();
    }

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    for (i, hash) in hashes.iter().enumerate() {
        st.record_height(*hash, (i as u32) + 1);
    }
    st.body.mark_pending(hashes[1]);
    hub.query
        .block_queue_enqueue(3, hashes[2].to_byte_array(), fks[2].0, b"wire3")
        .unwrap();
    let feed = ConfirmFeed::new();
    let n = rehydrate_block_queue_into_confirm(&hub, &mut st, &feed).unwrap();
    assert_eq!(n, 1, "height 3 is the only ready body");
    assert!(
        st.body.is_pending(&hashes[1]) && !st.body.is_missing(&hashes[1]),
        "pending-only gap height stays pending"
    );
    assert!(
        st.body.is_missing(&hashes[0]),
        "an empty gap height is marked missing"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Store Class A at tip+1 is offered when the known cache does not have it.
#[test]
fn class_a_rehydrate_offers_store_without_known_cache() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("ca-store-only");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let b = mine(gen, 1_300_000_600, 1);
    let hash = b.block_hash();
    let (rec, txs) =
        rbitcoin_query::prepare_block_for_archive(&hub.query, &hub.params, &b).unwrap();
    hub.query.commit_class_a_only(&rec, &txs).unwrap();
    let fk = hub.ensure_header_fk(&b.header).unwrap();
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    st.record_height(hash, 1);
    st.header_fks.insert(hash, fk);
    assert!(!st.body.is_known_archived(&hash));
    assert!(hub.query.is_block_archived(&hash.to_byte_array()).unwrap());
    let feed = ConfirmFeed::new();
    let n = rehydrate_class_a_into_body_queue(&hub, &mut st, &feed, 32).unwrap();
    assert_eq!(n, 1, "store Class A at tip+1 is offered");
    assert!(hub.query.block_queue_has_height(1));
    let _ = std::fs::remove_dir_all(dir);
}

/// A reconstructed header whose nonce no longer matches the indexed hash
/// is not offered. Merkle still matches the stored transactions.
#[test]
fn class_a_rehydrate_rejects_a_hash_mismatch() {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("ca-hash-mismatch");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let b = mine(gen, 1_300_000_600, 1);
    let hash = b.block_hash();
    let (rec, txs) =
        rbitcoin_query::prepare_block_for_archive(&hub.query, &hub.params, &b).unwrap();
    hub.query.commit_class_a_only(&rec, &txs).unwrap();
    let fk = hub.ensure_header_fk(&b.header).unwrap();
    let mut stored = hub.query.store.headers.get(fk).unwrap();
    stored.nonce = stored.nonce.wrapping_add(1);
    let offset = rbitcoin_store::file::FILE_HEADER_LEN as u64
        + (fk.0 - 1) * rbitcoin_store::header_table::HEADER_RECORD_LEN as u64;
    hub.query
        .store
        .headers
        .body
        .write_at(offset, &stored.encode())
        .unwrap();

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    st.record_height(hash, 1);
    st.header_fks.insert(hash, fk);
    let feed = ConfirmFeed::new();
    let n = rehydrate_class_a_into_body_queue(&hub, &mut st, &feed, 32).unwrap();
    assert_eq!(
        n, 0,
        "a header that does not hash to the index is not offered"
    );
    assert!(
        !hub.query.block_queue_has_height(1),
        "the mismatched body stays off the queue"
    );
    assert!(
        st.body.is_missing(&hash),
        "the height is marked missing for re-getdata"
    );
    assert!(
        !hub.query.is_block_archived(&hash.to_byte_array()).unwrap(),
        "Class A is cleared after the header check fails"
    );
    let _ = std::fs::remove_dir_all(dir);
}
