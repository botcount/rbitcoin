use bitcoin::Network;
use rbitcoin_esplora::tx_json::*;
use rbitcoin_primitives::{Fk, Height};
use rbitcoin_query::Query;

use rbitcoin_query::testutil::FixtureChain;
#[test]
fn last_push_data_direct_and_pushdata() {
    assert!(last_push_data(&[0x51]).is_none());
    assert_eq!(last_push_data(&[0x02, 0xaa, 0xbb]), Some(&[0xaa, 0xbb][..]));
    assert!(last_push_data(&[0x03, 0xaa]).is_none());
    assert_eq!(
        last_push_data(&[0x4c, 0x02, 0x11, 0x22]),
        Some(&[0x11, 0x22][..])
    );
    assert!(last_push_data(&[0x4c]).is_none()); // missing length
    assert!(last_push_data(&[0x4c, 0x05, 0x01]).is_none()); // truncated body
                                                            // OP_PUSHDATA2
    assert_eq!(
        last_push_data(&[0x4d, 0x02, 0x00, 0x33, 0x44]),
        Some(&[0x33, 0x44][..])
    );
    assert!(last_push_data(&[0x4d, 0x01]).is_none()); // short len field
    assert!(last_push_data(&[0x4d, 0x03, 0x00, 0x01]).is_none()); // short body
                                                                  // Non-push after push clears last.
    assert!(last_push_data(&[0x01, 0xaa, 0x51]).is_none());
    assert_eq!(last_push_data(&[0x00, 0x01, 0xee]), Some(&[0xee][..]));
    // `last_push_data`: direct `n == len`, PUSHDATA1 of one byte, the cursor after that push, PUSHDATA2 of length 0.
    assert!(last_push_data(&[0x02, 0xaa]).is_none());
    assert_eq!(last_push_data(&[0x4c, 0x01]), None);
    assert_eq!(last_push_data(&[0x4c, 0x01, 0x11]), Some(&[0x11][..]));
    assert_eq!(
        last_push_data(&[0x4c, 0x03, 0xaa, 0xbb, 0xcc, 0x01, 0xdd]),
        Some(&[0xdd][..])
    );
    assert_eq!(last_push_data(&[0x4d, 0x00, 0x00]), Some(&[][..]));
    assert_eq!(
        last_push_data(&[0x4d, 0x03, 0x00, 0xaa, 0xbb, 0xcc, 0x01, 0xdd]),
        Some(&[0xdd][..])
    );
}

