use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::extract::Query as AxumQuery;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::{self};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bitcoin::Network;
use rbitcoin_electrum::ServeLimits;
use rbitcoin_esplora::handlers;
use rbitcoin_esplora::server::*;
use rbitcoin_esplora::tx_json::tx_status_json;
use rbitcoin_primitives::Fk;
use rbitcoin_primitives::Height;
use rbitcoin_query::Query;
use rbitcoin_query::TxApply;
use rbitcoin_store::script_hash;
use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use rbitcoin_query::testutil::FixtureChain;

#[test]
fn esplora_accept_cap_stays_inside_the_semaphore() {
    let max = tokio::sync::Semaphore::MAX_PERMITS;
    assert_eq!(esplora_accept_cap(4), 32);
    assert!(
        esplora_accept_cap(max) <= max,
        "accept cap {} exceeds the semaphore",
        esplora_accept_cap(max)
    );
}

/// Esplora caps accepted TCP sockets, not only in-flight requests, and closes
/// a connection that never sends headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_cap_esplora_idle_tcp_is_closed() {
    let (_dir, q) = temp_query("esplora-idle-tcp");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let mut cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    cfg.limits.max_connections = 4;
    cfg.limits.idle_timeout = Duration::from_millis(300);
    let cap = rbitcoin_esplora::server::esplora_accept_cap(4);
    let handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    let addr = handle.local_addr;

    let mut opened = Vec::new();
    for _ in 0..(cap + 16) {
        opened.push(TcpStream::connect(addr).await.expect("connect"));
    }
    let mut still = accepted_within(opened, cap).await;
    assert!(
        !still.is_empty() && still.len() <= cap,
        "esplora held {} idle TCP connections; cap is {cap}",
        still.len()
    );
    let mut idle = still.pop().expect("accepted idle client");
    drop(still);
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 8];
        loop {
            match idle.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "an esplora connection that sent no headers was still open after 2s"
    );
    handle.shutdown().await;
}

/// Shutdown closes an accepted idle socket. The header timer is not what
/// closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_cap_esplora_shutdown_closes_idle_tcp() {
    let (_dir, q) = temp_query("esplora-shutdown-idle");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let mut cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    cfg.limits.max_connections = 1;
    cfg.limits.idle_timeout = Duration::from_secs(30);
    let cap = rbitcoin_esplora::server::esplora_accept_cap(1);
    let handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    let addr = handle.local_addr;

    let mut opened = Vec::new();
    for _ in 0..(cap + 16) {
        opened.push(TcpStream::connect(addr).await.expect("connect"));
    }
    let mut still = accepted_within(opened, cap).await;
    assert!(
        !still.is_empty() && still.len() <= cap,
        "esplora held {} idle TCP connections before shutdown; cap is {cap}",
        still.len()
    );
    let mut idle = still.pop().expect("accepted idle client");
    drop(still);
    handle.shutdown().await;
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        let mut buf = [0u8; 8];
        loop {
            match idle.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "shutdown left an idle esplora socket open");
}

/// `store(true)` ends the serve task without waiting out the shutdown poll.
#[tokio::test]
async fn esplora_shutdown_wakes_without_the_fifty_ms_poll() {
    tokio::time::pause();
    let (_dir, q) = temp_query("esplora-stop-wake");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let mut handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    handle.shutdown.store(true);
    tokio::task::yield_now().await;
    let joined = tokio::time::timeout(Duration::ZERO, &mut handle.task).await;
    tokio::time::resume();
    joined
        .expect("esplora stayed in the 50 ms shutdown poll")
        .expect("serve task");
}

