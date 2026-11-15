fn hostile_ver(addr: SocketAddr) -> VersionMessage {
    VersionMessage {
        version: 70016,
        services: ServiceFlags::NETWORK | ServiceFlags::WITNESS,
        timestamp: 0,
        receiver: Address::new(&addr, ServiceFlags::NONE),
        sender: Address::new(&addr, ServiceFlags::NONE),
        nonce: 7,
        user_agent: "/rbitcoin:test/".into(),
        start_height: 0,
        relay: true,
    }
}

#[tokio::test]
async fn addrv2_gossip_drops_unroutable_clearnet() {
    use bitcoin::p2p::message::NetworkMessage;
    use std::sync::{Arc, Mutex};

    let peers = rbitcoin_net::peers::PeerHub::new();
    let book = Arc::new(Mutex::new(rbitcoin_net::seeds::AddrMan::new()));
    peers.set_addrman(Arc::clone(&book));
    let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let ver = hostile_ver(src_addr);
    let peer = peers.register(
        src_addr,
        src_addr,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    peer.set_wants_addrv2();
    let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 8333);
    let dst = peers.register(
        dst_addr,
        dst_addr,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    dst.set_wants_addrv2();
    let (_dst_tx, mut dst_rx) = mpsc::unbounded_channel();
    dst.attach_out(_dst_tx);
    peers.set_mock_now(peer.connected_at_secs() + 10_000);

    let public = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 9)),
        port: 8333,
    };
    let lan = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(10, 0, 0, 1)),
        port: 8333,
    };
    let cgnat = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(100, 64, 0, 1)),
        port: 8333,
    };
    let zero_port = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 8)),
        port: 0,
    };
    let i2p = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::I2p([0x11; 32]),
        port: 0,
    };
    let tor_v2 = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::TorV2([0xab; 10]),
        port: 8333,
    };
    let cjdns_bad = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Cjdns(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        port: 8333,
    };
    let cjdns = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Cjdns(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
        port: 8333,
    };
    let unknown = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Unknown(0x99, vec![1, 2, 3, 4]),
        port: 8333,
    };
    let mut follow = PeerFollowState::new();
    on_addrv2(
        &mut follow,
        Some(peer.as_ref()),
        &[
            lan,
            public.clone(),
            cgnat,
            zero_port,
            i2p.clone(),
            tor_v2,
            cjdns_bad,
            cjdns.clone(),
            unknown.clone(),
        ],
    )
    .unwrap();

    let got = dst_rx
        .try_recv()
        .expect("the public address is relayed")
        .expect_msg();
    let NetworkMessage::AddrV2(sent) = got else {
        panic!("expected addrv2, got {got:?}");
    };
    assert_eq!(sent, vec![public, i2p, cjdns, unknown]);
    assert!(
        dst_rx.try_recv().is_err(),
        "unroutable rows, port-0 clearnet, Tor v2, and non-CJDNS are not relayed"
    );
    let book = book.lock().unwrap_or_else(|e| e.into_inner());
    let public_sock = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)), 8333);
    let lan_sock = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 8333);
    assert!(book.entry_of(&rbitcoin_net::NetAddr::from_socket(public_sock)).is_some());
    assert!(
        book.entry_of(&rbitcoin_net::NetAddr::from_socket(lan_sock)).is_none(),
        "learn_addrv2 does not store RFC1918"
    );
}

/// A peer gossips this node's own onion, I2P, CJDNS and external addresses
/// back on a clearnet session. The relay fan-out passes none of them on, to
/// a clearnet neighbor or to an onion one, and still relays a stranger's.
#[tokio::test]
async fn addrv2_relay_drops_own_addresses() {
    use bitcoin::p2p::message::NetworkMessage;

    let onion: rbitcoin_net::NetAddr =
        "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:8333"
            .parse()
            .unwrap();
    let rbitcoin_net::NetAddr::Onion { pk, .. } = onion else {
        unreachable!()
    };
    let cjdns_ip = Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 7);
    let peers = rbitcoin_net::peers::PeerHub::new();
    peers.set_p2p_onion(onion.host_str(), onion.port());
    peers.set_p2p_i2p(rbitcoin_net::NetAddr::I2p {
        dest: [0x11; 32],
        port: 0,
    });
    peers.set_external_ips(vec![
        IpAddr::V4(Ipv4Addr::new(42, 42, 42, 42)),
        IpAddr::V6(cjdns_ip),
    ]);
    let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let ver = hostile_ver(src_addr);
    let src = peers.register(
        src_addr,
        src_addr,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    src.set_wants_addrv2();
    let clear_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 8333);
    let clear = peers.register(
        clear_addr,
        clear_addr,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    clear.set_wants_addrv2();
    let (clear_tx, mut clear_rx) = mpsc::unbounded_channel();
    clear.attach_out(clear_tx);
    let onion_net: rbitcoin_net::NetAddr =
        "vww6ybal4bd7szmgncyruucpgfkqahzddi37ktceo3ah7ngmcopnpyyd.onion:8333"
            .parse()
            .unwrap();
    let onion_sock = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8333);
    let onion_peer = peers.register_net(
        onion_sock,
        onion_net,
        onion_sock,
        &ver,
        false,
        rbitcoin_net::peers::PeerConnType::OutboundFullRelay,
    );
    onion_peer.set_wants_addrv2();
    let (onion_tx, mut onion_rx) = mpsc::unbounded_channel();
    onion_peer.attach_out(onion_tx);
    peers.set_mock_now(src.connected_at_secs() + 10_000);

    let row = |addr: AddrV2, port: u16| AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr,
        port,
    };
    let stranger = row(AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 9)), 8333);
    // The onion neighbor's own self-announce (our onion, on an onion link)
    // is not relay; drop it so only the fan-out is counted.
    while clear_rx.try_recv().is_ok() {}
    while onion_rx.try_recv().is_ok() {}
    let mut follow = PeerFollowState::new();
    on_addrv2(
        &mut follow,
        Some(src.as_ref()),
        &[
            row(AddrV2::TorV3(pk), 8333),
            row(AddrV2::I2p([0x11; 32]), 0),
            row(AddrV2::Ipv4(Ipv4Addr::new(42, 42, 42, 42)), 8333),
            row(AddrV2::Cjdns(cjdns_ip), 8333),
            stranger.clone(),
        ],
    )
    .unwrap();

    let mut relayed = Vec::new();
    for rx in [&mut clear_rx, &mut onion_rx] {
        while let Ok(out) = rx.try_recv() {
            let NetworkMessage::AddrV2(rows) = out.expect_msg() else {
                panic!("expected addrv2");
            };
            relayed.extend(rows);
        }
    }
    assert!(!relayed.is_empty(), "the stranger's address is relayed");
    assert!(
        relayed.iter().all(|r| r == &stranger),
        "no neighbor is sent this node's own address: {relayed:?}"
    );
}

