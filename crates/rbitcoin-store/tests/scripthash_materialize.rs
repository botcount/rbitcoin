use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::fuse8_filter::SealedFuse8;
use rbitcoin_store::scripthash_layout::ShHeadKey;
use rbitcoin_store::scripthash_materialize::*;
use rbitcoin_store::scripthash_mphf::{mix_key16, MphfHead};
use std::fs::{self};
use std::hash::Hasher;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

use std::hash::Hash;

fn prefix_key(b0: u8) -> ShHeadKey {
    let mut k = [0u8; KEY16_LEN];
    k[0] = b0;
    k
}

#[test]
fn key16_identity_hasher_is_xor_of_u64_halves() {
    let mut k = [0u8; KEY16_LEN];
    k[0..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    k[8..16].copy_from_slice(&0x1112_1314_1516_1718u64.to_le_bytes());
    let mut h = Key16IdentityHasher::default();
    k.hash(&mut h);
    assert_eq!(h.finish(), 0x0102_0304_0506_0708u64 ^ 0x1112_1314_1516_1718);
    let mut h2 = Key16IdentityHasher::default();
    k.hash(&mut h2);
    assert_eq!(h.finish(), h2.finish());
}

#[test]
fn key16_identity_try_insert_first_second_third_is_pack8_word() {
    let mut map = Key16PackMap::default();
    let k = prefix_key(1);
    insert_key_pack(&mut map, k, Fk(10));
    assert_eq!(map.get(&k).copied(), Some(10));
    insert_key_pack(&mut map, k, Fk(11));
    assert_eq!(map.get(&k).copied(), Some(0));
    insert_key_pack(&mut map, k, Fk(12));
    assert_eq!(map.get(&k).copied(), Some(0), "third hit must not write");
    assert_eq!(KEYS_SPILL_MAGIC, b"SHKSP01\0");
    assert_eq!(KEYS_COLLECT_BYTES_PER_KEY, 64);
    assert_eq!(SH_EXTRACT_WORKER_RAM_BYTES, 3 << 29);
}

#[test]
fn flush_collect_tally_matches_per_item_and_skips_zeros() {
    let recs = AtomicU64::new(0);
    let hits = AtomicU64::new(0);
    let per = [AtomicU64::new(0), AtomicU64::new(0)];
    let mut one = CollectTally::new(2);
    for _ in 0..3 {
        one.recs = one.recs.saturating_add(1);
        one.hits = one.hits.saturating_add(1);
        one.per[0] = one.per[0].saturating_add(1);
        flush_collect_tally(&mut one, &recs, Some(&hits), Some(&per));
    }
    let batch_recs = AtomicU64::new(0);
    let batch_hits = AtomicU64::new(0);
    let batch_per = [AtomicU64::new(0), AtomicU64::new(0)];
    let mut batch = CollectTally {
        recs: 3,
        hits: 3,
        per: vec![3, 0],
    };
    flush_collect_tally(&mut batch, &batch_recs, Some(&batch_hits), Some(&batch_per));
    assert_eq!(
        recs.load(Ordering::Relaxed),
        batch_recs.load(Ordering::Relaxed)
    );
    assert_eq!(
        hits.load(Ordering::Relaxed),
        batch_hits.load(Ordering::Relaxed)
    );
    assert_eq!(
        per[0].load(Ordering::Relaxed),
        batch_per[0].load(Ordering::Relaxed)
    );
    assert_eq!(per[1].load(Ordering::Relaxed), 0);
    let snap = (
        batch_recs.load(Ordering::Relaxed),
        batch_hits.load(Ordering::Relaxed),
        batch_per[0].load(Ordering::Relaxed),
    );
    flush_collect_tally(&mut batch, &batch_recs, Some(&batch_hits), Some(&batch_per));
    assert_eq!(
        snap,
        (
            batch_recs.load(Ordering::Relaxed),
            batch_hits.load(Ordering::Relaxed),
            batch_per[0].load(Ordering::Relaxed),
        )
    );
}

#[test]
fn scanned_fks_count_finished_spans_not_max_fk() {
    let spans = worker_fk_spans(1, 100, 4);
    assert_eq!(spans[3], (76, 100));
    let scanned = AtomicU64::new(0);
    let one = note_scanned_fks(&scanned, spans[3].0, spans[3].0);
    assert_eq!(one, 1, "finishing one fk of the late span is not fk 76");
    note_scanned_fks(&scanned, spans[0].0, spans[0].1);
    assert_eq!(scanned.load(Ordering::Relaxed), 1 + 25);
}

#[test]
fn keys_spill_channel_is_one_slot() {
    let (tx, rx) = keys_spill_channel();
    let mut first = Key16PackMap::default();
    insert_key_pack(&mut first, prefix_key(1), Fk(1));
    tx.send((0, first)).unwrap();
    let mut second = Key16PackMap::default();
    insert_key_pack(&mut second, prefix_key(2), Fk(2));
    match tx.try_send((0, second)) {
        Err(mpsc::TrySendError::Full(_)) => {}
        other => panic!("second spill must block on the one-slot queue, got {other:?}"),
    }
    assert_eq!(rx.recv().unwrap().0, 0);
}

#[test]
fn keys_spill_writer_folds_two_senders() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-spill-writer").expect("temp");
    let seqs = [AtomicU32::new(0)];
    let err = Mutex::new(None);
    let (tx, rx) = keys_spill_channel();
    std::thread::scope(|scope| {
        scope.spawn(|| keys_spill_writer(rx, dir.path(), &seqs, &err));
        let tx_b = tx.clone();
        scope.spawn(move || {
            let mut map = Key16PackMap::default();
            insert_key_pack(&mut map, prefix_key(1), Fk(1));
            send_keys_spill(&tx_b, 0, map).unwrap();
        });
        let mut map = Key16PackMap::default();
        insert_key_pack(&mut map, prefix_key(2), Fk(9));
        send_keys_spill(&tx, 0, map).unwrap();
        drop(tx);
    });
    assert!(err.lock().unwrap().is_none());
    let got = load_keys_shard_map(dir.path(), 0).unwrap();
    assert_eq!(got.get(&prefix_key(1)).copied(), Some(1));
    assert_eq!(got.get(&prefix_key(2)).copied(), Some(9));
}

