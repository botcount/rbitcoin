//! The legacy sighash budget of an unconfirmed check. A standard P2SH
//! multisig spend of any input count fits, a hash repeated for one input is
//! paid once, and a transaction that asks for many different hashes of a large
//! body is still refused. The consensus check never applies the budget.
use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use rbitcoin_consensus::script::interpreter::{
    unconfirmed_legacy_sighash_budget, MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES,
};
use rbitcoin_consensus::{verify_tx_scripts_detached, verify_tx_scripts_unconfirmed};

use bitcoin::hashes::Hash;

/// A signature that parses, so CHECKSIG reaches the sighash. `ht` is the hash type.
fn dummy_sig(ht: u8) -> [u8; 9] {
    [0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, ht]
}

/// Compressed secp256k1 generator.
const PUBKEY: [u8; 33] = [
    0x02, 0x79, 0xBE, 0x66, 0x7E, 0xF9, 0xDC, 0xBB, 0xAC, 0x55, 0xA0, 0x62, 0x95, 0xCE, 0x87, 0x0B,
    0x07, 0x02, 0x9B, 0xFC, 0xDB, 0x2D, 0xCE, 0x28, 0xD9, 0x59, 0xF2, 0x81, 0x5B, 0x16, 0xF8, 0x17,
    0x98,
];

fn push(out: &mut Vec<u8>, data: &[u8]) {
    match data.len() {
        0..=75 => out.push(u8::try_from(data.len()).unwrap()),
        76..=255 => out.extend_from_slice(&[0x4c, u8::try_from(data.len()).unwrap()]),
        _ => {
            out.push(0x4d);
            out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        }
    }
    out.extend_from_slice(data);
}

fn input(i: usize, script_sig: Vec<u8>) -> TxIn {
    TxIn {
        previous_output: OutPoint {
            txid: Txid::from_byte_array([u8::try_from(i % 251).unwrap() + 1; 32]),
            vout: u32::try_from(i / 251).unwrap(),
        },
        script_sig: ScriptBuf::from_bytes(script_sig),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::new(),
    }
}

fn shell(inputs: Vec<TxIn>, extra_outputs: usize) -> Transaction {
    let out = TxOut {
        value: Amount::from_sat(1_000),
        script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
    };
    let output = vec![out; extra_outputs + 1];
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: inputs,
        output,
    }
}

/// `n` inputs of one P2SH 2-of-3 multisig, signed by keys 1 and 3.
fn multisig_spend(n: usize) -> (Transaction, Vec<TxOut>) {
    let secp = Secp256k1::new();
    let sks: Vec<SecretKey> = (1u8..=3)
        .map(|b| SecretKey::from_slice(&[b; 32]).unwrap())
        .collect();
    let mut redeem = vec![0x52];
    for sk in &sks {
        push(&mut redeem, &sk.public_key(&secp).serialize());
    }
    redeem.extend_from_slice(&[0x53, 0xae]);
    let spk = ScriptBuf::new_p2sh(&ScriptBuf::from_bytes(redeem.clone()).script_hash());
    let mut tx = shell((0..n).map(|i| input(i, Vec::new())).collect(), 0);
    let redeem_script = ScriptBuf::from_bytes(redeem.clone());
    let digests: Vec<_> = {
        let cache = SighashCache::new(&tx);
        (0..n)
            .map(|i| {
                cache
                    .legacy_signature_hash(i, &redeem_script, EcdsaSighashType::All as u32)
                    .unwrap()
            })
            .collect()
    };
    for (i, digest) in digests.iter().enumerate() {
        let msg = Message::from_digest(digest.to_byte_array());
        let mut sig_sig = vec![0x00];
        for sk in [&sks[0], &sks[2]] {
            let mut der = secp.sign_ecdsa(&msg, sk).serialize_der().to_vec();
            der.push(EcdsaSighashType::All as u8);
            push(&mut sig_sig, &der);
        }
        push(&mut sig_sig, &redeem);
        tx.input[i].script_sig = ScriptBuf::from_bytes(sig_sig);
    }
    let prevouts = (0..n)
        .map(|_| TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: spk.clone(),
        })
        .collect();
    (tx, prevouts)
}

fn work_error(tx: &Transaction, prevouts: &[TxOut]) -> bool {
    match verify_tx_scripts_unconfirmed(prevouts.to_vec(), tx.clone()) {
        Ok(()) => false,
        Err(e) => {
            assert!(e.to_string().contains("legacy sighash work"), "{e}");
            true
        }
    }
}

