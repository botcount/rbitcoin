use rbitcoin_consensus::ChainParams;
use rbitcoin_electrum::server::*;
use rbitcoin_net::MempoolHub;
use rbitcoin_primitives::{Fk, Height};
use rbitcoin_query::Query;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use rbitcoin_query::testutil::FixtureChain;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::silent_scan::no_times;

fn tmp_store() -> (rbitcoin_query::testutil::TempDir, Query) {
    rbitcoin_query::testutil::tiny_query_labeled("electrum")
}

#[allow(clippy::cognitive_complexity)] // one fixture, many parser arms
#[test]
fn config_helpers_and_param_parsers() {
    let params = ChainParams::regtest();
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    assert_eq!(cfg.genesis_hash_hex, params.genesis_hash.to_string());
    assert!(cfg.banner.contains("rbitcoin"));
    assert_eq!(
        cfg.tweaks_min_dust,
        rbitcoin_electrum::tweaks::DEFAULT_TWEAKS_MIN_DUST
    );

    let sh = electrum_scripthash_hex(&[0x51]);
    assert_eq!(sh.len(), 64);

    assert_eq!(param_u32(&json!([3]), 0).unwrap(), 3);
    assert_eq!(param_u32(&json!(["7"]), 0).unwrap(), 7);
    assert!(param_u32(&json!([]), 0).is_err());
    assert!(param_u32(&json!([true]), 0).is_err());
    assert!(param_u32(&json!([Value::Null]), 0).is_err());
    assert!(param_u32(&json!({"0": 1}), 0).is_err());
    assert!(param_u32(&json!([1.5]), 0).is_err());
    assert!(
        param_u32(&json!([1u64 << 32]), 0).is_err(),
        "u32 overflow must not wrap"
    );
    assert_eq!(param_u32(&json!([u32::MAX as u64]), 0).unwrap(), u32::MAX);
    assert!(param_i64(&json!([true]), 0).is_err());
    assert!(param_str(&json!([false]), 0).is_err());
    assert!(param_str(&json!({}), 0).is_err());
    assert!(parse_electrum_request_line("{").is_none());
    assert!(parse_electrum_request_line("").is_none());
    assert!(parse_electrum_request_line("not-json").is_none());
    let ping_line = parse_electrum_request_line(r#"{"id":1,"method":"server.ping"}"#).unwrap();
    assert_eq!(ping_line["method"], "server.ping");
    assert!(parse_electrum_request_line("[1]").unwrap().is_array());
    assert_eq!(param_i64(&json!([-1]), 0).unwrap(), -1);
    assert_eq!(param_i64(&json!(["10"]), 0).unwrap(), 10);
    assert_eq!(param_str(&json!(["hi"]), 0).unwrap(), "hi");
    assert!(param_str(&json!([1]), 0).is_err());

    let (dir, q) = tmp_store();
    q.set_sh_index_enabled(false);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let mut header_sub = false;
    let mut sh_subs = HashSet::new();
    let sh = electrum_scripthash_hex(&[0x51]);
    let err = dispatch(
        "blockchain.scripthash.get_balance",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert_eq!(err, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    // Tweaks and silent payments share that index gate.
    let tweaks_off = dispatch(
        "blockchain.tweaks.subscribe",
        &json!([]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert_eq!(tweaks_off, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    let sp_off = dispatch(
        "blockchain.silentpayments.subscribe",
        &json!([]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert_eq!(sp_off, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    let ping = dispatch(
        "server.ping",
        &json!([]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    assert_eq!(ping, Value::Null);
    let _ = std::fs::remove_dir_all(&dir);

    let mut sh_bytes = [0u8; 32];
    sh_bytes[0] = 0xaa;
    let sh_hex = hash_hex_rev(&sh_bytes);
    let parsed = param_scripthash(&json!([sh_hex]), 0).unwrap();
    assert_eq!(parsed, sh_bytes);
    assert!(param_scripthash(&json!(["aa"]), 0).is_err());

    let tid = param_txid(&json!([sh_hex]), 0).unwrap();
    assert_eq!(tid, sh_bytes);
    assert!(param_txid(&json!(["aabb"]), 0).is_err());
    assert!(param_txid(&json!(["zz".repeat(32)]), 0).is_err());

    // get_history window: 1-arg open + mempool; finite to excludes mempool.
    let (f, mp) = parse_get_history_window(&json!([sh_hex])).unwrap();
    assert_eq!(f.from_height, 0);
    assert!(f.to_height.is_none());
    assert!(mp);
    let (f, mp) = parse_get_history_window(&json!([sh_hex, 5])).unwrap();
    assert_eq!(f.from_height, 5);
    assert!(f.to_height.is_none());
    assert!(mp);
    let (f, mp) = parse_get_history_window(&json!([sh_hex, 2, -1])).unwrap();
    assert_eq!(f.from_height, 2);
    assert!(f.to_height.is_none());
    assert!(mp);
    let (f, mp) = parse_get_history_window(&json!([sh_hex, 1, 10])).unwrap();
    assert_eq!(f.from_height, 1);
    assert_eq!(f.to_height, Some(10));
    assert!(!mp);
    let asof_hex = "ab".repeat(32);
    let tagged = format!("asof:{asof_hex}");
    let (rest, h) = take_trailing_asof(
        "blockchain.scripthash.get_balance",
        &json!([sh_hex, tagged]),
    )
    .unwrap();
    assert!(h.is_some());
    assert_eq!(rest, json!([sh_hex]));
    let (rest_win, h_win) = take_trailing_asof(
        "blockchain.scripthash.get_history",
        &json!([sh_hex, 1, 10, tagged]),
    )
    .unwrap();
    assert!(h_win.is_some());
    assert_eq!(rest_win, json!([sh_hex, 1, 10]));
    let (rest_tx, h_tx) =
        take_trailing_asof("blockchain.transaction.get", &json!([sh_hex, tagged])).unwrap();
    assert!(h_tx.is_some());
    assert_eq!(rest_tx, json!([sh_hex]));
    let (rest_merkle, h_merkle) = take_trailing_asof(
        "blockchain.transaction.get_merkle",
        &json!([sh_hex, 0, tagged]),
    )
    .unwrap();
    assert!(h_merkle.is_some());
    assert_eq!(rest_merkle, json!([sh_hex, 0]));
    let (_, none) =
        take_trailing_asof("blockchain.scripthash.get_balance", &json!([sh_hex])).unwrap();
    assert!(none.is_none());
    let (_, not_hex) = take_trailing_asof(
        "blockchain.scripthash.get_balance",
        &json!([sh_hex, asof_hex]),
    )
    .unwrap();
    assert!(
        not_hex.is_none(),
        "bare trailing hex must not be asof (future positional hash args)"
    );
    let (_, leftover_obj) = take_trailing_asof(
        "blockchain.scripthash.get_balance",
        &json!([sh_hex, { "other": true }]),
    )
    .unwrap();
    assert!(leftover_obj.is_none());
    assert!(take_trailing_asof(
        "blockchain.scripthash.get_balance",
        &json!([sh_hex, "asof:zz"])
    )
    .unwrap_err()
    .contains("asof:<32-byte hex>"));
    let asof_tag = format!("asof:{}", "ab".repeat(32));
    let (_, ignored) =
        take_trailing_asof("blockchain.block.header", &json!([0, asof_tag])).unwrap();
    assert!(
        ignored.is_none(),
        "asof tag on a method that does not accept asof is leftover params, not a view"
    );
    assert!(parse_get_history_window(&json!({}))
        .unwrap_err()
        .contains("array"));

    assert!(parse_get_history_window(&json!([sh_hex, 10, 5]))
        .unwrap_err()
        .contains("from_height"));
    assert!(parse_get_history_window(&json!([sh_hex, 0, -2]))
        .unwrap_err()
        .contains("to_height"));
    let (f, mp) = parse_get_history_window(&json!([sh_hex, 0, 0])).unwrap();
    assert_eq!(f.from_height, 0);
    assert_eq!(f.to_height, Some(0));
    assert!(!mp, "to_height 0 is an empty confirmed window");
    let (f, mp) = parse_get_history_window(&json!([sh_hex, 5, 5])).unwrap();
    assert_eq!(f.from_height, 5);
    assert_eq!(f.to_height, Some(5));
    assert!(!mp);

    assert!(scripthash_status(&[]).is_empty());
    let one = scripthash_status(&[rbitcoin_query::ScriptHashHistoryItem {
        height: 1,
        txid: [1u8; 32],
        tx_fk: Fk::NULL,
        fee: None,
    }]);
    let preimage = format!("{}:1:", txid_hex(&[1u8; 32]));
    assert_eq!(
        one,
        rbitcoin_primitives::hex_encode(
            bitcoin::hashes::sha256::Hash::hash(preimage.as_bytes()).to_byte_array()
        ),
        "confirmed row status is tx_hash:height: with no block hash"
    );

    use bitcoin::hashes::Hash;
    let hdr = bitcoin::block::Header {
        version: bitcoin::block::Version::ONE,
        prev_blockhash: bitcoin::BlockHash::from_byte_array([0; 32]),
        merkle_root: bitcoin::TxMerkleNode::from_byte_array([0; 32]),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x207fffff),
        nonce: 0,
    };
    let hex = header_hex(&hdr);
    assert_eq!(hex.len(), 160);

    assert_eq!(tick_scan(Some(1), Some(1)), None);
    assert_eq!(tick_scan(Some(0), Some(2)), Some(Some(vec![1, 2])));
    assert_eq!(tick_scan(Some(0), Some(32)), Some(Some((1..=32).collect())));
    assert_eq!(
        tick_scan(Some(0), Some(33)),
        Some(None),
        "gap above TICK_SCAN_MAX_GAP restatuses all"
    );
    assert_eq!(tick_scan(Some(2), Some(1)), Some(None));
    assert_eq!(tick_scan(None, Some(0)), Some(None));
    let mut last = HashMap::new();
    let mut subs = HashSet::new();
    let sh = [9u8; 32];
    subs.insert(sh);
    let first = take_new_status(&mut last, &subs, sh, "aa".into()).unwrap();
    assert_eq!(first, "aa");
    assert!(take_new_status(&mut last, &subs, sh, "aa".into()).is_none());
    assert_eq!(
        take_new_status(&mut last, &subs, sh, "bb".into()).unwrap(),
        "bb"
    );
}

#[test]
fn drop_unsubscribed_status_clears_idle_hashes() {
    let mut last = HashMap::new();
    let gone = [1u8; 32];
    let keep = [2u8; 32];
    last.insert(gone, "x".into());
    last.insert(keep, "y".into());
    let mut subs = HashSet::new();
    subs.insert(keep);
    drop_unsubscribed_status(&mut last, &subs);
    assert_eq!(last.get(&keep), Some(&"y".to_string()));
    assert!(!last.contains_key(&gone));
}

#[test]
fn restatus_notes_scans_intermediate_tick_heights() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let merkle = [0x51; 32];
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
    let mut txid0 = [0u8; 32];
    txid0[0] = 0xa0;
    let ta0 = TxApply {
        tx: TxRecord {
            txid: txid0,
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
    let hash1 = rbitcoin_store::block_header_hash(1, &merkle, &[0x11; 32], 2, 0x207fffff, 1);
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
    let mut txid1 = [0u8; 32];
    txid1[0] = 0xa1;
    q.connect_block(
        Height(1),
        &h1,
        &[TxApply {
            tx: TxRecord {
                txid: txid1,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord {
                prev_txid: [0u8; 32],
                parent: rbitcoin_store::InputParent::Coinbase,
                sequence: u32::MAX,
                script_sig: vec![1],
                witness: vec![],
            }],
            outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x00])],
        }],
    )
    .unwrap();
    q.apply_sh_pending().unwrap();
    let sh = rbitcoin_store::script_hash(&[0x51]);
    let only_tip = restatus_notes(&q, None, &[sh], Some(&[1]));
    assert!(
        only_tip.is_empty(),
        "height 1 does not touch the OP_TRUE script"
    );
    let range = restatus_notes(&q, None, &[sh], Some(&[1, 0]));
    assert_eq!(range.len(), 1, "range must include the height-0 create");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A degraded or rebuilding scripthash index reads as empty history. The
/// subscription push must hold (None: the caller reads every sub again once the
/// index serves) instead of telling a wallet its address emptied.
#[test]
fn settled_restatus_holds_while_sh_index_degraded() {
    let (dir, q) = tmp_store();
    let sh = rbitcoin_store::script_hash(&[0x51]);
    assert!(settled_restatus_notes(&q, None, &[sh], None).is_some());
    q.mark_sh_degraded("test fault");
    assert!(
        settled_restatus_notes(&q, None, &[sh], None).is_none(),
        "degraded index must not produce statuses"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn negotiate_protocol_intersection_and_asof_dialect() {
    assert_eq!(negotiate_protocol(&json!([])).unwrap(), PROTOCOL_MAX);
    assert_eq!(negotiate_protocol(&json!(["c"])).unwrap(), PROTOCOL_MAX);
    assert_eq!(negotiate_protocol(&json!(["c", "1.4"])).unwrap(), "1.4");
    assert_eq!(negotiate_protocol(&json!(["c", "1.4.2"])).unwrap(), "1.4.2");
    assert_eq!(
        negotiate_protocol(&json!(["c", ["1.4", "1.4.2"]])).unwrap(),
        "1.4.2"
    );
    assert_eq!(
        negotiate_protocol(&json!(["c", PROTOCOL_ASOF])).unwrap(),
        PROTOCOL_ASOF
    );
    assert_eq!(
        negotiate_protocol(&json!(["c", ["1.4", PROTOCOL_ASOF]])).unwrap(),
        PROTOCOL_ASOF
    );
    assert_eq!(negotiate_protocol(&json!(["c", "1.6"])).unwrap(), "1.6");
    assert!(negotiate_protocol(&json!(["c", "1.7"]))
        .unwrap_err()
        .contains("unsupported"));
    assert!(negotiate_protocol(&json!(["c", ["1.6.1", "1.7"]]))
        .unwrap_err()
        .contains("unsupported"));
    assert!(negotiate_protocol(&json!(["c", "1.3"]))
        .unwrap_err()
        .contains("unsupported"));
    assert_eq!(negotiate_protocol(&json!(["c", "1.5"])).unwrap(), "1.5");
    assert_eq!(
        negotiate_protocol(&json!(["c", ["1.3", "1.5"]])).unwrap(),
        "1.5"
    );
}

/// Protocol 1.4 returns one concatenated header hex. 1.6 returns a header list.
#[test]
fn block_headers_use_hex_until_protocol_1_6() {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let mut v14 = ElectrumConn::new();
    dispatch_with_join(
        "server.version",
        &json!(["c", "1.4"]),
        &q,
        &cfg,
        &params,
        None,
        &mut v14,
    )
    .unwrap();
    let old = dispatch_with_join(
        "blockchain.block.headers",
        &json!([0, 1]),
        &q,
        &cfg,
        &params,
        None,
        &mut v14,
    )
    .unwrap();
    assert_eq!(old["hex"], "");
    assert!(old.get("headers").is_none(), "{old}");
    let mut v16 = ElectrumConn::new();
    dispatch_with_join(
        "server.version",
        &json!(["c", "1.6"]),
        &q,
        &cfg,
        &params,
        None,
        &mut v16,
    )
    .unwrap();
    let cur = dispatch_with_join(
        "blockchain.block.headers",
        &json!([0, 1]),
        &q,
        &cfg,
        &params,
        None,
        &mut v16,
    )
    .unwrap();
    assert_eq!(cur["headers"], json!([]));
    assert!(cur.get("hex").is_none(), "{cur}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One line, one byte at a time. A `BufReader` can pull the next server line
/// into its buffer and drop it, so a later read sees the wrong message.
async fn read_exact_line(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut byte))
            .await
            .expect("timeout")
            .unwrap();
        assert_ne!(n, 0, "electrum peer closed");
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(buf).unwrap()
}

pub(crate) async fn electrum_tcp_rpc(
    stream: &mut TcpStream,
    id: u32,
    method: &str,
    params: Value,
) -> Value {
    let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let resp = read_exact_line(stream).await;
    serde_json::from_str(&resp).unwrap()
}

fn dotted_at_least(version: &str, min: &[u64]) -> bool {
    let parts: Option<Vec<u64>> = version.split('.').map(|p| p.parse().ok()).collect();
    parts.is_some_and(|p| p.as_slice() >= min)
}

/// `server.version[0]` as the wallets that branch on it read it.
/// Sparrow `ElectrumServer.getServerCapability` (lowercased): batches when it
/// contains `electrumx`, or starts with `electrs/` and the version before the
/// first `-` is at least 0.9.0. BlueWallet `BlueElectrum.ts` (case-sensitive):
/// a string starting `ElectrumPersonalServer`, `electrs` or `Fulcrum` stops
/// batching unless the first space-separated word is `electrs` at 0.9.0 or
/// `Fulcrum` at 1.9.0 or later. Cake `getNodeIsElectrs` (lowercased) probes
/// tweaks only when it contains `electrs`.
fn assert_wallets_read(server: &str) {
    let lower = server.to_ascii_lowercase();
    let sparrow = lower.contains("electrumx")
        || lower
            .strip_prefix("electrs/")
            .is_some_and(|rest| dotted_at_least(rest.split('-').next().unwrap_or(""), &[0, 9, 0]));
    assert!(
        sparrow,
        "Sparrow would send one request per line for {server:?}"
    );
    let mut words = server.split(' ');
    let implementation = words.next().unwrap_or("");
    let version = words.next().unwrap_or("");
    let bluewallet = !["ElectrumPersonalServer", "electrs", "Fulcrum"]
        .iter()
        .any(|p| server.starts_with(p))
        || match implementation {
            "electrs" => dotted_at_least(version, &[0, 9, 0]),
            "Fulcrum" => dotted_at_least(version, &[1, 9, 0]),
            _ => false,
        };
    assert!(bluewallet, "BlueWallet would stop batching for {server:?}");
    assert!(
        lower.contains("electrs"),
        "Cake would skip the tweaks probe for {server:?}"
    );
}

pub(crate) async fn electrum_tcp_batch(stream: &mut TcpStream, items: &[Value]) -> Value {
    let mut line = serde_json::to_string(items).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    serde_json::from_str(&read_exact_line(stream).await).unwrap()
}

#[allow(clippy::cognitive_complexity)] // one TCP client, every batch arm
#[tokio::test]
async fn batch_line_answers_every_item_in_one_array() {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let banner = cfg.banner.clone();
    let max_line = cfg.max_line_bytes();
    let handle = run_electrum(cfg, Arc::new(q), params, tip_tx, None)
        .await
        .expect("listen");
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();

    let batch = electrum_tcp_batch(
        &mut stream,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"server.version","params":["Sparrow",["1.3","1.4.2"]]}),
            json!({"jsonrpc":"2.0","id":"two","method":"server.ping","params":[]}),
            json!({"jsonrpc":"2.0","id":3,"method":"no.such.method","params":[]}),
            json!({"jsonrpc":"2.0","id":4,"method":"blockchain.tweaks.subscribe","params":[0,1,false]}),
            json!({"jsonrpc":"2.0","id":5,"method":"blockchain.silentpayments.subscribe","params":[]}),
            json!({"jsonrpc":"2.0","id":6,"method":"server.banner","params":[]}),
        ],
    )
    .await;
    let items = batch
        .as_array()
        .expect("a batch line is answered by one array");
    let ids: Vec<&Value> = items.iter().map(|v| &v["id"]).collect();
    assert_eq!(
        ids,
        [
            &json!(1),
            &json!("two"),
            &json!(3),
            &json!(4),
            &json!(5),
            &json!(6)
        ],
        "{batch}"
    );
    assert!(items.iter().all(|v| v["jsonrpc"] == "2.0"), "{batch}");
    assert_wallets_read(items[0]["result"][0].as_str().unwrap());
    assert_eq!(
        items[0]["result"][1], "1.4.2",
        "Sparrow offers [1.3, 1.4.2]"
    );
    assert!(items[1]["result"].is_null() && items[1].get("error").is_none());
    assert!(
        items[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown method"),
        "{batch}"
    );
    for streaming in &items[3..5] {
        assert!(streaming.get("result").is_none(), "{streaming}");
        assert!(
            streaming["error"]["message"]
                .as_str()
                .unwrap()
                .contains("batch"),
            "{streaming}"
        );
    }
    assert_eq!(items[5]["result"].as_str(), Some(banner.as_str()));
    let ping = electrum_tcp_rpc(&mut stream, 7, "server.ping", json!([])).await;
    assert_eq!(
        ping["id"], 7,
        "a rejected stream must not write lines after the batch: {ping}"
    );

    stream.write_all(b"[]\n").await.unwrap();
    let empty: Value = serde_json::from_str(&read_exact_line(&mut stream).await).unwrap();
    assert_eq!(
        empty,
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"empty batch"}})
    );

    let page: Vec<Value> = (0..100u32)
        .map(|i| {
            json!({"jsonrpc":"2.0","id":i,"method":"blockchain.scripthash.get_balance",
                   "params":[format!("{i:064x}")]})
        })
        .collect();
    assert!(serde_json::to_string(&page).unwrap().len() < max_line);
    let answered = electrum_tcp_batch(&mut stream, &page).await;
    let answered = answered.as_array().expect("array");
    assert_eq!(answered.len(), 100);
    for (i, v) in answered.iter().enumerate() {
        assert_eq!(v["id"], i, "{v}");
        assert!(v.get("result").is_some() != v.get("error").is_some(), "{v}");
    }

    let mut oversize: Vec<Value> = (0..MAX_BATCH_ITEMS)
        .map(|i| json!({"jsonrpc":"2.0","id":i,"method":"server.ping","params":[]}))
        .collect();
    let full = electrum_tcp_batch(&mut stream, &oversize).await;
    assert_eq!(full.as_array().map(Vec::len), Some(MAX_BATCH_ITEMS));
    oversize.push(json!({"jsonrpc":"2.0","id":"over","method":"server.ping","params":[]}));
    assert_eq!(
        electrum_tcp_batch(&mut stream, &oversize).await,
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch too large"}})
    );
    let ping = electrum_tcp_rpc(&mut stream, 8, "server.ping", json!([])).await;
    assert_eq!(ping["id"], 8);

    let mixed = electrum_tcp_batch(
        &mut stream,
        &[
            json!({"jsonrpc":"2.0","method":"server.ping","params":[]}),
            json!({"jsonrpc":"2.0","id":null,"method":"server.ping","params":[]}),
            json!(1),
            json!("x"),
        ],
    )
    .await;
    let invalid = json!({"code":-32600,"message":"Invalid Request"});
    assert_eq!(
        mixed,
        json!([
            {"jsonrpc":"2.0","id":null,"result":null},
            {"jsonrpc":"2.0","id":null,"error":invalid},
            {"jsonrpc":"2.0","id":null,"error":invalid},
        ]),
        "a notification gets no slot; an explicit null id does"
    );

    let notes = [
        json!({"jsonrpc":"2.0","method":"server.ping","params":[]}),
        json!({"jsonrpc":"2.0","method":"blockchain.tweaks.subscribe","params":[0,1,false]}),
    ];
    let mut line = serde_json::to_string(&notes).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let ping = electrum_tcp_rpc(&mut stream, 9, "server.ping", json!([])).await;
    assert_eq!(
        ping,
        json!({"jsonrpc":"2.0","id":9,"result":null}),
        "a batch of notifications writes nothing"
    );

    let noted = format!("{:064x}", 0xabc);
    let watched = format!("{:064x}", 0xdef);
    let subs = [
        json!({"jsonrpc":"2.0","method":"blockchain.scripthash.subscribe","params":[noted]}),
        json!({"jsonrpc":"2.0","id":10,"method":"blockchain.scripthash.subscribe","params":[watched]}),
    ];
    let reply = electrum_tcp_batch(&mut stream, &subs).await;
    assert_eq!(reply.as_array().map(Vec::len), Some(1), "{reply}");
    assert!(reply[0].get("result").is_some(), "{reply}");
    let unsub = "blockchain.scripthash.unsubscribe";
    let was = electrum_tcp_rpc(&mut stream, 11, unsub, json!([watched])).await;
    assert_eq!(was["result"], json!(true), "{was}");
    let was = electrum_tcp_rpc(&mut stream, 12, unsub, json!([noted])).await;
    assert_eq!(
        was["result"],
        json!(false),
        "a notification is skipped, so it subscribes nothing: {was}"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A read-only batch still completes when the node-wide budget is 1.
/// Holding that permit stalls the batch; releasing it lets the batch finish.
#[tokio::test]
async fn read_only_batch_completes_when_the_store_budget_is_one() {
    let (dir, q) = tmp_store();
    q.set_api_read_budget(1);
    let q = Arc::new(q);
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.batch_window = 4;
    let handle = run_electrum(cfg, Arc::clone(&q), params, tip_tx, None)
        .await
        .expect("listen");
    let items: Vec<Value> = (0..3)
        .map(|id| {
            json!({"jsonrpc":"2.0","id":id,"method":"blockchain.scripthash.get_balance",
                   "params":[format!("{id:064x}")]})
        })
        .collect();
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        electrum_tcp_batch(&mut stream, &items),
    )
    .await
    .expect("budget 1 serializes the batch; it must not fail")
    .as_array()
    .expect("batch array")
    .clone();
    assert_eq!(reply.len(), 3, "{reply:?}");

    let hold = q.api_read_budget().acquire_owned().await.expect("permit");
    let mut stalled = TcpStream::connect(handle.local_addr).await.unwrap();
    let mut line = serde_json::to_string(&items).unwrap();
    line.push('\n');
    stalled.write_all(line.as_bytes()).await.unwrap();
    let early =
        tokio::time::timeout(Duration::from_millis(200), read_exact_line(&mut stalled)).await;
    assert!(early.is_err(), "the batch must wait for the held permit");
    drop(hold);
    let late = tokio::time::timeout(Duration::from_secs(5), read_exact_line(&mut stalled))
        .await
        .expect("batch finishes after the permit is released");
    let parsed: Value = serde_json::from_str(&late).unwrap();
    assert_eq!(parsed.as_array().expect("array").len(), 3, "{parsed}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A subscription restatus waits for the store budget on the connection task.
/// The only blocking-pool thread stays free for other work during that wait.
#[test]
fn subscription_restatus_waits_without_a_pool_thread() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let (dir, q) = tmp_store();
        q.set_api_read_budget(1);
        let q = Arc::new(q);
        let hold = q.api_read_budget().acquire_owned().await.expect("permit");
        let (_read, write) = tokio::io::duplex(256);
        let mut writer = ConnWrite::new(write, Duration::from_secs(2));
        let subs = HashSet::new();
        let mut last = HashMap::new();
        let q2 = Arc::clone(&q);
        let notes = tokio::spawn(async move {
            emit_sh_notes(&mut writer, &q2, None, &subs, &mut last, None).await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !notes.is_finished(),
            "restatus ran while the store permit was held"
        );
        let probe = tokio::time::timeout(Duration::from_secs(2), tokio::task::spawn_blocking(|| 7))
            .await
            .expect("the budget wait must leave the blocking pool free")
            .expect("probe join");
        assert_eq!(probe, 7);
        assert!(
            !notes.is_finished(),
            "restatus took the pool thread to wait"
        );
        drop(hold);
        tokio::time::timeout(Duration::from_secs(2), notes)
            .await
            .expect("restatus finishes after the permit is released")
            .expect("join")
            .expect("restatus");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[cfg(debug_assertions)]
fn get_balance(id: usize) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"blockchain.scripthash.get_balance",
           "params":[format!("{id:064x}")]})
}

#[cfg(debug_assertions)]
fn spawned(trace: &[BatchStep]) -> Vec<usize> {
    trace
        .iter()
        .filter_map(|s| match s {
            BatchStep::Spawn(i) => Some(*i),
            _ => None,
        })
        .collect()
}

#[cfg(debug_assertions)]
fn written(trace: &[BatchStep]) -> Vec<usize> {
    trace
        .iter()
        .filter_map(|s| match s {
            BatchStep::Write(i) => Some(*i),
            _ => None,
        })
        .collect()
}

#[cfg(debug_assertions)]
fn at(trace: &[BatchStep], step: BatchStep) -> usize {
    trace
        .iter()
        .position(|s| *s == step)
        .unwrap_or_else(|| panic!("{step:?} missing from {trace:?}"))
}

#[test]
fn batch_window_is_half_the_blocking_pool() {
    assert_eq!(batch_window(1), 1);
    assert_eq!(batch_window(MIN_BLOCKING_POOL), 2);
    assert_eq!(batch_window(12), 6);
    let params = ChainParams::regtest();
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    assert_eq!(cfg.batch_window, 2, "the smallest pool the node builds");
}

/// Read-only batch items run up to the configured window at a time and land
/// in request order. A subscribe waits for every earlier read to be written,
/// runs alone, and later items see what it did. The connection task runs on
/// this thread (current-thread runtime), so the armed trace sees its steps.
#[cfg(debug_assertions)]
#[tokio::test]
async fn batch_reads_overlap_and_land_in_request_order() {
    for window in [1, 2, 4] {
        reads_overlap_in_window(window).await;
    }
}

#[cfg(debug_assertions)]
#[allow(clippy::cognitive_complexity)] // one batch, every scheduling rule
async fn reads_overlap_in_window(window: usize) {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.batch_window = window;
    let handle = run_electrum(cfg, Arc::new(q), params, tip_tx, None)
        .await
        .expect("listen");
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();

    let watched = format!("{:064x}", 0xabc);
    let mut items: Vec<Value> = (0..6).map(get_balance).collect();
    items.push(json!({"jsonrpc":"2.0","id":6,"method":"blockchain.scripthash.subscribe","params":[watched]}));
    items.extend((7..10).map(get_balance));
    items.push(json!({"jsonrpc":"2.0","id":10,"method":"blockchain.scripthash.unsubscribe","params":[watched]}));
    items.push(
        json!({"jsonrpc":"2.0","method":"blockchain.scripthash.get_balance","params":[watched]}),
    );
    items.push(json!(12));

    batch_trace_arm();
    let reply = electrum_tcp_batch(&mut stream, &items).await;
    let trace = batch_trace_snapshot();

    let ids: Vec<Value> = reply
        .as_array()
        .expect("one array line")
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    let mut want: Vec<Value> = (0..=10).map(|i| json!(i)).collect();
    want.push(Value::Null);
    assert_eq!(ids, want, "{reply}");
    assert_eq!(
        reply[10]["result"],
        json!(true),
        "the subscribe ran before the unsubscribe: {reply}"
    );

    assert_eq!(written(&trace), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12]);
    assert_eq!(spawned(&trace), [0, 1, 2, 3, 4, 5, 7, 8, 9], "{trace:?}");
    let mut open = 0usize;
    let mut peak = 0usize;
    for step in &trace {
        match step {
            BatchStep::Spawn(_) => open += 1,
            BatchStep::Write(i) if spawned(&trace).contains(i) => open -= 1,
            _ => {}
        }
        peak = peak.max(open);
    }
    assert_eq!(peak, window, "{trace:?}");
    assert_eq!(
        at(&trace, BatchStep::Spawn(1)) < at(&trace, BatchStep::Write(0)),
        window > 1,
        "reads overlap only in a window above one: {trace:?}"
    );
    for barrier in [6, 10] {
        assert!(
            at(&trace, BatchStep::Write(barrier - 1)) < at(&trace, BatchStep::Serial(barrier)),
            "{barrier} waits for earlier reads: {trace:?}"
        );
    }
    assert!(
        at(&trace, BatchStep::Write(6)) < at(&trace, BatchStep::Spawn(7)),
        "reads after a subscribe start after it: {trace:?}"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A client that stops reading stops the batch: the server writes slot i
/// before it starts read i + window.
#[cfg(debug_assertions)]
#[tokio::test]
async fn stalled_batch_reader_holds_reads_to_the_window() {
    for window in [1, 3] {
        stalled_reader_in_window(window).await;
    }
}

#[cfg(debug_assertions)]
async fn stalled_reader_in_window(window: usize) {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    struct OnceRead(Vec<u8>);
    impl AsyncRead for OnceRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.0.is_empty() {
                return Poll::Pending;
            }
            let n = this.0.len().min(buf.remaining());
            buf.put_slice(&this.0[..n]);
            this.0.drain(..n);
            Poll::Ready(Ok(()))
        }
    }
    /// Takes the first write, then never another.
    struct OneWrite {
        took: bool,
        stalled: Arc<AtomicBool>,
        wake: Arc<tokio::sync::Notify>,
    }
    impl AsyncWrite for OneWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            if this.took {
                this.stalled.store(true, Ordering::SeqCst);
                this.wake.notify_waiters();
                return Poll::Pending;
            }
            this.took = true;
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    struct Duplex(OnceRead, OneWrite);
    impl AsyncRead for Duplex {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for Duplex {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().1).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().1).poll_flush(cx)
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().1).poll_shutdown(cx)
        }
    }

    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.batch_window = window;
    let items: Vec<Value> = (0..20).map(get_balance).collect();
    let mut line = serde_json::to_vec(&items).unwrap();
    line.push(b'\n');
    let stalled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(tokio::sync::Notify::new());
    let (_tip_tx, tip_rx) = broadcast::channel(1);
    batch_trace_arm();
    let task = tokio::spawn(handle_client(
        Duplex(
            OnceRead(line),
            OneWrite {
                took: false,
                stalled: Arc::clone(&stalled),
                wake: Arc::clone(&wake),
            },
        ),
        "127.0.0.1:9".parse().unwrap(),
        Arc::new(q),
        Arc::new(cfg),
        Arc::new(params),
        tip_rx,
        None,
        ElectrumStop::new(false),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let notified = wake.notified();
        if stalled.load(Ordering::SeqCst) {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "the second slot was never written");
        if tokio::time::timeout(left, notified).await.is_err() {
            panic!("the second slot was never written");
        }
    }
    let trace = batch_trace_snapshot();
    task.abort();
    assert_eq!(written(&trace), [0, 1], "{trace:?}");
    assert_eq!(
        spawned(&trace),
        (0..=window).collect::<Vec<_>>(),
        "slot 0 is written before read {window} starts, and slot 1 blocks the rest: {trace:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A batch reply goes to the socket one slot per write, so a client that
/// reads slowly holds the server to about one reply, not the whole array.
#[tokio::test]
async fn batch_reply_is_written_one_slot_at_a_time() {
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    struct OnceRead(Vec<u8>);
    impl AsyncRead for OnceRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.0.is_empty() {
                return Poll::Pending;
            }
            let n = this.0.len().min(buf.remaining());
            buf.put_slice(&this.0[..n]);
            this.0.drain(..n);
            Poll::Ready(Ok(()))
        }
    }
    #[derive(Default)]
    struct Seen {
        bytes: Vec<u8>,
        largest_write: usize,
    }
    struct Recorder {
        seen: Arc<Mutex<Seen>>,
        wake: Arc<tokio::sync::Notify>,
    }
    impl AsyncWrite for Recorder {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let mut seen = this.seen.lock().unwrap();
            seen.largest_write = seen.largest_write.max(buf.len());
            seen.bytes.extend_from_slice(buf);
            drop(seen);
            this.wake.notify_waiters();
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    struct Duplex(OnceRead, Recorder);
    impl AsyncRead for Duplex {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for Duplex {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().1).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().1).poll_flush(cx)
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().1).poll_shutdown(cx)
        }
    }

    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.banner = "b".repeat(64 * 1024);
    let slots = 40;
    let batch: Vec<Value> = (0..slots)
        .map(|i| json!({"jsonrpc":"2.0","id":i,"method":"server.banner","params":[]}))
        .collect();
    let mut line = serde_json::to_vec(&batch).unwrap();
    line.push(b'\n');
    let seen = Arc::new(Mutex::new(Seen::default()));
    let wake = Arc::new(tokio::sync::Notify::new());
    let (_tip_tx, tip_rx) = broadcast::channel(1);
    let task = tokio::spawn(handle_client(
        Duplex(
            OnceRead(line),
            Recorder {
                seen: Arc::clone(&seen),
                wake: Arc::clone(&wake),
            },
        ),
        "127.0.0.1:9".parse().unwrap(),
        Arc::new(q),
        Arc::new(cfg),
        Arc::new(params),
        tip_rx,
        None,
        ElectrumStop::new(false),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let notified = wake.notified();
        if seen.lock().unwrap().bytes.ends_with(b"]\n") {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "batch reply never finished");
        if tokio::time::timeout(left, notified).await.is_err() {
            panic!("batch reply never finished");
        }
    }
    task.abort();
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.bytes.iter().filter(|b| **b == b'\n').count(),
        1,
        "still one line"
    );
    let reply: Value = serde_json::from_slice(&seen.bytes).unwrap();
    let reply = reply.as_array().unwrap();
    assert_eq!(reply.len(), slots);
    assert!(reply.iter().enumerate().all(|(i, v)| v["id"] == i));
    assert!(
        seen.largest_write < 2 * 64 * 1024,
        "largest write {} bytes for a {} byte reply",
        seen.largest_write,
        seen.bytes.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(clippy::cognitive_complexity)] // one TCP client, static methods + no-hub fees
#[tokio::test]
async fn accept_client_ping_and_shutdown() {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let q = std::sync::Arc::new(q);
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let banner = cfg.banner.clone();
    let handle = run_electrum(cfg, q, params, tip_tx, None)
        .await
        .expect("listen");

    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let v = electrum_tcp_rpc(&mut stream, 1, "server.ping", json!([])).await;
    assert_eq!(v["id"], 1);
    assert!(v.get("result").is_some());
    assert!(
        v.get("chain_tip").is_none(),
        "ping must not grow chain_tip: {v}"
    );

    stream.write_all(b"\n").await.unwrap();
    stream
        .write_all(b"{\"id\":2,\"method\":\"server.ping\",}\n")
        .await
        .unwrap();
    let bad: Value = serde_json::from_str(&read_exact_line(&mut stream).await).unwrap();
    assert_eq!(
        bad,
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})
    );
    let pong = electrum_tcp_rpc(&mut stream, 2, "server.ping", json!([])).await;
    assert_eq!(pong, json!({"jsonrpc":"2.0","id":2,"result":null}));
    let v = electrum_tcp_rpc(&mut stream, 2, "server.version", json!([])).await;
    assert_eq!(v["id"], 2);
    let ver = v["result"].as_array().expect("version array");
    assert_eq!(ver.len(), 2);
    let cake_server = ver[0].as_str().expect("server.version[0] string");
    assert!(
        cake_server.to_ascii_lowercase().contains("rbitcoin"),
        "version[0] must still identify rbitcoin, got {cake_server:?}"
    );
    assert!(
        cake_server.contains(env!("CARGO_PKG_VERSION")),
        "version[0] must track workspace.package.version, got {cake_server:?}"
    );
    assert_wallets_read(cake_server);
    assert_eq!(ver[1], PROTOCOL_MAX);

    let features = electrum_tcp_rpc(&mut stream, 3, "server.features", json!([])).await;
    let features = &features["result"];
    assert_eq!(features["protocol_min"], PROTOCOL_MIN);
    assert_eq!(features["protocol_max"], PROTOCOL_MAX);
    assert_eq!(features["server_version"], ver[0]);
    assert_eq!(features["silent_payments"], json!([0]));
    assert_eq!(features["tweaks"], json!(true));
    assert_eq!(features["chain_tip"], json!(true));
    assert_eq!(features["asof"], json!(true));
    assert_eq!(features["asof_protocol"], PROTOCOL_ASOF);
    assert_eq!(features["hosts"], json!({}));

    let probe = electrum_tcp_rpc(
        &mut stream,
        4,
        "blockchain.tweaks.subscribe",
        json!([0, 1, false]),
    )
    .await;
    assert_eq!(probe["result"], json!({"0": {}}));
    let done: Value = serde_json::from_str(&read_exact_line(&mut stream).await).unwrap();
    assert_eq!(done["method"], "blockchain.tweaks.subscribe");
    assert_eq!(done["params"][0]["message"], "done");

    let banner_v = electrum_tcp_rpc(&mut stream, 5, "server.banner", json!([])).await;
    assert_eq!(banner_v["result"].as_str().unwrap(), banner);

    let don = electrum_tcp_rpc(&mut stream, 6, "server.donation_address", json!([])).await;
    assert!(don["result"].as_str().is_some());

    let peers = electrum_tcp_rpc(&mut stream, 7, "server.peers.subscribe", json!([])).await;
    assert_eq!(peers["result"], json!([]));

    let fee = electrum_tcp_rpc(&mut stream, 8, "blockchain.relayfee", json!([])).await;
    assert!(fee["result"].as_f64().is_some());

    let est = electrum_tcp_rpc(&mut stream, 9, "blockchain.estimatefee", json!([6])).await;
    assert_eq!(est["result"].as_f64(), Some(-1.0));

    let hist = electrum_tcp_rpc(&mut stream, 10, "mempool.get_fee_histogram", json!([])).await;
    assert_eq!(hist["result"], json!([]));

    let unk = electrum_tcp_rpc(&mut stream, 11, "no.such.method", json!([])).await;
    assert!(
        unk["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("unknown method")
            || unk["error"]
                .as_str()
                .unwrap_or("")
                .contains("unknown method"),
        "{unk}"
    );

    let bad = electrum_tcp_rpc(
        &mut stream,
        12,
        "blockchain.transaction.broadcast",
        json!(["zz"]),
    )
    .await;
    assert!(bad.get("error").is_some(), "{bad}");
    let bad_hex = electrum_tcp_rpc(
        &mut stream,
        13,
        "blockchain.transaction.broadcast",
        json!(["01000000000000000000"]),
    )
    .await;
    assert!(bad_hex.get("error").is_some(), "{bad_hex}");

    // The port is free once shutdown returns, so a restart can bind it.
    let addr = handle.local_addr;
    handle.shutdown().await;
    assert!(
        std::net::TcpListener::bind(addr).is_ok(),
        "electrum listener open after shutdown"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn electrum_fee_reports_follow_configured_and_enforced_floor() {
    let (dir, q) = tmp_store();
    let q = std::sync::Arc::new(q);
    let mp =
        MempoolHub::open_with_weight(dir.join("mempool"), std::sync::Arc::clone(&q), 0).unwrap();
    mp.set_min_relay_sat_kvb(2_500);
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, q, params, tip_tx, Some(mp))
        .await
        .expect("listen");
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let info = electrum_tcp_rpc(&mut stream, 1, "mempool.get_info", json!([])).await;
    let relay = electrum_tcp_rpc(&mut stream, 2, "blockchain.relayfee", json!([])).await;
    let sats = |v: &Value| (v.as_f64().unwrap() * 100_000_000.0).round() as u64;
    assert_eq!(sats(&info["result"]["minrelaytxfee"]), 2_500, "{info}");
    assert_eq!(sats(&info["result"]["mempoolminfee"]), 2_600, "{info}");
    assert_eq!(sats(&info["result"]["incrementalrelayfee"]), 100, "{info}");
    assert_eq!(sats(&relay["result"]), 2_600, "{relay}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// On a 1-worker runtime, ping must complete while another socket is inside
/// a real blocking store query (`blockchain.block.headers`).
#[tokio::test(flavor = "current_thread")]
async fn ping_overlaps_blocking_headers_on_one_worker() {
    use rbitcoin_consensus::Milestone;
    use rbitcoin_primitives::Height;
    use rbitcoin_query::accept_and_connect_block;
    use std::sync::atomic::AtomicU64;

    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    accept_and_connect_block(&q, &params, Height::GENESIS, &genesis, Milestone::NONE).unwrap();
    // Enough headers that a serial walk stays in-flight after ping is scheduled.
    let _ = rbitcoin_query::pad_empty_from(
        &q,
        &params,
        genesis.block_hash(),
        genesis.header.time,
        1,
        80,
        0,
    );
    let q = Arc::new(q);
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, q, params, tip_tx, None)
        .await
        .expect("listen");
    let addr = handle.local_addr;

    let mut a = TcpStream::connect(addr).await.unwrap();
    let mut b = TcpStream::connect(addr).await.unwrap();
    let a_started = Arc::new(AtomicBool::new(false));
    let a_done_us = Arc::new(AtomicU64::new(0));
    let b_done_us = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();

    let a_started_c = Arc::clone(&a_started);
    let a_done_c = Arc::clone(&a_done_us);
    let ha = tokio::spawn(async move {
        let req = json!({
            "jsonrpc":"2.0","id":10,
            "method":"blockchain.block.headers","params":[0, 2016]
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        a.write_all(line.as_bytes()).await.unwrap();
        a_started_c.store(true, Ordering::SeqCst);
        let mut reader = BufReader::new(a);
        let mut resp = String::new();
        reader.read_line(&mut resp).await.unwrap();
        a_done_c.store(t0.elapsed().as_micros() as u64, Ordering::SeqCst);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 10);
        assert!(v["result"]["count"].as_u64().unwrap() > 50);
    });

    let a_started_c = Arc::clone(&a_started);
    let b_done_c = Arc::clone(&b_done_us);
    let hb = tokio::spawn(async move {
        while !a_started_c.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let req = json!({"jsonrpc":"2.0","id":11,"method":"server.ping","params":[]});
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        b.write_all(line.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(b);
        let mut resp = String::new();
        reader.read_line(&mut resp).await.unwrap();
        b_done_c.store(t0.elapsed().as_micros() as u64, Ordering::SeqCst);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 11);
        assert!(v.get("result").is_some());
    });

    ha.await.expect("headers task");
    hb.await.expect("ping task");
    let a_done = a_done_us.load(Ordering::SeqCst);
    let b_done = b_done_us.load(Ordering::SeqCst);
    assert!(
        b_done < a_done,
        "ping finished at {b_done}µs, headers at {a_done}µs (expected overlap)"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn api_log_records_electrum_method() {
    let (dir, q) = tmp_store();
    let log_path = dir.join("api.jsonl");
    rbitcoin_log::init_api_log(&log_path).unwrap();
    let params = ChainParams::regtest();
    let q = std::sync::Arc::new(q);
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, q, params, tip_tx, None)
        .await
        .expect("listen");

    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let req = json!({"jsonrpc":"2.0","id":1,"method":"server.ping","params":[]});
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut resp = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .expect("timeout")
    .unwrap();
    handle.shutdown().await;
    rbitcoin_log::close_api_log();
    let body = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        body.contains("\"method\":\"server.ping\""),
        "api log missing ping: {body}"
    );
    assert!(body.contains("\"surface\":\"electrum\""));
    let _ = std::fs::remove_dir_all(&dir);
}

/// BCH-style optional `from_height`/`to_height` on get_history; status stays full.
#[test]
fn get_history_height_window_and_status_full() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..4u32 {
        let version = 1;
        let timestamp = h + 1;
        let bits = 0x207fffff;
        let nonce = h;
        let mut merkle = [0u8; 32];
        merkle[0..4].copy_from_slice(&h.to_le_bytes());
        merkle[5] = 0xee;
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
        let ta = TxApply {
            tx: TxRecord {
                txid,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord {
                prev_txid: [0u8; 32],
                parent: rbitcoin_store::InputParent::Coinbase,
                sequence: u32::MAX,
                script_sig: vec![h as u8],
                witness: vec![],
            }],
            outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
        };
        parent_hash = Some(header.hash);
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
    }

    let params = ChainParams::regtest();
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let mut header_sub = false;
    let mut sh_subs = HashSet::new();
    let sh = electrum_scripthash_hex(&[0x51]);

    let full = dispatch(
        "blockchain.scripthash.get_history",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    let full_arr = full.as_array().unwrap();
    assert_eq!(full_arr.len(), 4);

    // Inclusive from, exclusive to → heights 1 and 2 only.
    let windowed = dispatch(
        "blockchain.scripthash.get_history",
        &json!([sh, 1, 3]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    let w = windowed.as_array().unwrap();
    assert_eq!(w.len(), 2);
    assert_eq!(w[0]["height"], 1);
    assert_eq!(w[1]["height"], 2);
    assert!(w.len() < full_arr.len());

    // to_height=-1 is open upper (same as full for confirmed-only).
    let open_to = dispatch(
        "blockchain.scripthash.get_history",
        &json!([sh, 0, -1]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    assert_eq!(open_to.as_array().unwrap().len(), full_arr.len());

    // Subscribe status is always full history, independent of windowed calls.
    let status = dispatch(
        "blockchain.scripthash.subscribe",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    let status_s = status.as_str().unwrap();
    assert!(!status_s.is_empty());
    // Recompute status from full confirmed history and match.
    let sh_bytes = {
        let mut b = rbitcoin_primitives::hex_decode(&sh).unwrap();
        b.reverse();
        let mut out = [0u8; 32];
        out.copy_from_slice(&b);
        out
    };
    let full_hist = q.scripthash_history(&sh_bytes).unwrap();
    assert_eq!(full_hist.len(), 4);
    assert_eq!(scripthash_status(&full_hist), status_s);

    let tip_hash = parent_hash.unwrap();
    let mut txid = [0u8; 32];
    txid[0..4].copy_from_slice(&3u32.to_le_bytes());
    txid[31] = 0xcb;
    let asof = format!("asof:{}", hash_hex_rev(&tip_hash));
    let merkle_at = |height: u32, tag: &str| {
        electrum_at_chain_view(
            &q,
            "blockchain.transaction.get_merkle",
            &json!([txid_hex(&txid), height, tag]),
            |q, rpc_params, view, is_asof| {
                let mut pin_conn = ElectrumConn::new();
                dispatch_pinned(
                    "blockchain.transaction.get_merkle",
                    rpc_params,
                    q,
                    &cfg,
                    &params,
                    None,
                    &mut pin_conn,
                    view,
                    is_asof,
                )
            },
        )
        .0
    };
    let at_tip = merkle_at(3, &asof).expect("merkle at the pinned height");
    assert_eq!(at_tip["block_height"], 3);
    assert_eq!(at_tip["pos"], 0);
    let above = merkle_at(4, &asof).unwrap_err();
    assert!(above.contains("asof not on chain"), "{above}");

    let _ = prev;
    let _ = std::fs::remove_dir_all(&dir);
}

/// One block paying `0x51`, a pool, and a connection over a pipe that has
/// subscribed to that script and read its confirmed status. The tests pause
/// the clock, so a tick fires only when one advances it.
struct PoolPad {
    dir: rbitcoin_query::testutil::TempDir,
    q: Arc<Query>,
    mp: Arc<MempoolHub>,
    tip_tx: broadcast::Sender<TipNotify>,
    client: BufReader<tokio::io::DuplexStream>,
    sh: String,
}

impl PoolPad {
    async fn read(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.client.read_line(&mut line))
            .await
            .expect("a line from the server")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, id: u32, method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        self.client
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .unwrap();
        self.read().await
    }
}

/// A one-block store and a pool, served on a connection with no subscription.
async fn pool_pad() -> PoolPad {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let mut hash = [0u8; 32];
    hash[0] = 0x44;
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: hash,
        hash,
        size: 0,
        weight: 0,
    };
    let mut txid = [0u8; 32];
    txid[31] = 0xce;
    let ta = TxApply {
        tx: TxRecord {
            txid,
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
        outputs: vec![OutputRecord::unspent(1, vec![0x51])],
    };
    q.connect_block(Height(0), &header, &[ta]).unwrap();
    let params = ChainParams::regtest();
    let q = Arc::new(q);
    let mp = MempoolHub::open(dir.join("mempool"), Arc::clone(&q)).unwrap();
    let (tip_tx, tip_rx) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let (client, server) = tokio::io::duplex(1 << 16);
    tokio::spawn(handle_client(
        server,
        "127.0.0.1:9".parse().unwrap(),
        Arc::clone(&q),
        Arc::new(cfg),
        Arc::new(params),
        tip_rx,
        Some(Arc::clone(&mp)),
        ElectrumStop::new(false),
    ));
    PoolPad {
        dir,
        q,
        mp,
        tip_tx,
        client: BufReader::new(client),
        sh: electrum_scripthash_hex(&[0x51]),
    }
}

/// [`pool_pad`] with the block's output script subscribed.
async fn held_pool_pad() -> PoolPad {
    let mut pad = pool_pad().await;
    let sh = pad.sh.clone();
    let first = pad
        .call(1, "blockchain.scripthash.subscribe", json!([sh]))
        .await;
    assert!(!first["result"].as_str().unwrap().is_empty(), "{first}");
    pad
}

/// Replace block 0 with one paying `0x00`, so the scripthash watermark ends
/// where it started and only a read held back for the pool can notice.
fn replace_block(q: &Query) {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    q.disconnect_tip().unwrap();
    let mut hash = [0u8; 32];
    hash[0] = 0x45;
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 1,
        merkle_root: hash,
        hash,
        size: 0,
        weight: 0,
    };
    let mut txid = [0u8; 32];
    txid[0] = 0x98;
    txid[31] = 0xcf;
    let ta = TxApply {
        tx: TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![1],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(1, vec![0x00])],
    };
    q.connect_block(Height(0), &header, &[ta]).unwrap();
}

/// The server polls ticks and events before the next request, so a ping
/// answered with nothing ahead of it means those reads pushed nothing. Once
/// the pool is released, the next tick pushes the dropped script's null.
async fn ping_then_null_push(pad: &mut PoolPad, hold: rbitcoin_net::tx_relay::TemplateHold<'_>) {
    assert_eq!(pad.mp.pool_settle_gen() % 2, 1, "the hold is out");
    let ping = pad.call(9, "server.ping", json!([])).await;
    assert_eq!(ping["id"], 9, "pushed while the pool was held: {ping}");
    drop(hold);
    assert_eq!(pad.mp.pool_settle_gen() % 2, 0, "the hold is back");
    tokio::time::advance(Duration::from_millis(60)).await;
    let push = pad.read().await;
    assert_eq!(push["method"], "blockchain.scripthash.subscribe", "{push}");
    assert_eq!(push["params"][0], json!(pad.sh), "{push}");
    assert_eq!(push["params"][1], Value::Null, "{push}");
}

/// A reorg holds the template mark until its disconnected txs are back in
/// the pool. A tick that sees the watermark walk back in that hold pushes
/// nothing.
#[tokio::test(start_paused = true)]
async fn tick_waits_for_the_pool_after_a_reorg() {
    let mut pad = held_pool_pad().await;
    let mp = Arc::clone(&pad.mp);
    let hold = mp.template_hold();
    pad.q.disconnect_tip().unwrap();
    tokio::time::advance(Duration::from_millis(60)).await;
    ping_then_null_push(&mut pad, hold).await;
    let _ = std::fs::remove_dir_all(&pad.dir);
}

/// A tip event of an earlier reorg can be read while a later one holds the
/// pool. That restatus waits too, and the next tick reads it again.
#[tokio::test(start_paused = true)]
async fn tip_restatus_waits_for_a_held_pool() {
    let mut pad = held_pool_pad().await;
    let mp = Arc::clone(&pad.mp);
    let hold = mp.template_hold();
    replace_block(&pad.q);
    pad.tip_tx
        .send(TipNotify {
            height: 0,
            header_hex: "aa".repeat(80),
            reorg_from_height: Some(0),
        })
        .unwrap();
    ping_then_null_push(&mut pad, hold).await;
    let _ = std::fs::remove_dir_all(&pad.dir);
}

/// A pool admit announced while a reorg holds the pool restatuses on the
/// next tick.
#[tokio::test(start_paused = true)]
async fn announce_restatus_waits_for_a_held_pool() {
    use bitcoin::hashes::Hash;

    let mut pad = held_pool_pad().await;
    let mp = Arc::clone(&pad.mp);
    let hold = mp.template_hold();
    replace_block(&pad.q);
    let mut sh = rbitcoin_primitives::hex_decode(&pad.sh).unwrap();
    sh.reverse();
    pad.mp
        .announce
        .send(rbitcoin_net::tx_relay::MempoolAnnounce {
            txid: bitcoin::Txid::from_byte_array([0x45; 32]),
            replaced: vec![],
            replaced_scripthashes: vec![],
            scripthashes: vec![sh.try_into().unwrap()],
            outpoints: Default::default(),
        })
        .unwrap();
    ping_then_null_push(&mut pad, hold).await;
    let _ = std::fs::remove_dir_all(&pad.dir);
}

/// An announce pushes the status of each listed outpoint the connection
/// subscribed, on a connection with no scripthash subscription. A replaced
/// spend frees its outpoint, so the push says unspent.
#[tokio::test]
async fn announce_pushes_only_subscribed_outpoints() {
    use bitcoin::hashes::Hash;
    use bitcoin::{OutPoint, Txid};

    let mut pad = pool_pad().await;
    let watched = OutPoint {
        txid: Txid::from_byte_array([0x11; 32]),
        vout: 1,
    };
    let other = OutPoint {
        txid: Txid::from_byte_array([0x22; 32]),
        vout: 0,
    };
    let hex = watched.txid.to_string();
    let sub = pad
        .call(1, "blockchain.outpoint.subscribe", json!([hex.clone(), 1]))
        .await;
    assert_eq!(sub["result"]["spent"], false, "{sub}");
    let announce = |outpoints: Vec<OutPoint>| rbitcoin_net::tx_relay::MempoolAnnounce {
        txid: Txid::from_byte_array([0x45; 32]),
        replaced: vec![Txid::from_byte_array([0x46; 32])],
        replaced_scripthashes: vec![],
        scripthashes: vec![],
        outpoints: outpoints.into(),
    };

    pad.mp.announce.send(announce(vec![other])).unwrap();
    let ping = pad.call(2, "server.ping", json!([])).await;
    assert_eq!(
        ping["id"], 2,
        "pushed an outpoint nobody subscribed: {ping}"
    );

    pad.mp
        .announce
        .send(announce(vec![other, watched]))
        .unwrap();
    let push = pad.read().await;
    assert_eq!(
        push,
        json!({
            "jsonrpc": "2.0",
            "method": "blockchain.outpoint.subscribe",
            "params": [hex.clone(), 1, {"txid": hex, "vout": 1, "spent": false}],
        })
    );
    let ping = pad.call(3, "server.ping", json!([])).await;
    assert_eq!(ping["id"], 3, "one push per subscribed outpoint: {ping}");
    let _ = std::fs::remove_dir_all(&pad.dir);
}

#[tokio::test]
async fn chain_view_reorg_notifies_dropped_scripthash() {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let mut hash = [0u8; 32];
    hash[0] = 0x42;
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: hash,
        hash,
        size: 0,
        weight: 0,
    };
    let mut txid = [0u8; 32];
    txid[31] = 0xcb;
    let ta = TxApply {
        tx: TxRecord {
            txid,
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
        outputs: vec![OutputRecord::unspent(1, vec![0x51])],
    };
    let hfk0 = q.connect_block(Height(0), &header, &[ta]).unwrap();
    let sh = electrum_scripthash_hex(&[0x51]);

    let params = ChainParams::regtest();
    let q = std::sync::Arc::new(q);
    let (tip_tx, _) = broadcast::channel(2);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, std::sync::Arc::clone(&q), params, tip_tx.clone(), None)
        .await
        .unwrap();
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let mut line = serde_json::to_string(&json!({
        "jsonrpc":"2.0","id":1,"method":"blockchain.scripthash.subscribe","params":[sh]
    }))
    .unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut resp = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .unwrap()
    .unwrap();
    let first: Value = serde_json::from_str(&resp).unwrap();
    let status0 = first["result"].as_str().unwrap().to_string();
    assert!(!status0.is_empty());
    let _ = hfk0;
    // A never-used script answers null, and that null is remembered: the
    // reorg below restatuses every sub, and this one must not be pushed.
    let fresh = electrum_scripthash_hex(&[0x52]);
    let mut line = serde_json::to_string(&json!({
        "jsonrpc":"2.0","id":2,"method":"blockchain.scripthash.subscribe","params":[fresh]
    }))
    .unwrap();
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await.unwrap();
    resp.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .unwrap()
    .unwrap();
    let fresh_reply: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(fresh_reply["id"], 2, "{fresh_reply}");
    assert_eq!(fresh_reply["result"], Value::Null, "{fresh_reply}");

    q.disconnect_tip().unwrap();
    let mut hash_b = [0u8; 32];
    hash_b[0] = 0x43;
    let header_b = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 1,
        merkle_root: hash_b,
        hash: hash_b,
        size: 0,
        weight: 0,
    };
    let mut txid_b = [0u8; 32];
    txid_b[0] = 0x99;
    txid_b[31] = 0xcd;
    let ta_b = TxApply {
        tx: TxRecord {
            txid: txid_b,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![1],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(1, vec![0x00])],
    };
    q.connect_block(Height(0), &header_b, &[ta_b]).unwrap();
    tip_tx
        .send(TipNotify {
            height: 0,
            header_hex: "aa".repeat(80),
            reorg_from_height: Some(0),
        })
        .unwrap();
    resp.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .expect("reorg must restatus even when the new block misses the scripthash")
    .unwrap();
    let push: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        push["method"].as_str(),
        Some("blockchain.scripthash.subscribe")
    );
    assert_eq!(push["params"][0], json!(sh), "{push}");
    assert_eq!(
        push["params"][1],
        Value::Null,
        "the reorg dropped the script's only tx, so its status is null: {push}"
    );
    // The connection is serial: a redundant push for the fresh script would
    // arrive before this ping's reply.
    let mut line = serde_json::to_string(&json!({
        "jsonrpc":"2.0","id":3,"method":"server.ping","params":[]
    }))
    .unwrap();
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await.unwrap();
    resp.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .unwrap()
    .unwrap();
    let next: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        next["id"], 3,
        "no push for the still-empty fresh script: {next}"
    );

    // Header notify is written before the scripthash restatus, so it is the
    // barrier: the silence window starts after this tip handler has begun.
    let mut line = serde_json::to_string(&json!({
        "jsonrpc":"2.0","id":4,"method":"blockchain.headers.subscribe","params":[]
    }))
    .unwrap();
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await.unwrap();
    resp.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .unwrap()
    .unwrap();
    let hdr_sub: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(hdr_sub["id"], 4, "{hdr_sub}");
    assert!(hdr_sub.get("result").is_some(), "{hdr_sub}");

    tip_tx
        .send(TipNotify {
            height: 0,
            header_hex: "bb".repeat(80),
            reorg_from_height: Some(0),
        })
        .unwrap();
    resp.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut resp),
    )
    .await
    .expect("second tip must notify headers")
    .unwrap();
    let hdr_push: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        hdr_push["method"].as_str(),
        Some("blockchain.headers.subscribe"),
        "{hdr_push}"
    );
    resp.clear();
    let again = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        reader.read_line(&mut resp),
    )
    .await;
    assert!(
        again.is_err(),
        "emptied history stays null and is not pushed again: {resp}"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// DoS: line without newline beyond max must fail without allocating forever.
#[tokio::test]
async fn read_line_capped_rejects_oversize() {
    use std::io::Cursor;
    use tokio::io::BufReader;
    let mut r = BufReader::new(Cursor::new(vec![b'A'; 64]));
    let err = read_line_capped(&mut r, 32).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn read_line_capped_accepts_under_limit() {
    use std::io::Cursor;
    use tokio::io::BufReader;
    let mut r = BufReader::new(Cursor::new(b"{\"id\":1}\nnext\n".as_slice()));
    let line = read_line_capped(&mut r, 1024).await.unwrap().unwrap();
    assert_eq!(line, "{\"id\":1}");
    let line2 = read_line_capped(&mut r, 1024).await.unwrap().unwrap();
    assert_eq!(line2, "next");
}

/// A partial line with no newline is the bytes read, including a buffer that
/// fills the cap exactly. A shorter tail is still that tail.
#[tokio::test]
async fn read_line_capped_keeps_a_partial_line_through_the_cap() {
    use std::io::Cursor;
    use tokio::io::BufReader;
    let mut exact = BufReader::new(Cursor::new(b"AAAA".as_slice()));
    let line = read_line_capped(&mut exact, 4).await.unwrap().unwrap();
    assert_eq!(line, "AAAA");
    let mut short = BufReader::new(Cursor::new(b"AA".as_slice()));
    let line = read_line_capped(&mut short, 4).await.unwrap().unwrap();
    assert_eq!(line, "AA");
}

#[test]
fn broadcast_hex_cap_enforced() {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.max_broadcast_hex = 16;
    let mut header_sub = false;
    let mut sh_subs = HashSet::new();
    let err = dispatch(
        "blockchain.transaction.broadcast",
        &json!(["aa".repeat(20)]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(err.contains("too large"), "{err}");
    let pkg_err = dispatch(
        "blockchain.transaction.broadcast_package",
        &json!([["aa".repeat(20)]]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(pkg_err.contains("too large"), "{pkg_err}");
    let exact_hex = dispatch(
        "blockchain.transaction.broadcast",
        &json!(["00".repeat(8)]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(
        !exact_hex.contains("transaction hex too large"),
        "{exact_hex}"
    );
    cfg.max_broadcast_hex = 4;
    let exact_pkg = dispatch(
        "blockchain.transaction.broadcast_package",
        &json!([["0000"]]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(!exact_pkg.contains("package hex too large"), "{exact_pkg}");
    cfg.max_broadcast_hex = 8_000_004;
    let at_decode_cap = dispatch(
        "blockchain.transaction.broadcast",
        &json!(["00".repeat(4_000_000)]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(
        !at_decode_cap.contains("transaction too large"),
        "{at_decode_cap}"
    );
    let over_decode = dispatch(
        "blockchain.transaction.broadcast",
        &json!(["00".repeat(4_000_001)]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(
        over_decode.contains("transaction too large"),
        "{over_decode}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn serve_limits_public_proxy_defaults() {
    let lim = ServeLimits::for_public_proxy();
    assert_eq!(lim.max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(lim.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(
        lim.idle_timeout,
        Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
    );
    let params = ChainParams::regtest();
    let cfg = ElectrumConfig::for_params("0.0.0.0:50001".parse().unwrap(), &params);
    assert_eq!(cfg.limits, lim);
    assert_eq!(cfg.max_connections(), DEFAULT_MAX_CONNECTIONS);
    assert_eq!(cfg.max_line_bytes(), DEFAULT_MAX_LINE_BYTES);
}

#[tokio::test]
async fn tweaks_subscribe_dones_when_the_first_wave_finishes_count() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..5u32 {
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

    let params = ChainParams::regtest();
    let q = std::sync::Arc::new(q);
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, q, params, tip_tx, None)
        .await
        .expect("listen");

    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    async fn read_json(reader: &mut BufReader<&mut TcpStream>) -> Value {
        let mut resp = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut resp))
            .await
            .unwrap_or_else(|_| panic!("tweaks stream: timed out"))
            .unwrap();
        serde_json::from_str(&resp).unwrap_or_else(|e| panic!("tweaks stream parse {e}: {resp}"))
    }
    async fn subscribe(stream: &mut TcpStream, id: &str, start: u32, count: u32) {
        let req = json!({
            "jsonrpc":"2.0","id": id,
            "method":"blockchain.tweaks.subscribe",
            "params":[start, count, false]
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).await.unwrap();
    }

    subscribe(&mut stream, "scan", 1, 1).await;
    let mut reader = BufReader::new(&mut stream);

    let result = read_json(&mut reader).await;
    assert_eq!(result["id"], "scan");
    let map = result["result"].as_object().expect("result map");
    assert_eq!(map.len(), 1, "wave 0 result is one height, got {map:?}");
    assert!(map.contains_key("1"), "{map:?}");

    let done = read_json(&mut reader).await;
    assert_eq!(done["method"], "blockchain.tweaks.subscribe");
    assert_eq!(
        done["params"][0]["message"], "done",
        "count that fits in wave 0 must done, got {done}"
    );

    drop(reader);
    subscribe(&mut stream, "scan2", 2, 1).await;
    let mut reader = BufReader::new(&mut stream);
    let result2 = read_json(&mut reader).await;
    assert_eq!(result2["id"], "scan2");
    let map2 = result2["result"].as_object().expect("resubscribe result");
    assert!(
        map2.contains_key("2"),
        "Cake noData path resubscribes on the same socket, got {map2:?}"
    );

    let done2 = read_json(&mut reader).await;
    assert_eq!(done2["params"][0]["message"], "done");

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Chunk already due, heights still left: `done` with no further wave, and a
/// follow loop that was entered writes one wave and stops.
#[tokio::test]
async fn elapsed_tweaks_chunk_seals_while_heights_remain() {
    let (dir, q) = tmp_store();
    let q = std::sync::Arc::new(q);
    let chain = std::sync::Arc::new(ChainParams::mainnet());
    let started = Instant::now()
        .checked_sub(rbitcoin_electrum::tweaks::SUBSCRIBE_CHUNK)
        .expect("monotonic clock is older than one tweaks chunk");
    let peer: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let limits = rbitcoin_electrum::tweaks::subscribe_serve_limits(false);
    let idle = Duration::from_secs(1);
    let max_line = DEFAULT_MAX_LINE_BYTES;
    let last = 3_000u32;

    let (client, server) = tokio::io::duplex(1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server);
    let mut server_r = BufReader::new(server_r);
    let mut server_w = rbitcoin_electrum::server::ConnWrite::new(server_w, idle);
    let mut next = 0u32;
    tokio::time::timeout(
        Duration::from_secs(3),
        serve_tweaks_rest(
            &mut server_r,
            &mut server_w,
            &q,
            &chain,
            &peer,
            last,
            limits,
            0,
            max_line,
            started,
            &mut next,
        ),
    )
    .await
    .expect("wave-0 seal")
    .unwrap();
    drop(server_r);
    drop(server_w);
    let mut client = BufReader::new(client);
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    let done: Value = serde_json::from_str(&line).expect("done json");
    assert_eq!(
        done["params"][0]["message"], "done",
        "a due chunk writes done without another height wave"
    );
    line.clear();
    assert_eq!(
        client.read_line(&mut line).await.unwrap(),
        0,
        "no height line after done"
    );

    let (client, server) = tokio::io::duplex(1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server);
    let mut server_r = BufReader::new(server_r);
    let mut server_w = rbitcoin_electrum::server::ConnWrite::new(server_w, idle);
    let mut next = 0u32;
    tokio::time::timeout(
        Duration::from_secs(3),
        serve_tweaks_follow_waves(
            &mut server_r,
            &mut server_w,
            &q,
            &chain,
            &peer,
            last,
            limits,
            0,
            max_line,
            started,
            &mut next,
        ),
    )
    .await
    .expect("follow seal")
    .unwrap();
    drop(server_r);
    drop(server_w);
    let mut client = BufReader::new(client);
    line.clear();
    client.read_line(&mut line).await.unwrap();
    let wave: Value = serde_json::from_str(&line).expect("wave json");
    let keys = wave["params"][0].as_object().expect("height map").len();
    assert_eq!(keys, rbitcoin_electrum::tweaks::EMPTY_WAVE_HEIGHTS as usize);
    line.clear();
    assert_eq!(
        client.read_line(&mut line).await.unwrap(),
        0,
        "a due chunk does not write the next wave"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tweaks_subscribe_pre_taproot_collapses_empty_heights() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..5u32 {
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

    let params = ChainParams::mainnet();
    let q = std::sync::Arc::new(q);
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, q, params, tip_tx, None)
        .await
        .expect("listen");

    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let req = json!({
        "jsonrpc":"2.0","id":"scan",
        "method":"blockchain.tweaks.subscribe",
        "params":[0, 5, false]
    });
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut stream);

    async fn read_json(reader: &mut BufReader<&mut TcpStream>) -> Value {
        let mut resp = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut resp))
            .await
            .unwrap_or_else(|_| panic!("tweaks stream: timed out"))
            .unwrap();
        serde_json::from_str(&resp).unwrap_or_else(|e| panic!("tweaks stream parse {e}: {resp}"))
    }

    let result = read_json(&mut reader).await;
    assert_eq!(result["id"], "scan");
    let map = result["result"].as_object().expect("result map");
    assert_eq!(map.len(), 1, "probe/result stays one height, got {map:?}");
    assert!(map.contains_key("0"), "{map:?}");

    let n = read_json(&mut reader).await;
    assert_eq!(n["method"], "blockchain.tweaks.subscribe");
    let p = n["params"][0].as_object().expect("collapsed notify");
    assert_eq!(p.len(), 4, "heights 1..=4 in one notify, got {p:?}");
    for h in 1u32..=4 {
        assert!(p.contains_key(&h.to_string()), "{p:?}");
        assert!(p[&h.to_string()].as_object().unwrap().is_empty());
    }

    let done = read_json(&mut reader).await;
    assert_eq!(done["method"], "blockchain.tweaks.subscribe");
    assert_eq!(done["params"][0]["message"], "done");

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(clippy::cognitive_complexity)] // one store, every method's missing/wrong-type params
#[test]
fn dispatch_param_type_edges_and_subscribe_cap() {
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.max_scripthash_subs = 1;
    let mut header_sub = false;
    let mut sh_subs = HashSet::new();
    let sh = electrum_scripthash_hex(&[0x51]);
    let sh2 = electrum_scripthash_hex(&[0x52]);

    for (method, args, needle) in [
        ("blockchain.block.header", json!([]), "expected number"),
        ("blockchain.block.header", json!([true]), "expected number"),
        ("blockchain.block.headers", json!([]), "expected number"),
        (
            "blockchain.scripthash.get_history",
            json!([]),
            "expected string",
        ),
        (
            "blockchain.scripthash.get_history",
            json!([1]),
            "expected string",
        ),
        (
            "blockchain.scripthash.get_balance",
            json!(["aa"]),
            "failed to parse hex",
        ),
        (
            "blockchain.scripthash.listunspent",
            json!([true]),
            "expected string",
        ),
        (
            "blockchain.scripthash.subscribe",
            json!([]),
            "expected string",
        ),
        (
            "blockchain.scripthash.unsubscribe",
            json!(["zz".repeat(32)]),
            "failed to parse hex digit",
        ),
        (
            "blockchain.scripthash.get_mempool",
            json!({}),
            "expected string",
        ),
        (
            "blockchain.transaction.get",
            json!(["aabb"]),
            "failed to parse hex",
        ),
        (
            "blockchain.transaction.get_merkle",
            json!([sh]),
            "expected number",
        ),
        (
            "blockchain.transaction.broadcast",
            json!([]),
            "expected string",
        ),
        (
            "blockchain.transaction.broadcast",
            json!([1]),
            "expected string",
        ),
        (
            "blockchain.transaction.broadcast_package",
            json!([]),
            "expected array of hex txs",
        ),
        (
            "blockchain.transaction.broadcast_package",
            json!([1]),
            "expected array of hex txs",
        ),
        (
            "blockchain.transaction.broadcast_package",
            json!([[1]]),
            "tx must be hex",
        ),
        (
            "blockchain.transaction.broadcast_package",
            json!([["zz"]]),
            "invalid char, failed to create bytes from hex",
        ),
        (
            "blockchain.transaction.broadcast_package",
            json!([["00"]]),
            "IO error",
        ),
        (
            "blockchain.outpoint.get_status",
            json!([]),
            "expected string",
        ),
        (
            "blockchain.outpoint.subscribe",
            json!([sh]),
            "param 1 expected number",
        ),
        (
            "blockchain.outpoint.unsubscribe",
            json!([true, 0]),
            "expected string",
        ),
        (
            "blockchain.silentpayments.subscribe",
            json!([]),
            "expected string",
        ),
        (
            "blockchain.silentpayments.unsubscribe",
            json!([1, 2]),
            "expected string",
        ),
        (
            "blockchain.transaction.id_from_pos",
            json!([]),
            "expected number",
        ),
        ("no.such.method", json!([]), "unknown method"),
    ] {
        let err = dispatch(
            method,
            &args,
            &q,
            &cfg,
            &params,
            None,
            &mut header_sub,
            &mut sh_subs,
        )
        .unwrap_err();
        assert!(
            err.contains(needle),
            "{method} {args}: expected {needle:?} in {err}"
        );
    }

    let asof = format!("asof:{}", "ab".repeat(32));
    let err = dispatch(
        "blockchain.scripthash.get_balance",
        &json!([sh, asof]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(
        err.contains("asof not on chain"),
        "unknown asof hash: {err}"
    );

    dispatch(
        "blockchain.scripthash.subscribe",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    let err = dispatch(
        "blockchain.scripthash.subscribe",
        &json!([sh2]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap_err();
    assert!(err.contains("too many scripthash"), "{err}");
    let again = dispatch(
        "blockchain.scripthash.subscribe",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    assert_eq!(again, Value::Null, "empty store: no history, null status");
    let dropped = dispatch(
        "blockchain.scripthash.unsubscribe",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    assert_eq!(dropped, json!(true));
    let missing = dispatch(
        "blockchain.scripthash.unsubscribe",
        &json!([sh]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();
    assert_eq!(missing, json!(false));
    dispatch(
        "blockchain.scripthash.subscribe",
        &json!([sh2]),
        &q,
        &cfg,
        &params,
        None,
        &mut header_sub,
        &mut sh_subs,
    )
    .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sp_chunk_end_covers_one_height_and_stops_at_the_tip() {
    use rbitcoin_electrum::silent_scan::SP_SCAN_CHUNK;
    let chunk = SP_SCAN_CHUNK;
    assert_eq!(sp_chunk_end(0, chunk, chunk), chunk - 1);
    assert_eq!(sp_chunk_end(chunk, chunk, chunk), chunk);
    assert_eq!(sp_chunk_end(4, 4, chunk), 4);
    assert_eq!(sp_chunk_end(0, 1, chunk), 1);
    assert_eq!(sp_chunk_end(u32::MAX - 1, u32::MAX, chunk), u32::MAX);
    assert_eq!(sp_chunk_end(8, 100, 4), 11);
    assert_eq!(sp_chunk_end(8, 100, 1), 8);
}

/// One line from the server side of a duplex, as JSON.
async fn sp_next_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Value {
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(20), reader.read_line(&mut line))
        .await
        .expect("a line within the deadline")
        .expect("read");
    assert_ne!(n, 0, "the server closed the connection");
    serde_json::from_str(&line).expect("json line")
}

/// Notes of one scan through the first `progress` 1.0. Returns every history
/// entry and the progress values in order.
async fn sp_read_history<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    subscription: &Value,
) -> (Vec<Value>, Vec<f64>) {
    let mut history = Vec::new();
    let mut progress = Vec::new();
    loop {
        let note = sp_next_line(reader).await;
        assert_eq!(
            note["method"], "blockchain.silentpayments.subscribe",
            "{note}"
        );
        let params = note["params"]
            .as_array()
            .unwrap_or_else(|| panic!("Frigate notes carry positional params: {note}"));
        assert_eq!(params.len(), 3, "{note}");
        assert_eq!(
            &params[0], subscription,
            "the note names the subscribe result"
        );
        let p = params[1].as_f64().expect("progress");
        history.extend(params[2].as_array().expect("history").iter().cloned());
        progress.push(p);
        if p >= 1.0 {
            return (history, progress);
        }
    }
}

struct SpSession {
    client: tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    send: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tip_tx: broadcast::Sender<TipNotify>,
    _task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

/// The listener configuration of the silent-payment tests.
fn sp_cfg() -> ElectrumConfig {
    ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &ChainParams::regtest())
}

/// [`sp_cfg`] with history scan steps of four heights, for short chains.
fn sp_cfg_short_steps() -> ElectrumConfig {
    let mut cfg = sp_cfg();
    cfg.sp_scan_chunk = 4;
    cfg
}

fn sp_session(q: &Arc<Query>) -> SpSession {
    let params = ChainParams::regtest();
    let cfg = sp_cfg();
    let (client, server) = tokio::io::duplex(1 << 16);
    let (tip_tx, tip_rx) = broadcast::channel(8);
    let task = tokio::spawn(handle_client(
        server,
        "127.0.0.1:9".parse().unwrap(),
        Arc::clone(q),
        Arc::new(cfg),
        Arc::new(params),
        tip_rx,
        None,
        ElectrumStop::new(false),
    ));
    let (read, send) = tokio::io::split(client);
    SpSession {
        client: BufReader::new(read),
        send,
        tip_tx,
        _task: task,
    }
}

/// Sparrow's shape: the start is the wallet birthday in seconds, and the
/// payment is far more than one scan chunk below the tip. The scan runs
/// beside the session, so a request sent with the subscribe is answered
/// before the history instead of after it.
#[tokio::test]
async fn sp_timestamp_start_finds_a_payment_older_than_one_chunk() {
    use crate::silent_scan::{SpChain, SP_CHAIN_T0};
    use rbitcoin_electrum::silent_scan::SP_SCAN_CHUNK;

    // The older payment is more than 256 blocks below the tip.
    let last = SP_SCAN_CHUNK + 30;
    let chain = SpChain::new("el-sp-old", last, &[10, last - 1]);
    let mut s = sp_session(&chain.q);
    // Two hours after block 10 is twelve blocks later.
    let birthday = SP_CHAIN_T0 + (10 + 12) * 600;
    let lines = format!(
        "{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"blockchain.silentpayments.subscribe","params":chain.sub(json!(birthday))}),
        json!({"jsonrpc":"2.0","id":2,"method":"server.ping","params":[]}),
    );
    s.send.write_all(lines.as_bytes()).await.unwrap();

    let reply = sp_next_line(&mut s.client).await;
    assert_eq!(reply["id"], 1, "{reply}");
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"]["start_height"], 10, "{reply}");
    assert_eq!(reply["result"]["labels"], json!([0]), "{reply}");
    let pong = sp_next_line(&mut s.client).await;
    assert_eq!(
        pong["id"], 2,
        "a request must not wait for the history: {pong}"
    );

    let (history, progress) = sp_read_history(&mut s.client, &reply["result"]).await;
    let heights: Vec<u64> = history
        .iter()
        .map(|h| h["height"].as_u64().unwrap())
        .collect();
    assert_eq!(heights, vec![10, u64::from(last) - 1], "{history:?}");
    assert_eq!(history[0]["tx_hash"].as_str().unwrap().len(), 64);
    assert_eq!(history[0]["tweak_key"].as_str().unwrap().len(), 66);
    assert!(
        progress.len() >= 2,
        "history arrives in parts: {progress:?}"
    );
    let (done, parts) = progress.split_last().unwrap();
    assert_eq!(*done, 1.0);
    assert!(
        parts.iter().all(|p| *p > 0.0 && *p < 1.0) && parts.is_sorted(),
        "{progress:?}"
    );
}

/// A block connected while the history is scanned is part of the history.
/// Its tip event reached a session that had no live subscription yet.
#[tokio::test]
async fn sp_scan_reaches_blocks_connected_after_the_subscribe() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-grow", 10, &[8]);
    let params = Arc::new(ChainParams::regtest());
    let sub = rbitcoin_electrum::silent_scan::parse_sub(
        &chain.sub(json!(0)),
        bitcoin::Network::Regtest,
        Some(5),
        &no_times,
    )
    .unwrap();
    // The subscribe read height 5 as the tip. The chain is at 10 by now.
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let scan = tokio::spawn(run_sp_scan(
        Arc::clone(&chain.q),
        params,
        sub,
        0,
        5,
        rbitcoin_electrum::silent_scan::SP_SCAN_CHUNK,
        tx,
    ));
    let mut steps = Vec::new();
    while let Some(step) = rx.recv().await {
        steps.push(step.unwrap());
    }
    assert_eq!(scan.await.unwrap(), 2, "one chunk to 5, one to the tip");
    assert_eq!(steps[0].through, 5);
    assert!(steps[0].progress < 1.0, "height 5 is not the tip");
    assert!(steps[0].hits.is_empty());
    assert_eq!(steps[1].through, 10);
    assert_eq!(steps[1].progress, 1.0);
    assert_eq!(steps[1].hits.len(), 1);
    assert_eq!(steps[1].hits[0]["height"], 8);
}

/// Wait until nothing but `held` references the store: the scan task and
/// its blocking read are gone.
async fn sp_scan_task_gone(q: &Arc<Query>, held: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(q) > held {
        assert!(Instant::now() < deadline, "the scan task is still running");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A session that ends drops its scans: the task stops and its slot frees.
#[tokio::test(flavor = "current_thread")]
async fn sp_scan_stops_when_the_session_drops_it() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-drop", 15, &[]);
    let params = Arc::new(ChainParams::regtest());
    let cfg = sp_cfg_short_steps();
    let slots = cfg.sp_scan_slots.available_permits();
    let held = Arc::strong_count(&chain.q);
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!(0)),
        &mut conn,
        &cfg,
    )
    .await
    .unwrap();
    assert_eq!(cfg.sp_scan_slots.available_permits(), slots - 1);
    assert!(Arc::strong_count(&chain.q) > held, "the scan task runs");
    let (_, first) = next_sp_scan_step(&mut conn.sp_scans).await;
    let first = first.unwrap().unwrap();
    assert_eq!(first.through, 3, "one step of four heights");
    assert!(first.progress < 1.0);

    // The session ends with three steps of the scan left.
    drop(conn);
    assert_eq!(cfg.sp_scan_slots.available_permits(), slots);
    sp_scan_task_gone(&chain.q, held).await;
}

/// History scans are capped across connections. A subscribe past the cap is
/// refused in its reply, and a scan that ends frees its slot.
#[tokio::test(flavor = "current_thread")]
async fn sp_scans_are_capped_across_connections() {
    use crate::silent_scan::SpChain;

    assert_eq!(
        sp_cfg().sp_scan_slots.available_permits(),
        DEFAULT_MAX_SP_SCANS
    );
    let chain = SpChain::new("el-sp-slots", 15, &[]);
    let params = Arc::new(ChainParams::regtest());
    let mut cfg = sp_cfg_short_steps();
    cfg.sp_scan_slots = Arc::new(tokio::sync::Semaphore::new(2));
    let peer = "127.0.0.1:9".parse().unwrap();
    let subscribe = async |conn: &mut ElectrumConn, wallet: u8| -> Value {
        let mut body = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
        serve_sp_subscribe(
            &mut writer,
            &chain.q,
            &params,
            &peer,
            json!(1),
            &chain.sub_for(wallet, json!(0)),
            conn,
            &cfg,
        )
        .await
        .unwrap();
        serde_json::from_slice(&body).unwrap()
    };
    let (mut one, mut two) = (ElectrumConn::new(), ElectrumConn::new());
    assert!(subscribe(&mut one, 0).await.get("error").is_none());
    assert!(subscribe(&mut two, 1).await.get("error").is_none());
    // Both slots are held, by two connections.
    for conn in [&mut one, &mut two] {
        let refused = subscribe(conn, 2).await;
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("scan limit"),
            "{refused}"
        );
    }
    assert_eq!(one.sp_scans.len() + two.sp_scans.len(), 2);
    // Subscribing again replaces a scan in its own slot.
    assert!(subscribe(&mut one, 0).await.get("error").is_none());
    assert_eq!(one.sp_scans.len(), 1);
    assert_eq!(cfg.sp_scan_slots.available_permits(), 0);

    // A scan that reaches the tip frees its slot. The subscription stays.
    while !two.sp_scans.is_empty() {
        let mut sink = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut sink), Duration::from_secs(2));
        let (address, step) = next_sp_scan_step(&mut two.sp_scans).await;
        write_sp_scan_step(&mut writer, &chain.q, &params, &mut two, &address, step)
            .await
            .unwrap();
    }
    assert_eq!(two.sp_subs.len(), 1);
    assert_eq!(cfg.sp_scan_slots.available_permits(), 1);
    assert!(subscribe(&mut two, 2).await.get("error").is_none());

    // So does an unsubscribe.
    silentpayments_unsubscribe(&mut one, &chain.q, &params, &chain.sub_for(0, json!(0))).unwrap();
    assert_eq!(cfg.sp_scan_slots.available_permits(), 1);
}

/// Tip events are a hint. A session that missed one, or got it before the
/// history finished, still scans every height once.
#[tokio::test]
async fn sp_tip_note_covers_every_height_since_the_last_scan() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-tip", 3, &[]);
    let mut s = sp_session(&chain.q);
    let line = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"blockchain.silentpayments.subscribe","params":chain.sub(json!(0))}),
    );
    s.send.write_all(line.as_bytes()).await.unwrap();
    let reply = sp_next_line(&mut s.client).await;
    let (history, _) = sp_read_history(&mut s.client, &reply["result"]).await;
    assert!(history.is_empty(), "{history:?}");

    // Heights 4 and 5 connect. Only the event for 5 arrives.
    chain.extend(5, &[4]);
    let tip = |height: u32| TipNotify {
        height,
        header_hex: String::new(),
        reorg_from_height: None,
    };
    s.tip_tx.send(tip(5)).unwrap();
    let (history, progress) = sp_read_history(&mut s.client, &reply["result"]).await;
    assert_eq!(progress, vec![1.0]);
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["height"], 4);

    // A late event for a height already scanned sends nothing.
    s.tip_tx.send(tip(4)).unwrap();
    let ping = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":7,"method":"server.ping","params":[]})
    );
    s.send.write_all(ping.as_bytes()).await.unwrap();
    let pong = sp_next_line(&mut s.client).await;
    assert_eq!(pong["id"], 7, "height 4 was sent once already: {pong}");

    // A reorg replaces height 5 with a paying block's branch: rescan it.
    s.tip_tx
        .send(TipNotify {
            height: 5,
            header_hex: String::new(),
            reorg_from_height: Some(3),
        })
        .unwrap();
    let (history, _) = sp_read_history(&mut s.client, &reply["result"]).await;
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["height"], 4);
}

/// Notes until every subscription in `subs` has reached progress 1.0.
/// History by address.
async fn sp_read_all_done<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    subs: &[&Value],
) -> HashMap<String, Vec<Value>> {
    let mut history: HashMap<String, Vec<Value>> = HashMap::new();
    let mut done = HashSet::new();
    while done.len() < subs.len() {
        let note = sp_next_line(reader).await;
        let params = note["params"].as_array().expect("positional params");
        assert!(
            subs.contains(&&params[0]),
            "the note names a subscribe result: {note}"
        );
        let address = params[0]["address"].as_str().unwrap().to_string();
        history
            .entry(address.clone())
            .or_default()
            .extend(params[2].as_array().unwrap().iter().cloned());
        if params[1].as_f64().unwrap() >= 1.0 {
            done.insert(address);
        }
    }
    history
}

fn sp_subscribe_line(id: u32, params: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":id,"method":"blockchain.silentpayments.subscribe","params":params})
    )
}

/// Sparrow opens several wallets on one connection. Each address keeps its
/// own history scan and its own tip notes.
#[tokio::test]
async fn sp_two_addresses_share_a_connection() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-two", 0, &[]);
    chain.extend_paying(3, &[(1, 0), (2, 1)]);
    let mut s = sp_session(&chain.q);
    let lines = format!(
        "{}{}",
        sp_subscribe_line(1, chain.sub_for(0, json!(0))),
        sp_subscribe_line(2, chain.sub_for(1, json!(0))),
    );
    s.send.write_all(lines.as_bytes()).await.unwrap();
    let first = sp_next_line(&mut s.client).await;
    let second = sp_next_line(&mut s.client).await;
    assert_eq!((&first["id"], &second["id"]), (&json!(1), &json!(2)));
    let (a, b) = (&first["result"], &second["result"]);
    assert_ne!(a["address"], b["address"]);
    let history = sp_read_all_done(&mut s.client, &[a, b]).await;
    let heights = |sub: &Value, history: &HashMap<String, Vec<Value>>| -> Vec<u64> {
        history[sub["address"].as_str().unwrap()]
            .iter()
            .map(|h| h["height"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(heights(a, &history), vec![1]);
    assert_eq!(heights(b, &history), vec![2]);

    // Both are live: one block pays each.
    chain.extend_paying(5, &[(4, 1), (5, 0)]);
    s.tip_tx
        .send(TipNotify {
            height: 5,
            header_hex: String::new(),
            reorg_from_height: None,
        })
        .unwrap();
    let history = sp_read_all_done(&mut s.client, &[a, b]).await;
    assert_eq!(heights(a, &history), vec![5]);
    assert_eq!(heights(b, &history), vec![4]);

    // An unsubscribe ends one and leaves the other.
    let unsub = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":3,"method":"blockchain.silentpayments.unsubscribe","params":chain.sub_for(0, json!(0))})
    );
    s.send.write_all(unsub.as_bytes()).await.unwrap();
    let gone = sp_next_line(&mut s.client).await;
    assert_eq!(gone["result"], a["address"], "{gone}");
    chain.extend_paying(7, &[(6, 0), (7, 1)]);
    s.tip_tx
        .send(TipNotify {
            height: 7,
            header_hex: String::new(),
            reorg_from_height: None,
        })
        .unwrap();
    let history = sp_read_all_done(&mut s.client, &[b]).await;
    assert_eq!(heights(b, &history), vec![7]);
}

/// A connection's subscriptions are capped by the listener's configuration.
/// Subscribing again to an address it already has is not counted.
#[tokio::test]
async fn sp_subscriptions_are_capped_per_connection() {
    use crate::silent_scan::SpChain;

    let params = Arc::new(ChainParams::regtest());
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    assert_eq!(cfg.max_sp_subs, DEFAULT_MAX_SP_SUBS);
    assert_eq!(DEFAULT_MAX_SP_SUBS, 16);

    let chain = SpChain::new("el-sp-cap", 1, &[]);
    let mut conn = ElectrumConn::new();
    let peer = "127.0.0.1:9".parse().unwrap();
    let subscribe = async |conn: &mut ElectrumConn, wallet: u8, max_subs: usize| -> Value {
        let mut cfg = cfg.clone();
        cfg.max_sp_subs = max_subs;
        let mut body = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
        serve_sp_subscribe(
            &mut writer,
            &chain.q,
            &params,
            &peer,
            json!(1),
            &chain.sub_for(wallet, json!(0)),
            conn,
            &cfg,
        )
        .await
        .unwrap();
        serde_json::from_slice(&body).unwrap()
    };
    for wallet in 0..3 {
        let reply = subscribe(&mut conn, wallet, 3).await;
        assert!(reply.get("error").is_none(), "{reply}");
    }
    assert_eq!(conn.sp_scans.len(), 3);
    let over = subscribe(&mut conn, 3, 3).await;
    assert!(
        over["error"]["message"]
            .as_str()
            .unwrap()
            .contains("subscription limit reached (3)"),
        "{over}"
    );
    assert_eq!(conn.sp_scans.len(), 3);
    let again = subscribe(&mut conn, 0, 3).await;
    assert!(again.get("error").is_none(), "{again}");
    assert_eq!(conn.sp_scans.len(), 3);
    // A higher cap on the listener admits the address.
    let raised = subscribe(&mut conn, 3, 4).await;
    assert!(raised.get("error").is_none(), "{raised}");
    assert_eq!(conn.sp_scans.len(), 4);
}

/// Frigate: a subscribe to an address the connection has must not narrow
/// its coverage. The earlier start stays and is the one returned.
#[tokio::test]
async fn sp_resubscribe_keeps_the_earlier_start() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-widen", 9, &[]);
    let params = Arc::new(ChainParams::regtest());
    let mut conn = ElectrumConn::new();
    let peer = "127.0.0.1:9".parse().unwrap();
    let subscribe = async |conn: &mut ElectrumConn, wallet: u8, start: u32| -> Value {
        let mut body = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
        serve_sp_subscribe(
            &mut writer,
            &chain.q,
            &params,
            &peer,
            json!(1),
            &chain.sub_for(wallet, json!(start)),
            conn,
            &sp_cfg(),
        )
        .await
        .unwrap();
        serde_json::from_slice(&body).unwrap()
    };
    let first = subscribe(&mut conn, 0, 4).await;
    assert_eq!(first["result"]["start_height"], 4);
    // Still scanning: a later start keeps 4.
    let later = subscribe(&mut conn, 0, 7).await;
    assert_eq!(later["result"]["start_height"], 4, "{later}");
    assert_eq!(conn.sp_scans[0].sub.start, 4);
    // Another address is not bound by it.
    let other = subscribe(&mut conn, 1, 7).await;
    assert_eq!(other["result"]["start_height"], 7, "{other}");
    // An earlier start widens.
    let wider = subscribe(&mut conn, 0, 2).await;
    assert_eq!(wider["result"]["start_height"], 2, "{wider}");

    // Live: the history is sent, and a later start still keeps 2.
    while conn.sp_subs.len() < 2 {
        let mut sink = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut sink), Duration::from_secs(2));
        let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
        write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, step)
            .await
            .unwrap();
    }
    let live = subscribe(&mut conn, 0, 9).await;
    assert_eq!(live["result"]["start_height"], 2, "{live}");
}

/// `"FROM-TO"` scans that range and no further. The subscription is then
/// live for blocks connected after it.
#[tokio::test]
async fn sp_range_start_scans_only_the_range() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-range", 9, &[2, 5, 8]);
    let mut s = sp_session(&chain.q);
    s.send
        .write_all(sp_subscribe_line(1, chain.sub(json!("1-5"))).as_bytes())
        .await
        .unwrap();
    let reply = sp_next_line(&mut s.client).await;
    assert_eq!(reply["result"]["start_height"], 1, "{reply}");
    let (history, _) = sp_read_history(&mut s.client, &reply["result"]).await;
    let heights: Vec<u64> = history
        .iter()
        .map(|h| h["height"].as_u64().unwrap())
        .collect();
    assert_eq!(heights, vec![2, 5], "height 8 is past the range");

    chain.extend(10, &[10]);
    s.tip_tx
        .send(TipNotify {
            height: 10,
            header_hex: String::new(),
            reorg_from_height: None,
        })
        .unwrap();
    let (history, _) = sp_read_history(&mut s.client, &reply["result"]).await;
    let heights: Vec<u64> = history
        .iter()
        .map(|h| h["height"].as_u64().unwrap())
        .collect();
    assert_eq!(heights, vec![10], "heights 6 to 9 were not asked for");
}

/// A reorg while the history is scanned replaces heights the scan already
/// read. The scan goes back to the first of them.
#[tokio::test(flavor = "current_thread")]
async fn sp_reorg_restarts_a_running_scan_above_the_fork() {
    use crate::silent_scan::SpChain;

    let last = 15;
    let chain = SpChain::new("el-sp-reorg", last, &[3]);
    let params = Arc::new(ChainParams::regtest());
    let cfg = sp_cfg_short_steps();
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!(0)),
        &mut conn,
        &cfg,
    )
    .await
    .unwrap();
    let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
    assert_eq!(step.as_ref().unwrap().as_ref().unwrap().through, 3);
    write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, step)
        .await
        .unwrap();

    // Heights 2 and up are replaced. The first step, through 3, is sent.
    let reorg = TipNotify {
        height: last,
        header_hex: String::new(),
        reorg_from_height: Some(1),
    };
    restart_sp_scans_after_reorg(&mut conn.sp_scans, &chain.q, &params, &reorg);
    let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
    let again = step.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(again.through, 5, "the scan restarts at 2");
    assert_eq!(again.hits.len(), 1, "height 3 is scanned again");
    assert!(again.progress > 0.0 && again.progress < 1.0);
    write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, step)
        .await
        .unwrap();

    // A fork above what was sent restarts at the first height not sent.
    let shallow = TipNotify {
        height: last,
        header_hex: String::new(),
        reorg_from_height: Some(last - 1),
    };
    let sent = again_through(&conn);
    restart_sp_scans_after_reorg(&mut conn.sp_scans, &chain.q, &params, &shallow);
    let (_, step) = next_sp_scan_step(&mut conn.sp_scans).await;
    assert_eq!(step.unwrap().unwrap().through, sp_chunk_end(sent, last, 4));

    // Not a reorg: the scan is left alone.
    let plain = TipNotify {
        height: last,
        header_hex: String::new(),
        reorg_from_height: None,
    };
    let before = again_through(&conn);
    restart_sp_scans_after_reorg(&mut conn.sp_scans, &chain.q, &params, &plain);
    assert_eq!(again_through(&conn), before);
}