#[test]
fn leftover_keys_file_refuses_to_open() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-leftover-file").expect("temp");
    std::fs::create_dir_all(dir.join(KEYS_SUBDIR)).unwrap();
    let path = unsorted_keys_path(dir.path(), 0);
    rbitcoin_store::file::write_synced_tmp_rename(&path, b"not-a-spill-dir").unwrap();
    match load_keys_shard_map(dir.path(), 0) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("must refuse leftover keys file, got {other:?}"),
    }
    assert!(leftover_legacy_unsorted(dir.path()));
}

#[test]
fn two_spills_fold_repeat_key_to_zero_without_sort() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-two-spill").expect("temp");
    let a = prefix_key(1);
    let b = prefix_key(2);
    write_keys_spill_entries(dir.path(), 0, 0, &[(b, Some(Fk(5))), (a, Some(Fk(1)))]).unwrap();
    write_keys_spill_entries(dir.path(), 0, 1, &[(a, Some(Fk(9)))]).unwrap();
    assert!(unsorted_keys_spill_path(dir.path(), 0, 0).is_file());
    assert!(unsorted_keys_spill_path(dir.path(), 0, 1).is_file());
    assert!(unsorted_keys_path(dir.path(), 0).is_dir());
    let map = load_keys_shard_map(dir.path(), 0).unwrap();
    assert_eq!(map.get(&a).copied(), Some(0), "cross-spill repeat is multi");
    assert_eq!(map.get(&b).copied(), Some(5));
    assert_eq!(map.len(), 2);
}

#[test]
fn keys_spill_header_counts_select_the_key() {
    let mut bytes = vec![0u8; KEYS_SPILL_HDR];
    bytes[..8].copy_from_slice(KEYS_SPILL_MAGIC);
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
    let key = std::array::from_fn(|i| (i as u8).wrapping_add(1));
    bytes.extend_from_slice(&key);
    let mut map = Key16PackMap::default();
    fold_keys_spill_bytes(&mut map, &bytes).unwrap();
    assert_eq!(map.get(&key).copied(), Some(0));
    match fold_keys_spill_bytes(&mut map, &bytes[..15]) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("short spill header must refuse, got {other:?}"),
    }
}

#[test]
fn keys_spill_magic_wrong_refuses() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-bad-magic").expect("temp");
    std::fs::create_dir_all(unsorted_keys_path(dir.path(), 0)).unwrap();
    let path = unsorted_keys_spill_path(dir.path(), 0, 0);
    rbitcoin_store::file::write_synced_tmp_rename(&path, b"SHKEYS02........").unwrap();
    match load_keys_shard_map(dir.path(), 0) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("wrong magic must refuse, got {other:?}"),
    }
}

