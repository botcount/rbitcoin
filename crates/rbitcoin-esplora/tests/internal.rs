use bitcoin::absolute::LockTime;
use bitcoin::Txid;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
use rbitcoin_consensus::{ChainParams, Milestone};
use rbitcoin_esplora::internal::*;
use rbitcoin_esplora::server::{run_esplora, EsploraConfig, EsploraListen};
use rbitcoin_net::MempoolHub;
use serde_json::{json, Value};

use rbitcoin_primitives::Height;
use rbitcoin_query::accept_and_connect_block;
use rbitcoin_query::Query;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[test]
fn join_cached_objects_skips_none() {
    assert_eq!(
        join_cached_objects(&[Some("{\"a\":1}"), None, Some("{\"b\":2}")]),
        "[{\"a\":1},{\"b\":2}]"
    );
    assert_eq!(join_cached_objects(&[None, None]), "[]");
}

#[test]
fn cache_json_str_does_not_store_empty_on_err() {
    let slot = std::sync::OnceLock::<Box<str>>::new();
    assert!(cache_json_str(&slot, || Err::<String, ()>(())).is_none());
    assert!(slot.get().is_none());
    assert_eq!(
        cache_json_str(&slot, || Ok::<_, ()>("{\"ok\":true}".into())),
        Some("{\"ok\":true}")
    );
    assert_eq!(slot.get().map(|s| s.as_ref()), Some("{\"ok\":true}"));
}

fn spend_true(cb: Txid, fee: u64, spk: ScriptBuf) -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint { txid: cb, vout: 0 },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000 - fee),
            script_pubkey: spk,
        }],
    }
}

struct Pad {
    dir: rbitcoin_query::testutil::TempDir,
    q: Arc<Query>,
    hub: Arc<MempoolHub>,
    cbs: Vec<Txid>,
    genesis: bitcoin::BlockHash,
}

fn pad_hub(label: &str, n_cb: u32) -> Pad {
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled(label);
    let params = ChainParams::regtest();
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    accept_and_connect_block(&q, &params, Height::GENESIS, &genesis, Milestone::NONE).unwrap();
    let (_tip, _time, cbs) = rbitcoin_query::pad_empty_from(
        &q,
        &params,
        genesis.block_hash(),
        genesis.header.time,
        1,
        100 + n_cb,
        n_cb,
    );
    let q = Arc::new(q);
    let mp = dir.join("mp");
    std::fs::create_dir_all(&mp).unwrap();
    let hub = MempoolHub::open(&mp, Arc::clone(&q)).unwrap();
    hub.set_relay_enabled(true);
    Pad {
        dir,
        q,
        hub,
        cbs,
        genesis: genesis.block_hash(),
    }
}