/// First height the one running scan has not sent.
fn again_through(conn: &ElectrumConn) -> u32 {
    conn.sp_scans[0].next
}

/// The end of a range is coverage too: a re-subscribe keeps the later end,
/// and an open subscription stays open.
#[tokio::test]
async fn sp_resubscribe_keeps_the_later_end() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-end", 9, &[]);
    let params = Arc::new(ChainParams::regtest());
    let mut conn = ElectrumConn::new();
    let peer = "127.0.0.1:9".parse().unwrap();
    let subscribe = async |conn: &mut ElectrumConn, wallet: u8, start: Value| {
        let mut body = Vec::new();
        let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
        serve_sp_subscribe(
            &mut writer,
            &chain.q,
            &params,
            &peer,
            json!(1),
            &chain.sub_for(wallet, start),
            conn,
            &sp_cfg(),
        )
        .await
        .unwrap();
    };
    let held = |conn: &ElectrumConn, wallet: usize| {
        let sub = &conn.sp_scans[wallet].sub;
        (sub.start, sub.end)
    };
    // Open, then a range: still open.
    subscribe(&mut conn, 0, json!(2)).await;
    subscribe(&mut conn, 0, json!("5-7")).await;
    assert_eq!(held(&conn, 0), (2, None));
    // Two ranges: the earlier start and the later end.
    subscribe(&mut conn, 1, json!("4-6")).await;
    subscribe(&mut conn, 1, json!("1-3")).await;
    assert_eq!(held(&conn, 1), (1, Some(6)));
    subscribe(&mut conn, 1, json!("5-8")).await;
    assert_eq!(held(&conn, 1), (1, Some(8)));
    // A range, then open: open.
    subscribe(&mut conn, 1, json!(9)).await;
    assert_eq!(held(&conn, 1), (1, None));
}