#[test]
fn spill_largest_keys_writes_fattest_and_spills_while_over() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-spill-largest").expect("temp");
    let mut maps: Vec<Key16PackMap> = (0..3).map(|_| Key16PackMap::default()).collect();
    insert_key_pack(&mut maps[0], prefix_key(1), Fk(1));
    insert_key_pack(&mut maps[0], prefix_key(2), Fk(2));
    insert_key_pack(&mut maps[0], prefix_key(3), Fk(3));
    insert_key_pack(&mut maps[1], prefix_key(4), Fk(4));
    insert_key_pack(&mut maps[2], prefix_key(5), Fk(5));
    let seqs: Vec<AtomicU32> = (0..3).map(|_| AtomicU32::new(0)).collect();
    let budget = 2 * KEYS_COLLECT_BYTES_PER_KEY;
    spill_largest_keys_while_over(&mut maps, budget, |si, map| {
        spill_shard_map(dir.path(), si, &seqs, map)
    })
    .unwrap();
    assert!(maps[0].is_empty(), "fattest shard spilled first");
    assert!(
        maps[1].is_empty() ^ maps[2].is_empty(),
        "second while-over spill empties exactly one remaining shard"
    );
    let leftover = if maps[1].is_empty() { 2 } else { 1 };
    assert_eq!(maps[leftover].len(), 1, "one shard stays under budget");
    assert!(unsorted_keys_spill_path(dir.path(), 0, 0).is_file());
    let n_files = [0, 1, 2]
        .iter()
        .filter(|si| unsorted_keys_spill_path(dir.path(), **si, 0).is_file())
        .count();
    assert_eq!(n_files, 2);
    let got0 = load_keys_shard_map(dir.path(), 0).unwrap();
    assert_eq!(got0.len(), 3);
}

#[test]
fn multi_fuse8_contains_inserted_mix64() {
    let k = prefix_key(9);
    let mixed = mix_key16(&k);
    let fuse = SealedFuse8::build(&[mixed]).unwrap();
    assert!(fuse.contains(mixed));
}

#[test]
fn sh_extract_workers_cap_at_1_5gib() {
    assert_eq!(SH_EXTRACT_WORKER_RAM_BYTES, 3 << 29);
    assert_eq!(
        rbitcoin_store::sorted_run::workers_for_free_ram(8, 3 << 30, SH_EXTRACT_WORKER_RAM_BYTES),
        2
    );
    assert_eq!(
        rbitcoin_store::sorted_run::workers_for_free_ram(16, 24 << 30, SH_EXTRACT_WORKER_RAM_BYTES),
        16
    );
}

#[test]
fn keys_done_magic_is_shkeys02() {
    assert_eq!(KEYS_DONE_MAGIC, b"SHKEYS02");
    assert_eq!(POST_DONE_MAGIC, b"SHPOST02");
    assert_eq!(POSTS_SPILL_MAGIC, b"SHPST01\0");
    assert_eq!(POST_MAP_KEY_BYTES, 80);
    assert_eq!(POST_MAP_FK_BYTES, 8);
}

#[test]
fn spill_largest_posts_writes_fattest_shard() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-post-largest").expect("temp");
    let mut maps: Vec<PostPackMap> = (0..2).map(|_| PostPackMap::default()).collect();
    insert_post_fk(&mut maps[0], prefix_key(1), 1);
    insert_post_fk(&mut maps[0], prefix_key(1), 2);
    insert_post_fk(&mut maps[1], prefix_key(2), 3);
    let seqs: Vec<AtomicU32> = (0..2).map(|_| AtomicU32::new(0)).collect();
    let fat = maps[0].estimated_bytes() as u64;
    spill_largest_posts_while_over(&mut maps, fat.saturating_sub(1), |si, map| {
        spill_post_shard_map(dir.path(), si, &seqs, map)
    })
    .unwrap();
    assert!(maps[0].is_empty());
    assert_eq!(maps[1].map.len(), 1);
    let folded = load_post_shard_map(dir.path(), 0).unwrap();
    assert_eq!(
        folded.map.get(&prefix_key(1)).map(|v| v.as_slice()),
        Some(&[1u64, 2][..])
    );
}