fn hostile_addr(port: u16) -> AddrV2Message {
    AddrV2Message {
        time: 1_700_000_000,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 9)),
        port,
    }
}

#[tokio::test]
async fn hostile_peer_session() {
    use bitcoin::bip152::HeaderAndShortIds;
    use bitcoin::block::Header;
    use bitcoin::p2p::message_compact_blocks::CmpctBlock;
    use rbitcoin_mempool::AcceptError;

    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hostile-session");
    hub.ensure_genesis().unwrap();
    hub.confirm_empty_regtest(200)
        .unwrap();
    let genesis = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(0))
        .unwrap()
        .block_hash();

    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let mut follow = PeerFollowState::new();

    let hdr = Header {
        version: bitcoin::block::Version::TWO,
        prev_blockhash: BlockHash::from_byte_array([0x99; 32]),
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
        nonce: 0,
    };
    for batch in 0..4u32 {
        let headers: Vec<Header> = (0..2_000u32)
            .map(|i| {
                let mut h = hdr;
                h.nonce = batch * 2_000 + i;
                h
            })
            .collect();
        on_headers(&hub, &out_tx, &mut follow, Some(&peer), &headers)
            .await
            .unwrap();
    }
    assert_eq!(follow.pending_headers.len(), rbitcoin_net::peer::MAX_PENDING_HEADERS);
    let kept = *follow.pending_headers.keys().next().unwrap();
    let kept_hdr = follow.pending_headers[&kept];
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[kept_hdr],
    )
    .await
    .unwrap();
    assert_eq!(follow.pending_headers.len(), rbitcoin_net::peer::MAX_PENDING_HEADERS);
    assert!(follow.pending_headers.contains_key(&kept));
    let mut extra = hdr;
    extra.nonce = 9_000_000;
    let extra_hash = extra.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[extra])
        .await
        .unwrap();
    assert_eq!(follow.pending_headers.len(), rbitcoin_net::peer::MAX_PENDING_HEADERS);
    assert!(!follow.pending_headers.contains_key(&extra_hash));

    // One continuation needs one slot. The other orphans stay, so a later
    // parent can still join them.
    let mut continuation = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(200))
        .unwrap();
    continuation.prev_blockhash = hub.tip_hash().unwrap();
    continuation.merkle_root = bitcoin::TxMerkleNode::from_byte_array([0x55; 32]);
    continuation.time = continuation.time.saturating_add(600);
    continuation.nonce = 0;
    rbitcoin_consensus::grind_regtest_pow(&mut continuation);
    let continuation_hash = continuation.block_hash();
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[continuation],
    )
    .await
    .unwrap();
    assert_eq!(
        follow.pending_headers.len(),
        rbitcoin_net::peer::MAX_PENDING_HEADERS
    );
    assert!(follow.pending_headers.contains_key(&continuation_hash));

    while out_rx.try_recv().is_ok() {}

    let gh = GetHeadersMessage::new(vec![genesis], BlockHash::from_byte_array([0u8; 32]));
    on_getheaders(&hub, &out_tx, &mut follow, Some(&peer), &gh)
        .await
        .unwrap();
    let NetworkMessage::Headers(first) = out_rx.try_recv().unwrap().expect_msg() else {
        panic!("one getheaders under the budget must be answered");
    };
    assert_eq!(first.len(), 200, "locator after genesis walks the tip");
    let mut queued = 1usize;
    for _ in 0..400 {
        handle_decoded_peer_msg(
            RawNetworkMessage::new(
                Magic::from(Network::Regtest),
                NetworkMessage::GetHeaders(GetHeadersMessage::new(
                    vec![genesis],
                    BlockHash::from_byte_array([0u8; 32]),
                )),
            ),
            &hub,
            &out_tx,
            &mut follow,
            Some(&peer),
        )
        .await
        .unwrap();
        match out_rx.try_recv() {
            Ok(_) => queued += 1,
            Err(_) => break,
        }
    }
    let batch = first.len().saturating_mul(81);
    let room = rbitcoin_net::peers::PEER_SEND_BUDGET / batch;
    assert!(
        queued <= room + 2,
        "queued {queued} header batches while the peer read nothing (room {room})"
    );
    assert!(queued > 1, "a getheaders under the budget produced no further reply");

    peer.note_send_written(peer.send_queued());
    assert!(!peer.send_over_budget());
    let before = peer.send_queued();
    queue_accounted(
        Some(&peer),
        &out_tx,
        NetworkMessage::Inv(vec![Inventory::Block(genesis)]),
    )
    .unwrap();
    queue_accounted(
        Some(&peer),
        &out_tx,
        NetworkMessage::NotFound(vec![Inventory::Block(genesis)]),
    )
    .unwrap();
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![],
        output: vec![],
    };
    let tx_n = tx.total_size();
    queue_accounted(Some(&peer), &out_tx, NetworkMessage::Tx(tx)).unwrap();
    queue_accounted(
        Some(&peer),
        &out_tx,
        NetworkMessage::Addr(vec![(0, Address::new(&bind, ServiceFlags::NONE))]),
    )
    .unwrap();
    assert_eq!(
        peer.send_queued() - before,
        36 + 36 + tx_n + 30,
        "inv, notfound, tx, and addr share the send budget"
    );

    let block = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
    let block_n = block.total_size();
    assert!(block_n > 64, "a block is not the fallback size");
    assert_eq!(
        rbitcoin_net::peers::outbound_msg_bytes(&NetworkMessage::Block(block.clone())),
        block_n
    );
    let v2 = NetworkMessage::AddrV2(vec![AddrV2Message {
        time: 1,
        services: ServiceFlags::NONE,
        addr: AddrV2::Ipv4(Ipv4Addr::LOCALHOST),
        port: 1,
    }]);
    assert_eq!(rbitcoin_net::peers::outbound_msg_bytes(&v2), 61);
    let hsi = HeaderAndShortIds::from_block(&block, 1, 1, &[0]).unwrap();
    let compact_n = bitcoin::consensus::encode::serialize(&hsi).len();
    assert_eq!(
        rbitcoin_net::peers::outbound_msg_bytes(&NetworkMessage::CmpctBlock(CmpctBlock {
            compact_block: hsi
        })),
        compact_n
    );
    let txns = bitcoin::bip152::BlockTransactions {
        block_hash: block.block_hash(),
        transactions: vec![block.txdata[0].clone()],
    };
    let blocktxn_n = bitcoin::consensus::encode::serialize(&txns).len();
    assert!(blocktxn_n > 64, "a blocktxn is not the fallback size");
    assert_eq!(
        rbitcoin_net::peers::outbound_msg_bytes(&NetworkMessage::BlockTxn(
            bitcoin::p2p::message_compact_blocks::BlockTxn { transactions: txns }
        )),
        blocktxn_n
    );
    assert_eq!(rbitcoin_net::peers::PEER_SEND_BUDGET, 4 * 1024 * 1024);
    peer.note_send_written(peer.send_queued());
    peer.note_send_queued(rbitcoin_net::peers::PEER_SEND_BUDGET);
    assert!(
        !peer.send_over_budget(),
        "the cap itself is still inside the budget"
    );
    peer.note_send_queued(1);
    assert!(peer.send_over_budget());
    peer.note_send_written(peer.send_queued());

    while out_rx.try_recv().is_ok() {}
    handle_peer_inventory_msg(
        &NetworkMessage::GetAddr,
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
    )
    .unwrap();
    let mut addrs = 0usize;
    while let Ok(msg) = out_rx.try_recv() {
        if matches!(
            msg.expect_msg(),
            NetworkMessage::Addr(_) | NetworkMessage::AddrV2(_)
        ) {
            addrs += 1;
        }
    }
    assert_eq!(addrs, 1, "the first getaddr is answered");
    handle_peer_inventory_msg(
        &NetworkMessage::GetAddr,
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
    )
    .unwrap();
    assert!(
        out_rx.try_recv().is_err(),
        "a second getaddr is not answered"
    );

    hostile_addr_relay(&peers, &peer, &ver, &mut follow).await;

    follow.ban_score = follow
        .ban_score
        .saturating_add(tx_reject_ban_score(&AcceptError::Script(
            "script false".into(),
        )));
    assert_eq!(follow.ban_score, 10, "an invalid script raises ban score");
    follow.ban_score = follow.ban_score.saturating_add(tx_reject_ban_score(
        &AcceptError::Policy("bad-txns-too-many-sigops"),
    ));
    assert_eq!(follow.ban_score, 10, "a policy reject does not raise ban score");

    let _ = std::fs::remove_dir_all(dir);
}


