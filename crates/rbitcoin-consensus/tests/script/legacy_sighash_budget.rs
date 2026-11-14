//! The legacy sighash budget of an unconfirmed check. Every base hash is
//! charged the bytes its preimage serializes, so a standard P2SH multisig
//! spend of any input count and any mix of hash types fits, and a
//! transaction that asks for more hashing than a standard spend can need is
//! refused. Inputs that never hash earn no credit. The consensus check never
//! applies the budget.
use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use rbitcoin_consensus::script::interpreter::{
    LegacyPreimageSizes, MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES,
};
use rbitcoin_consensus::{verify_tx_scripts_detached, verify_tx_scripts_unconfirmed};

use bitcoin::hashes::Hash;

/// The flat budget this check had before the preimage charge: a standard
/// 22 input P2SH multisig spend was refused under it.
const OLD_FLAT_BUDGET: u64 = 400_000;

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

fn txout(script: &ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::from_sat(10_000),
        script_pubkey: script.clone(),
    }
}

/// Bytes one whole-transaction legacy preimage serializes for input `i`:
/// every other scriptSig blanked, `code` in this one, plus the hash type.
fn preimage_len(tx: &Transaction, i: usize, code: &[u8]) -> u64 {
    let mut t = tx.clone();
    for (j, inp) in t.input.iter_mut().enumerate() {
        inp.script_sig = if j == i {
            ScriptBuf::from_bytes(code.to_vec())
        } else {
            ScriptBuf::new()
        };
    }
    bitcoin::consensus::serialize(&t).len() as u64 + 4
}

/// Hash types whose preimage is the whole transaction: no ANYONECANPAY bit,
/// low five bits not NONE or SINGLE.
fn whole_tx_hash_types() -> Vec<u8> {
    (1u8..0x80).filter(|v| !matches!(v & 0x1f, 2 | 3)).collect()
}

/// A P2SH 2-of-3 or 3-of-3 multisig: redeem script, its scriptPubKey and keys.
fn multisig(m: u8) -> (Vec<SecretKey>, Vec<u8>, ScriptBuf) {
    let secp = Secp256k1::new();
    let sks: Vec<SecretKey> = (1u8..=3)
        .map(|b| SecretKey::from_slice(&[b; 32]).unwrap())
        .collect();
    let mut redeem = vec![0x50 + m];
    for sk in &sks {
        push(&mut redeem, &sk.public_key(&secp).serialize());
    }
    redeem.extend_from_slice(&[0x53, 0xae]);
    let spk = ScriptBuf::new_p2sh(&ScriptBuf::from_bytes(redeem.clone()).script_hash());
    (sks, redeem, spk)
}

/// `n` inputs of one P2SH multisig. `signers` are (key index, hash type) in
/// key order. `pad` extra outputs make the body larger.
fn multisig_spend_with(
    n: usize,
    m: u8,
    signers: &[(usize, EcdsaSighashType)],
    pad: usize,
) -> (Transaction, Vec<TxOut>) {
    let secp = Secp256k1::new();
    let (sks, redeem, spk) = multisig(m);
    let redeem_script = ScriptBuf::from_bytes(redeem.clone());
    let mut tx = shell((0..n).map(|i| input(i, Vec::new())).collect(), pad);
    let sigs: Vec<Vec<Vec<u8>>> = {
        let cache = SighashCache::new(&tx);
        (0..n)
            .map(|i| {
                signers
                    .iter()
                    .map(|(k, ty)| {
                        let d = cache
                            .legacy_signature_hash(i, &redeem_script, *ty as u32)
                            .unwrap();
                        let msg = Message::from_digest(d.to_byte_array());
                        let mut der = secp.sign_ecdsa(&msg, &sks[*k]).serialize_der().to_vec();
                        der.push(*ty as u8);
                        der
                    })
                    .collect()
            })
            .collect()
    };
    for (i, set) in sigs.iter().enumerate() {
        let mut script_sig = vec![0x00];
        for der in set {
            push(&mut script_sig, der);
        }
        push(&mut script_sig, &redeem);
        tx.input[i].script_sig = ScriptBuf::from_bytes(script_sig);
    }
    let prevouts = (0..n).map(|_| txout(&spk)).collect();
    (tx, prevouts)
}

/// `n` inputs of one P2SH 2-of-3 multisig, signed by keys 1 and 3 with ALL.
fn multisig_spend(n: usize) -> (Transaction, Vec<TxOut>) {
    let all = EcdsaSighashType::All;
    multisig_spend_with(n, 2, &[(0, all), (2, all)], 0)
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
    let spk = ScriptBuf::from_bytes(spk);
    let prevouts = (0..inputs).map(|_| txout(&spk)).collect();
    (tx, prevouts)
}

