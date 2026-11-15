use bitcoin::hashes::Hash;
use bitcoin::p2p::address::{AddrV2Message, Address};
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::ServiceFlags;
use bitcoin::{BlockHash, Wtxid};
pub use rbitcoin_net::peers::*;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use std::net::Ipv4Addr;

fn ver(ua: &str) -> VersionMessage {
    VersionMessage {
        version: 70016,
        services: ServiceFlags::NETWORK | ServiceFlags::WITNESS | ServiceFlags::P2P_V2,
        timestamp: 0,
        receiver: Address::new(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            ServiceFlags::NONE,
        ),
        sender: Address::new(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2),
            ServiceFlags::NONE,
        ),
        nonce: 1,
        user_agent: ua.into(),
        start_height: 0,
        relay: true,
    }
}

#[test]
fn pending_sendcmpct_from_u8_maps_wire_payload() {
    assert_eq!(PendingSendCmpct::from_u8(0), PendingSendCmpct::None);
    assert_eq!(PendingSendCmpct::from_u8(1), PendingSendCmpct::Lb);
    assert_eq!(PendingSendCmpct::from_u8(2), PendingSendCmpct::Hb);
    assert_eq!(PendingSendCmpct::from_u8(3), PendingSendCmpct::None);
    assert_eq!(PendingSendCmpct::from_u8(255), PendingSendCmpct::None);
    assert_eq!(PendingSendCmpct::None as u8, 0);
    assert_eq!(PendingSendCmpct::Lb as u8, 1);
    assert_eq!(PendingSendCmpct::Hb as u8, 2);
}

#[test]
fn trying_connection_log_is_p2p_not_v1() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(25, 0, 0, 1)), 8333);
    assert_eq!(
        trying_connection_log(PeerConnType::OutboundFullRelay, addr),
        "p2p: trying connection (outbound-full-relay) to 25.0.0.1:8333"
    );
    assert_eq!(
        trying_connection_log(PeerConnType::AddrFetch, addr),
        "p2p: trying connection (addr-fetch) to 25.0.0.1:8333"
    );
}

#[test]
fn snapshot_hides_fin_completed_keeps_connecting() {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let local = std::net::TcpStream::connect(addr).unwrap();
    let (far, _) = listener.accept().unwrap();
    drop(far);
    let _ = local.shutdown(std::net::Shutdown::Both);

    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let connecting = hub.register_connecting(a, a, true, PeerConnType::Inbound);
    connecting.attach_tcp_shutdown(local.try_clone().unwrap());
    assert_eq!(
        hub.snapshot().len(),
        1,
        "connecting peer stays in getpeerinfo during v2 handshake"
    );

    let done = hub.register(a, a, &ver("/rbitcoin:test/"), false, PeerConnType::Inbound);
    assert_eq!(
        done.sent_feefilter_sat_kvb(),
        -1,
        "registration has not sent a feefilter"
    );
    done.attach_tcp_shutdown(local);
    assert!(
        hub.snapshot().iter().all(|p| p.id != done.id),
        "completed FIN'd peer must not appear in getpeerinfo"
    );
    connecting.request_disconnect();
    assert!(
        hub.snapshot().iter().all(|p| p.id != connecting.id),
        "stopped connecting peer is omitted"
    );
}

#[test]
fn disconnect_id_shuts_down_tcp_so_far_side_sees_eof() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut local = std::net::TcpStream::connect(addr).unwrap();
    let (mut far, _) = listener.accept().unwrap();
    far.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445);
    let p = hub.register(
        a,
        b,
        &ver("/rbitcoin:0.1.0(testnode0)/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let killer = local.try_clone().unwrap();
    p.attach_tcp_shutdown(killer);

    assert!(hub.disconnect_id(0));

    let start = Instant::now();
    let mut buf = [0u8; 1];
    let n = far.read(&mut buf);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "far side must observe close promptly, took {:?}",
        start.elapsed()
    );
    match n {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
            ) => {}
        other => panic!("expected EOF/reset after disconnect_id, got {other:?}"),
    }
    let _ = local.write(&[1]);
}

#[test]
fn connecting_peer_v2_timeout_log_before_transport() {
    rbitcoin_log::capture_logs(true);
    let hub = PeerHub::new();
    hub.set_peer_timeout_secs(3);
    hub.set_mock_now(1_700_000_000);
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let p = hub.register_connecting(a, a, true, PeerConnType::Inbound);
    assert!(!p.handshake_complete());
    hub.set_mock_now(1_700_000_002);
    assert!(!p.stop.load(Ordering::SeqCst), "still inside peertimeout");
    hub.set_mock_now(1_700_000_003);
    let logs = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(
        p.stop.load(Ordering::SeqCst),
        "peertimeout must disconnect pre-verack"
    );
    assert!(
        hub.get(p.id).is_none(),
        "timed-out connecting peer is dropped"
    );
    assert!(
        logs.iter()
            .any(|(_, m)| m.contains("v2 handshake timed out, disconnecting peer=0")),
        "expected a native v2 handshake timeout, got {logs:?}"
    );
}

#[test]
fn p2p_timeouts_v2_logs_all_three_connecting_peer_ids() {
    rbitcoin_log::capture_logs(true);
    let hub = PeerHub::new();
    hub.set_peer_timeout_secs(3);
    hub.set_mock_now(1_700_000_000);
    let addr = |p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p);
    let peers: Vec<_> = (1..=3)
        .map(|p| {
            let s = hub.register_connecting(addr(p), addr(p), true, PeerConnType::Inbound);
            s.mark_v2_transport_ready();
            s
        })
        .collect();
    assert_eq!(peers.iter().map(|p| p.id).collect::<Vec<_>>(), [0, 1, 2]);
    hub.set_mock_now(1_700_000_003);
    let logs = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    let got: Vec<u64> = logs
        .iter()
        .filter_map(|(_, m)| {
            [0u64, 1, 2].into_iter().find(|id| {
                m.contains(&format!(
                    "version handshake timeout, disconnecting peer={id}"
                ))
            })
        })
        .collect();
    assert_eq!(
        got,
        vec![0, 1, 2],
        "timeout needles before TCP close, id order, got {logs:?}"
    );
}

#[test]
fn handshake_timeout_tick_logs_when_stop_already_set() {
    rbitcoin_log::capture_logs(true);
    let hub = PeerHub::new();
    hub.set_peer_timeout_secs(3);
    hub.set_mock_now(1_700_000_000);
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let p = hub.register_connecting(a, a, true, PeerConnType::Inbound);
    p.mark_v2_transport_ready();
    p.request_disconnect();
    let _ = rbitcoin_log::take_logs();
    hub.mock_now.store(1_700_000_003, Ordering::Release);
    let policy = rbitcoin_net::peer::HandshakePolicy {
        hub: None,
        peers: Some(&hub),
        session: Some(p.as_ref()),
        conn_type: PeerConnType::Inbound,
    };
    let err = rbitcoin_net::peer::fail_if_handshake_timed_out(&policy);
    let logs = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(
        matches!(err, Err(rbitcoin_net::error::NetError::Timeout)),
        "got {err:?}"
    );
    assert!(
        logs.iter()
            .any(|(_, m)| m.contains("version handshake timeout, disconnecting peer=0")),
        "pre-verack ping vs mocktime must still log the needle, got {logs:?}"
    );
}

