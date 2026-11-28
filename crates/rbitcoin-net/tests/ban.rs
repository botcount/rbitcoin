use bitcoin::p2p::address::Address;
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::ServiceFlags;
use rbitcoin_net::ban::{parse_ban_target, BanParseError, BanTarget};
use rbitcoin_net::peers::PeerConnType;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn ver() -> VersionMessage {
    let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444);
    VersionMessage {
        version: 70016,
        services: ServiceFlags::NETWORK | ServiceFlags::WITNESS,
        timestamp: 0,
        receiver: Address::new(&local, ServiceFlags::NONE),
        sender: Address::new(&local, ServiceFlags::NONE),
        nonce: 1,
        user_agent: "/rbitcoin:test/".into(),
        start_height: 0,
        relay: true,
    }
}

fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

#[test]
fn parse_ban_targets() {
    assert_eq!(
        parse_ban_target("1.2.3.4"),
        Ok(BanTarget::V4 {
            addr: Ipv4Addr::new(1, 2, 3, 4),
            prefix: 32
        })
    );
    assert_eq!(
        parse_ban_target("1.2.3.4/24"),
        Ok(BanTarget::V4 {
            addr: Ipv4Addr::new(1, 2, 3, 0),
            prefix: 24
        })
    );
    assert_eq!(
        parse_ban_target("2001:db8::1/32"),
        Ok(BanTarget::V6 {
            addr: "2001:db8::".parse().unwrap(),
            prefix: 32
        })
    );
    assert_eq!(
        parse_ban_target("::1"),
        Ok(BanTarget::V6 {
            addr: "::1".parse().unwrap(),
            prefix: 128
        })
    );
    let pk = [7u8; 32];
    let onion = format!("{}.onion", rbitcoin_net::netaddr::encode_onion_name(&pk));
    assert_eq!(parse_ban_target(&onion), Ok(BanTarget::Onion(pk)));
    assert_eq!(
        parse_ban_target(&format!("{onion}:8333")),
        Ok(BanTarget::Onion(pk))
    );
    let dest = [9u8; 32];
    let i2p = format!(
        "{}{}",
        rbitcoin_net::netaddr::encode_i2p_name(&dest),
        rbitcoin_net::netaddr::I2P_SUFFIX
    );
    assert_eq!(parse_ban_target(&i2p), Ok(BanTarget::I2p(dest)));
    assert_eq!(
        parse_ban_target(&format!("{i2p}:8333")),
        Ok(BanTarget::I2p(dest))
    );
    // An IPv4-mapped IPv6 target is stored as the IPv4 target it names:
    // admission canonicalizes mapped addresses to V4, so a stored V6
    // entry would never match anything.
    assert_eq!(
        parse_ban_target("::ffff:192.0.2.1"),
        Ok(BanTarget::V4 {
            addr: Ipv4Addr::new(192, 0, 2, 1),
            prefix: 32
        })
    );
    assert_eq!(
        parse_ban_target("::ffff:c000:0201"),
        Ok(BanTarget::V4 {
            addr: Ipv4Addr::new(192, 0, 2, 1),
            prefix: 32
        })
    );
    assert_eq!(
        parse_ban_target("::ffff:192.0.2.0/120"),
        Ok(BanTarget::V4 {
            addr: Ipv4Addr::new(192, 0, 2, 0),
            prefix: 24
        })
    );
}

#[test]
fn parse_ban_target_rejects_bad_and_broad() {
    for bad in [
        "",
        "not-an-address",
        "1.2.3.4/33",
        "1.2.3.4/x",
        "example.com",
        "1.2.3.4/",
    ] {
        assert_eq!(parse_ban_target(bad), Err(BanParseError::Invalid), "{bad}");
    }
    for broad in [
        "0.0.0.0/0",
        "1.2.3.4/7",
        "::/0",
        "2001:db8::/15",
        "::ffff:0.0.0.0/96",
        "::ffff:192.0.2.0/103",
    ] {
        assert_eq!(
            parse_ban_target(broad),
            Err(BanParseError::TooBroad),
            "{broad}"
        );
    }
}

#[test]
fn ban_target_matches_subnet_and_identity() {
    let subnet = parse_ban_target("10.1.2.3/24").unwrap();
    assert!(subnet.matches_ip(ip(10, 1, 2, 99)));
    assert!(!subnet.matches_ip(ip(10, 1, 3, 1)));
    let host = parse_ban_target("10.1.2.3").unwrap();
    assert!(host.matches_ip(ip(10, 1, 2, 3)));
    assert!(!host.matches_ip(ip(10, 1, 2, 4)));
    // A mapped spelling of a peer address matches its IPv4 ban.
    let host = parse_ban_target("192.0.2.1").unwrap();
    assert!(host.matches_ip("::ffff:192.0.2.1".parse::<IpAddr>().unwrap()));
    assert!(!host.matches_ip("::ffff:192.0.2.2".parse::<IpAddr>().unwrap()));
    let onion = BanTarget::Onion([7u8; 32]);
    assert!(onion.matches_net(rbitcoin_net::NetAddr::Onion {
        pk: [7u8; 32],
        port: 9999
    }));
    assert!(!onion.matches_net(rbitcoin_net::NetAddr::Onion {
        pk: [8u8; 32],
        port: 9999
    }));
    assert!(!onion.matches_ip(ip(10, 1, 2, 3)));
}

