use axum::Router;
use rbitcoin_consensus::ChainParams;
use rbitcoin_electrum::{run_electrum, ElectrumConfig, TipNotify, LISTEN_DOWN, LISTEN_UP};
use rbitcoin_log::progress;
use rbitcoin_node::health::*;
use rbitcoin_primitives::Network;
use rbitcoin_query::Query;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

fn following() -> ReadySnapshot {
    ReadySnapshot {
        phase: Phase::Following,
        in_ibd: false,
        blocks: 100,
        headers: 100,
        tip_age_secs: Some(0),
        max_tip_age_secs: 24 * 60 * 60,
        sh_lag: None,
        electrum_listen: rbitcoin_electrum::LISTEN_OFF,
    }
}

/// Phase, header-lag, scripthash-lag, and tip-age boundaries. One live
/// node would need a header fork, a stalled index, and a tip older than
/// `--max-tip-age` at once. `node_listen_and_exit` owns the HTTP answers
/// a probe sees: live during initial block download and ready after a fresh
/// block.
#[test]
fn readiness_reports_the_first_failing_gate() {
    let mut cases = Vec::new();
    for phase in Phase::ALL {
        let want = match phase {
            Phase::Following => Ok(()),
            other => Err(other.as_str().to_string()),
        };
        cases.push((
            ReadySnapshot {
                phase,
                in_ibd: phase != Phase::Following,
                ..following()
            },
            want,
        ));
    }
    cases.push((
        ReadySnapshot {
            in_ibd: true,
            headers: 200,
            ..following()
        },
        Err("initial block download".into()),
    ));
    let behind = |n: u32, sh_lag: Option<u32>| ReadySnapshot {
        headers: 100 + n,
        sh_lag,
        ..following()
    };
    cases.push((behind(READY_LAG_BLOCKS, None), Ok(())));
    cases.push((
        behind(READY_LAG_BLOCKS + 1, Some(0)),
        Err(format!(
            "tip {} blocks behind headers",
            READY_LAG_BLOCKS + 1
        )),
    ));
    cases.push((behind(0, Some(READY_LAG_BLOCKS)), Ok(())));
    cases.push((
        behind(0, Some(READY_LAG_BLOCKS + 1)),
        Err(format!(
            "scripthash index {} blocks behind tip",
            READY_LAG_BLOCKS + 1
        )),
    ));
    cases.push((
        ReadySnapshot {
            headers: 90,
            ..following()
        },
        Ok(()),
    ));
    let stale = |age: u64| ReadySnapshot {
        tip_age_secs: Some(age),
        ..following()
    };
    cases.push((stale(24 * 60 * 60), Ok(())));
    cases.push((
        stale(24 * 60 * 60 + 1),
        Err("tip stale (last block 86401s ago)".into()),
    ));
    cases.push((
        ReadySnapshot {
            tip_age_secs: None,
            ..following()
        },
        Ok(()),
    ));
    for (snap, want) in cases {
        assert_eq!(readiness(&snap), want, "{snap:?}");
    }
}

/// One listener, one gate. A timed-out `/readyz` keeps its permit, so the
/// next probe is 503 at once. After that work finishes, `/readyz` and
/// `/metrics` answer from the node again.
#[tokio::test(flavor = "multi_thread")]
async fn health_gate_caps() {
    let health = Health::new(NodeStatus::new(Network::Regtest, false));
    let addr = serve(router_for(health.clone(), true)).await;
    let gate = Arc::clone(&health.readyz);

    let held = Arc::clone(&gate)
        .acquire_many_owned((READYZ_IN_FLIGHT - 1) as u32)
        .await
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        gated(&gate, move || wait.recv().unwrap()),
    )
    .await;
    assert!(dropped.is_err(), "the request gave up while the work ran");
    assert_eq!(
        gate.available_permits(),
        0,
        "a timed-out request keeps its permit"
    );
    assert_eq!(
        get_soon(addr, "/readyz").await,
        "HTTP/1.1 503 Service Unavailable|not ready: busy\n"
    );

    let scrape_held = Arc::clone(&health.scrape).acquire_owned().await.unwrap();
    assert_eq!(
        get_soon(addr, "/metrics").await,
        "HTTP/1.1 503 Service Unavailable|metrics: a scrape is already running\n"
    );

    release.send(()).unwrap();
    drop(held);
    drop(scrape_held);
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.available_permits() < READYZ_IN_FLIGHT {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the permit returns when the work ends");
    assert_eq!(
        get_soon(addr, "/readyz").await,
        "HTTP/1.1 503 Service Unavailable|not ready: opening\n"
    );
    let scraped = get_soon(addr, "/metrics").await;
    assert!(
        scraped.starts_with("HTTP/1.1 200 OK|# HELP rbitcoin_build_info"),
        "{scraped}"
    );
}

