//! BIP155-capable peer identity: clearnet [`SocketAddr`] and Tor v3 onion.

use crate::error::NetError;
use bitcoin::p2p::address::{AddrV2, AddrV2Message};
use sha3::{Digest, Sha3_256};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

#[doc(hidden)]
pub const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
#[doc(hidden)]
pub const ONION_VERSION: u8 = 3;
#[doc(hidden)]
pub const ONION_NAME_LEN: usize = 56;
#[doc(hidden)]
pub const I2P_B32_LEN: usize = 52;
#[doc(hidden)]
pub const I2P_SUFFIX: &str = ".b32.i2p";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnlyNet {
    Ipv4,
    Ipv6,
    Onion,
    I2p,
    Cjdns,
}

impl OnlyNet {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ipv4" => Ok(Self::Ipv4),
            "ipv6" => Ok(Self::Ipv6),
            "onion" => Ok(Self::Onion),
            "i2p" => Ok(Self::I2p),
            "cjdns" => Ok(Self::Cjdns),
            other => Err(format!("unknown network {other}")),
        }
    }

    pub const ALL: [Self; 5] = [Self::Ipv4, Self::Ipv6, Self::Onion, Self::I2p, Self::Cjdns];

    pub fn of(addr: NetAddr) -> Self {
        match addr {
            NetAddr::Ip(s) if s.is_ipv4() => Self::Ipv4,
            NetAddr::Ip(_) => Self::Ipv6,
            NetAddr::Onion { .. } => Self::Onion,
            NetAddr::I2p { .. } => Self::I2p,
            NetAddr::Cjdns { .. } => Self::Cjdns,
            NetAddr::Name { .. } => Self::Ipv4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
            Self::Onion => "onion",
            Self::I2p => "i2p",
            Self::Cjdns => "cjdns",
        }
    }
}

/// Prefixes `Ipv6Addr` does not classify on 1.95.
///
/// `2001:db8::/32` and `3fff::/20` documentation, `2001:2::/48` benchmarking,
/// `2001:10::/28` ORCHID, `2001:20::/28` ORCHIDv2, and `100::/64` discard.
/// `is_documentation` is still unstable.
#[doc(hidden)]
pub fn ipv6_special_unroutable(v: Ipv6Addr) -> bool {
    let o = v.octets();
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x0d && o[3] == 0xb8 {
        return true;
    }
    // RFC 9637 documentation. Core `IsRFC9637` rejects this in `IsValid`.
    if o[0] == 0x3f && o[1] == 0xff && (o[2] & 0xf0) == 0 {
        return true;
    }
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x00 && (o[3] & 0xf0) == 0x10 {
        return true;
    }
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x00 && (o[3] & 0xf0) == 0x20 {
        return true;
    }
    // RFC 5180 benchmarking. The IPv4 twin is `198.18.0.0/15`. The last
    // 80 bits are free, so the whole prefix has to go.
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x00 && o[3] == 0x02 && o[4] == 0x00 && o[5] == 0x00
    {
        return true;
    }
    o[0] == 0x01 && o[1] == 0x00 && o[2..8].iter().all(|&b| b == 0)
}

