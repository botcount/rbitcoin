//! Admit applies constant scriptCode and a legacy sighash byte budget.
//! Block connect keeps the consensus flag word.
//!
//! Every base hash is charged the bytes its preimage serializes against
//! `MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES`, one total per transaction, and a
//! repeated hash type of one input is paid once. A script that checks a new
//! hash type each time is what reaches it, so these scripts push a different
//! hash type per CHECKSIG.

use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
use rbitcoin_consensus::mine_regtest_paying;
use rbitcoin_consensus::script::interpreter::MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES;
use rbitcoin_net::chain::tiny_regtest_hub_labeled;
use rbitcoin_net::tx_relay::MempoolHub;
use rbitcoin_primitives::Height;
use std::sync::Arc;

const DUMMY_SIG: [u8; 9] = [0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01];

/// The dummy signature with hash type `ht`. A different type is a different hash.
fn dummy_sig_ht(ht: u8) -> [u8; 9] {
    let mut sig = DUMMY_SIG;
    sig[8] = ht;
    sig
}

/// A distinct whole-transaction hash type for check `i`: no ANYONECANPAY bit,
/// low five bits not NONE or SINGLE, so the preimage is the whole body.
fn ht_of(i: u64) -> u8 {
    let types: Vec<u8> = (1u8..0x80).filter(|v| !matches!(v & 0x1f, 2 | 3)).collect();
    types[usize::try_from(i).unwrap() % types.len()]
}
/// Compressed secp256k1 generator. It parses, so CHECKSIG reaches sighash.
const PUBKEY: [u8; 33] = [
    0x02, 0x79, 0xBE, 0x66, 0x7E, 0xF9, 0xDC, 0xBB, 0xAC, 0x55, 0xA0, 0x62, 0x95, 0xCE, 0x87, 0x0B,
    0x07, 0x02, 0x9B, 0xFC, 0xDB, 0x2D, 0xCE, 0x28, 0xD9, 0x59, 0xF2, 0x81, 0x5B, 0x16, 0xF8, 0x17,
    0x98,
];

fn op_true() -> ScriptBuf {
    ScriptBuf::from_bytes(vec![0x51])
}

fn push_sig_pubkey(ht: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(44);
    v.push(DUMMY_SIG.len() as u8);
    v.extend_from_slice(&dummy_sig_ht(ht));
    v.push(PUBKEY.len() as u8);
    v.extend_from_slice(&PUBKEY);
    v
}

/// CHECKSIG runs in the scriptPubKey. The signature stays in scriptSig, so
/// FindAndDelete does not see it in scriptCode.
fn checksig_script(repeats: u64) -> (ScriptBuf, ScriptBuf) {
    let mut script_sig = Vec::with_capacity(44 * repeats as usize);
    let mut spk = Vec::with_capacity(repeats as usize * 2 + 1);
    for i in 0..repeats {
        script_sig.extend_from_slice(&push_sig_pubkey(ht_of(i)));
        spk.push(0xac);
        spk.push(0x75);
    }
    spk.push(0x51);
    assert!(script_sig.len() <= 10_000 && spk.len() <= 10_000);
    (
        ScriptBuf::from_bytes(script_sig),
        ScriptBuf::from_bytes(spk),
    )
}

fn spend(prev: &Transaction, script_sig: ScriptBuf) -> Transaction {
    let value = prev.output[0].value.to_sat() - 20_000;
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: prev.compute_txid(),
                vout: 0,
            },
            script_sig,
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: op_true(),
        }],
    }
}

fn coinbase_at(hub: &rbitcoin_net::ChainHub, height: u32) -> Transaction {
    hub.query
        .reconstruct_block_at_height(Height(height))
        .unwrap()
        .txdata
        .into_iter()
        .next()
        .unwrap()
}

