use rbitcoin_consensus::{ChainParams, TxTweak};
use rbitcoin_electrum::tweaks::*;
use rbitcoin_query::{Query, ThinTweakRangeLimits, ThinTweakRow};
use serde_json::{json, Value};

use rbitcoin_query::testutil::FixtureChain;

#[test]
fn subscribe_serve_limits_matches_query_default() {
    assert_eq!(
        subscribe_serve_limits(true),
        ThinTweakRangeLimits::default()
    );
    assert_eq!(subscribe_serve_limits(true).max_eligible, 16384);
    assert_eq!(subscribe_serve_limits(true).max_heights, 128);
    assert!(!subscribe_serve_limits(true).cut_through);
    assert!(subscribe_serve_limits(false).cut_through);
}

#[test]
fn parse_req_historical_defaults_false() {
    let r = parse_req(&json!([10, 3])).unwrap();
    assert_eq!(
        r,
        TweakReq {
            start: 10,
            count: 3,
            historical: false
        }
    );
    let h = parse_req(&json!([10, 3, true])).unwrap();
    assert!(h.historical);
    let c = parse_req(&json!([0, 1, false])).unwrap();
    assert!(!c.historical);
}

#[test]
fn last_height_stops_at_tip() {
    assert_eq!(last_height(880_791, 81_427, Some(962_217)), Some(962_217));
    assert_eq!(last_height(10, 3, Some(11)), Some(11));
    assert_eq!(last_height(10, 1, Some(100)), Some(10));
    assert_eq!(last_height(10, 1, Some(10)), Some(10));
    assert_eq!(last_height(50, 1, Some(10)), None);
    assert_eq!(last_height(0, 1, None), None);
}

#[test]
fn pre_taproot_wave_covers_1024_then_stops_at_activation() {
    assert_eq!(pre_taproot_wave_last(0, 10, 709_632), Some(10));
    assert_eq!(pre_taproot_wave_last(0, 2_000, 709_632), Some(1023));
    assert_eq!(
        pre_taproot_wave_last(709_000, 800_000, 709_632),
        Some(709_631)
    );
    assert_eq!(pre_taproot_wave_last(709_632, 800_000, 709_632), None);
    assert_eq!(pre_taproot_wave_last(0, 100, 0), None);
}