#[allow(clippy::cognitive_complexity)] // address-relay chapter of the same session
async fn hostile_addr_relay(
    peers: &std::sync::Arc<rbitcoin_net::peers::PeerHub>,
    peer: &std::sync::Arc<rbitcoin_net::peers::LivePeer>,
    ver: &VersionMessage,
    follow: &mut PeerFollowState,
) {
        peer.set_wants_addrv2();
        let (src_tx, mut src_rx) = mpsc::unbounded_channel();
        peer.attach_out(src_tx);
        let mut neigh = Vec::new();
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22000);
        let dst = peers.register(a, a, ver, true, rbitcoin_net::peers::PeerConnType::Inbound);
        dst.set_wants_addrv2();
        let (tx, rx) = mpsc::unbounded_channel();
        dst.attach_out(tx);
        neigh.push((dst.id, rx));
        let quiet_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 23000);
        let quiet = peers.register(
            quiet_addr,
            quiet_addr,
            ver,
            true,
            rbitcoin_net::peers::PeerConnType::Inbound,
        );
        let (quiet_tx, mut quiet_rx) = mpsc::unbounded_channel();
        quiet.attach_out(quiet_tx);

        let mut fresh = AddrTokenBucket::new(1_000);
        assert_eq!(fresh.take(5, 1_000), 1, "a new bucket holds one token");
        assert_eq!(fresh.take(1, 10_999), 0, "under 10 s is less than a token");
        let mut ten = AddrTokenBucket::new(1_000);
        assert_eq!(ten.take(1, 1_000), 1);
        assert_eq!(ten.take(1, 11_000), 1, "10 s refills one token");
        let mut capped = AddrTokenBucket::new(0);
        assert_eq!(
            capped.take(5_000, 1_000_000_000),
            MAX_ADDR_TO_SEND,
            "refill stops at the cap"
        );
        // A quiet peer refills its bucket: let 10_000 s of node time pass.
        peers.set_mock_now(peer.connected_at_secs() + 10_000);

        let list: Vec<AddrV2Message> = (1..=3u8)
            .map(|i| AddrV2Message {
                time: 1_700_000_000,
                services: ServiceFlags::NETWORK,
                addr: AddrV2::Ipv4(Ipv4Addr::new(123, 123, 123, i)),
                port: 8333,
            })
            .collect();
        rbitcoin_log::capture_logs(true);
        on_addrv2(follow, Some(peer.as_ref()), &list).unwrap();
        let lines = rbitcoin_log::take_logs();
        rbitcoin_log::capture_logs(false);
        let mut batched = None;
        let mut batched_id = None;
        for (id, rx) in &mut neigh {
            if let Ok(msg) = rx.try_recv() {
                assert!(rx.try_recv().is_err(), "the neighbor gets a single message");
                batched_id = Some(*id);
                batched = Some(msg.expect_msg());
            }
        }
        let got = batched.expect("one addrv2");
        let NetworkMessage::AddrV2(sent) = &got else {
            panic!("expected addrv2, got {got:?}");
        };
        assert_eq!(sent, &list);
        let nbytes = serialize(&got).len();
        let want = sending_addrv2_log(nbytes, batched_id.expect("neighbor id"));
        assert!(
            lines.iter().any(|(_, l)| l == &want),
            "missing {want:?} in {lines:?}"
        );

        for i in 1..4u16 {
            let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22000 + i);
            let p = peers.register(a, a, ver, true, rbitcoin_net::peers::PeerConnType::Inbound);
            p.set_wants_addrv2();
            let (tx, rx) = mpsc::unbounded_channel();
            p.attach_out(tx);
            neigh.push((p.id, rx));
        }

        let n = neigh.len();
        let mut even_port = None;
        let mut odd_port = None;
        let mut wrap_port = None;
        for port in 1..20_000u16 {
            let key = addr_key_oracle(&hostile_addr(port));
            if (key as usize) < n * n {
                continue;
            }
            let start = (key as usize) % n;
            if key & 1 == 0 {
                even_port.get_or_insert(port);
            } else if start + 1 == n {
                wrap_port.get_or_insert(port);
            } else if start != 0 {
                odd_port.get_or_insert(port);
            }
            if even_port.is_some() && odd_port.is_some() && wrap_port.is_some() {
                break;
            }
        }
        let ports = [
            even_port.expect("an even relay key"),
            odd_port.expect("an odd relay key whose next neighbor does not wrap"),
            wrap_port.expect("an odd relay key whose next neighbor wraps"),
        ];
        for port in ports {
            let msg = hostile_addr(port);
            on_addrv2(follow, Some(peer.as_ref()), std::slice::from_ref(&msg)).unwrap();
            let key = addr_key_oracle(&msg);
            let n_dest = if key & 1 == 0 { 1 } else { 2 };
            let start = (key as usize) % n;
            let mut expect = Vec::new();
            for step in 0..n_dest {
                let idx = if step == 0 {
                    start
                } else if start + 1 == n {
                    0
                } else {
                    start + 1
                };
                expect.push(neigh[idx].0);
            }
            expect.sort_unstable();
            let mut got_ids = Vec::new();
            for (id, rx) in &mut neigh {
                while rx.try_recv().is_ok() {
                    got_ids.push(*id);
                }
            }
            got_ids.sort_unstable();
            assert_eq!(
                got_ids, expect,
                "port {port} key {key:#x} start {start} must select those neighbors"
            );
            assert!(src_rx.try_recv().is_err(), "a peer does not relay to itself");
            assert!(
                quiet_rx.try_recv().is_err(),
                "a peer that did not ask for addrv2 is skipped"
            );
        }

        on_addrv2(follow, Some(peer.as_ref()), &[hostile_addr(8333)]).unwrap();
        let mut reached = 0usize;
        for (_, rx) in &mut neigh {
            if rx.try_recv().is_ok() {
                reached += 1;
            }
        }
        assert!(
            (1..=2).contains(&reached),
            "one address reached {reached} neighbors"
        );
        let burst: Vec<_> = (1..1000u16).map(hostile_addr).collect();
        on_addrv2(follow, Some(peer.as_ref()), &burst).unwrap();
        for (_, rx) in &mut neigh {
            while rx.try_recv().is_ok() {}
        }
        on_addrv2(follow, Some(peer.as_ref()), &[hostile_addr(9_000)]).unwrap();
        let mut extra_relay = 0usize;
        for (_, rx) in &mut neigh {
            if rx.try_recv().is_ok() {
                extra_relay += 1;
            }
        }
        assert_eq!(extra_relay, 0, "past the 1000-address burst nothing is relayed");

}