#[test]
fn hub_ban_add_remove_clear_and_expiry() {
    let hub = rbitcoin_net::PeerHub::new();
    hub.set_mock_now(1_000);
    let target = parse_ban_target("10.9.0.0/16").unwrap();
    let entry = hub.ban_add(target, 1_600).unwrap();
    assert_eq!(entry.created, 1_000);
    assert_eq!(entry.banned_until, 1_600);
    assert!(matches!(
        hub.ban_add(target, 1_700),
        Err(rbitcoin_net::ban::BanUpdateError::AlreadyBanned(_))
    ));
    assert!(hub.net_banned(rbitcoin_net::NetAddr::Ip(SocketAddr::new(
        ip(10, 9, 3, 4),
        8333
    ))));
    assert!(hub.ip_banned(ip(10, 9, 3, 4)));
    assert!(!hub.ip_banned(ip(10, 10, 3, 4)));
    assert_eq!(hub.ban_list().len(), 1);
    hub.set_mock_now(1_601);
    assert!(
        !hub.ip_banned(ip(10, 9, 3, 4)),
        "expired ban no longer refuses"
    );
    assert!(
        hub.ban_list().is_empty(),
        "expired ban is swept from the list"
    );
    hub.set_mock_now(2_000);
    let other = parse_ban_target("192.0.2.1").unwrap();
    hub.ban_add(other, 3_000).unwrap();
    assert_eq!(hub.ban_remove(other).unwrap().unwrap().target, other);
    assert!(hub.ban_remove(other).unwrap().is_none());
    hub.ban_add(target, 3_000).unwrap();
    assert_eq!(hub.ban_clear().unwrap(), 1);
    assert!(hub.ban_list().is_empty());
}

#[test]
fn ban_add_disconnects_matching_live_peer() {
    let hub = rbitcoin_net::PeerHub::new();
    let addr = SocketAddr::new(ip(203, 0, 113, 7), 8333);
    let other = SocketAddr::new(ip(198, 51, 100, 7), 8333);
    hub.register(addr, addr, &ver(), false, PeerConnType::OutboundFullRelay);
    hub.register(other, other, &ver(), false, PeerConnType::OutboundFullRelay);
    assert_eq!(hub.snapshot().len(), 2);
    let target = parse_ban_target("203.0.113.0/24").unwrap();
    hub.ban_add(target, hub.now_secs() + 600).unwrap();
    let rows = hub.snapshot();
    assert_eq!(rows.len(), 1, "banned peer leaves the session table");
    assert_eq!(rows[0].net, rbitcoin_net::NetAddr::Ip(other));
}

#[test]
fn ban_list_persists_to_ban_file() {
    let dir = rbitcoin_query::testutil::TempDir::labeled("ban-file").unwrap();
    let path = dir.path().join("banlist.json");
    let target = parse_ban_target("198.51.100.0/24").unwrap();
    let hub = rbitcoin_net::PeerHub::new();
    hub.set_mock_now(5_000);
    hub.set_ban_file(path.clone());
    hub.ban_add(target, 9_000).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("\"banned_nets\""), "{body}");
    assert!(body.contains("198.51.100.0/24"), "{body}");
    let hub2 = rbitcoin_net::PeerHub::new();
    hub2.set_mock_now(5_000);
    hub2.set_ban_file(path.clone());
    let rows = hub2.ban_list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target, target);
    assert_eq!(rows[0].banned_until, 9_000);
    assert!(hub2.ip_banned(ip(198, 51, 100, 55)));
    hub2.ban_clear().unwrap();
    let hub3 = rbitcoin_net::PeerHub::new();
    hub3.set_mock_now(5_000);
    hub3.set_ban_file(path);
    assert!(hub3.ban_list().is_empty(), "clear is persisted");
}

#[test]
fn ban_file_with_garbage_or_bad_rows_loads_what_it_can() {
    let dir = rbitcoin_query::testutil::TempDir::labeled("ban-file-bad").unwrap();
    let path = dir.path().join("banlist.json");
    std::fs::write(&path, "not json").unwrap();
    let hub = rbitcoin_net::PeerHub::new();
    hub.set_mock_now(5_000);
    hub.set_ban_file(path.clone());
    assert!(hub.ban_list().is_empty());
    std::fs::write(
        &path,
        r#"{"banned_nets":[
            {"address":"junk","ban_created":1,"banned_until":9000},
            {"address":"203.0.113.0/24","ban_created":10,"banned_until":9000},
            {"address":"203.0.113.0/24","ban_created":10,"banned_until":4000}
        ]}"#,
    )
    .unwrap();
    let hub = rbitcoin_net::PeerHub::new();
    hub.set_mock_now(5_000);
    hub.set_ban_file(path);
    let rows = hub.ban_list();
    assert_eq!(rows.len(), 1, "bad and expired rows are dropped: {rows:?}");
    assert_eq!(rows[0].created, 10);
}