#[test]
fn vin_inner_scripts_follow_prevout_type() {
    use bitcoin::hashes::Hash;
    use bitcoin::{
        absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Witness,
    };
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::testutil::FixtureChain;
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let p2pkh = ScriptBuf::from_bytes(vec![
        0x76, 0xa9, 0x14, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x88, 0xac,
    ]);
    let p2sh = ScriptBuf::from_bytes(vec![
        0xa9, 0x14, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
        0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x87,
    ]);
    let p2wpkh = ScriptBuf::from_bytes({
        let mut v = vec![0x00, 0x14];
        v.extend_from_slice(&[0x33; 20]);
        v
    });
    let witness_script = ScriptBuf::from_bytes(vec![0x51]);
    let p2wsh = ScriptBuf::new_p2wsh(&witness_script.wscript_hash());
    let p2tr =
        ScriptBuf::new_p2tr_tweaked(bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(
            bitcoin::XOnlyPublicKey::from_slice(&[0x44; 32]).unwrap(),
        ));
    let spks = [p2pkh, p2sh, p2wpkh, p2wsh, p2tr];

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("inner-scripts");
    let mut txid = [0u8; 32];
    txid[31] = 0xcb;
    let parent = TxApply {
        tx: TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: spks.len() as u32,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![0x01],
            witness: vec![],
        }],
        outputs: spks
            .iter()
            .map(|s| OutputRecord::unspent(10_000, s.to_bytes()))
            .collect(),
    };
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: [1u8; 32],
        hash: [1u8; 32],
        size: 0,
        weight: 0,
    };
    q.connect_block(Height(0), &header, &[parent]).unwrap();

    let prev = bitcoin::Txid::from_byte_array(txid);
    let mut child = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![
            TxIn {
                previous_output: OutPoint {
                    txid: prev,
                    vout: 0,
                },
                script_sig: ScriptBuf::from_bytes(vec![0x01, 0x51]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: prev,
                    vout: 1,
                },
                script_sig: ScriptBuf::from_bytes(vec![0x01, 0x51]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: prev,
                    vout: 2,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[&[0x30, 0x01][..], &[0x02, 0x55][..]]),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: prev,
                    vout: 3,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[&[0x00][..], witness_script.as_bytes()]),
            },
            TxIn {
                previous_output: OutPoint {
                    txid: prev,
                    vout: 4,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[
                    &[0x01][..],
                    &[0x51][..],
                    &{
                        let mut c = vec![0xc0];
                        c.extend_from_slice(&[0x44; 32]);
                        c
                    }[..],
                ]),
            },
        ],
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let v = build_tx_json_from_tx(&q, &child, Network::Regtest, None, None).unwrap();
    let vin = v["vin"].as_array().unwrap();
    assert!(vin[0].get("witness").is_none(), "p2pkh {}", vin[0]);
    assert!(vin[0].get("inner_redeemscript_asm").is_none(), "{}", vin[0]);
    assert!(
        vin[0].get("inner_witnessscript_asm").is_none(),
        "{}",
        vin[0]
    );
    assert!(
        vin[1]["inner_redeemscript_asm"]
            .as_str()
            .unwrap()
            .contains("OP_1")
            || vin[1]["inner_redeemscript_asm"]
                .as_str()
                .unwrap()
                .contains('1'),
        "p2sh {}",
        vin[1]
    );
    assert!(
        vin[1].get("inner_witnessscript_asm").is_none(),
        "{}",
        vin[1]
    );
    assert!(vin[2].get("witness").is_some(), "{}", vin[2]);
    assert!(
        vin[2].get("inner_witnessscript_asm").is_none(),
        "p2wpkh {}",
        vin[2]
    );
    let wsh = vin[3]["inner_witnessscript_asm"].as_str().unwrap();
    assert!(wsh.contains("OP_1") || wsh.contains('1'), "p2wsh {wsh}");
    let tr = vin[4]["inner_witnessscript_asm"].as_str().unwrap();
    assert!(
        tr.contains("OP_PUSHNUM_1") || tr == "OP_1",
        "tapscript {tr}"
    );
    assert!(
        !tr.contains("OP_UNKNOWN"),
        "control block must not be the leaf: {tr}"
    );

    child.input[4].witness = Witness::from_slice(&[&[0x22; 64][..]]);
    let keypath = build_tx_json_from_tx(&q, &child, Network::Regtest, None, None).unwrap();
    assert!(
        keypath["vin"][4].get("inner_witnessscript_asm").is_none(),
        "key path {}",
        keypath["vin"][4]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn script_asm_drops_empty_and_past_10k() {
    // `script_asm`: empty, length 10_000, length 10_001. Inner scripts call this.
    assert!(script_asm(&[]).is_none());
    assert!(script_asm(&vec![0x51; 10_000]).is_some());
    assert!(script_asm(&vec![0x51; 10_001]).is_none());
}

#[test]
fn tapscript_leaf_is_the_script_before_the_control() {
    // `tapscript_leaf`: annex, a two-item witness, and a five-item witness. Vin JSON calls this.
    let leaf = vec![0x51];
    let control = {
        let mut c = vec![0xc0];
        c.extend_from_slice(&[0x44; 32]);
        c
    };
    assert!(tapscript_leaf(&[]).is_none());
    assert_eq!(
        tapscript_leaf(&[leaf.clone(), control.clone()]),
        Some(leaf.as_slice())
    );
    assert_eq!(
        tapscript_leaf(&[leaf.clone(), control.clone(), vec![0x50, 0x01]]),
        Some(leaf.as_slice())
    );
    let wide = [
        vec![0x01],
        vec![0x02],
        vec![0x03],
        leaf.clone(),
        control.clone(),
    ];
    assert_eq!(tapscript_leaf(&wide), Some(leaf.as_slice()));
    let plain = [vec![0x01], leaf.clone(), control];
    assert_eq!(tapscript_leaf(&plain), Some(leaf.as_slice()));
}

#[test]
fn prevout_json_uses_the_stored_create() {
    use bitcoin::hashes::Hash;
    use bitcoin::{OutPoint, ScriptBuf, Sequence, TxIn, Txid, Witness};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("esplora-prevout-fk");
    let mut txid = [0u8; 32];
    txid[31] = 0xcb;
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: [1u8; 32],
        hash: [1u8; 32],
        size: 0,
        weight: 0,
    };
    let parent = TxApply {
        tx: TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(u32::MAX, vec![0], vec![])],
        outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
    };
    q.connect_block(Height(0), &header, &[parent]).unwrap();
    let fk = q.tx_fk_by_txid(&txid).unwrap().unwrap();
    let tin = TxIn {
        previous_output: OutPoint {
            txid: Txid::from_byte_array([0x99; 32]),
            vout: 0,
        },
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::new(),
    };
    let stored = rbitcoin_store::InputParent::Prevout {
        create_fk: fk,
        vout: 0,
    };
    let (prev, _value, _spk) = prevout_json(&q, &[stored], 0, &tin, Network::Regtest, None)
        .unwrap()
        .expect("stored create");
    assert_eq!(prev["value"], serde_json::json!(50_0000_0000u64));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn block_hash_hex_reverses_bytes() {
    let mut h = [0u8; 32];
    h[0] = 0xab;
    h[31] = 0xcd;
    let s = block_hash_hex(&h);
    assert_eq!(s.len(), 64);
    assert!(s.starts_with("cd"));
    assert!(s.ends_with("ab"));
}