fn addr_flood(first_octet: u8) -> Vec<AddrV2Message> {
    (0..MAX_ADDR_TO_SEND as u32)
        .map(|i| AddrV2Message {
            time: 1_700_000_000,
            services: ServiceFlags::NETWORK,
            addr: AddrV2::Ipv4(Ipv4Addr::new(first_octet, (i >> 8) as u8, i as u8, 1)),
            port: 8333,
        })
        .collect()
}

/// Core v31.1 ADDR processing: a peer's token bucket starts at one address,
/// and an address with no token left is dropped before AddrMan, not only
/// before relay. A peer with the `addr` permission is not rate limited.
#[test]
fn addrv2_flood_is_rate_limited_before_addrman() {
    for (perm, want_learned) in [(false, 1usize), (true, MAX_ADDR_TO_SEND)] {
        let peers = rbitcoin_net::peers::PeerHub::new();
        if perm {
            let mut t = rbitcoin_net::NetPermTable::default();
            t.whitelist
                .push(rbitcoin_net::parse_whitelist("addr@127.0.0.1").unwrap());
            peers.set_net_perms(t);
        }
        let am = std::sync::Arc::new(std::sync::Mutex::new(rbitcoin_net::seeds::AddrMan::new()));
        peers.set_addrman(am.clone());
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
        let ver = hostile_ver(bind);
        let peer = peers.register(bind, bind, &ver, true, rbitcoin_net::peers::PeerConnType::Inbound);
        let mut follow = PeerFollowState::new();
        on_addrv2(&mut follow, Some(peer.as_ref()), &addr_flood(11)).unwrap();
        assert_eq!(
            am.lock().unwrap().len(),
            want_learned,
            "addr permission {perm}: addresses that reach AddrMan"
        );
        let info = peers
            .snapshot()
            .into_iter()
            .find(|p| p.id == peer.id)
            .expect("peer info");
        let limited = (MAX_ADDR_TO_SEND - want_learned) as u64;
        assert_eq!(
            (info.addr_processed, info.addr_rate_limited),
            (want_learned as u64, limited),
            "getpeerinfo addr counters, addr permission {perm}"
        );
    }
}

