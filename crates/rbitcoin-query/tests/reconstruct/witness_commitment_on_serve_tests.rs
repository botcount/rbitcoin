use bitcoin::hashes::Hash;
use rbitcoin_consensus::{block_has_witness, witness_commitment_script};
use rbitcoin_primitives::Fk;
use rbitcoin_query::testutil::{
    header_with_merkle_marker, tiny_query_labeled, tx_apply_to_tx, FixtureChain,
};
use rbitcoin_query::{Query, TxApply};
use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

const RESERVED: [u8; 32] = [0u8; 32];

fn coinbase_only_block(witness: bool, commitment: bool) -> (HeaderRecord, Vec<TxApply>) {
    let mut outputs = vec![OutputRecord::unspent(50_0000_0000, vec![0x51])];
    if commitment {
        outputs.push(OutputRecord::unspent(
            0,
            witness_commitment_script(std::iter::empty::<[u8; 32]>(), &RESERVED),
        ));
    }
    let witness_stack = if witness {
        vec![RESERVED.to_vec()]
    } else {
        Vec::new()
    };
    let mut ta = TxApply {
        tx: TxRecord {
            txid: [0u8; 32],
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: outputs.len() as u32,
        },
        inputs: vec![InputRecord::coinbase(
            u32::MAX,
            vec![0x01, 0x02],
            witness_stack,
        )],
        outputs,
    };
    // A `seqsigwit` row records the wire txid, so the fixture needs the real one.
    ta.tx.txid = tx_apply_to_tx(&ta).compute_txid().to_byte_array();
    let header = header_with_merkle_marker(0, Fk::NULL, None, 0x96);
    (header, vec![ta])
}

fn flip_last_seqsigwit_byte(q: &Query, fk: Fk) {
    use std::io::{Read, Seek, SeekFrom, Write};

    let (off, len) = q.store().tx_seqsigwit_range(fk).expect("seqsigwit locator");
    let (_, inputs, _) = q.store().get_tx_full(fk).expect("decode record");
    assert!(
        inputs.last().is_some_and(|i| !i.witness.is_empty()),
        "fixture must end with a witness item"
    );
    let mut record = Vec::new();
    for inp in &inputs {
        inp.encode_into(&mut record);
    }
    let rec_len = record.len() as u64;
    assert!(
        rec_len <= len && len - rec_len < 8,
        "locator span {len} must be the record {rec_len} plus 8-byte padding"
    );

    // Append-only files are on the cold volume when `--datadir-cold` splits them.
    let store = q.store();
    let path = store
        .cold_path()
        .unwrap_or(store.path())
        .join("seqsigwit.body");
    let at = off + rec_len - 1;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open seqsigwit.body");
    f.seek(SeekFrom::Start(at)).expect("seek");
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte).expect("read byte");
    f.seek(SeekFrom::Start(at)).expect("seek back");
    f.write_all(&[byte[0] ^ 0x01]).expect("write byte");
    f.flush().expect("flush");
    // The RAM window would answer the next read from memory.
    q.clear_seqsigwit_ram_window();
}