/// One PeerHub while peers come and go: compact high-bandwidth
/// selection and its evictions, compact fill slots, per-peer block and
/// header marks, the single initial headers-sync peer, and each peer's
/// announced-wtxid set.
#[allow(clippy::cognitive_complexity)] // one hub, many peer-state arms
#[test]
fn peerhub_hb_select() {
    let hub = PeerHub::new();
    let join = |port: u16, inbound: bool| {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let conn = if inbound {
            PeerConnType::Inbound
        } else {
            PeerConnType::OutboundFullRelay
        };
        hub.register(a, a, &ver("/rbitcoin:0.1.0(testnode0)/"), inbound, conn)
    };
    let hb = |p: &LivePeer| p.hb_to.load(Ordering::Relaxed);
    let pending = |p: &LivePeer| p.pending_sendcmpct.load(Ordering::Relaxed);
    let sendcmpct = |rx: &mut mpsc::UnboundedReceiver<PeerOut>| match rx
        .try_recv()
        .expect("sendcmpct")
        .expect_msg()
    {
        NetworkMessage::SendCmpct(sc) => {
            assert_eq!(sc.version, 2);
            sc.send_compact
        }
        other => panic!("expected sendcmpct, got {other:?}"),
    };

    // An unknown id is ignored. A lone outbound is selected first.
    hub.maybe_select_hb(9_999);
    let out = join(18444, false);
    hub.maybe_select_hb(out.id);
    assert!(hb(&out));

    // Without a writer the HB choice waits as pending; with one it goes
    // out as sendcmpct at once.
    let a = join(18445, true);
    hub.maybe_select_hb(a.id);
    hub.maybe_select_hb(a.id);
    assert!(hb(&a));
    assert_eq!(pending(&a), PendingSendCmpct::Hb as u8);
    let b = join(18446, true);
    let (b_tx, mut b_rx) = mpsc::unbounded_channel();
    b.attach_out(b_tx);
    hub.maybe_select_hb(b.id);
    assert!(hb(&b) && sendcmpct(&mut b_rx));
    assert_eq!(pending(&b), 0);

    // A third inbound evicts the oldest inbound and keeps the lone
    // outbound. The evicted peer's LB choice waits as pending.
    let c = join(18447, true);
    hub.maybe_select_hb(c.id);
    assert!(hb(&out) && hb(&b) && hb(&c));
    assert!(
        !hb(&a),
        "lone outbound stays; first inbound is the eviction"
    );
    assert_eq!(pending(&a), PendingSendCmpct::Lb as u8);

    // Re-selecting b refreshes it (Core LRU), so the next inbound evicts c.
    hub.maybe_select_hb(b.id);
    let d = join(18448, true);
    hub.maybe_select_hb(d.id);
    assert!(hb(&b), "re-selected inbound stays HB");
    assert!(!hb(&c), "oldest unrefreshed inbound is evicted");
    assert!(hb(&d));

    // d leaves. Its slot is free, so e does not evict a live HB peer.
    hub.unregister(d.id);
    let e = join(18449, true);
    hub.maybe_select_hb(e.id);
    assert!(hb(&out) && hb(&b) && hb(&e));

    // The outbound leaves too. With no outbound the oldest HB peer, b,
    // is evicted, and its writer gets sendcmpct LB.
    hub.unregister(out.id);
    let f = join(18450, true);
    hub.maybe_select_hb(f.id);
    let g = join(18451, true);
    hub.maybe_select_hb(g.id);
    assert!(!hb(&b) && !sendcmpct(&mut b_rx));
    assert_eq!(pending(&b), 0);
    assert!(hb(&e) && hb(&f) && hb(&g));

    // Two compact fill slots per block. A release frees one; a peer that
    // leaves releases its slot.
    let blk = BlockHash::from_byte_array([0x11; 32]);
    assert!(hub.try_cmpct_fill_slot(blk, true));
    assert!(hub.try_cmpct_fill_slot(blk, true));
    assert!(!hub.try_cmpct_fill_slot(blk, true));
    hub.release_cmpct_fill(blk, true);
    assert!(hub.try_cmpct_fill_slot(blk, true));
    assert!(!hub.try_cmpct_fill_slot(blk, true));
    hub.release_cmpct_fill(blk, true);
    hub.release_cmpct_fill(blk, true);
    let blk = BlockHash::from_byte_array([0x22; 32]);
    assert!(g.try_cmpct_fill(blk));
    hub.unregister(g.id);
    assert!(hub.try_cmpct_fill_slot(blk, true));
    assert!(hub.try_cmpct_fill_slot(blk, true));
    assert!(!hub.try_cmpct_fill_slot(blk, true));

    // A block e sent is not announced back to e, once. Header marks.
    let blk = BlockHash::from_byte_array([0xab; 32]);
    assert!(!e.take_block_from_peer(&blk));
    e.note_block_from_peer(blk);
    assert!(e.take_block_from_peer(&blk));
    assert!(!e.take_block_from_peer(&blk));
    assert!(e.try_ask_headers_for_inv());
    assert!(!e.try_ask_headers_for_inv());
    e.note_best_header_sent(blk);
    e.note_best_known(blk);
    assert_eq!(e.header_marks(), (Some(blk), Some(blk)));
    assert!(e.advertises_network());

    // With an old tip only one peer runs initial headers sync. Inv adds
    // at most one extra getheaders peer per hash. When the sync peer
    // leaves, another may start.
    let now = 1_700_000_000;
    let genesis_time = 1_231_006_505;
    assert!(hub.try_start_headers_sync(&a, now, genesis_time));
    assert!(a.is_sync_started());
    assert!(!hub.try_start_headers_sync(&c, now, genesis_time));
    assert!(!c.is_sync_started());
    let (h1, h2) = (
        BlockHash::from_byte_array([1u8; 32]),
        BlockHash::from_byte_array([2u8; 32]),
    );
    assert!(hub.should_getheaders_for_inv(&a, h1));
    assert!(hub.should_getheaders_for_inv(&c, h1));
    assert!(!hub.should_getheaders_for_inv(&f, h1));
    assert!(hub.should_getheaders_for_inv(&f, h2));
    hub.unregister(a.id);
    assert!(!a.is_sync_started());
    assert!(hub.try_start_headers_sync(&c, now, genesis_time));

    // Announced wtxids are per peer, roll the oldest at the cap, and
    // tx inv is due on a timer or on request.
    let wtxid_n = |i: u32| {
        let mut w = [0u8; 32];
        w[..4].copy_from_slice(&i.to_le_bytes());
        Wtxid::from_byte_array(w)
    };
    e.note_announced_wtx(wtxid_n(0));
    assert!(e.has_announced_wtx(&wtxid_n(0)));
    assert!(!e.has_announced_wtx(&wtxid_n(1)));
    assert!(!f.has_announced_wtx(&wtxid_n(0)));
    assert!(!e.take_tx_inv_due(1_700_000_000));
    assert!(!e.take_tx_inv_due(1_700_000_010));
    assert!(e.take_tx_inv_due(1_700_000_040));
    e.request_tx_inv();
    assert!(e.take_tx_inv_due(1_700_000_041));
    for i in 1..=50_000 {
        e.note_announced_wtx(wtxid_n(i));
    }
    assert!(!e.has_announced_wtx(&wtxid_n(0)));
    assert!(e.has_announced_wtx(&wtxid_n(1)));
    assert!(e.has_announced_wtx(&wtxid_n(50_000)));
}

#[test]
fn session_heartbeat_replaces_stalled_headers_sync_peer_only_with_another_preferred() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
    let outbound = hub.register(
        a,
        a,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let wall = hub.now_secs();
    let start = wall.saturating_sub(16 * 60);
    assert!(hub.try_start_headers_sync(&outbound, start, start));
    hub.on_session_heartbeat();
    assert!(
        !outbound.stop.load(Ordering::SeqCst),
        "must not disconnect the only preferred download peer"
    );
    assert!(outbound.is_sync_started());

    // An addnode / --connect peer is `manual`. Core counts it as a
    // preferred download peer, so the stalled sync peer can go.
    let _manual = hub.register(b, b, &ver("/rbitcoin:0.1.0/"), false, PeerConnType::Manual);
    hub.on_session_heartbeat();
    assert!(
        outbound.stop.load(Ordering::SeqCst),
        "a live manual peer must let the stalled headers-sync peer be replaced"
    );
}

#[test]
fn noban_headers_timeout_clears_awaiting_so_a_new_getheaders_can_send() {
    pin_noban_headers_timeout_keep(true);
    pin_noban_headers_timeout_keep(false);
}

fn pin_noban_headers_timeout_keep(via_cidr: bool) {
    let hub = PeerHub::new();
    if via_cidr {
        let mut t = rbitcoin_net::NetPermTable::default();
        t.whitelist
            .push(rbitcoin_net::parse_whitelist("noban@127.0.0.1").unwrap());
        hub.set_net_perms(t);
    } else {
        hub.set_noban(true);
    }
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
    let inbound = hub.register(a, a, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    let _outbound = hub.register(
        b,
        b,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    if via_cidr {
        assert!(!hub.is_noban(), "CIDR noban must not set hub --trusted");
        assert!(inbound.session_noban());
    }
    let now = 1_700_000_000u64;
    let best = 1_231_006_505u64;
    assert!(hub.try_start_headers_sync(&inbound, now, best));
    inbound.note_awaiting_headers();
    assert!(inbound.is_awaiting_headers());
    let deadline = rbitcoin_net::chain::headers_download_timeout_secs(now, best);
    hub.set_mock_now(deadline + 1);
    assert!(
        !inbound.stop.load(Ordering::SeqCst),
        "noban stall must keep the TCP session (via_cidr={via_cidr})"
    );
    assert!(
        !inbound.is_sync_started(),
        "noban timeout must end the stalling sync (via_cidr={via_cidr})"
    );
    assert!(
        !inbound.is_awaiting_headers(),
        "in-flight getheaders must be released so a replacement can send"
    );
    assert!(
        hub.try_start_headers_sync(&inbound, deadline + 1, best),
        "after timeout another getheaders start must be allowed"
    );
}

#[test]
fn snapshot_permissions_are_cidr_table_not_hub_noban() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445);
    hub.register(
        a,
        bind,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    hub.set_noban(true);
    assert!(
        hub.snapshot()[0].permissions.is_empty(),
        "hub --trusted is a DoS bypass, not getpeerinfo.permissions"
    );
    let mut t = rbitcoin_net::NetPermTable::default();
    t.whitelist
        .push(rbitcoin_net::parse_whitelist("noban,out@127.0.0.1").unwrap());
    hub.set_net_perms(t);
    assert_eq!(hub.snapshot()[0].permissions, ["noban", "download"]);
}

#[test]
fn cidr_noban_is_per_peer_not_hub_wide() {
    let hub = PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445);
    let local = hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444),
        bind,
        &ver("/rbitcoin:0.1.0/"),
        true,
        PeerConnType::Inbound,
    );
    let other = hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 18444),
        bind,
        &ver("/rbitcoin:0.1.0/"),
        true,
        PeerConnType::Inbound,
    );
    let mut t = rbitcoin_net::NetPermTable::default();
    t.whitelist
        .push(rbitcoin_net::parse_whitelist("noban@127.0.0.1").unwrap());
    hub.set_net_perms(t);
    assert!(!hub.is_noban(), "a CIDR grant must not set hub --trusted");
    assert!(local.session_noban());
    assert!(
        !other.session_noban(),
        "non-matching inbound must not inherit CIDR noban"
    );
    hub.set_noban(true);
    assert!(
        other.session_noban(),
        "operator --trusted still covers every session"
    );
}

