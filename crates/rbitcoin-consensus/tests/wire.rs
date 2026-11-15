use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::script::ScriptBuf;
use bitcoin::{Amount, OutPoint, Sequence, Transaction, TxIn, TxOut, Witness};

fn tx(coinbase: bool, script_sig: Vec<u8>) -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: if coinbase {
                OutPoint::null()
            } else {
                OutPoint {
                    txid: bitcoin::Txid::from_byte_array([2; 32]),
                    vout: 0,
                }
            },
            script_sig: ScriptBuf::from_bytes(script_sig),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

#[test]
fn coinbase_script_sig_is_not_a_script_parse() {
    // Truncated PUSHDATA1. A non-coinbase input fails; a coinbase input is exempt.
    let bad = vec![0x4c];
    assert!(!rbitcoin_consensus::wire::scripts_parse(&tx(
        false,
        bad.clone()
    )));
    assert!(rbitcoin_consensus::wire::scripts_parse(&tx(true, bad)));
}