#[test]
fn ban_file_write_failure_rolls_back_and_retries() {
    use rbitcoin_net::ban::BanUpdateError;
    let dir = rbitcoin_query::testutil::TempDir::labeled("ban-file-fail").unwrap();
    let path = dir.path().join("banlist.json");
    let hub = rbitcoin_net::PeerHub::new();
    hub.set_mock_now(5_000);
    hub.set_ban_file(path);
    let target = parse_ban_target("198.51.100.0/24").unwrap();
    hub.ban_add(target, 9_000).unwrap();
    // A directory where the temp file must be created fails every write,
    // for root and non-root alike.
    std::fs::create_dir(dir.path().join("banlist.json.new")).unwrap();
    let other = parse_ban_target("203.0.113.0/24").unwrap();
    assert!(
        matches!(hub.ban_add(other, 9_000), Err(BanUpdateError::Persist(_))),
        "a failed add reports the write failure"
    );
    assert_eq!(hub.ban_list().len(), 1, "a failed add is rolled back");
    assert!(
        matches!(hub.ban_remove(target), Err(BanUpdateError::Persist(_))),
        "a failed remove reports the write failure"
    );
    assert_eq!(hub.ban_list().len(), 1, "a failed remove is rolled back");
    assert!(
        matches!(hub.ban_clear(), Err(BanUpdateError::Persist(_))),
        "a failed clear reports the write failure"
    );
    assert_eq!(hub.ban_list().len(), 1, "a failed clear is rolled back");
    std::fs::remove_dir(dir.path().join("banlist.json.new")).unwrap();
    assert_eq!(
        hub.ban_clear().unwrap(),
        1,
        "clear retries once storage recovers"
    );
    assert!(hub.ban_list().is_empty());
}

/// A ban that lands before publication refuses it: the published
/// table never holds the banned session.
#[test]
fn publication_refuses_a_banned_endpoint() {
    let hub = rbitcoin_net::PeerHub::new();
    let addr = SocketAddr::new(ip(203, 0, 113, 9), 8333);
    let net = rbitcoin_net::NetAddr::Ip(addr);
    let provisional =
        hub.register_connecting_net(addr, net, addr, false, PeerConnType::OutboundFullRelay);
    let target = parse_ban_target("203.0.113.9").unwrap();
    hub.ban_add(target, hub.now_secs() + 600).unwrap();
    let published = hub.register_handshaken_net(
        &provisional,
        rbitcoin_net::peers::PeerEndpoint {
            addr,
            net,
            addrbind: addr,
        },
        &ver(),
        PeerConnType::OutboundFullRelay,
        rbitcoin_net::v2::WireBytes::default(),
    );
    assert!(published.is_none(), "a banned endpoint is not published");
    hub.unregister(provisional.id);
    assert!(hub.snapshot().is_empty());
}

/// The interleaving the ban lock closes: `ban_add` and publication
/// race, and afterwards the banned session is either unpublished or
/// already disconnect-requested — never live and unnoticed.
#[test]
fn ban_and_publication_race_never_leaves_a_banned_session() {
    for _ in 0..200 {
        let hub = rbitcoin_net::PeerHub::new();
        let addr = SocketAddr::new(ip(203, 0, 113, 10), 8333);
        let net = rbitcoin_net::NetAddr::Ip(addr);
        let provisional =
            hub.register_connecting_net(addr, net, addr, false, PeerConnType::OutboundFullRelay);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let hub2 = std::sync::Arc::clone(&hub);
        let b2 = std::sync::Arc::clone(&barrier);
        let banner = std::thread::spawn(move || {
            b2.wait();
            let target = parse_ban_target("203.0.113.10").unwrap();
            hub2.ban_add(target, hub2.now_secs() + 600).unwrap();
        });
        barrier.wait();
        let published = hub.register_handshaken_net(
            &provisional,
            rbitcoin_net::peers::PeerEndpoint {
                addr,
                net,
                addrbind: addr,
            },
            &ver(),
            PeerConnType::OutboundFullRelay,
            rbitcoin_net::v2::WireBytes::default(),
        );
        banner.join().unwrap();
        match published {
            None => {
                hub.unregister(provisional.id);
                assert!(
                    hub.snapshot().iter().all(|r| r.net != net),
                    "refused publication left no row"
                );
            }
            Some(sess) => assert!(
                sess.disconnect_requested(),
                "published before the ban: the ban must disconnect it"
            ),
        }
    }
}