#[test]
fn first_subscribe_wave_indexed_shares_batch_and_pre_taproot_is_empty() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::{Query, TxApply};
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-electrum-first-wave-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(&dir).unwrap();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..4u32 {
        let mut merkle = [0u8; 32];
        merkle[0..4].copy_from_slice(&h.to_le_bytes());
        merkle[5] = 0xec;
        let hash = match parent_hash {
            None => merkle,
            Some(ph) => rbitcoin_store::block_header_hash(1, &ph, &merkle, h + 1, 0x207fffff, h),
        };
        let header = HeaderRecord {
            prev_fk: prev,
            version: 1,
            timestamp: h + 1,
            bits: 0x207fffff,
            nonce: h,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        };
        let mut txid = [0u8; 32];
        txid[0..4].copy_from_slice(&h.to_le_bytes());
        txid[31] = 0xcb;
        let ta = TxApply {
            tx: TxRecord {
                txid,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord::coinbase(u32::MAX, vec![h as u8], vec![])],
            outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
        };
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
        parent_hash = Some(hash);
        q.put_sp_tweaks_block(Height(h), prev, &[None]).unwrap();
    }

    let chain = ChainParams::regtest();
    let wave = first_subscribe_wave(
        &q,
        &chain,
        1,
        3,
        ThinTweakRangeLimits::default(),
        DEFAULT_TWEAKS_MIN_DUST,
    )
    .unwrap();
    let result: Value = serde_json::from_str(&wave.result_json).unwrap();
    assert_eq!(result.as_object().unwrap().len(), 1);
    assert!(result.get("1").unwrap().as_object().unwrap().is_empty());
    assert_eq!(wave.consumed, 3);
    assert_eq!(wave.rest_notifies.len(), 2);
    for (i, line) in wave.rest_notifies.iter().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap();
        let h = (i + 2).to_string();
        assert_eq!(v["method"], "blockchain.tweaks.subscribe");
        assert!(v["params"][0][&h].as_object().unwrap().is_empty(), "{v}");
    }
    // One indexed height, including a start that is also the last.
    let same = load_thin_batch(&q, 2, 2, ThinTweakRangeLimits::default()).unwrap();
    assert_eq!(same.len(), 1);
    assert_eq!(same[0].0, 2);
    let thin1 = q.load_thin_tweaks(Height(1)).unwrap().expect("indexed");
    assert_eq!(
        encode_thin_height_json(1, &thin1, DEFAULT_TWEAKS_MIN_DUST),
        wave.result_json
    );

    let main = ChainParams::mainnet();
    let pre = first_subscribe_wave(
        &q,
        &main,
        0,
        10,
        ThinTweakRangeLimits::default(),
        DEFAULT_TWEAKS_MIN_DUST,
    )
    .unwrap();
    assert_eq!(pre.consumed, 1);
    assert!(pre.rest_notifies.is_empty());
    let pre_v: Value = serde_json::from_str(&pre.result_json).unwrap();
    assert!(pre_v["0"].as_object().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remaining_lines_pre_taproot_are_one_empty_wave() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::{Query, TxApply};
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-electrum-empty-wave-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(&dir).unwrap();
    q.set_sptweaks_enabled(true, Height(10_000)).unwrap();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..8u32 {
        let mut merkle = [0u8; 32];
        merkle[0..4].copy_from_slice(&h.to_le_bytes());
        merkle[5] = 0xec;
        let hash = match parent_hash {
            None => merkle,
            Some(ph) => rbitcoin_store::block_header_hash(1, &ph, &merkle, h + 1, 0x207fffff, h),
        };
        let header = HeaderRecord {
            prev_fk: prev,
            version: 1,
            timestamp: h + 1,
            bits: 0x207fffff,
            nonce: h,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        };
        let mut txid = [0u8; 32];
        txid[0..4].copy_from_slice(&h.to_le_bytes());
        txid[31] = 0xcb;
        let ta = TxApply {
            tx: TxRecord {
                txid,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord::coinbase(u32::MAX, vec![h as u8], vec![])],
            outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
        };
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
        parent_hash = Some(hash);
    }
    let chain = ChainParams::mainnet();
    let wave = remaining_notify_lines(
        &q,
        &chain,
        1,
        7,
        ThinTweakRangeLimits::default(),
        DEFAULT_TWEAKS_MIN_DUST,
    )
    .unwrap();
    assert_eq!(wave.consumed, 7);
    assert_eq!(wave.lines.len(), 1);
    let v: Value = serde_json::from_str(&wave.lines[0]).unwrap();
    assert_eq!(v["method"], "blockchain.tweaks.subscribe");
    let map = v["params"][0].as_object().expect("collapsed empty map");
    assert_eq!(map.len(), 7, "{v}");
    for h in 1u32..=7 {
        assert!(
            map.get(&h.to_string())
                .and_then(|x| x.as_object())
                .is_some_and(|o| o.is_empty()),
            "{v}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn height_map_cut_through_omits_spent_p2tr() {
    use bitcoin::hashes::{hash160, Hash};
    use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-electrum-cut-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(&dir).unwrap();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[2u8; 32]).unwrap();
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let ser = pk.serialize();
    let h160 = hash160::Hash::hash(&ser);
    let mut p2wpkh = vec![0x00, 0x14];
    p2wpkh.extend_from_slice(h160.as_ref());
    let (xonly, _) = pk.x_only_public_key();
    let mut p2tr = vec![0x51, 0x20];
    p2tr.extend_from_slice(&xonly.serialize());
    let mut genesis_txid = [0u8; 32];
    genesis_txid[31] = 0xcb;
    let mut merkle0 = [0u8; 32];
    merkle0[5] = 0xec;
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
    let fk0 = q
        .connect_block(
            Height(0),
            &h0,
            &[TxApply {
                tx: TxRecord {
                    txid: genesis_txid,
                    version: 1,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                outputs: vec![OutputRecord::unspent(50_0000_0000, p2wpkh.clone())],
            }],
        )
        .unwrap();
    q.put_sp_tweaks_block(Height(0), fk0, &[None]).unwrap();
    let create0 = q.block_tx_fks(Height(0)).unwrap()[0];
    let mut spend_txid = [0u8; 32];
    spend_txid[0] = 0x11;
    spend_txid[31] = 0xcd;
    let hash1 = rbitcoin_store::block_header_hash(1, &h0.hash, &[0x11; 32], 2, 0x207fffff, 1);
    let h1 = HeaderRecord {
        prev_fk: fk0,
        version: 1,
        timestamp: 2,
        bits: 0x207fffff,
        nonce: 1,
        merkle_root: [0x11; 32],
        hash: hash1,
        size: 0,
        weight: 0,
    };
    let fk1 = q
        .connect_block(
            Height(1),
            &h1,
            &[TxApply {
                tx: TxRecord {
                    txid: spend_txid,
                    version: 2,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord {
                    prev_txid: genesis_txid,
                    parent: rbitcoin_store::InputParent::Prevout {
                        create_fk: create0,
                        vout: 0,
                    },
                    sequence: u32::MAX,
                    script_sig: vec![],
                    witness: vec![vec![0u8; 64], ser.to_vec()],
                }],
                outputs: vec![OutputRecord::unspent(49_0000_0000, p2tr)],
            }],
        )
        .unwrap();
    let mut tw = [0x02; 33];
    tw[0] = 0x02;
    q.put_sp_tweaks_block(Height(1), fk1, &[Some(tw)]).unwrap();
    let chain = ChainParams::regtest();
    let live = height_map_json(&q, &chain, 1, true, DEFAULT_TWEAKS_MIN_DUST).unwrap();
    let live_v: Value = serde_json::from_str(&live).unwrap();
    assert_eq!(live_v["1"].as_object().unwrap().len(), 1);
    let stale =
        height_map_json_with_tip(&q, &chain, 1, Some(0), false, DEFAULT_TWEAKS_MIN_DUST).unwrap();
    assert_eq!(stale, empty_height_json(1));

    let elig_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    let mut spend2 = [0u8; 32];
    spend2[0] = 0x22;
    let hash2 = rbitcoin_store::block_header_hash(1, &h1.hash, &[0x22; 32], 3, 0x207fffff, 2);
    let h2 = HeaderRecord {
        prev_fk: fk1,
        version: 1,
        timestamp: 3,
        bits: 0x207fffff,
        nonce: 2,
        merkle_root: [0x22; 32],
        hash: hash2,
        size: 0,
        weight: 0,
    };
    q.connect_block(
        Height(2),
        &h2,
        &[TxApply {
            tx: TxRecord {
                txid: spend2,
                version: 2,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord {
                prev_txid: spend_txid,
                parent: rbitcoin_store::InputParent::Prevout {
                    create_fk: elig_fk,
                    vout: 0,
                },
                sequence: u32::MAX,
                script_sig: vec![],
                witness: vec![vec![0u8; 64]],
            }],
            outputs: vec![OutputRecord::unspent(48_0000_0000, p2wpkh)],
        }],
    )
    .unwrap();

    let cut = height_map_json(&q, &chain, 1, true, DEFAULT_TWEAKS_MIN_DUST).unwrap();
    let cut_v: Value = serde_json::from_str(&cut).unwrap();
    assert!(
        cut_v["1"].as_object().unwrap().is_empty(),
        "cut-through must omit spent p2tr, got {cut}"
    );
    let hist = height_map_json(&q, &chain, 1, false, 0).unwrap();
    let hist_v: Value = serde_json::from_str(&hist).unwrap();
    assert_eq!(hist_v["1"].as_object().unwrap().len(), 1, "{hist}");

    let cut_sub = subscribe(&q, &json!([1, 1, false]), &chain, 0).unwrap();
    assert!(
        cut_sub["1"].as_object().unwrap().is_empty(),
        "subscribe cut-through omits the spent output: {cut_sub}"
    );
    let hist_sub = subscribe(&q, &json!([1, 1, true]), &chain, 0).unwrap();
    assert_eq!(hist_sub["1"].as_object().unwrap().len(), 1, "{hist_sub}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn done_notify_is_cake_message() {
    let v = done_notify();
    assert_eq!(v["method"], "blockchain.tweaks.subscribe");
    assert_eq!(v["params"][0]["message"], "done");
}

#[test]
fn seal_subscribe_chunk_only_when_more_remain_and_budget_elapsed() {
    use std::time::Duration;
    let min = Duration::from_secs(60);
    assert!(
        !seal_subscribe_chunk(Duration::from_secs(90), min, false),
        "natural end of count/tip is not a time seal"
    );
    assert!(
        !seal_subscribe_chunk(Duration::from_secs(59), min, true),
        "under budget keeps the RPC open"
    );
    assert!(
        seal_subscribe_chunk(min, min, true),
        "wave boundary at budget with heights left must done"
    );
    assert_eq!(SUBSCRIBE_CHUNK, Duration::from_secs(60));
    assert!(
        chunk_still_open(Duration::from_secs(59), true),
        "under 60s with heights left keeps the stream open"
    );
    assert!(
        !chunk_still_open(SUBSCRIBE_CHUNK, true),
        "60s with heights left seals the chunk"
    );
    assert!(!chunk_still_open(SUBSCRIBE_CHUNK, false));
}

#[test]
fn thin_json_matches_value_encoder() {
    let mut tweak = [0u8; 33];
    tweak[0] = 0x02;
    tweak[1] = 0xaa;
    let mut txid = [0u8; 32];
    txid[0] = 0x11;
    let row = rbitcoin_query::ThinTweakRow {
        txid,
        tweak,
        p2tr: vec![(1, [0x5f; 32], 5410)],
    };
    let s = encode_thin_height_json(850_000, &[row], 0);
    let v: Value = serde_json::from_str(&s).unwrap();
    let t = TxTweak {
        tweak,
        output_pubkeys: vec![rbitcoin_consensus::TaprootOut {
            vout: 1,
            xonly: [0x5f; 32],
            value: 5410,
        }],
    };
    let mut map = std::collections::BTreeMap::new();
    map.insert(txid, t);
    let expect = json!({ "850000": height_object(&map) });
    assert_eq!(v, expect);
}

/// Prune cannot walk the stem. An unsealed tip still in the keep window is
/// read from that file. A missing file is an error. A hole below the
/// watermark is an error, not an empty history.
#[test]
fn pruned_height_map_does_not_walk_a_hole() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{InputRecord, OutputRecord, TxRecord};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("el-tweak-hole");
    q.set_sptweaks_enabled(true, Height(5)).unwrap();
    let mut prev = Fk::NULL;
    let mut prev_hash = None;
    let mut at_five = Fk::NULL;
    for h in 0..=6 {
        let header = rbitcoin_query::testutil::header_with_merkle_marker(h, prev, prev_hash, 0xec);
        let script = if h == 5 {
            let mut s = rbitcoin_primitives::P2TR_PREFIX.to_vec();
            s.extend_from_slice(&[0x11; 32]);
            s
        } else if h == 6 {
            let mut s = rbitcoin_primitives::P2TR_PREFIX.to_vec();
            s.extend_from_slice(&[0x66; 32]);
            s
        } else {
            vec![0x51]
        };
        let fk = q
            .connect_block(
                Height(h),
                &header,
                &[TxApply {
                    tx: TxRecord {
                        txid: [h as u8; 32],
                        version: 1,
                        locktime: 0,
                        input_count: 1,
                        output_count: 1,
                    },
                    inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                    outputs: vec![OutputRecord::unspent(50_0000_0000, script)],
                }],
            )
            .unwrap();
        if h == 5 {
            at_five = fk;
        }
        prev = fk;
        prev_hash = Some(header.hash);
    }
    let tweak = [0x02; 33];
    q.put_sp_tweaks_block(Height(5), at_five, &[Some(tweak)])
        .unwrap();
    assert_eq!(q.tweak_index_next(), Some(6));
    q.set_prune_seqsigwit(true).unwrap();
    let chain = ChainParams::regtest();
    let unsealed = height_map_json(&q, &chain, 6, false, 0).unwrap();
    assert_eq!(unsealed, empty_height_json(6), "a coinbase has no tweak");
    std::fs::remove_file(q.store().path().join("seqsigwit.window/6.bin")).unwrap();
    q.clear_seqsigwit_ram_window();
    let missed = height_map_json(&q, &chain, 6, false, 0).unwrap_err();
    assert!(
        missed.contains("window miss"),
        "an unsealed tip has to read the keep file, got {missed}"
    );
    let hole = height_map_json(&q, &chain, 4, false, 0).unwrap_err();
    assert!(hole.contains("does not cover this height"), "{hole}");
    let sealed = height_map_json(&q, &chain, 5, false, 0).unwrap();
    let sealed_v: Value = serde_json::from_str(&sealed).unwrap();
    assert_eq!(sealed_v["5"].as_object().unwrap().len(), 1, "{sealed}");
    q.set_sptweaks_enabled(false, Height(5)).unwrap();
    let off = height_map_json(&q, &chain, 4, false, 0).unwrap_err();
    assert!(off.contains("unavailable with --prune-seqsigwit"), "{off}");
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prune is on and this height is not sealed. A real P2WPKH→P2TR spend
/// still in the keep window has to be in the subscribe result. An empty
/// map would tell the client the height was already scanned.
#[test]
#[allow(clippy::cognitive_complexity)]
fn unsealed_pruned_spend_is_not_an_empty_height() {
    use bitcoin::hashes::{hash160, Hash};
    use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("el-tweak-unsealed");
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[2u8; 32]).unwrap();
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let ser = pk.serialize();
    let mut p2wpkh = vec![0x00, 0x14];
    p2wpkh.extend_from_slice(hash160::Hash::hash(&ser).as_ref());
    let (xonly, _) = pk.x_only_public_key();
    let mut p2tr = vec![0x51, 0x20];
    p2tr.extend_from_slice(&xonly.serialize());
    let mut genesis_txid = [0u8; 32];
    genesis_txid[31] = 0xcb;
    let mut merkle0 = [0u8; 32];
    merkle0[5] = 0xec;
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
    let fk0 = q
        .connect_block(
            Height(0),
            &h0,
            &[TxApply {
                tx: TxRecord {
                    txid: genesis_txid,
                    version: 1,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                outputs: vec![OutputRecord::unspent(50_0000_0000, p2wpkh.clone())],
            }],
        )
        .unwrap();
    q.put_sp_tweaks_block(Height(0), fk0, &[None]).unwrap();
    let create0 = q.block_tx_fks(Height(0)).unwrap()[0];
    let mut spend_txid = [0u8; 32];
    spend_txid[0] = 0x11;
    spend_txid[31] = 0xcd;
    let hash1 = rbitcoin_store::block_header_hash(1, &h0.hash, &[0x11; 32], 2, 0x207fffff, 1);
    let h1 = HeaderRecord {
        prev_fk: fk0,
        version: 1,
        timestamp: 2,
        bits: 0x207fffff,
        nonce: 1,
        merkle_root: [0x11; 32],
        hash: hash1,
        size: 0,
        weight: 0,
    };
    let fk1 = q
        .connect_block(
            Height(1),
            &h1,
            &[TxApply {
                tx: TxRecord {
                    txid: spend_txid,
                    version: 2,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord {
                    prev_txid: genesis_txid,
                    parent: rbitcoin_store::InputParent::Prevout {
                        create_fk: create0,
                        vout: 0,
                    },
                    sequence: u32::MAX,
                    script_sig: vec![],
                    witness: vec![vec![0u8; 64], ser.to_vec()],
                }],
                outputs: vec![OutputRecord::unspent(49_0000_0000, p2tr)],
            }],
        )
        .unwrap();
    assert_eq!(q.tweak_index_next(), Some(1), "height 1 is not sealed");
    q.set_prune_seqsigwit(true).unwrap();
    let chain = ChainParams::regtest();
    let map = height_map_json(&q, &chain, 1, false, 0).unwrap();
    let v: Value = serde_json::from_str(&map).unwrap();
    assert_eq!(
        v["1"].as_object().unwrap().len(),
        1,
        "an unsealed payment must not be an empty height: {map}"
    );
    let cut = height_map_json(&q, &chain, 1, true, 0).unwrap();
    let cut_v: Value = serde_json::from_str(&cut).unwrap();
    assert_eq!(
        cut_v["1"].as_object().unwrap().len(),
        1,
        "the output is unspent, so cut-through keeps it: {cut}"
    );
    let wave = first_subscribe_wave(&q, &chain, 1, 1, subscribe_serve_limits(false), 0).unwrap();
    assert!(
        wave.result_json.contains("tweak"),
        "subscribe must not scan past the payment: {}",
        wave.result_json
    );
    assert_ne!(
        wave.result_json,
        empty_height_json(1),
        "{}",
        wave.result_json
    );

    let elig = q.block_tx_fks(Height(1)).unwrap()[0];
    let mut spend2 = [0u8; 32];
    spend2[0] = 0x22;
    let hash2 = rbitcoin_store::block_header_hash(1, &h1.hash, &[0x22; 32], 3, 0x207fffff, 2);
    let h2 = HeaderRecord {
        prev_fk: fk1,
        version: 1,
        timestamp: 3,
        bits: 0x207fffff,
        nonce: 2,
        merkle_root: [0x22; 32],
        hash: hash2,
        size: 0,
        weight: 0,
    };
    q.connect_block(
        Height(2),
        &h2,
        &[TxApply {
            tx: TxRecord {
                txid: spend2,
                version: 2,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord {
                prev_txid: spend_txid,
                parent: rbitcoin_store::InputParent::Prevout {
                    create_fk: elig,
                    vout: 0,
                },
                sequence: u32::MAX,
                script_sig: vec![],
                witness: vec![vec![0u8; 64]],
            }],
            outputs: vec![OutputRecord::unspent(48_0000_0000, p2wpkh)],
        }],
    )
    .unwrap();
    let spent = height_map_json(&q, &chain, 1, true, 0).unwrap();
    let spent_v: Value = serde_json::from_str(&spent).unwrap();
    assert!(
        spent_v["1"].as_object().unwrap().is_empty(),
        "cut-through drops a spent taproot output: {spent}"
    );
    let kept = height_map_json(&q, &chain, 1, false, 0).unwrap();
    let kept_v: Value = serde_json::from_str(&kept).unwrap();
    assert_eq!(kept_v["1"].as_object().unwrap().len(), 1, "{kept}");
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

fn dust_row(txid0: u8, p2tr: Vec<(u32, [u8; 32], u64)>) -> ThinTweakRow {
    let mut txid = [0u8; 32];
    txid[0] = txid0;
    let mut tweak = [0u8; 33];
    tweak[0] = 0x02;
    ThinTweakRow { txid, tweak, p2tr }
}

#[test]
fn omit_dust_default_drops_leq_1000_keeps_1001_and_empty_txs() {
    let rows = vec![
        dust_row(1, vec![(0, [0x11; 32], 1000), (1, [0x22; 32], 1001)]),
        dust_row(2, vec![(0, [0x33; 32], 546)]),
        dust_row(3, vec![(0, [0x44; 32], 0)]),
    ];
    let s = encode_thin_height_json(1, &rows, DEFAULT_TWEAKS_MIN_DUST);
    let v: Value = serde_json::from_str(&s).unwrap();
    let txs = v["1"].as_object().unwrap();
    assert_eq!(txs.len(), 1, "dust-only txs must leave the height map");
    let txid = rbitcoin_primitives::display_hash_hex(rows[0].txid);
    assert_eq!(txs[&txid]["output_pubkeys"].as_object().unwrap().len(), 1);
    assert_eq!(txs[&txid]["output_pubkeys"]["1"][1], 1001);
}

#[test]
fn omit_dust_546_drops_546_keeps_547() {
    let rows = vec![
        dust_row(1, vec![(0, [0xaa; 32], 546)]),
        dust_row(2, vec![(0, [0xbb; 32], 547)]),
    ];
    let s = encode_thin_height_json(1, &rows, 546);
    let v: Value = serde_json::from_str(&s).unwrap();
    let txs = v["1"].as_object().unwrap();
    assert_eq!(txs.len(), 1);
    let txid = rbitcoin_primitives::display_hash_hex(rows[1].txid);
    assert_eq!(txs[&txid]["output_pubkeys"]["0"][1], 547);
}

#[test]
fn omit_dust_zero_serves_every_value_including_zero() {
    let rows = vec![dust_row(1, vec![(0, [0x11; 32], 0), (1, [0x22; 32], 1)])];
    let s = encode_thin_height_json(1, &rows, 0);
    let v: Value = serde_json::from_str(&s).unwrap();
    let txs = v["1"].as_object().unwrap();
    assert_eq!(txs.len(), 1);
    let txid = rbitcoin_primitives::display_hash_hex(rows[0].txid);
    let outs = txs[&txid]["output_pubkeys"].as_object().unwrap();
    assert_eq!(outs.len(), 2);
    assert_eq!(outs["0"][1], 0);
    assert_eq!(outs["1"][1], 1);
}

#[test]
fn encode_after_default_dust_omits_dust_tx_keeps_height_key() {
    let rows = vec![
        dust_row(1, vec![(0, [0x11; 32], 1000)]),
        dust_row(2, vec![(1, [0x22; 32], 5410)]),
    ];
    let s = encode_thin_height_json(850_000, &rows, DEFAULT_TWEAKS_MIN_DUST);
    let v: Value = serde_json::from_str(&s).unwrap();
    let txs = v["850000"].as_object().unwrap();
    assert_eq!(txs.len(), 1, "height key stays; dust-only tx is gone");
    let txid = rbitcoin_primitives::display_hash_hex(rows[1].txid);
    assert!(txs.contains_key(&txid));
    assert_eq!(txs[&txid]["output_pubkeys"]["1"][1], 5410);
}