/// An unsubscribe stops the history scan it names.
#[tokio::test]
async fn sp_unsubscribe_drops_a_running_scan() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-unsub", 3, &[]);
    let params = Arc::new(ChainParams::regtest());
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    let mut conn = ElectrumConn::new();
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!(0)),
        &mut conn,
        &sp_cfg(),
    )
    .await
    .unwrap();
    assert_eq!(conn.sp_scans.len(), 1, "the subscribe starts a scan");
    let other = json!([
        "1f694e068028a717f8af6b9411f9a133dd3565258714cc226594b34db90c1f2c",
        chain.spend_hex
    ]);
    silentpayments_unsubscribe(&mut conn, &chain.q, &params, &other).unwrap();
    assert_eq!(conn.sp_scans.len(), 1, "another key leaves the scan");
    let keys = json!([chain.scan_hex, chain.spend_hex]);
    silentpayments_unsubscribe(&mut conn, &chain.q, &params, &keys).unwrap();
    assert!(conn.sp_scans.is_empty(), "the unsubscribe names this scan");
}

/// A tip still pushes a payment after an unsubscribe for a different address.
/// The matching unsubscribe stops that push.
#[tokio::test]
async fn silentpayments_unsubscribe_stops_only_the_matching_address() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-unsub-tip", 0, &[0]);
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let handle = run_electrum(cfg, Arc::clone(&chain.q), params, tip_tx.clone(), None)
        .await
        .expect("listen");
    let scan_hex = chain.scan_hex.clone();
    let spend_hex = chain.spend_hex.clone();
    // Another spend key: a different address under the same scan key.
    let wrong_hex = "025cc9856d6f8375350e123978daac200c260cb5b5ae83106cab90484dcd8fcf36";

    async fn subscribe_sp(stream: &mut TcpStream, scan: &str, spend: &str) {
        let res = electrum_tcp_rpc(
            stream,
            1,
            "blockchain.silentpayments.subscribe",
            json!([scan, spend, 0]),
        )
        .await;
        assert!(res.get("error").is_none(), "{res}");
        let hist = read_exact_line(stream).await;
        assert!(hist.contains("tx_hash"), "{hist}");
    }

    let mut mismatch = TcpStream::connect(handle.local_addr).await.unwrap();
    subscribe_sp(&mut mismatch, &scan_hex, &spend_hex).await;
    let other = electrum_tcp_rpc(
        &mut mismatch,
        2,
        "blockchain.silentpayments.unsubscribe",
        json!([scan_hex, wrong_hex]),
    )
    .await;
    assert!(other.get("error").is_none(), "{other}");
    let headers =
        electrum_tcp_rpc(&mut mismatch, 3, "blockchain.headers.subscribe", json!([])).await;
    assert!(headers["result"]["hex"].is_string(), "{headers}");

    let mut matched = TcpStream::connect(handle.local_addr).await.unwrap();
    subscribe_sp(&mut matched, &scan_hex, &spend_hex).await;
    let gone = electrum_tcp_rpc(
        &mut matched,
        2,
        "blockchain.silentpayments.unsubscribe",
        json!([scan_hex, spend_hex]),
    )
    .await;
    assert!(gone.get("error").is_none(), "{gone}");
    let headers =
        electrum_tcp_rpc(&mut matched, 3, "blockchain.headers.subscribe", json!([])).await;
    assert!(headers["result"]["hex"].is_string(), "{headers}");

    // A block that pays the address connects.
    chain.extend(1, &[1]);
    tip_tx
        .send(TipNotify {
            height: 1,
            header_hex: "aa".into(),
            reorg_from_height: None,
        })
        .unwrap();

    let mismatch_hdr = read_exact_line(&mut mismatch).await;
    assert!(
        mismatch_hdr.contains("blockchain.headers.subscribe"),
        "{mismatch_hdr}"
    );
    let still = read_exact_line(&mut mismatch).await;
    assert!(
        still.contains("blockchain.silentpayments.subscribe") && still.contains("tx_hash"),
        "{still}"
    );
    let matched_hdr = read_exact_line(&mut matched).await;
    assert!(
        matched_hdr.contains("blockchain.headers.subscribe"),
        "{matched_hdr}"
    );
    let mut byte = [0u8; 1];
    let extra = tokio::time::timeout(Duration::from_millis(300), matched.read(&mut byte)).await;
    assert!(extra.is_err(), "matching unsubscribe still pushed a note");
    let pong = electrum_tcp_rpc(&mut matched, 4, "server.ping", json!([])).await;
    assert!(pong.get("error").is_none(), "{pong}");

    handle.shutdown().await;
}