fn parse_http(buf: &[u8]) -> (u16, String) {
    let text = String::from_utf8_lossy(buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string();
    (status, body)
}

async fn http_get_unix(sock: &std::path::Path, path: &str) -> (u16, String) {
    let mut stream = UnixStream::connect(sock).await.expect("unix connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: api\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    parse_http(&buf)
}

async fn http_post_unix(sock: &std::path::Path, path: &str, body: &[u8]) -> (u16, String) {
    let mut stream = UnixStream::connect(sock).await.expect("unix connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: api\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    parse_http(&buf)
}

async fn run_unix(
    dir: &std::path::Path,
    q: Arc<Query>,
    mp: Option<Arc<MempoolHub>>,
) -> (rbitcoin_esplora::server::EsploraHandle, std::path::PathBuf) {
    let sock = dir.join("esplora.sock");
    let cfg =
        EsploraConfig::with_listen(EsploraListen::Unix(sock.clone()), bitcoin::Network::Regtest);
    let handle = run_esplora(cfg, q, mp).await.unwrap();
    (handle, sock)
}

/// `maxfeerate` rejects a fee over the cap, allows zero and 1 BTC/kvB, and
/// rejects a negative rate.
async fn maxfeerate_query_cases(sock: &std::path::Path, body: &[u8]) {
    let (st, resp) = http_post_unix(sock, "/txs/test?maxfeerate=0.00000001", body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr[0]["allowed"], false, "{resp}");
    assert_eq!(arr[0]["reject-reason"], "max-fee-exceeded");
    let (st, resp) = http_post_unix(sock, "/txs/test?maxfeerate=0", body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr[0]["allowed"], true, "{resp}");
    let (st, resp) = http_post_unix(sock, "/txs/test?maxfeerate=-1", body).await;
    assert_eq!(st, 400, "{resp}");
    assert!(resp.contains("Invalid maxfeerate"), "{resp}");
    let (st, resp) = http_post_unix(sock, "/txs/test?maxfeerate=1", body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr[0]["allowed"], true, "{resp}");
}

/// `/txs/test` dry-runs; `POST /tx` admits. `GET /broadcast` is gone.
async fn test_then_broadcast(sock: &std::path::Path, pad: &Pad, txs: &[Transaction]) {
    use bitcoin::consensus::encode::serialize_hex;

    let hex = serialize_hex(&txs[0]);
    let (st, body) = http_get_unix(sock, "/broadcast").await;
    assert_eq!(st, 404, "{body}");

    let body = serde_json::to_vec(&json!([&hex])).unwrap();
    let (st, resp) = http_post_unix(sock, "/txs/test", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr[0]["allowed"], true, "{resp}");
    assert!(
        !pad.hub.contains(&txs[0].compute_txid()),
        "test must not admit"
    );
    maxfeerate_query_cases(sock, &body).await;
    let edge: Vec<String> = (0..25).map(|_| hex.clone()).collect();
    let edge_body = serde_json::to_vec(&edge).unwrap();
    let (st, resp) = http_post_unix(sock, "/txs/test", &edge_body).await;
    assert_eq!(st, 200, "{resp}");
    assert!(resp.contains("package-contains-duplicates"), "{resp}");
    let too: Vec<String> = (0..26).map(|_| hex.clone()).collect();
    let body = serde_json::to_vec(&too).unwrap();
    let (st, resp) = http_post_unix(sock, "/txs/test", &body).await;
    assert_eq!(st, 400, "{resp}");
    assert!(resp.contains("Exceeded maximum of 25"), "{resp}");

    let (st, resp) = http_get_unix(sock, &format!("/broadcast?tx={hex}")).await;
    assert_eq!(st, 404, "{resp}");
    let (st, resp) = http_post_unix(sock, "/tx", hex.as_bytes()).await;
    assert_eq!(st, 200, "{resp}");
    assert_eq!(resp, txs[0].compute_txid().to_string());
    for tx in &txs[1..] {
        let (st, resp) = http_post_unix(sock, "/tx", serialize_hex(tx).as_bytes()).await;
        assert_eq!(st, 200, "{resp}");
    }
    assert!(txs.iter().all(|t| pad.hub.contains(&t.compute_txid())));
}

/// `/mempool` totals, then the public txid pages.
async fn mempool_totals_and_txid_pages(sock: &std::path::Path, pad: &Pad) {
    let live = pad.hub.list_live_meta();
    let expect_fee: u64 = live.iter().map(|(_, f, _)| *f).sum();
    let expect_vsize: u64 = live.iter().map(|(_, _, w)| w.saturating_add(3) / 4).sum();
    let (st, body) = http_get_unix(sock, "/mempool").await;
    assert_eq!(st, 200, "{body}");
    let mem: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(mem["count"].as_u64(), Some(3), "{body}");
    assert_eq!(mem["total_fee"].as_u64(), Some(expect_fee), "{body}");
    assert_eq!(mem["vsize"].as_u64(), Some(expect_vsize), "{body}");
    let hist = pad.hub.fee_histogram();
    let got = mem["fee_histogram"].as_array().expect("histogram");
    assert_eq!(got.len(), hist.len(), "{body}");
    assert!(!hist.is_empty(), "{body}");
    for (row, (rate_kvb, vs)) in got.iter().zip(hist) {
        let rate = row[0].as_f64().unwrap();
        assert!(rate > 0.0, "{row}");
        let expect = (rate_kvb as f64) / 1000.0;
        assert!((rate - expect).abs() < 1e-9, "{row} vs {rate_kvb}");
        assert_eq!(row[1].as_u64(), Some(vs), "{row}");
    }

    let (st, p1) = http_get_unix(sock, "/mempool/txids/page?max_txs=1").await;
    assert_eq!(st, 200, "{p1}");
    let a1: Vec<String> = serde_json::from_str(&p1).unwrap();
    assert_eq!(a1.len(), 1);
    let (st, p2) = http_get_unix(sock, &format!("/mempool/txids/page/{}?max_txs=10", a1[0])).await;
    assert_eq!(st, 200, "{p2}");
    let a2: Vec<String> = serde_json::from_str(&p2).unwrap();
    assert_eq!(a2.len(), 2, "{p2}");
    assert!(
        !a2.contains(&a1[0]),
        "cursor page must omit {}: {p2}",
        a1[0]
    );
}

async fn internal_mempool_pages(sock: &std::path::Path) {
    let (st, all) = http_get_unix(sock, "/internal/mempool/txs/all").await;
    assert_eq!(st, 200, "{all}");
    let all_arr: Vec<Value> = serde_json::from_str(&all).unwrap();
    assert_eq!(all_arr.len(), 3);
    let (st, p1) = http_get_unix(sock, "/internal/mempool/txs?max_txs=2").await;
    assert_eq!(st, 200, "{p1}");
    let a1: Vec<Value> = serde_json::from_str(&p1).unwrap();
    assert_eq!(a1.len(), 2);
    assert!(a1.iter().all(|v| v.get("txid").is_some()), "{p1}");
    let last = a1[1]["txid"].as_str().unwrap();
    let (st, p2) = http_get_unix(sock, &format!("/internal/mempool/txs/{last}?max_txs=2")).await;
    assert_eq!(st, 200, "{p2}");
    let a2: Vec<Value> = serde_json::from_str(&p2).unwrap();
    assert_eq!(a2.len(), 1);
    let last2 = a2[0]["txid"].as_str().unwrap();
    let (st, p3) = http_get_unix(sock, &format!("/internal/mempool/txs/{last2}?max_txs=2")).await;
    assert_eq!(st, 200, "{p3}");
    let a3: Vec<Value> = serde_json::from_str(&p3).unwrap();
    assert!(a3.is_empty(), "{p3}");
}

/// The public dialect `/block/:hash/txs/summary` is on the unix listener too.
async fn unix_block_txs_summary(sock: &std::path::Path, pad: &Pad) {
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    let cb = genesis.txdata[0].compute_txid().to_string();
    let (st, sum) = http_get_unix(sock, &format!("/block/{}/txs/summary", pad.genesis)).await;
    assert_eq!(st, 200, "{sum}");
    let sum: Vec<Value> = serde_json::from_str(&sum).unwrap();
    assert_eq!(sum.len(), 1, "genesis coinbase row");
    assert_eq!(sum[0][0].as_str(), Some(cb.as_str()));
    assert_eq!(sum[0][1], 0, "coinbase fee");
    assert_eq!(sum[0][3], 50_0000_0000u64, "coinbase value");
}

async fn internal_txs_and_block_txs(sock: &std::path::Path, pad: &Pad, mem: &str) {
    let conf = pad.cbs[3].to_string();
    let unknown = "00".repeat(32);
    let body = serde_json::to_vec(&json!([conf, mem, unknown])).unwrap();
    let (st, resp) = http_post_unix(sock, "/internal/txs", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 2, "{resp}");
    let (st, resp) = http_post_unix(sock, "/internal/txs", br#"["zz"]"#).await;
    assert_eq!(st, 400, "{resp}");
    let (st, resp) = http_post_unix(sock, "/internal/txs", b"[]").await;
    assert_eq!(st, 200, "{resp}");
    assert_eq!(resp, "[]");

    let body = serde_json::to_vec(&json!([conf, mem])).unwrap();
    let (st, resp) = http_post_unix(sock, "/internal/mempool/txs", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 1, "confirmed omitted: {resp}");

    let g = pad.genesis.to_string();
    let (st, body) = http_get_unix(sock, &format!("/internal/block/{g}/txs")).await;
    assert_eq!(st, 200, "{body}");
    let arr: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(arr.len(), 1, "genesis coinbase");
    assert_eq!(arr[0]["vin"][0]["is_coinbase"], true);
    let (st, ids) = http_get_unix(sock, &format!("/block/{g}/txids")).await;
    assert_eq!(st, 200, "{ids}");
    let txids: Vec<String> = serde_json::from_str(&ids).unwrap();
    assert_eq!(arr.len(), txids.len());
    assert_eq!(arr[0]["txid"].as_str(), Some(txids[0].as_str()));
    let (st, pubp) = http_get_unix(sock, &format!("/block/{g}/txs")).await;
    assert_eq!(st, 200, "{pubp}");
    let pub_arr: Vec<Value> = serde_json::from_str(&pubp).unwrap();
    assert_eq!(pub_arr.len(), 1);
    let (st, miss) = http_get_unix(sock, &format!("/internal/block/{}/txs", "11".repeat(32))).await;
    assert_eq!(st, 404, "{miss}");
}

/// Mempool spends answer the internal bulk outspends and the public `GET /txs/outspends`.
async fn outspends_by_txid_outpoint_and_query(sock: &std::path::Path, pad: &Pad) {
    let spent = pad.cbs[0].to_string();
    let unknown = "ff".repeat(32);
    let body = serde_json::to_vec(&json!([spent, unknown])).unwrap();
    let (st, resp) = http_post_unix(sock, "/internal/txs/outspends/by-txid", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 2, "same-length slots");
    assert_eq!(arr[0][0]["spent"], true);
    assert!(arr[0][0].get("vin").is_some(), "{resp}");
    assert_eq!(arr[1], json!([]));
    let op = format!("{spent}:0");
    let past = format!("{spent}:4294967295");
    let body = serde_json::to_vec(&json!([op, "bad", past])).unwrap();
    let (st, resp) = http_post_unix(sock, "/internal/txs/outspends/by-outpoint", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["spent"], true);
    assert_eq!(arr[1]["spent"], false);
    assert_eq!(arr[2], json!({"spent": false}), "vout past the outputs");

    let (st, resp) = http_get_unix(sock, &format!("/txs/outspends?txids={spent},{unknown}")).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0][0]["spent"], true);
    assert_eq!(arr[1], json!([]));
    let exactly: String = (0..50)
        .map(|_| "aa".repeat(32))
        .collect::<Vec<_>>()
        .join(",");
    let (st, resp) = http_get_unix(sock, &format!("/txs/outspends?txids={exactly}")).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 50, "{resp}");
    let too_many = (0..51)
        .map(|_| "aa".repeat(32))
        .collect::<Vec<_>>()
        .join(",");
    let (st, resp) = http_get_unix(sock, &format!("/txs/outspends?txids={too_many}")).await;
    assert_eq!(st, 400, "{resp}");
    assert!(resp.contains("Too many txids requested"), "{resp}");
}

/// One confirmed coinbase spend with `nout` OP_TRUE outputs. Returns its txid.
fn mine_fan_tx(pad: &Pad, nout: u32) -> Txid {
    use bitcoin::hashes::Hash;
    use rbitcoin_consensus::mine_regtest_paying_bits;

    let params = ChainParams::regtest();
    let tip_h = pad.q.tip_height().unwrap().0;
    let rec = pad
        .q
        .get_header(pad.q.tip_header_fk().unwrap().unwrap())
        .unwrap();
    let time = rec.timestamp + 600;
    let h = tip_h + 1;
    let bits = rbitcoin_query::expected_next_bits(&pad.q, &params, Height(h), time).unwrap();
    let spk = ScriptBuf::from_bytes(vec![0x51]);
    let mut fan = spend_true(pad.cbs[0], 0, spk.clone());
    fan.output = (0..nout)
        .map(|_| TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: spk.clone(),
        })
        .collect();
    let b = mine_regtest_paying_bits(
        bitcoin::BlockHash::from_byte_array(rec.hash),
        time,
        h,
        spk,
        vec![fan.clone()],
        bits,
    );
    accept_and_connect_block(&pad.q, &params, Height(h), &b, Milestone::NONE).unwrap();
    fan.compute_txid()
}