#[test]
fn trust_peer_addnode_refused() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    let trusted: SocketAddr = "192.0.2.1:8333".parse().unwrap();
    hub.set_trust_peer(Some(rbitcoin_net::NetAddr::Ip(trusted)), None);
    let other: SocketAddr = "192.0.2.2:8333".parse().unwrap();
    let err = hub.addnode(other, "add").unwrap_err();
    assert!(err.contains("trust-peer"), "{err}");
    assert!(err.contains("192.0.2.1:8333"), "{err}");
    assert!(take_dials(&mut rx).is_empty());
    hub.addnode(trusted, "add").unwrap();
    assert_eq!(take_dials(&mut rx).len(), 1);
    let err = hub
        .addconnection(other, PeerConnType::OutboundFullRelay)
        .unwrap_err();
    assert!(err.contains("192.0.2.1:8333"), "{err}");
    let err = hub
        .addnode_host("192.0.2.9:8333", "onetry", 8333)
        .unwrap_err();
    assert!(err.contains("trust-peer"), "{err}");
    hub.addnode_host("192.0.2.1:8333", "onetry", 8333).unwrap();
    hub.set_trust_peer(None, Some("seed.example:8333".into()));
    let err = hub.addnode(other, "remove").unwrap_err();
    assert!(err.contains("seed.example:8333"), "{err}");
    hub.addnode_host("seed.example:8333", "add", 8333).unwrap();
    let err = hub.addnode_host("other.example", "add", 8333).unwrap_err();
    assert!(err.contains("seed.example:8333"), "{err}");
}

/// A hostname trust peer allows the socket it resolves to, and no other socket.
#[test]
fn trust_peer_resolved_host_is_dialable() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_trust_peer(None, Some("localhost:8333".into()));
    hub.set_connect_hosts(vec!["localhost:8333".into()], 8333);
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "resolved trust host must dial: {got:?}");
    assert!(
        matches!(got[0].target, DialTarget::Socket(addr) if addr.ip().is_loopback() && addr.port() == 8333),
        "{got:?}"
    );
    let other: SocketAddr = "192.0.2.9:8333".parse().unwrap();
    let err = hub
        .dial_manual_target(DialTarget::Socket(other))
        .unwrap_err();
    assert!(err.contains("localhost:8333"), "{err}");
    assert!(take_dials(&mut rx).is_empty());
}

/// Relay on: the trusted session is not a BIP152 high-bandwidth peer.
/// The same helper still selects a session when the mode is off.
#[test]
fn trust_peer_skips_high_bandwidth() {
    use rbitcoin_net::chain::AcceptOutcome;
    use rbitcoin_net::peer::{maybe_select_hb_if_relay, note_accepted_hb};

    let (_dir, chain) = rbitcoin_net::chain::tiny_regtest_hub_labeled("trust-hb");
    let addr: SocketAddr = "1.2.3.4:8333".parse().unwrap();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let join = |hub: &Arc<PeerHub>| {
        hub.register(
            addr,
            bind,
            &ver("/rbitcoin:0.1.0/"),
            false,
            PeerConnType::OutboundFullRelay,
        )
    };

    let trusted_hub = PeerHub::new();
    trusted_hub.set_trust_peer(Some(rbitcoin_net::NetAddr::Ip(addr)), None);
    let trusted = join(&trusted_hub);
    maybe_select_hb_if_relay(&chain, Some(&trusted));
    note_accepted_hb(
        &Ok(AcceptOutcome::Accepted { height: 1 }),
        &Some((Arc::clone(&trusted_hub), trusted.id)),
    );
    assert!(
        !trusted.hb_to.load(Ordering::Relaxed),
        "trusted session stays low-bandwidth"
    );
    assert!(!trusted_hub
        .hb_selected
        .lock()
        .unwrap()
        .contains(&trusted.id));

    let open = PeerHub::new();
    let peer = join(&open);
    maybe_select_hb_if_relay(&chain, Some(&peer));
    assert!(
        peer.hb_to.load(Ordering::Relaxed),
        "flag off still selects high-bandwidth"
    );
}

/// A fresh tip with transaction relay still off has an empty pool and is
/// not receiving the transactions a compact block would need. Selection
/// waits for that relay bit. `-blocksonly` never selects.
#[test]
fn hb_selects_when_tx_relay_starts() {
    use rbitcoin_net::peer::maybe_select_hb_if_relay;

    let (dir, chain) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hb-before-relay");
    let mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir.path().join("mp"),
        std::sync::Arc::clone(&chain.query),
    )
    .unwrap();
    assert!(chain.attach_mempool(std::sync::Arc::clone(&mp)).is_ok());
    assert!(chain.in_ibd(), "regtest genesis is older than 24h");
    assert!(!mp.relay_enabled());

    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let join = |hub: &Arc<PeerHub>, addr: SocketAddr| {
        hub.register(
            addr,
            bind,
            &ver("/rbitcoin:0.1.0/"),
            true,
            PeerConnType::Inbound,
        )
    };
    let during = PeerHub::new();
    let ibd_peer = join(&during, "1.2.3.4:8333".parse().unwrap());
    maybe_select_hb_if_relay(&chain, Some(&ibd_peer));
    assert!(
        !ibd_peer.hb_to.load(Ordering::Relaxed),
        "IBD must not select a high-bandwidth peer"
    );

    chain.finished_ibd.store(true, Ordering::Release);
    assert!(!chain.in_ibd());
    assert!(
        !mp.relay_enabled(),
        "peer tx relay stays off through the pack"
    );
    maybe_select_hb_if_relay(&chain, Some(&ibd_peer));
    assert!(
        !ibd_peer.hb_to.load(Ordering::Relaxed),
        "a fresh tip does not select high-bandwidth while tx relay is off"
    );
    mp.set_relay_enabled(true);
    maybe_select_hb_if_relay(&chain, Some(&ibd_peer));
    assert!(
        ibd_peer.hb_to.load(Ordering::Relaxed),
        "tx relay selects a high-bandwidth peer"
    );

    chain.set_blocks_only(true);
    let quiet = PeerHub::new();
    let blocked = join(&quiet, "1.2.3.5:8333".parse().unwrap());
    maybe_select_hb_if_relay(&chain, Some(&blocked));
    assert!(
        !blocked.hb_to.load(Ordering::Relaxed),
        "blocksonly must not select a high-bandwidth peer"
    );
}

/// Compact getdata follows transaction relay. A hub with no mempool already
/// asks during IBD. `-blocksonly` never does.
#[test]
fn compact_getdata_follows_tx_relay() {
    use rbitcoin_net::peer::getdata_use_compact;

    let (dir, chain) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cmpct-getdata");
    let mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir.path().join("mp"),
        std::sync::Arc::clone(&chain.query),
    )
    .unwrap();
    assert!(chain.attach_mempool(std::sync::Arc::clone(&mp)).is_ok());
    assert!(chain.in_ibd());
    assert!(!mp.relay_enabled());
    assert!(!getdata_use_compact(&chain, 2), "IBD asks for a full block");
    assert!(!getdata_use_compact(&chain, 1));

    chain.finished_ibd.store(true, Ordering::Release);
    assert!(!chain.in_ibd());
    assert!(
        !getdata_use_compact(&chain, 2),
        "a fresh tip asks for a full block while tx relay is off"
    );
    mp.set_relay_enabled(true);
    assert!(
        getdata_use_compact(&chain, 2),
        "tx relay asks for a compact block"
    );
    assert!(
        !getdata_use_compact(&chain, 1),
        "compact v1 is not a getdata request"
    );
    chain.set_blocks_only(true);
    assert!(
        !getdata_use_compact(&chain, 2),
        "blocksonly keeps a full-block getdata"
    );

    let (_bare_dir, bare) = rbitcoin_net::chain::tiny_regtest_hub_labeled("cmpct-getdata-bare");
    assert!(bare.in_ibd());
    assert!(
        getdata_use_compact(&bare, 2),
        "a hub with no mempool already uses compact getdata"
    );
}

