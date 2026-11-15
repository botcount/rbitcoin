use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use rbitcoin_mempool::error::MempoolError;
use rbitcoin_mempool::packed::*;
use rbitcoin_primitives::{write_compact_size, Fk};

/// First vin's flag byte: the fixed prefix (ids, version and locktime,
/// 104 B) plus the one-byte input and output counts.
const VIN0_FLAGS: usize = 104 + 1 + 1;

fn sample_tx(annex: bool) -> Transaction {
    let mut witness = Witness::new();
    witness.push([0x01, 0x02]);
    if annex {
        witness.push([0x50, 0x00, 0xaa]);
    }
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: bitcoin::OutPoint {
                txid: Txid::from_byte_array([0xab; 32]),
                vout: 1,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness,
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

#[test]
fn packed_roundtrip_serialize_equals_admitted_raw() {
    let tx = sample_tx(true);
    let raw = serialize(&tx);
    let txid = tx.compute_txid();
    let wtxid = tx.compute_wtxid();
    let sh = [0x11u8; 32];
    let aux = [VinAux {
        prev_txid: tx.input[0].previous_output.txid,
        vout: 1,
        script_hash: Some(sh),
        create_fk: Some(Fk(42)),
    }];
    let packed =
        encode_packed_live(&tx, &txid, &wtxid, 123, 400, Some(80), 1_700_000_123, &aux).unwrap();
    let got = decode_packed_live(&packed).unwrap();
    assert_eq!(got.fee_sat, 123);
    assert_eq!(got.weight, 400);
    assert_eq!(got.sigop_cost, Some(80));
    assert_eq!(got.accept_secs, 1_700_000_123);
    assert_eq!(got.txid, txid);
    assert_eq!(got.wtxid, wtxid);
    assert_eq!(serialize(&got.tx), raw, "consensus serialize pin");
    assert_eq!(got.vins[0].script_hash, Some(sh));
    assert_eq!(got.vins[0].create_fk, Some(Fk(42)));
    // SEQ_FINAL | EMPTY_SCRIPT | CREATE_FK | SCRIPT_HASH. Witness is present.
    assert_eq!(
        packed[VIN0_FLAGS], 0b0001_1011,
        "vin flags {:08b}",
        packed[VIN0_FLAGS]
    );
}

/// Vin flag bits and a script or witness that ends the record. Encode and
/// decode share the constants, so a reopen of a tx this process wrote
/// still round-trips if a bit number changes. The byte is the schema an
/// older mempool file uses.
#[test]
fn packed_vin_flags_and_exact_tail_roundtrip() {
    let mut bare = sample_tx(false);
    bare.input[0].witness = Witness::new();
    let txid = bare.compute_txid();
    let wtxid = bare.compute_wtxid();
    let packed =
        encode_packed_live(&bare, &txid, &wtxid, 1, 400, Some(1), 1_700_000_000, &[]).unwrap();
    // SEQ_FINAL | EMPTY_SCRIPT | EMPTY_WITNESS, no aux. The witness/script
    // tails are absent, so this pins the three flag bits an operator reload reads.
    assert_eq!(
        packed[VIN0_FLAGS], 0b0000_0111,
        "vin flags {:08b}",
        packed[VIN0_FLAGS]
    );
    let got = decode_packed_live(&packed).unwrap();
    assert_eq!(serialize(&got.tx), serialize(&bare));

    bare.input[0].script_sig = ScriptBuf::from_bytes(vec![0x51]);
    bare.input[0].sequence = Sequence::ZERO;
    let txid = bare.compute_txid();
    let wtxid = bare.compute_wtxid();
    let packed =
        encode_packed_live(&bare, &txid, &wtxid, 1, 400, Some(1), 1_700_000_000, &[]).unwrap();
    // Non-empty script, empty witness, non-final sequence: only EMPTY_WITNESS.
    assert_eq!(
        packed[VIN0_FLAGS], 0b0000_0100,
        "vin flags {:08b}",
        packed[VIN0_FLAGS]
    );
    let got = decode_packed_live(&packed).unwrap();
    assert_eq!(got.tx.input[0].script_sig.as_bytes(), &[0x51]);
    assert_eq!(serialize(&got.tx), serialize(&bare));

    // No outputs: the script or the witness is the last byte of the vin
    // slice, so `end == buf.len()` is a real record and one byte short is not.
    let script_tail = encode_tail(true, false);
    assert!(decode_packed_live(&script_tail).is_ok());
    let mut torn = script_tail.clone();
    torn.pop();
    assert!(matches!(
        decode_packed_live(&torn),
        Err(MempoolError::Corrupt("packed script"))
    ));
    let witness_tail = encode_tail(false, true);
    assert!(decode_packed_live(&witness_tail).is_ok());
    let mut torn = witness_tail;
    torn.pop();
    assert!(matches!(
        decode_packed_live(&torn),
        Err(MempoolError::Corrupt("packed witness"))
    ));
}

/// One input, no outputs. `script` puts a one-byte scriptSig last; otherwise
/// a one-byte witness item is last.
fn encode_tail(script: bool, witness: bool) -> Vec<u8> {
    let mut tx = sample_tx(false);
    tx.output.clear();
    tx.input[0].witness = Witness::new();
    tx.input[0].script_sig = ScriptBuf::new();
    tx.input[0].sequence = Sequence::ZERO;
    if script {
        tx.input[0].script_sig = ScriptBuf::from_bytes(vec![0x51]);
    }
    if witness {
        let mut w = Witness::new();
        w.push([0x51]);
        tx.input[0].witness = w;
    }
    let (txid, wtxid) = (tx.compute_txid(), tx.compute_wtxid());
    encode_packed_live(&tx, &txid, &wtxid, 1, 400, Some(1), 1_700_000_000, &[]).unwrap()
}

/// Input and output counts past 100_000. A live mempool tx cannot be that
/// large. Opening a torn `tx.body` hits this decoder and must not allocate.
#[test]
fn packed_live_rejects_only_past_the_count_cap() {
    let tx = sample_tx(false);
    let (txid, wtxid) = (tx.compute_txid(), tx.compute_wtxid());
    let seed = encode_packed_live(&tx, &txid, &wtxid, 1, 400, Some(1), 1_700_000_000, &[]).unwrap();
    let with_counts = |n_in: u64, n_out: u64| {
        let mut buf = seed[..104].to_vec();
        write_compact_size(&mut buf, n_in);
        write_compact_size(&mut buf, n_out);
        buf
    };
    let counts = |buf: &[u8]| {
        matches!(
            decode_packed_live(buf),
            Err(MempoolError::Corrupt("packed live counts"))
        )
    };
    assert!(counts(&with_counts(100_001, 0)));
    assert!(counts(&with_counts(0, 100_001)));
    assert!(!counts(&with_counts(100_000, 0)));
    assert!(!counts(&with_counts(0, 100_000)));
}

#[test]
fn unknown_sigops_and_older_schema_records_decode() {
    let tx = sample_tx(false);
    let (txid, wtxid) = (tx.compute_txid(), tx.compute_wtxid());
    let v4 = encode_packed_live(&tx, &txid, &wtxid, 7, 400, None, 1_700_000_000, &[]).unwrap();
    assert_eq!(decode_packed_live(&v4).unwrap().sigop_cost, None);
    // A torn 8-byte write-back over 0xFF.. overestimates; any cost no block
    // can hold reads as unknown so the open-time pass recomputes it.
    for (stored, want) in [(80_000, Some(80_000)), (80_001, None)] {
        let rec = encode_packed_live(&tx, &txid, &wtxid, 7, 400, Some(stored), 1_700_000_000, &[])
            .unwrap();
        assert_eq!(decode_packed_live(&rec).unwrap().sigop_cost, want);
    }
    let mut v3 = v4.clone();
    v3.drain(ACCEPT_SECS_OFF..ACCEPT_SECS_OFF + 8);
    let got = decode_packed_live_v3(&v3).unwrap();
    assert_eq!((got.fee_sat, got.weight, got.sigop_cost), (7, 400, None));
    assert_eq!(got.accept_secs, 0, "schema 3 has no accept time");
    assert_eq!(got.txid, txid);
    assert_eq!(serialize(&got.tx), serialize(&tx));
    let mut v2 = v3.clone();
    v2.drain(SIGOP_COST_OFF..SIGOP_COST_OFF + 8);
    let got = decode_packed_live_v2(&v2).unwrap();
    assert_eq!((got.fee_sat, got.weight, got.sigop_cost), (7, 400, None));
    assert_eq!(got.txid, txid);
    assert_eq!(serialize(&got.tx), serialize(&tx));
    // Fixed prefix is 104 B (schema 4) / 96 B (schema 3) / 88 B (schema 2):
    // one byte short fails the length guard; exactly the prefix reaches the
    // input count.
    for (buf, min, decode) in [
        (&v4, 104, decode_packed_live as fn(&[u8]) -> _),
        (&v3, 96, decode_packed_live_v3),
        (&v2, 88, decode_packed_live_v2),
    ] {
        assert!(matches!(
            decode(&buf[..min - 1]),
            Err(MempoolError::Corrupt("packed live short"))
        ));
        assert!(matches!(
            decode(&buf[..min]),
            Err(MempoolError::Corrupt("compact size empty"))
        ));
    }
}

#[test]
fn decode_packed_prefix_keeps_fee_and_rejects_short() {
    let tx = sample_tx(false);
    let (txid, wtxid) = (tx.compute_txid(), tx.compute_wtxid());
    let packed =
        encode_packed_live(&tx, &txid, &wtxid, 99, 401, Some(3), 1_700_000_111, &[]).unwrap();
    let got = decode_packed(&packed, true, true).unwrap();
    assert_eq!(got.fee_sat, 99);
    assert_eq!(got.weight, 401);
    assert_eq!(got.sigop_cost, Some(3));
    assert_eq!(got.accept_secs, 1_700_000_111);
    assert_eq!(got.txid, txid);
    assert_eq!(got.wtxid, wtxid);
    assert_eq!(serialize(&got.tx), serialize(&tx));
    let mut v3 = packed.clone();
    v3.drain(ACCEPT_SECS_OFF..ACCEPT_SECS_OFF + 8);
    let got = decode_packed(&v3, true, false).unwrap();
    assert_eq!(got.fee_sat, 99);
    assert_eq!(got.weight, 401);
    assert_eq!(got.sigop_cost, Some(3));
    assert_eq!(got.accept_secs, 0);
    assert_eq!(got.txid, txid);
    let mut v2 = v3;
    v2.drain(SIGOP_COST_OFF..SIGOP_COST_OFF + 8);
    let got = decode_packed(&v2, false, false).unwrap();
    assert_eq!(got.fee_sat, 99);
    assert_eq!(got.weight, 401);
    assert_eq!(got.sigop_cost, None);
    assert_eq!(got.txid, txid);
    assert!(matches!(
        decode_packed(&packed[..16], true, true),
        Err(MempoolError::Corrupt("packed live short"))
    ));
}

#[test]
fn take_arr_keeps_four_and_rejects_short() {
    let mut off = 0usize;
    let got = take_arr::<4>(&[9, 8, 7, 6, 5], &mut off).unwrap();
    assert_eq!(got, [9, 8, 7, 6]);
    assert_eq!(off, 4);
    match take_arr::<4>(&[1, 2], &mut 0) {
        Err(MempoolError::Corrupt("packed short")) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("a short window was taken"),
    }
}

#[test]
fn core_dat_empty_roundtrip_and_rejects_short() {
    let raw = rbitcoin_mempool::CoreMempoolDat::default().encode(None);
    let got = rbitcoin_mempool::CoreMempoolDat::decode(&raw).unwrap();
    assert!(got.txs.is_empty());
    let keyed = rbitcoin_mempool::CoreMempoolDat::default().encode(Some([9u8; 8]));
    let got = rbitcoin_mempool::CoreMempoolDat::decode(&keyed).unwrap();
    assert!(got.txs.is_empty());
    match rbitcoin_mempool::CoreMempoolDat::decode(&[1, 0, 0]) {
        Err(err) => assert!(err.contains("end of data"), "{err}"),
        Ok(_) => panic!("a short file decoded"),
    }
}