fn repeat_ids(id: &str, n: usize) -> Vec<String> {
    (0..n).map(|_| id.to_string()).collect()
}

/// A confirmed 5,001-output tx: two copies answer in full; 50 copies pass
/// the 250,000-entry cap and get 400 on the public and internal routes. The
/// internal route also caps txids at 1,000.
#[tokio::test]
async fn outspends_bounds_total_entries() {
    let pad = pad_hub("outspends-cap", 1);
    let fan = mine_fan_tx(&pad, 5_001).to_string();
    let sock_dir = rbitcoin_query::testutil::TempDir::for_socket().expect("temp dir");
    let (handle, sock) = run_unix(
        sock_dir.path(),
        Arc::clone(&pad.q),
        Some(Arc::clone(&pad.hub)),
    )
    .await;

    let (st, resp) = http_get_unix(&sock, &format!("/txs/outspends?txids={fan},{fan}")).await;
    assert_eq!(st, 200, "{}", &resp[..resp.len().min(200)]);
    let arr: Vec<Vec<Value>> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|slots| slots.len() == 5_001));
    assert_eq!(arr[1][5_000], json!({"spent": false}));

    // The cap reads output counts from `create.loc`; no output is decoded.
    let txs = &pad.q.store().txs;
    let _ = txs.sample_reset_outputs_decoded();
    let over = repeat_ids(&fan, 50).join(",");
    let (st, resp) = http_get_unix(&sock, &format!("/txs/outspends?txids={over}")).await;
    assert_eq!(st, 400, "{}", &resp[..resp.len().min(200)]);
    assert_eq!(resp, "Too many outputs requested");
    assert_eq!(txs.sample_reset_outputs_decoded(), 0);

    let body = serde_json::to_vec(&repeat_ids(&fan, 50)).unwrap();
    let (st, resp) = http_post_unix(&sock, "/internal/txs/outspends/by-txid", &body).await;
    assert_eq!(st, 400, "{}", &resp[..resp.len().min(200)]);
    assert_eq!(resp, "Too many outputs requested");
    assert_eq!(txs.sample_reset_outputs_decoded(), 0);

    let unknown = "ff".repeat(32);
    let body = serde_json::to_vec(&repeat_ids(&unknown, 1_000)).unwrap();
    let (st, resp) = http_post_unix(&sock, "/internal/txs/outspends/by-txid", &body).await;
    assert_eq!(st, 200, "{}", &resp[..resp.len().min(200)]);
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr.len(), 1_000);
    let body = serde_json::to_vec(&repeat_ids(&unknown, 1_001)).unwrap();
    let (st, resp) = http_post_unix(&sock, "/internal/txs/outspends/by-txid", &body).await;
    assert_eq!(st, 400, "{}", &resp[..resp.len().min(200)]);
    assert_eq!(resp, "Too many txids requested");

    handle.shutdown().await;
    let _ = pad.dir;
}