#[test]
fn utxo_list_json_status_from_join_height_without_tx_head() {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{script_hash, HeaderRecord, InputRecord, OutputRecord, TxRecord};
    use std::time::{SystemTime, UNIX_EPOCH};

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-esplora-utxo-json-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(dir.join("store")).unwrap();

    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..2u32 {
        let version = 1;
        let timestamp = h + 1;
        let bits = 0x207fffff;
        let nonce = h;
        let mut merkle = [0u8; 32];
        merkle[0..4].copy_from_slice(&h.to_le_bytes());
        merkle[5] = 0xab;
        let hash = match parent_hash {
            None => merkle,
            Some(ph) => {
                rbitcoin_store::block_header_hash(version, &ph, &merkle, timestamp, bits, nonce)
            }
        };
        let header = HeaderRecord {
            prev_fk: prev,
            version,
            timestamp,
            bits,
            nonce,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        };
        let mut txid = [0u8; 32];
        txid[0..4].copy_from_slice(&h.to_le_bytes());
        txid[31] = 0xcb;
        let outputs = if h == 1 {
            vec![
                OutputRecord::unspent(50_0000_0000, vec![0x51]),
                OutputRecord::unspent(1_0000_0000, vec![0x51]),
            ]
        } else {
            vec![OutputRecord::unspent(50_0000_0000, vec![0x51])]
        };
        let ta = TxApply {
            tx: TxRecord {
                txid,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: outputs.len() as u32,
            },
            inputs: vec![InputRecord {
                prev_txid: [0u8; 32],
                parent: rbitcoin_store::InputParent::Coinbase,
                sequence: u32::MAX,
                script_sig: vec![h as u8],
                witness: vec![],
            }],
            outputs,
        };
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
        parent_hash = Some(hash);
    }

    let sh = script_hash(&[0x51]);
    let list = q.scripthash_listunspent(&sh).unwrap();
    assert_eq!(list.len(), 3);
    let at0 = list.iter().filter(|u| u.height == 0).count();
    let at1 = list.iter().filter(|u| u.height == 1).count();
    assert_eq!(at0, 1);
    assert_eq!(at1, 2);

    let arr = utxo_list_json(&q, &list).unwrap();
    let rows = arr.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for u in &list {
        let row = rows
            .iter()
            .find(|r| {
                r["vout"] == u.tx_pos
                    && r["status"]["block_height"].as_u64() == Some(u.height as u64)
                    && r["value"] == u.value
            })
            .expect("row");
        let (_fk, rec) = q
            .header_at_height(Height(u.height as u32))
            .unwrap()
            .unwrap();
        assert_eq!(row["status"]["confirmed"], true);
        assert_eq!(row["status"]["block_hash"], block_hash_hex(&rec.hash));
        assert_eq!(row["status"]["block_time"], rec.timestamp);
        assert_eq!(row["txid"], block_hash_hex(&u.tx_hash));
    }

    // Status comes from join height, not tx.head: a txid that is not in the
    // store still gets block_hash / block_time from header_at_height.
    let orphan = rbitcoin_query::ScriptHashUtxo {
        tx_hash: [0xee; 32],
        tx_pos: 7,
        height: 1,
        value: 42,
        create_tx_fk: Fk(1),
    };
    let miss = utxo_list_json(&q, &[orphan]).unwrap();
    let row = &miss.as_array().unwrap()[0];
    let (_fk, rec1) = q.header_at_height(Height(1)).unwrap().unwrap();
    assert_eq!(row["status"]["confirmed"], true);
    assert_eq!(row["status"]["block_height"], 1);
    assert_eq!(row["status"]["block_hash"], block_hash_hex(&rec1.hash));
    assert_eq!(row["status"]["block_time"], rec1.timestamp);
    assert_eq!(row["txid"], block_hash_hex(&[0xee; 32]));
    assert_eq!(row["vout"], 7);

    let mempool_row = rbitcoin_query::ScriptHashUtxo {
        tx_hash: [0x11; 32],
        tx_pos: 0,
        height: 0,
        value: 99,
        create_tx_fk: Fk::NULL,
    };
    let mem = utxo_list_json(&q, &[mempool_row]).unwrap();
    let mrow = &mem.as_array().unwrap()[0];
    assert_eq!(mrow["status"]["confirmed"], false);
    assert!(mrow["status"].get("block_height").is_none());
    assert!(mrow["status"].get("block_hash").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_tx_json_prevout_is_outs_only_and_txid_from_body() {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};
    use std::time::{SystemTime, UNIX_EPOCH};

    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-esplora-prevout-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(dir.join("store")).unwrap();

    let mut merkle0 = [0u8; 32];
    merkle0[0] = 0xaa;
    let h0 = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: merkle0,
        hash: merkle0,
        size: 0,
        weight: 0,
    };
    let mut create_txid = [0u8; 32];
    create_txid[31] = 0xcb;
    let ta0 = TxApply {
        tx: TxRecord {
            txid: create_txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![0],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
    };
    let hfk0 = q.connect_block(Height(0), &h0, &[ta0]).unwrap();
    let create_fk = q.block_tx_fks(Height(0)).unwrap()[0];

    let hash1 = rbitcoin_store::block_header_hash(1, &h0.hash, &[0x11; 32], 2, 0x207fffff, 1);
    let h1 = HeaderRecord {
        prev_fk: hfk0,
        version: 1,
        timestamp: 2,
        bits: 0x207fffff,
        nonce: 1,
        merkle_root: [0x11; 32],
        hash: hash1,
        size: 0,
        weight: 0,
    };
    let mut spend1_txid = [0u8; 32];
    spend1_txid[0] = 0x11;
    spend1_txid[31] = 0xcd;
    let ta1 = TxApply {
        tx: TxRecord {
            txid: spend1_txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: create_txid,
            parent: rbitcoin_store::InputParent::Prevout { create_fk, vout: 0 },
            sequence: u32::MAX,
            script_sig: vec![],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(49_0000_0000, vec![0x51])],
    };
    let hfk1 = q.connect_block(Height(1), &h1, &[ta1]).unwrap();
    let spend1_fk = q.block_tx_fks(Height(1)).unwrap()[0];

    let hash2 = rbitcoin_store::block_header_hash(1, &h1.hash, &[0x22; 32], 3, 0x207fffff, 2);
    let h2 = HeaderRecord {
        prev_fk: hfk1,
        version: 1,
        timestamp: 3,
        bits: 0x207fffff,
        nonce: 2,
        merkle_root: [0x22; 32],
        hash: hash2,
        size: 0,
        weight: 0,
    };
    let mut spend2_txid = [0u8; 32];
    spend2_txid[0] = 0x22;
    spend2_txid[31] = 0xce;
    let ta2 = TxApply {
        tx: TxRecord {
            txid: spend2_txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: spend1_txid,
            parent: rbitcoin_store::InputParent::Prevout {
                create_fk: spend1_fk,
                vout: 0,
            },
            sequence: u32::MAX,
            script_sig: vec![],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(48_0000_0000, vec![0xac])],
    };
    q.connect_block(Height(2), &h2, &[ta2]).unwrap();
    let spend2_fk = q.block_tx_fks(Height(2)).unwrap()[0];

    let v = build_tx_json(&q, spend2_fk, Network::Regtest, None).unwrap();
    assert_eq!(v["txid"], block_hash_hex(&spend2_txid));
    assert_eq!(v["vin"][0]["prevout"]["value"].as_i64(), Some(49_0000_0000));
    assert_eq!(v["sigops"], 4, "OP_CHECKSIG output scaled: {v}");
    assert_eq!(q.store().txs.body_txid(spend2_fk).unwrap(), spend2_txid);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pruned_tx_json_has_fee_from_txstat() {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord, TxStatRow};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("esplora-pruned-txstat");
    let mut merkle = [0u8; 32];
    merkle[0] = 0xaa;
    let h0 = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: merkle,
        hash: merkle,
        size: 0,
        weight: 0,
    };
    let mut txid = [0u8; 32];
    txid[31] = 0xcb;
    let ta0 = TxApply {
        tx: TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(u32::MAX, vec![0], vec![])],
        outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
    };
    let hfk0 = q.connect_block(Height(0), &h0, &[ta0]).unwrap();
    let fk = q.block_tx_fks(Height(0)).unwrap()[0];
    q.store()
        .write_txstat_row(
            fk,
            &TxStatRow {
                fee_sat: 0,
                base: 81,
                wit_extra: 0,
            },
        )
        .unwrap();
    let mut spend_txid = [0u8; 32];
    spend_txid[31] = 0xee;
    let h1_hash = rbitcoin_store::block_header_hash(1, &merkle, &spend_txid, 2, 0x207fffff, 0);
    let h1 = HeaderRecord {
        prev_fk: hfk0,
        version: 1,
        timestamp: 2,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: spend_txid,
        hash: h1_hash,
        size: 0,
        weight: 0,
    };
    let spend = TxApply {
        tx: TxRecord {
            txid: spend_txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: txid,
            parent: rbitcoin_store::InputParent::Prevout {
                create_fk: fk,
                vout: 0,
            },
            sequence: u32::MAX,
            script_sig: vec![0x51],
            witness: vec![vec![0xab]],
        }],
        outputs: vec![OutputRecord::unspent(49_0000_0000, vec![0x51])],
    };
    q.connect_block(Height(1), &h1, &[spend]).unwrap();
    let spend_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    q.set_pruneheight(Some(Height(1))).unwrap();
    let v = build_tx_json(&q, fk, Network::Regtest, None).unwrap();
    assert_eq!(v["pruned"], true);
    assert_eq!(v["vin"][0]["is_coinbase"], true);
    assert!(v["vin"][0].get("witness").is_none(), "{v}");
    assert!(v["vin"][0].get("scriptsig").is_none(), "{v}");
    let sv = build_tx_json(&q, spend_fk, Network::Regtest, None).unwrap();
    assert_eq!(sv["pruned"], true, "{sv}");
    assert_eq!(sv["vin"][0]["txid"], block_hash_hex(&txid));
    assert_eq!(sv["vin"][0]["vout"], 0);
    assert_eq!(sv["vin"][0]["is_coinbase"], false);
    assert!(sv["vin"][0].get("witness").is_none(), "{sv}");
    assert_eq!(sv["vin"][0]["prevout"]["value"], 5_000_000_000i64);
    q.set_pruneheight(Some(Height(0))).unwrap();
    assert_eq!(v["fee"], 0);
    assert_eq!(v["size"], 81);
    assert_eq!(v["weight"], 324);
    assert!(v.get("sigops").is_none(), "pruned JSON omits sigops: {v}");
    assert_eq!(v["vout"].as_array().unwrap().len(), 1);
    assert_eq!(v["txid"], block_hash_hex(&txid));

    q.store()
        .write_txstat_row(
            fk,
            &TxStatRow {
                fee_sat: 0,
                base: 0,
                wit_extra: 0,
            },
        )
        .unwrap();
    let raw = build_tx_json(&q, fk, Network::Regtest, None).unwrap();
    assert_eq!(raw["pruned"], true);
    assert_eq!(raw["vin"][0]["is_coinbase"], true, "{raw}");
    assert!(raw.get("fee").is_none(), "unstamped omits fee: {raw}");
    assert!(raw.get("size").is_none(), "{raw}");
    assert!(raw.get("weight").is_none(), "{raw}");
    assert!(raw.get("sigops").is_none(), "{raw}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prune mode is on and this height is still served. The JSON is the full
/// transaction, not a pruned stub and not a missing witness.
#[test]
fn kept_pruned_tx_json_serves_the_witness() {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("esplora-kept-prune");
    q.set_prune_seqsigwit(true).unwrap();
    let mut merkle = [0u8; 32];
    merkle[0] = 0xab;
    let h0 = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: merkle,
        hash: merkle,
        size: 0,
        weight: 0,
    };
    let mut txid = [0u8; 32];
    txid[31] = 0xcb;
    let ta0 = TxApply {
        tx: TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(
            u32::MAX,
            vec![0x11, 0x22],
            vec![vec![0x33]],
        )],
        outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
    };
    q.connect_block(Height(0), &h0, &[ta0]).unwrap();
    let fk = q.block_tx_fks(Height(0)).unwrap()[0];
    let v = build_tx_json(&q, fk, Network::Regtest, None).unwrap();
    assert!(v.get("pruned").is_none(), "{v}");
    assert_eq!(v["vin"][0]["is_coinbase"], true);
    assert_eq!(v["vin"][0]["scriptsig"], "1122");
    let _ = std::fs::remove_dir_all(&dir);
}