#[test]
fn progress_body_shapes() {
    assert_eq!(
        progress_body(Phase::Opening, None, None),
        serde_json::json!({ "phase": "opening", "stage": null, "finished": null })
    );
    let p = progress::Snapshot {
        stage: "scripthash keys collect",
        done: 1,
        total: 3,
        base: 0,
        elapsed: Duration::from_secs(60),
        started_at: std::time::SystemTime::UNIX_EPOCH,
    };
    assert_eq!(
        progress_body(Phase::Opening, Some(&p), None),
        serde_json::json!({
            "phase": "opening",
            "stage": "scripthash keys collect",
            "done": 1,
            "total": 3,
            "percent": 33.33,
            "elapsed_secs": 60,
            "eta_secs": 120,
            "finished": null,
        })
    );
    assert_eq!(
        progress_body(Phase::Following, None, Some(&p)),
        serde_json::json!({
            "phase": "following",
            "stage": null,
            "finished": {
                "stage": "scripthash keys collect",
                "done": 1,
                "total": 3,
                "elapsed_secs": 60,
            },
        })
    );
    let unknown = progress::Snapshot { total: 0, ..p };
    let body = progress_body(Phase::Indexing, Some(&unknown), None);
    assert_eq!(body["percent"], serde_json::Value::Null);
    assert_eq!(body["eta_secs"], serde_json::Value::Null);
}

