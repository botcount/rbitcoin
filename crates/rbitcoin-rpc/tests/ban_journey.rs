// Two regtest nodes: a manual ban drops the peer from `getpeerinfo`,
// refuses its reconnects in both directions, shows in `listbanned`, and
// `clearbanned` lets it connect again.

async fn start_ban_node(dir: &TempDir, label: &str) -> rbitcoin_net::P2PNode {
    let q = Query::open_or_create_tiny(dir.join(label)).unwrap();
    rbitcoin_net::P2PNode::start(
        "127.0.0.1:0".parse().unwrap(),
        q,
        rbitcoin_consensus::ChainParams::regtest(),
        rbitcoin_consensus::Milestone::NONE,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .expect("listen")
}

async fn wait_peer_count(ctx: &RpcContext, want: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let rows = dispatch(ctx, "getpeerinfo", vec![]).unwrap();
        if rows.as_array().map(Vec::len) == Some(want) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: getpeerinfo never reached {want} rows: {rows}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ban_journey_disconnect_refuse_list_clear_reconnect() {
    let fut = async {
        let (mut ctx, dir) = ctx_empty();
        let peer_dir = TempDir::labeled("ban-journey-peer").expect("peer dir");
        let mut banner = start_ban_node(&dir, "banner-store").await;
        let mut peer = start_ban_node(&peer_dir, "peer-store").await;
        ctx.peers = Some(std::sync::Arc::clone(&banner.peers));

        banner
            .peers
            .addconnection(peer.local_addr, rbitcoin_net::PeerConnType::OutboundFullRelay)
            .unwrap();
        wait_peer_count(&ctx, 1, "initial connect").await;

        dispatch(&ctx, "setban", vec![json!("127.0.0.1"), json!("add")]).unwrap();
        wait_peer_count(&ctx, 0, "ban disconnects the peer").await;

        let banned = dispatch(&ctx, "listbanned", vec![]).unwrap();
        assert_eq!(banned.as_array().map(Vec::len), Some(1), "{banned}");
        assert_eq!(banned[0]["address"], json!("127.0.0.1/32"));

        // Both directions are refused while the ban stands, and each
        // refusal is the dial call itself returning, not a wait: the
        // banner's own dial is rejected as banned before it connects, and
        // the peer's dial is dropped by the banner at accept, so its
        // handshake fails.
        let err = banner.follow_from(peer.local_addr).await.unwrap_err();
        assert!(
            err.to_string().contains("banned"),
            "outbound dial refused as banned: {err}"
        );
        let err = peer.follow_from(banner.local_addr).await.unwrap_err();
        assert!(
            !err.to_string().contains("banned"),
            "the peer cannot see why it was refused: {err}"
        );
        let rows = dispatch(&ctx, "getpeerinfo", vec![]).unwrap();
        assert_eq!(rows, json!([]), "no session survives a refused dial: {rows}");

        dispatch(&ctx, "clearbanned", vec![]).unwrap();
        assert_eq!(dispatch(&ctx, "listbanned", vec![]).unwrap(), json!([]));
        banner
            .peers
            .addconnection(peer.local_addr, rbitcoin_net::PeerConnType::OutboundFullRelay)
            .unwrap();
        wait_peer_count(&ctx, 1, "clearbanned lets the peer reconnect").await;

        banner.shutdown().await;
        peer.shutdown().await;
        let _ = std::fs::remove_dir_all(peer_dir.path());
    };
    tokio::time::timeout(Duration::from_secs(120), fut)
        .await
        .expect("ban journey wall timeout");
}