fn regtest_child(parent: &bitcoin::block::Header, tag: u8) -> bitcoin::block::Header {
    pow_child(parent, u32::from(tag))
}

fn pow_child(parent: &bitcoin::block::Header, n: u32) -> bitcoin::block::Header {
    let mut header = *parent;
    header.prev_blockhash = parent.block_hash();
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&n.to_le_bytes());
    header.merkle_root = bitcoin::TxMerkleNode::from_byte_array(merkle);
    header.time = parent.time.saturating_add(600);
    header.bits = bitcoin::CompactTarget::from_consensus(0x207f_ffff);
    header.nonce = 0;
    rbitcoin_consensus::grind_regtest_pow(&mut header);
    header
}

/// Does not connect to a header we have, and the hash misses `bits`.
/// A non-connecting header stays in the map without a store write.
fn unconnected_missing_target(n: u32) -> bitcoin::block::Header {
    let mut header = bitcoin::block::Header {
        version: bitcoin::block::Version::TWO,
        prev_blockhash: BlockHash::from_byte_array([0x99; 32]),
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
        nonce: n,
    };
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&n.to_le_bytes());
    header.merkle_root = bitcoin::TxMerkleNode::from_byte_array(merkle);
    let start = header.nonce;
    loop {
        if header.validate_pow(header.target()).is_err() {
            return header;
        }
        header.nonce = header.nonce.wrapping_add(1);
        assert_ne!(header.nonce, start, "regtest target rejected no nonce");
    }
}

/// Same shape as [`pow_child`], but the hash misses `bits`. No grind.
fn child_missing_target(parent: &bitcoin::block::Header, n: u32) -> bitcoin::block::Header {
    let mut header = *parent;
    header.prev_blockhash = parent.block_hash();
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&n.to_le_bytes());
    header.merkle_root = bitcoin::TxMerkleNode::from_byte_array(merkle);
    header.time = parent.time.saturating_add(600);
    header.bits = bitcoin::CompactTarget::from_consensus(0x207f_ffff);
    header.nonce = n;
    let start = header.nonce;
    loop {
        if header.validate_pow(header.target()).is_err() {
            return header;
        }
        header.nonce = header.nonce.wrapping_add(1);
        assert_ne!(header.nonce, start, "regtest target rejected no nonce");
    }
}

fn pending_keys(follow: &PeerFollowState) -> HashSet<BlockHash> {
    follow.pending_headers.keys().copied().collect()
}

/// Put `headers` in the pending map. Used to reach [`MAX_PENDING_HEADERS`]
/// without a clone of that map on every header.
fn fill_pending(follow: &mut PeerFollowState, headers: &[bitcoin::block::Header]) {
    for header in headers {
        follow.pending_headers.insert(header.block_hash(), *header);
    }
}

/// Tip height for the pending-cap journeys. A one-block fork from two below
/// this tip has less work than the tip. The hostile locator journey stays at
/// height 200: its getheaders reply is the whole chain.
const CAP_TIP: u32 = 4;

fn nonconnecting_header(nonce: u32) -> bitcoin::block::Header {
    bitcoin::block::Header {
        version: bitcoin::block::Version::TWO,
        prev_blockhash: BlockHash::from_byte_array([0x99; 32]),
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
        nonce,
    }
}

