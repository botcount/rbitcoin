use bitcoin::absolute::LockTime;
use bitcoin::block::{Block, Header, Version};
use bitcoin::hashes::Hash;
use bitcoin::{
    Amount, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Txid, Witness,
};
use rbitcoin_consensus::block::{validate_block_structure, JobPrevouts};
use rbitcoin_consensus::{ChainParams, ConsensusError, ValidationContext, REGTEST_POW_BITS};

fn ctx() -> ValidationContext<'static> {
    let params = Box::leak(Box::new(ChainParams::regtest()));
    ValidationContext::archive_structure(params)
}

fn block_with(txs: Vec<Transaction>) -> Block {
    let mut block = Block {
        header: Header {
            version: Version::from_consensus(4),
            prev_blockhash: BlockHash::from_byte_array([0; 32]),
            merkle_root: TxMerkleNode::from_byte_array([0; 32]),
            time: 1_290_000_000,
            bits: CompactTarget::from_consensus(REGTEST_POW_BITS),
            nonce: 0,
        },
        txdata: txs,
    };
    block.header.merkle_root = block.compute_merkle_root().expect("merkle");
    block
}

fn coinbase() -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![0x01, 0x00]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

fn spend(txid_byte: u8, vout: u32) -> TxIn {
    TxIn {
        previous_output: OutPoint {
            txid: Txid::from_byte_array([txid_byte; 32]),
            vout,
        },
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::new(),
    }
}

fn tx_with(input: Vec<TxIn>) -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: LockTime::ZERO,
        input,
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

fn assert_dup(err: ConsensusError) {
    match err {
        ConsensusError::BadTx(s) if s.contains("bad-txns-inputs-duplicate") => {}
        other => panic!("expected bad-txns-inputs-duplicate, got {other:?}"),
    }
}

#[test]
fn duplicate_input_is_one_set_for_the_block() {
    validate_block_structure(
        &block_with(vec![coinbase(), tx_with(vec![spend(1, 0)])]),
        &ctx(),
    )
    .expect("one input");

    let dup = tx_with(vec![spend(2, 0), spend(2, 0)]);
    assert_dup(validate_block_structure(&block_with(vec![coinbase(), dup]), &ctx()).unwrap_err());

    let dup3 = tx_with(vec![spend(3, 0), spend(4, 0), spend(3, 0)]);
    assert_dup(validate_block_structure(&block_with(vec![coinbase(), dup3]), &ctx()).unwrap_err());

    let mut cb = coinbase();
    cb.input.push(cb.input[0].clone());
    assert_dup(validate_block_structure(&block_with(vec![cb]), &ctx()).unwrap_err());

    let mut input = Vec::with_capacity(64);
    for i in 0..64u32 {
        let mut tid = [0u8; 32];
        tid[..4].copy_from_slice(&i.to_le_bytes());
        input.push(TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array(tid),
                vout: i,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        });
    }
    validate_block_structure(&block_with(vec![coinbase(), tx_with(input)]), &ctx())
        .expect("64 unique inputs");
}

#[test]
fn job_prevouts_empty_follows_len() {
    let empty = JobPrevouts::owned(Vec::new());
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    let one = JobPrevouts::owned(vec![TxOut {
        value: Amount::from_sat(1),
        script_pubkey: ScriptBuf::new(),
    }]);
    assert!(!one.is_empty());
    assert_eq!(one.len(), 1);
}

/// CheckTransaction's output walk. `i64::MAX` still fits the wire `u64` and is
/// too-large money, not a negative amount. One sat past it is negative.
#[test]
fn output_at_i64_max_is_too_large_not_negative() {
    let mut tx = coinbase();
    tx.output[0].value = Amount::from_sat(i64::MAX as u64);
    let err = rbitcoin_consensus::block::tx_value_out(&tx).unwrap_err();
    assert!(
        matches!(err, ConsensusError::BadTx(s) if s.contains("vout-toolarge")),
        "{err:?}"
    );
    tx.output[0].value = Amount::from_sat((i64::MAX as u64).saturating_add(1));
    let err = rbitcoin_consensus::block::tx_value_out(&tx).unwrap_err();
    assert!(
        matches!(err, ConsensusError::BadTx(s) if s.contains("vout-negative")),
        "{err:?}"
    );
}
