//! Admit applies constant scriptCode and a legacy sighash byte cap.
//! Block connect keeps the consensus flag word.

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
/// Compressed secp256k1 generator. It parses, so CHECKSIG reaches sighash.
const PUBKEY: [u8; 33] = [
    0x02, 0x79, 0xBE, 0x66, 0x7E, 0xF9, 0xDC, 0xBB, 0xAC, 0x55, 0xA0, 0x62, 0x95, 0xCE, 0x87, 0x0B,
    0x07, 0x02, 0x9B, 0xFC, 0xDB, 0x2D, 0xCE, 0x28, 0xD9, 0x59, 0xF2, 0x81, 0x5B, 0x16, 0xF8, 0x17,
    0x98,
];

fn op_true() -> ScriptBuf {
    ScriptBuf::from_bytes(vec![0x51])
}

fn push_sig_pubkey() -> Vec<u8> {
    let mut v = Vec::with_capacity(44);
    v.push(DUMMY_SIG.len() as u8);
    v.extend_from_slice(&DUMMY_SIG);
    v.push(PUBKEY.len() as u8);
    v.extend_from_slice(&PUBKEY);
    v
}

/// CHECKSIG runs in the scriptPubKey. The signature stays in scriptSig, so
/// FindAndDelete does not see it in scriptCode.
fn checksig_script(repeats: u64) -> (ScriptBuf, ScriptBuf) {
    let chunk = push_sig_pubkey();
    let mut script_sig = Vec::with_capacity(chunk.len() * repeats as usize);
    let mut spk = Vec::with_capacity(repeats as usize * 2 + 1);
    for _ in 0..repeats {
        script_sig.extend_from_slice(&chunk);
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

/// Smallest repeat count whose `n * tx.total_size()` exceeds the admit cap.
fn over_cap_repeats(prev: &Transaction) -> u64 {
    let mut repeats = 1u64;
    loop {
        let (script_sig, _) = checksig_script(repeats);
        let cost = repeats * spend(prev, script_sig).total_size() as u64;
        if cost > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES {
            return repeats;
        }
        repeats += 1;
        assert!(
            repeats <= 200,
            "legacy sighash cap is not reachable under the script size cap"
        );
    }
}

#[test]
fn admit_rejects_codeseparator_and_legacy_sighash_work_block_connect_accepts() {
    let (dir, hub) = tiny_regtest_hub_labeled("unconf-script");
    hub.ensure_genesis().unwrap();
    let genesis = hub.tip_hash().unwrap();
    let t0 = hub.tip_header().unwrap().time;
    let probe = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array([1u8; 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000),
            script_pubkey: op_true(),
        }],
    };
    let over_n = over_cap_repeats(&probe);
    let (under_sig, under_spk) = checksig_script(1);
    let (over_sig, over_spk) = checksig_script(over_n);
    let codesep = ScriptBuf::from_bytes(vec![0xab, 0x51]);
    let b1 = mine_regtest_paying(genesis, t0.saturating_add(1), 1, codesep, vec![]);
    hub.accept_block(b1.clone()).unwrap();
    let b2 = mine_regtest_paying(b1.block_hash(), t0.saturating_add(2), 2, under_spk, vec![]);
    hub.accept_block(b2.clone()).unwrap();
    let b3 = mine_regtest_paying(b2.block_hash(), t0.saturating_add(3), 3, over_spk, vec![]);
    hub.accept_block(b3).unwrap();
    hub.confirm_empty_regtest(100).unwrap();
    assert_eq!(hub.tip_height(), Some(103));

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
    assert_eq!(hub.tip_height(), Some(104));

    let under_cb = coinbase_at(&hub, 2);
    let under = spend(&under_cb, under_sig);
    let under_cost = under.total_size() as u64;
    assert!(under_cost <= MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES);
    mp.accept_tx(&under)
        .expect("one legacy sighash of a small tx stays under the cap");
    assert!(mp.contains(&under.compute_txid()));

    let over_cb = coinbase_at(&hub, 3);
    let over = spend(&over_cb, over_sig);
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

fn two_input(a: &Transaction, b: &Transaction, sig_a: ScriptBuf, sig_b: ScriptBuf) -> Transaction {
    let value = a.output[0].value.to_sat() + b.output[0].value.to_sat() - 40_000;
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![
            TxIn {
                previous_output: OutPoint {
                    txid: a.compute_txid(),
                    vout: 0,
                },
                script_sig: sig_a,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: b.compute_txid(),
                    vout: 0,
                },
                script_sig: sig_b,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
        ],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: op_true(),
        }],
    }
}