#[test]
fn a_flipped_witness_byte_is_not_served() {
    let (dir, q) = tiny_query_labeled("reconstruct-witness-flip");
    let (header, txs) = coinbase_only_block(true, true);
    let header_fk = q.commit_class_a_only(&header, &txs).expect("archive");

    let intact = q
        .reconstruct_archived_block(&header.hash)
        .expect("reconstruct intact")
        .expect("archived body");
    assert!(
        block_has_witness(&intact),
        "fixture must carry witness for the check to apply"
    );

    let fks = q
        .header_tx_fks(header_fk, None)
        .expect("tx fks")
        .expect("body list");
    flip_last_seqsigwit_byte(&q, fks[0]);

    let err = q.reconstruct_archived_block(&header.hash).expect_err(
        "a block whose rebuilt witness no longer matches its coinbase \
         commitment must not be served",
    );
    assert!(
        format!("{err:?}").contains("commitment"),
        "rejected, but not for the witness commitment: {err:?}"
    );
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn an_intact_witness_block_still_serves() {
    let (dir, q) = tiny_query_labeled("reconstruct-witness-intact");
    let (header, txs) = coinbase_only_block(true, true);
    q.commit_class_a_only(&header, &txs).expect("archive");

    let served = q
        .reconstruct_archived_block(&header.hash)
        .expect("reconstruct")
        .expect("archived body");
    assert!(block_has_witness(&served));
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn a_block_without_witness_still_serves() {
    let (dir, q) = tiny_query_labeled("reconstruct-no-witness");
    let (header, txs) = coinbase_only_block(false, false);
    q.commit_class_a_only(&header, &txs).expect("archive");

    let served = q
        .reconstruct_archived_block(&header.hash)
        .expect("reconstruct")
        .expect("archived body");
    assert!(!block_has_witness(&served));
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn a_flipped_witness_byte_is_not_served_as_block_bytes() {
    let (dir, q) = tiny_query_labeled("reconstruct-witness-bytes");
    let (header, txs) = coinbase_only_block(true, true);
    let header_fk = q.commit_class_a_only(&header, &txs).expect("archive");
    assert!(q
        .witness_block_bytes_by_hash(&header.hash)
        .expect("encode intact")
        .is_some());

    let fks = q
        .header_tx_fks(header_fk, None)
        .expect("tx fks")
        .expect("body list");
    flip_last_seqsigwit_byte(&q, fks[0]);

    assert!(
        q.witness_block_bytes_by_hash(&header.hash)
            .expect("encode flipped")
            .is_none(),
        "encoded block bytes are not served when the witness no longer matches"
    );
    let _ = std::fs::remove_dir_all(dir.path());
}

/// Two transactions: a coinbase carrying the commitment, and a second one with
/// a witness whose null prevout keeps its wtxid independent of the commitment.
fn block_with_a_witness_tx() -> (HeaderRecord, Vec<TxApply>) {
    let mut second = TxApply {
        tx: TxRecord {
            txid: [0u8; 32],
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(
            u32::MAX,
            vec![0x02],
            vec![vec![0x11, 0x22]],
        )],
        outputs: vec![OutputRecord::unspent(49_0000_0000, vec![0x51])],
    };
    let second_tx = tx_apply_to_tx(&second);
    second.tx.txid = second_tx.compute_txid().to_byte_array();
    let second_wtxid = second_tx.compute_wtxid().to_byte_array();

    let mut coinbase = TxApply {
        tx: TxRecord {
            txid: [0u8; 32],
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: 2,
        },
        inputs: vec![InputRecord::coinbase(
            u32::MAX,
            vec![0x01, 0x02],
            vec![RESERVED.to_vec()],
        )],
        outputs: vec![
            OutputRecord::unspent(50_0000_0000, vec![0x51]),
            OutputRecord::unspent(0, witness_commitment_script([second_wtxid], &RESERVED)),
        ],
    };
    coinbase.tx.txid = tx_apply_to_tx(&coinbase).compute_txid().to_byte_array();
    let header = header_with_merkle_marker(0, Fk::NULL, None, 0x98);
    (header, vec![coinbase, second])
}

#[test]
fn a_flipped_non_coinbase_witness_byte_is_not_served() {
    let (dir, q) = tiny_query_labeled("reconstruct-witness-tx");
    let (header, txs) = block_with_a_witness_tx();
    let header_fk = q.commit_class_a_only(&header, &txs).expect("archive");

    let fks = q
        .header_tx_fks(header_fk, None)
        .expect("tx fks")
        .expect("body list");
    assert_eq!(fks.len(), 2);
    assert!(q
        .witness_block_bytes_by_hash(&header.hash)
        .expect("encode intact")
        .is_some());

    flip_last_seqsigwit_byte(&q, fks[1]);

    assert!(
        q.witness_block_bytes_by_hash(&header.hash)
            .expect("encode flipped")
            .is_none(),
        "a flipped witness byte in a served transaction is not written out"
    );
    let err = q
        .reconstruct_archived_block(&header.hash)
        .expect_err("the rebuilt block is refused");
    assert!(format!("{err:?}").contains("commitment"), "{err:?}");
    let _ = std::fs::remove_dir_all(dir.path());
}

#[test]
fn a_commitment_without_a_reserved_value_still_serves() {
    let (dir, q) = tiny_query_labeled("reconstruct-commitment-no-reserved");
    let (header, txs) = coinbase_only_block(false, true);
    q.commit_class_a_only(&header, &txs).expect("archive");

    let served = q
        .reconstruct_archived_block(&header.hash)
        .expect("reconstruct")
        .expect("archived body");
    assert!(!block_has_witness(&served));
    let _ = std::fs::remove_dir_all(dir.path());
}
