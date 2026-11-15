use rbitcoin_primitives::{Fk, TableKind};
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::{TableFile, FILE_HEADER_LEN};
use rbitcoin_store::hashhead::{initial_slots_for, HashHead, HeadScale};
use rbitcoin_store::header_table::*;

fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("header");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn sample(hash: [u8; 32]) -> HeaderRecord {
    HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 100,
        bits: 0x1d00ffff,
        nonce: 7,
        merkle_root: [2u8; 32],
        hash,
        size: 0,
        weight: 0,
    }
}

#[test]
fn header_put_get_by_hash_open_flush() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let h1 = [1u8; 32];
    let h2 = [2u8; 32];
    let fk1 = t.ensure(&sample(h1)).unwrap();
    let fk2 = t.ensure(&sample(h2)).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get(fk1).unwrap().hash, h1);
    assert_eq!(t.get(fk2).unwrap().hash, h2);
    assert_eq!(t.get_by_hash(&h1).unwrap().unwrap().0, fk1);
    assert!(t.get_by_hash(&[9u8; 32]).unwrap().is_none());
    assert!(matches!(t.get(Fk::NULL), Err(StoreError::InvalidFk)));
    assert!(matches!(t.get(Fk(99)), Err(StoreError::NotFound)));
    assert!(matches!(
        HeaderRecord::decode(&[0u8; 10]),
        Err(StoreError::Corrupt(_))
    ));
    t.flush().unwrap();
    t.flush_async().unwrap();
    drop(t);
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get_by_hash(&h2).unwrap().unwrap().1.nonce, 7);
    // Shrink OS file below HWM so open clamps logical to a non-record size.
    {
        use rbitcoin_store::file::FILE_HEADER_LEN;
        let body = dir.join("header.body");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&body)
            .unwrap()
            .set_len((FILE_HEADER_LEN + 3) as u64)
            .unwrap();
    }
    assert!(matches!(
        HeaderTable::open_tiny(&dir),
        Err(StoreError::Corrupt(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Real block-header fields with PoW-style hash committed to a real parent.
fn linked_child(parent: &HeaderRecord, parent_fk: Fk, salt: u32) -> HeaderRecord {
    let version = 1;
    let timestamp = 1_700_000_000 + salt;
    let bits = 0x207fffff;
    let nonce = salt;
    let mut merkle = [0u8; 32];
    merkle[0..4].copy_from_slice(&salt.to_le_bytes());
    let hash = block_header_hash(version, &parent.hash, &merkle, timestamp, bits, nonce);
    HeaderRecord {
        prev_fk: parent_fk,
        version,
        timestamp,
        bits,
        nonce,
        merkle_root: merkle,
        hash,
        size: 0,
        weight: 0,
    }
}

#[test]
fn ensure_batch_linked_headers_roundtrip() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let g = sample([0x10; 32]);
    let a = linked_child(&g, Fk(1), 1);
    let b = linked_child(&a, Fk(2), 2);
    let fks = t.ensure_batch(&[g.clone(), a.clone(), b.clone()]).unwrap();
    assert_eq!(fks, vec![Fk(1), Fk(2), Fk(3)]);
    assert_eq!(t.count(), 3);
    assert_eq!(t.get_by_hash(&g.hash).unwrap().unwrap().0, Fk(1));
    assert_eq!(t.get_by_hash(&b.hash).unwrap().unwrap().0, Fk(3));
    drop(t);
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(t.count(), 3);
    assert_eq!(t.get_by_hash(&a.hash).unwrap().unwrap().1.nonce, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_batch_duplicate_hash_is_one_row() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let g = sample([0x21; 32]);
    let fks = t.ensure_batch(&[g.clone(), g.clone()]).unwrap();
    assert_eq!(fks[0], fks[1]);
    assert_eq!(t.count(), 1);
    let again = t.ensure_batch(&[g]).unwrap();
    assert_eq!(again[0], fks[0]);
    assert_eq!(t.count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_batch_false_parent_writes_nothing() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let g = sample([0x31; 32]);
    let a = linked_child(&g, Fk(1), 1);
    let honest = linked_child(&a, Fk(2), 2);
    let mut lying = honest.clone();
    lying.prev_fk = Fk(1);
    let err = t.ensure_batch(&[g.clone(), a.clone(), lying]).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(_)),
        "false parent edge must be rejected, got {err}"
    );
    assert_eq!(t.count(), 0, "failed batch must not append");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_batch_parent_in_batch_assigns_prev_fk() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let g = sample([0x41; 32]);
    let a = linked_child(&g, Fk(1), 7);
    let fks = t.ensure_batch(&[g, a.clone()]).unwrap();
    assert_eq!(fks, vec![Fk(1), Fk(2)]);
    assert_eq!(t.get(Fk(2)).unwrap().prev_fk, Fk(1));
    assert_eq!(t.get(Fk(2)).unwrap().hash, a.hash);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Production failure shape: same full hash as an older block, but `prev_fk`
/// points at a tip-extension header. Without the write gate this plants a
/// false child edge that resume walks as "headers past tip".
#[test]
fn ensure_rejects_duplicate_hash_with_divergent_prev_and_false_parent_edge() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();

    // G (null prev, synthetic hash) → A → B → C (real linked hashes).
    let g = sample([0x11; 32]);
    let g_fk = t.ensure(&g).unwrap();
    let a = linked_child(&g, g_fk, 1);
    let a_fk = t.ensure(&a).unwrap();
    let b = linked_child(&a, a_fk, 2);
    let b_fk = t.ensure(&b).unwrap();
    let c = linked_child(&b, b_fk, 3);
    let c_fk = t.ensure(&c).unwrap();
    assert_eq!(t.count(), 4);

    // Poison: re-insert G's identity with prev_fk = C (false parent).
    let mut poison = g.clone();
    poison.prev_fk = c_fk;
    let again = t.ensure(&poison).unwrap();
    assert_eq!(again, g_fk, "same hash must not create a second row");
    assert_eq!(t.count(), 4, "duplicate hash must not grow the table");
    assert_eq!(t.get(g_fk).unwrap().prev_fk, Fk::NULL);

    // First-time insert of a header whose hash commits to A as parent, but
    // caller lies with prev_fk = C → corrupt (false parent edge).
    let honest = linked_child(&a, a_fk, 99);
    let mut lying = honest.clone();
    lying.prev_fk = c_fk;
    let err = t.ensure(&lying).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(_)),
        "false parent edge must be rejected at write gate, got {err}"
    );
    assert_eq!(t.count(), 4);

    let ok = t.ensure(&honest).unwrap();
    assert_eq!(t.count(), 5);
    assert_eq!(t.get(ok).unwrap().prev_fk, a_fk);

    // Children of C: only what truly points at C (none of the poisons).
    let mut kids_of_c = 0u32;
    for id in 1..=t.count() {
        let rec = t.get(Fk(id)).unwrap();
        if rec.prev_fk == c_fk {
            kids_of_c += 1;
        }
    }
    assert_eq!(
        kids_of_c, 0,
        "C must not gain false children from poison puts"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn body_scan_stops_at_the_callers_count_and_bits_match_rows() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let bits = [0x207f_ffff, 0x1e0f_ffff, 0x1d00_ffff];
    for (i, b) in bits.into_iter().enumerate() {
        let mut rec = sample([i as u8 + 1; 32]);
        rec.bits = b;
        t.ensure(&rec).unwrap();
    }
    let mut seen = Vec::new();
    t.for_each_record_through(2, |fk, rec| {
        seen.push((fk.0, rec.bits));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, vec![(1, bits[0]), (2, bits[1])]);
    let got = t.bits_in_fk_order().unwrap();
    assert_eq!(got, bits);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_head_rolls_generation_when_gen0_is_full() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let mut hashes = Vec::new();
    for i in 0u32..80 {
        let mut hash = [0u8; 32];
        hash[0..4].copy_from_slice(&i.to_le_bytes());
        hash[4] = 0xa5;
        hashes.push(hash);
        t.ensure(&sample(hash)).unwrap();
    }
    assert!(
        dir.join("header.head.g1").is_file(),
        "tiny 64-slot gen0 must roll header.head.g1"
    );
    assert!(dir.join("header.head").is_file());
    let first = t.get_by_hash(&hashes[0]).unwrap().unwrap();
    let last = t.get_by_hash(&hashes[79]).unwrap().unwrap();
    assert_eq!(first.1.hash, hashes[0]);
    assert_eq!(last.1.hash, hashes[79]);
    assert_eq!(t.ensure(&sample(hashes[0])).unwrap(), first.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_batch_rolls_generation_when_gen0_is_full() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let recs: Vec<HeaderRecord> = (0u32..80)
        .map(|i| {
            let mut hash = [0u8; 32];
            hash[0..4].copy_from_slice(&i.to_le_bytes());
            hash[4] = 0xa5;
            sample(hash)
        })
        .collect();
    let fks = t.ensure_batch(&recs).unwrap();
    assert_eq!(fks.len(), 80);
    assert_eq!(t.count(), 80);
    assert!(
        dir.join("header.head.g1").is_file(),
        "tiny 64-slot gen0 must roll header.head.g1"
    );
    assert_eq!(t.get_by_hash(&recs[0].hash).unwrap().unwrap().0, Fk(1));
    assert_eq!(t.get_by_hash(&recs[79].hash).unwrap().unwrap().0, Fk(80));
    drop(t);
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(
        t.get_by_hash(&recs[40].hash).unwrap().unwrap().1.hash,
        recs[40].hash
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_head_open_grows_undersized_single_gen() {
    let dir = tmp();
    let hashes: Vec<[u8; 32]> = (0u32..5)
        .map(|i| {
            let mut h = [0u8; 32];
            h[0..4].copy_from_slice(&i.to_le_bytes());
            h[8] = 0x3c;
            h
        })
        .collect();
    {
        let body = TableFile::create(dir.join("header.body"), TableKind::Header).unwrap();
        for (i, hash) in hashes.iter().enumerate() {
            let rec = sample(*hash);
            let off = FILE_HEADER_LEN as u64 + (i as u64) * HEADER_RECORD_LEN as u64;
            body.write_at(off, &rec.encode()).unwrap();
        }
        body.set_logical_len(FILE_HEADER_LEN as u64 + 5 * HEADER_RECORD_LEN as u64)
            .unwrap();
        body.flush().unwrap();
        let h = HashHead::create_with_slots(dir.join("header.head"), 32).unwrap();
        for (i, hash) in hashes.iter().enumerate() {
            h.insert(hash, Fk(i as u64 + 1)).unwrap();
        }
        h.flush().unwrap();
        assert_eq!(h.slots(), 32);
    }
    #[cfg(unix)]
    let old = std::fs::File::open(dir.join("header.head")).unwrap();
    #[cfg(unix)]
    let old_ino = {
        use std::os::unix::fs::MetadataExt;
        old.metadata().unwrap().ino()
    };
    let t = HeaderTable::open_tiny(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            std::fs::metadata(dir.join("header.head")).unwrap().ino(),
            old_ino,
            "HeaderTable::open must replace undersized header.head, not punch it"
        );
        drop(old);
    }
    for hash in &hashes {
        assert_eq!(t.get_by_hash(hash).unwrap().unwrap().1.hash, *hash);
    }
    for i in 5u32..40 {
        let mut hash = [0u8; 32];
        hash[0..4].copy_from_slice(&i.to_le_bytes());
        hash[8] = 0x3c;
        t.ensure(&sample(hash)).unwrap();
    }
    assert!(
        !dir.join("header.head.g1").is_file(),
        "open-grow to 64 slots must absorb 40 headers without rolling"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_head_empty_target_sized_gen0_with_body_is_layout_refuse() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    t.ensure(&sample([0x44; 32])).unwrap();
    t.flush().unwrap();
    drop(t);
    std::fs::remove_file(dir.join("header.head")).unwrap();
    let slots = initial_slots_for(HeadScale::Tiny);
    let staging = tmp();
    let h = HashHead::create_with_slots(staging.join("header.head"), slots).unwrap();
    h.flush().unwrap();
    drop(h);
    std::fs::rename(staging.join("header.head"), dir.join("header.head")).unwrap();
    let _ = std::fs::remove_dir_all(&staging);
    let err = match HeaderTable::open_tiny(&dir) {
        Err(e) => e,
        Ok(_) => panic!("expected Layout refuse for empty target-sized header.head"),
    };
    match err {
        StoreError::Layout(m) => {
            assert!(m.contains("header.head"), "{m}");
            assert!(m.contains("header.body"), "{m}");
            assert!(m.contains("header.head.mlt"), "{m}");
        }
        other => panic!("expected Layout, got {other}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_head_directory_is_layout_refuse() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    drop(t);
    let head = dir.join("header.head");
    std::fs::remove_file(&head).unwrap();
    std::fs::create_dir(&head).unwrap();
    std::fs::write(head.join("00"), b"x").unwrap();
    std::fs::write(head.join("01"), b"y").unwrap();
    let err = match HeaderTable::open_tiny(&dir) {
        Err(e) => e,
        Ok(_) => panic!("expected Layout refuse for sharded header.head dir"),
    };
    match err {
        StoreError::Layout(m) => {
            assert!(m.contains("header.head"), "{m}");
        }
        other => panic!("expected Layout, got {other}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ensure_same_hash_twice_is_idempotent() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    let h = sample([7u8; 32]);
    let fk1 = t.ensure(&h).unwrap();
    let mut h2 = h.clone();
    h2.prev_fk = Fk(999); // divergent prev ignored on hit
    let fk2 = t.ensure(&h2).unwrap();
    assert_eq!(fk1, fk2);
    assert_eq!(t.count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_record_roundtrip_is_88_bytes() {
    let rec = HeaderRecord {
        prev_fk: Fk(0x0807_0605_0403_0201),
        version: 0x0403_0201,
        timestamp: 0x0807_0605,
        bits: 0x0c0b_0a09,
        nonce: 0x100f_0e0d,
        merkle_root: std::array::from_fn(|i| i as u8),
        hash: std::array::from_fn(|i| (i as u8).wrapping_add(32)),
        size: 285,
        weight: 1140,
    };
    let enc = rec.encode();
    assert_eq!(enc.len(), HEADER_RECORD_LEN);
    let back = HeaderRecord::decode(&enc).unwrap();
    assert_eq!(back.size, 0);
    assert_eq!(back.weight, 0);
    assert_eq!(back.prev_fk, rec.prev_fk);
    assert_eq!(back.version, rec.version);
    assert_eq!(back.timestamp, rec.timestamp);
    assert_eq!(back.bits, rec.bits);
    assert_eq!(back.nonce, rec.nonce);
    assert_eq!(back.merkle_root, rec.merkle_root);
    assert_eq!(back.hash, rec.hash);
    assert!(matches!(
        HeaderRecord::decode(&[0u8; 40]),
        Err(StoreError::Corrupt(m)) if m == "short header record"
    ));
    let mut longer = [0u8; HEADER_RECORD_LEN_V24];
    longer[..HEADER_RECORD_LEN].copy_from_slice(&enc);
    let wide = HeaderRecord::decode(&longer).unwrap();
    assert_eq!(wide.hash, rec.hash);
    assert_eq!(wide.nonce, rec.nonce);
}

#[test]
fn rewrite_v24_header_body_strips_size_weight() {
    let dir = tmp();
    let rec = sample([0x11; 32]);
    {
        let body = TableFile::create(dir.join("header.body"), TableKind::Header).unwrap();
        let mut raw = [0u8; HEADER_RECORD_LEN_V24];
        raw[..HEADER_RECORD_LEN].copy_from_slice(&rec.encode());
        raw[88..92].copy_from_slice(&285u32.to_le_bytes());
        raw[92..96].copy_from_slice(&1140u32.to_le_bytes());
        body.write_at(FILE_HEADER_LEN as u64, &raw).unwrap();
        body.set_logical_len(FILE_HEADER_LEN as u64 + HEADER_RECORD_LEN_V24 as u64)
            .unwrap();
        body.flush().unwrap();
        let h = HashHead::create_with_slots(dir.join("header.head"), 64).unwrap();
        h.insert(&rec.hash, Fk(1)).unwrap();
        h.flush().unwrap();
    }
    HeaderTable::rewrite_v24_body_to_88(&dir).unwrap();
    let t = HeaderTable::open_tiny(&dir).unwrap();
    let got = t.get(Fk(1)).unwrap();
    assert_eq!(got.hash, rec.hash);
    assert_eq!(got.size, 0);
    assert_eq!(got.weight, 0);
    assert_eq!(t.count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_head_occupied_sums_base_and_gen() {
    let dir = tmp();
    assert_eq!(header_head_occupied(&dir).unwrap(), 0);
    {
        let h = HashHead::create_with_slots(dir.join("header.head"), 64).unwrap();
        h.insert(&[0x11; 32], Fk(1)).unwrap();
        h.flush().unwrap();
    }
    assert_eq!(header_head_occupied(&dir).unwrap(), 1);
    {
        let g = HashHead::create_with_slots(dir.join("header.head.g1"), 64).unwrap();
        g.insert(&[0x22; 32], Fk(2)).unwrap();
        g.insert(&[0x33; 32], Fk(3)).unwrap();
        g.flush().unwrap();
    }
    assert_eq!(header_head_occupied(&dir).unwrap(), 3);
    std::fs::write(dir.join("header.head"), [0u8; 3]).unwrap();
    assert!(header_head_occupied(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewrite_v24_already_88_is_nop() {
    let dir = tmp();
    let t = HeaderTable::create_tiny(&dir).unwrap();
    t.ensure(&sample([0x22; 32])).unwrap();
    t.flush().unwrap();
    drop(t);
    HeaderTable::rewrite_v24_body_to_88(&dir).unwrap();
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(t.count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 11×96 B is the same byte length as 12×88 B. Occupied 11 must strip.
#[test]
fn rewrite_v24_ambiguous_96_strips_each_row() {
    let dir = tmp();
    let n = 11u64;
    let recs: Vec<HeaderRecord> = (0..n).map(|i| sample([0xA0 + i as u8; 32])).collect();
    {
        let body = TableFile::create(dir.join("header.body"), TableKind::Header).unwrap();
        let mut blob = Vec::with_capacity(n as usize * HEADER_RECORD_LEN_V24);
        for rec in &recs {
            let mut raw = [0u8; HEADER_RECORD_LEN_V24];
            raw[..HEADER_RECORD_LEN].copy_from_slice(&rec.encode());
            raw[88..92].copy_from_slice(&7u32.to_le_bytes());
            blob.extend_from_slice(&raw);
        }
        body.write_at(FILE_HEADER_LEN as u64, &blob).unwrap();
        body.set_logical_len(FILE_HEADER_LEN as u64 + blob.len() as u64)
            .unwrap();
        body.flush().unwrap();
        let h = HashHead::create_with_slots(dir.join("header.head"), 64).unwrap();
        for (i, rec) in recs.iter().enumerate() {
            h.insert(&rec.hash, Fk(i as u64 + 1)).unwrap();
        }
        h.flush().unwrap();
    }
    HeaderTable::rewrite_v24_body_to_88(&dir).unwrap();
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(t.count(), n);
    for (i, rec) in recs.iter().enumerate() {
        let got = t.get(Fk(i as u64 + 1)).unwrap();
        assert_eq!(got.hash, rec.hash);
        assert_eq!(got.size, 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 12×88 B is the same byte length as 11×96 B. Occupied 12 must not strip.
#[test]
fn rewrite_v24_ambiguous_88_stays() {
    let dir = tmp();
    let n = 12u64;
    let recs: Vec<HeaderRecord> = (0..n).map(|i| sample([0xB0 + i as u8; 32])).collect();
    {
        let body = TableFile::create(dir.join("header.body"), TableKind::Header).unwrap();
        let mut blob = Vec::with_capacity(n as usize * HEADER_RECORD_LEN);
        for rec in &recs {
            blob.extend_from_slice(&rec.encode());
        }
        body.write_at(FILE_HEADER_LEN as u64, &blob).unwrap();
        body.set_logical_len(FILE_HEADER_LEN as u64 + blob.len() as u64)
            .unwrap();
        body.flush().unwrap();
        let h = HashHead::create_with_slots(dir.join("header.head"), 64).unwrap();
        for (i, rec) in recs.iter().enumerate() {
            h.insert(&rec.hash, Fk(i as u64 + 1)).unwrap();
        }
        h.flush().unwrap();
    }
    let before = std::fs::metadata(dir.join("header.body")).unwrap().len();
    HeaderTable::rewrite_v24_body_to_88(&dir).unwrap();
    assert_eq!(
        std::fs::metadata(dir.join("header.body")).unwrap().len(),
        before
    );
    let t = HeaderTable::open_tiny(&dir).unwrap();
    assert_eq!(t.count(), n);
    assert_eq!(t.get(Fk(n)).unwrap().hash, recs[n as usize - 1].hash);
    let _ = std::fs::remove_dir_all(&dir);
}