/// `inputs` inputs, each spending `(OP_CHECKSIG OP_DROP) * checks OP_TRUE` with
/// one signature push per check. `hash_type(i)` is the hash type of check `i`.
/// `pad` extra outputs make the body larger.
fn checksig_spend(
    inputs: usize,
    checks: usize,
    pad: usize,
    hash_type: impl Fn(usize) -> u8,
) -> (Transaction, Vec<TxOut>) {
    let mut spk = Vec::new();
    let mut script_sig = Vec::new();
    for i in 0..checks {
        push(&mut script_sig, &dummy_sig(hash_type(i)));
        push(&mut script_sig, &PUBKEY);
        spk.extend_from_slice(&[0xac, 0x75]);
    }
    spk.push(0x51);
    assert!(checks * 2 < 201 && script_sig.len() <= 10_000);
    let tx = shell(
        (0..inputs).map(|i| input(i, script_sig.clone())).collect(),
        pad,
    );
    let prevouts = (0..inputs)
        .map(|_| TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: ScriptBuf::from_bytes(spk.clone()),
        })
        .collect();
    (tx, prevouts)
}

/// Bytes the unconfirmed check charges for distinct hashes: each one is the
/// non-witness size plus that input's scriptCode.
fn charged(tx: &Transaction, prevouts: &[TxOut], distinct_per_input: usize) -> u64 {
    prevouts
        .iter()
        .map(|p| distinct_per_input as u64 * (tx.base_size() as u64 + p.script_pubkey.len() as u64))
        .sum()
}

#[test]
fn p2sh_multisig_spends_of_22_30_and_60_inputs_pass_the_unconfirmed_check() {
    for n in [1usize, 21, 22, 30, 60] {
        let (tx, prevouts) = multisig_spend(n);
        // Keys 1 and 3 sign, so the walk tries three keys: three checks per
        // input, each hashing the body and the 105 byte redeem script.
        let redeem_len = 105u64;
        let need = 3 * n as u64 * (tx.base_size() as u64 + redeem_len);
        assert!(
            n < 22 || need > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES,
            "{n} inputs: a flat 400,000 byte budget would refuse this"
        );
        verify_tx_scripts_unconfirmed(prevouts.clone(), tx.clone())
            .unwrap_or_else(|e| panic!("{n} inputs: {e}"));
        verify_tx_scripts_detached(prevouts, tx).unwrap();
    }
}

#[test]
fn a_repeated_hash_type_and_script_code_is_hashed_and_paid_once() {
    // 99 CHECKSIGs, one hash type, over a body that is far past the budget.
    let (tx, prevouts) = checksig_spend(1, 99, 2_000, |_| 1);
    assert!(charged(&tx, &prevouts, 99) > unconfirmed_legacy_sighash_budget(&tx));
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("one hash serves every check");
}

#[test]
fn many_different_hashes_of_a_large_body_are_still_refused() {
    // The same shape with a new hash type per check: 99 distinct hashes of a
    // 20 kB body, about 2 MB, against a budget of about 420 kB.
    let (tx, prevouts) = checksig_spend(1, 99, 2_000, |i| u8::try_from(i + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
    // Block connect has no budget: the same transaction passes its scripts.
    verify_tx_scripts_detached(prevouts, tx).expect("consensus flags do not charge");
}

#[test]
fn many_inputs_each_asking_for_extra_hashes_share_one_budget() {
    // 8 inputs of 6 distinct hashes each. One hash per input is free, so 40
    // of 48 hashes are charged. A body of about 80 kB makes that 3 MB.
    let (tx, prevouts) = checksig_spend(8, 6, 2_000, |i| u8::try_from(i + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
    verify_tx_scripts_detached(prevouts, tx).unwrap();
    // The same inputs with one hash type each hash once per input.
    let (tx, prevouts) = checksig_spend(8, 6, 2_000, |_| 1);
    verify_tx_scripts_unconfirmed(prevouts, tx).unwrap();
}

/// The check refuses exactly when the model says: each distinct hash costs
/// the non-witness size plus scriptCode, and the whole transaction owns
/// `base_size * inputs + 400,000` bytes.
#[test]
fn the_unconfirmed_check_refuses_exactly_where_the_budget_ends() {
    let mut accepted_any = false;
    let mut refused_any = false;
    for checks in 1..=99usize {
        let (tx, prevouts) = checksig_spend(1, checks, 100, |i| u8::try_from(i + 1).unwrap());
        let want_refused = charged(&tx, &prevouts, checks) > unconfirmed_legacy_sighash_budget(&tx);
        assert_eq!(
            work_error(&tx, &prevouts),
            want_refused,
            "{checks} distinct hashes, charged {} of {}",
            charged(&tx, &prevouts, checks),
            unconfirmed_legacy_sighash_budget(&tx)
        );
        accepted_any |= !want_refused;
        refused_any |= want_refused;
    }
    assert!(
        accepted_any && refused_any,
        "the sweep must cross the budget"
    );
}

/// The memo keeps eight hashes. Cycling through eight hash types is paid
/// once each. A ninth pushes the oldest out, so every check of the cycle is
/// hashed and charged again.
#[test]
fn the_memo_is_bounded_and_a_miss_is_charged() {
    let (tx, prevouts) = checksig_spend(1, 90, 2_000, |i| u8::try_from(i % 8 + 1).unwrap());
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("eight hash types fit the memo");
    let (tx, prevouts) = checksig_spend(1, 90, 2_000, |i| u8::try_from(i % 9 + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
}