/// Admission vsize rounds up. This spend has no sigops, so its admission
/// weight equals its BIP141 weight.
async fn test_rounds_vsize_up(sock: &std::path::Path, pad: &Pad) {
    use bitcoin::consensus::encode::serialize_hex;

    let witness_script = ScriptBuf::from_bytes(vec![0x51]);
    let parent = spend_true(
        pad.cbs[3],
        1_000,
        ScriptBuf::new_p2wsh(&witness_script.wscript_hash()),
    );
    let (st, resp) = http_post_unix(sock, "/tx", serialize_hex(&parent).as_bytes()).await;
    assert_eq!(st, 200, "{resp}");
    let mut child = spend_true(
        parent.compute_txid(),
        2_000,
        ScriptBuf::from_bytes(vec![0x51]),
    );
    child.input[0].witness = Witness::from_slice(&[witness_script.as_bytes()]);
    let weight = child.weight().to_wu();
    assert_ne!(weight % 4, 0, "the fixture needs a weight that rounds");

    let body = serde_json::to_vec(&json!([serialize_hex(&child)])).unwrap();
    let (st, resp) = http_post_unix(sock, "/txs/test", &body).await;
    assert_eq!(st, 200, "{resp}");
    let arr: Vec<Value> = serde_json::from_str(&resp).unwrap();
    assert_eq!(arr[0]["allowed"], true, "{resp}");
    assert_eq!(arr[0]["vsize"].as_u64(), Some(weight.div_ceil(4)), "{resp}");
}