/// Tunnel and translation prefixes.
///
/// A public embedded IPv4 stays only in its canonical form: 6to4 and local
/// NAT64 with the non-IPv4 bits zero, well-known NAT64, IPv4-compatible
/// `::/96`, and SIIT `::ffff:0:0:0/96`. Any other suffix is a distinct map
/// key and would fill the flat 8192 book. Teredo `2001::/32` is dropped
/// entirely: the server address is 32 free bits, so a canonical suffix
/// still fills the book. IPv4-mapped `::ffff:0:0/96` is unwrapped earlier.
#[doc(hidden)]
pub fn ipv6_embedded_bogon(v: Ipv6Addr) -> bool {
    let o = v.octets();
    // Teredo. Client IPv4 is bit-flipped; server, flags, and port are free.
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x00 && o[3] == 0x00 {
        return true;
    }
    // 6to4. Bytes 2–5 are the IPv4. Bytes 6–15 must be zero.
    if o[0] == 0x20 && o[1] == 0x02 {
        return ipv4_unroutable(Ipv4Addr::new(o[2], o[3], o[4], o[5]))
            || o[6..].iter().any(|&b| b != 0);
    }
    // Well-known NAT64 `64:ff9b::/96`.
    if o[0] == 0x00
        && o[1] == 0x64
        && o[2] == 0xff
        && o[3] == 0x9b
        && o[4..12].iter().all(|&b| b == 0)
    {
        return ipv4_unroutable(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // Local NAT64 `64:ff9b:1::/48`. Bytes 6–11 must be zero.
    if o[0] == 0x00 && o[1] == 0x64 && o[2] == 0xff && o[3] == 0x9b && o[4] == 0x00 && o[5] == 0x01
    {
        return ipv4_unroutable(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
            || o[6..12].iter().any(|&b| b != 0);
    }
    // SIIT `::ffff:0:0:0/96`. Not IPv4-mapped `::ffff:0:0/96`.
    if o[..8].iter().all(|&b| b == 0) && o[8] == 0xff && o[9] == 0xff && o[10] == 0 && o[11] == 0 {
        return ipv4_unroutable(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // Deprecated IPv4-compatible `::/96`.
    if o[..12].iter().all(|&b| b == 0) {
        return ipv4_unroutable(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    false
}

/// `100.64.0.0/10`. `Ipv4Addr::is_shared` is still unstable on 1.95.
#[doc(hidden)]
pub fn ipv4_shared(v: Ipv4Addr) -> bool {
    let o = v.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// `198.18.0.0/15`. `Ipv4Addr::is_benchmarking` is still unstable on 1.95.
#[doc(hidden)]
pub fn ipv4_benchmarking(v: Ipv4Addr) -> bool {
    let o = v.octets();
    o[0] == 198 && (o[1] & 0xfe) == 18
}

/// `240.0.0.0/4`. `Ipv4Addr::is_reserved` is still unstable on 1.95.
#[doc(hidden)]
pub fn ipv4_reserved(v: Ipv4Addr) -> bool {
    v.octets()[0] & 0xf0 == 240
}

/// `0.0.0.0/8`. `is_unspecified` is only `0.0.0.0`.
#[doc(hidden)]
pub fn ipv4_this_network(v: Ipv4Addr) -> bool {
    v.octets()[0] == 0
}

#[doc(hidden)]
pub fn ipv4_unroutable(v: Ipv4Addr) -> bool {
    v.is_unspecified()
        || v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        // `is_broadcast` is only `255.255.255.255`, already in `240.0.0.0/4`.
        || v.is_multicast()
        || v.is_documentation()
        || ipv4_shared(v)
        || ipv4_benchmarking(v)
        || ipv4_reserved(v)
        || ipv4_this_network(v)
}

/// Clearnet bogon. CJDNS `fc00::/8` is an overlay, not a unique-local bogon.
pub fn clearnet_unroutable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => ipv4_unroutable(v),
        IpAddr::V6(v) => {
            if is_cjdns_ip(v) {
                return false;
            }
            if let Some(v4) = v.to_ipv4_mapped() {
                return ipv4_unroutable(v4);
            }
            v.is_unspecified()
                // `::1` is IPv4-compatible `0.0.0.1`, which is this-network.
                || v.is_unicast_link_local()
                || v.is_unique_local()
                || v.is_multicast()
                || ipv6_special_unroutable(v)
                || ipv6_embedded_bogon(v)
        }
    }
}

/// Seed inject, IBD and steady-state addr gossip, getaddr, and peers-file
/// load. Onion, I2P, and CJDNS are not IP bogons. `add_learned` stays open
/// for `addpeeraddress` and `--connect`.
pub fn netaddr_unroutable_gossip(addr: NetAddr) -> bool {
    match addr {
        NetAddr::Ip(s) => clearnet_unroutable(s.ip()),
        NetAddr::Onion { .. } | NetAddr::I2p { .. } | NetAddr::Cjdns { .. } => false,
        NetAddr::Name { .. } => true,
    }
}

/// Rows the addrv2 relay loop must not forward.
///
/// Port 0 is not a dialable endpoint except I2P, which Core advertises with
/// port 0. Tor v2 and CJDNS outside `fc00::/8` never become a [`NetAddr`],
/// so the unroutable check would not see them. Unknown BIP155 network ids
/// are still forwarded.
pub fn addrv2_skip_relay(msg: &AddrV2Message) -> bool {
    match &msg.addr {
        AddrV2::I2p(_) => false,
        AddrV2::TorV2(_) => true,
        AddrV2::Cjdns(ip) if !is_cjdns_ip(*ip) => true,
        _ if msg.port == 0 => true,
        _ => NetAddr::from_addrv2(msg).is_some_and(netaddr_unroutable_gossip),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetAddr {
    Ip(SocketAddr),
    Onion {
        pk: [u8; 32],
        port: u16,
    },
    I2p {
        dest: [u8; 32],
        port: u16,
    },
    Cjdns {
        ip: Ipv6Addr,
        port: u16,
    },
    /// A hostname the `--proxy` resolves. It keys a live session and a pin, and
    /// never reaches the address book or gossip.
    Name {
        id: NameId,
        port: u16,
    },
}

/// A hostname interned for the life of the process, so [`NetAddr`] stays `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NameId(u32);

const MAX_NAMES: usize = 1024;
static NAMES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

impl NameId {
    /// `None` once [`MAX_NAMES`] distinct names are held.
    pub fn intern(host: &str) -> Option<Self> {
        let host = host.to_ascii_lowercase();
        let mut names = NAMES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(at) = names.iter().position(|n| *n == host) {
            return Some(Self(at as u32));
        }
        if names.len() >= MAX_NAMES {
            return None;
        }
        names.push(host);
        Some(Self(names.len() as u32 - 1))
    }

    /// The id of a name already interned; never takes a slot.
    pub fn get(host: &str) -> Option<Self> {
        let host = host.to_ascii_lowercase();
        let names = NAMES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        names
            .iter()
            .position(|n| *n == host)
            .map(|at| Self(at as u32))
    }

    pub fn host(self) -> String {
        NAMES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())[self.0 as usize]
            .clone()
    }
}

/// BIP155 / Core: CJDNS overlay is `fc00::/8`, not RFC4193 `fc00::/7`.
pub fn is_cjdns_ip(ip: Ipv6Addr) -> bool {
    ip.octets()[0] == 0xfc
}

impl fmt::Display for NetAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NetAddr::Ip(addr) => write!(f, "{addr}"),
            NetAddr::Onion { pk, port } => {
                write!(f, "{}.onion:{port}", encode_onion_name(&pk))
            }
            NetAddr::I2p { dest, port } => {
                write!(f, "{}{I2P_SUFFIX}:{port}", encode_i2p_name(&dest))
            }
            NetAddr::Cjdns { ip, port } => write!(f, "{}", SocketAddr::from((ip, port))),
            NetAddr::Name { id, port } => write!(f, "{}:{port}", id.host()),
        }
    }
}

impl FromStr for NetAddr {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_net_addr(s)
    }
}

impl NetAddr {
    pub fn from_addrv2(msg: &AddrV2Message) -> Option<Self> {
        match &msg.addr {
            // SAM 3.1 has no ports. Core advertises I2P with port 0.
            AddrV2::I2p(dest) => Some(NetAddr::I2p {
                dest: *dest,
                port: msg.port,
            }),
            _ if msg.port == 0 => None,
            AddrV2::Ipv4(_) | AddrV2::Ipv6(_) => msg.socket_addr().ok().map(NetAddr::from_socket),
            AddrV2::TorV3(pk) => Some(NetAddr::Onion {
                pk: *pk,
                port: msg.port,
            }),
            AddrV2::Cjdns(ip) if is_cjdns_ip(*ip) => Some(NetAddr::Cjdns {
                ip: *ip,
                port: msg.port,
            }),
            AddrV2::TorV2(_) | AddrV2::Cjdns(_) | AddrV2::Unknown(_, _) => None,
        }
    }

    pub fn to_addrv2(self) -> AddrV2 {
        match self {
            NetAddr::Ip(s) => match s.ip() {
                IpAddr::V4(v4) => AddrV2::Ipv4(v4),
                IpAddr::V6(v6) => AddrV2::Ipv6(v6),
            },
            NetAddr::Onion { pk, .. } => AddrV2::TorV3(pk),
            NetAddr::I2p { dest, .. } => AddrV2::I2p(dest),
            NetAddr::Cjdns { ip, .. } => AddrV2::Cjdns(ip),
            NetAddr::Name { .. } => AddrV2::Unknown(u8::MAX, Vec::new()),
        }
    }

    pub fn name(host: &str, port: u16) -> Option<Self> {
        NameId::intern(host).map(|id| NetAddr::Name { id, port })
    }

    /// [`Self::name`] for a name that must already be known: a question about
    /// it does not take an interner slot.
    pub fn known_name(host: &str, port: u16) -> Option<Self> {
        NameId::get(host).map(|id| NetAddr::Name { id, port })
    }

    pub fn from_socket(s: SocketAddr) -> Self {
        match s.ip() {
            IpAddr::V6(v) if is_cjdns_ip(v) => NetAddr::Cjdns {
                ip: v,
                port: s.port(),
            },
            _ => NetAddr::Ip(s),
        }
    }

    pub fn socket_addr(self) -> Option<SocketAddr> {
        match self {
            NetAddr::Ip(s) => Some(s),
            NetAddr::Cjdns { ip, port } => Some(SocketAddr::from((ip, port))),
            NetAddr::Onion { .. } | NetAddr::I2p { .. } | NetAddr::Name { .. } => None,
        }
    }

    /// Onion or I2P: an overlay row the dial picker keeps a slot for.
    pub fn is_overlay(self) -> bool {
        matches!(self, NetAddr::Onion { .. } | NetAddr::I2p { .. })
    }

    pub fn is_ipv6(self) -> bool {
        match self {
            NetAddr::Ip(s) => s.is_ipv6(),
            NetAddr::Cjdns { .. } => true,
            NetAddr::Onion { .. } | NetAddr::I2p { .. } | NetAddr::Name { .. } => false,
        }
    }

    pub fn port(self) -> u16 {
        match self {
            NetAddr::Ip(s) => s.port(),
            NetAddr::Onion { port, .. }
            | NetAddr::I2p { port, .. }
            | NetAddr::Cjdns { port, .. }
            | NetAddr::Name { port, .. } => port,
        }
    }

    pub fn host_str(self) -> String {
        match self {
            NetAddr::Ip(s) => s.ip().to_string(),
            NetAddr::Cjdns { ip, .. } => ip.to_string(),
            NetAddr::Onion { pk, .. } => format!("{}.onion", encode_onion_name(&pk)),
            NetAddr::I2p { dest, .. } => format!("{}{I2P_SUFFIX}", encode_i2p_name(&dest)),
            NetAddr::Name { id, .. } => id.host(),
        }
    }

    pub fn network_label(self) -> &'static str {
        OnlyNet::of(self).label()
    }
}

#[doc(hidden)]
pub fn parse_net_addr(s: &str) -> Result<NetAddr, NetError> {
    let Some((host, port_s)) = s.rsplit_once(':') else {
        return Err(NetError::Encode(format!("bad peer address {s}")));
    };
    if let Some(name) = strip_i2p_suffix(host) {
        let port: u16 = port_s
            .parse()
            .map_err(|_| NetError::Encode(format!("bad i2p port {s}")))?;
        let dest = decode_i2p_name(name)
            .ok_or_else(|| NetError::Encode(format!("bad i2p address {s}")))?;
        return Ok(NetAddr::I2p { dest, port });
    }
    if let Some(name) = strip_onion_suffix(host) {
        let port: u16 = port_s
            .parse()
            .map_err(|_| NetError::Encode(format!("bad onion port {s}")))?;
        let pk = decode_onion_name(name)
            .ok_or_else(|| NetError::Encode(format!("bad onion address {s}")))?;
        return Ok(NetAddr::Onion { pk, port });
    }
    s.parse()
        .map(NetAddr::from_socket)
        .map_err(|_| NetError::Encode(format!("bad peer address {s}")))
}

#[doc(hidden)]
pub fn strip_i2p_suffix(host: &str) -> Option<&str> {
    let b = host.as_bytes();
    let suf = I2P_SUFFIX.as_bytes();
    if b.len() <= suf.len() {
        return None;
    }
    if !b[b.len() - suf.len()..].eq_ignore_ascii_case(suf) {
        return None;
    }
    Some(&host[..host.len() - suf.len()])
}

#[doc(hidden)]
pub fn encode_i2p_name(dest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(I2P_B32_LEN);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in dest {
        // The shift cleared the low 8 bits, so adding the byte is the same as or.
        acc = (acc << 8) + u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[doc(hidden)]
pub fn decode_i2p_name(name: &str) -> Option<[u8; 32]> {
    if name.len() != I2P_B32_LEN {
        return None;
    }
    let mut out = [0u8; 32];
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut n = 0usize;
    for c in name.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => 26 + (c - b'2'),
            _ => return None,
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if n >= 32 {
                return None;
            }
            out[n] = (acc >> bits) as u8;
            n += 1;
        }
    }
    if n != 32 {
        return None;
    }
    Some(out)
}

#[doc(hidden)]
pub fn strip_onion_suffix(host: &str) -> Option<&str> {
    let b = host.as_bytes();
    if b.len() < 6 {
        return None;
    }
    if !b[b.len() - 6..].eq_ignore_ascii_case(b".onion") {
        return None;
    }
    Some(&host[..host.len() - 6])
}

#[doc(hidden)]
pub fn onion_checksum(pk: &[u8; 32]) -> [u8; 2] {
    let mut h = Sha3_256::new();
    h.update(b".onion checksum");
    h.update(pk);
    h.update([ONION_VERSION]);
    let d = h.finalize();
    [d[0], d[1]]
}

#[doc(hidden)]
pub fn encode_onion_name(pk: &[u8; 32]) -> String {
    let mut payload = [0u8; 35];
    payload[..32].copy_from_slice(pk);
    let sum = onion_checksum(pk);
    payload[32..34].copy_from_slice(&sum);
    payload[34] = ONION_VERSION;
    b32_encode(&payload)
}

#[doc(hidden)]
pub fn decode_onion_name(name: &str) -> Option<[u8; 32]> {
    if name.len() != ONION_NAME_LEN {
        return None;
    }
    let payload = b32_decode(name)?;
    if payload[34] != ONION_VERSION {
        return None;
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&payload[..32]);
    if onion_checksum(&pk) != [payload[32], payload[33]] {
        return None;
    }
    Some(pk)
}

#[doc(hidden)]
pub fn b32_encode(bytes: &[u8; 35]) -> String {
    let mut out = String::with_capacity(ONION_NAME_LEN);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((acc >> bits) & 31) as usize] as char);
        }
    }
    out
}

#[doc(hidden)]
pub fn b32_decode(s: &str) -> Option<[u8; 35]> {
    let mut out = [0u8; 35];
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut n = 0usize;
    for c in s.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => 26 + (c - b'2'),
            _ => return None,
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if n >= 35 {
                return None;
            }
            out[n] = (acc >> bits) as u8;
            n += 1;
        }
    }
    if n != 35 || bits != 0 {
        return None;
    }
    Some(out)
}
