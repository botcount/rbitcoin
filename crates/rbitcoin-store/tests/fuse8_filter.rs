use rbitcoin_store::binary_fuse8::{BinaryFuse8, Fingerprints};
use rbitcoin_store::error::StoreError;
use rbitcoin_store::fuse8_filter::*;
fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("fuse8");
    let _ = std::fs::create_dir_all(&p);
    p
}

#[test]
fn fuse8_segment_count_must_be_a_multiple_of_segment_length() {
    let segment_length = 4u32;
    let mut payload = vec![0u8; 28 + 14];
    payload[8..12].copy_from_slice(&segment_length.to_le_bytes());
    payload[12..16].copy_from_slice(&3u32.to_le_bytes());
    payload[16..20].copy_from_slice(&6u32.to_le_bytes());
    payload[20..28].copy_from_slice(&14u64.to_le_bytes());
    match parse_v2_geometry(&payload) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("multiple"), "{msg}"),
        Ok(_) => panic!("segment count 6 with length 4 was accepted"),
        Err(other) => panic!("{other}"),
    }
    let mut zero = vec![0u8; 28 + 8];
    zero[8..12].copy_from_slice(&segment_length.to_le_bytes());
    zero[12..16].copy_from_slice(&3u32.to_le_bytes());
    zero[20..28].copy_from_slice(&8u64.to_le_bytes());
    match parse_v2_geometry(&zero) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("multiple"), "{msg}"),
        Ok(_) => panic!("segment count 0 was accepted"),
        Err(other) => panic!("{other}"),
    }
    let dir = tmp();
    let path = dir.join("geom.fuse8");
    let mut raw = Vec::from(*MAGIC);
    raw.extend_from_slice(&VERSION_V2.to_le_bytes());
    raw.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    raw.extend_from_slice(&payload);
    std::fs::write(&path, &raw).unwrap();
    match open_file(&path) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("non-multiple geometry opened: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fuse8_segment_length_must_be_power_of_two() {
    let mut payload = vec![0u8; 8 + 4 + 4 + 4 + 8 + 13];
    payload[8..12].copy_from_slice(&6u32.to_le_bytes());
    payload[12..16].copy_from_slice(&5u32.to_le_bytes());
    payload[16..20].copy_from_slice(&1u32.to_le_bytes());
    payload[20..28].copy_from_slice(&13u64.to_le_bytes());
    match parse_v2_geometry(&payload) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("power of two"), "{msg}"),
        Ok(_) => panic!("segment length 6 was accepted"),
        Err(other) => panic!("{other}"),
    }
}

fn decode_body(payload: &[u8]) -> Result<BinaryFuse8, StoreError> {
    let geo = parse_v2_geometry(payload)?;
    let fingerprints = Fingerprints::Heap(
        payload[geo.fp_off..geo.fp_off + geo.fp_len]
            .to_vec()
            .into_boxed_slice(),
    );
    Ok(BinaryFuse8 {
        seed: geo.seed,
        segment_length: geo.segment_length,
        segment_length_mask: geo.segment_length_mask,
        segment_count_length: geo.segment_count_length,
        fingerprints,
    })
}

