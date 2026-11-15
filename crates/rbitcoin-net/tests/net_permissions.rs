use rbitcoin_net::net_permissions::*;
use std::net::SocketAddrV4;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn bind() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18444))
}

#[test]
fn loopback_forward_does_not_inherit_loopback_net_permission() {
    let peers = rbitcoin_net::peers::PeerHub::new();
    let mut t = NetPermTable::default();
    t.whitelist
        .push(parse_whitelist("noban,relay,forcerelay@127.0.0.1").unwrap());
    t.whitebind
        .push(parse_whitebind("noban@127.0.0.1:8335").unwrap());
    peers.set_net_perms(t);
    let loopback: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let mapped: SocketAddr = "[::ffff:127.0.0.1]:40000".parse().unwrap();
    let p2p: SocketAddr = "0.0.0.0:8333".parse().unwrap();
    let wallet: SocketAddr = "127.0.0.1:8335".parse().unwrap();
    let remote: SocketAddr = "203.0.113.9:8333".parse().unwrap();

    assert!(peers
        .permission_flags(loopback, true, p2p)
        .has(NetPermissionFlags::NOBAN));

    peers.set_overlay_loopback_forward(true);
    for src in [loopback, mapped] {
        let flags = peers.permission_flags(src, true, p2p);
        assert!(
            !flags.has(NetPermissionFlags::NOBAN),
            "{src} took noban from the loopback subnet"
        );
        assert!(!flags.has(NetPermissionFlags::RELAY));
        assert!(!flags.has(NetPermissionFlags::FORCE_RELAY));
    }
    assert!(peers
        .permission_flags(loopback, true, wallet)
        .has(NetPermissionFlags::NOBAN));
    assert!(!peers
        .permission_flags(loopback, true, wallet)
        .has(NetPermissionFlags::RELAY));

    use bitcoin::p2p::address::Address;
    use bitcoin::p2p::message_network::VersionMessage;
    use bitcoin::p2p::ServiceFlags;
    let ver = VersionMessage {
        version: 70016,
        services: ServiceFlags::NONE,
        timestamp: 0,
        receiver: Address::new(&loopback, ServiceFlags::NONE),
        sender: Address::new(&loopback, ServiceFlags::NONE),
        nonce: 1,
        user_agent: "/rbitcoin:test/".into(),
        start_height: 0,
        relay: true,
    };
    let live = peers.register(
        loopback,
        p2p,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    assert!(
        !live.session_noban(),
        "a loopback inbound session still took noban"
    );
    assert!(!live.session_relay_perm());
    assert!(!live.session_forcerelay());
    let bound = peers.register(
        loopback,
        wallet,
        &ver,
        true,
        rbitcoin_net::peers::PeerConnType::Inbound,
    );
    assert!(
        bound.session_noban(),
        "permission-bind on another port still grants"
    );
    assert!(!bound.session_relay_perm());

    let mut wide = NetPermTable::default();
    wide.whitelist
        .push(parse_whitelist("relay@203.0.113.9").unwrap());
    peers.set_net_perms(wide);
    assert!(peers
        .permission_flags(remote, true, p2p)
        .has(NetPermissionFlags::RELAY));
    assert!(!peers
        .permission_flags(loopback, true, p2p)
        .has(NetPermissionFlags::RELAY));

    peers.set_net_perms(NetPermTable::default());
    assert_eq!(
        peers.permission_flags(loopback, true, p2p),
        NetPermissionFlags::NONE
    );
}

#[test]
fn loopback_grant_does_not_cover_onion_or_i2p() {
    let mut t = NetPermTable::default();
    t.whitelist
        .push(parse_whitelist("noban@127.0.0.1").unwrap());
    let onion = rbitcoin_net::NetAddr::Onion {
        pk: [0x11; 32],
        port: 8333,
    };
    let i2p = rbitcoin_net::NetAddr::I2p {
        dest: [0x22; 32],
        port: 0,
    };
    assert!(!t
        .flags_for_net(onion, true, bind())
        .has(NetPermissionFlags::NOBAN));
    assert!(!t
        .flags_for_net(i2p, true, bind())
        .has(NetPermissionFlags::NOBAN));
    let local = rbitcoin_net::NetAddr::Ip("127.0.0.1:8333".parse().unwrap());
    assert!(t
        .flags_for_net(local, true, bind())
        .has(NetPermissionFlags::NOBAN));
}

#[test]
fn implicit_whitelist_defaults() {
    let g = parse_whitelist("127.0.0.1").unwrap();
    assert!(g.flags.has(NetPermissionFlags::IMPLICIT));
    assert!(g.inbound);
    assert!(!g.outbound);
    let flags = apply_implicit(g.flags, true, false);
    assert_eq!(
        flags.to_strings(),
        ["noban", "relay", "mempool", "download"]
    );
}

#[test]
fn empty_explicit_is_none_even_with_forcerelay() {
    let g = parse_whitelist("@127.0.0.1").unwrap();
    assert!(!g.flags.has(NetPermissionFlags::IMPLICIT));
    let flags = apply_implicit(g.flags, true, true);
    assert!(flags.to_strings().is_empty());
}

#[test]
fn whitelistrelay_off_drops_relay() {
    let g = parse_whitelist("127.0.0.1").unwrap();
    let flags = apply_implicit(g.flags, false, false);
    assert_eq!(flags.to_strings(), ["noban", "mempool", "download"]);
}

#[test]
fn whitelistforcerelay_adds_forcerelay() {
    let g = parse_whitelist("127.0.0.1").unwrap();
    let flags = apply_implicit(g.flags, true, true);
    assert_eq!(
        flags.to_strings(),
        ["noban", "forcerelay", "relay", "mempool", "download"]
    );
}

#[test]
fn explicit_noban_implies_download() {
    let g = parse_whitelist("noban@127.0.0.1").unwrap();
    assert_eq!(g.flags.to_strings(), ["noban", "download"]);
}

#[test]
fn all_lists_core_names() {
    let g = parse_whitelist("all@127.0.0.1").unwrap();
    assert_eq!(
        g.flags.to_strings(),
        [
            "bloomfilter",
            "noban",
            "forcerelay",
            "relay",
            "mempool",
            "download",
            "addr"
        ]
    );
}

#[test]
fn direction_only_is_init_error() {
    let err = parse_whitelist("in,out@127.0.0.1").unwrap_err();
    assert!(
        err.contains("Only direction was set, no permissions"),
        "{err}"
    );
}

#[test]
fn unknown_perm_is_init_error() {
    let err = parse_whitelist("oopsie@127.0.0.1").unwrap_err();
    assert!(err.contains("Invalid P2P permission"), "{err}");
}

#[test]
fn whitelist_port_is_invalid_netmask() {
    let err = parse_whitelist("noban@127.0.0.1:230").unwrap_err();
    assert!(
        err.contains("Invalid netmask specified in --net-permission"),
        "{err}"
    );
}

#[test]
fn whitebind_cidr_cannot_resolve() {
    let err = parse_whitebind("noban@127.0.0.1/10").unwrap_err();
    assert!(
        err.contains("Cannot resolve --net-permission-bind address"),
        "{err}"
    );
}

#[test]
fn outbound_only_noban() {
    let g = parse_whitelist("noban,out@127.0.0.1").unwrap();
    let mut t = NetPermTable::default();
    t.whitelist.push(WhitelistGrant {
        subnet: g.subnet,
        flags: apply_implicit(g.flags, true, false),
        inbound: g.inbound,
        outbound: g.outbound,
    });
    assert_eq!(t.strings_for(ip(), false, bind()), ["noban", "download"]);
    assert!(t.strings_for(ip(), true, bind()).is_empty());
}

#[test]
fn inbound_default_misses_outbound() {
    let g = parse_whitelist("noban@127.0.0.1").unwrap();
    let mut t = NetPermTable::default();
    t.whitelist.push(WhitelistGrant {
        subnet: g.subnet,
        flags: g.flags,
        inbound: g.inbound,
        outbound: g.outbound,
    });
    assert!(t.strings_for(ip(), false, bind()).is_empty());
    assert_eq!(t.strings_for(ip(), true, bind()), ["noban", "download"]);
}

#[test]
fn whitebind_merges_with_whitelist() {
    let wb = parse_whitebind("bloomfilter,forcerelay@127.0.0.1:18444").unwrap();
    let wl = parse_whitelist("noban@127.0.0.1").unwrap();
    let mut t = NetPermTable::default();
    t.whitebind.push(wb);
    t.whitelist.push(WhitelistGrant {
        subnet: wl.subnet,
        flags: wl.flags,
        inbound: wl.inbound,
        outbound: wl.outbound,
    });
    let s = t.strings_for(ip(), true, bind());
    assert_eq!(
        s,
        ["bloomfilter", "noban", "forcerelay", "relay", "download"]
    );
}

#[test]
fn whitebind_out_is_init_error() {
    let err = parse_whitebind("noban,out@127.0.0.1:18444").unwrap_err();
    assert!(
        err.contains("--net-permission-bind may only be used for incoming connections"),
        "{err}"
    );
}

#[test]
fn whitelist_prefix_and_ipv6() {
    let g = parse_whitelist("noban@127.0.0.1/0").unwrap();
    assert!(g.subnet.contains(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    assert!(!g.subnet.contains(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
    let err = parse_whitelist("noban@127.0.0.1/33").unwrap_err();
    assert!(err.contains("Invalid netmask specified in"), "{err}");
    let err = parse_whitelist("noban@127.0.0.1/nope").unwrap_err();
    assert!(err.contains("Invalid netmask specified in"), "{err}");
}

#[test]
fn prefix_masks_keep_the_named_bits() {
    let host = |o: [u8; 4]| IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]));
    let v4 = parse_whitelist("noban@10.0.0.0/8").unwrap();
    assert!(v4.subnet.contains(host([10, 1, 2, 3])));
    assert!(!v4.subnet.contains(host([11, 0, 0, 1])));
    let pair = parse_whitelist("noban@10.0.0.0/31").unwrap();
    assert!(pair.subnet.contains(host([10, 0, 0, 0])));
    assert!(pair.subnet.contains(host([10, 0, 0, 1])));
    assert!(!pair.subnet.contains(host([10, 0, 0, 2])));
    let one = parse_whitelist("noban@10.1.2.3/32").unwrap();
    assert!(one.subnet.contains(host([10, 1, 2, 3])));
    assert!(!one.subnet.contains(host([10, 1, 2, 4])));
    let any = parse_whitelist("noban@0.0.0.0/0").unwrap();
    assert!(any.subnet.contains(host([8, 8, 8, 8])));

    let v6 = parse_whitelist("noban@2001:db8::/32").unwrap();
    assert!(v6
        .subnet
        .contains(IpAddr::V6("2001:db8:1::".parse().unwrap())));
    assert!(!v6
        .subnet
        .contains(IpAddr::V6("2001:db9::".parse().unwrap())));
}

#[test]
fn whitebind_matches_only_that_socket() {
    let wb = parse_whitebind("noban@127.0.0.1:18444").unwrap();
    let mut t = NetPermTable::default();
    t.whitebind.push(wb);
    let hit = SocketAddr::from(([127, 0, 0, 1], 18444));
    let other = SocketAddr::from(([127, 0, 0, 1], 18445));
    assert!(t.flags_for(ip(), true, hit).has(NetPermissionFlags::NOBAN));
    assert!(
        !t.flags_for(ip(), true, other)
            .has(NetPermissionFlags::NOBAN),
        "a different port is not this bind"
    );
    assert_eq!(
        t.flags_for(ip(), false, hit),
        NetPermissionFlags::NONE,
        "whitebind is inbound only"
    );
}

#[test]
fn ipv6_cidr_matches_only_v6() {
    let g = parse_whitelist("noban@::1").unwrap();
    assert!(g.subnet.contains(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
    assert!(!g.subnet.contains(ip()));
    let g = parse_whitelist("noban@2001:db8::/32").unwrap();
    assert!(g
        .subnet
        .contains(IpAddr::V6("2001:db8::1".parse().unwrap())));
    assert!(!g
        .subnet
        .contains(IpAddr::V6("2001:db9::1".parse().unwrap())));
    let g = parse_whitelist("noban@[::1]/128").unwrap();
    assert!(g.subnet.contains(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
    let err = parse_whitelist("noban@::1/129").unwrap_err();
    assert!(err.contains("Invalid netmask specified in"), "{err}");
}

#[test]
fn whitebind_port_zero_and_missing() {
    let err = parse_whitebind("noban@127.0.0.1").unwrap_err();
    assert!(
        err.contains("Need to specify a port with --net-permission-bind"),
        "{err}"
    );
    let err = parse_whitebind("noban@127.0.0.1:0").unwrap_err();
    assert!(
        err.contains("Need to specify a port with --net-permission-bind"),
        "{err}"
    );
    let err = parse_whitebind("noban@not-an-addr:18444").unwrap_err();
    assert!(
        err.contains("Cannot resolve --net-permission-bind address"),
        "{err}"
    );
}

#[test]
fn flags_bitor_and_unmatched_subnet() {
    let a = NetPermissionFlags::NOBAN | NetPermissionFlags::RELAY;
    assert!(a.has(NetPermissionFlags::NOBAN));
    assert!(a.has(NetPermissionFlags::RELAY));
    let same = NetPermissionFlags::NOBAN | NetPermissionFlags::NOBAN;
    assert!(
        same.has(NetPermissionFlags::NOBAN),
        "or with the same flag must keep that bit"
    );
    let mut b = NetPermissionFlags::BLOOM;
    b |= NetPermissionFlags::ADDR;
    assert_eq!(b.to_strings(), ["bloomfilter", "addr"]);
    let g = parse_whitelist("noban@10.0.0.1/32").unwrap();
    let mut t = NetPermTable::default();
    t.whitelist.push(g);
    assert!(t.strings_for(ip(), true, bind()).is_empty());
}

#[test]
fn permission_masks_keep_their_implied_bits() {
    assert!(NetPermissionFlags::FORCE_RELAY.has(NetPermissionFlags::RELAY));
    assert_ne!(NetPermissionFlags::FORCE_RELAY, NetPermissionFlags::RELAY);
    assert!(NetPermissionFlags::NOBAN.has(NetPermissionFlags::DOWNLOAD));
    assert_ne!(NetPermissionFlags::NOBAN, NetPermissionFlags::DOWNLOAD);
    assert_eq!(NetPermissionFlags::NOBAN.0 & (1 << 4), 1 << 4);
    for flag in [
        NetPermissionFlags::BLOOM,
        NetPermissionFlags::FORCE_RELAY,
        NetPermissionFlags::RELAY,
        NetPermissionFlags::NOBAN,
        NetPermissionFlags::MEMPOOL,
        NetPermissionFlags::DOWNLOAD,
        NetPermissionFlags::ADDR,
    ] {
        assert!(NetPermissionFlags::ALL.has(flag), "{flag:?}");
    }
    let mut flagged = NetPermissionFlags::ALL;
    flagged.add(NetPermissionFlags::IMPLICIT);
    assert!(flagged.has(NetPermissionFlags::IMPLICIT));
    flagged.clear_implicit();
    assert!(!flagged.has(NetPermissionFlags::IMPLICIT));
    assert!(flagged.has(NetPermissionFlags::MEMPOOL));
}

#[test]
fn v1_is_a_bind_permission_on_a_private_address() {
    let g = parse_whitebind("v1,noban@127.0.0.1:8335").unwrap();
    assert_eq!(g.flags.to_strings(), ["noban", "download", "v1"]);
    let all = parse_whitebind("all@127.0.0.1:8335").unwrap();
    assert!(!all.flags.to_strings().contains(&"v1"), "all is Core's set");
    let err = parse_whitelist("v1@127.0.0.1").unwrap_err();
    assert!(err.contains("--net-permission-bind"), "{err}");
    for bind in [
        "127.0.0.1:8335",
        "[::1]:8335",
        "10.0.0.5:8335",
        "172.17.0.1:8335",
        "192.168.1.5:8335",
        "100.64.10.10:8335",
        "[fd00::1]:8335",
        "[::ffff:127.0.0.1]:8335",
    ] {
        assert!(parse_whitebind(&format!("v1@{bind}")).is_ok(), "{bind}");
    }
    for bind in [
        "0.0.0.0:8335",
        "[::]:8335",
        "8.8.8.8:8335",
        "[2001:4860:4860::8888]:8335",
        "[::ffff:0.0.0.0]:8335",
        "[2002:808:808:1::1]:8335",
        "[2001:0:4136:e378:8000:63bf:3fff:fdd2]:8335",
        "[fc00::1]:8335",
        "169.254.1.1:8335",
    ] {
        let err = parse_whitebind(&format!("v1@{bind}")).unwrap_err();
        assert!(err.contains("loopback or private address"), "{bind}: {err}");
    }
}