/// Bytes the check charges for `checks` distinct whole-transaction hashes of
/// every input: the preimage each one serializes.
fn charged(tx: &Transaction, prevouts: &[TxOut], checks: usize) -> u64 {
    prevouts
        .iter()
        .enumerate()
        .map(|(i, p)| checks as u64 * preimage_len(tx, i, p.script_pubkey.as_bytes()))
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
            n < 22 || need > OLD_FLAT_BUDGET,
            "{n} inputs: the old flat 400,000 byte budget refuses this"
        );
        verify_tx_scripts_unconfirmed(prevouts.clone(), tx.clone())
            .unwrap_or_else(|e| panic!("{n} inputs: {e}"));
        verify_tx_scripts_detached(prevouts, tx).unwrap();
    }
}

/// A 2-of-3 spend where one key signs ALL and another ALL|ANYONECANPAY needs
/// two hashes per input. 37 inputs were refused when each hash cost the whole
/// body and only one per input was free. Every count passes up to a 100 kvB
/// standard spend.
#[test]
fn a_multisig_signed_with_two_hash_types_is_admitted_at_any_input_count() {
    let signers = [
        (0usize, EcdsaSighashType::All),
        (2, EcdsaSighashType::AllPlusAnyoneCanPay),
    ];
    for n in [36usize, 37, 60, 100, 330] {
        let (tx, prevouts) = multisig_spend_with(n, 2, &signers, 0);
        if n == 330 {
            assert!(
                (95_000..=100_000).contains(&tx.base_size()),
                "{} bytes",
                tx.base_size()
            );
        }
        verify_tx_scripts_detached(prevouts.clone(), tx.clone()).expect("consensus valid");
        verify_tx_scripts_unconfirmed(prevouts, tx).unwrap_or_else(|e| panic!("{n} inputs: {e}"));
    }
}

/// A 3-of-3 spend with three hash types at 100 kvB: three hashes per input.
#[test]
fn a_three_of_three_with_three_hash_types_passes_at_100_kvb() {
    let signers = [
        (0usize, EcdsaSighashType::All),
        (1, EcdsaSighashType::AllPlusAnyoneCanPay),
        (2, EcdsaSighashType::None),
    ];
    let (tx, prevouts) = multisig_spend_with(260, 3, &signers, 0);
    assert!(
        tx.base_size() <= 100_000,
        "{} bytes over 100 kvB",
        tx.base_size()
    );
    verify_tx_scripts_detached(prevouts.clone(), tx.clone()).expect("consensus valid");
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("admitted");
}

