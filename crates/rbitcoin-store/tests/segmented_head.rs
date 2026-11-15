use rbitcoin_primitives::Fk;
use rbitcoin_store::address_head::HeadLayout;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::fuse8_filter::fuse_key_from_mixed;
use rbitcoin_store::segmented_head::*;
use std::path::PathBuf;
use std::sync::Arc;

fn tmp() -> PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("seghead");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn mixed(i: u64) -> [u8; 32] {
    let mut m = [0u8; 32];
    m[0..8].copy_from_slice(&i.to_le_bytes());
    m[8] = 0xA5;
    m
}

#[test]
fn ensure_open_reuses_unsealed_tail_and_meta_decodes() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    let (bits, descs) = read_meta(&dir).unwrap();
    assert_eq!(bits, 10);
    assert!(descs.is_empty());
    let raw = std::fs::read(meta_path(&dir)).unwrap();
    assert!(matches!(
        read_meta_buf(&raw[..8]),
        Err(StoreError::Corrupt("tx.head.meta short"))
    ));
    h.ensure_open_for(1).unwrap();
    assert_eq!(h.segment_count(), 1);
    h.ensure_open_for(1).unwrap();
    assert_eq!(h.segment_count(), 1, "unsealed tail stays the open segment");
    let (bits, descs) = read_meta(&dir).unwrap();
    assert_eq!(bits, 10);
    assert_eq!(descs.len(), 1);
    assert_eq!(descs[0].first_fk, 1);
    assert_eq!(descs[0].count, 0);
    assert_eq!(descs[0].file_id, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn probe_candidates_batch_empty_is_empty() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    assert!(h.probe_candidates_batch(&[]).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_flat_head_layout_on_open() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let mut entries = Vec::new();
        for i in 0..50u64 {
            entries.push((mixed(i + 1), Fk(i + 1)));
        }
        h.insert_many(&mut entries).unwrap();
        h.flush().unwrap();
    }
    assert!(dir.join("tx.head").join("meta").is_file());
    // Flatten to legacy flat paths.
    let root = dir.join("tx.head");
    std::fs::rename(root.join("meta"), dir.join("tx.head.meta")).unwrap();
    for ent in std::fs::read_dir(&root).unwrap().flatten() {
        let name = ent.file_name();
        let s = name.to_string_lossy();
        if s == "meta" || s == "meta.tmp" {
            continue;
        }
        let dst = dir.join(format!("tx.head.{s}"));
        std::fs::rename(ent.path(), &dst).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(dir.join("tx.head.meta").is_file());
    assert!(!dir.join("tx.head").join("meta").exists());

    match SegmentedTxHead::open(&dir) {
        Err(StoreError::Corrupt(m)) => {
            assert_eq!(m, INDEX_REFUSE_FLAT_HEAD);
            assert!(m.contains("Class A kept"), "{m}");
        }
        Ok(_) => panic!("flat tx.head.meta must refuse open"),
        Err(other) => panic!("expected INDEX_REFUSE_FLAT_HEAD, got {other}"),
    }
    assert!(dir.join("tx.head.meta").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Class A can hold bodies the head never gets (a write that appends and
/// then rejects on the planned-fk check). The inserted fk stream then has
/// a gap. A segment must still seal exactly the fks it holds: the last
/// entries before the roll must stay findable after the seal publishes.
#[test]
fn fk_gap_before_roll_keeps_entries_findable_after_seal() {
    let dir = tmp();
    // 8-bit head: 256 slots, max_keys = floor(0.8*256) = 204.
    let layout = HeadLayout::new(8).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    // Production collect reads txids for the fk range from txid.body,
    // which also holds the orphan bodies the head never saw.
    let collect: SealCollect = std::sync::Arc::new(|first_fk, count| {
        Ok((0..count)
            .map(|i| (fuse_key_from_mixed(&mixed(first_fk + i)), (i as u32) + 1))
            .collect())
    });
    let orphans = 51..=60u64;
    let mut entries: Vec<_> = (1..=300u64)
        .filter(|fk| !orphans.contains(fk))
        .map(|fk| (mixed(fk), Fk(fk)))
        .collect();
    h.insert_many_with(&mut entries, collect.clone()).unwrap();
    h.flush().unwrap();
    // The next insert publishes the finished seal (OA unlinked).
    h.insert_many_with(&mut [(mixed(301), Fk(301))], collect)
        .unwrap();
    assert!(h.sealed_segment_count() >= 1, "segment 0 must have sealed");

    let mut lost = Vec::new();
    for fk in (1..=301u64).filter(|fk| !orphans.contains(fk)) {
        let cands = h.probe_candidates(&mixed(fk)).unwrap();
        if !cands.contains(&Fk(fk)) {
            lost.push(fk);
        }
    }
    assert!(
        lost.is_empty(),
        "head lost {} entries: {lost:?}",
        lost.len()
    );
    assert_eq!(h.last_inserted_fk(), 301, "coverage must reach the last fk");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A gap at the very end of a segment's span rolls on the next fk (the
/// `take == 0` path), and the result still reopens: `open` requires
/// `first_fk + count` of one segment to equal the next segment's first_fk.
#[test]
fn fk_gap_at_segment_end_rolls_and_reopens() {
    let dir = tmp();
    let layout = HeadLayout::new(8).unwrap();
    let collect: SealCollect = std::sync::Arc::new(|first_fk, count| {
        Ok((0..count)
            .map(|i| (fuse_key_from_mixed(&mixed(first_fk + i)), (i as u32) + 1))
            .collect())
    });
    // fks 195..=210 are orphans: the span of segment 0 (1..=204) ends
    // inside the gap, and 211 is the first fk past it.
    let orphan = |fk: u64| (195..=210).contains(&fk);
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let mut entries: Vec<_> = (1..=260u64)
            .filter(|fk| !orphan(*fk))
            .map(|fk| (mixed(fk), Fk(fk)))
            .collect();
        h.insert_many_with(&mut entries, collect.clone()).unwrap();
        h.flush().unwrap();
        h.insert_many_with(&mut [(mixed(261), Fk(261))], collect)
            .unwrap();
        h.flush().unwrap();
        assert_eq!(h.first_fks_snapshot(), vec![1, 195]);
        assert_eq!(h.last_inserted_fk(), 261);
    }
    let h = SegmentedTxHead::open(&dir).unwrap();
    for fk in (1..=261u64).filter(|fk| !orphan(*fk)) {
        assert!(
            h.probe_candidates(&mixed(fk)).unwrap().contains(&Fk(fk)),
            "fk={fk} lost after reopen"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A gap longer than a whole segment: the empty segments inside it must
/// roll (not spin), and entries on both sides stay findable after reopen.
#[test]
fn fk_gap_longer_than_a_segment_rolls_empty_spans() {
    let dir = tmp();
    let layout = HeadLayout::new(8).unwrap();
    let collect: SealCollect = std::sync::Arc::new(|first_fk, count| {
        Ok((0..count)
            .map(|i| (fuse_key_from_mixed(&mixed(first_fk + i)), (i as u32) + 1))
            .collect())
    });
    // max_keys = 204; orphans 150..=700 cover more than two full spans.
    let orphan = |fk: u64| (150..=700).contains(&fk);
    let live: Vec<u64> = (1..=760u64).filter(|fk| !orphan(*fk)).collect();
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let mut entries: Vec<_> = live.iter().map(|&fk| (mixed(fk), Fk(fk))).collect();
        h.insert_many_with(&mut entries, collect.clone()).unwrap();
        h.flush().unwrap();
        h.insert_many_with(&mut [(mixed(761), Fk(761))], collect)
            .unwrap();
        h.flush().unwrap();
        assert_eq!(h.last_inserted_fk(), 761);
    }
    let h = SegmentedTxHead::open(&dir).unwrap();
    for fk in live.iter().copied().chain([761]) {
        assert!(
            h.probe_candidates(&mixed(fk)).unwrap().contains(&Fk(fk)),
            "fk={fk} lost"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A later fk behind the open segment's `first_fk` is refused. Callers
/// pass non-decreasing absolute fks; this path does not reorder them.
#[test]
fn fk_behind_open_segment_is_corrupt() {
    let dir = tmp();
    let layout = HeadLayout::new(8).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    let collect: SealCollect = std::sync::Arc::new(|first_fk, count| {
        Ok((0..count)
            .map(|i| (fuse_key_from_mixed(&mixed(first_fk + i)), (i as u32) + 1))
            .collect())
    });
    // 300 opens the segment; 5 is behind that first_fk. max_keys = 204.
    let mut entries = [(mixed(300), Fk(300)), (mixed(5), Fk(5))];
    let err = h.insert_many_with(&mut entries, collect).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(m) if m == "tx.head insert fk before segment"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_collect_runs_on_sidecar_not_insert_thread() {
    let dir = tmp();
    let layout = HeadLayout::new(8).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    let caller = std::thread::current().id();
    let collect_tid = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tid = collect_tid.clone();
    let collect: SealCollect = std::sync::Arc::new(move |first_fk, count| {
        *tid.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
        Ok((0..count)
            .map(|i| {
                let fk = first_fk + i;
                (fuse_key_from_mixed(&mixed(fk)), (i as u32) + 1)
            })
            .collect())
    });
    let mut entries: Vec<_> = (0..205u64).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
    h.insert_many_with(&mut entries, collect).unwrap();
    h.flush().unwrap();
    let got = collect_tid
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("collect ran on sidecar");
    assert_ne!(got, caller, "collect must run on the seal sidecar");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_roll_seal_lookup_roundtrip() {
    let dir = tmp();
    // 10-bit head: 1024 slots, max_keys = floor(0.8*1024)=819
    let layout = HeadLayout::new(10).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    assert_eq!(h.max_keys, 819);

    let n = 820u64; // forces a roll (max_keys=819)
    let mut entries = Vec::with_capacity(n as usize);
    for i in 0..n {
        entries.push((mixed(i + 1), Fk(i + 1)));
    }
    h.insert_many(&mut entries).unwrap();
    assert!(h.segment_count() >= 2, "segs={}", h.segment_count());
    h.flush().unwrap();
    assert!(h.sealed_segment_count() >= 1);
    assert_eq!(
        h.sealed_fuse_resident_bytes(),
        0,
        "seal publish must map fuse, not keep the build Box"
    );

    for i in [1u64, 400, 819, 820] {
        let cands = h.probe_candidates(&mixed(i)).unwrap();
        assert!(
            cands.iter().any(|f| f.0 == i),
            "missing fk={i} cands={cands:?}"
        );
    }
    let miss = h.probe_candidates(&mixed(0xDEAD_BEEF)).unwrap();
    assert!(miss.is_empty() || !miss.iter().any(|f| f.0 == 0xDEAD_BEEF));

    h.flush().unwrap();
    drop(h);
    let h2 = SegmentedTxHead::open(&dir).unwrap();
    for i in [1u64, 500, 820] {
        let cands = h2.probe_candidates(&mixed(i)).unwrap();
        assert!(cands.iter().any(|f| f.0 == i), "reopen missing {i}");
    }
    // Sealed fuse never FN on members of first segment.
    let cands = h2.probe_candidates(&mixed(1)).unwrap();
    assert!(cands.iter().any(|f| f.0 == 1));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seal_mphf_one_candidate_bip30_unlinks_oa() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    let n = 820u64;
    let mut entries: Vec<_> = (0..n).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
    h.insert_many(&mut entries).unwrap();
    h.flush().unwrap();
    assert!(h.sealed_segment_count() >= 1);
    let sealed = dir.join("tx.head").join("000000");
    assert!(!sealed.is_file(), "sealed OA file must be unlinked");
    assert!(rbitcoin_store::tx_head_mphf::TxHeadMphf::exists(&sealed));
    assert!(!rbitcoin_store::tx_head_mphf::rel_path(&sealed).is_file());
    assert_eq!(
        &std::fs::read(rbitcoin_store::tx_head_mphf::mphf_path(&sealed)).unwrap()[0..4],
        b"BDZ2"
    );
    assert!(dir.join("tx.head").join("000000.fuse8").is_file());
    let cands = h.probe_candidates(&mixed(1)).unwrap();
    assert_eq!(cands.len(), 1, "cands={cands:?}");
    assert_eq!(cands[0], Fk(1));
    let mut saw_miss = false;
    for i in 0..32u64 {
        let miss = h.probe_candidates(&mixed(0xDEAD_BEEF + i)).unwrap();
        if miss.is_empty() {
            saw_miss = true;
            break;
        }
    }
    assert!(saw_miss, "a sealed unknown key is not a member");

    let k = mixed(0xB1B0);
    let collect: SealCollect = Arc::new(move |first_fk, count| {
        Ok((0..count)
            .map(|i| {
                let fk = first_fk + i;
                let m = if fk == 821 { k } else { mixed(fk) };
                (fuse_key_from_mixed(&m), (i as u32) + 1)
            })
            .collect())
    });
    h.insert_many_with(&mut [(k, Fk(821))], collect.clone())
        .unwrap();
    let mut fill: Vec<_> = (822..1639).map(|i| (mixed(i), Fk(i))).collect();
    h.insert_many_with(&mut fill, collect.clone()).unwrap();
    h.insert_many_with(&mut [(k, Fk(1639))], collect).unwrap();
    let cands = h.probe_candidates(&k).unwrap();
    assert_eq!(
        cands.first().copied(),
        Some(Fk(1639)),
        "newest first {cands:?}"
    );
    assert!(cands.iter().any(|f| f.0 == 821), "cands={cands:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seal publish must persist sealed meta **before** unlinking the open OA.
/// A meta-persist failure mid-publish (crash model) must leave the OA on
/// disk so reopen serves the segment unsealed instead of forcing a full
/// head rebuild.
#[test]
fn seal_publish_keeps_oa_when_meta_persist_fails() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let n = 820u64; // max_keys=819 → roll + background seal of file 000000
        let mut entries: Vec<_> = (0..n).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
        h.insert_many(&mut entries).unwrap();
        // Block `meta.tmp` so persist_meta fails inside the publish.
        let block = dir.join("tx.head").join("meta.tmp");
        std::fs::create_dir(&block).unwrap();
        let err = h
            .flush()
            .expect_err("meta persist must fail during publish");
        let _ = err;
        std::fs::remove_dir(&block).unwrap();
    }
    let oa = dir.join("tx.head").join("000000");
    assert!(
        oa.is_file(),
        "OA must not be unlinked before sealed meta is durable"
    );
    let h2 = SegmentedTxHead::open(&dir).expect("reopen without rebuild");
    let cands = h2.probe_candidates(&mixed(1)).unwrap();
    assert!(cands.iter().any(|f| f.0 == 1), "cands={cands:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Crash between sealed-meta persist and OA unlink leaves a leftover open
/// OA next to the sealed `.mphf`. Open must discard it (sealed segments
/// never read the base file).
#[test]
fn open_discards_leftover_oa_for_sealed_segment() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let n = 820u64;
        let mut entries: Vec<_> = (0..n).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
        h.insert_many(&mut entries).unwrap();
        h.flush().unwrap();
        assert!(h.sealed_segment_count() >= 1);
    }
    let oa = dir.join("tx.head").join("000000");
    let mphf = dir.join("tx.head").join("000000.mphf");
    assert!(!oa.is_file());
    assert!(mphf.is_file());
    let mphf_before = std::fs::read(&mphf).unwrap();
    std::fs::write(&oa, b"leftover pre-unlink OA").unwrap();
    let h2 = SegmentedTxHead::open(&dir).unwrap();
    assert!(
        !oa.is_file(),
        "leftover sealed-segment OA must be discarded on open"
    );
    assert_eq!(
        std::fs::read(&mphf).unwrap(),
        mphf_before,
        "mphf must be unchanged when discarding leftover OA"
    );
    let cands = h2.probe_candidates(&mixed(1)).unwrap();
    assert!(cands.iter().any(|f| f.0 == 1), "cands={cands:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One seal pad, then a v1 fuse file written the way seal names `.fuse8`.
#[test]
fn v1_fuse_and_mono_head_refuse_open() {
    let dir = tmp();
    let layout = HeadLayout::new(10).unwrap();
    // 0.8 * 1024 = 819 → seal at 820.
    let n = 820u64;
    {
        let h = SegmentedTxHead::create(&dir, layout).unwrap();
        let mut entries: Vec<_> = (0..n).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
        h.insert_many(&mut entries).unwrap();
        h.flush().unwrap();
        assert!(h.sealed_segment_count() >= 1);
        let fuse_path = dir.join("tx.head").join("000000.fuse8");
        assert!(
            fuse_path.is_file(),
            "seal writes 000000.fuse8, got {}",
            fuse_path.display()
        );
    }

    // Same pad: leftover v1 fuse refuses open.
    let fuse_path = dir.join("tx.head").join("000000.fuse8");
    let mut raw = Vec::from(*b"BF8R");
    raw.extend_from_slice(&1u32.to_le_bytes());
    raw.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&fuse_path, &raw).unwrap();
    match SegmentedTxHead::open(&dir) {
        Err(StoreError::Corrupt(m)) => {
            assert_eq!(m, rbitcoin_store::fuse8_filter::INDEX_REFUSE_FUSE8_V1);
        }
        Ok(_) => panic!("v1 fuse must refuse SegmentedTxHead::open"),
        Err(other) => panic!("expected INDEX_REFUSE_FUSE8_V1, got {other}"),
    }

    // Same suite budget: count-only roll + mono refuse without extra full pads.
    let dir_roll = tmp();
    let layout_roll = HeadLayout::new(8).unwrap();
    let h_roll = SegmentedTxHead::create(&dir_roll, layout_roll).unwrap();
    let mut fill: Vec<_> = (0..254u64).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
    h_roll.insert_many(&mut fill).unwrap();
    assert!(h_roll.segment_count() >= 2);
    h_roll.flush().unwrap();
    assert!(h_roll.sealed_segment_count() >= 1);
    assert!(h_roll
        .probe_candidates(&mixed(50))
        .unwrap()
        .iter()
        .any(|f| f.0 == 50));
    assert!(h_roll
        .probe_candidates(&mixed(220))
        .unwrap()
        .iter()
        .any(|f| f.0 == 220));

    let dir_mono = tmp();
    std::fs::write(dir_mono.join("tx.head"), b"legacy").unwrap();
    let layout_mono = HeadLayout::new(10).unwrap();
    let err = SegmentedTxHead::create(&dir_mono, layout_mono)
        .err()
        .expect("must refuse mono head");
    let s = format!("{err}");
    assert!(s.contains("legacy") || s.contains("reindex"), "{s}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir_roll);
    let _ = std::fs::remove_dir_all(&dir_mono);
}

/// Roll opens the next OA and returns without joining BDZ/fuse. The sealing
/// OA stays in the Open wave until [`SegmentedTxHead::flush`].
#[test]
fn roll_does_not_join_seal_open_probes_sealing_oa() {
    let dir = tmp();
    // 8-bit: 256 slots, max_keys = floor(0.8*256)=204.
    let layout = HeadLayout::new(8).unwrap();
    let h = SegmentedTxHead::create(&dir, layout).unwrap();
    let n = 205u64;
    let mut entries: Vec<_> = (0..n).map(|i| (mixed(i + 1), Fk(i + 1))).collect();
    h.insert_many(&mut entries).unwrap();
    assert!(h.segment_count() >= 2, "segs={}", h.segment_count());
    assert_eq!(
        h.sealed_segment_count(),
        0,
        "insert_many must not join/publish the sidecar seal"
    );
    let unsealed = h.unsealed_ranges();
    assert!(
        unsealed.len() >= 2,
        "tail + in-flight seal, unsealed={unsealed:?}"
    );
    let oa = dir.join("tx.head").join("000000");
    assert!(oa.is_file(), "sealing OA stays on disk until publish");
    let open = h
        .probe_candidates_batch_wave(
            &[mixed(1)],
            HeadProbeWave::Open,
            None,
            &mut rbitcoin_store::IoCtx::none(),
        )
        .unwrap();
    assert!(
        open[0].iter().any(|f| f.0 == 1),
        "Open wave must probe the sealing OA, cands={:?}",
        open[0]
    );
    assert!(h
        .probe_candidates(&mixed(1))
        .unwrap()
        .iter()
        .any(|f| f.0 == 1));
    assert!(h
        .probe_candidates(&mixed(205))
        .unwrap()
        .iter()
        .any(|f| f.0 == 205));

    h.flush().unwrap();
    assert!(h.sealed_segment_count() >= 1);
    assert!(!oa.is_file(), "flush publishes and unlinks the OA");
    assert!(rbitcoin_store::tx_head_mphf::TxHeadMphf::exists(&oa));
    let open_after = h
        .probe_candidates_batch_wave(
            &[mixed(1)],
            HeadProbeWave::Open,
            None,
            &mut rbitcoin_store::IoCtx::none(),
        )
        .unwrap();
    assert!(
        !open_after[0].iter().any(|f| f.0 == 1),
        "sealed segment leaves the Open wave, cands={:?}",
        open_after[0]
    );
    assert!(h
        .probe_candidates(&mixed(1))
        .unwrap()
        .iter()
        .any(|f| f.0 == 1));
    let _ = std::fs::remove_dir_all(&dir);
}