/// Two pruned blocks with no sealed tweak rows: each height reads from its
/// keep file. The file of height 1 is gone.
fn sp_pruned_chain_missing_height_1() -> (rbitcoin_query::testutil::TempDir, Arc<Query>) {
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = tmp_store();
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let mut script = vec![0x51, 0x20];
    script.extend_from_slice(&[0x11; 32]);
    let mut prev = (Fk::NULL, None::<[u8; 32]>);
    for h in 0..2u32 {
        let mut merkle = [0x44; 32];
        merkle[0] = h as u8;
        let hash = match prev.1 {
            None => merkle,
            Some(parent) => {
                rbitcoin_store::block_header_hash(1, &parent, &merkle, 1, 0x207fffff, 0)
            }
        };
        let header = HeaderRecord {
            prev_fk: prev.0,
            version: 1,
            timestamp: 1,
            bits: 0x207fffff,
            nonce: 0,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        };
        let fk = q
            .connect_block(
                Height(h),
                &header,
                &[TxApply {
                    tx: TxRecord {
                        txid: merkle,
                        version: 1,
                        locktime: 0,
                        input_count: 1,
                        output_count: 1,
                    },
                    inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                    outputs: vec![OutputRecord::unspent(50_0000_0000, script.clone())],
                }],
            )
            .unwrap();
        prev = (fk, Some(hash));
    }
    std::fs::remove_file(q.store().path().join("seqsigwit.window/1.bin")).unwrap();
    q.clear_seqsigwit_ram_window();
    (dir, Arc::new(q))
}

