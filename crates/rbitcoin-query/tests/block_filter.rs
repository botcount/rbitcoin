use bitcoin::absolute::LockTime;
use bitcoin::block::{Header, Version as BlockVersion};
use bitcoin::hashes::Hash;
use bitcoin::{Block, OutPoint};

use bitcoin::transaction::Version;
use bitcoin::{Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

#[test]
fn wire_filter_missing_prevout_is_corrupt() {
    let (_dir, q) = rbitcoin_query::testutil::tiny_query_labeled("filter-missing-prevout");
    let spend = Transaction {
        version: Version::ONE,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([0x7a; 32]),
                vout: 1,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    let block = Block {
        header: Header {
            version: BlockVersion::ONE,
            prev_blockhash: bitcoin::BlockHash::from_byte_array([0; 32]),
            merkle_root: bitcoin::TxMerkleNode::from_byte_array([0; 32]),
            time: 2,
            bits: bitcoin::CompactTarget::from_consensus(0x207fffff),
            nonce: 0,
        },
        txdata: vec![spend],
    };
    let err = q.basic_filter_from_wire_block(&block).unwrap_err();
    assert!(
        err.to_string().contains("blockfilter prevout missing"),
        "{err}"
    );
}

/// GCS keys are the block hash. Membership uses rust-bitcoin's own key schedule.
#[test]
fn basic_filter_keys_match_the_block_hash() {
    let hash: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_add(1));
    let a: &[u8] = &[0x51];
    let b: &[u8] = &[0x52, 0x21];
    let filter = rbitcoin_query::basic_filter_from_scripts(&hash, [a, b], std::iter::empty())
        .expect("encode");
    let block_hash = bitcoin::BlockHash::from_byte_array(hash);
    assert!(filter
        .match_any(&block_hash, [a, b].into_iter())
        .expect("match"));
    let miss: &[u8] = &[0xff, 0x00];
    assert!(!filter
        .match_any(&block_hash, [miss].into_iter())
        .expect("miss"));
    let again = rbitcoin_query::basic_filter_from_scripts(&hash, [a, b], std::iter::empty())
        .expect("encode again");
    assert_eq!(filter.content, again.content);
}
