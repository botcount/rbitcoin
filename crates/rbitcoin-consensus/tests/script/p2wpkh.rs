use bitcoin::Transaction;
use rbitcoin_consensus::block::ScriptCheckJob;
use rbitcoin_consensus::script::classify::{program_len, ProgramLen};
use rbitcoin_consensus::script::p2wpkh::*;

use bitcoin::absolute::LockTime;
use bitcoin::script::ScriptBuf;
use bitcoin::{Amount, OutPoint, Sequence, TxIn, TxOut, Witness};

fn fixture_keyhash(spk: &[u8]) -> &[u8; 20] {
    let ProgramLen::N20(keyhash) = program_len(&spk[2..]) else {
        panic!("fixture is a 20-byte program");
    };
    keyhash
}

fn job_with_witness(items: &[&[u8]]) -> ScriptCheckJob {
    let mut spk = vec![0x00, 0x14];
    spk.extend([0u8; 20]);
    {
        let txid = [0u8; 32];
        let prevouts = vec![TxOut {
            value: Amount::from_sat(10),
            script_pubkey: ScriptBuf::from_bytes(spk),
        }];
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(items),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        };
        let flags = rbitcoin_consensus::block::ScriptVerifyFlags {
            bip65_active: true,
            bip112_active: true,
            bip66_active: true,
            bip16_active: true,
            taproot_active: true,
            minimal_if: false,
            nullfail: false,
            low_s: false,
            strictenc: false,
            null_dummy: false,
            minimal_data: false,
            witness_pubkeytype: false,
            witness_active: true,
            discourage_upgradable_witness: false,
            const_scriptcode: false,
            cleanstack: false,
        };
        ScriptCheckJob::with_txid(txid, prevouts, tx, flags)
    }
}

#[test]
fn witness_shape_errors() {
    let job = job_with_witness(&[]);
    let pre = rbitcoin_consensus::TxScriptPrecompute::from_tx(&job.tx);
    assert!({
        let i = job.inputs().next().unwrap();
        let spk = job.prevout_script(i).expect("spk");
        let amount = job.prevout_amount(i).expect("amount");
        verify(&job, i, &job.tx, &pre, fixture_keyhash(spk), spk, amount)
    }
    .is_err());

    let job = job_with_witness(&[&[0x01]]);
    let pre = rbitcoin_consensus::TxScriptPrecompute::from_tx(&job.tx);
    assert!({
        let i = job.inputs().next().unwrap();
        let spk = job.prevout_script(i).expect("spk");
        let amount = job.prevout_amount(i).expect("amount");
        verify(&job, i, &job.tx, &pre, fixture_keyhash(spk), spk, amount)
    }
    .is_err());

    let job = job_with_witness(&[&[], &[0x02; 33]]);
    let pre = rbitcoin_consensus::TxScriptPrecompute::from_tx(&job.tx);
    assert!({
        let i = job.inputs().next().unwrap();
        let spk = job.prevout_script(i).expect("spk");
        let amount = job.prevout_amount(i).expect("amount");
        verify(&job, i, &job.tx, &pre, fixture_keyhash(spk), spk, amount)
    }
    .is_err());

    let job = job_with_witness(&[&[0x30, 0x01, 0x01, 0x01], &[0x02; 33]]);
    let pre = rbitcoin_consensus::TxScriptPrecompute::from_tx(&job.tx);
    let err = {
        let i = job.inputs().next().unwrap();
        let spk = job.prevout_script(i).expect("spk");
        let amount = job.prevout_amount(i).expect("amount");
        verify(&job, i, &job.tx, &pre, fixture_keyhash(spk), spk, amount)
    }
    .unwrap_err();
    assert!(format!("{err}").contains("p2wpkh"));

    let redeem = {
        let mut r = vec![0x00, 0x14];
        r.extend([0u8; 20]);
        r
    };
    let job = job_with_witness(&[&[0x01]]);
    let pre = rbitcoin_consensus::TxScriptPrecompute::from_tx(&job.tx);
    assert!({
        let i = job.inputs().next().unwrap();
        let amount = job.prevout_amount(i).expect("amount");
        verify_with_keyhash(&job, i, &job.tx, &[0u8; 20], &redeem, &pre, amount)
    }
    .is_err());
}

#[test]
fn witness_element_at_the_520_byte_cap_is_not_push_size() {
    let cap = vec![0x02u8; 520];
    let over = vec![0x02u8; 521];
    assert!(check_element_sizes(&cap, &[0x02; 33]).is_ok());
    let sig_over = check_element_sizes(&over, &[0x02; 33]).unwrap_err();
    assert!(format!("{sig_over}").contains("PUSH_SIZE"), "{sig_over}");
    assert!(check_element_sizes(&[0x30, 0x01], &cap).is_ok());
    let pk_over = check_element_sizes(&[0x30, 0x01], &over).unwrap_err();
    assert!(format!("{pk_over}").contains("PUSH_SIZE"), "{pk_over}");
}