const SP_MISS_SCAN: &str = "0f694e068028a717f8af6b9411f9a133dd3565258714cc226594b34db90c1f2c";
const SP_MISS_SPEND: &str = "025cc9856d6f8375350e123978daac200c260cb5b5ae83106cab90484dcd8fcf36";

/// A pruned height whose keep file is gone is an error. The scan must not
/// report progress 1.0 with an empty history, or the client walks past it.
#[tokio::test(flavor = "current_thread")]
async fn pruned_scan_miss_is_not_an_empty_history() {
    use std::io::Cursor;
    use std::net::SocketAddr;

    let (dir, q) = sp_pruned_chain_missing_height_1();
    let chain = Arc::new(ChainParams::regtest());
    let params = json!([SP_MISS_SCAN, SP_MISS_SPEND, 0]);
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(Cursor::new(&mut body), Duration::from_secs(2));
    let mut conn = ElectrumConn::new();
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    serve_sp_subscribe(
        &mut writer,
        &q,
        &chain,
        &peer,
        json!(1),
        &params,
        &mut conn,
        &sp_cfg(),
    )
    .await
    .unwrap();
    assert_eq!(conn.sp_scans.len(), 1, "height 0 reads, so the scan starts");
    let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
    write_sp_scan_step(&mut writer, &q, &chain, &mut conn, &address, step)
        .await
        .unwrap();
    assert!(conn.sp_scans.is_empty(), "a failed scan is over");
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("window miss"),
        "a keep-file miss has to be an error, got {text}"
    );
    assert!(
        !text.contains("1.0"),
        "the miss must not finish the scan, got {text}"
    );
    assert!(
        conn.sp_subs.is_empty(),
        "a failed scan is not a live subscription"
    );
    let sub = rbitcoin_electrum::silent_scan::parse_sub(
        &params,
        bitcoin::Network::Regtest,
        Some(1),
        &no_times,
    )
    .unwrap();
    let mut tip_body = Vec::new();
    let mut tip_writer = ConnWrite::new(Cursor::new(&mut tip_body), Duration::from_secs(2));
    let read = emit_sp_tip(&mut tip_writer, &q, &chain, &sub, 1, 1)
        .await
        .unwrap();
    assert!(!read, "the miss is reported to the caller");
    let tip_text = String::from_utf8(tip_body).unwrap();
    assert!(
        tip_text.contains("window miss"),
        "a tip note must not hide the miss, got {tip_text}"
    );
    assert!(
        !tip_text.contains("1.0"),
        "a tip miss must not report the height scanned, got {tip_text}"
    );
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A live subscription whose new heights cannot be read ends, as a failed
/// history scan does. Left live, it would mark those heights scanned.
#[tokio::test(flavor = "current_thread")]
async fn sp_tip_miss_ends_the_subscription() {
    use std::io::Cursor;

    let (dir, q) = sp_pruned_chain_missing_height_1();
    let chain = Arc::new(ChainParams::regtest());
    let sub = |start: Value| {
        rbitcoin_electrum::silent_scan::parse_sub(
            &json!([SP_MISS_SCAN, SP_MISS_SPEND, start]),
            bitcoin::Network::Regtest,
            Some(1),
            &no_times,
        )
        .unwrap()
    };
    let tip = TipNotify {
        height: 1,
        header_hex: String::new(),
        reorg_from_height: None,
    };

    // A tip event: height 1 has no keep file.
    let mut conn = ElectrumConn::new();
    conn.sp_subs.push(SpLive {
        sub: sub(json!(0)),
        through: 0,
    });
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(Cursor::new(&mut body), Duration::from_secs(2));
    emit_sp_tips(&mut writer, &q, &chain, &mut conn, &tip)
        .await
        .unwrap();
    assert!(conn.sp_subs.is_empty(), "the subscription ended");
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("window miss"), "{text}");

    // A height already scanned is not read again, and the subscription stays.
    let mut conn = ElectrumConn::new();
    conn.sp_subs.push(SpLive {
        sub: sub(json!(0)),
        through: 1,
    });
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(Cursor::new(&mut body), Duration::from_secs(2));
    emit_sp_tips(&mut writer, &q, &chain, &mut conn, &tip)
        .await
        .unwrap();
    assert_eq!(conn.sp_subs.len(), 1);
    assert_eq!(conn.sp_subs[0].through, 1);
    assert!(body.is_empty());

    // The catch-up after a history scan: the scan ended at 0, the tip is 1.
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &q,
        &chain,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &json!([SP_MISS_SCAN, SP_MISS_SPEND, "0-0"]),
        &mut conn,
        &sp_cfg(),
    )
    .await
    .unwrap();
    conn.sp_scans[0].sub.end = None;
    let address = conn.sp_scans[0].sub.address.clone();
    let at_zero = SpScanStep {
        hits: Vec::new(),
        through: 0,
        progress: 1.0,
    };
    write_sp_scan_step(
        &mut writer,
        &q,
        &chain,
        &mut conn,
        &address,
        Some(Ok(at_zero)),
    )
    .await
    .unwrap();
    assert!(conn.sp_scans.is_empty());
    assert!(conn.sp_subs.is_empty(), "height 1 was not read");
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("window miss"), "{text}");
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reorg may leave the tip below the last height scanned. The heights
/// above the new tip are replaced, so the blocks that connect there later
/// are scanned.
#[tokio::test(flavor = "current_thread")]
async fn sp_reorg_to_a_lower_tip_rewinds_the_scan() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-lower", 10, &[9]);
    let params = ChainParams::regtest();
    let sub = rbitcoin_electrum::silent_scan::parse_sub(
        &chain.sub(json!(0)),
        bitcoin::Network::Regtest,
        Some(10),
        &no_times,
    )
    .unwrap();
    let mut conn = ElectrumConn::new();
    conn.sp_subs.push(SpLive { sub, through: 10 });
    let tip = |height: u32, reorg_from_height: Option<u32>| TipNotify {
        height,
        header_hex: String::new(),
        reorg_from_height,
    };
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));

    // A late event for a lower height is not a reorg: nothing rewinds.
    emit_sp_tips(&mut writer, &chain.q, &params, &mut conn, &tip(5, None))
        .await
        .unwrap();
    assert_eq!(conn.sp_subs[0].through, 10);

    // The chain forks at 6 and its new tip is 8.
    emit_sp_tips(&mut writer, &chain.q, &params, &mut conn, &tip(8, Some(6)))
        .await
        .unwrap();
    assert_eq!(conn.sp_subs[0].through, 8, "heights 9 and 10 are gone");

    // Blocks connect at 9 and 10 again. Height 9 pays.
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    emit_sp_tips(&mut writer, &chain.q, &params, &mut conn, &tip(10, None))
        .await
        .unwrap();
    assert_eq!(conn.sp_subs[0].through, 10);
    let note: Value = serde_json::from_slice(&body).expect("a note for height 9");
    assert_eq!(note["params"][2][0]["height"], 9, "{note}");
}

