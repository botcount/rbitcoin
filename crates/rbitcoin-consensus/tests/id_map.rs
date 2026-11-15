use rbitcoin_consensus::id_map::*;
use rbitcoin_primitives::Fk;
use std::hash::Hasher;

#[test]
fn txid_hasher_uses_first_8_bytes() {
    let mut h = TxidHasher::default();
    let mut t = [0u8; 32];
    t[..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    t[8] = 0xff;
    h.write(&t);
    assert_eq!(h.finish(), 0x0102_0304_0506_0708);
    let mut other = [0u8; 32];
    other[..8].copy_from_slice(&t[..8]);
    other[31] = 1;
    let mut h2 = TxidHasher::default();
    h2.write(&other);
    assert_eq!(
        h2.finish(),
        h.finish(),
        "same prefix must hash equal; full-key eq still separates them"
    );
    let mut m = IdMap::default();
    m.insert(t, (Fk(1), (0, 1)));
    m.insert(other, (Fk(2), (0, 1)));
    assert_eq!(m.get(&t).map(|v| v.0), Some(Fk(1)));
    assert_eq!(m.get(&other).map(|v| v.0), Some(Fk(2)));
}

#[test]
fn txid_hasher_places_a_short_write_in_little_endian_bytes() {
    let mut h = TxidHasher::default();
    h.write(&[0x11, 0x22, 0x33]);
    assert_eq!(h.finish(), 0x11 | (0x22 << 8) | (0x33 << 16));
}

#[test]
fn outpoint_hasher_mixes_vout() {
    use std::collections::HashSet;
    use std::hash::{BuildHasher, BuildHasherDefault};
    let mut prefix = [0u8; 32];
    prefix[..8].copy_from_slice(&0x1111_2222_3333_4444u64.to_le_bytes());
    let mut other = prefix;
    other[8] = 0xaa;
    let build = BuildHasherDefault::<OutPointHasher>::default();
    let hash_of = |txid: [u8; 32], vout: u32| build.hash_one((txid, vout));
    assert_ne!(
        hash_of(prefix, 0),
        hash_of(prefix, 1),
        "same txid prefix, vout 0 vs 1 must hash distinct"
    );
    assert_ne!(
        hash_of(prefix, 0),
        hash_of(other, 0),
        "different txid, same vout must hash distinct (TxidHasher overwrites with vout)"
    );
    type Set = HashSet<([u8; 32], u32), BuildHasherDefault<OutPointHasher>>;
    let mut s: Set = HashSet::with_hasher(BuildHasherDefault::default());
    assert!(s.insert((prefix, 0)));
    assert!(
        s.insert((prefix, 1)),
        "same txid prefix, vout 0 vs 1 must be distinct"
    );
    assert_eq!(s.len(), 2);
    assert!(
        s.insert((other, 0)),
        "different txid, same vout must stay distinct"
    );
    assert_eq!(s.len(), 3);
}