/// mempool.space's backend on the group-restricted socket: it tests and
/// broadcasts three spends, pages the mempool, then pulls the bulk routes.
/// Last, an explorer reads totals above `--max-sh-creates`.
#[tokio::test]
async fn esplora_unix_internal() {
    use std::os::unix::fs::PermissionsExt;

    let pad = pad_hub("unix-internal", 4);
    let sock_dir = rbitcoin_query::testutil::TempDir::for_socket().expect("temp dir");
    let (handle, sock) = run_unix(
        sock_dir.path(),
        Arc::clone(&pad.q),
        Some(Arc::clone(&pad.hub)),
    )
    .await;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o660,
        "esplora socket is group-restricted, got {mode:o}"
    );
    let (st, body) = http_get_unix(&sock, "/blocks/tip/height").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, pad.q.tip_height().unwrap().0.to_string());

    let spk = ScriptBuf::from_bytes(vec![0x51]);
    let txs: Vec<Transaction> = (0..3)
        .map(|i| spend_true(pad.cbs[i], 1_000 + i as u64, spk.clone()))
        .collect();
    test_then_broadcast(&sock, &pad, &txs).await;
    mempool_totals_and_txid_pages(&sock, &pad).await;
    internal_mempool_pages(&sock).await;
    internal_txs_and_block_txs(&sock, &pad, &txs[0].compute_txid().to_string()).await;
    unix_block_txs_summary(&sock, &pad).await;
    outspends_by_txid_outpoint_and_query(&sock, &pad).await;
    test_rounds_vsize_up(&sock, &pad).await;
    internal_totals_lift_the_cap(&sock, &pad).await;

    handle.shutdown().await;
    let _ = pad.dir;
}