/// A reorg may leave the tip below the end of a closed range that is still
/// being scanned. The scan ends at the new tip, and the subscription does
/// not count the heights above it as scanned.
#[tokio::test(flavor = "current_thread")]
async fn sp_reorg_below_a_range_end_stops_the_scan_at_the_tip() {
    use crate::silent_scan::SpChain;

    let chain = SpChain::new("el-sp-range-reorg", 15, &[]);
    let params = Arc::new(ChainParams::regtest());
    let cfg = sp_cfg_short_steps();
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!("0-15")),
        &mut conn,
        &cfg,
    )
    .await
    .unwrap();
    // The chain forks at 4 and its new tip is 9, below the range end.
    let reorg = TipNotify {
        height: 9,
        header_hex: String::new(),
        reorg_from_height: Some(4),
    };
    restart_sp_scans_after_reorg(&mut conn.sp_scans, &chain.q, &params, &reorg);
    let last = loop {
        let (_, step) = next_sp_scan_step(&mut conn.sp_scans).await;
        let step = step.unwrap().unwrap();
        assert!(step.through <= 9, "scanned {} past the tip", step.through);
        if step.progress >= 1.0 {
            break step.through;
        }
    };
    assert_eq!(last, 9, "the scan ends at the new tip");

    // A finished step above the chain tip leaves the subscription at the tip.
    let address = conn.sp_scans[0].sub.address.clone();
    let past_tip = SpScanStep {
        hits: Vec::new(),
        through: 20,
        progress: 1.0,
    };
    write_sp_scan_step(
        &mut writer,
        &chain.q,
        &params,
        &mut conn,
        &address,
        Some(Ok(past_tip)),
    )
    .await
    .unwrap();
    assert_eq!(conn.sp_subs[0].through, 15, "the chain tip is 15");
}