/// Probe until the accept loop has refused down to `cap`, or the deadline.
async fn accepted_within(mut opened: Vec<TcpStream>, cap: usize) -> Vec<TcpStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut probes = tokio::task::JoinSet::new();
        for mut sock in opened {
            probes.spawn(async move {
                let mut buf = [0u8; 1];
                let open = match tokio::time::timeout(
                    Duration::from_millis(20),
                    sock.read(&mut buf),
                )
                .await
                {
                    Err(_) => true,
                    Ok(Ok(0)) | Ok(Err(_)) => false,
                    Ok(Ok(_)) => true,
                };
                (sock, open)
            });
        }
        let mut still = Vec::new();
        while let Some(joined) = probes.join_next().await {
            let (sock, open) = joined.expect("probe");
            if open {
                still.push(sock);
            }
        }
        if !still.is_empty() && still.len() <= cap {
            return still;
        }
        if std::time::Instant::now() >= deadline {
            return still;
        }
        opened = still;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_listen_creates_a_missing_parent_dir() {
    let (_dir, q) = temp_query("esplora-unix-parent");
    // A store path is too long for `sockaddr_un`. The socket path stays short.
    let short = std::path::PathBuf::from(format!("/tmp/rbtc-es-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&short);
    std::fs::create_dir_all(&short).unwrap();
    let sock = short.join("m").join("s.sock");
    let cfg = EsploraConfig::with_listen(EsploraListen::Unix(sock.clone()), Network::Regtest);
    let mut handle = run_esplora(cfg, Arc::new(q), None)
        .await
        .expect("nested socket parent is created");
    assert!(sock.exists(), "bind did not create {}", sock.display());
    handle.shutdown.store(true);
    tokio::time::timeout(Duration::from_secs(2), &mut handle.task)
        .await
        .expect("esplora shutdown")
        .expect("serve task");
    let _ = std::fs::remove_dir_all(&short);
}

fn temp_query(label: &str) -> (rbitcoin_query::testutil::TempDir, Query) {
    rbitcoin_query::testutil::tiny_query_labeled(label)
}

fn coinbase(h: u32, prev: Fk, parent_hash: Option<[u8; 32]>) -> (HeaderRecord, TxApply) {
    let version = 1;
    let timestamp = h + 1;
    let bits = 0x207fffff;
    let nonce = h;
    let mut txid = [0u8; 32];
    txid[0..4].copy_from_slice(&h.to_le_bytes());
    txid[31] = 0xcb;
    // One coinbase, so the header root is that txid. A proof built from the
    // stored header then matches `extract_matches`.
    let merkle = rbitcoin_store::merkle_root_from_txids(&[txid]);
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
    (header, ta)
}

#[tokio::test]
async fn merkleblock_proof_keeps_a_stored_root_that_does_not_match() {
    use bitcoin::consensus::encode::deserialize;
    use bitcoin::hashes::Hash;
    use bitcoin::MerkleBlock;

    let (_dir, q) = temp_query("mb-bad-root");
    let (mut header, ta) = coinbase(0, Fk::NULL, None);
    header.merkle_root = [0xab; 32];
    header.hash = [0xab; 32];
    let txid = ta.tx.txid;
    q.connect_block(Height(0), &header, std::slice::from_ref(&ta))
        .unwrap();
    let q = Arc::new(q);
    let st = join_only_state(
        Arc::clone(&q),
        Arc::new(Mutex::new(rbitcoin_esplora::server::JoinCache::default())),
    );
    let resp = handlers::tx_merkleblock_proof_sync(st, block_hash_hex(&txid));
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let mb: MerkleBlock =
        deserialize(&rbitcoin_primitives::hex_decode(body.trim()).unwrap()).unwrap();
    assert_eq!(mb.header.merkle_root.to_byte_array(), [0xab; 32]);
    let mut matches = Vec::new();
    let mut indexes = Vec::new();
    assert!(mb.extract_matches(&mut matches, &mut indexes).is_err());
}

fn header_value(text: &str, name: &str) -> Option<String> {
    let want = name.to_ascii_lowercase();
    text.split("\r\n\r\n")
        .next()
        .unwrap_or("")
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(&want))
        .map(|(_, v)| v.trim().to_string())
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let (status, _hdrs, body) = http_get_raw(addr, path).await;
    (status, body)
}

async fn http_get_raw(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
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
    (status, text, body)
}

async fn http_post(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
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

#[test]
fn esplora_listen_parse_tcp_and_path() {
    assert!(matches!(
        EsploraListen::parse("127.0.0.1:3000", 3000).unwrap(),
        EsploraListen::Tcp(_)
    ));
    assert!(matches!(
        EsploraListen::parse("", 3000).unwrap(),
        EsploraListen::Tcp(a) if a.port() == 3000
    ));
    #[cfg(unix)]
    {
        assert!(matches!(
            EsploraListen::parse("/run/rbitcoin/esplora.sock", 3000).unwrap(),
            EsploraListen::Unix(_)
        ));
        assert!(matches!(
            EsploraListen::parse("./esplora.sock", 3000).unwrap(),
            EsploraListen::Unix(_)
        ));
        // One path arm each: a leading dot, a slash, a `.sock` suffix.
        assert!(matches!(
            EsploraListen::parse(".hidden", 3000).unwrap(),
            EsploraListen::Unix(_)
        ));
        assert!(matches!(
            EsploraListen::parse("foo/bar", 3000).unwrap(),
            EsploraListen::Unix(_)
        ));
        assert!(matches!(
            EsploraListen::parse("esplora.sock", 3000).unwrap(),
            EsploraListen::Unix(_)
        ));
    }
    assert!(EsploraListen::parse("not-an-addr", 3000).is_err());
}

#[test]
fn join_idle_evicts_after_ttl() {
    let mut map = HashMap::new();
    let now = Instant::now();
    map.insert(
        "stale".into(),
        ClientJoins {
            last_req: now.checked_sub(JOIN_IDLE + Duration::from_secs(1)).unwrap(),
            ..ClientJoins::default()
        },
    );
    map.insert(
        "fresh".into(),
        ClientJoins {
            last_req: now,
            ..ClientJoins::default()
        },
    );
    sweep_clients(&mut map, now);
    assert!(!map.contains_key("stale"));
    assert!(map.contains_key("fresh"));
}

#[test]
fn join_idle_at_the_ttl_is_gone_and_the_client_cap_keeps_256() {
    let mut map = HashMap::new();
    let now = Instant::now();
    map.insert(
        "edge".into(),
        ClientJoins {
            last_req: now.checked_sub(JOIN_IDLE).unwrap(),
            ..ClientJoins::default()
        },
    );
    map.insert(
        "inside".into(),
        ClientJoins {
            last_req: now
                .checked_sub(JOIN_IDLE - Duration::from_nanos(1))
                .unwrap(),
            ..ClientJoins::default()
        },
    );
    sweep_clients(&mut map, now);
    assert!(!map.contains_key("edge"));
    assert!(map.contains_key("inside"));

    let mut full = HashMap::new();
    for i in 0..JOIN_MAX_CLIENTS {
        full.insert(
            format!("c{i}"),
            ClientJoins {
                last_req: now,
                ..ClientJoins::default()
            },
        );
    }
    sweep_clients(&mut full, now);
    assert_eq!(full.len(), JOIN_MAX_CLIENTS);

    full.insert(
        "oldest".into(),
        ClientJoins {
            last_req: now.checked_sub(Duration::from_secs(1)).unwrap(),
            ..ClientJoins::default()
        },
    );
    sweep_clients(&mut full, now);
    assert_eq!(full.len(), JOIN_MAX_CLIENTS);
    assert!(!full.contains_key("oldest"));
}

#[test]
fn join_bulk_cap_is_16_mib_and_keeps_a_two_mib_slot() {
    assert_eq!(JOIN_BULK_CAP, 16_777_216);
    let two_mib = rbitcoin_query::ShJoinSlot::with_spender_fk_count((2 * 1024 * 1024 - 144) / 8);
    assert!(two_mib.packed_bytes() > 1_048_576);
    assert!(two_mib.packed_bytes() < JOIN_BULK_CAP);
    let mut c = ClientJoins {
        last_bulk: HashMap::from([([1u8; 32], two_mib)]),
        ..ClientJoins::default()
    };
    cap_bulk(&mut c);
    assert_eq!(c.last_bulk.len(), 1);
}

#[test]
fn cap_bulk_keeps_the_cap_and_drops_one_byte_over() {
    let exact = rbitcoin_query::ShJoinSlot::with_spender_fk_count((JOIN_BULK_CAP - 144) / 8);
    assert_eq!(exact.packed_bytes(), JOIN_BULK_CAP);
    let mut at = ClientJoins {
        last_bulk: HashMap::from([([2u8; 32], Arc::clone(&exact))]),
        ..ClientJoins::default()
    };
    cap_bulk(&mut at);
    assert_eq!(at.last_bulk.len(), 1);

    let over = rbitcoin_query::testutil::sh_join_slot_over_16mib();
    let mut c = ClientJoins {
        last_bulk: HashMap::from([([3u8; 32], over)]),
        ..ClientJoins::default()
    };
    cap_bulk(&mut c);
    assert!(c.last_bulk.is_empty());
}

#[test]
fn retain_join_budget_keeps_a_last_sh_at_the_cap() {
    let exact = rbitcoin_query::ShJoinSlot::with_spender_fk_count((JOIN_BULK_CAP - 144) / 8);
    let mut c = ClientJoins {
        last_sh: Some(([4u8; 32], exact, 1)),
        ..ClientJoins::default()
    };
    retain_join_budget(&mut c);
    assert!(c.last_sh.is_some());
}

#[test]
fn named_join_client_drops_only_an_empty_id() {
    assert_eq!(named_join_client(None), None);
    assert_eq!(named_join_client(Some("")), None);
    assert_eq!(named_join_client(Some("c1")), Some("c1"));
}

#[test]
fn esplora_stop_load_follows_store() {
    let stop = EsploraStop::new(false);
    assert!(!stop.load());
    stop.store(true);
    assert!(stop.load());
}

#[test]
fn blindbit_default_inflight_is_half_the_pool() {
    let pool = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .max(4);
    assert_eq!(rbitcoin_esplora::blindbit::default_max_inflight(), pool / 2);
    assert!(rbitcoin_esplora::blindbit::default_max_inflight() >= 2);
}

#[test]
fn esplora_perf_window_resets_and_totals_keep_the_note() {
    const NOTE: u64 = 1_000_000_000;
    let _ = sample_reset_perf();
    let before = perf_totals();
    METER.note(NOTE);
    let window = sample_reset_perf();
    let after = perf_totals();
    assert!(window.0 >= 1 && window.1 >= NOTE && window.2 >= NOTE);
    assert!(after.0 > before.0);
    assert!(after.1 >= before.1.saturating_add(NOTE));
}

fn join_only_state(q: Arc<Query>, cache: Arc<Mutex<JoinCache>>) -> AppState {
    AppState {
        query: q,
        network: Network::Regtest,
        mempool: None,
        max_body: 1 << 20,
        sh_join: cache,
        join_header_trusted: true,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    }
}

#[test]
fn with_sh_join_empty_slot_unblocks_waiter() {
    let (_dir, q) = temp_query("join-empty-waiter");
    let st = Arc::new(join_only_state(
        Arc::new(q),
        Arc::new(Mutex::new(JoinCache::default())),
    ));
    let sh = [0x11u8; 32];
    let (leader_in, leader_in_rx) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let st_l = Arc::clone(&st);
    let leader = std::thread::spawn(move || {
        st_l.with_sh_join(Some("c1"), &sh, |slot| {
            *slot = None;
            let _ = leader_in.send(());
            let _ = release_rx.recv();
        });
    });
    leader_in_rx.recv().expect("leader entered f");
    let inf = st
        .sh_join
        .lock()
        .unwrap()
        .clients
        .get("c1")
        .and_then(|c| c.inflight.get(&sh).cloned())
        .expect("leader inflight");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let st_w = Arc::clone(&st);
    let waiter = std::thread::spawn(move || {
        st_w.with_sh_join(Some("c1"), &sh, |slot| {
            let _ = done_tx.send(slot.is_none());
        });
    });
    assert!(
        inf.wait_queued(Duration::from_secs(2)),
        "waiter did not queue on the empty join"
    );
    let _ = release.send(());
    leader.join().expect("leader");
    let empty = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("waiter unblocked after empty join");
    assert!(empty, "finished empty slot");
    waiter.join().expect("waiter");
}

#[test]
fn with_sh_join_leader_panic_unblocks_waiter() {
    let (_dir, q) = temp_query("join-panic-waiter");
    let st = Arc::new(join_only_state(
        Arc::new(q),
        Arc::new(Mutex::new(JoinCache::default())),
    ));
    let sh = [0x22u8; 32];
    let (leader_in, leader_in_rx) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let st_l = Arc::clone(&st);
    let leader = std::thread::spawn(move || {
        st_l.with_sh_join(Some("c1"), &sh, |_slot| {
            let _ = leader_in.send(());
            let _ = release_rx.recv();
            panic!("join leader unwind");
        });
    });
    leader_in_rx.recv().expect("leader entered f");
    let inf = st
        .sh_join
        .lock()
        .unwrap()
        .clients
        .get("c1")
        .and_then(|c| c.inflight.get(&sh).cloned())
        .expect("leader inflight");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let st_w = Arc::clone(&st);
    let waiter = std::thread::spawn(move || {
        st_w.with_sh_join(Some("c1"), &sh, |_slot| {
            let _ = done_tx.send(());
        });
    });
    assert!(
        inf.wait_queued(Duration::from_secs(2)),
        "waiter did not queue before the leader unwound"
    );
    let _ = release.send(());
    let _ = leader.join();
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("waiter unblocked after leader panic");
    waiter.join().expect("waiter");
}

#[tokio::test]
async fn with_sh_join_last1_clone_visible_to_overlapping_get() {
    use rbitcoin_store::script_hash;

    let (_a1, spk1) = regtest_p2wpkh();
    let (dir, q) = temp_query("join-last1-clone");
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    let prev = q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let (h1, cb1) = coinbase(1, prev, Some(h0.hash));
    q.connect_block(Height(1), &h1, &[cb1, two_script_pay(0x11, spk1.clone())])
        .unwrap();
    let q = Arc::new(q);
    let sh1 = script_hash(spk1.as_bytes());
    let h1hex = block_hash_hex(&sh1);
    let cache = Arc::new(Mutex::new(JoinCache::default()));
    let app = app_with_join(Arc::clone(&q), Arc::clone(&cache), true);
    let (st, body) =
        oneshot_http(&app, get_with_client(&format!("/scripthash/{h1hex}"), "c1")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(cache.lock().unwrap().last_sh_key("c1"), Some(sh1));

    let st = Arc::new(join_only_state(Arc::clone(&q), Arc::clone(&cache)));
    let (holder_in, holder_in_rx) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let st_a = Arc::clone(&st);
    let holder = std::thread::spawn(move || {
        st_a.with_sh_join(Some("c1"), &sh1, |slot| {
            assert!(slot.is_some(), "holder must see warm last-1");
            let _ = holder_in.send(());
            let _ = release_rx.recv();
        });
    });
    holder_in_rx.recv().expect("holder entered f");
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let st_b = Arc::clone(&st);
    let overlap = std::thread::spawn(move || {
        st_b.with_sh_join(Some("c1"), &sh1, |slot| {
            let _ = seen_tx.send(slot.is_some());
        });
    });
    let saw = seen_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("overlap entered f");
    assert!(
        saw,
        "overlapping same-sh GET must clone last-1, not take it"
    );
    let _ = release.send(());
    holder.join().expect("holder");
    overlap.join().expect("overlap");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn with_sh_join_drops_last_sh_over_bulk_cap() {
    let (_dir, q) = temp_query("join-oversize-last-sh");
    let cache = Arc::new(Mutex::new(JoinCache::default()));
    let st = join_only_state(Arc::new(q), Arc::clone(&cache));
    let sh = [0x33u8; 32];
    let slot = rbitcoin_query::testutil::sh_join_slot_over_16mib();
    assert!(
        slot.packed_bytes() > JOIN_BULK_CAP,
        "fixture must exceed 16 MiB packed"
    );
    st.with_sh_join(Some("c1"), &sh, |s| {
        *s = Some(Arc::clone(&slot));
    });
    assert!(
        cache.lock().unwrap().last_sh_key("c1").is_none(),
        "oversize last-1 is used then not retained"
    );
}

#[test]
fn late_waiter_keeps_a_newer_last_sh() {
    let (_dir, q) = temp_query("join-late-waiter-last-sh");
    let cache = Arc::new(Mutex::new(JoinCache::default()));
    let st = Arc::new(join_only_state(Arc::new(q), Arc::clone(&cache)));
    let sh_old = [0x11u8; 32];
    let sh_new = [0x22u8; 32];
    let slot = rbitcoin_query::testutil::sh_join_slot_small();
    let (leader_in, leader_in_rx) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let st_l = Arc::clone(&st);
    let slot_l = Arc::clone(&slot);
    let leader = std::thread::spawn(move || {
        st_l.with_sh_join(Some("c1"), &sh_old, |s| {
            *s = Some(slot_l);
            let _ = leader_in.send(());
            let _ = release_rx.recv();
        });
    });
    leader_in_rx.recv().expect("leader entered f");
    let slot_b = Arc::clone(&slot);
    st.with_sh_join(Some("c1"), &sh_new, |n| {
        *n = Some(slot_b);
    });
    assert_eq!(
        cache.lock().unwrap().last_sh_key("c1"),
        Some(sh_new),
        "a newer script that finishes while the older leader is inside f is last-1"
    );
    let (waiter_in, waiter_in_rx) = std::sync::mpsc::channel::<()>();
    let (waiter_go, waiter_go_rx) = std::sync::mpsc::channel::<()>();
    let st_w = Arc::clone(&st);
    let slot_w = Arc::clone(&slot);
    let waiter = std::thread::spawn(move || {
        st_w.with_sh_join(Some("c1"), &sh_old, |s| {
            *s = Some(slot_w);
            let _ = waiter_in.send(());
            let _ = waiter_go_rx.recv();
        });
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        waiter_in_rx.try_recv().is_err(),
        "waiter must block on the leader, not run the join itself"
    );
    let _ = release.send(());
    leader.join().expect("leader");
    assert_eq!(
        cache.lock().unwrap().last_sh_key("c1"),
        Some(sh_new),
        "an older leader that finishes after a newer script must not replace last-1"
    );
    waiter_in_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("waiter entered f after the leader finished");
    let _ = waiter_go.send(());
    waiter.join().expect("waiter");
    assert_eq!(
        cache.lock().unwrap().last_sh_key("c1"),
        Some(sh_new),
        "a late waiter must not put the old scripthash back over a newer last-1"
    );
}

#[cfg(unix)]
#[cfg(unix)]
#[tokio::test]
async fn bind_unix_socket_does_not_strip_dir_search() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    extern "C" {
        fn umask(mask: u32) -> u32;
    }
    // SAFETY: `umask` is process-global. This test restores `old` before return.
    let old = unsafe { umask(0o022) };
    let stop = Arc::new(AtomicBool::new(false));
    let bare = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = Arc::clone(&stop);
        let bare = Arc::clone(&bare);
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let path = std::env::temp_dir().join(format!(
                    "rbitcoin-umask-probe-esplora-{}-{}",
                    std::process::id(),
                    n
                ));
                n += 1;
                if std::fs::create_dir(&path).is_err() {
                    continue;
                }
                let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o111);
                let _ = std::fs::remove_dir(&path);
                if mode.is_ok_and(|m| m == 0) {
                    bare.store(true, Ordering::Relaxed);
                    break;
                }
            }
        })
    };
    let dir = rbitcoin_query::testutil::TempDir::for_socket().expect("temp dir");
    for i in 0..200 {
        let sock = dir.path().join(format!("s{i}.sock"));
        let listener = rbitcoin_esplora::server::bind_unix_mode(&sock, 0o660).expect("bind");
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "socket mode");
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        if bare.load(Ordering::Relaxed) {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    // SAFETY: restores the umask saved at the start of this test.
    unsafe { umask(old) };
    assert!(
        !bare.load(Ordering::Relaxed),
        "unix bind changed umask and a temp dir lost search permission"
    );
}