/// `(status, lowercased head, body)` for one GET on the unix socket.
/// `client` names a join client; the socket trusts that header.
async fn http_get_unix_head(
    sock: &std::path::Path,
    path: &str,
    client: Option<&str>,
) -> (u16, String, String) {
    let mut stream = UnixStream::connect(sock).await.expect("unix connect");
    let named = client
        .map(|c| format!("X-Rbitcoin-Client: {c}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: api\r\n{named}Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let (st, body) = parse_http(&buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let head = text
        .split("\r\n\r\n")
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (st, head, body)
}

async fn http_get_tcp(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("tcp connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    parse_http(&buf)
}

/// Mine `n` blocks on the tip whose coinbase pays `spk`.
fn mine_paying(q: &Query, spk: &ScriptBuf, n: u32) {
    let params = ChainParams::regtest();
    let tip = q.tip_height().unwrap().0;
    let (_, rec) = q.header_at_height(Height(tip)).unwrap().unwrap();
    let mut prev = <bitcoin::BlockHash as bitcoin::hashes::Hash>::from_byte_array(rec.hash);
    let mut time = rec.timestamp;
    for h in tip + 1..=tip + n {
        time += 600;
        let bits = rbitcoin_query::expected_next_bits(q, &params, Height(h), time).unwrap();
        let b = rbitcoin_consensus::mine_regtest_paying_bits(
            prev,
            time,
            h,
            spk.clone(),
            Vec::new(),
            bits,
        );
        accept_and_connect_block(q, &params, Height(h), &b, Milestone::NONE).unwrap();
        prev = b.block_hash();
    }
    q.apply_sh_pending().unwrap();
}

fn tip_headers(head: &str, q: &Query, height: u32) {
    let (_, rec) = q.header_at_height(Height(height)).unwrap().unwrap();
    let hash = rbitcoin_primitives::display_hash_hex(rec.hash);
    assert!(
        head.contains(&format!("x-bitcoin-chain-tip: {hash}")),
        "{head}"
    );
    assert!(
        head.contains(&format!("x-bitcoin-chain-tip-height: {height}")),
        "{head}"
    );
}

/// `p` refuses above the cap on the socket (before and after its `/internal`
/// twin answers) and on TCP; the twin answers `public` with the tip stamp at
/// `at`, and TCP has no twin.
async fn capped_but_internal(
    sock: &std::path::Path,
    tcp: std::net::SocketAddr,
    q: &Query,
    p: &str,
    public: (&str, &str),
    at: u32,
) {
    let (st, _, body) = http_get_unix_head(sock, p, Some("explorer")).await;
    assert_eq!(st, 503, "socket {p} stays capped: {body}");
    assert!(body.contains("exceeds --max-sh-creates"), "{body}");
    let (st, head, body) =
        http_get_unix_head(sock, &format!("/internal{p}"), Some("explorer")).await;
    assert_eq!(st, 200, "/internal{p}: {body}");
    assert_eq!(body, public.1, "/internal{p} is the public body");
    tip_headers(&head, q, at);
    tip_headers(public.0, q, at);
    let (st, _, body) = http_get_unix_head(sock, p, Some("explorer")).await;
    assert_eq!(st, 503, "socket {p} still capped after /internal: {body}");
    let (st, body) = http_get_tcp(tcp, p).await;
    assert_eq!(st, 503, "TCP {p} stays capped: {body}");
    let (st, body) = http_get_tcp(tcp, &format!("/internal{p}")).await;
    assert_eq!(
        (st, body.as_str()),
        (404, "Not Found"),
        "TCP has no /internal{p}"
    );
}

/// Capability probe: an unused scripthash is a light 200 of zeros here and
/// 404 on a node without the route. A malformed one is 404 like the public route.
async fn internal_route_probe(sock: &std::path::Path) {
    let zeros = "00".repeat(32);
    let (st, body) = http_get_unix(sock, &format!("/internal/scripthash/{zeros}")).await;
    assert_eq!(st, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["chain_stats"]["tx_count"], 0, "{body}");
    for bad in ["/internal/scripthash/zz", "/internal/address/nope"] {
        let (st, body) = http_get_unix(sock, bad).await;
        assert_eq!((st, body.as_str()), (404, "Not Found"), "{bad}");
    }
}

/// The explorer's one-off totals for a script above `--max-sh-creates`:
/// `/internal/{address,scripthash}/:x` answers on the unix socket with the
/// body, `?asof=` and tip headers of the public route at cap 0. The public
/// routes still refuse on the socket and on TCP, and TCP has no such route.
async fn internal_totals_lift_the_cap(sock: &std::path::Path, pad: &Pad) {
    let q = &pad.q;
    let p2wsh = ScriptBuf::new_p2wsh(&ScriptBuf::from_bytes(vec![0x51]).wscript_hash());
    mine_paying(q, &p2wsh, 3);
    let addr = bitcoin::Address::from_script(&p2wsh, bitcoin::Network::Regtest)
        .unwrap()
        .to_string();
    let h51 = rbitcoin_primitives::scripthash_display_hex(&[0x51]);
    let tip = q.tip_height().unwrap().0;
    let (_, at50) = q.header_at_height(Height(50)).unwrap().unwrap();
    let asof50 = rbitcoin_primitives::display_hash_hex(at50.hash);
    let tcp_cfg =
        EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), bitcoin::Network::Regtest);
    let tcp = run_esplora(tcp_cfg, Arc::clone(q), Some(Arc::clone(&pad.hub)))
        .await
        .unwrap();

    let paths = [
        format!("/scripthash/{h51}"),
        format!("/address/{addr}"),
        format!("/scripthash/{h51}?asof={asof50}"),
        format!("/address/{addr}?asof={asof50}"),
    ];
    let mut uncapped = Vec::new();
    q.set_max_sh_creates(0);
    for p in &paths {
        let (st, head, body) = http_get_unix_head(sock, p, None).await;
        assert_eq!(st, 200, "{p}: {body}");
        uncapped.push((head, body));
    }
    q.set_max_sh_creates(2);
    for (p, (pub_head, pub_body)) in paths.iter().zip(&uncapped) {
        let at = if p.contains("asof") { 50 } else { tip };
        capped_but_internal(sock, tcp.local_addr, q, p, (pub_head, pub_body), at).await;
    }
    let totals: Value = serde_json::from_str(&uncapped[0].1).unwrap();
    assert!(
        totals["chain_stats"]["funded_txo_count"].as_u64() > Some(2),
        "{totals}"
    );
    assert!(
        totals["mempool_stats"]["tx_count"].as_u64() > Some(0),
        "{totals}"
    );
    let addr_totals: Value = serde_json::from_str(&uncapped[1].1).unwrap();
    assert_eq!(
        addr_totals["chain_stats"]["funded_txo_count"], 3,
        "{addr_totals}"
    );
    let at50: Value = serde_json::from_str(&uncapped[3].1).unwrap();
    assert_eq!(at50["chain_stats"]["tx_count"], 0, "{at50}");

    internal_route_probe(sock).await;
    internal_totals_take_heavy_turns(q, sock, &h51, &asof50).await;
    q.set_max_sh_creates(0);
    tcp.shutdown().await;
}

/// `--heavy-sh-creates` still runs the lifted joins one at a time.
#[cfg(debug_assertions)]
async fn internal_totals_take_heavy_turns(
    q: &Query,
    sock: &std::path::Path,
    h51: &str,
    asof: &str,
) {
    q.set_heavy_sh_creates(1);
    q.sh_join_probe.arm();
    let path = format!("/internal/scripthash/{h51}?asof={asof}");
    let (a, b) = tokio::join!(http_get_unix(sock, &path), http_get_unix(sock, &path));
    assert_eq!((a.0, b.0), (200, 200), "{a:?} {b:?}");
    assert_eq!(a.1, b.1);
    assert_eq!(q.sh_join_probe.entered(), (0, 2), "two heavy joins");
    assert_eq!(q.sh_join_probe.peaks().1, 1, "heavy joins take turns");
    q.set_heavy_sh_creates(rbitcoin_query::DEFAULT_HEAVY_SH_CREATES);
}

#[cfg(not(debug_assertions))]
async fn internal_totals_take_heavy_turns(_: &Query, _: &std::path::Path, _: &str, _: &str) {}