/// Smallest repeat count where one input stays inside the cap and both do not.
fn shared_budget_repeats(prev: &Transaction) -> u64 {
    let mut repeats = 1u64;
    loop {
        let (sig, spk) = checksig_script(repeats);
        let tx = two_input(prev, prev, sig.clone(), sig);
        let per = repeats * (tx.base_size() as u64 + spk.len() as u64);
        if per <= MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
            && per.saturating_mul(2) > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
        {
            return repeats;
        }
        repeats += 1;
        assert!(
            repeats <= 100,
            "two inputs never cross the cap inside the script size limit"
        );
    }
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
    let push = push_sig_pubkey();
    let mut script_sig = Vec::new();
    for _ in 0..repeats {
        script_sig.extend_from_slice(&push);
        spk.push(0xac);
        spk.push(0x75);
    }
    spk.push(0x51);
    (
        ScriptBuf::from_bytes(script_sig),
        ScriptBuf::from_bytes(spk),
    )
}

fn scriptcode_over_total_size(prev: &Transaction) -> (u64, usize) {
    for pad in 1..=16 {
        for repeats in 1..80 {
            let (sig, spk) = padded_checksig(pad, repeats);
            if sig.len() > 10_000 || spk.len() > 10_000 {
                continue;
            }
            if pad + (repeats as usize) * 2 + 1 > 201 {
                continue;
            }
            let tx = spend(prev, sig);
            let by_total = repeats * tx.total_size() as u64;
            let by_code = repeats * (tx.base_size() as u64 + spk.len() as u64);
            if by_total <= MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
                && by_code > MAX_UNCONFIRMED_LEGACY_SIGHASH_BYTES
            {
                return (repeats, pad);
            }
        }
    }
    panic!("scriptCode charge is not distinguishable from total size");
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
fn admit_shares_the_sighash_budget_and_counts_scriptcode() {
    let probe = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array([1u8; 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000),
            script_pubkey: op_true(),
        }],
    };
    let shared_n = shared_budget_repeats(&probe);
    let (shared_sig, shared_spk) = checksig_script(shared_n);
    let (code_n, pad) = scriptcode_over_total_size(&probe);
    let (code_sig, code_spk) = padded_checksig(pad, code_n);
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
    mine(shared_spk.clone());
    mine(shared_spk);
    mine(code_spk);
    mine(delete_spk);
    mine(ScriptBuf::from_bytes(vec![0x00]));
    hub.confirm_empty_regtest(100).unwrap();

    let mp = MempoolHub::open(dir.join("mp"), Arc::clone(&hub.query)).unwrap();
    mp.set_relay_enabled(true);

    let left = coinbase_at(&hub, 1);
    let right = coinbase_at(&hub, 2);
    let both = two_input(&left, &right, shared_sig.clone(), shared_sig.clone());
    let err = mp
        .accept_tx(&both)
        .expect_err("two inputs share one legacy sighash budget");
    assert!(
        err.to_string().contains("legacy sighash work"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);
    assert!(!mp.contains(&both.compute_txid()));

    let one = spend(&left, shared_sig);
    mp.accept_tx(&one)
        .expect("one input of that same script stays under the cap");
    assert!(mp.contains(&one.compute_txid()));

    let wide = spend(&coinbase_at(&hub, 3), code_sig);
    let err = mp
        .accept_tx(&wide)
        .expect_err("scriptCode bytes count toward the cap");
    assert!(
        err.to_string().contains("legacy sighash work"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);

    let deleted = spend(&coinbase_at(&hub, 4), delete_sig);
    let err = mp
        .accept_tx(&deleted)
        .expect_err("a signature inside scriptCode is policy");
    assert!(
        err.to_string().contains("SIG_FINDANDDELETE"),
        "admit: {err}"
    );
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 0);

    let failed = spend(&coinbase_at(&hub, 5), ScriptBuf::new());
    let err = mp
        .accept_tx(&failed)
        .expect_err("a false script is still a script failure");
    assert_eq!(rbitcoin_net::peer::tx_reject_ban_score(&err), 10, "{err}");
    let _ = std::fs::remove_dir_all(dir);
}
