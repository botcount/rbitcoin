use rbitcoin_net::asmap::*;
use std::net::IpAddr;
use std::net::Ipv4Addr;

#[test]
fn asmap_rejected_bytecodes_fail_sanity() {
    for bytes in [
        ASMAP_SECOND_DEFAULT,
        ASMAP_TRAILING_ONE,
        ASMAP_EIGHT_TRAILING_ZEROS,
        ASMAP_TWO_SHORT_MATCHES,
        ASMAP_MATCH_PAST_END,
        ASMAP_JUMP_PAST_FILE,
        ASMAP_JUMP_SHORT_TAIL,
        ASMAP_JUMP_OFF_END,
        &[],
        &[0],
    ] {
        assert!(
            AsMap::from_bytes(bytes.to_vec()).is_none(),
            "accepted {bytes:?}"
        );
    }
}

#[test]
fn asmap_two_prefix_const_matches_encoder() {
    assert_eq!(two_prefix_asmap_bytes(), TWO_PREFIX_ASMAP);
}

#[test]
fn asmap_fuzz_seed_fixture_is_two_prefix_then_ip16() {
    const SEED: &[u8] = include_bytes!("../tests/fixtures/asmap_two_prefix_plus_ip.bin");
    assert!(SEED.len() > 16, "asmap then leftover ip16");
    let (asmap, ip) = SEED.split_at(SEED.len() - 16);
    assert_eq!(asmap, TWO_PREFIX_ASMAP);
    let m = AsMap::from_bytes(asmap.to_vec()).expect("two-prefix");
    let mut ip16 = [0u8; 16];
    ip16.copy_from_slice(ip);
    assert_eq!(m.interpret_ip16(&ip16), 1);
    assert_eq!(ip16, ip16_for_lookup(IpAddr::V4(Ipv4Addr::new(1, 2, 0, 0))));
}

fn program(parts: &[Vec<bool>]) -> Vec<u8> {
    let mut bits = Vec::new();
    for part in parts {
        bits.extend(part);
    }
    pack_le(&bits)
}

fn instr_default(asn: u32) -> Vec<bool> {
    let mut out = instr_type(DEFAULT);
    out.extend(encode_bits(asn, 1, &ASN_BIT_SIZES));
    out
}

#[test]
fn asmap_loopback_reports_one_two_and_the_default() {
    let m = AsMap::from_bytes(LOOPBACK_ASMAP.to_vec()).expect("loopback asmap");
    let asn = |a, b, c, d| m.mapped_as(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
    assert_eq!(asn(127, 0, 0, 1), 1);
    assert_eq!(asn(127, 0, 0, 2), 2);
    assert_eq!(asn(127, 0, 0, 3), 9);
}

#[test]
fn asmap_jump_follows_the_ip_bit() {
    let ret1 = instr_return(1);
    assert_eq!(ret1.len(), 17);
    let bytes = program(&[instr_jump(ret1.len() as u32), ret1, instr_return(2)]);
    let m = AsMap::from_bytes(bytes).expect("jump program");
    let mut ip1 = [0u8; 16];
    ip1[0] = 0x80;
    assert_eq!(m.interpret_ip16(&[0u8; 16]), 1);
    assert_eq!(m.interpret_ip16(&ip1), 2);
}

#[test]
fn asmap_exact_tail_match_returns_the_default_on_a_miss() {
    let mut parts = vec![instr_default(5)];
    for _ in 0..15 {
        parts.push(instr_match(&[false; 8]));
    }
    parts.push(instr_match(&[true; 8]));
    parts.push(instr_return(4));
    let m = AsMap::from_bytes(program(&parts)).expect("tail match");
    assert_eq!(m.interpret_ip16(&[0u8; 16]), 5);
}

#[test]
fn asmap_return_of_three_decodes_as_three() {
    let bytes = pack_le(&instr_return(3));
    let mut pos = 0usize;
    assert_eq!(decode_type(&mut pos, &bytes), RETURN);
    assert_eq!(decode_asn(&mut pos, &bytes), 3);
}

#[test]
fn asmap_sixteen_byte_matches_fill_the_address() {
    let mut parts = Vec::new();
    for _ in 0..16 {
        parts.push(instr_match(&[false; 8]));
    }
    parts.push(instr_return(1));
    assert!(AsMap::from_bytes(program(&parts)).is_some());
}

#[test]
fn asmap_short_then_full_match_is_sane() {
    let parts = [
        instr_match(&[false; 6]),
        instr_match(&[false; 8]),
        instr_return(1),
    ];
    assert!(AsMap::from_bytes(program(&parts)).is_some());
}

#[test]
fn asmap_jump_past_an_earlier_target_fails_sanity() {
    let jump_bits = instr_jump(17);
    let bytes = pack_le(&jump_bits);
    let mut pos = 0usize;
    let mut bits = 128i32;
    let mut jumps = vec![(0usize, 10i32)];
    assert!(
        !sanity_jump(&mut pos, &bytes, &mut bits, &mut jumps),
        "a jump onto or past the previous target is rejected"
    );
}