/// After the scripthash pack the index is in tip mode. The block that
/// leaves IBD turns transaction relay on, sends the fee filter, and selects
/// the peer. During the pack (direct mode) that block does neither. A stale
/// accept and `-blocksonly` do not select.
#[tokio::test]
async fn leaving_ibd_starts_tx_relay_and_high_bandwidth_in_tip_mode() {
    use rbitcoin_net::chain::AcceptOutcome;
    use rbitcoin_net::peer::accept_received_from_peer;

    let (dir, chain) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hb-leaving");
    chain.ensure_genesis().unwrap();
    let mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir.path().join("mp"),
        std::sync::Arc::clone(&chain.query),
    )
    .unwrap();
    assert!(chain.attach_mempool(std::sync::Arc::clone(&mp)).is_ok());
    assert!(chain.in_ibd());
    assert!(!mp.relay_enabled());

    let peers = PeerHub::new();
    let peer = peers.register(
        "1.2.3.4:8333".parse().unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    peer.attach_out(out_tx);
    let now = chain.clock.now_secs() as u32;
    let block = rbitcoin_consensus::mine_empty_regtest(chain.tip_hash().unwrap(), now, 1);
    let got = accept_received_from_peer(&chain, block, Some(&peer))
        .await
        .expect("fresh tip connects");
    assert!(matches!(got, AcceptOutcome::Accepted { .. }));
    assert!(!chain.in_ibd());
    assert!(
        mp.relay_enabled(),
        "a tip-mode block that leaves IBD enables tx relay"
    );
    assert!(
        peer.hb_to.load(Ordering::Relaxed),
        "that peer is high-bandwidth once relay is on"
    );
    assert!(
        mp.take_fee_backfill_due(),
        "the relay edge owes fee-history preload"
    );
    let mut saw_fee = false;
    while let Ok(out) = out_rx.try_recv() {
        if matches!(out.expect_msg(), NetworkMessage::FeeFilter(_)) {
            saw_fee = true;
        }
    }
    assert!(saw_fee, "the relay edge sends one fee filter");

    let (dir_direct, direct) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hb-leaving-direct");
    direct.ensure_genesis().unwrap();
    direct
        .query
        .enter_direct_index_mode()
        .expect("direct mode is the pack");
    let direct_mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir_direct.path().join("mp"),
        std::sync::Arc::clone(&direct.query),
    )
    .unwrap();
    assert!(direct
        .attach_mempool(std::sync::Arc::clone(&direct_mp))
        .is_ok());
    let direct_peers = PeerHub::new();
    let direct_peer = direct_peers.register(
        "1.2.3.7:8333".parse().unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let direct_now = direct.clock.now_secs() as u32;
    let direct_block =
        rbitcoin_consensus::mine_empty_regtest(direct.tip_hash().unwrap(), direct_now, 1);
    accept_received_from_peer(&direct, direct_block, Some(&direct_peer))
        .await
        .expect("pack-window tip connects");
    assert!(!direct.in_ibd());
    assert!(
        !direct_mp.relay_enabled(),
        "the pack does not enable tx relay"
    );
    assert!(!direct_peer.hb_to.load(Ordering::Relaxed));
    assert!(!direct_mp.take_fee_backfill_due());

    let (dir_old, old) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hb-leaving-stale");
    old.ensure_genesis().unwrap();
    let old_mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir_old.path().join("mp"),
        std::sync::Arc::clone(&old.query),
    )
    .unwrap();
    assert!(old.attach_mempool(std::sync::Arc::clone(&old_mp)).is_ok());
    let old_peers = PeerHub::new();
    let old_peer = old_peers.register(
        "1.2.3.5:8333".parse().unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let stale = old.clock.now_secs() as u32 - 2 * 24 * 3600;
    let stale_block = rbitcoin_consensus::mine_empty_regtest(old.tip_hash().unwrap(), stale, 1);
    accept_received_from_peer(&old, stale_block, Some(&old_peer))
        .await
        .expect("stale tip connects");
    assert!(old.in_ibd());
    assert!(!old_peer.hb_to.load(Ordering::Relaxed));
    assert!(!old_mp.relay_enabled());

    let (dir_bo, bo) = rbitcoin_net::chain::tiny_regtest_hub_labeled("hb-leaving-bo");
    bo.ensure_genesis().unwrap();
    bo.set_blocks_only(true);
    let bo_mp = rbitcoin_net::tx_relay::MempoolHub::open(
        dir_bo.path().join("mp"),
        std::sync::Arc::clone(&bo.query),
    )
    .unwrap();
    assert!(bo.attach_mempool(std::sync::Arc::clone(&bo_mp)).is_ok());
    let bo_peers = PeerHub::new();
    let bo_peer = bo_peers.register(
        "1.2.3.6:8333".parse().unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let bo_now = bo.clock.now_secs() as u32;
    let bo_block = rbitcoin_consensus::mine_empty_regtest(bo.tip_hash().unwrap(), bo_now, 1);
    accept_received_from_peer(&bo, bo_block, Some(&bo_peer))
        .await
        .expect("blocksonly tip connects");
    assert!(!bo.in_ibd());
    assert!(
        !bo_peer.hb_to.load(Ordering::Relaxed),
        "blocksonly does not select a high-bandwidth peer"
    );
    assert!(!bo_mp.relay_enabled());
    assert!(!bo_mp.pool_maintained());
}

fn hold_session(hub: &Arc<PeerHub>, addr: SocketAddr) -> Arc<LivePeer> {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    hub.register(
        addr,
        bind,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    )
}

fn dial_socket(req: &DialRequest) -> SocketAddr {
    match req.target {
        DialTarget::Socket(addr) => addr,
        DialTarget::Domain { ref host, port } => panic!("expected a socket, got {host}:{port}"),
    }
}

/// `--trust-peer seed.example` keeps one socket. A later answer disconnects
/// the previous one and `addnode` of that socket fails.
#[test]
fn trust_peer_resolve_replaces_socket() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_trust_peer(None, Some("seed.example:8333".into()));
    hub.set_connect_hosts(vec!["seed.example:8333".into()], 8333);
    let first: SocketAddr = "1.2.3.4:8333".parse().unwrap();
    let second: SocketAddr = "5.6.7.8:8333".parse().unwrap();
    assert!(
        !rbitcoin_net::netaddr::clearnet_unroutable(second.ip()),
        "replacement address must not be an unroutable-gossip address"
    );
    let generation = std::cell::Cell::new(0u8);
    let resolve = |host: &str| {
        assert!(host.contains("seed.example"), "resolver saw {host}");
        let addr = if generation.get() == 0 { first } else { second };
        Ok(DialTarget::Socket(addr))
    };
    hub.redial_remembered_with(resolve);
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "first resolution dials once: {got:?}");
    assert_eq!(dial_socket(&got[0]), first);

    let old = hold_session(&hub, first);
    generation.set(1);
    hub.redial_remembered_with(resolve);
    assert!(
        hub.get(old.id)
            .is_some_and(|p| p.stop.load(Ordering::Relaxed)),
        "previous trust socket is disconnecting; the row stays until its session returns"
    );
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "new socket is dialed: {got:?}");
    assert_eq!(dial_socket(&got[0]), second);
    let err = hub.addnode(first, "add").unwrap_err();
    assert!(err.contains("seed.example"), "{err}");

    let live = hold_session(&hub, second);
    hub.redial_remembered_with(resolve);
    assert!(hub.get(live.id).is_some(), "same socket stays up");
    assert!(
        take_dials(&mut rx).is_empty(),
        "a live resolution opens no second session"
    );
}

/// `--connect` hostnames replace one socket each. A shared socket stays
/// while any current name still uses it. An IP literal is not re-resolved.
#[test]
fn connect_resolve_replaces_socket() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_connect_hosts(vec!["a.example:8333".into(), "b.example:8333".into()], 8333);
    let literal: SocketAddr = "8.8.8.8:8333".parse().unwrap();
    hub.set_connect_addrs(vec![rbitcoin_net::NetAddr::Ip(literal)]);
    let shared: SocketAddr = "1.2.3.4:8333".parse().unwrap();
    let moved: SocketAddr = "5.6.7.8:8333".parse().unwrap();
    let phase = std::cell::Cell::new(0u8);
    let asked = std::cell::RefCell::new(Vec::new());
    let resolve = |host: &str| {
        asked.borrow_mut().push(host.to_string());
        let addr = if host.starts_with("a.") {
            if phase.get() == 0 {
                shared
            } else {
                moved
            }
        } else if host.starts_with("b.") {
            if phase.get() < 2 {
                shared
            } else {
                moved
            }
        } else {
            panic!("resolved an address that is not a connect hostname: {host}");
        };
        Ok(DialTarget::Socket(addr))
    };
    hub.redial_remembered_with(resolve);
    assert!(
        asked.borrow().iter().all(|h| !h.contains("8.8.8.8")),
        "an IP literal is not re-resolved: {:?}",
        asked.borrow()
    );
    let got = take_dials(&mut rx);
    let ips: Vec<SocketAddr> = got.iter().map(dial_socket).collect();
    assert!(ips.contains(&shared), "{ips:?}");
    assert!(ips.contains(&literal), "{ips:?}");

    let shared_peer = hold_session(&hub, shared);
    let literal_peer = hold_session(&hub, literal);
    phase.set(1);
    hub.redial_remembered_with(resolve);
    assert!(
        hub.get(shared_peer.id).is_some(),
        "a socket still used by b.example stays connected"
    );
    assert!(
        hub.get(literal_peer.id).is_some(),
        "the other connect peer stays"
    );
    let got = take_dials(&mut rx);
    assert!(
        got.iter().any(|d| dial_socket(d) == moved),
        "a.example dials its new socket: {got:?}"
    );

    let moved_peer = hold_session(&hub, moved);
    phase.set(2);
    hub.redial_remembered_with(resolve);
    assert!(
        hub.get(shared_peer.id)
            .is_some_and(|p| p.stop.load(Ordering::Relaxed)),
        "the shared socket is disconnecting once neither name still uses it"
    );
    assert!(hub.get(literal_peer.id).is_some());
    assert!(
        hub.get(moved_peer.id).is_some(),
        "resolving to the live socket opens no second session"
    );
    assert!(
        take_dials(&mut rx).iter().all(|d| dial_socket(d) != moved),
        "no second dial to the live socket"
    );
}