/// Fill the pending map to [`MAX_PENDING_HEADERS`] with non-connecting
/// headers. The first header is admitted by `on_headers`. The rest are the
/// same headers, inserted without cloning the map on every header.
async fn fill_nonconnecting_to_cap(
    hub: &ChainHub,
    out_tx: &mpsc::UnboundedSender<PeerOut>,
    follow: &mut PeerFollowState,
    peer: &rbitcoin_net::peers::LivePeer,
) {
    let headers: Vec<bitcoin::block::Header> = (0..MAX_PENDING_HEADERS as u32)
        .map(nonconnecting_header)
        .collect();
    on_headers(hub, out_tx, follow, Some(peer), &[headers[0]])
        .await
        .unwrap();
    assert!(
        follow
            .pending_headers
            .contains_key(&headers[0].block_hash()),
        "a non-connecting header was not admitted"
    );
    fill_pending(follow, &headers[1..]);
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
}

/// At `MAX_PENDING_HEADERS`, a header the store will not keep must not drop
/// a non-connecting header or a fork that loses to the tip. A header the
/// store accepts still drops a losing fork and keeps one with more work.
#[tokio::test]
async fn cap_evicts_only_after_the_store_accepts() {
    use bitcoin::block::Header;
    use std::cmp::Ordering;

    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-store");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();

    let mut above_limit = tip;
    above_limit.prev_blockhash = tip.block_hash();
    above_limit.merkle_root = bitcoin::TxMerkleNode::from_byte_array([0x11; 32]);
    above_limit.time = tip.time.saturating_add(600);
    above_limit.bits = bitcoin::CompactTarget::from_consensus(0x217f_ffff);
    above_limit.nonce = 0;
    assert!(
        above_limit.validate_pow(above_limit.target()).is_ok(),
        "a target above the pow limit still meets its own bits"
    );
    assert!(
        !hub.header_claimed_pow_ok(&above_limit),
        "pow limit rejects this target"
    );

    let mut bad_diff = tip;
    bad_diff.prev_blockhash = tip.block_hash();
    bad_diff.merkle_root = bitcoin::TxMerkleNode::from_byte_array([0x22; 32]);
    bad_diff.time = tip.time.saturating_add(600);
    bad_diff.bits = bitcoin::CompactTarget::from_consensus(0x207f_fffe);
    bad_diff.nonce = 0;
    rbitcoin_consensus::grind_regtest_pow(&mut bad_diff);
    assert!(hub.header_claimed_pow_ok(&bad_diff));
    let bad_diff_hash = bad_diff.block_hash();

    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let mut follow = PeerFollowState::new();
    fill_nonconnecting_to_cap(&hub, &out_tx, &mut follow, &peer).await;
    let junk_hash = *follow.pending_headers.keys().next().unwrap();
    for rejected in [&above_limit, &bad_diff] {
        let before = pending_keys(&follow);
        on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[*rejected])
            .await
            .unwrap();
        assert_eq!(
            pending_keys(&follow),
            before,
            "a header the store rejects does not evict non-connecting headers"
        );
    }
    assert!(follow.pending_headers.contains_key(&junk_hash));
    assert!(hub
        .query
        .get_header_by_hash(&bad_diff_hash.to_byte_array())
        .unwrap()
        .is_none());

    let parent = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP - 1))
        .unwrap();
    // A child of genesis is not the next block's difficulty. The store
    // rejects it. A one-block fork from two below the tip has less work
    // than the tip and is a header the store keeps.
    let fork_parent = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP - 2))
        .unwrap();
    let heavier_a = regtest_child(&parent, 0xa1);
    let heavier_b = regtest_child(&heavier_a, 0xa2);
    let heavier_tip = heavier_b.block_hash();
    let mut follow = PeerFollowState::new();
    let peer = peers.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445),
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[heavier_a, heavier_b],
    )
    .await
    .unwrap();
    assert_eq!(
        announced_work_cmp(&hub, &follow.pending_headers, heavier_tip),
        Some(Ordering::Greater)
    );
    let losers: Vec<Header> = (0..MAX_PENDING_HEADERS as u32 - 2)
        .map(|n| pow_child(&fork_parent, n))
        .collect();
    let loser_hash = losers[0].block_hash();
    let loser_hashes: HashSet<BlockHash> = losers.iter().map(|header| header.block_hash()).collect();
    // One sibling through the admit path. The rest fill the cap. Each
    // `on_headers` clones the whole map, and the eviction below is the
    // call under test.
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[losers[0]])
        .await
        .unwrap();
    fill_pending(&mut follow, &losers[1..]);
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    assert_eq!(
        announced_work_cmp(&hub, &follow.pending_headers, loser_hash),
        Some(Ordering::Less)
    );
    for rejected in [&above_limit, &bad_diff] {
        let before = pending_keys(&follow);
        on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[*rejected])
            .await
            .unwrap();
        assert_eq!(
            pending_keys(&follow),
            before,
            "a header the store rejects does not evict a losing fork"
        );
    }
    assert!(follow.pending_headers.contains_key(&heavier_tip));
    assert!(follow.pending_headers.contains_key(&loser_hash));

    let accepted = regtest_child(&tip, 0xb1);
    let accepted_hash = accepted.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[accepted])
        .await
        .unwrap();
    assert!(follow.pending_headers.contains_key(&accepted_hash));
    assert!(follow.pending_headers.contains_key(&heavier_tip));
    let dropped = loser_hashes
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(
        dropped, 1,
        "an accepted header drops one fork with less work"
    );
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    assert!(hub
        .query
        .get_header_by_hash(&accepted_hash.to_byte_array())
        .unwrap()
        .is_some());
}