/// One input per coin, each with `script_sig`, and `outputs` outputs of 1,000
/// sats that make the body (and so every hash of it) large.
fn wide_spend(coins: &[Transaction], script_sig: &ScriptBuf, outputs: usize) -> Transaction {
    let total: u64 = coins.iter().map(|c| c.output[0].value.to_sat()).sum();
    let mut output = vec![
        TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: op_true(),
        };
        outputs
    ];
    output.push(TxOut {
        value: Amount::from_sat(total - 20_000 - 1_000 * outputs as u64),
        script_pubkey: op_true(),
    });
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: coins
            .iter()
            .map(|c| TxIn {
                previous_output: OutPoint {
                    txid: c.compute_txid(),
                    vout: 0,
                },
                script_sig: script_sig.clone(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output,
    }
}

/// Bytes one whole-transaction legacy preimage serializes for input 0: every
/// other scriptSig blanked, `code` in this one, plus the hash type.
fn preimage_len(tx: &Transaction, code: &[u8]) -> u64 {
    let mut t = tx.clone();
    for (j, inp) in t.input.iter_mut().enumerate() {
        inp.script_sig = if j == 0 {
            ScriptBuf::from_bytes(code.to_vec())
        } else {
            ScriptBuf::new()
        };
    }
    bitcoin::consensus::serialize(&t).len() as u64 + 4
}

const CHECKS: u64 = 99;
const WIDE_OUTPUTS: usize = 7_000;

#[test]
fn admit_rejects_codeseparator_and_legacy_sighash_work_block_connect_accepts() {
    let (dir, hub) = tiny_regtest_hub_labeled("unconf-script");
    hub.ensure_genesis().unwrap();
    let genesis = hub.tip_hash().unwrap();
    let t0 = hub.tip_header().unwrap().time;
    let (sig, spk) = checksig_script(CHECKS);
    let codesep = ScriptBuf::from_bytes(vec![0xab, 0x51]);
    let mut prev = genesis;
    let mut mine = |height: u32, script: ScriptBuf| {
        let block = mine_regtest_paying(prev, t0.saturating_add(height), height, script, vec![]);
        prev = block.block_hash();
        hub.accept_block(block).unwrap();
    };
    mine(1, codesep);
    // Coins at heights 2..=7 run 99 CHECKSIGs over 99 hash types.
    for height in 2..=7 {
        mine(height, spk.clone());
    }
    hub.confirm_empty_regtest(100).unwrap();
    assert_eq!(hub.tip_height(), Some(107));

    let mp = MempoolHub::open(dir.join("mp"), Arc::clone(&hub.query)).unwrap();
    mp.set_relay_enabled(true);

    let codesep_cb = coinbase_at(&hub, 1);
    let codesep_spend = spend(&codesep_cb, ScriptBuf::new());
    let err = mp
        .accept_tx(&codesep_spend)
        .expect_err("constant scriptCode rejects OP_CODESEPARATOR");
    assert!(err.to_string().contains("OP_CODESEPARATOR"), "admit: {err}");
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);
    assert!(!mp.contains(&codesep_spend.compute_txid()));

    let prev = hub.tip_hash().unwrap();
    let time = hub.tip_header().unwrap().time.saturating_add(1);
    let height = hub.tip_height().unwrap().saturating_add(1);
    let block = mine_regtest_paying(prev, time, height, op_true(), vec![codesep_spend]);
    hub.accept_block(block)
        .expect("consensus flags still accept OP_CODESEPARATOR OP_TRUE");
    assert_eq!(hub.tip_height(), Some(108));

    // One input: 99 hashes of a 70 kB body, about 7 MB, inside the budget.
    let under = wide_spend(&[coinbase_at(&hub, 2)], &sig, WIDE_OUTPUTS);
    let under_cost = CHECKS * preimage_len(&under, spk.as_bytes());
    assert!(under_cost < MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    mp.accept_tx(&under)
        .expect("99 legacy sighashes of one input stay under the cap");
    assert!(mp.contains(&under.compute_txid()));

    // Five such inputs share one budget: about 35 MB.
    let coins: Vec<Transaction> = (3..=7).map(|h| coinbase_at(&hub, h)).collect();
    let over = wide_spend(&coins, &sig, WIDE_OUTPUTS);
    assert!(over.weight().to_wu() <= 400_000);
    assert!(
        5 * CHECKS * preimage_len(&over, spk.as_bytes()) > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
    );
    let err = mp
        .accept_tx(&over)
        .expect_err("repeated full-tx legacy sighashes exceed the cap");
    assert!(
        err.to_string().contains("legacy sighash work"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);
    assert!(!mp.contains(&over.compute_txid()));
    let _ = std::fs::remove_dir_all(dir);
}

/// Pad is executed and dropped. It sits in scriptCode and not in the spender.
fn padded_checksig(pad_chunks: usize, repeats: u64) -> (ScriptBuf, ScriptBuf) {
    let mut spk = Vec::new();
    let chunk = [0x11u8; 520];
    for _ in 0..pad_chunks {
        spk.push(0x4d);
        spk.extend_from_slice(&520u16.to_le_bytes());
        spk.extend_from_slice(&chunk);
        spk.push(0x75);
    }
    let mut script_sig = Vec::new();
    for i in 0..repeats {
        script_sig.extend_from_slice(&push_sig_pubkey(ht_of(i)));
        spk.push(0xac);
        spk.push(0x75);
    }
    spk.push(0x51);
    (
        ScriptBuf::from_bytes(script_sig),
        ScriptBuf::from_bytes(spk),
    )
}

/// Output count where five inputs of the padded script stay inside the budget
/// without their scriptCode and do not with it.
fn scriptcode_decides(sig: &ScriptBuf, spk: &ScriptBuf, repeats: u64) -> usize {
    let hashes = 5 * repeats;
    for outputs in (1_000..9_000).step_by(100) {
        let coin = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_0000_0000),
                script_pubkey: op_true(),
            }],
        };
        let tx = wide_spend(&vec![coin; 5], sig, outputs);
        let without = hashes * preimage_len(&tx, &[]);
        let with = hashes * preimage_len(&tx, spk.as_bytes());
        if without <= MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
            && with > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
        {
            return outputs;
        }
    }
    panic!("scriptCode charge is not distinguishable from the body");
}