/// Bare `OP_TRUE` inputs hash nothing, so they must not pay for the inputs
/// that do. 2,400 of them used to fund 22 inputs of 100 CHECKSIGs over 100 hash
/// types: 228,868,200 bytes hashed by a 99 kB transaction.
#[test]
fn inputs_that_never_hash_cannot_pay_for_22_script_inputs() {
    // 100 CHECKSIG DROP with the signature in the scriptPubKey. PUSHDATA1 keeps
    // FindAndDelete from matching, so constant scriptCode holds.
    let mut spk = Vec::new();
    for ht in &whole_tx_hash_types()[..100] {
        spk.extend_from_slice(&[0x4c, 0x09]);
        spk.extend_from_slice(&dummy_sig(*ht));
        push(&mut spk, &PUBKEY);
        spk.extend_from_slice(&[0xac, 0x75]);
    }
    spk.push(0x51);
    let (script_inputs, credit) = (22usize, 2_400usize);
    let tx = shell(
        (0..script_inputs + credit)
            .map(|i| input(i, Vec::new()))
            .collect(),
        0,
    );
    let spk = ScriptBuf::from_bytes(spk);
    let mut prevouts = vec![txout(&spk); script_inputs];
    prevouts.extend(vec![txout(&ScriptBuf::from_bytes(vec![0x51])); credit]);
    let hashed = (script_inputs * 100) as u64 * preimage_len(&tx, 0, spk.as_bytes());
    assert!(tx.weight().to_wu() < 400_000);
    assert_eq!(hashed, 228_868_200);
    assert!(hashed > 7 * MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    assert!(work_error(&tx, &prevouts));
    verify_tx_scripts_detached(prevouts, tx).expect("block connect has no budget");
}

/// A signed P2PKH spend of `n` inputs and `pad` extra outputs.
fn p2pkh_spend(
    n: usize,
    pad: usize,
    extra: Vec<TxIn>,
) -> (Transaction, Vec<TxOut>, ScriptBuf, usize) {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let pk = bitcoin::PublicKey::new(sk.public_key(&secp));
    let p2pkh = ScriptBuf::new_p2pkh(&pk.pubkey_hash());
    let first = extra.len();
    let mut inputs = extra;
    inputs.extend((0..n).map(|i| input(first + i, Vec::new())));
    let mut tx = shell(inputs, pad);
    let digests: Vec<_> = {
        let cache = SighashCache::new(&tx);
        (first..first + n)
            .map(|i| {
                cache
                    .legacy_signature_hash(i, &p2pkh, EcdsaSighashType::All as u32)
                    .unwrap()
            })
            .collect()
    };
    for (k, d) in digests.iter().enumerate() {
        let msg = Message::from_digest(d.to_byte_array());
        let mut der = secp.sign_ecdsa(&msg, &sk).serialize_der().to_vec();
        der.push(1);
        let mut script_sig = Vec::new();
        push(&mut script_sig, &der);
        push(&mut script_sig, &pk.to_bytes());
        tx.input[first + k].script_sig = ScriptBuf::from_bytes(script_sig);
    }
    let prevouts = (0..n).map(|_| txout(&p2pkh)).collect();
    (tx, prevouts, p2pkh, first)
}

/// The P2PKH inputs hash the whole body once each and used to do it outside
/// the budget they added to: ten of them beside one input of 15 hash types
/// and 9,000 outputs hashed 2,262,640 bytes against a budget of 1,414,200.
/// They are charged now, and the charge fits.
#[test]
fn p2pkh_inputs_are_charged_to_the_budget_they_share() {
    let types = whole_tx_hash_types();
    let (proto, script_prev) = checksig_spend(1, 15, 0, |i| types[i]);
    let (tx, mut prevouts, p2pkh, first) = p2pkh_spend(10, 9_000, vec![proto.input[0].clone()]);
    assert_eq!(first, 1);
    prevouts.insert(0, script_prev[0].clone());
    let hashed = 15 * preimage_len(&tx, 0, prevouts[0].script_pubkey.as_bytes())
        + (1..=10)
            .map(|i| preimage_len(&tx, i, p2pkh.as_bytes()))
            .sum::<u64>();
    assert!(hashed > 2_000_000 && hashed < MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("admitted");
}

/// The most a P2PKH-only transaction can hash at the 400,000 weight limit is
/// about 23.4 MB (467 inputs and 3,000 outputs). It stays admitted.
#[test]
fn a_p2pkh_spend_at_the_standard_weight_limit_is_admitted() {
    let (tx, prevouts, p2pkh, _) = p2pkh_spend(467, 3_000, Vec::new());
    assert!(tx.weight().to_wu() <= 400_000, "{}", tx.weight().to_wu());
    let hashed: u64 = (0..467)
        .map(|i| preimage_len(&tx, i, p2pkh.as_bytes()))
        .sum();
    assert!(hashed > 22_000_000 && hashed < MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("admitted");
}

/// P2PKH hashing counts against the same total: the 23 MB above plus 297
/// hashes of the same body from three script inputs pass the ceiling.
#[test]
fn p2pkh_hashing_and_script_hashing_share_one_ceiling() {
    let types = whole_tx_hash_types();
    let script_sig = {
        let mut s = Vec::new();
        for ty in types.iter().take(99) {
            push(&mut s, &dummy_sig(*ty));
            push(&mut s, &PUBKEY);
        }
        s
    };
    let extra: Vec<TxIn> = (0..3).map(|i| input(900 + i, script_sig.clone())).collect();
    let (tx, mut prevouts, p2pkh, first) = p2pkh_spend(467, 3_000, extra);
    let mut spk = Vec::new();
    for _ in 0..99 {
        spk.extend_from_slice(&[0xac, 0x75]);
    }
    spk.push(0x51);
    let spk = ScriptBuf::from_bytes(spk);
    let mut all: Vec<TxOut> = (0..3).map(|_| txout(&spk)).collect();
    all.append(&mut prevouts);
    let p2pkh_bytes: u64 = (first..first + 467)
        .map(|i| preimage_len(&tx, i, p2pkh.as_bytes()))
        .sum();
    let script_bytes: u64 = 3 * 99 * preimage_len(&tx, 0, spk.as_bytes());
    assert!(p2pkh_bytes < MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    assert!(script_bytes < MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    assert!(p2pkh_bytes + script_bytes > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    assert!(work_error(&tx, &all));
    verify_tx_scripts_detached(all, tx).expect("block connect has no budget");
}

/// 99 CHECKSIGs, one hash type, over a body that would be far past the budget
/// if every check were charged.
#[test]
fn a_repeated_hash_type_and_script_code_is_hashed_and_paid_once() {
    let (tx, prevouts) = checksig_spend(5, 99, 8_000, |_| 1);
    assert!(charged(&tx, &prevouts, 99) > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("one hash serves every check");
}

#[test]
fn many_different_hashes_of_a_large_body_are_refused() {
    // The same shape with a new hash type per check: 5 x 99 distinct hashes of
    // an 80 kB body, about 40 MB.
    let (tx, prevouts) = checksig_spend(5, 99, 8_000, |i| u8::try_from(i + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
    // Block connect has no budget: the same transaction passes its scripts.
    verify_tx_scripts_detached(prevouts, tx).expect("consensus flags do not charge");
}

#[test]
fn inputs_asking_for_extra_hashes_share_one_budget() {
    // 8 inputs of 99 distinct hashes each over an 80 kB body is 63 MB.
    let (tx, prevouts) = checksig_spend(8, 99, 8_000, |i| u8::try_from(i + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
    verify_tx_scripts_detached(prevouts, tx).unwrap();
    // The same inputs with one hash type each hash once per input.
    let (tx, prevouts) = checksig_spend(8, 99, 8_000, |_| 1);
    verify_tx_scripts_unconfirmed(prevouts, tx).unwrap();
}

/// The check refuses exactly when the model says: each distinct hash costs the
/// bytes its preimage serializes, and the whole transaction owns one fixed
/// ceiling.
#[test]
fn the_unconfirmed_check_refuses_exactly_where_the_budget_ends() {
    let types = whole_tx_hash_types();
    let mut accepted_any = false;
    let mut refused_any = false;
    for checks in (1..=99usize).step_by(7).chain([88, 89, 90, 91, 92, 99]) {
        let (tx, prevouts) = checksig_spend(4, checks, 9_000, |i| types[i]);
        let cost = charged(&tx, &prevouts, checks);
        let want_refused = cost > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES;
        assert_eq!(
            work_error(&tx, &prevouts),
            want_refused,
            "{checks} distinct hashes, charged {cost} of {MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES}"
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
    let (tx, prevouts) = checksig_spend(6, 90, 9_000, |i| u8::try_from(i % 8 + 1).unwrap());
    verify_tx_scripts_unconfirmed(prevouts, tx).expect("eight hash types fit the memo");
    let (tx, prevouts) = checksig_spend(6, 90, 9_000, |i| u8::try_from(i % 9 + 1).unwrap());
    assert!(work_error(&tx, &prevouts));
}

/// The charge is the length of the preimage the library serializes, for every
/// hash type byte, signed input and scriptCode length, on a small transaction
/// and one that crosses the compact-size boundaries. A SINGLE hash with no
/// output of its own hashes nothing and is charged nothing.
#[test]
fn the_charge_is_the_serialized_preimage_length_for_every_hash_type() {
    use bitcoin::sighash::EncodeSigningDataResult;
    let wide_out = |len: usize, value: u64| TxOut {
        value: Amount::from_sat(value),
        script_pubkey: ScriptBuf::from_bytes(vec![0x51; len]),
    };
    let mut small = shell((0..3).map(|i| input(i, vec![0x51; i * 7])).collect(), 0);
    small.output.push(wide_out(40, 5));
    small.output.push(wide_out(300, 6));
    let mut wide = shell((0..300).map(|i| input(i, vec![0x51; i % 5])).collect(), 258);
    wide.output.push(wide_out(300, 9));
    let mut checked = 0;
    for tx in [&small, &wide] {
        let sizes = LegacyPreimageSizes::of(tx);
        let cache = SighashCache::new(tx);
        for ty in 0..=255u32 {
            for i in [0usize, 1, 2, 259, 260, 299] {
                if i >= tx.input.len() {
                    continue;
                }
                for len in [0usize, 4, 25, 105, 252, 253, 520] {
                    let code = ScriptBuf::from_bytes(vec![0x51; len]);
                    let mut preimage = Vec::new();
                    let want =
                        match cache.legacy_encode_signing_data_to(&mut preimage, i, &code, ty) {
                            EncodeSigningDataResult::SighashSingleBug => 0,
                            EncodeSigningDataResult::WriteResult(r) => {
                                r.unwrap();
                                preimage.len() as u64
                            }
                        };
                    assert_eq!(
                        sizes.preimage_len(tx, i, ty, len),
                        want,
                        "type {ty:#04x} input {i} scriptCode {len}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 5_000);
}