#[test]
fn addnode_unknown_command() {
    let hub = PeerHub::new();
    let (tx, _rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let err = hub.addnode(a, "nope").unwrap_err();
    assert!(err.contains("unknown addnode command"), "{err}");
}

#[test]
fn addnode_net_without_a_dialer_errors() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    for cmd in ["add", "onetry", "remove"] {
        let err = hub.addnode(a, cmd).unwrap_err();
        assert!(err.contains("no dialer"), "{cmd}: {err}");
    }
    assert!(
        hub.added
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "a refused add must not be remembered"
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.redial_remembered();
    assert!(
        take_dials(&mut rx).is_empty(),
        "a refused add must not stay on the redial list"
    );
}

fn take_dials(rx: &mut mpsc::UnboundedReceiver<DialRequest>) -> Vec<DialRequest> {
    let mut out = Vec::new();
    while let Ok(req) = rx.try_recv() {
        out.push(req);
    }
    out
}

#[test]
fn parse_peer_addr_localhost_and_default_port() {
    let with_port = parse_peer_addr_with_port("localhost:18444", None).expect("localhost:port");
    assert!(with_port.is_ipv4());
    assert_eq!(with_port.port(), 18444);
    let bare = parse_peer_addr_with_port("localhost", Some(18444)).expect("localhost default");
    assert!(bare.is_ipv4());
    assert_eq!(bare.port(), 18444);
    let lit = "127.0.0.1:18444".parse::<SocketAddr>().unwrap();
    assert_eq!(
        parse_peer_addr_with_port("127.0.0.1:18444", None).unwrap(),
        lit
    );
    assert!(parse_peer_addr_with_port("not-a-real-host.invalid", Some(18444)).is_err());
}

#[test]
fn addnode_add_keeps_unresolved_host_for_redial() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.addnode_host("not-a-real-host.invalid", "add", 18444)
        .expect("unresolved add is remembered");
    assert!(take_dials(&mut rx).is_empty());
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9);
    hub.redial_remembered_with(|_| Ok(DialTarget::Socket(addr)));
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "later resolve must dial: {got:?}");
    assert!(matches!(got[0].target, DialTarget::Socket(a) if a == addr));
    assert!(hub
        .addnode_host("not-a-real-host.invalid", "onetry", 18444)
        .is_err());
}

#[test]
fn redial_same_endpoint_in_addnode_and_connect_dials_once() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.addnode_host("127.0.0.1:18444", "add", 18444).unwrap();
    assert_eq!(take_dials(&mut rx).len(), 1);
    hub.set_connect_hosts(vec!["127.0.0.1:18444".into()], 18444);
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(
        got.len(),
        1,
        "addnode and --connect of one endpoint share one dial per pass: {got:?}"
    );
    assert!(matches!(got[0].typ, PeerConnType::Manual));

    // A --connect host on its own redials as `manual` too (Core `-connect`).
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_connect_hosts(vec!["127.0.0.1:18445".into()], 18444);
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(matches!(got[0].typ, PeerConnType::Manual), "{got:?}");
    // A queued request counts as in flight; dropping it ends that dial.
    drop(got);

    // A --connect address (IP or overlay) redials in the same pass. One
    // that meets a hostname at an endpoint shares its dial, and a live
    // session is not dialled again.
    let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445);
    let onion: rbitcoin_net::NetAddr =
        "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:8333"
            .parse()
            .unwrap();
    hub.set_connect_addrs(vec![rbitcoin_net::NetAddr::Ip(ip), onion]);
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|d| d.typ == PeerConnType::Manual), "{got:?}");
    assert_eq!(got[0].target, DialTarget::Socket(ip), "{got:?}");
    assert_eq!(got[1].target, DialTarget::from_net(onion), "{got:?}");
    drop(got);
    let _live = hub.register(
        ip,
        ip,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::Manual,
    );
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "only the onion has no live session: {got:?}");
    assert_eq!(got[0].target, DialTarget::from_net(onion), "{got:?}");
}

#[test]
fn addnode_skips_an_endpoint_that_is_connected_or_dialling() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18446);
    let live = hub.register(
        ip,
        ip,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    // Core `AlreadyConnectedToHost` / `AlreadyConnectedToAddressPort`.
    hub.addnode_host("127.0.0.1:18446", "onetry", 18444)
        .unwrap();
    hub.addnode_host("127.0.0.1:18446", "add", 18444).unwrap();
    hub.addnode(ip, "onetry").unwrap();
    let got = take_dials(&mut rx);
    assert!(
        got.is_empty(),
        "a connected endpoint is not dialled again: {got:?}"
    );

    let onion: rbitcoin_net::NetAddr =
        "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:8333"
            .parse()
            .unwrap();
    let dialing = hub.dial_in_flight(&DialTarget::from_net(onion));
    hub.addnode_host(&onion.to_string(), "onetry", 18444)
        .unwrap();
    let got = take_dials(&mut rx);
    assert!(got.is_empty(), "nor is one still dialling: {got:?}");
    drop(dialing);
    // A dial counts from the moment it is queued, before the dial task
    // picks it up: `onetry`, `add` and a redial pass right behind it
    // queue nothing more.
    hub.addnode_host(&onion.to_string(), "onetry", 18444)
        .unwrap();
    hub.addnode_host(&onion.to_string(), "add", 18444).unwrap();
    hub.redial_remembered();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "a queued dial is not queued again: {got:?}");
    assert_eq!(got[0].target, DialTarget::from_net(onion), "{got:?}");

    live.request_disconnect();
    hub.addnode_host("127.0.0.1:18446", "onetry", 18444)
        .unwrap();
    let got = take_dials(&mut rx);
    assert_eq!(got.len(), 1, "once the session ends, onetry dials: {got:?}");
    assert_eq!(got[0].target, DialTarget::Socket(ip), "{got:?}");
    assert_eq!(got[0].typ, PeerConnType::Manual, "{got:?}");
}

#[test]
fn a_queued_feeler_does_not_hold_off_a_manual_dial() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18447);
    // A feeler's probe has no connect deadline. Counted in flight, a
    // stuck one would keep `addnode` and the redial pass off this
    // endpoint until it ended.
    hub.addconnection(ip, PeerConnType::Feeler).unwrap();
    let feeler = take_dials(&mut rx);
    assert_eq!(feeler.len(), 1, "{feeler:?}");
    assert_eq!(feeler[0].typ, PeerConnType::Feeler, "{feeler:?}");
    hub.addnode(ip, "onetry").unwrap();
    let got = take_dials(&mut rx);
    assert_eq!(
        got.len(),
        1,
        "a queued feeler does not block addnode: {got:?}"
    );
    assert_eq!(got[0].typ, PeerConnType::Manual, "{got:?}");
    drop(feeler);
}

/// A private-broadcast type would run the bland VERSION and then a full
/// peer session, so `addconnection` refuses it like `Inbound`.
#[test]
fn addconnection_refuses_types_it_cannot_open() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18448);
    for typ in [PeerConnType::Inbound, PeerConnType::PrivateBroadcast] {
        let err = hub.addconnection(ip, typ).unwrap_err();
        assert!(err.contains(typ.as_str()), "{typ:?}: {err}");
    }
    let got = take_dials(&mut rx);
    assert!(got.is_empty(), "a refused type was dialed: {got:?}");
}

#[tokio::test]
async fn slow_redial_resolve_does_not_stall_runtime() {
    let hub = PeerHub::new();
    hub.set_connect_hosts(vec!["slow.example".into()], 18444);
    let flag = Arc::new(AtomicBool::new(false));
    let flag2 = Arc::clone(&flag);
    let progress = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        flag2.store(true, Ordering::SeqCst);
    });
    let hub2 = Arc::clone(&hub);
    let slow = tokio::task::spawn_blocking(move || {
        hub2.redial_remembered_with(|_| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            Err("slow".into())
        });
    });
    progress.await.unwrap();
    assert!(
        flag.load(Ordering::SeqCst),
        "a blocking resolve must not stall other Tokio tasks"
    );
    slow.await.unwrap();
}