#[test]
fn leftover_shunsrt3_done_is_not_ok() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-sh-done-v3-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let n_shards = 2usize;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"SHUNSRT3");
    buf.extend_from_slice(&(n_shards as u32).to_le_bytes());
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    fs::write(dir.join("DONE"), &buf).unwrap();
    fs::write(unsorted_shard_path(&dir, 0), [0u8; 24]).unwrap();
    fs::write(unsorted_shard_path(&dir, 1), []).unwrap();
    assert!(
        unsorted_done_last_fk(&dir, n_shards).is_none(),
        "SHUNSRT3 must not count as DONE.keys"
    );
    assert!(leftover_legacy_unsorted(&dir));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn leftover_shunsrt2_done_is_not_ok() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-sh-done-v2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let n_shards = 2usize;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"SHUNSRT2");
    buf.extend_from_slice(&(n_shards as u32).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    fs::write(dir.join("DONE"), &buf).unwrap();
    fs::write(unsorted_shard_path(&dir, 0), []).unwrap();
    fs::write(unsorted_shard_path(&dir, 1), []).unwrap();
    assert!(
        unsorted_done_last_fk(&dir, n_shards).is_none(),
        "SHUNSRT2 without last_fk must restart collect"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plan_unsorted_collect_appends_only_when_no_shards() {
    assert!(matches!(
        plan_unsorted_collect(None, 10, false, true),
        UnsortedCollectAction::Full
    ));
    assert!(matches!(
        plan_unsorted_collect(Some(10), 10, false, true),
        UnsortedCollectAction::Skip
    ));
    assert!(matches!(
        plan_unsorted_collect(Some(4), 10, false, true),
        UnsortedCollectAction::Append { first: 5, last: 10 }
    ));
    assert!(matches!(
        plan_unsorted_collect(Some(4), 10, true, true),
        UnsortedCollectAction::Skip
    ));
    assert!(matches!(
        plan_unsorted_collect(Some(4), 10, false, false),
        UnsortedCollectAction::Full
    ));
}

#[test]
fn keys_spill_delta_roundtrip_first_fk_order() {
    let mut map = Key16PackMap::default();
    insert_key_pack(&mut map, prefix_key(3), Fk(100));
    insert_key_word(&mut map, prefix_key(1), 0);
    insert_key_pack(&mut map, prefix_key(2), Fk(20));
    let bytes = encode_keys_spill(&map);
    assert_eq!(&bytes[0..8], KEYS_SPILL_MAGIC);
    let mut got = Key16PackMap::default();
    fold_keys_spill_bytes(&mut got, &bytes).unwrap();
    assert_eq!(got.get(&prefix_key(1)).copied(), Some(0));
    assert_eq!(got.get(&prefix_key(2)).copied(), Some(20));
    assert_eq!(got.get(&prefix_key(3)).copied(), Some(100));
}

#[test]
fn reused_script_two_spans_one_spill_rec_under_cap() {
    let mut map = Key16PackMap::default();
    let k = prefix_key(7);
    insert_key_pack(&mut map, k, Fk(1));
    insert_key_pack(&mut map, k, Fk(65_536));
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&k).copied(), Some(0));
    let bytes = encode_keys_spill(&map);
    let n_multi = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let n_single = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    assert_eq!(n_multi, 1, "one rec while the map has not filled");
    assert_eq!(n_single, 0);
}

#[test]
fn hashmap_iteration_order_seals_valid_mphf() {
    let mut map = Key16PackMap::default();
    insert_key_pack(&mut map, prefix_key(1), Fk(10));
    insert_key_pack(&mut map, prefix_key(2), Fk(20));
    insert_key_word(&mut map, prefix_key(3), 0);
    let mut recs = Vec::with_capacity(map.len());
    let mut mixed = Vec::with_capacity(map.len());
    for (k, w) in &map {
        mixed.push(mix_key16(k));
        recs.push((*k, *w));
    }
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-map-mphf").expect("temp");
    let base = dir.join("00");
    MphfHead::write_pack8_mixed(&base, &recs, &mixed).unwrap();
    let h = MphfHead::open(&base).unwrap();
    assert_eq!(
        h.get(&prefix_key(1)).unwrap().unwrap(),
        rbitcoin_store::scripthash_layout::ShHeadValue::inline_one(Fk(10))
    );
    assert_eq!(
        h.get(&prefix_key(2)).unwrap().unwrap(),
        rbitcoin_store::scripthash_layout::ShHeadValue::inline_one(Fk(20))
    );
    assert_eq!(
        h.get(&prefix_key(3)).unwrap().unwrap(),
        rbitcoin_store::scripthash_layout::ShHeadValue::Empty
    );
}

