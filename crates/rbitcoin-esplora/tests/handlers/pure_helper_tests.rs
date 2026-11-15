use rbitcoin_esplora::handlers::{
    block_summary_json, fee_estimates_from, outspend_json, skip_rows_after, sort_txs_newest_first,
    summaries_json, truncate_page,
};
use rbitcoin_primitives::Fk;
use rbitcoin_primitives::Height;
use rbitcoin_query::testutil::FixtureChain;
use rbitcoin_query::Query;
use rbitcoin_query::ScriptHashTxSummary;
use rbitcoin_query::TxApply;
use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};
use serde_json::json;

fn temp_query() -> (rbitcoin_query::testutil::TempDir, Query) {
    rbitcoin_query::testutil::tiny_query_labeled("esplora-pure")
}

fn seed_genesis(q: &Query) -> [u8; 32] {
    let merkle = [0xab; 32];
    let hash = rbitcoin_store::block_header_hash(1, &[0u8; 32], &merkle, 1, 0x207f_ffff, 0);
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207f_ffff,
        nonce: 0,
        merkle_root: merkle,
        hash,
        size: 0,
        weight: 0,
    };
    let ta = TxApply {
        tx: TxRecord {
            txid: [0xcb; 32],
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![0x00],
            witness: vec![vec![0u8; 32]],
        }],
        outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
    };
    let _ = q.connect_block(Height(0), &header, &[ta]).unwrap();
    hash
}

#[test]
fn fee_routes_keep_the_estimator_resolution() {
    // BTC/kvB as the estimator publishes it: whole sat/kvB
    let pairs = [
        (1, 0.000_012_34),
        (3, 0.000_009_87),
        (6, 0.000_003_1),
        (144, 0.000_001_01),
    ];
    let est = fee_estimates_from(&pairs).expect("estimates");
    assert_eq!(est["1"], json!(1.234));
    assert_eq!(est["3"], json!(0.987));
    assert_eq!(est["144"], json!(0.101));
}

#[test]
fn block_summary_bits_u32_and_witness_size_weight() {
    let (dir, q) = temp_query();
    let hash = seed_genesis(&q);
    let summary = block_summary_json(&q, &hash).expect("summary");
    assert!(
        summary["bits"].is_u64(),
        "Esplora bits is u32, not hex: {}",
        summary["bits"]
    );
    assert_eq!(summary["bits"], 0x207f_ffff);
    let block = q.reconstruct_archived_block(&hash).unwrap().unwrap();
    assert_eq!(summary["size"], block.total_size() as u64);
    assert_eq!(summary["weight"], block.weight().to_wu());
    assert_ne!(
        summary["weight"].as_u64().unwrap(),
        summary["size"].as_u64().unwrap().saturating_mul(4),
        "witness block weight is not 4×stripped size"
    );
    let mtp = rbitcoin_query::median_time_past(&q, Height(0)).unwrap();
    assert_eq!(summary["mediantime"], json!(mtp));
    let missing = rbitcoin_esplora::server::store_err(rbitcoin_store::StoreError::NotFound);
    assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn outspend_json_spent_includes_vin() {
    let (dir, q) = temp_query();
    let _hash = seed_genesis(&q);
    let create_fk = q.tx_fk_by_txid(&[0xcb; 32]).unwrap().unwrap();
    let spend = (
        TxRecord {
            txid: [0x11; 32],
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        vec![InputRecord {
            prev_txid: [0xcb; 32],
            parent: rbitcoin_store::InputParent::Prevout { create_fk, vout: 0 },
            sequence: u32::MAX,
            script_sig: vec![],
            witness: vec![],
        }],
        vec![OutputRecord::unspent(1, vec![0x51])],
    );
    let spend_fk = q.store().put_tx_full_batch_indexed(&[spend], true).unwrap()[0];
    q.store()
        .put_spend_create(create_fk, 0, spend_fk, 0)
        .unwrap();
    let view = q.pin_chain_view().unwrap().expect("tip");
    q.store()
        .header_txs
        .put_range(
            &rbitcoin_store::ClassCWrite::for_test(),
            view.header_fk,
            create_fk,
            2,
        )
        .unwrap();
    q.store()
        .strong_tx
        .set_strong(
            &rbitcoin_store::ClassCWrite::for_test(),
            spend_fk,
            view.header_fk,
        )
        .unwrap();
    q.store().rebuild_height_fence().unwrap();
    let v = outspend_json(&q, None, &[0xcb; 32], 0, Some(&view)).unwrap();
    assert_eq!(v["spent"], true, "{v}");
    assert!(v.get("txid").is_some(), "{v}");
    assert_eq!(v["vin"], 0, "outspend vin from spent slot: {v}");
    assert!(v.get("status").is_some(), "{v}");
    assert!(v.get("next").is_none(), "{v}");
    let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["spent", "status", "txid", "vin"], "{v}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bare_multisig_of_three_bytes_is_multisig() {
    // `is_bare_multisig`: a script of length 3 ending in CHECKMULTISIG. Classification calls this.
    let s = bitcoin::Script::from_bytes(&[0x51, 0x51, rbitcoin_primitives::OP_CHECKMULTISIG]);
    assert!(rbitcoin_esplora::script_fields::is_bare_multisig(s));
}

#[test]
fn newest_sort_skip_and_page_cut() {
    // `sort_txs_newest_first`, `skip_rows_after`, `truncate_page`. `/txs` pages call these.
    let unconf = json!({"txid": "aa", "status": {"confirmed": false}});
    let conf = json!({"txid": "bb", "status": {"confirmed": true, "block_height": 1}});
    for mut rows in [
        vec![conf.clone(), unconf.clone()],
        vec![unconf.clone(), conf.clone()],
    ] {
        sort_txs_newest_first(&mut rows);
        assert_eq!(rows[0]["txid"], "aa");
        assert_eq!(rows[1]["txid"], "bb");
    }
    let mut id = [0u8; 32];
    id[0] = 1;
    let hex = rbitcoin_esplora::server::block_hash_hex(&id);
    let rows = vec![
        json!({"txid": "00"}),
        json!({"txid": hex}),
        json!({"txid": "ff"}),
    ];
    let rest = skip_rows_after(rows, Some(id)).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["txid"], "ff");
    let mut page: Vec<_> = (0..25).map(|i| json!(i)).collect();
    truncate_page(&mut page, 25);
    assert_eq!(page.len(), 25);
    assert_eq!(page[24], json!(24));
    page.push(json!(25));
    truncate_page(&mut page, 25);
    assert_eq!(page.len(), 25);
    assert_eq!(page[24], json!(24));
}

#[test]
fn summary_time_at_height_zero_and_no_position_below() {
    let (dir, q) = temp_query();
    let _hash = seed_genesis(&q);
    let fk = q.tx_fk_by_txid(&[0xcb; 32]).unwrap().unwrap();
    let items = [
        ScriptHashTxSummary {
            txid: [0xcb; 32],
            value: 1,
            height: 0,
            tx_fk: Fk::NULL,
        },
        ScriptHashTxSummary {
            txid: [1; 32],
            value: 1,
            height: -1,
            tx_fk: fk,
        },
    ];
    let v = summaries_json(&q, &items).unwrap();
    assert_eq!(v[0]["time"], 1, "{v}");
    assert!(v[1].get("tx_position").is_none(), "{v}");
    let _ = std::fs::remove_dir_all(dir);
}