/// Children of the tip have more work than the tip, so a full map of them
/// must still take one more. A longer fork than that child stays.
#[tokio::test]
async fn cap_accepts_the_next_header_when_equal_work_fills_the_map() {
    use std::cmp::Ordering;

    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-equal");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let parent = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP - 1))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let heavy_a = regtest_child(&parent, 0xc1);
    let heavy_b = regtest_child(&heavy_a, 0xc2);
    let heavy_c = regtest_child(&heavy_b, 0xc3);
    let heavy = heavy_c.block_hash();
    let mut follow = PeerFollowState::new();
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[heavy_a, heavy_b, heavy_c],
    )
    .await
    .unwrap();
    let siblings: Vec<_> = (0..MAX_PENDING_HEADERS as u32 - 3)
        .map(|n| pow_child(&tip, n))
        .collect();
    let sibling_hashes: HashSet<BlockHash> =
        siblings.iter().map(|header| header.block_hash()).collect();
    // Siblings are not one chain, so a batch would keep only its last header.
    // One goes through `on_headers`. The rest fill the cap. The calls below
    // are what evict.
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[siblings[0]])
        .await
        .unwrap();
    fill_pending(&mut follow, &siblings[1..]);
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    let sibling_hash = siblings[0].block_hash();
    assert_eq!(
        announced_work_cmp(&hub, &follow.pending_headers, sibling_hash),
        Some(Ordering::Greater),
        "a child of the tip has more work than the tip"
    );
    assert_eq!(
        announced_work_cmp(&hub, &follow.pending_headers, heavy),
        Some(Ordering::Greater)
    );

    let next = pow_child(&tip, 9_000_000);
    let next_hash = next.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[next])
        .await
        .unwrap();
    assert!(
        follow.pending_headers.contains_key(&next_hash),
        "a full map of tip children still accepts the next header"
    );
    assert!(follow.pending_headers.contains_key(&heavy));
    let dropped = sibling_hashes
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(dropped, 1, "one equal-work sibling makes the slot");
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);

    let grand = pow_child(&next, 9_000_001);
    let grand_hash = grand.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[grand])
        .await
        .unwrap();
    assert!(follow.pending_headers.contains_key(&grand_hash));
    assert!(
        follow.pending_headers.contains_key(&next_hash),
        "the parent of the new header stays"
    );
    assert!(follow.pending_headers.contains_key(&heavy));
    let dropped = sibling_hashes
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(
        dropped, 2,
        "the grandchild drops one more sibling and leaves the longer fork"
    );
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    assert_eq!(
        work_of_header_path(&hub, &follow.pending_headers, grand_hash),
        work_of_header_path(&hub, &follow.pending_headers, heavy),
        "the three-header fork matches the grandchild"
    );
}

/// Headers that miss their target and do not connect fill the map without a
/// grind. One of them makes room for a header that meets the target. A
/// connecting header that misses its target is not inserted.
#[tokio::test]
async fn cap_accepts_the_next_header_when_failed_pow_fills_the_map() {
    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-pow");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let parent = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP - 1))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let heavy_a = regtest_child(&parent, 0xd1);
    let heavy_b = regtest_child(&heavy_a, 0xd2);
    let heavy_c = regtest_child(&heavy_b, 0xd3);
    let heavy = heavy_c.block_hash();
    let mut follow = PeerFollowState::new();
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[heavy_a, heavy_b, heavy_c],
    )
    .await
    .unwrap();
    let missed = child_missing_target(&tip, 0);
    let missed_hash = missed.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[missed])
        .await
        .unwrap();
    assert!(
        !follow.pending_headers.contains_key(&missed_hash),
        "a connecting header that misses its target was inserted"
    );
    assert_eq!(follow.pending_headers.len(), 3);
    let sample = unconnected_missing_target(0);
    assert!(
        !hub.header_claimed_pow_ok(&sample),
        "the filler misses its target"
    );
    let fillers: Vec<_> = (0..MAX_PENDING_HEADERS as u32 - 3)
        .map(unconnected_missing_target)
        .collect();
    let filler_hashes: HashSet<BlockHash> =
        fillers.iter().map(|header| header.block_hash()).collect();
    // One filler through the admit path. The rest are the same headers.
    // The successor below is the eviction under test.
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[fillers[0]],
    )
    .await
    .unwrap();
    assert!(
        follow
            .pending_headers
            .contains_key(&fillers[0].block_hash()),
        "a header that misses its target was not admitted"
    );
    fill_pending(&mut follow, &fillers[1..]);
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    assert!(follow.pending_headers.contains_key(&heavy));

    let next = pow_child(&tip, 9_000_000);
    let next_hash = next.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[next])
        .await
        .unwrap();
    assert!(
        follow.pending_headers.contains_key(&next_hash),
        "a valid successor was refused"
    );
    assert!(follow.pending_headers.contains_key(&heavy));
    let dropped = filler_hashes
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(dropped, 1, "one header that misses its target makes the slot");
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
    assert!(hub
        .query
        .get_header_by_hash(&next_hash.to_byte_array())
        .unwrap()
        .is_some());
}