/// A range scan cut short by a reorg ends at the new tip. A block that
/// connects inside the range before that scan finishes had no live
/// subscription to notify, so the finishing step reads it.
#[tokio::test(flavor = "current_thread")]
async fn sp_range_scan_reads_a_block_connected_inside_the_range() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-range-grow", 9, &[]);
    let params = Arc::new(ChainParams::regtest());
    let cfg = sp_cfg_short_steps();
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!("0-9")),
        &mut conn,
        &cfg,
    )
    .await
    .unwrap();
    // The range ran to 15 before a reorg left the tip at 9.
    conn.sp_scans[0].sub.end = Some(15);
    let reorg = TipNotify {
        height: 9,
        header_hex: String::new(),
        reorg_from_height: Some(4),
    };
    restart_sp_scans_after_reorg(&mut conn.sp_scans, &chain.q, &params, &reorg);
    let (address, done) = loop {
        let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
        if step.as_ref().unwrap().as_ref().unwrap().progress >= 1.0 {
            break (address, step);
        }
        write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, step)
            .await
            .unwrap();
    };
    assert_eq!(done.as_ref().unwrap().as_ref().unwrap().through, 9);

    // Heights 10 to 12 connect before the session takes the last step. 11
    // pays, and it is inside the range.
    chain.extend(12, &[11]);
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, done)
        .await
        .unwrap();
    assert_eq!(conn.sp_subs[0].through, 12);
    let text = String::from_utf8(body).unwrap();
    let heights: Vec<u64> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .flat_map(|note| note["params"][2].as_array().cloned().unwrap_or_default())
        .map(|entry| entry["height"].as_u64().unwrap())
        .collect();
    assert_eq!(heights, vec![11], "{text}");
}