/// Core's block download window (`BlockRequested`, `RemoveBlockRequest`,
/// the `SendMessages` timeout): one block interval plus half of one per
/// other downloading peer, restarted when the oldest block in flight
/// leaves, ended when none is left. Our 10 s re-ask of a block already in
/// flight keeps its place and does not restart the window.
#[test]
fn block_download_window_follows_the_oldest_block_in_flight() {
    const INTERVAL: u64 = 600;
    let hub = PeerHub::new();
    let t = 1_700_000_000;
    hub.set_mock_now(t);
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18460);
    let p = hub.register(a, a, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    let [x, y, z] = [1u8, 2, 3].map(|b| BlockHash::from_byte_array([b; 32]));

    p.note_blocks_requested(&[x, y]);
    assert_eq!(hub.peers_downloading_from.load(Ordering::Relaxed), 1);
    assert_eq!(p.block_download_timed_out(t + INTERVAL, INTERVAL), None);
    assert_eq!(
        p.block_download_timed_out(t + INTERVAL + 1, INTERVAL),
        Some(x)
    );

    hub.set_mock_now(t + 300);
    p.note_blocks_requested(&[x, z]);
    p.sync_block_download(t + 300, |h| [x, y, z].contains(h));
    assert_eq!(
        p.block_download_timed_out(t + INTERVAL + 1, INTERVAL),
        Some(x),
        "a re-ask of a block in flight restarted the window"
    );
    p.sync_block_download(t + 400, |h| [x, z].contains(h));
    assert_eq!(
        p.block_download_timed_out(t + INTERVAL + 1, INTERVAL),
        Some(x),
        "a newer block leaving restarted the window"
    );
    p.sync_block_download(t + 500, |h| *h == z);
    assert_eq!(
        p.block_download_timed_out(t + 500 + INTERVAL, INTERVAL),
        None
    );
    assert_eq!(
        p.block_download_timed_out(t + 500 + INTERVAL + 1, INTERVAL),
        Some(z),
        "the oldest block leaving restarts the window at that time"
    );

    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18461);
    let q = hub.register(b, b, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    q.note_blocks_requested(&[x]);
    assert_eq!(hub.peers_downloading_from.load(Ordering::Relaxed), 2);
    assert_eq!(
        p.block_download_timed_out(t + 500 + INTERVAL * 3 / 2, INTERVAL),
        None,
        "each other downloading peer adds half an interval"
    );
    assert_eq!(
        p.block_download_timed_out(t + 500 + INTERVAL * 3 / 2 + 1, INTERVAL),
        Some(z)
    );

    p.sync_block_download(t + 600, |_| false);
    assert_eq!(hub.peers_downloading_from.load(Ordering::Relaxed), 1);
    assert_eq!(
        p.block_download_timed_out(t + 100 * INTERVAL, INTERVAL),
        None
    );
    hub.unregister(q.id);
    assert_eq!(
        hub.peers_downloading_from.load(Ordering::Relaxed),
        0,
        "a dropped session still counted as downloading"
    );
}

/// Core runs the ping timeout only once `-peertimeout` has passed since
/// connect. Core's functional framework sets `peertimeout=999999999` so a
/// `setmocktime` jump cannot drop peers (`feature_bip68_sequence.py`).
#[test]
fn ping_timeout_waits_for_the_peer_timeout() {
    let hub = PeerHub::new();
    hub.set_mock_now(1_700_000_000);
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    let p = hub.register(a, a, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    let now = hub.now_secs();
    assert!(p.take_ping_send(now).is_some(), "expected send");
    hub.set_peer_timeout_secs(999_999_999);
    assert!(
        p.ping_timeout(now + 6_000).is_none(),
        "inside the peer timeout the ping does not time out"
    );
    hub.set_peer_timeout_secs(60);
    assert!(
        p.ping_timeout(now + 20 * 60).is_none(),
        "the ping timeout starts the second after twenty minutes"
    );
    assert!(p.ping_timeout(now + 20 * 60 + 1).is_some());

    let late = hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18445),
        &ver("/rbitcoin:0.1.0/"),
        true,
        PeerConnType::Inbound,
    );
    assert!(late.take_ping_send(now).is_some(), "expected send");
    hub.set_peer_timeout_secs(2_000);
    assert!(
        late.ping_timeout(now + 2_000).is_none(),
        "inactivity checks start the second after the peer timeout"
    );
    assert!(late.ping_timeout(now + 2_001).is_some());
}

#[test]
fn pick_stale_follow_evict_none_on_empty() {
    assert!(pick_stale_follow_evict(&[], 7, &[]).is_none());
}

#[test]
fn pick_stale_follow_evict_picks_only_from_candidates() {
    let ids = [3u64, 9, 12];
    let groups = [1u64, 2, 3];
    for salt in 0..16u64 {
        let got = pick_stale_follow_evict(&ids, salt, &groups).unwrap();
        assert!(ids.contains(&got), "salt={salt} got={got}");
    }
    assert_eq!(pick_stale_follow_evict(&ids, 0, &groups), Some(3));
    assert_eq!(pick_stale_follow_evict(&ids, 1, &groups), Some(9));
    assert_eq!(pick_stale_follow_evict(&ids, 2, &groups), Some(12));
    assert_eq!(pick_stale_follow_evict(&ids, 3, &groups), Some(3));
}

#[test]
fn pick_stale_follow_evict_prefers_duplicate_group() {
    let ids = [1u64, 2, 3];
    let groups = [10u64, 10, 20];
    assert_eq!(pick_stale_follow_evict(&ids, 0, &groups), Some(1));
    assert_eq!(pick_stale_follow_evict(&ids, 1, &groups), Some(2));
    for salt in 0..16u64 {
        let got = pick_stale_follow_evict(&ids, salt, &groups).unwrap();
        assert!(got == 1 || got == 2, "salt={salt} got={got}");
    }
}

/// An outbound onion or I2P session keys `addr` as `0.0.0.0:<port>`. Stale
/// rotation groups by net, so two overlay peers are not one duplicate group
/// that it evicts ahead of every clearnet peer (#1511).
#[test]
fn stale_follow_groups_overlay_peers_apart() {
    use rbitcoin_net::{netgroup_net, NetAddr};
    let hub = PeerHub::new();
    let placeholder: SocketAddr = "0.0.0.0:8333".parse().unwrap();
    let mut live = Vec::new();
    for onion in [
        "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:8333",
        "vww6ybal4bd7szmgncyruucpgfkqahzddi37ktceo3ah7ngmcopnpyyd.onion:8333",
    ] {
        let net: NetAddr = onion.parse().unwrap();
        live.push(hub.register_net(
            placeholder,
            net,
            placeholder,
            &ver("/rbitcoin:0.1.0/"),
            false,
            PeerConnType::OutboundFullRelay,
        ));
    }
    let clear: SocketAddr = "45.1.0.1:8333".parse().unwrap();
    let clear = hub.register(
        clear,
        clear,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let rows = hub.outbound_full_relay_rows();
    let ids: Vec<u64> = rows.iter().map(|(id, _)| *id).collect();
    let groups: Vec<u64> = rows
        .iter()
        .map(|(_, net)| netgroup_net(*net, None))
        .collect();
    let picked: HashSet<u64> = (0..16u64)
        .filter_map(|salt| pick_stale_follow_evict(&ids, salt, &groups))
        .collect();
    assert!(
        picked.contains(&clear.id),
        "two onion peers must not share one group: groups={groups:?}"
    );
}

#[test]
fn outbound_full_relay_ids_skips_inbound_and_noban() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
    let c = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3);
    let _in = hub.register(a, a, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    let out = hub.register(
        b,
        b,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let _br = hub.register(
        c,
        c,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::BlockRelay,
    );
    let ids = hub.outbound_full_relay_ids();
    assert_eq!(ids, vec![out.id]);
    hub.set_noban(true);
    assert!(
        hub.outbound_full_relay_ids().is_empty(),
        "noban hub must not offer rotate victims"
    );
}

/// Core's `GetFullOutboundConnCount` counts every outbound full-relay
/// session. Catch-up sessions are not hub rows, so catch-up reports its
/// count and the hub adds it. Inbound, block-relay, and `noban` do not
/// change that sum.
#[test]
fn full_outbound_count_adds_catch_up_sessions() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
    let c = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3);
    assert_eq!(hub.full_outbound_count(), 0);
    let _in = hub.register(a, a, &ver("/rbitcoin:0.1.0/"), true, PeerConnType::Inbound);
    let _out = hub.register(
        b,
        b,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let _br = hub.register(
        c,
        c,
        &ver("/rbitcoin:0.1.0/"),
        false,
        PeerConnType::BlockRelay,
    );
    // Core `GetFullOutboundConnCount` counts only `fSuccessfullyConnected`.
    let d = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4);
    let _connecting = hub.register_connecting_net(
        d,
        rbitcoin_net::NetAddr::from_socket(d),
        d,
        false,
        PeerConnType::OutboundFullRelay,
    );
    assert_eq!(
        hub.full_outbound_count(),
        1,
        "VERSION not done: not counted"
    );
    hub.set_catch_up_full_outbound(2);
    assert_eq!(hub.full_outbound_count(), 3);
    hub.set_noban(true);
    assert_eq!(hub.full_outbound_count(), 3, "noban sessions still count");
    hub.set_catch_up_full_outbound(0);
    assert_eq!(hub.full_outbound_count(), 1);
}