/// One new header repeated for a whole `headers` message needs one slot.
#[tokio::test]
async fn cap_counts_one_repeated_header() {
    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-repeat");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let mut follow = PeerFollowState::new();
    fill_nonconnecting_to_cap(&hub, &out_tx, &mut follow, &peer).await;
    let before = pending_keys(&follow);
    let mut next = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    next.prev_blockhash = hub.tip_hash().unwrap();
    next.merkle_root = bitcoin::TxMerkleNode::from_byte_array([0x61; 32]);
    next.time = next.time.saturating_add(600);
    next.nonce = 0;
    rbitcoin_consensus::grind_regtest_pow(&mut next);
    let next_hash = next.block_hash();
    let repeated = vec![next; MAX_HEADERS_RESULTS];
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &repeated)
        .await
        .unwrap();
    assert!(
        follow.pending_headers.contains_key(&next_hash),
        "the repeated header was refused"
    );
    let dropped = before
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(dropped, 1, "one repeated header takes one slot");
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
}

fn bad_diffbits_child(tip: &bitcoin::block::Header) -> bitcoin::block::Header {
    let mut header = *tip;
    header.prev_blockhash = tip.block_hash();
    header.merkle_root = bitcoin::TxMerkleNode::from_byte_array([0x22; 32]);
    header.time = tip.time.saturating_add(600);
    header.bits = bitcoin::CompactTarget::from_consensus(0x207f_fffe);
    header.nonce = 0;
    rbitcoin_consensus::grind_regtest_pow(&mut header);
    header
}

/// Room left in the map. A child the store rejects for `bad-diffbits` is not
/// inserted, and block asks do not move for it. Harder regtest bits would
/// otherwise outrank the next valid header.
#[tokio::test]
async fn cap_store_kept_headers_skip_a_rejected_tip_child() {
    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-reject");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let mut follow = PeerFollowState::new();
    let ask = BlockHash::from_byte_array([0x44; 32]);
    follow.requested_blocks.insert(ask);
    let bad = bad_diffbits_child(&tip);
    assert!(hub.header_claimed_pow_ok(&bad));
    let bad_hash = bad.block_hash();
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &[bad])
        .await
        .unwrap();
    assert!(
        !follow.pending_headers.contains_key(&bad_hash),
        "a header the store rejects was inserted while the map had room"
    );
    assert!(hub
        .query
        .get_header_by_hash(&bad_hash.to_byte_array())
        .unwrap()
        .is_none());
    assert!(
        follow.requested_blocks.contains(&ask),
        "a rejected header dropped an ask"
    );
    assert!(
        !follow.requested_blocks.contains(&bad_hash),
        "a rejected header was asked for"
    );
    assert!(follow.pending_headers.is_empty());
}

/// The last header of a message can be stored while an earlier hash is not
/// on that walk. Only the stored hash is admitted. A full map still drops
/// enough stale headers for that one hash.
#[tokio::test]
async fn cap_store_kept_headers_skip_an_earlier_hash() {
    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-walk");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let mut follow = PeerFollowState::new();
    fill_nonconnecting_to_cap(&hub, &out_tx, &mut follow, &peer).await;
    let before = pending_keys(&follow);
    let side = pow_child(&tip, 0x51);
    let accepted = pow_child(&tip, 0x52);
    let side_hash = side.block_hash();
    let accepted_hash = accepted.block_hash();
    on_headers(
        &hub,
        &out_tx,
        &mut follow,
        Some(&peer),
        &[side, accepted],
    )
    .await
    .unwrap();
    assert!(
        !follow.pending_headers.contains_key(&side_hash),
        "an earlier header the store never saw was admitted"
    );
    assert!(hub
        .query
        .get_header_by_hash(&side_hash.to_byte_array())
        .unwrap()
        .is_none());
    assert!(follow.pending_headers.contains_key(&accepted_hash));
    assert!(hub
        .query
        .get_header_by_hash(&accepted_hash.to_byte_array())
        .unwrap()
        .is_some());
    let dropped = before
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(dropped, 1, "only the stored header takes a slot");
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
}

/// The last header is already in the store, so this call writes nothing.
/// Proof-of-work headers in front of it must not clear the map.
#[tokio::test]
async fn cap_already_stored_tip_frees_no_slot() {
    let (_dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cap-evict-stored-tip");
    hub.ensure_genesis().unwrap();
    hub.generate_to_script(CAP_TIP, ScriptBuf::from_bytes(vec![0x51]), vec![])
        .unwrap();
    let tip = hub
        .query
        .wire_header_at_height(rbitcoin_primitives::Height(CAP_TIP))
        .unwrap();
    let peers = rbitcoin_net::peers::PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let ver = hostile_ver(bind);
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let peer = peers.register(
        bind,
        bind,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    let mut follow = PeerFollowState::new();
    fill_nonconnecting_to_cap(&hub, &out_tx, &mut follow, &peer).await;
    let before = pending_keys(&follow);
    let front: Vec<Header> = (0..2u32).map(|n| pow_child(&tip, 0x70 + n)).collect();
    let front_hashes: Vec<BlockHash> = front.iter().map(|h| h.block_hash()).collect();
    let mut message = front;
    message.push(tip);
    on_headers(&hub, &out_tx, &mut follow, Some(&peer), &message)
        .await
        .unwrap();
    for hash in &front_hashes {
        assert!(
            !follow.pending_headers.contains_key(hash),
            "a header in front of the stored tip was admitted"
        );
        assert!(hub
            .query
            .get_header_by_hash(&hash.to_byte_array())
            .unwrap()
            .is_none());
    }
    let dropped = before
        .iter()
        .filter(|hash| !follow.pending_headers.contains_key(*hash))
        .count();
    assert_eq!(dropped, 0, "an already-stored last header frees no slot");
    assert_eq!(follow.pending_headers.len(), MAX_PENDING_HEADERS);
}