#[test]
fn insert_post_fk_out_of_order_stays_strict_and_roundtrips() {
    let mut map = PostPackMap::default();
    let k = prefix_key(4);
    insert_post_fk(&mut map, k, 3);
    insert_post_fk(&mut map, k, 1);
    insert_post_fk(&mut map, k, 3);
    assert_eq!(map.map.get(&k).map(|v| v.as_slice()), Some(&[1u64, 3][..]));
    assert_eq!(map.n_fks, 2);
    let bytes = encode_posts_spill(&map);
    let mut round = PostPackMap::default();
    fold_posts_spill_bytes(&mut round, &bytes).unwrap();
    assert_eq!(
        round.map.get(&k).map(|v| v.as_slice()),
        Some(&[1u64, 3][..])
    );
}

#[test]
fn fold_post_spills_merges_sorted_runs() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-post-merge-order").expect("temp");
    let k = prefix_key(8);
    write_post_spill_entries(dir.path(), 0, 0, &[(k, &[1u64, 5][..])]).unwrap();
    write_post_spill_entries(dir.path(), 0, 1, &[(k, &[3u64, 5, 9][..])]).unwrap();
    let map = load_post_shard_map(dir.path(), 0).unwrap();
    assert_eq!(
        map.map.get(&k).map(|v| v.as_slice()),
        Some(&[1u64, 3, 5, 9][..])
    );
    assert_eq!(map.n_fks, 4);
}

#[test]
fn posts_two_fks_one_spill_rec_under_cap() {
    let mut map = PostPackMap::default();
    let k = prefix_key(7);
    insert_post_fk(&mut map, k, 2);
    insert_post_fk(&mut map, k, 2);
    insert_post_fk(&mut map, k, 11);
    assert_eq!(map.map.get(&k).map(|v| v.as_slice()), Some(&[2u64, 11][..]));
    assert_eq!(map.n_fks, 2);
    assert_eq!(
        map.estimated_bytes(),
        POST_MAP_KEY_BYTES + 2 * POST_MAP_FK_BYTES
    );
    let bytes = encode_posts_spill(&map);
    assert_eq!(&bytes[0..8], POSTS_SPILL_MAGIC);
    let n_keys = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(n_keys, 1);
    let mut round = PostPackMap::default();
    fold_posts_spill_bytes(&mut round, &bytes).unwrap();
    assert_eq!(
        round.map.get(&k).map(|v| v.as_slice()),
        Some(&[2u64, 11][..])
    );
}

#[test]
fn tiny_post_budget_spills_then_flush() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-tiny-post").expect("temp");
    let mut maps = vec![PostPackMap::default()];
    let seqs = [AtomicU32::new(0)];
    insert_post_fk(&mut maps[0], prefix_key(1), 1);
    insert_post_fk(&mut maps[0], prefix_key(1), 2);
    insert_post_fk(&mut maps[0], prefix_key(2), 3);
    spill_largest_posts_while_over(&mut maps, 100, |si, map| {
        spill_post_shard_map(dir.path(), si, &seqs, map)
    })
    .unwrap();
    assert!(maps[0].is_empty());
    insert_post_fk(&mut maps[0], prefix_key(2), 4);
    spill_all_post_maps(&mut maps, |si, map| {
        spill_post_shard_map(dir.path(), si, &seqs, map)
    })
    .unwrap();
    assert!(unsorted_post_spill_path(dir.path(), 0, 0).is_file());
    assert!(unsorted_post_spill_path(dir.path(), 0, 1).is_file());
    assert!(unsorted_post_path(dir.path(), 0).is_dir());
    let folded = load_post_shard_map(dir.path(), 0).unwrap();
    assert_eq!(
        folded.map.get(&prefix_key(1)).map(|v| v.as_slice()),
        Some(&[1u64, 2][..])
    );
    assert_eq!(
        folded.map.get(&prefix_key(2)).map(|v| v.as_slice()),
        Some(&[3u64, 4][..])
    );
}

#[test]
fn two_post_spills_fold_same_key() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-post-fold").expect("temp");
    let k = prefix_key(9);
    write_post_spill_entries(dir.path(), 0, 0, &[(k, &[1u64, 3][..])]).unwrap();
    write_post_spill_entries(dir.path(), 0, 1, &[(k, &[3u64, 5][..])]).unwrap();
    let map = load_post_shard_map(dir.path(), 0).unwrap();
    assert_eq!(
        map.map.get(&k).map(|v| v.as_slice()),
        Some(&[1u64, 3, 5][..])
    );
}

