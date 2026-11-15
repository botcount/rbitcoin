use bitcoin::hashes::Hash;
use rbitcoin_consensus::{ChainParams, Milestone};
use rbitcoin_primitives::Height;
use rbitcoin_query::confirm_run::confirm_wire_run;
use rbitcoin_query::connect_facade::*;
use rbitcoin_query::ConfirmError;
use rbitcoin_query::Query;

use bitcoin::absolute::LockTime;
use bitcoin::block::Version;

use bitcoin::script::ScriptBuf;
use bitcoin::transaction::Version as TxVersion;
use bitcoin::{Amount, OutPoint, Sequence, Target, Transaction, TxIn, TxOut, Witness};
use rbitcoin_consensus::block::ScriptCheckJob;
use rbitcoin_consensus::{genesis_block, mine_empty_regtest, ConsensusError};

use std::path::PathBuf;
fn temp_store() -> (PathBuf, Query) {
    let path = rbitcoin_store::testutil::fresh_path("consensus-cov");
    std::fs::create_dir_all(&path).unwrap();
    let q = Query::open_or_create_tiny(&path).expect("open store");
    (path, q)
}

#[test]
fn verify_tx_scripts_detached_acs_job() {
    let tx = Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array([1; 32]),
                vout: 0,
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
    let prevouts = vec![TxOut {
        value: Amount::from_sat(10),
        script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
    }];
    rbitcoin_consensus::verify_tx_scripts_detached(prevouts.clone(), tx.clone()).unwrap();
    let job = ScriptCheckJob::new(
        prevouts,
        tx,
        rbitcoin_consensus::block::ScriptVerifyFlags::buried(true, true, true, true, true),
    );
    rbitcoin_consensus::block::verify_one_script_job(&job).unwrap();
}

#[test]
fn verify_tx_scripts_detached_forks_op_true_and_op_return() {
    let spend = |spk: Vec<u8>| {
        let tx = Transaction {
            version: TxVersion::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::from_byte_array([1; 32]),
                    vout: 0,
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
        let prevouts = vec![TxOut {
            value: Amount::from_sat(10),
            script_pubkey: ScriptBuf::from_bytes(spk),
        }];
        rbitcoin_consensus::verify_tx_scripts_detached_forks(
            prevouts, tx, true, true, true, true, true,
        )
    };
    spend(vec![0x51]).unwrap();
    assert!(spend(vec![0x6a]).is_err());
}

#[test]
fn regtest_connect_archive_and_confirm_path() {
    let (path, q) = temp_store();
    let params = ChainParams::regtest();
    let ms = Milestone::height(1_000_000);
    let genesis = genesis_block(&params);
    let gen_fk = accept_and_connect_block(&q, &params, Height::GENESIS, &genesis, ms).unwrap();

    let b1 = mine_empty_regtest(genesis.block_hash(), genesis.header.time + 600, 1);
    // prepare helpers stay (CPU-side); confirm is sole Class A.
    let (_hr, _txs) = prepare_block_for_archive(&q, &params, &b1).unwrap();
    accept_and_connect_block(&q, &params, Height(1), &b1, ms).unwrap();
    // already-have prepare after connect keeps the parent fk
    let (hr, _) = prepare_block_for_archive(&q, &params, &b1).unwrap();
    assert_eq!(hr.prev_fk, gen_fk);

    let _ = std::fs::remove_dir_all(&path);
}

#[test]
fn assemble_second_block_rejects_stale_nversion() {
    let (path, q) = temp_store();
    let params = ChainParams::regtest();
    let ms = Milestone::height(1_000_000);
    let genesis = genesis_block(&params);
    accept_and_connect_block(&q, &params, Height::GENESIS, &genesis, ms).unwrap();

    let b1 = mine_empty_regtest(genesis.block_hash(), genesis.header.time + 600, 1);
    let mut b2 = mine_empty_regtest(b1.block_hash(), b1.header.time + 600, 2);
    b2.header.version = Version::from_consensus(1);
    let target = Target::from_compact(b2.header.bits);
    for nonce in 0..u32::MAX {
        b2.header.nonce = nonce;
        if b2.header.validate_pow(target).is_ok() {
            break;
        }
    }
    let err = confirm_wire_run(&q, &params, ms, &[(Height(1), b1), (Height(2), b2)]).unwrap_err();
    assert!(
        matches!(err, ConfirmError::Rules(ConsensusError::BadVersion(1))),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&path);
}
