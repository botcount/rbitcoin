use bitcoin::p2p::Magic;
use rbitcoin_consensus::{ChainParams, Milestone};
use rbitcoin_net::error::NetError;
use rbitcoin_net::peers::PeerConnType;
use rbitcoin_net::service::*;
use rbitcoin_query::Query;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// First handshaken row in the table, polled without sleeping so a row
/// published before its fields are set is the one seen.
fn first_handshaken(
    peers: Arc<rbitcoin_net::peers::PeerHub>,
    inbound: bool,
) -> tokio::task::JoinHandle<Option<rbitcoin_net::peers::PeerInfo>> {
    tokio::task::spawn_blocking(move || {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            let row = peers
                .snapshot()
                .into_iter()
                .find(|p| p.inbound == inbound && p.handshake_complete);
            if row.is_some() {
                return row;
            }
        }
        None
    })
}

#[test]
fn magic_for_all_networks() {
    assert_eq!(
        magic_for_params(&ChainParams::mainnet()),
        Magic::from(bitcoin::Network::Bitcoin)
    );
    assert_eq!(
        magic_for_params(&ChainParams::testnet()),
        Magic::from(bitcoin::Network::Testnet)
    );
    assert_eq!(
        magic_for_params(&ChainParams::signet()),
        Magic::from(bitcoin::Network::Signet)
    );
    assert_eq!(magic_for_params(&ChainParams::regtest()), Magic::REGTEST);
    assert_eq!(
        magic_for_params(&ChainParams::testnet4()),
        Magic::from(bitcoin::Network::Testnet4)
    );
    assert_eq!(
        magic_for_params(&ChainParams::testnet5()),
        Magic::from_bytes(*b"FIVE"),
        "BIP 95 message start"
    );
}

#[test]
fn custom_signet_uses_challenge_derived_magic() {
    use bitcoin::ScriptBuf;

    let challenge = ScriptBuf::from_bytes(vec![0x51]);
    let params = ChainParams::custom_signet(challenge, 60).unwrap();
    assert_eq!(
        magic_for_params(&params),
        Magic::from_bytes([0x54, 0xd2, 0x6f, 0xbd])
    );
}

/// A feeler does not use `prepare_outbound_session`. It still must not
/// open a socket to an address that misbehaved.
#[tokio::test]
async fn feeler_does_not_dial_a_discouraged_address() {
    let peers = rbitcoin_net::peers::PeerHub::new();
    peers.set_mock_now(1_700_000_000);
    let addr = std::net::SocketAddr::from(([1, 2, 3, 4], 8333));
    peers.note_misbehavior_addr(addr.ip());
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("feeler-discouraged");
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        run_outbound_session(
            rbitcoin_net::peers::DialTarget::Socket(addr),
            None,
            Magic::REGTEST,
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            std::sync::Arc::new(hub),
            peers,
            "/rbitcoin:test/".into(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            rbitcoin_net::peers::PeerConnType::Feeler,
            rbitcoin_net::socks::Dialer::Direct,
        ),
    )
    .await;
    let _ = std::fs::remove_dir_all(dir.path());
    let err = result
        .expect("a refused feeler returns before the connect")
        .unwrap_err();
    assert!(
        matches!(err, NetError::Protocol("discouraged")),
        "feeler dialed a discouraged address, got {err:?}"
    );
}

/// `--proxy` turns the DNS seeds into addr-fetch dials by name. The
/// proxy resolves the name, so the node holds no address for it.
#[tokio::test]
async fn proxied_dns_seed_dials_by_name_through_socks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let socks = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut greeting = [0u8; 2];
        s.read_exact(&mut greeting).await.unwrap();
        let mut methods = vec![0u8; greeting[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        if methods.contains(&0x02) {
            s.write_all(&[5, 0x02]).await.unwrap();
            let mut ulen = [0u8; 2];
            s.read_exact(&mut ulen).await.unwrap();
            let mut user = vec![0u8; ulen[1] as usize];
            s.read_exact(&mut user).await.unwrap();
            let mut plen = [0u8; 1];
            s.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            s.read_exact(&mut pass).await.unwrap();
            s.write_all(&[1, 0]).await.unwrap();
        } else {
            s.write_all(&[5, 0x00]).await.unwrap();
        }
        let mut req = [0u8; 5];
        s.read_exact(&mut req).await.unwrap();
        assert_eq!(req[3], 3, "ATYP domain");
        let mut host = vec![0u8; req[4] as usize];
        s.read_exact(&mut host).await.unwrap();
        let mut port = [0u8; 2];
        s.read_exact(&mut port).await.unwrap();
        s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
        (String::from_utf8(host).unwrap(), u16::from_be_bytes(port))
    });
    let peers = rbitcoin_net::peers::PeerHub::new();
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("proxied-dns-seed");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_outbound_session(
            rbitcoin_net::peers::DialTarget::Domain {
                host: "seed.example".into(),
                port: 8333,
            },
            None,
            Magic::REGTEST,
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            std::sync::Arc::new(hub),
            peers,
            "/rbitcoin:test/".into(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            rbitcoin_net::peers::PeerConnType::AddrFetch,
            rbitcoin_net::socks::Dialer::socks(proxy, false),
        ),
    )
    .await;
    let _ = std::fs::remove_dir_all(dir.path());
    assert!(
        result
            .expect("the session ends when the proxy closes")
            .is_err(),
        "the proxy closed before VERSION"
    );
    assert_eq!(socks.await.unwrap(), ("seed.example".to_string(), 8333));
}

