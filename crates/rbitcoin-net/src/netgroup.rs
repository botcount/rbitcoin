//! Outbound / inbound netgroup key: asmap ASN, or IPv4 `/16` / IPv6 `/32`.

use crate::asmap::AsMap;
use crate::NetAddr;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

/// Stable neighborhood key. With asmap, the ASN (IPv4 and IPv6 that map to
/// the same ASN share a key). Without, Core-style prefix groups.
pub fn netgroup(addr: SocketAddr, asmap: Option<&AsMap>) -> u64 {
    if let Some(m) = asmap {
        return u64::from(m.mapped_as(addr.ip()));
    }
    prefix_group(addr.ip())
}

/// [`netgroup`] for any session net. An onion or I2P session's `addr` is the
/// `0.0.0.0` placeholder, so these key by network and the first four address
/// bits, as Core's `GetGroup` does. The keys sit above every prefix group and
/// ASN, so they never collide with an IP group.
pub fn netgroup_net(addr: NetAddr, asmap: Option<&AsMap>) -> u64 {
    const OVERLAY: u64 = 1 << 32;
    match addr {
        NetAddr::Ip(sock) => netgroup(sock, asmap),
        NetAddr::Cjdns { ip, port } => netgroup(SocketAddr::from((ip, port)), asmap),
        // BIP155 network ids: 4 is TorV3, 5 is I2P.
        NetAddr::Onion { pk, .. } => OVERLAY | (4 << 8) | u64::from(pk[0] >> 4),
        NetAddr::I2p { dest, .. } => OVERLAY | (5 << 8) | u64::from(dest[0] >> 4),
        // A `--proxy` hostname has no address bits to split on.
        NetAddr::Name { .. } => OVERLAY | (u64::from(u8::MAX) << 8),
    }
}

/// Walk `ranked` and prefer addrs whose group is not in `occupied` or already
/// picked. Once every unused-group candidate is taken, fill remaining slots
/// in ranked order (duplicates / occupied groups).
pub fn select_diverse(
    ranked: &[SocketAddr],
    max: usize,
    occupied_groups: &HashSet<u64>,
    mut group_of: impl FnMut(SocketAddr) -> u64,
) -> Vec<SocketAddr> {
    if max == 0 || ranked.is_empty() {
        return Vec::new();
    }
    let mut picked = Vec::new();
    let mut used = occupied_groups.clone();
    for &addr in ranked {
        if picked.len() >= max {
            break;
        }
        let g = group_of(addr);
        if used.contains(&g) {
            continue;
        }
        used.insert(g);
        picked.push(addr);
    }
    if picked.len() < max {
        for &addr in ranked {
            if picked.len() >= max {
                break;
            }
            if picked.contains(&addr) {
                continue;
            }
            picked.push(addr);
        }
    }
    picked
}

#[doc(hidden)]
pub fn prefix_group(ip: IpAddr) -> u64 {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            (u64::from(o[0]) << 8) | u64::from(o[1])
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            u64::from(u32::from_be_bytes([o[0], o[1], o[2], o[3]]))
        }
    }
}
