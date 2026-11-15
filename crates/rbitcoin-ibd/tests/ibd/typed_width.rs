use bitcoin::CompactTarget;
use rbitcoin_ibd::ibd::header_walk::*;

fn empty_adopt(base_height: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(ADOPT_MAGIC);
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&[0x11; 32]);
    buf.extend_from_slice(&42u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 32]);
    write_header(&mut buf, None);
    write_times(&mut buf, &[]);
    write_diff(
        &mut buf,
        &DiffSnap {
            period_header: None,
            period_height: 1,
            full_diff_bits: Some(CompactTarget::from_consensus(5)),
            full_diff_height: 2,
        },
    );
    buf.extend_from_slice(&[0u8; 32]);
    buf.extend_from_slice(&[0u8; 32]);
    buf.extend_from_slice(&base_height.to_le_bytes());
    buf.extend_from_slice(&[0u8; 32]);
    write_header(&mut buf, None);
    buf.extend_from_slice(&3u32.to_le_bytes());
    write_header(&mut buf, None);
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&6u32.to_le_bytes());
    write_diff(
        &mut buf,
        &DiffSnap {
            period_header: None,
            period_height: 9,
            full_diff_bits: Some(CompactTarget::from_consensus(7)),
            full_diff_height: 8,
        },
    );
    buf
}

#[test]
fn adopt_diff_and_times_roundtrip_and_reject_short() {
    let bytes = empty_adopt(7);
    let walk = parse_adopt(&bytes).expect("adopt");
    assert_eq!(walk.base_height, 7);
    assert_eq!(walk.checkpoints[0].height, 42);
    assert_eq!(walk.tip.diff.period_height, 3);
    assert_eq!(walk.tip.diff.full_diff_height, 4);
    assert_eq!(
        walk.tip.diff.full_diff_bits,
        Some(CompactTarget::from_consensus(6))
    );
    assert_eq!(walk.base_diff.period_height, 9);
    assert_eq!(walk.base_diff.full_diff_height, 8);
    assert_eq!(
        walk.base_diff.full_diff_bits,
        Some(CompactTarget::from_consensus(7))
    );
    assert!(parse_adopt(b"rbtchdr1").is_none());
    assert!(parse_adopt(&bytes[..20]).is_none());

    let diff = DiffSnap {
        period_header: None,
        period_height: 15,
        full_diff_bits: Some(CompactTarget::from_consensus(11)),
        full_diff_height: 16,
    };
    let mut raw = Vec::new();
    write_diff(&mut raw, &diff);
    assert_eq!(read_diff(&raw), Some(diff));
    assert!(read_diff(&raw[..4]).is_none());

    let mut times = Vec::new();
    write_times(&mut times, &[11, 22]);
    assert_eq!(read_times(&times), Some(vec![11, 22]));
    assert!(read_times(&[1]).is_none());
}
