use rbitcoin_store::bdz::*;
use rbitcoin_store::error::StoreError;

#[test]
fn read_packed_zero_modulus_or_vertices_is_corrupt() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz-zero-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("z.bdz");
    let mut hdr = vec![0u8; 32];
    hdr[0..4].copy_from_slice(b"BDZ2");
    hdr[4..8].copy_from_slice(&1u32.to_le_bytes());
    hdr[8..12].copy_from_slice(&2u32.to_le_bytes());
    hdr[12..16].copy_from_slice(&4u32.to_le_bytes());
    hdr[16..24].copy_from_slice(&1u64.to_le_bytes());
    hdr[24..28].copy_from_slice(&0u32.to_le_bytes());
    hdr[28..32].copy_from_slice(&1u32.to_le_bytes());
    hdr.extend_from_slice(&[0u8; 8]);
    std::fs::write(&path, &hdr).unwrap();
    match BdzMphf::read_packed_from(&path) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("modulus"), "{m}"),
        other => panic!("zero modulus must be Corrupt, got {other:?}"),
    }
    hdr[12..16].copy_from_slice(&0u32.to_le_bytes());
    hdr[24..28].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &hdr).unwrap();
    match BdzMphf::read_packed_from(&path) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("modulus"), "{m}"),
        other => panic!("zero vertex count must be Corrupt, got {other:?}"),
    }
    // n == 1 does not use the modulus. A `>` → `>=` gate would reject it.
    hdr[8..12].copy_from_slice(&1u32.to_le_bytes());
    hdr[12..16].copy_from_slice(&0u32.to_le_bytes());
    hdr[24..28].copy_from_slice(&0u32.to_le_bytes());
    std::fs::write(&path, &hdr).unwrap();
    let one = BdzMphf::read_packed_from(&path).expect("n=1 ignores a zero modulus");
    assert_eq!(one.index(9).unwrap(), one.index(9).unwrap());
    let zero_mod = BdzMphf {
        n: 2,
        m: 4,
        seed: 1,
        modulus: 0,
        g: GStore::Ram(vec![0u32; 4].into_boxed_slice()),
        compact: None,
    };
    match zero_mod.index_batch(&[1], &mut rbitcoin_store::IoCtx::none()) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("modulus"), "{m}"),
        other => panic!("index with modulus 0 must be Corrupt, got {other:?}"),
    }
    let zero_m = BdzMphf {
        n: 2,
        m: 0,
        seed: 1,
        modulus: 1,
        g: GStore::Ram(vec![0u32; 1].into_boxed_slice()),
        compact: None,
    };
    match zero_m.index_batch(&[1], &mut rbitcoin_store::IoCtx::none()) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("modulus"), "{m}"),
        other => panic!("index with m 0 must be Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bdz_injective_10k() {
    let keys: Vec<u64> = (0..10_000u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(7))
        .collect();
    let f = BdzMphf::build(&keys).unwrap();
    let mut seen = vec![false; keys.len()];
    for &k in &keys {
        let i = f.index(k).unwrap() as usize;
        assert!(i < keys.len());
        assert!(!seen[i], "collision at {i}");
        seen[i] = true;
    }
    assert!(seen.iter().all(|&b| b));
    let miss = f.index(0xDEAD_BEEF_u64).unwrap();
    assert!(miss < keys.len() as u32);
}

#[test]
fn bdz_injective_2k() {
    let keys: Vec<u64> = (0..2_000u64)
        .map(|i| i.wrapping_mul(0xD1B5_4A32_D192_ED03).wrapping_add(11))
        .collect();
    let f = BdzMphf::build(&keys).unwrap();
    let mut seen = vec![false; keys.len()];
    for &k in &keys {
        let i = f.index(k).unwrap() as usize;
        assert!(i < keys.len());
        assert!(!seen[i], "collision at {i}");
        seen[i] = true;
    }
    assert!(seen.iter().all(|&b| b));
}

#[test]
fn bdz_empty_and_one() {
    let z = BdzMphf::build(&[]).unwrap();
    assert_eq!(z.n(), 0);
    let one = BdzMphf::build(&[42]).unwrap();
    assert_eq!(one.index(42).unwrap(), 0);
    assert_eq!(one.index(99).unwrap(), 0);
}