#[test]
fn empty_fuse_file_is_corrupt_empty() {
    let dir = tmp();
    let path = dir.join("empty.fuse8");
    std::fs::write(&path, b"").unwrap();
    match SealedFuse8::read_from(&path) {
        Err(StoreError::Corrupt(m)) => assert_eq!(m, "fuse8 file empty"),
        other => panic!("empty fuse must be Corrupt empty, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_false_negatives_and_roundtrip() {
    let keys: Vec<u64> = (0..10_000u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(7))
        .collect();
    let f = SealedFuse8::build(&keys).unwrap();
    for &k in &keys {
        assert!(f.contains(k), "FN on {k}");
    }
    let dir = tmp();
    let path = dir.join("seg.fuse8");
    f.write_to(&path).unwrap();
    let f2 = SealedFuse8::read_from(&path).unwrap();
    assert_eq!(
        f2.fingerprint_heap_bytes(),
        0,
        "open_file must map fingerprints, not copy them"
    );
    assert_eq!(f2.fingerprint_bytes(), f.fingerprint_bytes());
    for &k in &keys {
        assert!(f2.contains(k));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_build_and_bad_header_errors() {
    let empty = SealedFuse8::build(&[]).unwrap();
    let _ = empty.contains(0);
    assert!(empty.fingerprint_bytes() > 0);

    let dir = tmp();
    let path = dir.join("bad.fuse8");
    std::fs::write(
        &path,
        b"XXXX\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    )
    .unwrap();
    assert!(matches!(open_file(&path), Err(StoreError::Corrupt(_))));
    let mut bad_ver = Vec::from(*MAGIC);
    bad_ver.extend_from_slice(&99u32.to_le_bytes());
    bad_ver.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &bad_ver).unwrap();
    assert!(matches!(open_file(&path), Err(StoreError::Corrupt(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v1_opens_as_index_refuse() {
    let dir = tmp();
    let path = dir.join("legacy.fuse8");
    let mut raw = Vec::from(*MAGIC);
    raw.extend_from_slice(&VERSION_V1.to_le_bytes());
    raw.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    match open_file(&path) {
        Err(StoreError::Corrupt(m)) => {
            assert_eq!(m, INDEX_REFUSE_FUSE8_V1);
            assert!(m.contains("wipe store/tx.head"));
            assert!(m.contains("Class A kept"));
        }
        other => panic!("v1 must refuse, got {other:?}"),
    }
    match SealedFuse8::read_from(&path) {
        Err(StoreError::Corrupt(m)) => assert_eq!(m, INDEX_REFUSE_FUSE8_V1),
        other => panic!("read_from v1 must refuse, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v2_unreadable_body_is_corrupt() {
    let dir = tmp();
    let path = dir.join("badv2.fuse8");
    let mut raw = Vec::from(*MAGIC);
    raw.extend_from_slice(&VERSION_V2.to_le_bytes());
    raw.extend_from_slice(&4u64.to_le_bytes());
    raw.extend_from_slice(&[0u8; 4]);
    std::fs::write(&path, &raw).unwrap();
    match open_file(&path) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("unreadable"), "{m}"),
        other => panic!("short v2 body must be Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fuse8_wrapped_body_length_is_corrupt() {
    let dir = tmp();
    let path = dir.join("wrap.fuse8");
    let mut raw = Vec::from(*MAGIC);
    raw.extend_from_slice(&VERSION_V2.to_le_bytes());
    raw.extend_from_slice(&u64::MAX.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    match open_file(&path) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("unreadable"), "{m}"),
        other => panic!("wrapped body length must be Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decode_body_rejects_absurd_fingerprint_len() {
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes()); // seed
    body.extend_from_slice(&64u32.to_le_bytes()); // seg_len
    body.extend_from_slice(&63u32.to_le_bytes()); // mask
    body.extend_from_slice(&64u32.to_le_bytes()); // count_len
    body.extend_from_slice(&(600 * 1024 * 1024u64).to_le_bytes()); // absurd
                                                                   // no fingerprints
    assert!(decode_body(&body).is_err());
}

#[test]
fn decode_body_rejects_invalid_geometry_and_truncation() {
    assert!(decode_body(&[0u8; 10]).is_err());
    // Valid geometry header but fingerprints truncated.
    let mut body = Vec::new();
    body.extend_from_slice(&7u64.to_le_bytes());
    body.extend_from_slice(&64u32.to_le_bytes());
    body.extend_from_slice(&63u32.to_le_bytes());
    body.extend_from_slice(&64u32.to_le_bytes());
    body.extend_from_slice(&8u64.to_le_bytes()); // claims 8 fps
    body.extend_from_slice(&[1, 2, 3]); // only 3
    assert!(decode_body(&body).is_err());
    // segment_length == 0
    let mut bad_seg = Vec::new();
    bad_seg.extend_from_slice(&1u64.to_le_bytes());
    bad_seg.extend_from_slice(&0u32.to_le_bytes());
    bad_seg.extend_from_slice(&0u32.to_le_bytes());
    bad_seg.extend_from_slice(&0u32.to_le_bytes());
    bad_seg.extend_from_slice(&0u64.to_le_bytes());
    assert!(decode_body(&bad_seg).is_err());
    // mask != segment_length - 1
    let mut bad_mask = Vec::new();
    bad_mask.extend_from_slice(&1u64.to_le_bytes());
    bad_mask.extend_from_slice(&64u32.to_le_bytes());
    bad_mask.extend_from_slice(&0u32.to_le_bytes()); // wrong mask
    bad_mask.extend_from_slice(&64u32.to_le_bytes());
    bad_mask.extend_from_slice(&0u64.to_le_bytes());
    assert!(decode_body(&bad_mask).is_err());
}

#[test]
fn decode_body_rejects_fingerprint_len_below_hash_geometry() {
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&64u32.to_le_bytes());
    body.extend_from_slice(&63u32.to_le_bytes());
    body.extend_from_slice(&(64u32 * 156).to_le_bytes());
    body.extend_from_slice(&8u64.to_le_bytes());
    body.extend_from_slice(&[0u8; 8]);
    match decode_body(&body) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("hash geometry"), "{m}");
        }
        other => panic!("expected geometry refuse, got {other:?}"),
    }
    let ok = BinaryFuse8::try_from_keys(&[1u64, 2, 3]).unwrap();
    let encoded = encode_body(&ok);
    decode_body(&encoded).expect("honest filter must still decode");
}

#[test]
fn fuse_key_from_mixed_xors_quarters() {
    let m: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(1));
    assert_eq!(fuse_key_from_mixed(&m), 2_314_885_668_798_488_672);
    assert_eq!(fuse_key_from_mixed(&[0u8; 32]), 0);
}

#[test]
fn roundtrip_v2_write_read_and_fingerprint_bytes() {
    let dir = tmp();
    let path = dir.join("rt.fuse8");
    let gate = SealedFuse8::build(&[10u64, 20, 30, 40, 50]).unwrap();
    assert!(gate.fingerprint_bytes() > 0);
    assert!(gate.contains(10));
    assert!(gate.contains(50));
    gate.write_to(&path).unwrap();
    let g = open_file(&path).unwrap();
    assert!(g.contains(10));
    assert!(g.contains(50));
    // Empty keys path builds dummy.
    let empty = SealedFuse8::build(&[]).unwrap();
    assert!(empty.contains(0)); // dummy always built
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_unknown_version_is_corrupt() {
    let dir = tmp();
    let path = dir.join("v99.fuse8");
    let mut raw = Vec::from(*MAGIC);
    raw.extend_from_slice(&99u32.to_le_bytes());
    raw.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    assert!(open_file(&path).is_err());
    std::fs::write(
        &path,
        b"XXXX\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    )
    .unwrap();
    assert!(open_file(&path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v2_geometry_reads_each_fixed_field() {
    let mut payload = vec![0u8; 28 + 12];
    payload[0..8].copy_from_slice(&0x0807_0605_0403_0201u64.to_le_bytes());
    payload[8..12].copy_from_slice(&4u32.to_le_bytes());
    payload[12..16].copy_from_slice(&3u32.to_le_bytes());
    payload[16..20].copy_from_slice(&4u32.to_le_bytes());
    payload[20..28].copy_from_slice(&12u64.to_le_bytes());
    let geo = match parse_v2_geometry(&payload) {
        Ok(geo) => geo,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(geo.seed, 0x0807_0605_0403_0201);
    assert_eq!(geo.segment_length, 4);
    assert_eq!(geo.segment_length_mask, 3);
    assert_eq!(geo.segment_count_length, 4);
    assert_eq!(geo.fp_off, 28);
    assert_eq!(geo.fp_len, 12);
    match parse_v2_geometry(&payload[..27]) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("short"), "{msg}"),
        Ok(_) => panic!("short payload decoded"),
        Err(err) => panic!("{err}"),
    }
}