fn two_script_pay(tag: u8, spk: bitcoin::ScriptBuf) -> TxApply {
    let mut txid = [0u8; 32];
    txid[0] = tag;
    txid[31] = 0xaa;
    TxApply {
        tx: TxRecord {
            txid,
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(1_0000_0000, spk.to_bytes())],
    }
}

fn app_with_join(q: Arc<Query>, cache: Arc<Mutex<JoinCache>>, trusted: bool) -> Router {
    let state = AppState {
        query: q,
        network: Network::Regtest,
        mempool: None,
        max_body: 1 << 20,
        sh_join: cache,
        join_header_trusted: trusted,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    Router::new()
        .route("/scripthash/{hash}", get(handlers::scripthash_info))
        .route("/scripthash/{hash}/utxo", get(handlers::scripthash_utxo))
        .route("/scripthash/{hash}/txs", get(handlers::scripthash_txs))
        .route(
            "/scripthash/{hash}/txs/chain",
            get(handlers::scripthash_txs_chain),
        )
        .route(
            "/scripthash/{hash}/txs/summary",
            get(handlers::scripthash_txs_summary),
        )
        .route("/address/{addr}/utxo", get(handlers::address_utxo))
        .route("/scripthashes/txs", post(handlers::post_scripthashes_txs))
        .route(
            "/scripthashes/txs/summary",
            post(handlers::post_scripthashes_txs_summary),
        )
        .with_state(state)
}

async fn oneshot_http(app: &Router, req: axum::http::Request<axum::body::Body>) -> (u16, String) {
    use tower::ServiceExt;
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get_with_client(path: &str, client: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .uri(path)
        .header("X-Rbitcoin-Client", client)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// TCP mounts no `/internal` route, so with the SH index off an internal
/// stats path is still the unknown-path 404 while the public twin is 503.
#[tokio::test]
async fn tcp_internal_stats_path_is_404_without_sh_index() {
    let (_dir, q) = temp_query("esplora-internal-sh-off");
    q.set_sh_index_enabled(false);
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    let zeros = "00".repeat(32);
    let (st, body) = http_get(handle.local_addr, &format!("/scripthash/{zeros}")).await;
    assert_eq!(st, 503, "{body}");
    for p in [
        format!("/internal/scripthash/{zeros}"),
        format!("/internal/address/bcrt1q?asof={zeros}"),
    ] {
        let (st, body) = http_get(handle.local_addr, &p).await;
        assert_eq!((st, body.as_str()), (404, "Not Found"), "{p}");
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn address_without_sh_index_is_503_disabled() {
    let (dir, q) = temp_query("esplora-sh-off");
    q.set_sh_index_enabled(false);
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let state = AppState {
        query: Arc::new(q),
        network: Network::Regtest,
        mempool: None,
        max_body: 1024,
        sh_join: Arc::new(Mutex::new(JoinCache::default())),
        join_header_trusted: false,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/address/{addr}/utxo", get(handlers::address_utxo))
        .route("/addresses/txs", post(handlers::post_addresses_txs))
        .route("/scripthashes/txs", post(handlers::post_scripthashes_txs))
        .route("/blocks/tip/height", get(tip_height))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            stamp_chain_view_mw,
        ))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dummy = "bcrt1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqz8z5y2";
    let (st, body) = http_get(addr, &format!("/address/{dummy}/utxo")).await;
    assert_eq!(st, 503, "{body}");
    assert_eq!(body, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    let (st, body) = http_get(addr, "/blocks/tip/height").await;
    assert_eq!(st, 200, "tip must work without SH: {body}");
    let (st, body) = http_post(addr, "/addresses/txs", b"[]").await;
    assert_eq!(st, 503, "{body}");
    assert_eq!(body, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    let (st, body) = http_post(addr, "/scripthashes/txs", b"[]").await;
    assert_eq!(st, 503, "{body}");
    assert_eq!(body, rbitcoin_query::SCRIPTHASH_INDEX_DISABLED);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_503_chain_view_moved_omits_tip_header() {
    let (dir, q) = temp_query("http-503-moved");
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let state = AppState {
        query: Arc::new(q),
        network: Network::Regtest,
        mempool: None,
        max_body: 1024,
        sh_join: Arc::new(Mutex::new(JoinCache::default())),
        join_header_trusted: false,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    async fn die_tip(State(st): State<AppState>) -> &'static str {
        st.query.disconnect_tip().unwrap();
        "ok"
    }
    let app = Router::new()
        .route("/blocks/tip/hash", get(die_tip))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            stamp_chain_view_mw,
        ))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (st, raw, body) = http_get_raw(addr, "/blocks/tip/hash").await;
    assert_eq!(st, 503, "body={body}");
    assert!(
        body.contains("chain view moved"),
        "503 body must name the move: {body}"
    );
    assert!(
        header_value(&raw, HDR_CHAIN_TIP).is_none(),
        "503 must not stamp a fork tip"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pin_or_reject_without_asof_on_an_empty_chain() {
    let (dir, q) = temp_query("pin-empty");
    match pin_or_reject(&q, rbitcoin_query::ChainViewKind::Tip, None) {
        Ok(None) => {}
        Ok(Some(_)) => panic!("empty chain pinned a view"),
        Err(r) => panic!("status {}", r.status()),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn asof_of_a_moved_view_is_404() {
    let (dir, q) = temp_query("asof-moved");
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    let hash = h0.hash;
    q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let state = AppState {
        query: Arc::new(q),
        network: Network::Regtest,
        mempool: None,
        max_body: 1024,
        sh_join: Arc::new(Mutex::new(JoinCache::default())),
        join_header_trusted: false,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    async fn pin_then_move(State(st): State<AppState>, AsOf(asof): AsOf) -> Response {
        let view = match pin_or_reject(&st.query, rbitcoin_query::ChainViewKind::Tip, asof) {
            Ok(v) => v,
            Err(r) => return r,
        };
        st.query.disconnect_tip().unwrap();
        maybe_attach_view("ok".into_response(), view)
    }
    let app = Router::new()
        .route("/tx/{txid}/status", get(pin_then_move))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            stamp_chain_view_mw,
        ))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (st, body) = http_get(
        addr,
        &format!("/tx/ab/status?asof={}", block_hash_hex(&hash)),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    assert!(!body.contains("chain view moved"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn body_at_max_is_not_413_and_300_slots_fit() {
    // Body length against `max_body`, and the 300-element outs list. The routes call these checks.
    let (dir, q) = temp_query("body-max");
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let q = Arc::new(q);
    let mp = dir.join("mp");
    std::fs::create_dir_all(&mp).unwrap();
    let hub = rbitcoin_net::MempoolHub::open(&mp, Arc::clone(&q)).unwrap();
    let state = |max_body: usize, mempool: bool| AppState {
        query: Arc::clone(&q),
        network: Network::Regtest,
        mempool: mempool.then(|| Arc::clone(&hub)),
        max_body,
        sh_join: Arc::new(Mutex::new(JoinCache::default())),
        join_header_trusted: false,
        block_template: None,
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    let at = Bytes::new();
    let over = Bytes::from_static(b"x");
    let codes = |name: &str, at_max: Response, too_big: Response| {
        assert_ne!(at_max.status(), StatusCode::PAYLOAD_TOO_LARGE, "{name}");
        assert_eq!(too_big.status(), StatusCode::PAYLOAD_TOO_LARGE, "{name}");
    };
    codes(
        "post_block_height_outs",
        handlers::post_block_height_outs(State(state(0, false)), AsOf(None), at.clone()).await,
        handlers::post_block_height_outs(State(state(0, false)), AsOf(None), over.clone()).await,
    );
    codes(
        "post_scripthashes_txs",
        handlers::post_scripthashes_txs(
            State(state(0, false)),
            JoinClient(None),
            AxumQuery(handlers::AfterTxidQuery::default()),
            at.clone(),
        )
        .await,
        handlers::post_scripthashes_txs(
            State(state(0, false)),
            JoinClient(None),
            AxumQuery(handlers::AfterTxidQuery::default()),
            over.clone(),
        )
        .await,
    );
    codes(
        "post_txs_test",
        handlers::post_txs_test(
            State(state(0, false)),
            AxumQuery(handlers::MaxFeeQuery::default()),
            at.clone(),
        )
        .await,
        handlers::post_txs_test(
            State(state(0, false)),
            AxumQuery(handlers::MaxFeeQuery::default()),
            over.clone(),
        )
        .await,
    );
    codes(
        "post_tx",
        handlers::post_tx(State(state(0, true)), at.clone()).await,
        handlers::post_tx(State(state(0, true)), over.clone()).await,
    );
    codes(
        "post_tx_package",
        handlers::post_tx_package(State(state(0, true)), at.clone()).await,
        handlers::post_tx_package(State(state(0, true)), over.clone()).await,
    );
    #[cfg(unix)]
    {
        codes(
            "post_internal_txs",
            rbitcoin_esplora::internal::post_internal_txs(State(state(0, false)), at.clone()).await,
            rbitcoin_esplora::internal::post_internal_txs(State(state(0, false)), over.clone())
                .await,
        );
        codes(
            "post_outspends_by_txid",
            rbitcoin_esplora::internal::post_outspends_by_txid(State(state(0, false)), at.clone())
                .await,
            rbitcoin_esplora::internal::post_outspends_by_txid(
                State(state(0, false)),
                over.clone(),
            )
            .await,
        );
        codes(
            "post_outspends_by_outpoint",
            rbitcoin_esplora::internal::post_outspends_by_outpoint(
                State(state(0, false)),
                at.clone(),
            )
            .await,
            rbitcoin_esplora::internal::post_outspends_by_outpoint(
                State(state(0, false)),
                over.clone(),
            )
            .await,
        );
    }
    let slots = Bytes::from(serde_json::to_vec(&vec![[0u32, 0, 0]; 300]).unwrap());
    let resp =
        handlers::post_block_height_outs(State(state(slots.len(), false)), AsOf(None), slots).await;
    assert_ne!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn output_by_position_paths_accept_asof() {
    assert!(path_accepts_asof("/block-height/1/tx/0"));
    assert!(path_accepts_asof("/block-height/1/tx/0/out/0"));
    assert!(path_accepts_asof("/tx/ab/out/0"));
    assert!(path_accepts_asof("/block-heights/txs/outs"));
    assert!(!path_accepts_asof("/block-height/1"));
    assert!(!path_accepts_asof("/tx/ab"));
    assert!(!path_accepts_asof("/block/ab/tx/0"));
}

#[test]
fn config_defaults_use_public_proxy_limits() {
    let cfg = EsploraConfig::new("0.0.0.0:3000".parse().unwrap());
    assert_eq!(cfg.limits, ServeLimits::for_public_proxy());
}

/// The socket's uncapped stats twins pin and accept `?asof=` like `/address/:a`.
#[test]
fn internal_stats_paths_pin_the_sh_view() {
    for internal in ["/internal/address/bcrt1q", "/internal/scripthash/ab"] {
        assert!(path_accepts_asof(internal), "{internal}");
        assert!(path_uses_sh_view(internal), "{internal}");
    }
    assert!(!path_accepts_asof("/internal/block/ab/txs"));
    assert!(!path_uses_sh_view("/internal/block/ab/txs"));
}

#[test]
fn asof_query_is_gated_to_documented_routes() {
    assert!(path_accepts_asof("/tx/ab/status"));
    assert!(path_accepts_asof("/tx/ab/outspends"));
    assert!(path_accepts_asof("/tx/ab/outspend/0"));
    assert!(path_accepts_asof("/scripthash/ab/utxo"));
    assert!(path_accepts_asof("/address/bcrt1q/txs/chain/cd"));
    assert!(path_accepts_asof("/address/bcrt1q/txs/summary"));
    assert!(path_accepts_asof("/scripthash/ab/txs/summary/cd"));
    assert!(!path_accepts_asof("/address/bcrt1q/txs/mempool"));
    assert!(!path_accepts_asof("/tx/ab"));
    assert!(!path_accepts_asof("/block-height/1"));
    assert!(!path_accepts_asof("/mempool"));
    assert!(!path_accepts_asof("/scripthash/ab/txs/mempool"));
    assert!(path_never_pins("/mempool"));
    assert!(path_never_pins("/mempool/txids"));
    assert!(path_never_pins("/fee-estimates"));
    assert!(path_never_pins("/tx"));
    assert!(!path_never_pins("/tx/ab"));
    assert!(parse_asof_param(&AsOfQuery { asof: None })
        .unwrap()
        .is_none());
    assert!(parse_asof_param(&AsOfQuery {
        asof: Some(String::new())
    })
    .is_err());
    assert!(parse_asof_param(&AsOfQuery {
        asof: Some("zz".into())
    })
    .is_err());
    assert!(parse_hash32("aa").is_err());
    assert!(parse_hash32("zz".repeat(32).as_str()).is_err());
}

/// No-hub Esplora: empty mempool, flat fee estimates, POST /tx is 503.
/// Live `esplora_broadcast` always has a hub.
#[tokio::test]
async fn remaining_routes_fixture() {
    let (dir, q) = temp_query("remain");
    let (header, ta) = coinbase(0, Fk::NULL, None);
    q.connect_block(Height(0), &header, &[ta]).unwrap();
    let q = Arc::new(q);
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::clone(&q), None)
        .await
        .expect("listen");
    let addr = handle.local_addr;

    let (st, body) = http_get(addr, "/mempool").await;
    assert_eq!(st, 200, "{body}");
    let mem: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(mem["count"], 0);

    // No mempool, no estimate. Regtest returns an empty map so a client can
    // apply its own fallback. It must not invent a sat/vB.
    let (st, body) = http_get(addr, "/fee-estimates").await;
    assert_eq!(st, 200, "{body}");
    let estimates: Value = serde_json::from_str(&body).expect("fee-estimates json");
    assert_eq!(
        estimates,
        json!({}),
        "an empty estimator must not invent a target: {body}"
    );

    let main_cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Bitcoin);
    let main_handle = run_esplora(main_cfg, Arc::clone(&q), None)
        .await
        .expect("mainnet listen");
    let (st, body) = http_get(main_handle.local_addr, "/fee-estimates").await;
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("fee estimates unavailable"), "{body}");

    // mempool.space's tiers are its backend's /api/v1 surface, not Esplora's.
    for path in ["/fees/recommended", "/v1/fees/recommended"] {
        let (st, body) = http_get(addr, path).await;
        assert_eq!(st, 404, "{path}: {body}");
    }

    // mempool.space's websocket is its backend's /api/v1/ws, not Esplora's.
    for path in ["/ws", "/v1/ws"] {
        let (st, body) = http_get(addr, path).await;
        assert_eq!(st, 404, "{path}: {body}");
    }

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req =
        "POST /tx HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nab";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("503") || text.contains("mempool"),
        "expected 503 without hub: {text}"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reconstruct meters + no-hub empty mempool lists (HTTP JSON/raw/status ride
/// `esplora_broadcast_visible_in_rpc_and_electrum`).
#[allow(clippy::cognitive_complexity)] // meters + leftover header/height/status needles
#[tokio::test]
async fn block_raw_summary_status_and_mempool_routes() {
    use bitcoin::consensus::encode::deserialize;
    use bitcoin::hashes::Hash;
    use bitcoin::{Block, MerkleBlock};

    let (dir, q) = temp_query("p0-block");
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    let mut hashes = Vec::new();
    let mut coinbase_txids = Vec::new();
    for h in 0..3u32 {
        let (header, ta) = coinbase(h, prev, parent_hash);
        parent_hash = Some(header.hash);
        hashes.push(header.hash);
        coinbase_txids.push(ta.tx.txid);
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
    }
    let q = Arc::new(q);
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::clone(&q), None)
        .await
        .expect("listen");
    let addr = handle.local_addr;

    let _ = q.sample_reset_reconstruct_archived();
    let h1 = block_hash_hex(&hashes[1]);
    let (st, body) = http_get(addr, &format!("/block/{h1}")).await;
    assert_eq!(st, 200, "block json {body}");
    assert_eq!(
        q.sample_reset_reconstruct_archived(),
        0,
        "/block JSON uses stamped size/weight"
    );

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req =
        format!("GET /block/{h1}/raw HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http headers");
    let raw = &buf[sep + 4..];
    assert!(
        String::from_utf8_lossy(&buf[..sep]).contains("200"),
        "raw status"
    );
    let block: Block = deserialize(raw).expect("decode raw block");
    assert_eq!(block.txdata.len(), 1);
    assert!(
        q.sample_reset_reconstruct_archived() >= 1,
        "/block/:hash/raw must reconstruct wire"
    );

    let _ = q.sample_reset_reconstruct_archived();
    let (st, _) = http_get(addr, "/blocks").await;
    assert_eq!(st, 200);
    assert_eq!(
        q.sample_reset_reconstruct_archived(),
        0,
        "/blocks summaries use stamped size/weight"
    );

    let txid0 = block_hash_hex(&coinbase_txids[0]);
    let _ = q.sample_reset_reconstruct_archived();
    let (st, body) = http_get(addr, &format!("/tx/{txid0}/merkleblock-proof")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        q.sample_reset_reconstruct_archived(),
        0,
        "merkleblock-proof uses txid.body + header, not full reconstruct"
    );
    let mb_bytes = rbitcoin_primitives::hex_decode(&body).unwrap();
    let mb: MerkleBlock = deserialize(&mb_bytes).expect("merkleblock");
    let mut matches = Vec::new();
    let mut indexes = Vec::new();
    mb.extract_matches(&mut matches, &mut indexes).unwrap();
    assert_eq!(indexes, vec![0]);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0],
        bitcoin::Txid::from_byte_array(coinbase_txids[0])
    );
    let stored = q.wire_header_at_height(Height(0)).unwrap();
    assert_eq!(
        mb.header, stored,
        "merkleblock proof header is the stored block"
    );

    let (st, body) = http_get(addr, "/mempool/txids").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, "[]");
    let (st, body) = http_get(addr, "/mempool/recent").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, "[]");

    let sh = rbitcoin_store::script_hash(&[0x51]);
    let sh_hex = block_hash_hex(&sh);
    let (st, body) = http_get(addr, &format!("/scripthash/{sh_hex}/txs/mempool")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body, "[]");

    let (st, body) = http_get(addr, "/block-height/1").await;
    assert_eq!(st, 200, "block-height body={body}");
    assert_eq!(body, block_hash_hex(&hashes[1]));
    let (st, _) = http_get(addr, "/block-height/99").await;
    assert_eq!(st, 404);

    let hash_disp = block_hash_hex(&hashes[1]);
    let (st, body) = http_get(addr, &format!("/block/{hash_disp}/header")).await;
    assert_eq!(st, 200, "header body len={}", body.len());
    assert_eq!(body.len(), 160);
    let wire = q.wire_header_at_height(Height(1)).unwrap();
    let expected = encode_header_hex(&wire).unwrap();
    assert_eq!(body, expected);
    let miss = "ff".repeat(32);
    let (st, _) = http_get(addr, &format!("/block/{miss}/header")).await;
    assert_eq!(st, 404);

    let (fk, _) = q.get_tx_by_txid(&coinbase_txids[0]).unwrap().unwrap();
    let st_json = tx_status_json(&q, fk).unwrap();
    assert_eq!(st_json["confirmed"], true);
    assert_eq!(st_json["block_height"], 0);

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regtest P2WPKH address + scriptPubKey.
fn regtest_p2wpkh() -> (String, bitcoin::ScriptBuf) {
    regtest_p2wpkh_sk(7)
}

fn regtest_p2wpkh_sk(fill: u8) -> (String, bitcoin::ScriptBuf) {
    use bitcoin::key::CompressedPublicKey;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bitcoin::{Address, Network, PrivateKey};
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[fill; 32]).expect("sk");
    let pk = PrivateKey::new(sk, Network::Regtest);
    let cpk = CompressedPublicKey::from_private_key(&secp, &pk).expect("cpk");
    let addr = Address::p2wpkh(&cpk, Network::Regtest);
    let spk = addr.script_pubkey();
    (addr.to_string(), spk)
}

fn display_txid(txid: bitcoin::Txid) -> String {
    txid.to_string()
}

/// electrs: a start index at or past the last tx is 404, not an empty page.
#[tokio::test]
async fn block_txs_past_end_is_not_found() {
    let (dir, q) = temp_query("txs-range");
    let (header, ta) = coinbase(0, Fk::NULL, None);
    let hash = header.hash;
    q.connect_block(Height(0), &header, &[ta]).unwrap();
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    let h = block_hash_hex(&hash);
    let (st, body) = http_get(handle.local_addr, &format!("/block/{h}/txs/0")).await;
    assert_eq!(st, 200, "{body}");
    let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(rows.as_array().map(|a| a.len()), Some(1), "{body}");
    let (st, body) = http_get(handle.local_addr, &format!("/block/{h}/txs/25")).await;
    assert_eq!(st, 404, "{body}");
    assert!(body.contains("start index out of range"), "{body}");
    let (st, body) = http_get(handle.local_addr, &format!("/block/{h}/txs/1")).await;
    assert_eq!(st, 400, "{body}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spend of `prev:vout` with the given outputs; `wit` adds a witness stack.
fn summary_spend(tag: u8, prev: [u8; 32], vout: u32, outs: &[u64], wit: bool) -> TxApply {
    let mut txid = [tag; 32];
    txid[31] = 0x5a;
    TxApply {
        tx: TxRecord {
            txid,
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: outs.len() as u32,
        },
        inputs: vec![InputRecord {
            prev_txid: prev,
            parent: rbitcoin_store::InputParent::Unlinked { vout },
            sequence: u32::MAX,
            script_sig: vec![],
            witness: if wit {
                vec![vec![0x30; 71], vec![0x02; 33]]
            } else {
                vec![]
            },
        }],
        outputs: outs
            .iter()
            .map(|&v| OutputRecord::unspent(v, vec![0x51]))
            .collect(),
    }
}

async fn assert_block_routes_not_found(addr: SocketAddr, hash: &str, reason: &str) {
    for suffix in [
        "",
        "/status",
        "/header",
        "/raw",
        "/txids",
        "/txid/0",
        "/txs",
        "/txs/0",
        "/txs/summary",
    ] {
        let (st, _, _) = http_get_raw(addr, &format!("/block/{hash}{suffix}")).await;
        assert_eq!(st, 404, "{reason}: {suffix}");
    }
}

/// `/block/:hash/txs/summary`: one `[txid, fee, weight, value]` row per tx in
/// `/txids` order, coinbase first with fee 0, the same chain-tip stamp, and
/// the same archive eligibility as `/txids`, including current-process reorgs.
#[tokio::test]
async fn block_txs_summary_rows_and_misses() {
    let (dir, q) = temp_query("blk-txs-summary");
    let mut prev = Fk::NULL;
    let mut parent_hash = None;
    let mut cbs = Vec::new();
    for h in 0..3u32 {
        let (header, ta) = coinbase(h, prev, parent_hash);
        parent_hash = Some(header.hash);
        cbs.push(ta.tx.txid);
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
    }
    let cb_value = 50_0000_0000u64;
    let a_outs = [20_0000_0000, 0, 29_9999_0000];
    let a = summary_spend(0xa1, cbs[0], 0, &a_outs, true);
    let b = summary_spend(0xb2, cbs[1], 0, &[cb_value - 2_500], false);
    let c = summary_spend(0xc3, a.tx.txid, 2, &[29_9998_0000, 0], true);
    let prevout_value = [cb_value, cb_value, a_outs[2]];
    let (h3, cb3) = coinbase(3, prev, parent_hash);
    let applies = vec![cb3, a, b, c];
    q.connect_block(Height(3), &h3, &applies).unwrap();
    let q = Arc::new(q);
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::clone(&q), None)
        .await
        .expect("listen");
    let addr = handle.local_addr;
    let hash = block_hash_hex(&h3.hash);

    let (st, raw, body) = http_get_raw(addr, &format!("/block/{hash}/txs/summary")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        header_value(&raw, HDR_CHAIN_TIP).as_deref(),
        Some(hash.as_str())
    );
    let rows: Vec<Value> = serde_json::from_str(&body).unwrap();
    let (st, ids) = http_get(addr, &format!("/block/{hash}/txids")).await;
    assert_eq!(st, 200, "{ids}");
    let ids: Vec<String> = serde_json::from_str(&ids).unwrap();
    assert_eq!(rows.len(), applies.len(), "{body}");
    for (i, (row, ta)) in rows.iter().zip(&applies).enumerate() {
        let tx = rbitcoin_query::testutil::tx_apply_to_tx(ta);
        let out_sum: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
        let fee = if i == 0 {
            0
        } else {
            prevout_value[i - 1] - out_sum
        };
        let want = json!([ids[i], fee, tx.weight().to_wu(), out_sum]);
        assert_eq!(row, &want, "row {i}");
    }
    assert_eq!(rows[3][1], 1_0000, "{body}");

    let unknown = "ff".repeat(32);
    for (path, want) in [
        (format!("/block/{unknown}/txs/summary"), 404),
        ("/block/zz/txs/summary".to_string(), 404),
        (format!("/block/{hash}/txs/0"), 200),
    ] {
        let (st, body) = http_get(addr, &path).await;
        assert_eq!(st, want, "{path}: {body}");
    }

    let parent_fk = q.header_at_height(Height(3)).unwrap().unwrap().0;
    let (header_only, header_only_cb) = coinbase(4, parent_fk, Some(h3.hash));
    q.put_header(&header_only).unwrap();
    let header_only_hex = block_hash_hex(&header_only.hash);
    assert_block_routes_not_found(addr, &header_only_hex, "header without an archived body").await;

    q.commit_class_a_only(&header_only, &[header_only_cb])
        .unwrap();
    assert_block_routes_not_found(addr, &header_only_hex, "archived body that never connected")
        .await;

    q.disconnect_tip().unwrap();
    let (mut alt, alt_cb) = coinbase(3, prev, parent_hash);
    alt.nonce += 1;
    alt.hash = rbitcoin_store::block_header_hash(
        alt.version,
        &parent_hash.unwrap(),
        &alt.merkle_root,
        alt.timestamp,
        alt.bits,
        alt.nonce,
    );
    q.connect_block(Height(3), &alt, &[alt_cb]).unwrap();
    let (st_ids, stale_ids) = http_get(addr, &format!("/block/{hash}/txids")).await;
    let (st, body) = http_get(addr, &format!("/block/{hash}/txs/summary")).await;
    assert_eq!((st, st_ids), (200, 200), "reorged out: {body}");
    assert_eq!(serde_json::from_str::<Vec<Value>>(&body).unwrap(), rows);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&stale_ids).unwrap(),
        ids
    );
    let alt_hex = block_hash_hex(&alt.hash);
    let (st, body) = http_get(addr, &format!("/block/{alt_hex}/txs/summary")).await;
    assert_eq!(st, 200, "{body}");
    let alt_rows: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(alt_rows.len(), 1, "{body}");

    let first = q
        .store()
        .header_txs
        .get_range(parent_fk)
        .unwrap()
        .unwrap()
        .0;
    q.store()
        .write_txstat_row(
            first,
            &rbitcoin_store::TxStatRow {
                fee_sat: 0,
                base: 0,
                wit_extra: 0,
            },
        )
        .unwrap();
    let rebuilds_before = q.reconstruct_archived.load(Ordering::Relaxed);
    for suffix in ["", "/txs/summary"] {
        let (st, _, _) = http_get_raw(addr, &format!("/block/{hash}{suffix}")).await;
        assert_eq!(
            st, 404,
            "unstamped stale summary must not reconstruct: {suffix}"
        );
    }

    assert_eq!(
        q.reconstruct_archived.load(Ordering::Relaxed),
        rebuilds_before,
        "summary requests must not reconstruct an unstamped stale block"
    );

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

struct ScriptHold {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl ScriptHold {
    fn all() -> Self {
        let n = rbitcoin_consensus::script_pool::worker_spawn_count();
        assert!(n >= 1, "script pool did not start");
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let parked = Arc::new((Mutex::new(()), std::sync::Condvar::new()));
        for _ in 0..n {
            let entered = Arc::clone(&entered);
            let gate = Arc::clone(&gate);
            let parked = Arc::clone(&parked);
            rbitcoin_consensus::script_pool::spawn_detached(move || {
                {
                    let _g = parked.0.lock().unwrap_or_else(|p| p.into_inner());
                    entered.fetch_add(1, Ordering::SeqCst);
                    parked.1.notify_all();
                }
                let (lock, cv) = &*gate;
                let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
                while !*g {
                    g = cv.wait(g).unwrap_or_else(|p| p.into_inner());
                }
            });
        }
        let start = Instant::now();
        let (lock, cv) = &*parked;
        let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
        while entered.load(Ordering::SeqCst) < n {
            let left = Duration::from_secs(2).saturating_sub(start.elapsed());
            assert!(!left.is_zero(), "script workers did not park");
            let (next, _timeout) = cv.wait_timeout(g, left).unwrap_or_else(|p| p.into_inner());
            g = next;
        }
        Self { gate }
    }
}

impl Drop for ScriptHold {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }
}

/// A template at 4 KiB keeps `cache-control: no-store` and does not encode
/// until a script worker is free.
#[test]
fn heavy_template_encode_waits_for_a_script_worker() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread;

    let chars = rbitcoin_electrum::JSON_CPU_MIN - 2;
    let body = Arc::new(Value::String("a".repeat(chars)));
    assert!(rbitcoin_electrum::json_is_cpu_heavy(&body));
    let hold = ScriptHold::all();
    let resp = handlers::gbt_json(&body);
    let cc = resp
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(cc, "no-store");

    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut pending = Box::pin(ensure_json(resp));
    assert!(
        matches!(pending.as_mut().poll(&mut cx), Poll::Pending),
        "a template at 4 KiB encoded while script workers were held"
    );
    drop(hold);
    let start = Instant::now();
    let ready = loop {
        match pending.as_mut().poll(&mut cx) {
            Poll::Ready(resp) => break resp,
            Poll::Pending => {
                assert!(start.elapsed() < Duration::from_secs(2));
                thread::park_timeout(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(ready.status(), axum::http::StatusCode::OK);
    let mut body_fut = Box::pin(axum::body::to_bytes(ready.into_body(), usize::MAX));
    let bytes = loop {
        match body_fut.as_mut().poll(&mut cx) {
            Poll::Ready(bytes) => break bytes.expect("template body"),
            Poll::Pending => {
                assert!(start.elapsed() < Duration::from_secs(2));
                thread::park_timeout(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(bytes.as_ref(), serde_json::to_vec(body.as_ref()).unwrap());
}

fn queued_join() -> InflightJoin {
    InflightJoin {
        done: Mutex::new(None),
        cv: std::sync::Condvar::new(),
        seq: 1,
        queued: Mutex::new(false),
        queued_cv: std::sync::Condvar::new(),
    }
}

#[test]
fn a_flag_set_before_the_waiter_arms_is_still_visible() {
    let inf = queued_join();
    {
        let mut queued = inf.queued.lock().unwrap_or_else(|p| p.into_inner());
        *queued = true;
        inf.queued_cv.notify_all();
    }
    assert!(
        inf.wait_queued(Duration::from_millis(20)),
        "a notify with no waiter must still be visible on the flag"
    );
}

#[test]
fn a_waiter_that_locks_first_wakes_when_the_flag_is_stored() {
    let inf = Arc::new(queued_join());
    let waiting = Arc::clone(&inf);
    let waiter = std::thread::spawn(move || waiting.wait_queued(Duration::from_secs(2)));
    {
        let mut queued = inf.queued.lock().unwrap_or_else(|p| p.into_inner());
        *queued = true;
        inf.queued_cv.notify_all();
    }
    assert!(
        waiter.join().expect("waiter"),
        "the queued flag is under the mutex the waiter holds"
    );
}

/// `/blocks` takes the shared store budget. A held permit stalls the GET.
/// Releasing it lets the handler finish. The route uses `spawn_join`.
#[tokio::test]
async fn esplora_blocks_waits_on_the_store_budget() {
    let (dir, q) = temp_query("esplora-budget");
    q.set_api_read_budget(1);
    let q = Arc::new(q);
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::clone(&q), None)
        .await
        .expect("listen");
    let hold = q.api_read_budget().acquire_owned().await.expect("permit");
    let addr = handle.local_addr;
    let stalled = tokio::spawn(async move { http_get(addr, "/blocks").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !stalled.is_finished(),
        "GET /blocks ran while the only store permit was held"
    );
    drop(hold);
    let (st, body) = tokio::time::timeout(Duration::from_secs(5), stalled)
        .await
        .expect("GET finishes after the permit")
        .expect("join");
    assert_eq!(st, 200, "{body}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The 15s cache mutex is not held while `template_prev` waits on connect.
#[tokio::test]
async fn block_template_does_not_lock_the_cache_across_template_prev() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("gbt-cache-hold").expect("dir");
    let hub = rbitcoin_net::ChainHub::new(
        Query::open_or_create_tiny(dir.path().join("store")).unwrap(),
        rbitcoin_consensus::ChainParams::regtest(),
        rbitcoin_consensus::Milestone::NONE,
    );
    hub.ensure_genesis().unwrap();
    let q = Arc::clone(&hub.query);
    let mp = rbitcoin_net::MempoolHub::open_with_weight(
        dir.path().join("mempool"),
        Arc::clone(&q),
        300_000_000,
    )
    .unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let mp_hold = Arc::clone(&mp);
    let holder = std::thread::spawn(move || {
        let hold = mp_hold.template_hold();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(hold);
    });
    held_rx.recv().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let calls_fn = Arc::clone(&calls);
    let mp_fn = Arc::clone(&mp);
    let st = AppState {
        query: q,
        network: Network::Regtest,
        mempool: Some(mp),
        max_body: 1 << 20,
        sh_join: Arc::new(Mutex::new(JoinCache::default())),
        join_header_trusted: false,
        block_template: Some(BlockTemplateFn(Arc::new(move || {
            let _ = mp_fn.template_prev();
            calls_fn.fetch_add(1, Ordering::Relaxed);
            Ok(json!({"height": 1}))
        }))),
        gbt_cache: Arc::new(Mutex::new(None)),
    };
    let cache = Arc::clone(&st.gbt_cache);
    let task = tokio::spawn(async move { handlers::block_template(State(st)).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !task.is_finished(),
        "block-template returned while template_prev was held"
    );
    assert!(
        cache.try_lock().is_ok(),
        "gbt cache held across template_prev"
    );
    release_tx.send(()).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("block-template finishes after the hold")
        .expect("join");
    assert_eq!(response.status(), 200);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    holder.join().unwrap();
    let _ = std::fs::remove_dir_all(dir.path());
}

include!("esplora_sh_journey.rs");
include!("esplora_http_journey.rs");
include!("blindbit_journey.rs");