#[test]
fn leftover_post_file_refuses_to_open() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-leftover-post").expect("temp");
    std::fs::create_dir_all(dir.join(POST_SUBDIR)).unwrap();
    let path = unsorted_post_path(dir.path(), 0);
    rbitcoin_store::file::write_synced_tmp_rename(&path, b"not-a-spill-dir").unwrap();
    match load_post_shard_map(dir.path(), 0) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("must refuse leftover post file, got {other:?}"),
    }
    assert!(leftover_legacy_unsorted(dir.path()));
    assert!(leftover_post_file(dir.path()));
}

#[test]
fn leftover_post_spill_wrong_magic_refuses() {
    let dir = rbitcoin_store::testutil::TempDir::labeled("sh-post-bad-magic").expect("temp");
    std::fs::create_dir_all(unsorted_post_path(dir.path(), 0)).unwrap();
    let path = unsorted_post_spill_path(dir.path(), 0, 0);
    rbitcoin_store::file::write_synced_tmp_rename(&path, b"SHPOST02........").unwrap();
    match load_post_shard_map(dir.path(), 0) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("wrong magic must refuse, got {other:?}"),
    }
}

#[test]
fn posts_spill_fallocate_padding_refuses() {
    let mut map = PostPackMap::default();
    insert_post_fk(&mut map, prefix_key(1), 7);
    let mut bytes = encode_posts_spill(&map);
    bytes.extend_from_slice(&[0u8; 64]);
    match fold_posts_spill_bytes(&mut PostPackMap::default(), &bytes) {
        Err(StoreError::Corrupt(m)) => {
            assert!(
                m.contains("trailing bytes"),
                "padding after SHPST01 must not scan as recs, got {m}"
            );
        }
        other => panic!("fallocate padding must refuse, got {other:?}"),
    }
}

#[test]
fn fold_posts_spill_bytes_reads_key_count_and_refuses_short() {
    let mut map = PostPackMap::default();
    let k = prefix_key(12);
    insert_post_fk(&mut map, k, 4);
    insert_post_fk(&mut map, k, 9);
    let bytes = encode_posts_spill(&map);
    let mut round = PostPackMap::default();
    fold_posts_spill_bytes(&mut round, &bytes).unwrap();
    assert_eq!(
        round.map.get(&k).map(|v| v.as_slice()),
        Some(&[4u64, 9][..])
    );
    match fold_posts_spill_bytes(&mut PostPackMap::default(), &bytes[..POSTS_SPILL_HDR]) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("posts spill truncated"), "{m}");
        }
        other => panic!("header without keys must truncate, got {other:?}"),
    }
    match fold_posts_spill_bytes(&mut PostPackMap::default(), &bytes[..POSTS_SPILL_HDR - 1]) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("short spill must refuse, got {other:?}"),
    }
    let mut bad = bytes.clone();
    bad[0] = b'X';
    match fold_posts_spill_bytes(&mut PostPackMap::default(), &bad) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("wipe store/scripthash.unsorted"), "{m}");
        }
        other => panic!("bad magic must refuse, got {other:?}"),
    }
}

#[test]
fn key16_hasher_reads_sixteen_and_skips_short() {
    let mut k = [0u8; KEY16_LEN];
    k[0..8].copy_from_slice(&9u64.to_le_bytes());
    k[8..16].copy_from_slice(&4u64.to_le_bytes());
    let mut h = Key16IdentityHasher::default();
    h.write(&k);
    assert_eq!(h.finish(), 9 ^ 4);
    h.write(&[1, 2, 3, 4]);
    assert_eq!(h.finish(), 9 ^ 4);
}

#[test]
fn phase_done_reads_the_last_fk_and_ignores_a_short_file() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-phase-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_phase_done(&dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, 42, &[3, 4]).unwrap();
    assert_eq!(
        read_phase_done(&dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, 2).map(|d| d.counts),
        Some(vec![3, 4])
    );
    assert_eq!(unsorted_done_last_fk(&dir, 2), Some(42));
    let path = dir.join(KEYS_DONE_NAME);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(20);
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(unsorted_done_last_fk(&dir, 2), None);
    std::fs::write(&path, b"SHKEYS02").unwrap();
    assert_eq!(unsorted_done_last_fk(&dir, 2), None);
    let _ = std::fs::remove_dir_all(&dir);
}