#[test]
fn assigned_peel_index_is_rel_minus_one() {
    let keys = [10u64, 20, 30, 40];
    let rels = [3u32, 1, 4, 2];
    let values: Vec<u32> = rels.iter().map(|r| r - 1).collect();
    let f = BdzMphf::build_assigned(&keys, &values, 4).unwrap();
    assert_eq!(f.modulus(), 4);
    for (&k, &rel) in keys.iter().zip(rels.iter()) {
        assert_eq!(f.index(k).unwrap(), rel - 1);
    }
    let miss = f.index(0xDEAD_BEEF_u64).unwrap();
    assert!(miss < 4);
}

#[test]
fn assigned_peel_modulus_hole_and_bip30_newest() {
    let keys = [1u64, 2, 3];
    let values = [0u32, 2, 3];
    let modulus = 4u32;
    let f = BdzMphf::build_assigned(&keys, &values, modulus).unwrap();
    assert_eq!(f.n(), 3);
    assert_eq!(f.modulus(), modulus);
    assert_eq!(f.index(1).unwrap(), 0);
    assert_eq!(f.index(2).unwrap(), 2);
    assert_eq!(f.index(3).unwrap(), 3);
    let miss = f.index(99).unwrap();
    assert!(miss < modulus);

    let newest_key = 7u64;
    let newest_rel = 5u32;
    let f2 = BdzMphf::build_assigned(&[newest_key, 8], &[newest_rel - 1, 0], 5).unwrap();
    assert_eq!(f2.index(newest_key).unwrap(), newest_rel - 1);
    assert_eq!(f2.index(8).unwrap(), 0);
    assert!(f2.index(123).unwrap() < 5);
}

#[test]
fn assigned_peel_2k_permutation() {
    let n = 2_000u32;
    let keys: Vec<u64> = (0..n as u64)
        .map(|i| i.wrapping_mul(0xD1B5_4A32_D192_ED03).wrapping_add(11))
        .collect();
    let values: Vec<u32> = (0..n).map(|i| (i * 17 + 3) % n).collect();
    let mut seen = vec![false; n as usize];
    for &v in &values {
        assert!(!seen[v as usize], "test permutation");
        seen[v as usize] = true;
    }
    let f = BdzMphf::build_assigned(&keys, &values, n).unwrap();
    for (&k, &want) in keys.iter().zip(values.iter()) {
        assert_eq!(f.index(k).unwrap(), want);
    }
}

#[test]
fn g_bits_for_modulus_width() {
    assert_eq!(g_bits_for_modulus(1), 1);
    assert_eq!(g_bits_for_modulus(2), 1);
    assert_eq!(g_bits_for_modulus(1 << 24), 24);
    assert_eq!(g_bits_for_modulus(1 << 25), 25);
}

#[test]
fn pack_g_roundtrip_bits() {
    for g_bits in [1u32, 2, 24, 25, 26, 32] {
        let g: Vec<u32> = (0..64).map(|i| i % (1u32 << (g_bits.min(5)))).collect();
        let packed = pack_g(&g, g_bits);
        for (v, &want) in g.iter().enumerate() {
            assert_eq!(
                unpack_g_at(&packed, v as u32, g_bits),
                want,
                "g_bits={g_bits} v={v}"
            );
        }
    }
}

#[test]
fn unpack_g_bits_from_page_window_matches_unpack_g_at() {
    let g_bits = 25u32;
    let n = 1_400u32;
    let g: Vec<u32> = (0..n).map(|i| i.wrapping_mul(17)).collect();
    let packed = pack_g(&g, g_bits);
    assert!(packed.len() > G_PAGE_BYTES);
    let page0 = &packed[..G_PAGE_BYTES];
    let page1 = &packed[G_PAGE_BYTES..];
    let straddle = (0..n)
        .find(|&v| vertex_straddles_page(v, g_bits))
        .expect("25-bit field must cross a 4 KiB page");
    assert_eq!(
        unpack_g_bits_from_page_window(page0, Some(page1), 0, straddle, g_bits),
        unpack_g_at(&packed, straddle, g_bits),
    );
    let on_p0 = 0u32;
    assert!(!vertex_straddles_page(on_p0, g_bits));
    assert_eq!(
        unpack_g_bits_from_page_window(page0, None, 0, on_p0, g_bits),
        unpack_g_at(&packed, on_p0, g_bits),
    );
    let on_p1 = (0..n)
        .find(|&v| {
            let start = u64::from(v) * u64::from(g_bits);
            (start / 8 / G_PAGE_BYTES as u64) == 1 && !vertex_straddles_page(v, g_bits)
        })
        .expect("vertex contained in page 1");
    assert_eq!(
        unpack_g_bits_from_page_window(page1, None, 1, on_p1, g_bits),
        unpack_g_at(&packed, on_p1, g_bits),
    );
}