/// Past the end of a range nothing is read: the client did not ask for it.
#[tokio::test(flavor = "current_thread")]
async fn sp_range_scan_does_not_read_past_its_end() {
    use crate::silent_scan::SpChain;

    let mut chain = SpChain::new("el-sp-range-end", 9, &[]);
    let params = Arc::new(ChainParams::regtest());
    let cfg = sp_cfg_short_steps();
    let mut conn = ElectrumConn::new();
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    serve_sp_subscribe(
        &mut writer,
        &chain.q,
        &params,
        &"127.0.0.1:9".parse().unwrap(),
        json!(1),
        &chain.sub(json!("0-9")),
        &mut conn,
        &cfg,
    )
    .await
    .unwrap();
    chain.extend(12, &[11]);
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(std::io::Cursor::new(&mut body), Duration::from_secs(2));
    while !conn.sp_scans.is_empty() {
        let (address, step) = next_sp_scan_step(&mut conn.sp_scans).await;
        write_sp_scan_step(&mut writer, &chain.q, &params, &mut conn, &address, step)
            .await
            .unwrap();
    }
    assert_eq!(conn.sp_subs[0].through, 12);
    let text = String::from_utf8(body).unwrap();
    assert!(
        !text.contains("tx_hash"),
        "height 11 is past the range: {text}"
    );
}

/// A start the node cannot read is refused in the reply to the subscribe.
/// A JSON-RPC notification has no error member for it.
#[tokio::test(flavor = "current_thread")]
async fn sp_subscribe_refuses_a_start_it_cannot_read() {
    use std::io::Cursor;

    let (dir, q) = sp_pruned_chain_missing_height_1();
    let chain = Arc::new(ChainParams::regtest());
    let mut body = Vec::new();
    let mut writer = ConnWrite::new(Cursor::new(&mut body), Duration::from_secs(2));
    let mut conn = ElectrumConn::new();
    serve_sp_subscribe(
        &mut writer,
        &q,
        &chain,
        &"127.0.0.1:9".parse().unwrap(),
        json!(7),
        &json!([SP_MISS_SCAN, SP_MISS_SPEND, 1]),
        &mut conn,
        &sp_cfg(),
    )
    .await
    .unwrap();
    let reply: Value = serde_json::from_slice(&body).expect("one reply line");
    assert_eq!(reply["id"], 7, "{reply}");
    assert!(reply.get("result").is_none(), "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("window miss"),
        "{reply}"
    );
    assert!(conn.sp_scans.is_empty(), "no scan starts: {reply}");
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A client that never reads must not hold the request. The write itself
/// returns; the connection task drops on that error.
#[tokio::test]
async fn stalled_electrum_write_is_closed() {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    struct PendingWriter;
    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let mut writer = PendingWriter;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        rbitcoin_electrum::server::write_line(
            &mut writer,
            json!({"jsonrpc":"2.0","id": 1, "result": null}),
            std::time::Duration::from_millis(40),
        ),
    )
    .await
    .expect("a stalled write returns before the test bound");
    let err = result.expect_err("stalled write is an error");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

/// A scripthash status push that the client never reads ends the connection.
/// The push is the only write after subscribe: the chain height does not move,
/// so the periodic rescan has nothing new to send.
#[tokio::test]
async fn stalled_scripthash_push_ends_the_connection() {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    struct ScriptedRead {
        data: Vec<u8>,
        pos: usize,
    }
    impl AsyncRead for ScriptedRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.pos >= this.data.len() {
                return Poll::Pending;
            }
            let n = (this.data.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.data[this.pos..this.pos + n]);
            this.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    struct StallWriter {
        got: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        stall: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wake: std::sync::Arc<tokio::sync::Notify>,
    }
    impl AsyncWrite for StallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.stall.load(std::sync::atomic::Ordering::SeqCst) {
                return Poll::Pending;
            }
            self.got.lock().unwrap().extend_from_slice(buf);
            self.wake.notify_waiters();
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct Pair<R, W> {
        reader: R,
        writer: W,
    }
    impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for Pair<R, W> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
        }
    }
    impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for Pair<R, W> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().writer).poll_flush(cx)
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
        }
    }

    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height::GENESIS,
        &genesis,
        rbitcoin_consensus::Milestone::NONE,
    )
    .unwrap();
    let last = 1 + params.coinbase_maturity();
    let (_tip, _time, cbs) = rbitcoin_query::pad_empty_from(
        &q,
        &params,
        genesis.block_hash(),
        genesis.header.time,
        1,
        last,
        1,
    );
    let q = std::sync::Arc::new(q);
    let hub = std::sync::Arc::new(
        MempoolHub::open(dir.join("mempool"), std::sync::Arc::clone(&q)).unwrap(),
    );
    hub.set_relay_enabled(true);

    let sh = electrum_scripthash_hex(&[0x51]);
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"blockchain.scripthash.subscribe\",\"params\":[\"{sh}\"]}}\n"
    );
    let stall = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let stream = Pair {
        reader: ScriptedRead {
            data: line.into_bytes(),
            pos: 0,
        },
        writer: StallWriter {
            got: std::sync::Arc::clone(&got),
            stall: std::sync::Arc::clone(&stall),
            wake: std::sync::Arc::clone(&wake),
        },
    };
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.limits.idle_timeout = std::time::Duration::from_millis(80);
    let (tip_tx, tip_rx) = tokio::sync::broadcast::channel(1);
    let stop = ElectrumStop::new(false);
    let task = tokio::spawn(handle_client(
        stream,
        "127.0.0.1:9".parse().unwrap(),
        std::sync::Arc::clone(&q),
        std::sync::Arc::new(cfg),
        std::sync::Arc::new(params),
        tip_rx,
        Some(std::sync::Arc::clone(&hub)),
        stop,
    ));
    let _tip_tx = tip_tx;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let notified = wake.notified();
        let buf = got.lock().unwrap().clone();
        if String::from_utf8_lossy(&buf).contains("\"id\":1") {
            break;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "subscribe reply must land before the push stalls"
        );
        if tokio::time::timeout(left, notified).await.is_err() {
            panic!("subscribe reply must land before the push stalls");
        }
    }
    stall.store(true, std::sync::atomic::Ordering::SeqCst);

    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint {
                txid: cbs[0],
                vout: 0,
            },
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(50_0000_0000 - 10_000),
            script_pubkey: bitcoin::ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    hub.accept_tx(&tx)
        .expect("mempool spend of the subscribed script");
    let joined = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("stalled push ends the connection task");
    let err = joined
        .expect("connection task")
        .expect_err("stalled push is an error");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        err.to_string().contains("write timeout"),
        "the push timeout must end the task, got {err}"
    );
    let _ = dir;
}

/// `store(true)` ends a parked client without waiting out the stop poll.
#[tokio::test]
async fn client_stop_wakes_without_the_hundred_ms_poll() {
    tokio::time::pause();
    let (client, server) = tokio::io::duplex(64);
    let stop = ElectrumStop::new(false);
    let params = ChainParams::regtest();
    let (dir, q) = tmp_store();
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    let (_tip_tx, tip_rx) = broadcast::channel(1);
    let task = tokio::spawn(handle_client(
        server,
        "127.0.0.1:9".parse().unwrap(),
        Arc::new(q),
        Arc::new(cfg),
        Arc::new(params),
        tip_rx,
        None,
        Arc::clone(&stop),
    ));
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    stop.store(true);
    tokio::time::resume();
    let joined = tokio::time::timeout(Duration::from_secs(2), task).await;
    joined
        .expect("client did not stop")
        .expect("client task")
        .expect("stop ends the client");
    drop(client);
    let _ = dir;
}

/// `shutdown` stores the stop bit before the accept task drops. The listener
/// gauge is off, not down.
#[tokio::test]
async fn shutdown_marks_the_listener_off() {
    use std::sync::atomic::AtomicU8;
    let state = Arc::new(AtomicU8::new(LISTEN_DOWN));
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let mut cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    cfg.listen_state = Some(Arc::clone(&state));
    let (tip_tx, _) = broadcast::channel(1);
    let handle = run_electrum(cfg, Arc::new(q), params, tip_tx, None)
        .await
        .expect("listen");
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.load(Ordering::SeqCst) != LISTEN_UP {
        assert!(Instant::now() < deadline, "listener did not come up");
        tokio::task::yield_now().await;
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    handle.shutdown().await;
    assert_eq!(state.load(Ordering::SeqCst), LISTEN_OFF);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Mempool verbose fields match the encoded transaction.
#[test]
fn mempool_verbose_tx_fields_match_the_encoded_tx() {
    use bitcoin::hashes::Hash;
    let (_dir, query) = tmp_store();
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig: bitcoin::ScriptBuf::from_bytes(vec![0x01, 0x02]),
            sequence: bitcoin::Sequence::MAX,
            witness: bitcoin::Witness::from_slice(&[b"abcd" as &[u8]]),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(50),
            script_pubkey: bitcoin::ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    let raw = bitcoin::consensus::serialize(&tx);
    let txid = tx.compute_txid().to_byte_array();
    let network = bitcoin::Network::Regtest;
    let from_tx = verbose_tx_from_tx(&query, &tx, &txid, None, network);
    assert_eq!(from_tx["hex"], rbitcoin_primitives::hex_encode(&raw));
    assert_eq!(from_tx["confirmations"], 0);
    assert_eq!(from_tx["vin"][0]["sequence"], 0xffff_ffffu32);
    assert_eq!(
        from_tx["hash"].as_str().unwrap(),
        hash_hex_rev(&tx.compute_wtxid().to_byte_array())
    );
    assert_ne!(from_tx["hash"], from_tx["txid"]);

    let prev = bitcoin::Txid::from_byte_array([0x11; 32]);
    let witnessed = bitcoin::TxIn {
        previous_output: bitcoin::OutPoint {
            txid: prev,
            vout: 1,
        },
        script_sig: bitcoin::ScriptBuf::new(),
        sequence: bitcoin::Sequence::MAX,
        witness: bitcoin::Witness::from_slice(&[b"abcd" as &[u8]]),
    };
    let vin = verbose_vin(&witnessed);
    assert_eq!(vin["txinwitness"][0], "61626364");
    let bare = bitcoin::TxIn {
        previous_output: bitcoin::OutPoint {
            txid: prev,
            vout: 1,
        },
        script_sig: bitcoin::ScriptBuf::new(),
        sequence: bitcoin::Sequence::MAX,
        witness: bitcoin::Witness::new(),
    };
    assert!(verbose_vin(&bare).get("txinwitness").is_none());
}

mod surface_tests;

/// Any client can open connections, so connect and disconnect log at DEBUG.
/// At INFO they let an unauthenticated client fill the operator's log.
#[tokio::test]
async fn connect_and_disconnect_log_below_info() {
    use rbitcoin_log::Level;
    let (dir, q) = tmp_store();
    let params = ChainParams::regtest();
    let (tip_tx, _) = broadcast::channel(4);
    let cfg = ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &params);
    rbitcoin_log::capture_logs(true);
    let handle = run_electrum(cfg, Arc::new(q), params, tip_tx, None)
        .await
        .expect("listen");
    let mut stream = TcpStream::connect(handle.local_addr).await.unwrap();
    let pong = electrum_tcp_rpc(&mut stream, 1, "server.ping", json!([])).await;
    assert!(pong.get("error").is_none(), "{pong}");
    drop(stream);
    let mut logs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !logs
        .iter()
        .any(|(_, m): &(Level, String)| m.starts_with("electrum: disconnect"))
    {
        assert!(Instant::now() < deadline, "no disconnect line: {logs:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
        logs.extend(rbitcoin_log::take_logs());
    }
    rbitcoin_log::capture_logs(false);
    handle.shutdown().await;
    let lines: Vec<_> = logs
        .iter()
        .filter(|(_, m)| {
            m.starts_with("electrum: connect") || m.starts_with("electrum: disconnect")
        })
        .collect();
    assert_eq!(lines.len(), 2, "{logs:?}");
    assert!(lines.iter().all(|(l, _)| *l == Level::Debug), "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A client causes any socket error by closing or resetting, so that end logs
/// at DEBUG. A dispatch that panicked is this side's fault and stays at WARN.
#[tokio::test]
async fn panicked_electrum_disconnect_logs_at_warn() {
    use rbitcoin_log::Level;
    let peer: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let joined = tokio::spawn(async { panic!("handler bug") })
        .await
        .expect_err("the task panicked");
    rbitcoin_log::capture_logs(true);
    rbitcoin_electrum::server::log_disconnect(peer, &Ok(()));
    rbitcoin_electrum::server::log_disconnect(peer, &Err(std::io::ErrorKind::UnexpectedEof.into()));
    rbitcoin_electrum::server::log_disconnect(peer, &Err(std::io::Error::other(joined)));
    let levels: Vec<Level> = rbitcoin_log::take_logs()
        .into_iter()
        .map(|(l, _)| l)
        .collect();
    rbitcoin_log::capture_logs(false);
    assert_eq!(levels, [Level::Debug, Level::Debug, Level::Warn]);
}
