use rbitcoin_net::asmap::two_prefix_asmap_bytes;
use rbitcoin_net::asmap::AsMap;
use rbitcoin_net::netgroup::*;
use std::net::{IpAddr, SocketAddr};
use std::net::{Ipv4Addr, Ipv6Addr};

fn v4(a: u8, b: u8, c: u8, d: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), 8333)
}

#[test]
fn netgroup_prefix_ipv4_slash16() {
    assert_eq!(
        netgroup(v4(1, 2, 3, 4), None),
        netgroup(v4(1, 2, 9, 9), None)
    );
    assert_ne!(
        netgroup(v4(1, 2, 3, 4), None),
        netgroup(v4(1, 3, 0, 1), None)
    );
}

#[test]
fn netgroup_prefix_ipv6_slash32() {
    let a = SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        1,
    );
    let b = SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1)),
        1,
    );
    let c = SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb9, 0, 0, 0, 0, 0, 1)),
        1,
    );
    assert_eq!(netgroup(a, None), netgroup(b, None));
    assert_ne!(netgroup(a, None), netgroup(c, None));
}

#[test]
fn netgroup_asmap_two_asns() {
    let m = AsMap::from_bytes(two_prefix_asmap_bytes()).unwrap();
    let a = v4(1, 2, 3, 4);
    let b = v4(1, 2, 9, 9);
    let c = v4(3, 4, 0, 1);
    assert_eq!(netgroup(a, Some(&m)), netgroup(b, Some(&m)));
    assert_ne!(netgroup(a, Some(&m)), netgroup(c, Some(&m)));
    let v6 = SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0102, 0x0304)),
        1,
    );
    assert_eq!(netgroup(a, Some(&m)), netgroup(v6, Some(&m)));
}

fn group_octet0(a: SocketAddr) -> u64 {
    match a.ip() {
        IpAddr::V4(v) => u64::from(v.octets()[0]),
        IpAddr::V6(_) => 0,
    }
}

#[test]
fn select_diverse_skips_occupied_asn() {
    use std::collections::HashSet;
    let m = AsMap::from_bytes(two_prefix_asmap_bytes()).unwrap();
    let ranked = vec![v4(1, 2, 0, 1), v4(3, 4, 0, 1), v4(1, 2, 0, 2)];
    let mut occupied = HashSet::new();
    occupied.insert(netgroup(v4(1, 2, 0, 9), Some(&m)));
    let got = select_diverse(&ranked, 2, &occupied, |a| netgroup(a, Some(&m)));
    assert_eq!(got[0], v4(3, 4, 0, 1));
    assert_eq!(got[1], v4(1, 2, 0, 1));
}

#[test]
fn select_diverse_skips_occupied_then_fills() {
    use std::collections::HashSet;
    let ranked: Vec<SocketAddr> = (1..=4).map(|i| v4(i, 0, 0, 1)).collect();
    let mut occupied = HashSet::new();
    occupied.insert(1);
    let got = select_diverse(&ranked, 3, &occupied, group_octet0);
    assert_eq!(got[0], v4(2, 0, 0, 1));
    assert_eq!(got[1], v4(3, 0, 0, 1));
    assert_eq!(got[2], v4(4, 0, 0, 1));
    assert!(!got.contains(&v4(1, 0, 0, 1)));
}

#[test]
fn select_diverse_thin_book_still_fills_max() {
    use std::collections::HashSet;
    let ranked = vec![v4(1, 0, 0, 1), v4(1, 0, 0, 2), v4(1, 0, 0, 3)];
    let occupied = HashSet::new();
    let got = select_diverse(&ranked, 3, &occupied, group_octet0);
    assert_eq!(got.len(), 3);
    assert_eq!(got, ranked);
}

#[test]
fn select_diverse_four_groups_max_eight() {
    use std::collections::HashSet;
    let mut ranked = Vec::new();
    for g in 1u8..=4 {
        ranked.push(v4(g, 0, 0, 1));
        ranked.push(v4(g, 0, 0, 2));
    }
    let got = select_diverse(&ranked, 8, &HashSet::new(), group_octet0);
    assert_eq!(got.len(), 8);
    let first4: Vec<u8> = got[..4]
        .iter()
        .map(|a| match a.ip() {
            IpAddr::V4(v) => v.octets()[0],
            IpAddr::V6(_) => 0,
        })
        .collect();
    assert_eq!(first4, vec![1, 2, 3, 4]);
}

/// Onion and I2P get their own groups, keyed by network and the first four
/// address bits as Core does. Neither shares the `0.0.0.0` placeholder group.
#[test]
fn netgroup_net_keys_overlay_by_network() {
    use rbitcoin_net::NetAddr;
    let onion = |b: u8| NetAddr::Onion {
        pk: [b; 32],
        port: 8333,
    };
    let i2p = NetAddr::I2p {
        dest: [0x10; 32],
        port: 8333,
    };
    let placeholder = netgroup(SocketAddr::from(([0, 0, 0, 0], 8333)), None);
    assert_eq!(
        netgroup_net(onion(0x10), None),
        netgroup_net(onion(0x1f), None),
        "same first four bits share a group"
    );
    assert_ne!(
        netgroup_net(onion(0x10), None),
        netgroup_net(onion(0x20), None)
    );
    assert_ne!(netgroup_net(onion(0x10), None), netgroup_net(i2p, None));
    assert_ne!(netgroup_net(onion(0x00), None), placeholder);
    assert_ne!(netgroup_net(i2p, None), placeholder);
    assert_eq!(
        netgroup_net(NetAddr::Ip(v4(1, 2, 3, 4)), None),
        netgroup(v4(1, 2, 3, 4), None)
    );
}