#[test]
fn assigned_packed_fd_matches_ram_and_straddles() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let n = 1_200u32;
    let modulus = 1u32 << 25;
    let keys: Vec<u64> = (0..n as u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(13))
        .collect();
    let values: Vec<u32> = (0..n).map(|i| i * 17 + 3).collect();
    let ram = BdzMphf::build_assigned(&keys, &values, modulus).unwrap();
    let p = dir.join("t.mphf");
    ram.write_packed_to(&p).unwrap();
    let raw = std::fs::read(&p).unwrap();
    assert_eq!(&raw[0..4], b"BDZ2");
    let fd = BdzMphf::read_packed_from(&p).unwrap();
    assert_eq!(fd.g_bytes_resident(), 0);
    assert_eq!(fd.modulus(), modulus);
    for &k in &keys {
        assert_eq!(ram.index(k).unwrap(), fd.index(k).unwrap());
    }
    let miss = 0xDEAD_BEEF_u64;
    assert_eq!(ram.index(miss).unwrap(), fd.index(miss).unwrap());
    assert!(fd.index(miss).unwrap() < modulus);

    let mut straddle_key = None;
    for &k in &keys {
        let verts = ram.vertices(k);
        let g_bits = g_bits_for_modulus(modulus);
        if verts.iter().any(|&v| vertex_straddles_page(v, g_bits)) {
            straddle_key = Some(k);
            break;
        }
    }
    let k = straddle_key.expect("expected a page-straddling vertex at 25-bit width");
    let verts = fd.vertices(k);
    let g_bits = g_bits_for_modulus(modulus);
    let mut pages: Vec<u32> = verts
        .iter()
        .copied()
        .flat_map(|v| vertex_pages(v, g_bits))
        .collect();
    pages.sort_unstable();
    pages.dedup();
    let _ = fd.take_g_page_preads();
    assert_eq!(fd.index(k).unwrap(), ram.index(k).unwrap());
    assert_eq!(fd.take_g_page_preads(), pages.len() as u64);
    assert!(
        pages.len() >= 2,
        "straddle vertex must include both page sides"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn assigned_packed_fd_index_batch_matches_ram() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz2-batch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let n = 1_200u32;
    let modulus = 1u32 << 25;
    let keys: Vec<u64> = (0..n as u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(13))
        .collect();
    let values: Vec<u32> = (0..n).map(|i| i * 17 + 3).collect();
    let ram = BdzMphf::build_assigned(&keys, &values, modulus).unwrap();
    let p = dir.join("t.mphf");
    ram.write_packed_to(&p).unwrap();
    let fd = BdzMphf::read_packed_from(&p).unwrap();
    let g_bits = g_bits_for_modulus(modulus);
    assert!(
        keys.iter().any(|&k| ram
            .vertices(k)
            .iter()
            .any(|&v| vertex_straddles_page(v, g_bits))),
        "batch must include a page-straddling vertex"
    );
    let mut want: Vec<u32> = keys.iter().map(|&k| ram.index(k).unwrap()).collect();
    let miss = 0xDEAD_BEEF_u64;
    want.push(ram.index(miss).unwrap());
    let mut batch_keys = keys.clone();
    batch_keys.push(miss);
    let got = fd
        .index_batch(&batch_keys, &mut rbitcoin_store::IoCtx::none())
        .unwrap();
    assert_eq!(got, want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn packed_g_scatter_reversed_pages_match_forward() {
    let g_bits = 25u32;
    let g: Vec<u32> = (0..3_000u32)
        .map(|i| i.wrapping_mul(17).wrapping_add(3))
        .collect();
    let packed = pack_g(&g, g_bits);
    assert!(
        packed.len() > G_PAGE_BYTES * 2,
        "need ≥3 g pages so fill order is not sequential-adjacent only"
    );
    assert!(
        (0..g.len() as u32).any(|v| vertex_straddles_page(v, g_bits)),
        "packed g must include a page-straddling vertex"
    );
    let pages: Vec<(u32, &[u8])> = packed
        .chunks(G_PAGE_BYTES)
        .enumerate()
        .map(|(i, c)| (i as u32, c))
        .collect();
    let verts: Vec<[u32; 3]> = (0..g.len() as u32).map(|v| [v, v, v]).collect();
    let want: Vec<[u32; 3]> = verts
        .iter()
        .map(|&[a, b, c]| {
            [
                unpack_g_at(&packed, a, g_bits),
                unpack_g_at(&packed, b, g_bits),
                unpack_g_at(&packed, c, g_bits),
            ]
        })
        .collect();
    assert_eq!(scatter_g_words(g_bits, &verts, &pages), want);
    let mut rev = pages.clone();
    rev.reverse();
    assert_eq!(scatter_g_words(g_bits, &verts, &rev), want);
    let mut rot = pages.clone();
    rot.rotate_left(1);
    assert_eq!(scatter_g_words(g_bits, &verts, &rot), want);
}

#[test]
fn assigned_packed_fd_index_batch_held_matches_ram() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz2-held-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let n = 1_200u32;
    let modulus = 1u32 << 25;
    let keys: Vec<u64> = (0..n as u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(13))
        .collect();
    let values: Vec<u32> = (0..n).map(|i| i * 17 + 3).collect();
    let ram = BdzMphf::build_assigned(&keys, &values, modulus).unwrap();
    let p = dir.join("t.mphf");
    ram.write_packed_to(&p).unwrap();
    let fd = BdzMphf::read_packed_from(&p).unwrap();
    let mut want: Vec<u32> = keys.iter().map(|&k| ram.index(k).unwrap()).collect();
    let miss = 0xDEAD_BEEF_u64;
    want.push(ram.index(miss).unwrap());
    let mut batch_keys = keys.clone();
    batch_keys.push(miss);
    let mut session = rbitcoin_store::uring_session::UringSession::try_open_kind(
        rbitcoin_store::uring_session::SessionKind::Pool,
        32,
    )
    .expect("pool");
    let got = {
        let mut ctx = rbitcoin_store::IoCtx::held(&mut session);
        fd.index_batch(&batch_keys, &mut ctx).unwrap()
    };
    session.drain_all().unwrap();
    assert_eq!(got, want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compact_injective_2k() {
    let keys: Vec<u64> = (0..2_000u64)
        .map(|i| i.wrapping_mul(0xD1B5_4A32_D192_ED03).wrapping_add(11))
        .collect();
    let f = BdzMphf::build_compact(&keys).unwrap();
    let mut seen = vec![false; keys.len()];
    for &k in &keys {
        let i = f.index(k).unwrap() as usize;
        assert!(i < keys.len());
        assert!(!seen[i], "collision at {i}");
        seen[i] = true;
    }
    assert!(seen.iter().all(|&b| b));
    let miss = f.index(0xDEAD_BEEF_u64).unwrap();
    assert!(miss < keys.len() as u32);
}

#[test]
fn compact_empty_and_one() {
    let z = BdzMphf::build_compact(&[]).unwrap();
    assert_eq!(z.n(), 0);
    let one = BdzMphf::build_compact(&[42]).unwrap();
    assert_eq!(one.index(42).unwrap(), 0);
    assert_eq!(one.index(99).unwrap(), 0);
}

#[test]
fn compact_packed_fd_is_bdz3_and_matches_ram() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz3-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let keys: Vec<u64> = (0..2_000u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(13))
        .collect();
    let ram = BdzMphf::build_compact(&keys).unwrap();
    let p = dir.join("t.mphf");
    ram.write_compact_to(&p).unwrap();
    let raw = std::fs::read(&p).unwrap();
    assert_eq!(&raw[0..4], b"BDZ3");
    assert_ne!(&raw[0..4], b"BDZ2");
    let g_bits = u32::from_le_bytes(raw[28..32].try_into().unwrap());
    assert_eq!(g_bits, 2);
    let fd = BdzMphf::read_compact_from(&p).unwrap();
    assert_eq!(fd.g_bytes_resident(), 0);
    let m = compact_vertex_count(keys.len() as u32);
    let occ_off = HEADER_LEN3 + packed_g_bytes(m, COMPACT_G_BITS);
    assert_ne!(
        occ_off % 8,
        0,
        "occ follows packed g; rank must popcount bytes, not &[u64]"
    );
    let n_supers = (m as usize).div_ceil(RANK_SUPER_BITS as usize);
    let supers_heap = (n_supers + 1) * 4;
    assert_eq!(
        fd.occ_bytes_resident(),
        supers_heap,
        "open must map occ; heap is supers only"
    );
    assert!(occ_packed_bytes(m) as usize > supers_heap);
    assert_eq!(raw.len() as u64, ram.trailer_off());
    assert_eq!(fd.trailer_off(), ram.trailer_off());
    for &k in &keys {
        assert_eq!(ram.index(k).unwrap(), fd.index(k).unwrap());
    }
    let miss = 0xDEAD_BEEF_u64;
    assert_eq!(ram.index(miss).unwrap(), fd.index(miss).unwrap());
    assert!(fd.index(miss).unwrap() < keys.len() as u32);
    let _ = fd.take_g_page_preads();
    assert_eq!(fd.index(keys[0]).unwrap(), ram.index(keys[0]).unwrap());
    let empty = dir.join("empty.mphf");
    std::fs::File::create(&empty).unwrap();
    match BdzMphf::read_compact_from(&empty) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("empty mphf must be Corrupt, got {other:?}"),
    }
    assert!(fd.take_g_page_preads() >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn packed_header_fields_survive_the_read() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz-hdr-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("h.bdz");
    let mut hdr = vec![0u8; 32];
    hdr[0..4].copy_from_slice(b"BDZ2");
    hdr[4..8].copy_from_slice(&1u32.to_le_bytes());
    hdr[8..12].copy_from_slice(&2u32.to_le_bytes());
    hdr[12..16].copy_from_slice(&4u32.to_le_bytes());
    hdr[16..24].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    hdr[24..28].copy_from_slice(&9u32.to_le_bytes());
    hdr[28..32].copy_from_slice(&8u32.to_le_bytes());
    hdr.extend(std::iter::repeat_n(0u8, packed_g_bytes(4, 8) as usize));
    std::fs::write(&path, &hdr).unwrap();
    let got = BdzMphf::read_packed_from(&path).unwrap();
    assert_eq!(got.n, 2);
    assert_eq!(got.m, 4);
    assert_eq!(got.seed, 0x0102_0304_0506_0708);
    assert_eq!(got.modulus, 9);
    let short = dir.join("short.bdz");
    std::fs::write(&short, &hdr[..31]).unwrap();
    match BdzMphf::read_packed_from(&short) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("short"), "{m}"),
        other => panic!("short packed header must be Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compact_header_fields_survive_the_read() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bdz3-hdr-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let keys = [1u64, 2, 3, 4, 5];
    let ram = BdzMphf::build_compact(&keys).unwrap();
    let path = dir.join("c.mphf");
    ram.write_compact_to(&path).unwrap();
    let got = BdzMphf::read_compact_from(&path).unwrap();
    assert_eq!(got.n, ram.n);
    assert_eq!(got.m, ram.m);
    assert_eq!(got.seed, ram.seed);
    assert_eq!(got.modulus, ram.modulus);
    for &k in &keys {
        assert_eq!(got.index(k).unwrap(), ram.index(k).unwrap());
    }
    let short = dir.join("short.mphf");
    std::fs::write(&short, [0u8; 31]).unwrap();
    match BdzMphf::read_compact_from(&short) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("g length"), "{m}"),
        other => panic!("short compact header must be Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