fn fill_addrman(n: usize) -> rbitcoin_net::seeds::AddrMan {
    let mut am = rbitcoin_net::seeds::AddrMan::new();
    for i in 0..n {
        let a = (i >> 8) as u8;
        let b = (i & 0xff) as u8;
        am.add_with_flags(
            SocketAddr::from(([a, b, 1, 1], 8333)),
            rbitcoin_net::seeds::PeerFlags::empty(),
        );
    }
    am
}

fn addr_ips(v: &[(u32, Address)]) -> Vec<IpAddr> {
    v.iter()
        .filter_map(|(_, a)| a.socket_addr().ok().map(|s| s.ip()))
        .collect()
}

#[test]
fn getaddr_cache_bind_key_and_ttl() {
    let hub = PeerHub::new();
    hub.set_mock_now(1_700_000_000);
    hub.set_addrman(Arc::new(Mutex::new(fill_addrman(5_000))));
    let a = addr_ips(&hub.addr_response_for_bind(SocketAddr::from(([127, 0, 0, 1], 18444))));
    let b = addr_ips(&hub.addr_response_for_bind(SocketAddr::from(([127, 0, 0, 1], 18445))));
    let c = addr_ips(&hub.addr_response_for_bind(SocketAddr::from(([127, 0, 0, 1], 18446))));
    let mapped = addr_ips(&hub.addr_response_for_bind(SocketAddr::from((
        Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped(),
        18444,
    ))));
    assert_eq!(a.len(), 1000);
    assert_eq!(b.len(), 1000);
    assert_eq!(c.len(), 1000);
    assert_eq!(
        a, mapped,
        "IPv4-mapped IPv6 must share the clearnet cache key"
    );
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    hub.set_mock_now(1_700_000_000 + 24 * 60 * 60);
    let expired = addr_ips(&hub.addr_response_for_bind(SocketAddr::from(([127, 0, 0, 1], 18444))));
    assert_eq!(expired.len(), 1000);
    assert_ne!(a, expired);
}

#[test]
fn getaddr_omits_unroutable_clearnet() {
    let hub = PeerHub::new();
    hub.set_mock_now(1_700_000_000);
    let mut am = rbitcoin_net::seeds::AddrMan::new();
    for addr in ["10.0.0.1:8333", "100.64.0.1:8333", "127.0.0.1:8333"] {
        assert!(am.add_learned(addr.parse().unwrap(), 64));
    }
    hub.set_addrman(Arc::new(Mutex::new(am)));
    let got = hub.addr_response_for_bind(SocketAddr::from(([127, 0, 0, 1], 18444)));
    assert!(
        got.is_empty(),
        "getaddr must not advertise unroutable clearnet: {got:?}"
    );
}

include!("overlay_addrman_journey.rs");

#[test]
fn external_ip_uses_the_clearnet_bogon_filter() {
    let blocked = [
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
        IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 1)),
    ];
    for ip in blocked {
        assert!(
            !ip_is_advertisable(&ip, true),
            "{ip} must not be self-announced"
        );
    }
    assert!(ip_is_advertisable(
        &IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
        false
    ));
}

/// Tx-relaying newcomers finish their handshakes at the same moment. Each
/// check evicts its own peer: none is closed while an unprotected resident
/// is left, and the tx-relay share still holds once every check returns.
/// TCP handshakes cannot finish this close together, so the checks run on
/// threads against one hub.
#[test]
fn concurrent_tx_relay_newcomers_keep_the_inbound_share() {
    const MAX_INBOUND: usize = 60;
    let share = MAX_INBOUND / 2;
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8333);
    for round in 0..300 {
        let hub = PeerHub::new();
        for i in 0..share {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, i as u8 + 1)), 8333);
            hub.register(addr, bind, &ver("/resident/"), true, PeerConnType::Inbound);
        }
        let newcomers: Vec<_> = (11..19)
            .map(|net| {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(net, 0, 0, 1)), 8333);
                hub.register(addr, bind, &ver("/newcomer/"), true, PeerConnType::Inbound)
            })
            .collect();
        let go = AtomicBool::new(false);
        let closed = std::thread::scope(|s| {
            let checks: Vec<_> = newcomers
                .iter()
                .map(|newcomer| {
                    let (hub, go) = (&hub, &go);
                    s.spawn(move || {
                        while !go.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                        let stays = hub.make_room_for_tx_relay_inbound(newcomer, MAX_INBOUND);
                        if !stays {
                            hub.disconnect_id(newcomer.id);
                        }
                        stays
                    })
                })
                .collect();
            go.store(true, Ordering::Release);
            checks
                .into_iter()
                .map(|c| c.join().unwrap())
                .filter(|stays| !stays)
                .count()
        });
        let tx_relay = hub
            .snapshot()
            .iter()
            .filter(|p| p.inbound && p.relay)
            .count();
        assert_eq!(
            closed, 0,
            "round {round}: a newcomer closed with residents to evict"
        );
        assert_eq!(
            tx_relay, share,
            "round {round}: tx-relaying inbounds after the checks"
        );
    }
}

#[test]
fn second_dial_of_the_same_target_stacks() {
    let hub = PeerHub::new();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let target = DialTarget::Socket(addr);
    let first = hub.dial_in_flight(&target);
    let second = hub.dial_in_flight(&target);
    let n = first
        .counts
        .lock()
        .unwrap()
        .get(&first.key)
        .expect("in flight")
        .n;
    assert_eq!(n, 2, "two guards on one target count two dials");
    drop((first, second));
}

#[test]
fn try_evict_inbound_disconnects_one_peer() {
    let hub = PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8333);
    for i in 0..30u8 {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(i + 1, 1, 0, 1)), 8333);
        hub.register_connecting(addr, bind, true, PeerConnType::Inbound);
    }
    let before = hub.snapshot().len();
    assert!(hub.try_evict_inbound(), "an inbound peer is evicted");
    assert_eq!(
        hub.snapshot().len(),
        before - 1,
        "eviction disconnects that peer"
    );
}

#[test]
fn tx_relay_room_ignores_outbound_and_non_relay() {
    let hub = PeerHub::new();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8333);
    let newcomer = hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)), 8333),
        bind,
        &ver("/new/"),
        true,
        PeerConnType::Inbound,
    );
    hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(2, 0, 0, 1)), 8333),
        bind,
        &ver("/out/"),
        false,
        PeerConnType::OutboundFullRelay,
    );
    let mut quiet = ver("/quiet/");
    quiet.relay = false;
    hub.register(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(3, 0, 0, 1)), 8333),
        bind,
        &quiet,
        true,
        PeerConnType::Inbound,
    );
    assert!(
        hub.make_room_for_tx_relay_inbound(&newcomer, 2),
        "outbound relay and a non-relaying inbound are outside the tx-relay cap"
    );
}

/// A `--proxy --connect name` session carries the proxy link like a proxied IP
/// does, so the peer is told the onion address we listen on.
#[test]
fn a_proxied_name_session_is_told_the_onion_we_listen_on() {
    let (hub, _rx) = proxied_hub();
    hub.set_discover(true);
    hub.set_p2p_onion(
        "24yjwaf2ni6wz44pmqpw3jyjhksoko7wxzelp6uyep2pdlgb5epbnoqd.onion".into(),
        8333,
    );
    let target = hub.host_dial_target("peer.invalid:8333").unwrap();
    let session = hub.register_connecting_net(
        target.version_socket(),
        target.net_addr(),
        target.version_socket(),
        false,
        PeerConnType::Manual,
    );
    session.set_wants_addrv2();
    match session.take_self_announce_msg() {
        Some(NetworkMessage::AddrV2(v)) => {
            assert!(
                v.iter()
                    .any(|m| matches!(m.addr, bitcoin::p2p::address::AddrV2::TorV3(_))),
                "{v:?}"
            );
        }
        other => panic!("expected the onion in an AddrV2, got {other:?}"),
    }
}

/// With 1024 names interned, a new name has no key of its own. The dial is
/// refused, not keyed `0.0.0.0:port` where it would share a key with every
/// other overflow name. Runs in its own process: the interner is global.
#[test]
fn a_full_interner_refuses_a_name_dial_instead_of_sharing_one_key() {
    const CHILD: &str = "RBTC_NAME_CAP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "peers::a_full_interner_refuses_a_name_dial_instead_of_sharing_one_key",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .expect("spawn");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }
    for i in 0..1024 {
        assert!(rbitcoin_net::NetAddr::name(&format!("fill-{i}.example"), 1).is_some());
    }
    assert!(rbitcoin_net::NetAddr::name("over.example", 8333).is_none());
    let (hub, _rx) = proxied_hub();
    let target = DialTarget::Domain {
        host: "over.example".into(),
        port: 8333,
    };
    let err = hub.dial_manual_target(target).unwrap_err();
    assert!(err.contains("too many distinct"), "{err}");
}