fn findanddelete_scripts() -> (ScriptBuf, ScriptBuf) {
    let mut sig_push = Vec::with_capacity(10);
    sig_push.push(DUMMY_SIG.len() as u8);
    sig_push.extend_from_slice(&DUMMY_SIG);
    let mut spk = sig_push.clone();
    spk.extend_from_slice(&[0x75, 0xac, 0x75, 0x51]);
    let mut script_sig = sig_push;
    script_sig.push(PUBKEY.len() as u8);
    script_sig.extend_from_slice(&PUBKEY);
    (
        ScriptBuf::from_bytes(script_sig),
        ScriptBuf::from_bytes(spk),
    )
}

#[test]
fn admit_counts_scriptcode_in_the_budget_and_refuses_other_script_failures() {
    // 16 pad chunks and 92 checks: 200 non-push ops, 8.6 kB of scriptCode.
    let repeats = 92u64;
    let (code_sig, code_spk) = padded_checksig(16, repeats);
    assert!(code_spk.len() <= 10_000 && code_sig.len() <= 10_000);
    let outputs = scriptcode_decides(&code_sig, &code_spk, repeats);
    let (delete_sig, delete_spk) = findanddelete_scripts();

    let (dir, hub) = tiny_regtest_hub_labeled("unconf-script-budget");
    hub.ensure_genesis().unwrap();
    let mut prev = hub.tip_hash().unwrap();
    let mut time = hub.tip_header().unwrap().time;
    let mut height = 0u32;
    let mut mine = |spk: ScriptBuf| {
        height += 1;
        time = time.saturating_add(1);
        let block = mine_regtest_paying(prev, time, height, spk, vec![]);
        prev = block.block_hash();
        hub.accept_block(block).unwrap();
    };
    for _ in 0..5 {
        mine(code_spk.clone());
    }
    mine(delete_spk);
    mine(ScriptBuf::from_bytes(vec![0x00]));
    hub.confirm_empty_regtest(100).unwrap();

    let mp = MempoolHub::open(dir.join("mp"), Arc::clone(&hub.query)).unwrap();
    mp.set_relay_enabled(true);

    let coins: Vec<Transaction> = (1..=5).map(|h| coinbase_at(&hub, h)).collect();
    let wide = wide_spend(&coins, &code_sig, outputs);
    assert!(wide.weight().to_wu() <= 400_000);
    let err = mp
        .accept_tx(&wide)
        .expect_err("scriptCode bytes count toward the cap");
    assert!(
        err.to_string().contains("legacy sighash work"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);
    // Four inputs of the same script and body are inside it.
    let four = wide_spend(&coins[..4], &code_sig, outputs);
    mp.accept_tx(&four).expect("four inputs stay under the cap");
    assert!(mp.contains(&four.compute_txid()));

    let deleted = spend(&coinbase_at(&hub, 6), delete_sig);
    let err = mp
        .accept_tx(&deleted)
        .expect_err("a signature inside scriptCode is policy");
    assert!(
        err.to_string().contains("SIG_FINDANDDELETE"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);

    let failed = spend(&coinbase_at(&hub, 7), ScriptBuf::new());
    let err = mp
        .accept_tx(&failed)
        .expect_err("a false script is still a script failure");
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 10, "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

fn push_data(out: &mut Vec<u8>, data: &[u8]) {
    if data.len() > 75 {
        out.extend_from_slice(&[0x4c, u8::try_from(data.len()).unwrap()]);
    } else {
        out.push(u8::try_from(data.len()).unwrap());
    }
    out.extend_from_slice(data);
}

/// A standard consolidation: P2SH 2-of-3 multisig coins signed by keys 1 and 3
/// into one output. Admit takes it at any input count, as Core does, and with
/// a second hash type per input as well (ALL and ALL|ANYONECANPAY), up to a
/// 100 kvB spend.
#[test]
fn admit_accepts_p2sh_multisig_spends_of_any_input_count_and_hash_types() {
    use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};

    let secp = Secp256k1::new();
    let sks: Vec<SecretKey> = (1u8..=3)
        .map(|b| SecretKey::from_slice(&[b; 32]).unwrap())
        .collect();
    let mut redeem = vec![0x52];
    for sk in &sks {
        push_data(&mut redeem, &sk.public_key(&secp).serialize());
    }
    redeem.extend_from_slice(&[0x53, 0xae]);
    let redeem_script = ScriptBuf::from_bytes(redeem.clone());
    let spk = ScriptBuf::new_p2sh(&redeem_script.script_hash());

    let (dir, hub) = tiny_regtest_hub_labeled("unconf-multisig");
    hub.ensure_genesis().unwrap();
    let mut prev = hub.tip_hash().unwrap();
    let mut time = hub.tip_header().unwrap().time;
    let all = EcdsaSighashType::All;
    let acp = EcdsaSighashType::AllPlusAnyoneCanPay;
    // Each spend takes its own run of coins, so all of them sit together.
    let spends: [(u32, [(usize, EcdsaSighashType); 2]); 6] = [
        (22, [(0, all), (2, all)]),
        (30, [(0, all), (2, all)]),
        (60, [(0, all), (2, all)]),
        (37, [(0, all), (2, acp)]),
        (100, [(0, all), (2, acp)]),
        (330, [(0, all), (2, acp)]),
    ];
    let total: u32 = spends.iter().map(|(n, _)| n).sum();
    for height in 1..=total {
        time = time.saturating_add(1);
        let block = mine_regtest_paying(prev, time, height, spk.clone(), vec![]);
        prev = block.block_hash();
        hub.accept_block(block).unwrap();
    }
    hub.confirm_empty_regtest(100).unwrap();
    let mp = MempoolHub::open(dir.join("mp"), Arc::clone(&hub.query)).unwrap();
    mp.set_relay_enabled(true);

    let mut first = 1u32;
    for (n, signers) in spends {
        let coins: Vec<Transaction> = (first..first + n).map(|h| coinbase_at(&hub, h)).collect();
        let mut tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: coins
                .iter()
                .map(|c| TxIn {
                    previous_output: OutPoint {
                        txid: c.compute_txid(),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output: vec![TxOut {
                value: Amount::from_sat(
                    coins
                        .iter()
                        .map(|c| c.output[0].value.to_sat())
                        .sum::<u64>()
                        - 100_000,
                ),
                script_pubkey: op_true(),
            }],
        };
        let sigs: Vec<Vec<Vec<u8>>> = {
            let cache = SighashCache::new(&tx);
            (0..n as usize)
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
                push_data(&mut script_sig, der);
            }
            push_data(&mut script_sig, &redeem);
            tx.input[i].script_sig = ScriptBuf::from_bytes(script_sig);
        }
        assert!(tx.weight().to_wu() <= 400_000, "{n} inputs");
        mp.accept_tx(&tx)
            .unwrap_or_else(|e| panic!("{n} inputs: {e}"));
        assert!(mp.contains(&tx.compute_txid()), "{n} inputs");
        first += n;
    }
    let _ = std::fs::remove_dir_all(dir);
}