/// A running stage shows on `/progress` and as `/metrics` gauges served by
/// the health router. `/readyz` keeps its reason text as is.
#[tokio::test(flavor = "multi_thread")]
async fn running_stage_is_visible_on_every_route() {
    let addr = serve(router(NodeStatus::new(Network::Regtest, false), true)).await;
    // The registry is process-global and a store test in this binary may
    // start its own stage; retry a few times rather than flake.
    let mut last = String::new();
    for _ in 0..5 {
        let stage = progress::begin("health test stage", 8);
        stage.set_done(2);
        let p = get_soon(addr, "/progress").await;
        let r = get_soon(addr, "/readyz").await;
        let m = get_soon(addr, "/metrics").await;
        drop(stage);
        let ok = p.starts_with("HTTP/1.1 200 OK|")
            && p.contains(r#""stage":"health test stage""#)
            && p.contains(r#""done":2"#)
            && p.contains(r#""total":8"#)
            && p.contains(r#""percent":25.0"#)
            && r == "HTTP/1.1 503 Service Unavailable|not ready: opening\n"
            && m.contains("rbitcoin_progress_done{stage=\"health test stage\"} 2\n")
            && m.contains("rbitcoin_progress_target{stage=\"health test stage\"} 8\n");
        if ok {
            return;
        }
        last = format!("{p}\n---\n{r}\n---\n{m}");
    }
    panic!("stage not visible:\n{last}");
}

/// `/metrics` is absent without `--metrics`.
#[tokio::test]
async fn metrics_route_needs_the_flag() {
    let addr = serve(router(NodeStatus::new(Network::Regtest, false), false)).await;
    let m = get_soon(addr, "/metrics").await;
    assert!(m.starts_with("HTTP/1.1 404 Not Found|"), "{m}");
}

/// Idle TCP to the health listener is capped, and a socket that sends
/// nothing is closed. A client must not be able to hold a connection until
/// the process runs out of descriptors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_cap_idle_health_is_closed() {
    let status = NodeStatus::new(Network::Regtest, false);
    let handle = run_health("127.0.0.1:0".parse().unwrap(), status, false)
        .await
        .expect("health listen");
    let addr = handle.local_addr;

    let mut opened = Vec::new();
    for _ in 0..80 {
        opened.push(TcpStream::connect(addr).await.expect("connect"));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut still = Vec::new();
    loop {
        let mut probes = tokio::task::JoinSet::new();
        for mut sock in opened {
            probes.spawn(async move {
                let open = tcp_still_open(&mut sock).await;
                (sock, open)
            });
        }
        still.clear();
        while let Some(joined) = probes.join_next().await {
            let (sock, open) = joined.expect("probe");
            if open {
                still.push(sock);
            }
        }
        if !still.is_empty() && still.len() <= HEALTH_MAX_CONNECTIONS {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        opened = std::mem::take(&mut still);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        !still.is_empty() && still.len() <= HEALTH_MAX_CONNECTIONS,
        "health held {} idle connections; cap is {HEALTH_MAX_CONNECTIONS}",
        still.len()
    );
    let mut idle = still.pop().expect("accepted idle client");
    drop(still);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        tcp_still_open(&mut idle).await,
        "idle health connection closed before the header timeout"
    );
    let closed = tokio::time::timeout(REQUEST_TIMEOUT + Duration::from_secs(2), async {
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
        "an idle health connection was still open after the header timeout"
    );
    drop(handle);
}

/// `/readyz` is not ok after the Electrum accept task stops on its own.
#[tokio::test]
async fn listener_cap_readyz_reports_electrum_down() {
    let status = NodeStatus::new(Network::Regtest, false);
    status.enter(Phase::Following);
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-health-electrum-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let q = Query::open_or_create_tiny(&dir).expect("query");
    let (tip_tx, _) = tokio::sync::broadcast::channel::<TipNotify>(4);
    let mut cfg =
        ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &ChainParams::regtest());
    cfg.listen_state = Some(Arc::clone(&status.electrum_listen));
    let mut electrum = run_electrum(cfg, Arc::new(q), ChainParams::regtest(), tip_tx, None)
        .await
        .expect("electrum");
    let health = run_health("127.0.0.1:0".parse().unwrap(), Arc::clone(&status), false)
        .await
        .expect("health listen");
    let marked = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if status.electrum_listen.load(Ordering::SeqCst) == LISTEN_UP {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        marked.is_ok(),
        "electrum accept task never marked the listener up"
    );
    let ready = get_soon(health.local_addr, "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 200 OK|ok\n"), "{ready}");
    let task = electrum.tasks.pop().expect("accept task");
    task.abort();
    let _ = task.await;
    assert_eq!(
        status.electrum_listen.load(Ordering::SeqCst),
        LISTEN_DOWN,
        "accept task stopped without shutdown and left the listener up"
    );
    let body = get_soon(health.local_addr, "/readyz").await;
    assert!(
        body.starts_with("HTTP/1.1 503 Service Unavailable|not ready: electrum listener down\n"),
        "{body}"
    );
    drop(health);
    drop(electrum);
    let _ = std::fs::remove_dir_all(&dir);
}

async fn tcp_still_open(sock: &mut TcpStream) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 1];
    match tokio::time::timeout(Duration::from_millis(40), sock.read(&mut buf)).await {
        Err(_) => true,
        Ok(Ok(0)) | Ok(Err(_)) => false,
        Ok(Ok(_)) => true,
    }
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// [`get`], failing rather than hanging if the answer waits for a permit.
async fn get_soon(addr: SocketAddr, path: &str) -> String {
    tokio::time::timeout(Duration::from_secs(2), get(addr, path))
        .await
        .expect("answered without queueing")
}

async fn get(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    format!("{}|{body}", head.lines().next().unwrap())
}

mod metrics;