/// Asking about a name nobody added (a live check, `addnode remove`) must not
/// spend one of the interner's slots, or a flood of removes fills it for good.
#[test]
fn asking_about_unknown_names_leaves_the_interner_alone() {
    let hub = PeerHub::new();
    for i in 0..2000 {
        let target = DialTarget::Domain {
            host: format!("never-added-{i}.example"),
            port: 8333,
        };
        assert!(!hub.is_target_live(&target));
        assert!(!hub.disconnect_target(&target));
    }
    assert!(rbitcoin_net::NetAddr::name("added-after.example", 8333).is_some());
}

/// With `--proxy`, a `--connect` or `addnode` name goes to the proxy as a
/// domain (SOCKS address type 3). The node never asks the OS resolver, so
/// the unresolvable `.invalid` name still produces a dial.
#[test]
fn proxy_dials_connect_host_by_name() {
    let hub = PeerHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_proxies(rbitcoin_net::NetProxies {
        dialer: rbitcoin_net::Dialer::socks("127.0.0.1:9050".parse().unwrap(), true),
        i2p_sam: None,
    });
    hub.set_connect_hosts(vec!["peer.invalid:8333".into()], 8333);
    hub.redial_remembered();
    hub.addnode_host("other.invalid", "onetry", 8333).unwrap();
    let got = take_dials(&mut rx);
    let names: Vec<_> = got
        .iter()
        .map(|d| match &d.target {
            DialTarget::Domain { host, port } => format!("{host}:{port}"),
            DialTarget::Socket(a) => panic!("resolved locally to {a}"),
        })
        .collect();
    assert_eq!(names, ["peer.invalid:8333", "other.invalid:8333"]);
}

/// An IPv6 literal needs no lookup. Under `--proxy` it stays a socket target,
/// not a domain cut at its last colon.
#[test]
fn proxy_keeps_ipv6_literals_as_sockets() {
    let hub = PeerHub::new();
    hub.set_proxies(rbitcoin_net::NetProxies {
        dialer: rbitcoin_net::Dialer::socks("127.0.0.1:9050".parse().unwrap(), true),
        i2p_sam: None,
    });
    hub.note_default_port(8333);
    for literal in ["::1", "2001:db8::1"] {
        match hub.host_dial_target(literal).unwrap() {
            DialTarget::Socket(addr) => assert_eq!(addr.port(), 8333),
            DialTarget::Domain { host, .. } => panic!("{literal} became domain {host}"),
        }
    }
}

fn proxied_hub() -> (
    std::sync::Arc<PeerHub>,
    mpsc::UnboundedReceiver<DialRequest>,
) {
    let hub = PeerHub::new();
    let (tx, rx) = mpsc::unbounded_channel();
    hub.set_dialer(tx);
    hub.set_proxies(rbitcoin_net::NetProxies {
        dialer: rbitcoin_net::Dialer::socks("127.0.0.1:9050".parse().unwrap(), true),
        i2p_sam: None,
    });
    (hub, rx)
}

/// A `--proxy` name is the live key of its session. A redial pass while the
/// session is up opens no second one, and two names on one port stay apart.
#[test]
fn proxy_names_are_the_live_key_of_their_sessions() {
    let (hub, mut rx) = proxied_hub();
    hub.set_connect_hosts(vec!["a.invalid:8333".into(), "b.invalid:8333".into()], 8333);
    hub.redial_remembered();
    let first = take_dials(&mut rx);
    assert_eq!(first.len(), 2);
    let a = first
        .iter()
        .find(|d| matches!(&d.target, DialTarget::Domain { host, .. } if host == "a.invalid"))
        .unwrap();
    let _session = hub.register_connecting_net(
        a.target.version_socket(),
        a.target.net_addr(),
        a.target.version_socket(),
        false,
        PeerConnType::Manual,
    );
    drop(first);
    hub.redial_remembered();
    let again: Vec<_> = take_dials(&mut rx)
        .into_iter()
        .map(|d| match d.target {
            DialTarget::Domain { host, .. } => host,
            DialTarget::Socket(a) => panic!("resolved locally to {a}"),
        })
        .collect();
    assert_eq!(
        again,
        ["b.invalid"],
        "a.invalid is live; only b.invalid is dialed"
    );
}

/// The session of the `--trust-peer` hostname is the trusted session, so the
/// block and tx waiver applies to it.
#[test]
fn proxied_trust_peer_hostname_is_the_trusted_session() {
    let (hub, _rx) = proxied_hub();
    hub.set_trust_peer(None, Some("trusted.invalid".into()));
    let target = hub.host_dial_target("trusted.invalid:8333").unwrap();
    let trusted = hub.register_connecting_net(
        target.version_socket(),
        target.net_addr(),
        target.version_socket(),
        false,
        PeerConnType::Manual,
    );
    assert!(hub.session_is_trusted_peer(&trusted));
    let other = hub.host_dial_target("other.invalid:8333").unwrap();
    let stranger = hub.register_connecting_net(
        other.version_socket(),
        other.net_addr(),
        other.version_socket(),
        false,
        PeerConnType::Manual,
    );
    assert!(!hub.session_is_trusted_peer(&stranger));
}

/// Filter replies and a served compact block take their consensus payload
/// on the send budget. A 2,000-hash `cfheaders` is far past the 64-byte
/// fallback, and a prefilled transaction past 1 KiB is not the compact-block stub.
#[test]
fn filter_and_compact_block_replies_charge_their_consensus_payload() {
    use bitcoin::bip152::HeaderAndShortIds;
    use bitcoin::bip158::{FilterHash, FilterHeader};
    use bitcoin::consensus::encode::serialize;
    use bitcoin::p2p::message_compact_blocks::CmpctBlock;
    use bitcoin::p2p::message_filter::{CFCheckpt, CFHeaders, CFilter};

    let zero = [0u8; 32];
    let block_hash = BlockHash::from_byte_array(zero);
    let cfilter = CFilter {
        filter_type: 0,
        block_hash,
        filter: vec![0x11; 200],
    };
    let cfilter_n = serialize(&cfilter).len();
    assert!(cfilter_n > 64, "a filter body is not the fallback size");
    assert_eq!(
        outbound_msg_bytes(&NetworkMessage::CFilter(cfilter)),
        cfilter_n
    );

    let cfheaders = CFHeaders {
        filter_type: 0,
        stop_hash: block_hash,
        previous_filter_header: FilterHeader::from_byte_array(zero),
        filter_hashes: vec![FilterHash::from_byte_array(zero); 2_000],
    };
    let cfheaders_n = serialize(&cfheaders).len();
    assert!(
        cfheaders_n >= 64_000,
        "two thousand filter hashes are not the fallback size"
    );
    assert_eq!(
        outbound_msg_bytes(&NetworkMessage::CFHeaders(cfheaders)),
        cfheaders_n
    );

    let checkpoints = 3;
    let cfcheckpt = CFCheckpt {
        filter_type: 0,
        stop_hash: block_hash,
        filter_headers: vec![FilterHeader::from_byte_array(zero); checkpoints],
    };
    let cfcheckpt_n = serialize(&cfcheckpt).len();
    assert!(cfcheckpt_n >= 32 * checkpoints);
    assert_eq!(
        outbound_msg_bytes(&NetworkMessage::CFCheckpt(cfcheckpt)),
        cfcheckpt_n
    );

    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    let fat = bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: bitcoin::ScriptBuf::from_bytes(vec![0x6a; 1_500]),
        }],
    };
    let block = bitcoin::Block {
        header: genesis.header,
        txdata: vec![fat],
    };
    let hsi = HeaderAndShortIds::from_block(&block, 1, 1, &[0]).expect("prefilled coinbase");
    let compact_n = serialize(&hsi).len();
    assert!(
        compact_n > 1_024,
        "a prefilled fat transaction is not the compact-block stub"
    );
    let served = NetworkMessage::CmpctBlock(CmpctBlock { compact_block: hsi });
    assert_eq!(outbound_msg_bytes(&served), compact_n);
    assert_eq!(
        outbound_queued_bytes(&PeerOut::Served(served)),
        compact_n,
        "a served compact block uses the same payload charge"
    );
}

#[test]
fn pong_reads_eight_byte_nonce_and_rejects_short() {
    let hub = PeerHub::new();
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
    let p = hub.register(a, a, &ver("/rbitcoin:test/"), false, PeerConnType::Inbound);
    let nonce = p.take_ping_send(10_000).expect("ping due");
    assert!(p.on_pong(&nonce.to_le_bytes(), 10_001).is_none());
    let short = p.on_pong(&[1, 2, 3], 10_002).expect("short pong");
    assert!(short.contains("Short payload"), "{short}");
    let nonce = p.take_ping_send(10_200).expect("next ping");
    let mut wrong = nonce.to_le_bytes();
    wrong[0] ^= 0xff;
    let mismatch = p.on_pong(&wrong, 10_201).expect("mismatched pong");
    assert!(mismatch.contains("Nonce mismatch"), "{mismatch}");
}