/// One datadir, two node lifetimes. The first is dropped without
/// shutdown. The second binds loopback as with onion listen, adds a
/// second listener, and shuts down with one session task that finishes
/// in the grace window and one that would linger.
#[tokio::test]
async fn service_listen_shutdown() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-p2p-listen-shutdown-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let node = P2PNode::start(
        "127.0.0.1:0".parse().unwrap(),
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    assert!(node.hub.shared_arc().is_some());
    let t0 = std::time::Instant::now();
    drop(node);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "dropping the node must abort connect-retry without waiting out the runtime"
    );

    let mut node = P2PNode::start_with_dialer(
        "127.0.0.1:0".parse().unwrap(),
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(onion)/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    assert_eq!(node.local_addr.ip(), std::net::Ipv4Addr::LOCALHOST);
    assert_ne!(node.local_addr.port(), 0);
    let extra = node
        .add_listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    assert_ne!(extra, node.local_addr);
    assert!(std::net::TcpStream::connect(extra).is_ok());

    tokio::time::pause();
    let go = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(AtomicBool::new(false));
    let (go_task, flag) = (Arc::clone(&go), Arc::clone(&done));
    node.tasks.push(tokio::spawn(async move {
        go_task.notified().await;
        flag.store(true, Ordering::SeqCst);
    }));
    node.tasks.push(tokio::spawn(std::future::pending()));
    let started = tokio::time::Instant::now();
    let mut shutdown = std::pin::pin!(node.shutdown());
    for _ in 0..8 {
        tokio::select! {
            biased;
            _ = &mut shutdown => panic!("shutdown returned before the grace advance"),
            _ = tokio::task::yield_now() => {}
        }
    }
    go.notify_one();
    for _ in 0..8 {
        if done.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        done.load(Ordering::SeqCst),
        "the short task runs when released, before the grace elapses"
    );
    tokio::time::advance(Duration::from_millis(750)).await;
    let mut finished = false;
    for _ in 0..64 {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                finished = true;
                break;
            }
            _ = tokio::task::yield_now() => {}
        }
    }
    assert!(finished, "shutdown did not return on the grace advance");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(750),
        "shutdown returned before the grace: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown waited past the grace: {elapsed:?}"
    );
    tokio::time::resume();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2p_outbound_only_dials_without_listener() {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-outbound-only-{n}"));
    std::fs::create_dir_all(dir.join("seed")).unwrap();
    std::fs::create_dir_all(dir.join("follow")).unwrap();
    let qa = Query::open_or_create_tiny(dir.join("seed")).unwrap();
    let qb = Query::open_or_create_tiny(dir.join("follow")).unwrap();
    let params = ChainParams::regtest();
    let seeder = P2PNode::start_with_agent(
        "127.0.0.1:0".parse().unwrap(),
        qa,
        params.clone(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(seed)/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let mut follower = P2PNode::start_outbound_only(
        qb,
        params,
        Milestone::NONE,
        "/rbitcoin:0.1.0(follow)/".into(),
        0,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    assert_eq!(
        follower.local_addr,
        "127.0.0.1:0".parse().unwrap(),
        "outbound-only must not bind a P2P port"
    );
    assert_eq!(follower.max_inbound, 0);
    let inbound_err = follower
        .peers
        .addconnection(seeder.local_addr, PeerConnType::Inbound)
        .unwrap_err();
    assert!(
        inbound_err.contains("inbound"),
        "addconnection inbound must refuse: {inbound_err}"
    );

    follower.follow_from(seeder.local_addr).await.unwrap();
    let mut linked = false;
    for _ in 0..100 {
        if follower.follow_live_count() >= 1 && !seeder.peers.snapshot().is_empty() {
            linked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    seeder.shutdown().await;
    follower.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(linked, "outbound-only follower must handshake the seeder");
}

/// Outbound half of the `getpeerinfo` time fields: the dialing row keeps
/// `conntime` across the post-VERSION swap, and its counters see the
/// responder's handshake bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_row_keeps_conntime_through_handshake() {
    use rbitcoin_net::peer::{connect_and_handshake_timed, HandshakePolicy};

    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-outbound-conntime-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(&dir).unwrap();
    let node = P2PNode::start_outbound_only(
        q,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(dialer)/".into(),
        0,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let far = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let far_addr = far.local_addr().unwrap();
    const T0: u64 = 1_700_000_000;
    node.peers.set_mock_now(T0);
    node.peers
        .addconnection(far_addr, PeerConnType::OutboundFullRelay)
        .unwrap();
    let (stream, _) = far.accept().await.unwrap();
    let mut connecting = None;
    for _ in 0..200 {
        connecting = node.peers.snapshot().into_iter().find(|p| !p.inbound);
        if connecting.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let connecting = connecting.expect("connecting outbound row");
    assert!(!connecting.handshake_complete);
    assert_eq!(connecting.conntime, T0);
    assert_eq!(connecting.last_recv, 0, "the responder has sent nothing");
    assert_eq!(connecting.version, 0);
    node.peers.set_mock_now(T0 + 30);
    let watch = first_handshaken(node.peers.clone(), false);
    let _hs = connect_and_handshake_timed(
        Duration::from_secs(5),
        stream,
        Magic::REGTEST,
        far_addr,
        far_addr,
        0,
        true,
        "/rbitcoin:test/",
        HandshakePolicy::plain(),
    )
    .await
    .unwrap();
    let done = watch.await.unwrap();
    node.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
    let done = done.expect("handshaken outbound row");
    assert_eq!(done.id, connecting.id);
    assert_eq!(done.conntime, T0, "the handshaken row keeps conntime");
    assert_eq!(done.last_recv, T0 + 30);
    assert_eq!(done.version, rbitcoin_net::peer::OUR_PROTOCOL_VERSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_onion_loopback_inbound_handshake() {
    use bitcoin::p2p::Magic;
    use rbitcoin_net::peer::{connect_and_handshake_timed, HandshakePolicy};
    use tokio::net::TcpStream;

    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-listen-onion-hs-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let q = Query::open_or_create_tiny(&dir).unwrap();
    let node = P2PNode::start_with_dialer(
        "127.0.0.1:0".parse().unwrap(),
        q,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(onion)/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    // Core `rpc_net.py`: conntime is set once at connect, lastsend and
    // lastrecv stay 0 until bytes move, and v2 handshake bytes move them.
    const T0: u64 = 1_700_000_000;
    node.peers.set_mock_now(T0);
    let stream = TcpStream::connect(node.local_addr).await.unwrap();
    let mut connecting = None;
    for _ in 0..200 {
        connecting = node.peers.snapshot().into_iter().find(|p| p.inbound);
        if connecting.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let connecting = connecting.expect("connecting inbound row");
    assert!(!connecting.handshake_complete);
    assert_eq!(connecting.conntime, T0);
    assert_eq!((connecting.last_send, connecting.last_recv), (0, 0));
    assert_eq!(connecting.version, 0);
    node.peers.set_mock_now(T0 + 30);
    let watch = first_handshaken(node.peers.clone(), true);
    let _hs = connect_and_handshake_timed(
        Duration::from_secs(5),
        stream,
        Magic::REGTEST,
        node.local_addr,
        node.local_addr,
        0,
        false,
        "/rbitcoin:test/",
        HandshakePolicy::plain(),
    )
    .await
    .unwrap();
    let inbound = watch.await.unwrap();
    node.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
    let inbound = inbound.expect("loopback client must show as inbound in getpeerinfo");
    assert_eq!(inbound.id, connecting.id);
    assert_eq!(inbound.conntime, T0, "the handshaken row keeps conntime");
    assert_eq!((inbound.last_send, inbound.last_recv), (T0 + 30, T0 + 30));
    assert_eq!(inbound.version, rbitcoin_net::peer::OUR_PROTOCOL_VERSION);
}

#[tokio::test]
async fn onion_bind_admission_skips_group_cooldown() {
    use rbitcoin_net::net_permissions::NetPermTable;
    use rbitcoin_net::peer::{connect_and_handshake_timed, HandshakePolicy};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    let (_dir, query) = rbitcoin_query::testutil::tiny_query_labeled("onion-bind-admission");
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let mut node = P2PNode::start(
        listen,
        query,
        ChainParams::regtest(),
        Milestone::NONE,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let ordinary = node.local_addr;
    let onion = node.add_listen(listen).await.unwrap();
    node.peers.set_net_perms(NetPermTable {
        onion_bind: Some(onion),
        ..Default::default()
    });

    for port in 40000..40030 {
        node.peers.register_connecting(
            SocketAddr::from(([127, 0, 4, 9], port)),
            ordinary,
            true,
            PeerConnType::Inbound,
        );
    }
    assert!(node.peers.try_evict_inbound());

    let mut stream = TcpStream::connect(ordinary).await.unwrap();
    let bytes = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut [0]))
        .await
        .expect("the ordinary listener should refuse the connection before handshake")
        .unwrap();
    assert_eq!(bytes, 0);

    let stream = TcpStream::connect(onion).await.unwrap();
    connect_and_handshake_timed(
        Duration::from_secs(5),
        stream,
        Magic::REGTEST,
        onion,
        onion,
        0,
        false,
        "/rbitcoin:test/",
        HandshakePolicy::plain(),
    )
    .await
    .expect("the marked listener should admit the forwarder during the group cooldown");
    node.shutdown().await;
}

/// The 2 s redial pass must not stack a dial that is still connecting,
/// and must not lose the target to one that never finishes. A SOCKS
/// proxy that accepts and never answers holds the first dial in
/// `connect_net`: later passes skip it until the connect times out, then
/// dial again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redial_skips_a_connect_target_still_dialling() {
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&accepts);
    let wake = Arc::new(tokio::sync::Notify::new());
    let wake_accept = Arc::clone(&wake);
    let stall = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = proxy.accept().await {
            seen.fetch_add(1, Ordering::SeqCst);
            wake_accept.notify_waiters();
            held.push(sock);
        }
    });
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-redial-in-flight-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let connect_bound = Duration::from_millis(500);
    let node = P2PNode::start_outbound_only(
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(redial)/".into(),
        0,
        rbitcoin_net::socks::Dialer::socks(proxy_addr, false).with_connect_floor(connect_bound),
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    node.peers.set_connect_addrs(vec![rbitcoin_net::NetAddr::Ip(
        "127.0.0.1:18444".parse().unwrap(),
    )]);
    node.peers.redial_remembered();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notified = wake.notified();
        if accepts.load(Ordering::SeqCst) > 0 {
            break;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "the first dial never reached the proxy");
        if tokio::time::timeout(left, notified).await.is_err() {
            panic!("the first dial never reached the proxy");
        }
    }
    assert!(
        !node.peers.in_flight_nets().is_empty(),
        "the first dial stays in flight while the proxy holds it"
    );
    for _ in 0..3 {
        node.peers.redial_remembered();
    }
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        1,
        "a dial still connecting must not be dialled again"
    );
    // The floor is the connect timeout. The in-flight flag drops when
    // that task is polled, which is later than 3× the floor under load.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !node.peers.in_flight_nets().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "a connect that never finishes must time out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    node.peers.redial_remembered();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notified = wake.notified();
        if accepts.load(Ordering::SeqCst) >= 2 {
            break;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "a connect that never finishes must time out and be redialled"
        );
        if tokio::time::timeout(left, notified).await.is_err() {
            panic!("a connect that never finishes must time out and be redialled");
        }
    }
    stall.abort();
    drop(node);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `--proxy` name goes through SOCKS like an IP peer, so its connect gets
/// the SOCKS bound, not the 8 s direct one. A proxy that accepts and never
/// answers frees the dial when that bound runs out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxied_name_connect_gets_the_socks_bound() {
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let wake = Arc::new(tokio::sync::Notify::new());
    let wake_accept = Arc::clone(&wake);
    let stall = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = proxy.accept().await {
            held.push(sock);
            wake_accept.notify_one();
        }
    });
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-proxied-name-bound-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let connect_bound = Duration::from_millis(300);
    let dialer =
        rbitcoin_net::socks::Dialer::socks(proxy_addr, false).with_connect_floor(connect_bound);
    let node = P2PNode::start_outbound_only(
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(name-bound)/".into(),
        0,
        dialer.clone(),
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    node.peers.set_proxies(rbitcoin_net::NetProxies {
        dialer,
        i2p_sam: None,
    });
    node.peers
        .addnode_host("peer.invalid", "onetry", 18444)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), wake.notified())
        .await
        .expect("the name dial never reached the proxy");
    let deadline = std::time::Instant::now() + connect_bound * 3;
    while !node.peers.in_flight_nets().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "a stalled SOCKS connect to a name must end at the SOCKS bound"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stall.abort();
    drop(node);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A silent inbound peer, and one that sends a byte every few hundred
/// milliseconds during key exchange, must not hold an inbound slot past one
/// peer-timeout. One address must not fill the inbound table with sockets
/// that have not finished the handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_cap_silent_inbound_releases_the_slot() {
    use tokio::io::AsyncWriteExt;
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-inbound-deadline-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let node = P2PNode::start_with_agent(
        "127.0.0.1:0".parse().unwrap(),
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(deadline)/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    node.peers.set_peer_timeout_secs(1);
    let mut silent = tokio::net::TcpStream::connect(node.local_addr)
        .await
        .unwrap();
    let mut drip = tokio::net::TcpStream::connect(node.local_addr)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let mut saw_row = false;
    let mut cleared = false;
    while started.elapsed() < Duration::from_secs(3) {
        if !node.peers.snapshot().is_empty() {
            saw_row = true;
        }
        let _ = drip.write_all(&[0x11]).await;
        if saw_row && node.peers.snapshot().is_empty() {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(saw_row, "the silent inbound never took a slot");
    assert!(
        cleared && started.elapsed() < Duration::from_secs(2),
        "unfinished handshakes still held slots after {:?}",
        started.elapsed()
    );
    let _ = silent.shutdown().await;
    drop(silent);
    drop(drip);

    node.peers.set_peer_timeout_secs(30);
    let mut parked = Vec::new();
    for _ in 0..24 {
        parked.push(
            tokio::net::TcpStream::connect(node.local_addr)
                .await
                .unwrap(),
        );
    }
    let cap = rbitcoin_net::peers::MAX_INCOMPLETE_INBOUND_PER_ADDR;
    // The listener drops each connect past the cap at accept, so that
    // socket reads EOF. Wait until every one of those has, with the cap's
    // worth of handshakes holding slots.
    let wait = std::time::Instant::now();
    let mut refused = vec![false; parked.len()];
    loop {
        let n = node.peers.snapshot().len();
        assert!(n <= cap, "one address held {n} unfinished handshakes");
        for (s, r) in parked.iter().zip(refused.iter_mut()) {
            let mut b = [0u8; 1];
            *r |= matches!(s.try_read(&mut b), Ok(0));
        }
        let closed = refused.iter().filter(|r| **r).count();
        if n == cap && closed == parked.len() - cap {
            break;
        }
        assert!(
            wait.elapsed() < Duration::from_secs(5),
            "{n} slots held and {closed} of {} over-cap connects closed",
            parked.len() - cap
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(parked);
    node.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A node started with `hold_inbound` binds its listener but accepts
/// nothing until `publish_local_endpoints` installs the permission table and
/// endpoints. Tor or I2P inbound that arrives while start is still wiring
/// forwarding comes from loopback; it must be judged as forwarded, not
/// against the per-source handshake cap (#2099).
#[tokio::test]
async fn held_inbound_waits_for_the_forwarding_facts() {
    use rbitcoin_net::net_permissions::NetPermTable;
    use rbitcoin_net::peers::MAX_INCOMPLETE_INBOUND_PER_ADDR;
    use tokio::net::TcpStream;

    let (_dir, query) = rbitcoin_query::testutil::tiny_query_labeled("held-inbound");
    let mut node = P2PNode::start_inner(
        Some("127.0.0.1:0".parse().unwrap()),
        query,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:test-held-inbound/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
        true,
    )
    .await
    .unwrap();
    // An `add_listen` bind is held too: here the marked onion listener.
    let onion = node
        .add_listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let n = MAX_INCOMPLETE_INBOUND_PER_ADDR + 2;
    let mut early = Vec::new();
    for target in [node.local_addr, onion] {
        for _ in 0..n {
            early.push(TcpStream::connect(target).await.unwrap());
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        node.peers.snapshot().iter().all(|p| !p.inbound),
        "nothing is accepted before the endpoints are published"
    );

    node.peers.set_overlay_loopback_forward(true);
    node.peers.publish_local_endpoints(
        node.local_addr.port(),
        Vec::new(),
        NetPermTable {
            onion_bind: Some(onion),
            ..Default::default()
        },
    );
    let mut inbound = 0;
    for _ in 0..200 {
        inbound = node.peers.snapshot().iter().filter(|p| p.inbound).count();
        if inbound == 2 * n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    node.shutdown().await;
    assert_eq!(
        inbound,
        2 * n,
        "every early loopback inbound on either listener is admitted as forwarded"
    );
}

#[tokio::test]
async fn marked_onion_listener_ignores_the_ordinary_source_cap() {
    use rbitcoin_net::net_permissions::NetPermTable;
    use rbitcoin_net::peer::V2PlainSession;
    use rbitcoin_net::peers::MAX_INCOMPLETE_INBOUND_PER_ADDR;
    use tokio::net::TcpStream;

    let (_dir, query) = rbitcoin_query::testutil::tiny_query_labeled("onion-inbound-limit");
    let loopback = "127.0.0.1:0".parse().unwrap();
    let mut node = P2PNode::start_with_agent(
        loopback,
        query,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:test-onion-listener/".into(),
        2,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let ordinary = node.local_addr;
    let onion = node.add_listen(loopback).await.unwrap();
    node.peers.set_net_perms(NetPermTable {
        onion_bind: Some(onion),
        ..Default::default()
    });

    let _ordinary_reservations: Vec<_> = (0..MAX_INCOMPLETE_INBOUND_PER_ADDR)
        .map(|_| {
            node.peers
                .try_reserve_incomplete(loopback.ip(), ordinary)
                .unwrap()
        })
        .collect();
    let handshake = |target| async move {
        let stream = TcpStream::connect(target).await.unwrap();
        V2PlainSession::outbound_regtest(stream, "/rbitcoin:test-client/", Duration::from_secs(3))
            .await
    };
    let source_capped = handshake(ordinary).await;
    let marked = handshake(onion).await;
    node.shutdown().await;
    assert!(source_capped.is_err(), "ordinary source cap");
    marked.expect("marked onion listener has room under the shared inbound limit");
}

#[test]
fn marked_onion_reservations_leave_the_forwarder_ip_budget_to_ordinary_tcp() {
    use rbitcoin_net::net_permissions::NetPermTable;
    use rbitcoin_net::peers::{PeerHub, MAX_INCOMPLETE_INBOUND_PER_ADDR};

    let peers = PeerHub::new();
    let source = "10.0.3.1".parse().unwrap();
    let ordinary = "10.0.3.2:8333".parse().unwrap();
    let onion = "10.0.3.2:8334".parse().unwrap();
    peers.set_net_perms(NetPermTable {
        onion_bind: Some(onion),
        ..Default::default()
    });
    let marked = peers.try_reserve_incomplete(source, onion).unwrap();
    let mut ordinary_reservations: Vec<_> = (0..MAX_INCOMPLETE_INBOUND_PER_ADDR)
        .map(|_| peers.try_reserve_incomplete(source, ordinary).unwrap())
        .collect();
    assert!(peers.try_reserve_incomplete(source, ordinary).is_none());
    assert!(peers.try_reserve_incomplete(source, onion).is_some());
    drop(marked);
    assert!(peers.try_reserve_incomplete(source, ordinary).is_none());
    drop(ordinary_reservations.pop());
    assert!(peers.try_reserve_incomplete(source, ordinary).is_some());
}

#[tokio::test]
async fn marked_onion_handshake_uses_and_releases_a_shared_inbound_slot() {
    use rbitcoin_net::chain::{tiny_regtest_hub_labeled, ChainHub};
    use rbitcoin_net::net_permissions::NetPermTable;
    use rbitcoin_net::peer::V2PlainSession;
    use rbitcoin_net::peers::PeerHub;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use tokio::net::TcpStream;
    use tokio::sync::{watch, Semaphore};
    use tokio::time::timeout;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let onion = listener.local_addr().unwrap();
    let (dir, hub) = tiny_regtest_hub_labeled("onion-shared-inbound-slot");
    let peers = PeerHub::new();
    peers.set_net_perms(NetPermTable {
        onion_bind: Some(onion),
        ..Default::default()
    });
    peers.set_peer_timeout_secs(10);
    let inbound_slots = Arc::new(Semaphore::new(1));
    let occupied = inbound_slots.try_acquire().unwrap();
    let (stop, stop_rx) = watch::channel(false);
    let accept = spawn_inbound_accept(
        listener,
        onion,
        ChainHub::into_arc(hub),
        Arc::clone(&peers),
        "/rbitcoin:test-onion-slot/".into(),
        Magic::REGTEST,
        1,
        Arc::clone(&inbound_slots),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(Vec::new())),
        stop_rx,
    );

    let open_transport = || async {
        let stream = TcpStream::connect(onion).await.unwrap();
        timeout(
            Duration::from_secs(3),
            V2PlainSession::outbound_bip324(stream),
        )
        .await
        .expect("marked listener answers within the test deadline")
    };
    assert!(
        open_transport().await.is_err(),
        "the shared slot is occupied"
    );
    drop(occupied);
    let _pending = open_transport().await.expect("the shared slot is free");
    tokio::time::pause();
    assert_eq!(inbound_slots.available_permits(), 0, "VERSION is pending");
    let _released = timeout(Duration::from_secs(11), inbound_slots.acquire())
        .await
        .expect("the handshake deadline releases the shared slot")
        .unwrap();
    assert!(peers.snapshot().is_empty(), "the timed-out peer is removed");

    stop.send(true).unwrap();
    accept.await.unwrap();
    drop(dir);
}

#[test]
fn outbound_connect_timeout_is_eight_seconds() {
    assert_eq!(OUTBOUND_CONNECT_TIMEOUT, Duration::from_secs(8));
}

#[test]
fn accept_retries_a_full_fd_table_and_stops_on_a_dead_listener() {
    let aborted = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "reset");
    let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "signal");
    let emfile = std::io::Error::from_raw_os_error(24);
    let enfile = std::io::Error::from_raw_os_error(23);
    let dead = std::io::Error::new(std::io::ErrorKind::InvalidInput, "closed");
    assert!(!accept_error_ends_listen(&aborted));
    assert!(!accept_error_ends_listen(&interrupted));
    assert!(!accept_error_ends_listen(&emfile));
    assert!(!accept_error_ends_listen(&enfile));
    assert!(accept_error_ends_listen(&dead));
    let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "rst");
    let wsaemfile = std::io::Error::from_raw_os_error(10024);
    let wsaenobufs = std::io::Error::from_raw_os_error(10055);
    let wsaenetdown = std::io::Error::from_raw_os_error(10050);
    let eperm = std::io::Error::from_raw_os_error(1);
    let enobufs = std::io::Error::from_raw_os_error(105);
    let enomem = std::io::Error::from_raw_os_error(12);
    let ebadf = std::io::Error::from_raw_os_error(9);
    assert!(!accept_error_ends_listen(&reset));
    assert!(!accept_error_ends_listen(&wsaemfile));
    assert!(!accept_error_ends_listen(&wsaenobufs));
    assert!(!accept_error_ends_listen(&wsaenetdown));
    assert!(!accept_error_ends_listen(&eperm));
    assert!(!accept_error_ends_listen(&enobufs));
    assert!(!accept_error_ends_listen(&enomem));
    assert!(accept_error_ends_listen(&ebadf));
}

#[test]
fn inbound_handshake_limit_cannot_exceed_sixty_seconds() {
    use rbitcoin_net::peer::{inbound_handshake_limit, HANDSHAKE_TIMEOUT};
    assert_eq!(inbound_handshake_limit(1), Duration::from_secs(1));
    assert_eq!(inbound_handshake_limit(60), HANDSHAKE_TIMEOUT);
    assert_eq!(inbound_handshake_limit(3600), HANDSHAKE_TIMEOUT);
}

#[tokio::test]
async fn shutdown_finishes_without_the_dial_grace() {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-shutdown-grace-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let node = P2PNode::start_outbound_only(
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:test/".into(),
        0,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let t0 = tokio::time::Instant::now();
    node.shutdown().await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "dial and retry must observe shutdown, took {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// No IPv6 socket on this host: `socket(AF_INET6)` fails, or `[::]` is not
/// an address the kernel will bind.
fn ipv6_unavailable(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::EAFNOSUPPORT) {
        return true;
    }
    #[cfg(windows)]
    if e.raw_os_error() == Some(10047) {
        // WSAEAFNOSUPPORT: no IPv6 stack. Rust reports it as Uncategorized.
        return true;
    }
    e.kind() == std::io::ErrorKind::AddrNotAvailable
}

#[cfg(windows)]
#[test]
fn ipv6_unavailable_reads_wsa_eafnosupport() {
    let e = std::io::Error::from_raw_os_error(10047);
    assert!(ipv6_unavailable(&e));
}

/// A dual-stack inbound session: the node on `[::]:0`, the client's local
/// address and its open half of the session, and the node's handshaken row.
struct DualStack {
    _dir: rbitcoin_query::testutil::TempDir,
    node: P2PNode,
    client: std::net::SocketAddr,
    _session: Box<dyn std::any::Any + Send>,
    row: rbitcoin_net::peers::PeerInfo,
}

/// Start a node on `[::]:0` under `perms` and handshake with it from
/// `127.0.0.1`. `None` (with a SKIP line) only when the host has no IPv6 or
/// its `[::]` listener is IPv6-only; any other failure panics.
async fn dual_stack_session(
    label: &str,
    perms: rbitcoin_net::net_permissions::NetPermTable,
) -> Option<DualStack> {
    use rbitcoin_net::peer::{connect_and_handshake_timed, HandshakePolicy};
    let ua = format!("/rbitcoin:0.1.0({label})/");
    let (dir, query) = rbitcoin_query::testutil::tiny_query_labeled(label);
    let node = match P2PNode::start_with_dialer(
        "[::]:0".parse().unwrap(),
        query,
        ChainParams::regtest(),
        Milestone::NONE,
        ua.clone(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        rbitcoin_net::socks::Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    {
        Ok(node) => node,
        Err(NetError::Io(e) | NetError::Bind { source: e, .. }) if ipv6_unavailable(&e) => {
            eprintln!("SKIP {label}: this host cannot listen on [::]: {e}");
            return None;
        }
        Err(e) => panic!("start a node on [::]:0: {e}"),
    };
    node.peers.set_net_perms(perms);
    let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, node.local_addr.port()));
    let stream = match tokio::net::TcpStream::connect(v4).await {
        Ok(stream) => stream,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            eprintln!(
                "SKIP {label}: the [::] listener is not dual-stack \
                 (net.ipv6.bindv6only=1?): {e}"
            );
            node.shutdown().await;
            return None;
        }
        Err(e) => panic!("connect to the [::] listener on {v4}: {e}"),
    };
    let client = stream.local_addr().unwrap();
    let watch = first_handshaken(node.peers.clone(), true);
    let session = connect_and_handshake_timed(
        Duration::from_secs(5),
        stream,
        Magic::REGTEST,
        v4,
        v4,
        0,
        false,
        "/rbitcoin:test/",
        HandshakePolicy::plain(),
    )
    .await
    .unwrap();
    assert_eq!(
        session.0.user_agent, ua,
        "{label}: {v4} was answered by another node"
    );
    let row = watch.await.unwrap().expect("handshaken inbound row");
    Some(DualStack {
        _dir: dir,
        node,
        client,
        _session: Box::new(session),
        row,
    })
}

/// A `[::]` listener is dual-stack on Linux, so an IPv4 client is accepted
/// as `::ffff:a.b.c.d`. Its row carries the IPv4 address (Core
/// `CNetAddr` maps v4-in-v6 to IPv4), and a local `127.0.0.1` client that
/// misbehaves is disconnected but not discouraged: it can connect again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_stack_inbound_ipv4_is_seen_as_ipv4() {
    use rbitcoin_net::peer::punish_disconnect;
    use tokio::net::TcpStream;

    let Some(ds) = dual_stack_session("dual-stack-inbound", Default::default()).await else {
        return;
    };
    let (node, row, client) = (&ds.node, &ds.row, ds.client);
    let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, node.local_addr.port()));
    let shown = (row.addr, row.net.to_string());
    let live = node.peers.live.read().unwrap().get(&row.id).cloned();
    let mut score = 0u32;
    punish_disconnect(&mut score, live.as_deref());
    let mut reconnected = false;
    let end = std::time::Instant::now() + Duration::from_secs(5);
    let mut again = Vec::new();
    while !reconnected && std::time::Instant::now() < end {
        if let Ok(s) = TcpStream::connect(v4).await {
            again.push(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        reconnected = node
            .peers
            .snapshot()
            .iter()
            .any(|p| p.inbound && p.id != row.id);
    }
    drop(again);
    ds.node.shutdown().await;
    assert_eq!(
        (shown, reconnected),
        ((client, client.to_string()), true),
        "(getpeerinfo addr, reconnect after a loopback misbehavior) of a dual-stack IPv4 inbound"
    );
}

/// `getpeerinfo` showed a dual-stack IPv4 inbound as `::ffff:127.0.0.1`, so
/// an operator may have whitelisted that form. Core maps it to `127.0.0.1`
/// when it parses the grant, so the inbound keeps noban.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_stack_inbound_ipv4_keeps_a_mapped_whitelist_grant() {
    use rbitcoin_net::net_permissions::{parse_whitelist, NetPermTable};

    let mut perms = NetPermTable::default();
    perms
        .whitelist
        .push(parse_whitelist("noban@::ffff:127.0.0.1").unwrap());
    let Some(ds) = dual_stack_session("dual-stack-mapped-grant", perms).await else {
        return;
    };
    let live = ds.node.peers.live.read().unwrap().get(&ds.row.id).cloned();
    let noban = live.map(|l| l.session_noban());
    ds.node.shutdown().await;
    assert_eq!(
        noban,
        Some(true),
        "noban@::ffff:127.0.0.1 on a dual-stack 127.0.0.1 inbound"
    );
}

/// A full outbound handshake refreshes the row we dialed; an inbound
/// session leaves its row alone. Both run through a node's real connection
/// paths, not the registration helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshakes_refresh_only_outbound_rows() {
    use rbitcoin_net::peers::DialTarget;
    use rbitcoin_net::seeds::AddrMan;
    use rbitcoin_net::socks::Dialer;
    use std::sync::Mutex;

    let (_srv_dir, srv_query) =
        rbitcoin_query::testutil::tiny_query_labeled("handshake-refresh-srv");
    let srv = P2PNode::start_with_dialer(
        "127.0.0.1:0".parse().unwrap(),
        srv_query,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:srv/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .expect("server node");
    let (_cli_dir, cli_query) =
        rbitcoin_query::testutil::tiny_query_labeled("handshake-refresh-cli");
    let cli = P2PNode::start_with_dialer(
        "127.0.0.1:0".parse().unwrap(),
        cli_query,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:cli/".into(),
        rbitcoin_net::DEFAULT_MAX_INBOUND,
        Dialer::Direct,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .expect("client node");

    let now = 1_700_000_000u64;
    cli.peers.set_mock_now(now);
    let target = rbitcoin_net::NetAddr::from_socket(srv.local_addr);
    let mut book = AddrMan::new();
    assert!(book.add_learned_addr_at(target, target, 1_690_000_000, 0, now, 8));
    let book = Arc::new(Mutex::new(book));
    cli.peers.set_addrman(Arc::clone(&book));

    let prepared = prepare_outbound_session(
        DialTarget::from_net(target),
        None,
        Magic::REGTEST,
        cli.local_addr,
        Arc::clone(&cli.hub),
        Arc::clone(&cli.peers),
        cli.user_agent.clone(),
        Arc::clone(&cli.follow_live),
        PeerConnType::OutboundFullRelay,
        Dialer::Direct,
    )
    .await
    .expect("outbound handshake");
    let live = first_handshaken(cli.peers.clone(), false);
    let task = tokio::spawn(run_prepared_outbound(prepared));
    live.await.unwrap().expect("live outbound row");
    assert_eq!(
        book.lock().unwrap().last_seen_of(&target),
        1_690_000_000,
        "a live outbound is not advertised as just seen"
    );

    // Inbound: bind a fixed source, put that row in the book, handshake in.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let src = sock.local_addr().unwrap();
    let src_net = rbitcoin_net::NetAddr::from_socket(src);
    assert!(book
        .lock()
        .unwrap()
        .add_learned_addr_at(src_net, src_net, 1_690_000_000, 0, now, 8));
    let watch = first_handshaken(cli.peers.clone(), true);
    let stream = sock.connect(cli.local_addr).await.unwrap();
    let hs = rbitcoin_net::peer::connect_and_handshake_timed(
        Duration::from_secs(5),
        stream,
        Magic::REGTEST,
        src,
        cli.local_addr,
        0,
        false,
        "/rbitcoin:inbound/",
        rbitcoin_net::peer::HandshakePolicy::plain(),
    )
    .await
    .expect("inbound handshake");
    let row = watch.await.unwrap().expect("handshaken inbound row");
    assert!(row.inbound);
    assert_eq!(
        book.lock().unwrap().last_seen_of(&src_net),
        1_690_000_000,
        "a live inbound does not refresh its row"
    );

    // Close the outbound peer: Core `FinalizeNode` refreshes at disconnect.
    srv.shutdown().await;
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("outbound session ends")
        .expect("outbound task")
        .expect("outbound session result");
    assert_eq!(
        book.lock().unwrap().last_seen_of(&target),
        now as u32,
        "the outbound row refreshes when the session ends"
    );

    // Close the inbound peer: an inbound session never refreshes a row.
    drop(hs);
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while cli.peers.snapshot().iter().any(|p| p.inbound) && std::time::Instant::now() < end {
        tokio::task::yield_now().await;
    }
    assert!(
        !cli.peers.snapshot().iter().any(|p| p.inbound),
        "the inbound session closed before the row-time check"
    );
    assert_eq!(
        book.lock().unwrap().last_seen_of(&src_net),
        1_690_000_000,
        "an inbound session never refreshes its row"
    );

    cli.shutdown().await;
}

/// A ban that lands while an outbound dial is still connecting must
/// not leave a live session: publication re-checks the ban list after
/// the handshake and refuses the session. A gated SOCKS proxy holds the
/// dial inside `connect_net`, before any session row exists, so nothing
/// but the re-check can stop this session: `ban_add` has no live or
/// provisional row to disconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ban_added_during_dial_blocks_session_publication() {
    use rbitcoin_net::peer::{connect_and_handshake_timed, HandshakePolicy};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let far = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let far_addr = far.local_addr().unwrap();

    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let at_gate = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (gate_seen, gate_release) = (Arc::clone(&at_gate), Arc::clone(&release));
    let gate = tokio::spawn(async move {
        let (mut c, _) = proxy.accept().await.unwrap();
        let mut ver_n = [0u8; 2];
        c.read_exact(&mut ver_n).await.unwrap();
        let mut methods = vec![0u8; ver_n[1] as usize];
        c.read_exact(&mut methods).await.unwrap();
        if methods.contains(&0x02) {
            c.write_all(&[5, 0x02]).await.unwrap();
            let mut ver = [0u8; 1];
            c.read_exact(&mut ver).await.unwrap();
            let mut ulen = [0u8; 1];
            c.read_exact(&mut ulen).await.unwrap();
            let mut user = vec![0u8; ulen[0] as usize];
            c.read_exact(&mut user).await.unwrap();
            let mut plen = [0u8; 1];
            c.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            c.read_exact(&mut pass).await.unwrap();
            c.write_all(&[1, 0]).await.unwrap();
        } else {
            c.write_all(&[5, 0x00]).await.unwrap();
        }
        let mut hdr = [0u8; 4];
        c.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[1], 1, "test gate expects an IPv4 CONNECT");
        let mut a = [0u8; 4];
        c.read_exact(&mut a).await.unwrap();
        let mut p = [0u8; 2];
        c.read_exact(&mut p).await.unwrap();
        let dest = SocketAddr::from((Ipv4Addr::new(a[0], a[1], a[2], a[3]), u16::from_be_bytes(p)));
        gate_seen.notify_one();
        gate_release.notified().await;
        let mut peer = tokio::net::TcpStream::connect(dest).await.unwrap();
        c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut c, &mut peer).await;
    });

    let (_dir, q) = rbitcoin_query::testutil::tiny_query_labeled("ban-race");
    let mut node = P2PNode::start_outbound_only(
        q,
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(ban-race)/".into(),
        0,
        rbitcoin_net::socks::Dialer::socks(proxy_addr, false),
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    let peers = Arc::clone(&node.peers);
    let dial = tokio::spawn(async move { node.follow_from(far_addr).await });
    tokio::time::timeout(Duration::from_secs(5), at_gate.notified())
        .await
        .expect("the dial never reached the proxy gate");
    assert!(
        peers.snapshot().is_empty(),
        "no session row exists while the dial is held"
    );
    let target = rbitcoin_net::parse_ban_target("127.0.0.1").unwrap();
    peers.ban_add(target, peers.now_secs() + 3600).unwrap();
    release.notify_one();
    // The far end answers the handshake once the splice is up; its own
    // result does not matter, the dial side may close on refusal.
    let far_side = tokio::spawn(async move {
        let (stream, _) = far.accept().await.unwrap();
        let _ = connect_and_handshake_timed(
            Duration::from_secs(5),
            stream,
            Magic::REGTEST,
            far_addr,
            far_addr,
            0,
            true,
            "/rbitcoin:test/",
            HandshakePolicy::plain(),
        )
        .await;
    });
    let err = dial
        .await
        .unwrap()
        .expect_err("a session banned mid-dial is refused");
    assert!(err.to_string().contains("banned"), "dial error: {err}");
    assert!(
        peers.snapshot().is_empty(),
        "no session is published: {:?}",
        peers.snapshot()
    );
    far_side.abort();
    gate.abort();
}
