use rbitcoin_store::error::StoreError;
use rbitcoin_store::spend_page_log::{
    open_slot_present, pace_after_checkpoint, spend_checkpoint_early, spend_note_arms,
    SpendPageLog, LOG_IMAGE_CAP, OPEN_EDGE_CAP, OPEN_SLOT_PRESENCE_BYTES, SPEND_PAGES_COMMIT_NAME,
    SPEND_PAGES_NAME,
};
use std::path::PathBuf;

fn dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("rbitcoin-spend-pages-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn image_cap_schedules_one_followup_and_the_edge_cap_stays_due() {
    assert!(!spend_checkpoint_early(
        OPEN_EDGE_CAP - 1,
        0,
        LOG_IMAGE_CAP - 1
    ));
    assert!(spend_checkpoint_early(OPEN_EDGE_CAP, 0, 0));
    assert!(spend_checkpoint_early(0, 0, LOG_IMAGE_CAP));
    assert_eq!(
        pace_after_checkpoint(0, 0, LOG_IMAGE_CAP, false),
        (true, true)
    );
    assert_eq!(
        pace_after_checkpoint(0, 0, LOG_IMAGE_CAP, true),
        (false, true)
    );
    assert_eq!(
        pace_after_checkpoint(OPEN_EDGE_CAP, 0, LOG_IMAGE_CAP, true),
        (true, true)
    );
    assert_eq!(pace_after_checkpoint(0, 0, 0, true), (false, false));
    assert!(spend_note_arms(0, 0, LOG_IMAGE_CAP, false));
    assert!(!spend_note_arms(0, 0, LOG_IMAGE_CAP, true));
    assert!(spend_note_arms(OPEN_EDGE_CAP, 0, LOG_IMAGE_CAP, true));
    assert!(!spend_note_arms(
        OPEN_EDGE_CAP - 1,
        0,
        LOG_IMAGE_CAP - 1,
        false
    ));
}

#[test]
fn open_slot_presence_is_one_bit_of_512() {
    assert_eq!(OPEN_SLOT_PRESENCE_BYTES, 64);
    let mut present = [0u64; 8];
    assert!(!open_slot_present(&present, 0));
    assert!(!open_slot_present(&present, 63));
    assert!(!open_slot_present(&present, 64));
    assert!(!open_slot_present(&present, 511));
    assert!(!open_slot_present(&present, 512));
    present[0] |= 1 << 63;
    present[1] |= 1;
    present[7] |= 1 << 63;
    assert!(!open_slot_present(&present, 0));
    assert!(open_slot_present(&present, 63));
    assert!(open_slot_present(&present, 64));
    assert!(!open_slot_present(&present, 65));
    assert!(open_slot_present(&present, 511));
    assert!(!open_slot_present(&present, 512));
}

#[test]
fn batched_slot_read_matches_the_single_offset_read() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let noted = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let shadow = [7u8, 9, 8, 7, 6, 5, 4, 3];
    log.note_slots(&[(16, noted)]).unwrap();
    let open_offs = [24u64, 16, 32];
    let open_batch = log.slots_at(&open_offs);
    assert_eq!(
        open_batch[0], None,
        "sibling on an open page is a presence miss"
    );
    assert_eq!(open_batch[1], Some(noted));
    assert_eq!(open_batch[2], None);
    assert_eq!(log.slot(24), None);
    assert_eq!(log.slot(16), Some(noted));
    assert_eq!(log.slot(32), None);
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(16, shadow)]).unwrap();
    let offs = [16u64, 24, 4096];
    let batch = log.slots_at(&offs);
    assert_eq!(log.slot(16), Some(shadow));
    assert_eq!(log.slot(24), Some([0u8; 8]));
    assert_eq!(log.slot(4096), None);
    assert_eq!(
        batch[0],
        Some(shadow),
        "a later note shadows the page image"
    );
    assert_eq!(
        batch[1],
        Some([0u8; 8]),
        "a slot with no note is the committed image"
    );
    assert_eq!(batch[2], None, "the next page was never noted");
    let _ = std::fs::remove_dir_all(&d);
}

fn zeros(off: u64, len: usize) -> Result<Vec<u8>, StoreError> {
    let _ = off;
    Ok(vec![0u8; len])
}

#[test]
fn covered_slot_presence_does_not_read_the_generation_file() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let pages = d.join(SPEND_PAGES_NAME);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&pages)
        .unwrap();
    file.set_len(0).unwrap();
    assert!(
        log.covers(16),
        "a committed slot stays covered without reading the generation file"
    );
    assert!(
        !log.covers(4096),
        "a page the commit does not name is still the body"
    );
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.slot(16)));
    assert!(
        read.is_err(),
        "the truncated generation file is what the byte read hits"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Notes taken during the cooled copy sit in `open`. The snapshot already
/// merged them with the frozen slots. Install must drop that page without
/// reading the generation file.
#[test]
fn install_does_not_read_open_notes_already_in_the_generation() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let older = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let newer = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, older)]).unwrap();
    log.freeze_before_cooled_copy();
    log.note_slots(&[(24, newer)]).unwrap();
    log.prepare_commit(5, 8192, zeros, |_| Ok(true)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(d.join(SPEND_PAGES_NAME))
        .unwrap()
        .set_len(0)
        .unwrap();
    log.install_commit().unwrap();
    assert!(
        log.covers(16),
        "the frozen slot stays covered without reading the generation file"
    );
    assert!(
        log.covers(24),
        "the note taken during the copy stays covered without that read"
    );
    assert_eq!(
        log.open_edge_count(),
        0,
        "open notes already in the image leave open"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A note that lands after the snapshot is not in the image. Install keeps it.
#[test]
fn install_keeps_a_note_taken_after_the_snapshot() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let older = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let newer = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let after = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, older)]).unwrap();
    log.freeze_before_cooled_copy();
    log.note_slots(&[(24, newer)]).unwrap();
    log.prepare_commit(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(32, after)]).unwrap();
    log.install_commit().unwrap();
    assert_eq!(log.slot(16), Some(older));
    assert_eq!(log.slot(24), Some(newer));
    assert_eq!(log.slot(32), Some(after));
    assert_eq!(
        log.open_edge_count(),
        1,
        "a note taken after the snapshot stays open"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn second_generation_keeps_the_first_image_bytes() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let first = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    log.note_slots(&[(4096, slot)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(&file[..first.len()], first.as_slice());
    assert_eq!(&file[16..24], &slot);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    assert_eq!(loaded.slot(4096), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn record_past_committed_length_is_not_served() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let path = d.join(SPEND_PAGES_COMMIT_NAME);
    let mut buf = std::fs::read(&path).unwrap();
    buf[16..24].copy_from_slice(&8u64.to_le_bytes());
    std::fs::write(&path, &buf).unwrap();
    match SpendPageLog::load(&d) {
        Err(err) => assert!(matches!(
            err,
            StoreError::Corrupt("spend page commit names bytes past the committed length")
        )),
        Ok(_) => panic!("load served a commit record past the committed length"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn short_tail_does_not_hide_a_later_body_slot() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let early = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, early)]).unwrap();
    log.commit_open(1, 100, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(early));
    assert_eq!(
        log.slot(200),
        None,
        "a committed tail shorter than the page leaves the later slot on the body"
    );
    let mut body = vec![0u8; 4096];
    body[200..208].copy_from_slice(&later);
    log.note_slots(&[(24, early)]).unwrap();
    assert_eq!(
        log.slot(200),
        None,
        "an open note does not hide slots past its short base"
    );
    log.commit_open(2, 4096, |_off, len| Ok(body[..len].to_vec()), |_| Ok(true))
        .unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(&file[200..208], &later, "compose copies the body tail");
    assert_eq!(log.slot(200), Some(later));
    log.commit_open(3, 4096, |_off, len| Ok(body[..len].to_vec()), |_| Ok(true))
        .unwrap();
    let mut folded = None;
    log.fold_into(
        |off, image| {
            folded = Some((off, image.to_vec()));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    let (off, image) = folded.expect("the extended page folds");
    assert_eq!(off, 16);
    assert_eq!(
        &image[184..192],
        &later,
        "fold writes the body tail, not zeros"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn checkpoint_rewrites_the_log_down_to_pages_still_named() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot), (4096, slot)]).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096 + 8, slot)]).unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, _image| {
            wrote.push(off);
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote, vec![16]);
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 4096, "a folded page is not left in the log");
    assert_eq!(&file[..8], &slot);
    assert_eq!(&file[8..16], &slot);
    assert_eq!(log.slot(16), None);
    assert_eq!(log.slot(4096), Some(slot));
    assert_eq!(log.slot(4104), Some(slot));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(slot));
    assert_eq!(loaded.slot(4104), Some(slot));
    assert_eq!(loaded.slot(16), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn reload_prefers_a_generation_file_over_a_stale_pages_file() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let pages = d.join(SPEND_PAGES_NAME);
    let image = std::fs::read(&pages).unwrap();
    let commit = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    let generation = u32::from_le_bytes(commit[12..16].try_into().unwrap());
    let gen_path = d.join(format!("{SPEND_PAGES_NAME}.{generation}"));
    std::fs::write(&gen_path, &image).unwrap();
    std::fs::write(&pages, vec![0u8; image.len()]).unwrap();
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    std::fs::write(&gen_path, [0u8; 1]).unwrap();
    match SpendPageLog::load(&d) {
        Err(err) => assert!(matches!(err, StoreError::Corrupt("spend.pages short"))),
        Ok(_) => panic!("a short generation file fell back to spend.pages"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failed_pages_rename_does_not_publish_that_generation_again() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    // The generation file cannot replace a directory, so the pages rename fails
    // after the commit already names that generation.
    std::fs::create_dir(d.join(SPEND_PAGES_NAME)).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(first));
    let commit = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    let generation = u32::from_le_bytes(commit[12..16].try_into().unwrap());
    let gen_path = d.join(format!("{SPEND_PAGES_NAME}.{generation}"));
    let image = std::fs::read(&gen_path).unwrap();
    assert_eq!(&image[16..24], &first);
    let second = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(4096, second)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(std::fs::read(&gen_path).unwrap(), image);
    let commit = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    let next = u32::from_le_bytes(commit[12..16].try_into().unwrap());
    assert_ne!(next, generation);
    assert!(d.join(format!("{SPEND_PAGES_NAME}.{next}")).is_file());
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(first));
    assert_eq!(loaded.slot(4096), Some(second));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn held_snapshot_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [5u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fold_skips_a_page_dirtied_while_an_earlier_page_is_copied() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot), (4096, slot)]).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    let mut early = 0u32;
    log.fold_into(
        |_off, _image| {
            early += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(early, 0, "the publishing checkpoint does not fold");
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, image| {
            wrote.push(off);
            if off == 16 {
                log.note_slots(&[(4096 + 8, later)])?;
            }
            let _ = image;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote, vec![16]);
    assert_eq!(log.slot(4096 + 8), Some(later));
    assert_eq!(log.slot(4096), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn three_batches_on_one_page_are_one_image() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let a = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let b = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let c = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, a)]).unwrap();
    log.note_slots(&[(24, b)]).unwrap();
    log.note_slots(&[(32, c)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 4096);
    assert_eq!(&file[16..24], &a);
    assert_eq!(&file[24..32], &b);
    assert_eq!(&file[32..40], &c);
    assert_eq!(file[40], 0, "an untouched neighbor stays in the image");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn note_during_compose_stays_out_of_that_image() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [4u8, 1, 2, 3, 4, 5, 6, 7];
    let second = [5u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    log.commit_open(
        4,
        8192,
        |off, len| {
            log.note_slots(&[(4096, second)])?;
            zeros(off, len)
        },
        |_| Ok(true),
    )
    .unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 4096);
    assert_eq!(log.slot(16), Some(first));
    assert_eq!(log.slot(4096), Some(second));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_during_compose_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096, slot)]).unwrap();
    log.commit_open(
        9,
        8192,
        |off, len| {
            log.hold_fold_above(1)?;
            zeros(off, len)
        },
        |_| Ok(true),
    )
    .unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a hold taken while composing must survive publish"
    );
    assert_eq!(log.slot(16), Some(slot));
    assert_eq!(log.slot(4096), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn open_edge_cap_waits_only_after_the_sync_thread_is_bound() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let d = dir();
    let log = Arc::new(SpendPageLog::create(&d));
    let slot = [8u8, 1, 2, 3, 4, 5, 6, 7];
    let mut batch = Vec::with_capacity(512);
    let mut n = 0u64;
    while n < 2_000_000 {
        let page = n / 512;
        let rel = n % 512;
        batch.push((page * 4096 + rel * 8, slot));
        n += 1;
        if batch.len() == 512 {
            log.note_slots(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        log.note_slots(&batch).unwrap();
    }
    let started = Instant::now();
    log.note_slots(&[(n * 8, slot)]).unwrap();
    log.wait_open_edge_room();
    let unbound = started.elapsed();
    assert!(
        unbound < Duration::from_millis(500),
        "replay must not wait for a sync thread that is not running, waited {unbound:?}"
    );
    log.bind_sync_wake(Arc::new((
        std::sync::Mutex::new(false),
        std::sync::Condvar::new(),
    )));
    let published = 3907 * 4096;
    let log2 = Arc::clone(&log);
    let gate = Arc::new(AtomicBool::new(false));
    let gate2 = Arc::clone(&gate);
    let worker = std::thread::spawn(move || {
        while !gate2.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(80));
        log2.commit_open(3, published, zeros, |_| Ok(true)).unwrap();
    });
    gate.store(true, Ordering::Release);
    let started = Instant::now();
    log.wait_open_edge_room();
    let waited = started.elapsed();
    worker.join().unwrap();
    assert!(
        waited >= Duration::from_millis(60),
        "the next batch waits for a checkpoint once the sync thread is bound, waited {waited:?}"
    );
    assert_eq!(log.slot(n * 8), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn disconnect_during_first_compose_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(
        9,
        8192,
        |off, len| {
            log.hold_fold_above(1)?;
            zeros(off, len)
        },
        |_| Ok(true),
    )
    .unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0, "a hold during the first compose is not folded");
    log.commit_open(1, 8192, zeros, |_| Ok(false)).unwrap();
    writes = 0;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0, "the abandoned spend is not copied later");
    assert_eq!(log.slot(16), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn failed_compose_retries_the_frozen_page() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let second = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    let err = log
        .commit_open(
            4,
            8192,
            |_off, _len| Err(StoreError::Corrupt("boom")),
            |_| Ok(true),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("boom")));
    assert_eq!(log.slot(16), Some(first));
    assert!(!d.join(SPEND_PAGES_COMMIT_NAME).exists());
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let retried = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(&retried[16..24], &first);
    assert_eq!(log.slot(16), Some(first));
    let _ = std::fs::remove_dir_all(&d);
    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, first)]).unwrap();
    log.commit_open(
        4,
        8192,
        |_off, _len| Err(StoreError::Corrupt("boom")),
        |_| Ok(true),
    )
    .unwrap_err();
    log.note_slots(&[(4096, second)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(first));
    assert_eq!(log.slot(4096), Some(second));
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 8192);
    assert_eq!(&file[16..24], &first);
    assert_eq!(&file[4096..4104], &second);
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(first));
    assert_eq!(loaded.slot(4096), Some(second));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retry_keeps_a_committed_page_that_confirm_reopened() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let sealed = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let newer = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let other = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, sealed)]).unwrap();
    log.commit_open(4, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096, other)]).unwrap();
    log.commit_open(
        5,
        16384,
        |_off, _len| Err(StoreError::Corrupt("boom")),
        |_| Ok(true),
    )
    .unwrap_err();
    log.note_slots(&[(24, newer)]).unwrap();
    log.commit_open(5, 16384, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(sealed));
    assert_eq!(log.slot(24), Some(newer));
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(sealed));
    assert_eq!(loaded.slot(4096), Some(other));
    assert_eq!(loaded.slot(24), Some(newer));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retry_after_hold_keeps_the_exploded_page() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let sealed = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let waiting = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, sealed)]).unwrap();
    log.commit_open(9, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096, fresh)]).unwrap();
    log.commit_open(
        9,
        16384,
        |_off, _len| Err(StoreError::Corrupt("boom")),
        |_| Ok(true),
    )
    .unwrap_err();
    log.note_slots(&[(8192, waiting)]).unwrap();
    log.hold_fold_above(1).unwrap();
    log.commit_open(4, 16384, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(8192), Some(waiting));
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(sealed));
    assert_eq!(loaded.slot(4096), Some(fresh));
    assert_eq!(loaded.slot(8192), Some(waiting));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_reopen_keeps_the_sealed_page() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let sealed = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let undone = [4u8, 1, 2, 3, 4, 5, 6, 7];
    let other = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, sealed)]).unwrap();
    log.commit_open(4, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(24, undone), (4096, other)]).unwrap();
    log.commit_open(5, 16384, zeros, |slot| Ok(slot[0] != 4))
        .unwrap();
    assert_eq!(log.slot(16), Some(sealed));
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(sealed));
    assert_eq!(loaded.slot(24), Some([0u8; 8]));
    assert_eq!(loaded.slot(4096), Some(other));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retry_merges_a_later_note_on_the_frozen_page() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let sealed = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let frozen_slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let replaced = [8u8, 1, 2, 3, 4, 5, 6, 7];
    let added = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, sealed)]).unwrap();
    log.commit_open(4, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(24, frozen_slot), (4096, frozen_slot)])
        .unwrap();
    log.commit_open(
        5,
        16384,
        |_off, _len| Err(StoreError::Corrupt("boom")),
        |_| Ok(true),
    )
    .unwrap_err();
    log.note_slots(&[(24, replaced), (32, added)]).unwrap();
    log.commit_open(5, 16384, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(sealed));
    assert_eq!(loaded.slot(24), Some(replaced));
    assert_eq!(loaded.slot(32), Some(added));
    assert_eq!(loaded.slot(4096), Some(frozen_slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retry_after_hold_omits_a_rejected_image_slot() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let keep_slot = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let drop_slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    let noted = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, keep_slot), (24, drop_slot)]).unwrap();
    log.commit_open(9, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(32, noted), (4096, fresh)]).unwrap();
    log.commit_open(
        9,
        16384,
        |_off, _len| Err(StoreError::Corrupt("boom")),
        |_| Ok(true),
    )
    .unwrap_err();
    log.hold_fold_above(1).unwrap();
    log.commit_open(4, 16384, zeros, |slot| Ok(slot[0] != 4))
        .unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(keep_slot));
    assert_eq!(loaded.slot(24), Some([0u8; 8]));
    assert_eq!(loaded.slot(32), Some(noted));
    assert_eq!(loaded.slot(4096), Some(fresh));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn explode_keeps_a_strong_slot_and_a_newer_open_note() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let newer = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot), (24, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.note_slots(&[(16, newer)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    // The open note at rel 2 wins. The image slot at rel 3 is still strong.
    assert_eq!(log.slot(16), Some(newer));
    assert_eq!(log.slot(24), Some(slot));
    assert_eq!(log.slot(0), Some([0u8; 8]));
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn review_1501_fix_live_explode_keeps_frozen_note() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let older = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let sibling = [7u8, 1, 2, 3, 4, 5, 6, 7];
    let newer = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let waiting = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, older), (24, sibling)]).unwrap();
    log.commit_open(9, 16384, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(16, newer)]).unwrap();
    // The page already has a base, so compose does not read the body.
    // `keep` runs after the freeze and is what leaves that snapshot behind.
    log.commit_open(9, 16384, zeros, |_| Err(StoreError::Corrupt("boom")))
        .unwrap_err();
    log.note_slots(&[(8192, waiting)]).unwrap();
    log.hold_fold_above(1).unwrap();
    log.commit_open(4, 16384, zeros, |_| Ok(true)).unwrap();
    assert_eq!(
        log.slot(16),
        Some(newer),
        "a frozen note must not lose to the older committed slot"
    );
    assert_eq!(log.slot(24), Some(sibling));
    assert_eq!(log.slot(8192), Some(waiting));
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(newer));
    assert_eq!(loaded.slot(24), Some(sibling));
    assert_eq!(loaded.slot(8192), Some(waiting));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn loaded_commit_above_the_checkpoint_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [5u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let log = SpendPageLog::load(&d).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(false)).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, image| {
            writes += 1;
            assert!(
                !image.windows(8).any(|w| w == slot),
                "a not-strong slot from a higher commit must not reach the body"
            );
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    assert_ne!(log.slot(16), Some(slot));
    assert!(!d.join(SPEND_PAGES_COMMIT_NAME).exists());
    assert!(!d.join(SPEND_PAGES_NAME).exists());
    log.hold_fold_above(0).unwrap();
    assert!(!d.join(SPEND_PAGES_COMMIT_NAME).exists());
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_ne!(loaded.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn loaded_commit_above_the_checkpoint_keeps_a_strong_slot() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let strong = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, strong), (24, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let log = SpendPageLog::load(&d).unwrap();
    log.commit_open(4, 8192, zeros, |meta| Ok(meta[0] == 1))
        .unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, image| {
            writes += 1;
            assert!(!image.windows(8).any(|w| w == stale));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    assert_eq!(log.slot(16), Some(strong));
    assert_ne!(log.slot(24), Some(stale));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(strong));
    assert_ne!(loaded.slot(24), Some(stale));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn loaded_commit_above_the_checkpoint_does_not_carry_a_stale_page() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let log = SpendPageLog::load(&d).unwrap();
    log.note_slots(&[(4096, fresh)]).unwrap();
    log.commit_open(4, 8192, zeros, |meta| Ok(meta[0] == 4))
        .unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, image| {
            writes += 1;
            assert!(!image.windows(8).any(|w| w == stale));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    assert_eq!(log.slot(4096), Some(fresh));
    assert_ne!(log.slot(16), Some(stale));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(fresh));
    assert_ne!(loaded.slot(16), Some(stale));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn note_during_explode_is_not_folded_at_the_lower_height() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let old = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, old)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let mut noted = false;
    log.commit_open(4, 8192, zeros, |meta| {
        if !noted {
            noted = true;
            log.note_slots(&[(4096, fresh)]).unwrap();
        }
        Ok(*meta == fresh)
    })
    .unwrap();
    assert!(noted);
    // The replacement spend is still best-chain. This checkpoint sampled 4.
    assert_eq!(log.slot(4096), Some(fresh));
    log.commit_open(4, 8192, zeros, |_| Ok(false)).unwrap();
    let mut wrote_fresh = false;
    log.fold_into(
        |_off, image| {
            if image.windows(8).any(|w| w == fresh) {
                wrote_fresh = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(
        !wrote_fresh,
        "a spend noted during explode must not fold at the lower checkpoint"
    );
    assert_ne!(log.slot(4096), Some(fresh));
    let _ = std::fs::remove_dir_all(&d);
}

fn body_with(slots: &[(u64, [u8; 8])]) -> impl Fn(u64, usize) -> Result<Vec<u8>, StoreError> + '_ {
    move |off, len| {
        let mut buf = vec![0u8; len];
        for &(abs, meta) in slots {
            if abs < off {
                continue;
            }
            let at = (abs - off) as usize;
            if at + 8 <= buf.len() {
                buf[at..at + 8].copy_from_slice(&meta);
            }
        }
        Ok(buf)
    }
}

#[test]
fn rejected_spend_already_on_the_body_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let live = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, live), (24, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, live), (24, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert_eq!(log.slot(16), Some(live));
    assert_ne!(log.slot(24), Some(stale));
    log.commit_open(4, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    log.fold_into(
        |_off, image| {
            assert!(
                !image.windows(8).any(|w| w == stale),
                "a rejected spend already on the body must not be folded again"
            );
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_page_already_on_the_body_is_cleared() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |_| Ok(false)).unwrap();
    assert_ne!(log.slot(16), Some(stale));
    log.commit_open(4, 8192, &read, |_| Ok(false)).unwrap();
    let mut cleared = false;
    log.fold_into(
        |off, image| {
            assert!(
                !image.windows(8).any(|w| w == stale),
                "a rejected page already on the body must not be folded again"
            );
            if off == 16 && image.len() >= 8 && image[..8] == [0u8; 8] {
                cleared = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(
        cleared,
        "fold must write zeros over a rejected spend the body already has"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_body_survives_a_failed_clear() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, stale)];
    let err = log
        .commit_open(
            4,
            8192,
            |_off, _len| Err(StoreError::Corrupt("body read failed")),
            |_| Ok(false),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("body read failed")));
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a failed clear must leave the commit so a restart can retry"
    );
    assert_eq!(log.slot(16), Some(stale));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(stale));
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |_| Ok(false)).unwrap();
    assert_ne!(log.slot(16), Some(stale));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_clear_survives_a_partial_keep_read_error() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let live = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, live), (24, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let err = log
        .commit_open(
            4,
            8192,
            |_off, _len| Err(StoreError::Corrupt("body read failed")),
            |meta| Ok(meta[0] == 1),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("body read failed")));
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).exists());
    assert_eq!(log.slot(16), Some(live));
    assert_eq!(
        log.slot(24),
        Some(stale),
        "a failed clear must keep serving the old image"
    );
    let planted = [(24, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert_eq!(log.slot(16), Some(live));
    assert_ne!(log.slot(24), Some(stale));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_clear_survives_a_lower_checkpoint() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |_| Ok(false)).unwrap();
    assert_eq!(log.slot(16), Some([0u8; 8]));
    log.hold_fold_above(2).unwrap();
    log.commit_open(2, 8192, &read, |_| Ok(false)).unwrap();
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a later checkpoint must not delete the zero image"
    );
    assert_eq!(log.slot(16), Some([0u8; 8]));
    let mut cleared = false;
    log.fold_into(
        |off, image| {
            assert!(!image.windows(8).any(|w| w == stale));
            if off == 16 && image.len() >= 8 && image[..8] == [0u8; 8] {
                cleared = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(
        cleared,
        "fold must write the zeros the clear already stored"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn stored_zero_survives_a_note_during_the_hold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let strong = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |_| Ok(false)).unwrap();
    assert_eq!(log.slot(16), Some([0u8; 8]));
    log.note_slots(&[(4096, strong)]).unwrap();
    log.commit_open(4, 8192, &read, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(4096), Some(strong));
    log.hold_fold_above(2).unwrap();
    let mut noted = false;
    log.commit_open(2, 8192, &read, |meta| {
        if !noted {
            noted = true;
            log.note_slots(&[(24, fresh)]).unwrap();
        }
        let _ = meta;
        Ok(true)
    })
    .unwrap();
    assert!(noted);
    log.commit_open(2, 8192, &read, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(24), Some(fresh));
    assert_eq!(
        log.slot(16),
        Some([0u8; 8]),
        "a note on a zero page must not bring the rejected spend back"
    );
    log.commit_open(2, 8192, &read, |_| Ok(true)).unwrap();
    let mut wrote_stale = false;
    log.fold_into(
        |_off, image| {
            if image.windows(8).any(|w| w == stale) {
                wrote_stale = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(
        !wrote_stale,
        "fold must not write the rejected spend the zero image cleared"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn stored_zero_survives_a_strong_slot() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let strong = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, strong), (24, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(24, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert_eq!(log.slot(16), Some(strong));
    assert_eq!(log.slot(24), Some([0u8; 8]));
    log.hold_fold_above(2).unwrap();
    log.commit_open(2, 8192, &read, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(strong));
    assert_eq!(
        log.slot(24),
        Some([0u8; 8]),
        "a strong slot must not rebuild the page from the body"
    );
    let mut writes = 0u32;
    log.fold_into(
        |_off, image| {
            writes += 1;
            assert!(!image.windows(8).any(|w| w == stale));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0, "this checkpoint must not fold the held page");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn prior_zero_survives_a_new_reject() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let live = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, live), (24, stale), (32, fresh)])
        .unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let first_slots = [(24, stale)];
    let first = body_with(&first_slots);
    log.commit_open(4, 8192, &first, |meta| Ok(meta[0] != 2))
        .unwrap();
    assert_eq!(log.slot(24), Some([0u8; 8]));
    assert_eq!(log.slot(32), Some(fresh));
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    log.hold_fold_above(2).unwrap();
    let planted = [(24, stale), (32, fresh)];
    let read = body_with(&planted);
    log.commit_open(2, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert_eq!(log.slot(16), Some(live));
    assert_ne!(log.slot(32), Some(fresh));
    assert_eq!(
        log.slot(24),
        Some([0u8; 8]),
        "a stored zero must stay when another slot on the page is rejected"
    );
    log.commit_open(2, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    let mut wrote_stale = false;
    log.fold_into(
        |_off, image| {
            if image.windows(8).any(|w| w == stale) {
                wrote_stale = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(
        !wrote_stale,
        "fold must not write the spend the stored zero already cleared"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn prior_zero_survives_a_reject_the_body_lacks() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(24, stale), (32, fresh)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let first_slots = [(24, stale)];
    let first = body_with(&first_slots);
    log.commit_open(4, 8192, &first, |meta| Ok(meta[0] == 4))
        .unwrap();
    assert_eq!(log.slot(24), Some([0u8; 8]));
    assert_eq!(log.slot(32), Some(fresh));
    log.hold_fold_above(2).unwrap();
    let planted = [(24, stale)];
    let read = body_with(&planted);
    log.commit_open(2, 8192, &read, |_| Ok(false)).unwrap();
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a stored zero must keep the commit when the new reject is not on the body"
    );
    assert_eq!(log.slot(24), Some([0u8; 8]));
    assert_ne!(log.slot(32), Some(fresh));
    log.commit_open(2, 8192, &read, |_| Ok(false)).unwrap();
    let mut wrote_stale = false;
    log.fold_into(
        |_off, image| {
            if image.windows(8).any(|w| w == stale) {
                wrote_stale = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(!wrote_stale);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn strong_sibling_survives_a_reject_the_body_lacks() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let strong = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let reject = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(6, 12288, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(5).unwrap();
    let zero_slots = [(16, stale)];
    let zero_body = body_with(&zero_slots);
    log.commit_open(5, 12288, &zero_body, |_| Ok(false))
        .unwrap();
    assert_eq!(log.slot(16), Some([0u8; 8]));
    log.note_slots(&[(4096, strong), (8192, reject)]).unwrap();
    log.commit_open(5, 12288, &zero_body, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(4096), Some(strong));
    assert_eq!(log.slot(8192), Some(reject));
    log.hold_fold_above(2).unwrap();
    log.commit_open(2, 12288, &zero_body, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a best-chain page must keep the commit when another page is rejected"
    );
    assert_eq!(log.slot(4096), Some(strong));
    assert_eq!(log.slot(16), Some([0u8; 8]));
    assert_ne!(log.slot(8192), Some(reject));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(strong));
    assert_eq!(loaded.slot(16), Some([0u8; 8]));
    assert_ne!(loaded.slot(8192), Some(reject));
    drop(loaded);
    let mut wrote_strong = false;
    let mut wrote_reject = false;
    log.fold_into(
        |_off, image| {
            if image.windows(8).any(|w| w == strong) {
                wrote_strong = true;
            }
            if image.windows(8).any(|w| w == reject) {
                wrote_reject = true;
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert!(wrote_strong, "fold must write the best-chain sibling");
    assert!(!wrote_reject, "fold must not write the rejected page");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn strong_sibling_survives_a_reject_already_on_the_body() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let strong = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let stale = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale), (4096, strong)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let planted = [(16, stale)];
    let read = body_with(&planted);
    log.commit_open(4, 8192, &read, |meta| Ok(meta[0] == 1))
        .unwrap();
    assert_eq!(
        log.slot(4096),
        Some(strong),
        "a best-chain page must stay when the rejected page is cleared"
    );
    assert_eq!(log.slot(16), Some([0u8; 8]));
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(strong));
    assert_eq!(loaded.slot(16), Some([0u8; 8]));
    let mut wrote_stale = false;
    loaded
        .fold_into(
            |_off, image| {
                if image.windows(8).any(|w| w == stale) {
                    wrote_stale = true;
                }
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert!(!wrote_stale);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_ignores_a_commit_at_or_below_the_tip() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(9).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn short_commit_file_is_not_rewritten_by_the_hold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    let path = d.join(SPEND_PAGES_COMMIT_NAME);
    std::fs::write(&path, [0u8; 4]).unwrap();
    log.hold_fold_above(1).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len(), 4);
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0);
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn short_tail_is_logged_and_a_slot_past_published_is_corrupt() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(4, 100, zeros, |_| Ok(true)).unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 100);
    assert_eq!(&file[16..24], &slot);
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(4096, slot)]).unwrap();
    log.commit_open(4, 100, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(4096), None, "a page past the body is dropped");
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(96, slot)]).unwrap();
    log.commit_open(4, 100, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(96), None, "a slot past the body is dropped");
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    let err = log
        .commit_open(4, 8192, zeros, |_| Err(StoreError::Corrupt("keep")))
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("keep")));
    assert_eq!(log.slot(16), Some(slot));
    assert!(!d.join(SPEND_PAGES_COMMIT_NAME).exists());
    log.commit_open(4, 8192, |_off, _len| Ok(vec![7u8; 8]), |_| Ok(true))
        .unwrap();
    let file = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(file.len(), 4096);
    assert_eq!(file[0], 7);
    assert_eq!(file[8], 0);
    assert_eq!(&file[16..24], &slot);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fold_does_not_write_the_spent_file_header() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot), (4096, slot)]).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let mut offs = Vec::new();
    log.fold_into(
        |off, image| {
            offs.push((off, image.len()));
            if off == 16 {
                assert_eq!(&image[..8], &slot);
            }
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(offs, vec![(16, 4096 - 16), (4096, 4096)]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_after_publish_before_install_is_not_folded() {
    let d = dir();
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.prepare_commit(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    loaded.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(
        writes, 0,
        "a crash before install must not fold that generation"
    );
    assert_eq!(loaded.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.prepare_commit(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.install_commit().unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a hold before install must survive the directory swap"
    );
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fold_keeps_the_log_while_a_dirtied_page_is_still_open() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let a = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let b = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let extra = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, a)]).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096, b)]).unwrap();
    log.commit_open(
        3,
        8192,
        |off, len| {
            log.note_slots(&[(4096 + 8, extra)])?;
            zeros(off, len)
        },
        |_| Ok(true),
    )
    .unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, _image| {
            wrote.push(off);
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote, vec![16]);
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "fold must not delete the log while a published page is still open"
    );
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(b));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn keep_error_during_scrub_leaves_the_open_note() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    let err = log.commit_open(
        3,
        8192,
        |_off, len| {
            log.note_slots(&[(4096, later)]).unwrap();
            Ok(vec![0u8; len])
        },
        |meta| {
            if meta == &later {
                Err(StoreError::Corrupt("keep failed"))
            } else {
                Ok(true)
            }
        },
    );
    assert!(err.is_err());
    assert_eq!(log.slot(4096), Some(later));
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(4096), Some(later));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(4096), Some(later));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn explode_keep_error_keeps_the_image() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let other = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    let err = log
        .commit_open(1, 8192, zeros, |_| Err(StoreError::Corrupt("keep")))
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("keep")));
    assert_eq!(log.slot(16), Some(slot));
    log.note_slots(&[(4096, other)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(slot));
    assert_eq!(log.slot(4096), Some(other));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    assert_eq!(loaded.slot(4096), Some(other));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn pages_shorter_than_the_commit_are_corrupt() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let pages = d.join(SPEND_PAGES_NAME);
    let bytes = std::fs::read(&pages).unwrap();
    assert!(bytes.len() > 8);
    std::fs::write(&pages, &bytes[..8]).unwrap();
    assert!(matches!(
        SpendPageLog::load(&d),
        Err(StoreError::Corrupt("spend.pages short"))
    ));
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    std::fs::remove_file(d.join(SPEND_PAGES_NAME)).unwrap();
    assert!(matches!(
        SpendPageLog::load(&d),
        Err(StoreError::Corrupt("spend.pages short"))
    ));
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let commit = d.join(SPEND_PAGES_COMMIT_NAME);
    let mut buf = std::fs::read(&commit).unwrap();
    buf[16..24].copy_from_slice(&0u64.to_le_bytes());
    buf[24..28].copy_from_slice(&0u32.to_le_bytes());
    buf.truncate(28);
    std::fs::write(&commit, &buf).unwrap();
    std::fs::remove_file(d.join(SPEND_PAGES_NAME)).unwrap();
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rejected_open_page_is_not_published() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(false)).unwrap();
    assert_eq!(log.slot(16), None);
    assert!(!d.join(SPEND_PAGES_COMMIT_NAME).exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fold_of_page_zero_skips_the_file_header() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(2, 8192, |_off, len| Ok(vec![0xABu8; len]), |_| Ok(true))
        .unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, image| {
            wrote.push((off, image.to_vec()));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote.len(), 1);
    assert_eq!(
        wrote[0].0, 16,
        "page 0 fold must not pwrite the file header"
    );
    assert_eq!(wrote[0].1.len(), 4096 - 16);
    assert_eq!(&wrote[0].1[..8], &slot);
    assert_ne!(wrote[0].1[0], 0xAB);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_between_prepare_and_install_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.prepare_commit(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.install_commit().unwrap();
    let buf = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    assert_eq!(u16::from_le_bytes(buf[6..8].try_into().unwrap()), 1);
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a hold after publish and before install is not folded"
    );
    assert_eq!(log.slot(16), Some(slot));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    writes = 0;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(writes, 0, "reopen keeps the hold");
    log.commit_open(1, 8192, zeros, |_| Ok(false)).unwrap();
    writes = 0;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "the next checkpoint does not copy the abandoned spend"
    );
    assert_eq!(log.slot(16), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn note_during_a_rejected_clear_keeps_the_page_image() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [0xabu8; 8];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    let noted = std::sync::atomic::AtomicBool::new(false);
    log.commit_open(
        1,
        8192,
        |off, len| {
            if off == 0 && !noted.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log.note_slots(&[(32, [1, 2, 3, 4, 5, 6, 7, 8])]).unwrap();
            }
            zeros(off, len)
        },
        |_| Ok(false),
    )
    .unwrap();
    assert_eq!(
        log.slot(16),
        Some(stale),
        "a covered slot still answers from the generation image"
    );
    assert_eq!(
        log.slot(24),
        Some([0u8; 8]),
        "an explicit zero in that image suppresses the body"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_during_the_clearing_image_keeps_the_commit_bit() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let stale = [3u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, stale)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(4).unwrap();
    let calls = std::sync::atomic::AtomicU32::new(0);
    let planted = [(16u64, stale)];
    let read = body_with(&planted);
    log.commit_open(
        4,
        8192,
        |off, len| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1 {
                log.hold_fold_above(1)?;
            }
            read(off, len)
        },
        |_| Ok(false),
    )
    .unwrap();
    let buf = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    assert_eq!(
        u16::from_le_bytes(buf[6..8].try_into().unwrap()),
        1,
        "a hold during the clearing image must stay on the new commit"
    );
    let loaded = SpendPageLog::load(&d).unwrap();
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(writes, 0, "a restart must not fold the old-chain image");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn redirtied_page_stays_in_the_commit_when_an_older_page_folds() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [5u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(4096, slot)]).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(2, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(
        3,
        8192,
        |off, len| {
            log.note_slots(&[(24, later)])?;
            zeros(off, len)
        },
        |_| Ok(true),
    )
    .unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, _image| {
            wrote.push(off);
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote, vec![4096]);
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "fold must not delete a commit that still names an unfolded page"
    );
    assert_eq!(log.slot(16), Some(slot));
    assert_eq!(log.slot(24), Some(later));
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn missing_or_short_pages_file_refuses_open() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    let pages = d.join(SPEND_PAGES_NAME);
    let mut bytes = std::fs::read(&pages).unwrap();
    bytes.pop();
    std::fs::write(&pages, &bytes).unwrap();
    assert!(matches!(
        SpendPageLog::load(&d),
        Err(StoreError::Corrupt("spend.pages short"))
    ));
    std::fs::remove_file(&pages).unwrap();
    assert!(matches!(
        SpendPageLog::load(&d),
        Err(StoreError::Corrupt("spend.pages short"))
    ));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn explode_keeps_the_commit_when_keep_fails() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    let err = log
        .commit_open(1, 8192, zeros, |_| Err(StoreError::Corrupt("keep")))
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt("keep")));
    assert_eq!(log.slot(16), Some(slot));
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).exists());
    log.commit_open(1, 8192, zeros, |_| Ok(true)).unwrap();
    assert_eq!(log.slot(16), Some(slot));
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_during_fold_leaves_the_commit() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot), (4096, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, _image| {
            wrote.push(off);
            log.hold_fold_above(1)?;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(wrote, vec![16]);
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).exists());
    assert_eq!(log.slot(16), Some(slot));
    assert_eq!(log.slot(4096), Some(slot));
    let mut later = 0u32;
    log.fold_into(
        |_off, _image| {
            later += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(later, 0);
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot), (4096, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    let mut wrote = Vec::new();
    log.fold_into(
        |off, _image| {
            wrote.push(off);
            Ok(())
        },
        || {
            log.hold_fold_above(1)?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(wrote, vec![16, 4096]);
    assert!(
        d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a hold during the body sync must not delete the log"
    );
    assert_eq!(log.slot(16), Some(slot));
    assert_eq!(log.slot(4096), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

/// A damaged commit must not open. An unaligned offset, an image longer
/// than one page, or the same page named twice would make the fold write
/// that image at that offset.
#[test]
fn reload_refuses_a_commit_record_that_cannot_be_one_page() {
    fn plant(dir: &std::path::Path, recs: &[(u64, u64, u32)], file_len: usize) {
        use rbitcoin_primitives::{SCHEMA_VERSION, STORE_MAGIC};
        let n = recs.len();
        let mut buf = vec![0u8; 28 + n * 24 + 4];
        buf[0..4].copy_from_slice(&STORE_MAGIC);
        buf[4..6].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        buf[12..16].copy_from_slice(&1u32.to_le_bytes());
        buf[16..24].copy_from_slice(&(file_len as u64).to_le_bytes());
        buf[24..28].copy_from_slice(&(n as u32).to_le_bytes());
        for (i, (page, log_off, image_len)) in recs.iter().enumerate() {
            let at = 28 + i * 24;
            buf[at..at + 8].copy_from_slice(&page.to_le_bytes());
            buf[at + 8..at + 16].copy_from_slice(&log_off.to_le_bytes());
            buf[at + 16..at + 20].copy_from_slice(&image_len.to_le_bytes());
        }
        std::fs::write(dir.join(SPEND_PAGES_COMMIT_NAME), &buf).unwrap();
        std::fs::write(dir.join(SPEND_PAGES_NAME), vec![0u8; file_len]).unwrap();
    }
    let d = dir();
    plant(&d, &[(8, 0, 8)], 8);
    assert!(
        matches!(SpendPageLog::load(&d), Err(StoreError::Corrupt(_))),
        "an unaligned page offset must not open"
    );
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    plant(&d, &[(4096, 0, 8192)], 8192);
    assert!(
        matches!(SpendPageLog::load(&d), Err(StoreError::Corrupt(_))),
        "an image longer than one page must not open"
    );
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    plant(&d, &[(4096, 0, 8), (4096, 8, 8)], 16);
    assert!(
        matches!(SpendPageLog::load(&d), Err(StoreError::Corrupt(_))),
        "a page named twice must not open"
    );
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    plant(&d, &[(4096, 0, 8)], 8);
    assert!(
        SpendPageLog::load(&d).is_ok(),
        "a short tail image is one page"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn reload_drops_pages_without_a_commit_and_serves_a_bad_magic() {
    let d = dir();
    std::fs::write(d.join(SPEND_PAGES_NAME), [1u8; 8]).unwrap();
    let log = SpendPageLog::load(&d).unwrap();
    assert!(!d.join(SPEND_PAGES_NAME).exists());
    assert_eq!(log.slot(16), None);
    std::fs::write(d.join(SPEND_PAGES_COMMIT_NAME), [0u8; 28]).unwrap();
    assert!(matches!(SpendPageLog::load(&d), Err(StoreError::BadMagic)));
    std::fs::write(d.join(SPEND_PAGES_COMMIT_NAME), [0u8; 4]).unwrap();
    assert!(matches!(
        SpendPageLog::load(&d),
        Err(StoreError::Corrupt("spend.pages.commit short"))
    ));
    let _ = std::fs::remove_dir_all(&d);
}

/// Confirm publishes a longer `spent.body` end while fold copies cooled pages.
/// The fold must not store the shorter length it sampled.
#[test]
fn fold_does_not_publish_a_shorter_body_end() {
    use rbitcoin_primitives::Fk;
    use rbitcoin_store::Store;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    const PAGE: u64 = 4096;
    let d = dir();
    let store = Arc::new(Store::create_tiny(&d).unwrap());
    let nbytes = (256 * PAGE) as usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    store.txs.spent.finish_prepared(prep).unwrap();
    let end = store.txs.spent.body_published_len();
    let mut slots = Vec::new();
    let mut abs = 16u64;
    while abs.saturating_add(8) <= end {
        slots.push(abs);
        let next = (abs & !(PAGE - 1)).saturating_add(PAGE);
        if next <= abs {
            break;
        }
        abs = next;
    }
    assert!(
        slots.len() >= 64,
        "fold needs many pages, got {}",
        slots.len()
    );

    let mut saw_shrink = false;
    for _ in 0..4 {
        let mut edges = Vec::with_capacity(slots.len());
        let mut known = Vec::with_capacity(slots.len());
        for (i, &abs) in slots.iter().enumerate() {
            edges.push((abs, Fk(1), 0, Fk(10 + i as u64), 0));
            known.push((Fk(1), 0u8, 0));
        }
        let cold = store
            .put_spend_batch_by_abs_meta_known(&edges, &known)
            .unwrap();
        assert!(cold.is_empty());
        store.checkpoint_observed_spend().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let shrunk = Arc::new(AtomicBool::new(false));
        let base = store.txs.spent.body.logical_len();
        let landed = Arc::new(AtomicU64::new(base));
        let next = Arc::new(AtomicU64::new(base.saturating_add(8)));
        let cap = base.saturating_add(32 * 1024 * 1024);
        let racer = {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            let shrunk = Arc::clone(&shrunk);
            let landed = Arc::clone(&landed);
            let next = Arc::clone(&next);
            std::thread::spawn(move || {
                let spent = &store.txs.spent;
                while !stop.load(Ordering::Relaxed) {
                    let n = next.fetch_add(8, Ordering::Relaxed);
                    if n >= cap {
                        if spent.body.logical_len() < cap - 8 {
                            shrunk.store(true, Ordering::Release);
                            break;
                        }
                        std::hint::spin_loop();
                        continue;
                    }
                    let _ = spent.note_body_end(n, true);
                    if spent.body.logical_len() < n {
                        shrunk.store(true, Ordering::Release);
                        break;
                    }
                    landed.fetch_max(n, Ordering::Release);
                }
            })
        };
        let started = next.load(Ordering::Relaxed);
        while next.load(Ordering::Relaxed) == started
            && started < cap
            && !shrunk.load(Ordering::Relaxed)
        {
            std::hint::spin_loop();
        }
        store.checkpoint_observed_spend().unwrap();
        stop.store(true, Ordering::Release);
        racer.join().unwrap();
        let end = store.txs.spent.body.logical_len();
        let posted = landed.load(Ordering::Acquire);
        if shrunk.load(Ordering::Acquire) || end < posted {
            saw_shrink = true;
            break;
        }
        assert!(
            !d.join(SPEND_PAGES_COMMIT_NAME).exists(),
            "the second checkpoint must fold the cooled pages"
        );
    }
    assert!(
        !saw_shrink,
        "fold stored a spent.body end confirm had already grown past"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn reload_drops_a_page_past_the_repaired_body_and_cuts_a_straddle() {
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(4096, slot)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let loaded = SpendPageLog::load_clipped(&d, 4096).unwrap();
    assert_eq!(loaded.slot(4096), None, "a page at the new end is gone");
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot), (96, later)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let loaded = SpendPageLog::load_clipped(&d, 100).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    assert_eq!(loaded.slot(96), None, "a slot past the cut is gone");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn review_1501_fix_clipped_commit_is_durable() {
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    let later = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot), (4096, later)]).unwrap();
    log.cover_confirmed_tip(9);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    let pages = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load_clipped(&d, 4096).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    assert_eq!(loaded.slot(4096), None);
    drop(loaded);
    assert_eq!(
        std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap(),
        pages,
        "clipping rewrites the commit, not the image file"
    );
    let buf = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    assert_eq!(u16::from_le_bytes(buf[6..8].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(buf[12..16].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 1);
    let sealed_at = 28 + 24;
    assert_eq!(
        u32::from_le_bytes(buf[sealed_at..sealed_at + 4].try_into().unwrap()),
        9
    );
    let again = SpendPageLog::load(&d).unwrap();
    assert_eq!(again.slot(16), Some(slot));
    assert_eq!(
        again.slot(4096),
        None,
        "a later open must not bring back the clipped page"
    );
    let mut writes = 0u32;
    again
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(writes, 0, "the held bit survives the clipped commit");
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot), (96, later)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let pages = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load_clipped(&d, 100).unwrap();
    assert_eq!(loaded.slot(96), None);
    drop(loaded);
    assert_eq!(std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap(), pages);
    let buf = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    let image_len = u32::from_le_bytes(buf[44..48].try_into().unwrap());
    assert_eq!(image_len, 100, "the commit names the shortened image");
    let again = SpendPageLog::load(&d).unwrap();
    assert_eq!(again.slot(16), Some(slot));
    assert_eq!(again.slot(96), None);
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let before = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load_clipped(&d, 8192).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    drop(loaded);
    assert_eq!(
        std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap(),
        before,
        "a clip that drops nothing leaves the commit file alone"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn freeze_tip_above_the_snapshot_holds_the_fold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    log.cover_confirmed_tip(9);
    log.hold_fold_above(5).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a freeze tip above the disconnect holds the fold"
    );
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn second_disconnect_during_deferred_compose_keeps_the_hold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let rejected = [9u8, 1, 2, 3, 4, 5, 6, 7];
    let kept = [8u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, rejected), (4096, kept)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.commit_open(
        1,
        8192,
        |off, len| {
            log.hold_fold_above(0)?;
            zeros(off, len)
        },
        |meta| Ok(*meta != rejected),
    )
    .unwrap();
    let buf = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    assert_eq!(u16::from_le_bytes(buf[6..8].try_into().unwrap()), 1);
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a second disconnect during compose is not folded"
    );
    assert_eq!(log.slot(4096), Some(kept));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn prepare_keeps_the_old_image_until_install() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let rejected = [6u8, 1, 2, 3, 4, 5, 6, 7];
    let kept = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, rejected), (4096, kept)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    log.prepare_commit(1, 8192, zeros, |meta| Ok(*meta != rejected))
        .unwrap();
    assert_eq!(
        log.slot(16),
        Some(rejected),
        "the old image stays until install"
    );
    assert_eq!(log.slot(4096), Some(kept));
    log.install_commit().unwrap();
    assert_eq!(log.slot(4096), Some(kept));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn skip_rewrites_a_logged_spend_the_body_does_not_hold() {
    use rbitcoin_primitives::Fk;
    use rbitcoin_store::Store;
    let d = dir();
    let store = Store::create_tiny(&d).unwrap();
    let nbytes = 4096usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    let abs = prep.start;
    store.txs.spent.finish_prepared(prep).unwrap();
    store.note_open_spend_slots(&[(abs, [0u8; 8])]).unwrap();
    let cold = store
        .put_spend_batch_by_abs_meta_known(&[(abs, Fk(1), 0, Fk(77), 0)], &[(Fk(77), 0, 0)])
        .unwrap();
    assert!(cold.is_empty());
    let got = store.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(
        got[0].unwrap().0,
        Fk(77),
        "a zero image must not hide the spend"
    );
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let store = Store::create_tiny(&d).unwrap();
    let mut blob_prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    blob_prep.body_blob[0..8].copy_from_slice(&[0, 77, 0, 0, 0, 0, 0, 0]);
    let abs = blob_prep.start;
    store
        .txs
        .spent
        .write_body_blob_bulk(blob_prep.start, &blob_prep.body_blob)
        .unwrap();
    store.txs.spent.finish_prepared(blob_prep).unwrap();
    let cold = store
        .put_spend_batch_by_abs_meta_known(&[(abs, Fk(1), 0, Fk(77), 0)], &[(Fk(77), 0, 0)])
        .unwrap();
    assert!(cold.is_empty());
    store.checkpoint_observed_spend().unwrap();
    assert!(
        !d.join(SPEND_PAGES_COMMIT_NAME).exists(),
        "a body that already has the spend is not noted again"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn review_1501_fix_put_spend_logged_slot() {
    use rbitcoin_primitives::Fk;
    use rbitcoin_store::point_table::put_spend_on_create_at;
    use rbitcoin_store::tx_table::encode_spent_slot;
    use rbitcoin_store::Store;
    let d = dir();
    let store = Store::create_tiny(&d).unwrap();
    let nbytes = 4096usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    let abs = prep.start;
    let len = prep.body_blob.len() as u64;
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    store.txs.spent.finish_prepared(prep).unwrap();
    store.note_open_spend_slots(&[(abs, [0u8; 8])]).unwrap();
    store.checkpoint_observed_spend().unwrap();
    put_spend_on_create_at(
        &store.txs,
        &store.spenders,
        Fk(1),
        0,
        Fk(9),
        4,
        Some((abs, len)),
    )
    .unwrap();
    let got = store.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(
        got[0].map(|(field, _flags, vin)| (field, vin)),
        Some((Fk(9), 4)),
        "a logged zero must not hide the new spend"
    );
    // The next checkpoint keeps a best-chain sole spend and must not fold
    // the older zero image back over it.
    store
        .confirmed
        .set(
            &rbitcoin_store::ClassCWrite::for_test(),
            rbitcoin_primitives::Height(0),
            Fk(1),
        )
        .unwrap();
    store
        .header_txs
        .put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk(1), Fk(1), 9)
        .unwrap();
    store
        .strong_tx
        .set_strong(&rbitcoin_store::ClassCWrite::for_test(), Fk(9), Fk(1))
        .unwrap();
    store.rebuild_height_fence().unwrap();
    store.checkpoint_observed_spend().unwrap();
    store.checkpoint_observed_spend().unwrap();
    let got = store.get_spender_meta_at_abs_batch(&[abs]).unwrap();
    assert_eq!(
        got[0].map(|(field, _flags, vin)| (field, vin)),
        Some((Fk(9), 4))
    );
    let expect = encode_spent_slot(0, Fk(9), 4).unwrap();
    assert_eq!(store.txs.spent.pread_span(abs, 8).unwrap(), expect);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn review_1501_fix_put_spend_tail_stays_on_the_body() {
    use rbitcoin_primitives::Fk;
    use rbitcoin_store::point_table::put_spend_on_create_at;
    use rbitcoin_store::tx_table::encode_spent_slot;
    use rbitcoin_store::Store;
    let d = dir();
    let store = Store::create_tiny(&d).unwrap();
    let nbytes = 64usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    let abs = prep.start;
    let len = prep.body_blob.len() as u64;
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    assert!(
        abs.saturating_add(8) > store.txs.spent.body_published_len(),
        "the overlay tail is not published yet"
    );
    put_spend_on_create_at(
        &store.txs,
        &store.spenders,
        Fk(1),
        0,
        Fk(9),
        4,
        Some((abs, len)),
    )
    .unwrap();
    let expect = encode_spent_slot(0, Fk(9), 4).unwrap();
    assert_eq!(
        store.txs.spent.pread_span(abs, 8).unwrap(),
        expect,
        "a same-batch tail spend stays on spent.body"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn widen_holds_a_loaded_commit_above_the_checkpoint() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    loaded.widen_for_checkpoint(4);
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(
        writes, 0,
        "a loaded commit above this checkpoint is not folded"
    );
    assert_eq!(loaded.slot(16), Some(slot));
    loaded.widen_for_checkpoint(9);
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(
        writes, 0,
        "the hold from the higher commit stays until the image is filtered"
    );
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    loaded.widen_for_checkpoint(3);
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(writes, 1, "a commit at this checkpoint still folds");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn reloaded_freeze_tip_holds_the_fold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.cover_confirmed_tip(9);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    loaded.hold_fold_above(5).unwrap();
    loaded.widen_cool_through();
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(writes, 0, "a reloaded freeze tip still holds the fold");
    assert_eq!(loaded.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);

    let d = dir();
    let log = SpendPageLog::create(&d);
    log.note_slots(&[(16, slot)]).unwrap();
    log.cover_confirmed_tip(9);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    let commit = d.join(SPEND_PAGES_COMMIT_NAME);
    let buf = std::fs::read(&commit).unwrap();
    std::fs::write(&commit, &buf[..buf.len() - 4]).unwrap();
    let loaded = SpendPageLog::load(&d).unwrap();
    loaded.hold_fold_above(5).unwrap();
    loaded.widen_cool_through();
    let mut writes = 0u32;
    loaded
        .fold_into(
            |_off, _image| {
                writes += 1;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(
        writes, 1,
        "a commit with no freeze tip uses its snapshot height"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(all(unix, debug_assertions))]
struct ResetDirSync;

#[cfg(all(unix, debug_assertions))]
impl Drop for ResetDirSync {
    fn drop(&mut self) {
        rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    }
}

#[cfg(all(unix, debug_assertions))]
#[test]
fn directory_sync_failure_after_commit_rename_keeps_the_generation() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    // The generation file syncs the directory once. The commit sync is next.
    let _reset = ResetDirSync;
    rbitcoin_store::file::arm_fail_dir_sync_after(1, 1);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(first));
    let second = [3u8, 1, 2, 3, 4, 5, 6, 7];
    loaded.note_slots(&[(4096, second)]).unwrap();
    loaded.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    let commit = std::fs::read(d.join(SPEND_PAGES_COMMIT_NAME)).unwrap();
    let generation = u32::from_le_bytes(commit[12..16].try_into().unwrap());
    assert_eq!(
        generation, 2,
        "a failed directory sync must not reuse generation 1"
    );
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(first));
    assert_eq!(loaded.slot(4096), Some(second));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(all(unix, debug_assertions))]
#[test]
fn directory_sync_failure_leaves_the_generation_file() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let first = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, first)]).unwrap();
    // The generation file syncs the directory once. The commit sync and the
    // pages-rename sync are the next two.
    let _reset = ResetDirSync;
    rbitcoin_store::file::arm_fail_dir_sync_after(1, 2);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    assert!(
        d.join(format!("{SPEND_PAGES_NAME}.1")).is_file(),
        "a failed commit directory sync must leave the generation file"
    );
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).is_file());
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(first));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn disconnect_during_held_keep_is_not_folded() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(1).unwrap();
    let mut seen = 0u32;
    log.commit_open(1, 8192, zeros, |_| {
        seen += 1;
        if seen == 1 {
            log.hold_fold_above(1).unwrap();
            return Ok(true);
        }
        Ok(false)
    })
    .unwrap();
    assert_eq!(log.slot(16), Some(slot));
    log.widen_for_checkpoint(1);
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a disconnect during the held keep must not fold the old image"
    );
    assert!(d.join(SPEND_PAGES_COMMIT_NAME).is_file());
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(all(unix, debug_assertions))]
#[test]
fn directory_sync_failure_keeps_the_page_file_after_fold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    let _reset = ResetDirSync;
    // Leave spend.pages.1 in place. The fold's directory sync is the one
    // that fails, after the commit unlink.
    rbitcoin_store::file::arm_fail_dir_sync_after(1, 2);
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    let gen = d.join(format!("{SPEND_PAGES_NAME}.1"));
    assert!(gen.is_file());
    let commit = d.join(SPEND_PAGES_COMMIT_NAME);
    let commit_bytes = std::fs::read(&commit).unwrap();
    log.commit_open(4, 8192, zeros, |_| Ok(true)).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 1);
    log.fold_into(|_off, _image| Ok(()), || Ok(())).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    assert!(
        gen.is_file(),
        "a failed directory sync after the commit unlink must leave the page file"
    );
    if !commit.is_file() {
        std::fs::write(&commit, &commit_bytes).unwrap();
    }
    drop(log);
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(all(unix, debug_assertions))]
#[test]
fn directory_sync_failure_during_hold_still_holds() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    let _reset = ResetDirSync;
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 1);
    log.hold_fold_above(1).unwrap();
    rbitcoin_store::file::arm_fail_dir_sync_after(0, 0);
    log.widen_cool_through();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0, "a failed directory sync still holds the fold");
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(debug_assertions)]
thread_local! {
    static REVIEW_ABS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(debug_assertions)]
fn review_1501_freeze_probe(
    spenders: &rbitcoin_store::spender_table::SpenderTable,
    log: &SpendPageLog,
) {
    let e1 = match spenders.append(rbitcoin_primitives::Fk(2), 0, rbitcoin_primitives::Fk::NULL) {
        Ok(fk) => fk,
        Err(_) => panic!("overflow append failed"),
    };
    let e2 = match spenders.append(rbitcoin_primitives::Fk(3), 1, e1) {
        Ok(fk) => fk,
        Err(_) => panic!("overflow append failed"),
    };
    let meta = match rbitcoin_store::tx_table::encode_spent_slot(
        rbitcoin_store::output_flags::MULTI_SPENDER,
        e2,
        0,
    ) {
        Ok(meta) => meta,
        Err(_) => panic!("multi slot encode failed"),
    };
    let abs = REVIEW_ABS.with(|slot| slot.get());
    match log.note_slots(&[(abs, meta)]) {
        Ok(()) => {}
        Err(_) => panic!("note during freeze failed"),
    }
    rbitcoin_store::io_handle::arm_fail_sync(spenders.body.read_fd(), 1);
}

#[cfg(debug_assertions)]
struct ClearFreezeProbe(rbitcoin_store::io_handle::IoHandle);

#[cfg(debug_assertions)]
impl Drop for ClearFreezeProbe {
    fn drop(&mut self) {
        rbitcoin_store::Store::disarm_spend_freeze_probe();
        rbitcoin_store::io_handle::disarm_sync(self.0);
    }
}

/// A multi-list node noted after the early overflow sync must not be named by
/// the commit unless a second sync made that node durable.
#[cfg(debug_assertions)]
#[test]
fn review_1501_ovf_sync() {
    let d = dir();
    let s = rbitcoin_store::Store::create_tiny(&d).unwrap();
    s.confirmed
        .set(
            &rbitcoin_store::ClassCWrite::for_test(),
            rbitcoin_primitives::Height(0),
            rbitcoin_primitives::Fk(1),
        )
        .unwrap();
    let tx = rbitcoin_store::TxRecord {
        txid: [0x11; 32],
        version: 1,
        locktime: 0,
        input_count: 1,
        output_count: 1,
    };
    let inputs = vec![rbitcoin_store::InputRecord {
        prev_txid: [0u8; 32],
        parent: rbitcoin_store::InputParent::Coinbase,
        sequence: u32::MAX,
        script_sig: vec![0x01],
        witness: vec![],
    }];
    let outputs = vec![rbitcoin_store::OutputRecord::unspent(1, vec![0x51])];
    let fks = s
        .txs
        .put_full_batch_indexed(&[(tx, inputs, outputs)], false)
        .unwrap();
    let (off, len) = s.txs.spent_range(fks[0]).unwrap();
    let abs = rbitcoin_store::spent_abs(off, 0);
    assert!(
        rbitcoin_store::spend_page_log::is_historical(s.txs.spent.body_published_len(), abs),
        "the body must already cover the slot"
    );
    REVIEW_ABS.with(|slot| slot.set(abs));
    let _clear = ClearFreezeProbe(s.spenders.body.read_fd());
    rbitcoin_store::Store::arm_spend_freeze_probe(review_1501_freeze_probe);
    let err = match s.checkpoint_spend_through(0) {
        Err(err) => err,
        Ok(()) => {
            panic!("a multi-list note after the early sync was committed without a second ovf sync")
        }
    };
    let msg = err.to_string();
    assert!(
        msg.contains("spent.ovf"),
        "the second overflow sync must fail before the commit rename"
    );
    drop(s);
    let loaded = rbitcoin_store::Store::open(&d).unwrap();
    let (multi, field, vin) = loaded.txs.get_output_spender_meta_at(off, len, 0).unwrap();
    assert!(
        !multi && field.is_null() && vin == 0,
        "a failed overflow sync must not leave the multi slot in the commit"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A note taken after the published length is sampled is composed at the
/// length the freeze records.
#[test]
fn review_1501_published_sample() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.prepare_commit_in(
        1,
        100,
        zeros,
        |_| Ok(true),
        |page| {
            page.note_slots(&[(200, slot)])?;
            page.freeze_open();
            page.cover_published_len(4096);
            Ok(())
        },
        |_| Ok(1),
    )
    .unwrap();
    log.install_commit().unwrap();
    assert_eq!(
        log.slot(200),
        Some(slot),
        "a note past the earlier published length must stay in the commit"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(debug_assertions)]
struct ClearHide;

#[cfg(debug_assertions)]
impl Drop for ClearHide {
    fn drop(&mut self) {
        rbitcoin_store::io_handle::disarm_hide_after_pread();
    }
}

/// The batch reader must see the page log even when a pread of `spent.body`
/// happens before the log would have been checked.
#[cfg(debug_assertions)]
#[test]
fn review_1501_meta_order() {
    let d = dir();
    let s = rbitcoin_store::Store::create_tiny(&d).unwrap();
    let tx = rbitcoin_store::TxRecord {
        txid: [0x22; 32],
        version: 1,
        locktime: 0,
        input_count: 1,
        output_count: 1,
    };
    let inputs = vec![rbitcoin_store::InputRecord {
        prev_txid: [0u8; 32],
        parent: rbitcoin_store::InputParent::Coinbase,
        sequence: u32::MAX,
        script_sig: vec![0x01],
        witness: vec![],
    }];
    let outputs = vec![rbitcoin_store::OutputRecord::unspent(1, vec![0x51])];
    let fks = s
        .txs
        .put_full_batch_indexed(&[(tx, inputs, outputs)], false)
        .unwrap();
    let (off, len) = s.txs.spent_range(fks[0]).unwrap();
    let abs = rbitcoin_store::spent_abs(off, 0);
    let slot =
        rbitcoin_store::tx_table::encode_spent_slot(0, rbitcoin_primitives::Fk(9), 1).unwrap();
    s.note_open_spend_slots(&[(abs, slot)]).unwrap();
    let _clear = ClearHide;
    rbitcoin_store::io_handle::arm_hide_after_pread();
    let metas = s.txs.get_output_spender_metas_at(off, len, &[0]).unwrap();
    assert_eq!(
        metas,
        vec![(0, false, rbitcoin_primitives::Fk(9), 1)],
        "a body pread between the log read and the answer must not hide the spend"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A spend the second keep accepts above the freeze tip raises that tip, so
/// a later disconnect holds and does not fold it.
#[test]
fn review_1501_sealed_tip() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.cover_confirmed_tip(1);
    log.note_slots(&[(16, slot)]).unwrap();
    log.prepare_commit_in(
        1,
        8192,
        zeros,
        |_| Ok(true),
        |page| {
            page.freeze_open();
            Ok(())
        },
        |_| Ok(5),
    )
    .unwrap();
    log.install_commit().unwrap();
    log.hold_fold_above(1).unwrap();
    log.widen_for_checkpoint(1);
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 0,
        "a spend reconnected above the freeze must not be folded"
    );
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(debug_assertions)]
struct FoldGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(debug_assertions)]
thread_local! {
    static FOLD_GATE: std::cell::RefCell<Option<FoldGate>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(debug_assertions)]
fn review_1501_fold_probe() {
    let gate = FOLD_GATE.with(|slot| slot.borrow_mut().take());
    let Some(gate) = gate else {
        return;
    };
    let _ = gate.entered.send(());
    let _ = gate.release.recv();
}

#[cfg(debug_assertions)]
struct ClearFoldProbe;

#[cfg(debug_assertions)]
impl Drop for ClearFoldProbe {
    fn drop(&mut self) {
        rbitcoin_store::Store::disarm_spend_fold_probe();
        FOLD_GATE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

/// The cooled-page copy holds the `--datadir-hdd` spindle until it returns.
/// Flag off does not lock.
#[cfg(debug_assertions)]
#[test]
fn review_1501_fold_spindle() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = dir();
    let s = rbitcoin_store::Store::create_tiny(&d).unwrap();
    let _clear = ClearFoldProbe;
    for hdd in [true, false] {
        s.set_datadir_hdd(hdd);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                FOLD_GATE.with(|slot| {
                    *slot.borrow_mut() = Some(FoldGate {
                        entered: entered_tx,
                        release: release_rx,
                    });
                });
                rbitcoin_store::Store::arm_spend_fold_probe(review_1501_fold_probe);
                s.checkpoint_spend_through(0)
                    .expect("checkpoint reaches the fold");
            });
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("fold probe");
            scope.spawn(|| {
                s.spindle_wait();
                let _ = done_tx.send(());
            });
            let early = done_rx.recv_timeout(Duration::from_millis(300));
            release_tx.send(()).expect("release fold");
            if hdd {
                assert!(
                    early.is_err(),
                    "the next Class A burst can start while the fold is copying"
                );
                done_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("waiter finishes after the fold drops the spindle");
            } else {
                early.expect("flag off does not lock the fold");
            }
        });
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

/// A published page is the generation file. A zero in that file hides the body.
///
/// The body a later compose would read is a different pattern. Lookup returns
/// the file, including an explicit zero. A slot past the short image is not
/// covered. Fold copies the file bytes, skipping the 16-byte header on page 0,
/// and the directory then misses the page.
#[test]
fn published_image_is_the_generation_file_until_fold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let spend = [4u8, 1, 2, 3, 4, 5, 6, 7];
    let neighbor = [9u8, 9, 9, 9, 9, 9, 9, 9];
    let zero = [0u8; 8];
    let mut body = [0xabu8; 200];
    body[16..24].copy_from_slice(&spend);
    body[24..32].copy_from_slice(&neighbor);
    let published = body.len() as u64;
    log.note_slots(&[(16, spend)]).unwrap();
    log.commit_open(
        1,
        published,
        |_off, len| Ok(body[..len].to_vec()),
        |_| Ok(true),
    )
    .unwrap();
    log.note_slots(&[(24, zero)]).unwrap();
    let later_body = [0x11u8; 200];
    log.commit_open(
        2,
        published,
        |_off, len| Ok(later_body[..len].to_vec()),
        |_| Ok(true),
    )
    .unwrap();
    let carried = std::fs::read(d.join(SPEND_PAGES_NAME)).unwrap();
    assert_eq!(&carried[16..24], &spend);
    assert_eq!(&carried[24..32], &zero);
    assert_eq!(&carried[32..40], &body[32..40]);
    assert_ne!(&carried[32..40], &later_body[32..40]);
    assert_eq!(log.slot(16), Some(spend));
    assert_eq!(
        log.slot(24),
        Some(zero),
        "an explicit zero in the generation file hides the body"
    );
    assert_eq!(log.slot(32), Some(body[32..40].try_into().unwrap()));
    assert_eq!(
        log.slot(published),
        None,
        "a slot past the published image stays on spent.body"
    );
    assert_eq!(
        log.resident_image_bytes(),
        0,
        "the installed directory keeps file coordinates, not page images"
    );
    log.commit_open(
        3,
        published,
        |_off, len| Ok(later_body[..len].to_vec()),
        |_| Ok(true),
    )
    .unwrap();
    let mut folded = None;
    log.fold_into(
        |off, image| {
            folded = Some((off, image.to_vec()));
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    let (off, image) = folded.expect("fold copies the generation file");
    assert_eq!(off, 16);
    assert_eq!(
        image,
        carried[16..],
        "fold writes the generation-file bytes past the page-0 header"
    );
    assert_eq!(log.slot(16), None, "a folded page leaves the directory");
    assert_eq!(log.slot(24), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// The distinct-page budget arms inside 64k..=128k and stays armed while the
/// open set is still over it. The edge cap is unchanged.
#[test]
fn page_budget_stays_armed_and_the_edge_cap_is_unchanged() {
    use std::sync::{Arc, Condvar, Mutex};

    assert_eq!(OPEN_EDGE_CAP, 2_000_000);
    let budget = rbitcoin_store::spend_page_log::OPEN_PAGE_BUDGET;
    assert!(
        (65_536..=131_072).contains(&budget),
        "the page budget must sit in the 64k..=128k band"
    );
    assert!(!spend_checkpoint_early(0, budget - 1, 0));
    assert!(spend_checkpoint_early(0, budget, 0));
    assert!(spend_checkpoint_early(OPEN_EDGE_CAP, 0, 0));
    assert_eq!(
        pace_after_checkpoint(0, budget, 0, true),
        (true, true),
        "a full page budget keeps requesting after the image follow-up is used"
    );
    assert_eq!(
        pace_after_checkpoint(0, budget - 1, 0, true),
        (false, false)
    );
    assert!(spend_note_arms(0, budget, 0, true));
    assert!(!spend_note_arms(0, budget - 1, 0, true));

    let d = dir();
    let log = SpendPageLog::create(&d);
    let wake = Arc::new((Mutex::new(false), Condvar::new()));
    log.bind_sync_wake(Arc::clone(&wake));
    let slot = [3u8, 1, 2, 3, 4, 5, 6, 7];
    let mut batch = Vec::with_capacity(1024);
    for i in 0..budget {
        batch.push((i * 4096, slot));
        if batch.len() == 1024 {
            log.note_slots(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        log.note_slots(&batch).unwrap();
    }
    assert!(
        log.checkpoint_early(),
        "noting one slot on each budget page must arm a checkpoint"
    );
    log.relax_pace();
    assert!(
        log.checkpoint_early(),
        "relaxing the pace must keep the checkpoint armed while those pages are open"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Freeze wakes an edge-cap waiter before the cooled body copy runs.
#[cfg(debug_assertions)]
#[test]
fn freeze_wakes_the_edge_cap_before_the_cooled_copy() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    let d = dir();
    let store = rbitcoin_store::Store::create_tiny(&d).unwrap();
    let nbytes = 4096usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    store.txs.spent.finish_prepared(prep).unwrap();
    let slot =
        rbitcoin_store::tx_table::encode_spent_slot(0, rbitcoin_primitives::Fk::NULL, 0).unwrap();
    store.note_open_spend_slots(&[(16, slot)]).unwrap();
    store.checkpoint_spend_through(0).unwrap();
    assert_eq!(store.spend_directory_slot(16), Some(slot));

    let mut batch = Vec::with_capacity(4096);
    let mut n = 0u64;
    while n < OPEN_EDGE_CAP {
        let page = 1 + n / 512;
        let rel = n % 512;
        batch.push((page * 4096 + rel * 8, slot));
        n += 1;
        if batch.len() == 4096 {
            store.note_open_spend_slots(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.note_open_spend_slots(&batch).unwrap();
    }
    let wake = Arc::new((Mutex::new(false), Condvar::new()));
    store.bind_spend_sync_wake(Arc::clone(&wake));
    let (lock, cv) = &*wake;
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let released = AtomicBool::new(false);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            store.wait_spend_edge_room();
            released.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        });
        let (_guard, waited) = cv
            .wait_timeout(guard, Duration::from_secs(30))
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            !waited.timed_out(),
            "confirm must be blocked on the edge cap before the checkpoint"
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let checkpoint = scope.spawn(|| {
            FOLD_GATE.with(|slot| {
                *slot.borrow_mut() = Some(FoldGate {
                    entered: entered_tx,
                    release: release_rx,
                });
            });
            rbitcoin_store::Store::arm_spend_fold_probe(review_1501_fold_probe);
            store.checkpoint_spend_through(0)
        });
        entered_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the cooled copy must start");
        let woke = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).expect("release fold");
        woke.expect("freeze must wake the edge-cap waiter before the cooled copy finishes");
        assert!(released.load(Ordering::SeqCst));
        checkpoint
            .join()
            .unwrap()
            .expect("checkpoint finishes after the copy");
    });
    rbitcoin_store::Store::disarm_spend_fold_probe();
    assert_eq!(
        store.spend_directory_slot(16),
        None,
        "the cooled page is copied from the generation file before the next one is assembled"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(debug_assertions)]
thread_local! {
    static SEAL_DURING_COPY: std::cell::RefCell<Option<std::sync::Arc<rbitcoin_store::Store>>> =
        const { std::cell::RefCell::new(None) };
    static SEAL_PROBE_RAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(debug_assertions)]
fn seal_during_copy_probe() {
    SEAL_PROBE_RAN.with(|ran| ran.set(true));
    SEAL_DURING_COPY.with(|slot| {
        let store = slot.borrow();
        let store = store.as_ref().expect("store");
        assert!(
            !store.spend_log_above(7),
            "publish_open_spend_pages must not seal tip 8 before the cooled copy"
        );
        assert!(
            store.spend_log_above(4),
            "the generation sealed at 5 stays above 4 during the copy"
        );
    });
}

#[cfg(debug_assertions)]
struct ClearSealProbe;

#[cfg(debug_assertions)]
impl Drop for ClearSealProbe {
    fn drop(&mut self) {
        rbitcoin_store::Store::disarm_spend_fold_probe();
        SEAL_DURING_COPY.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

/// The freeze before the cooled copy must not stamp the new tip.
///
/// The generation being copied was sealed at an older checkpoint. A disconnect
/// below the new tip and at or above that generation has to fold those pages.
/// Stamping the new tip first holds the fold, and the next checkpoint
/// republishes them.
#[test]
fn early_freeze_does_not_seal_the_tip_before_the_cooled_copy() {
    let d = dir();
    let store = std::sync::Arc::new(rbitcoin_store::Store::create_tiny(&d).unwrap());
    let nbytes = 8192usize;
    let prep = store
        .txs
        .spent
        .prepare_batch_encode(1, nbytes, |_i, buf| {
            buf.resize(buf.len() + nbytes, 0);
        })
        .unwrap()
        .expect("one spent append");
    store
        .txs
        .spent
        .write_body_blob_bulk(prep.start, &prep.body_blob)
        .unwrap();
    store.txs.spent.finish_prepared(prep).unwrap();
    let slot =
        rbitcoin_store::tx_table::encode_spent_slot(0, rbitcoin_primitives::Fk::NULL, 0).unwrap();
    store.note_open_spend_slots(&[(16, slot)]).unwrap();
    store.checkpoint_spend_through(5).unwrap();
    assert!(
        store.spend_log_above(4),
        "the generation sealed at 5 is above 4"
    );
    assert!(
        !store.spend_log_above(7),
        "7 is still above that sealed generation"
    );
    store.note_open_spend_slots(&[(4096, slot)]).unwrap();
    store.txs.freeze_open_spend_pages();
    assert!(
        !store.spend_log_above(7),
        "freezing the open pages does not raise the sealed tip"
    );
    assert!(store.spend_log_above(4), "the older seal stays in place");
    #[cfg(debug_assertions)]
    {
        let _clear = ClearSealProbe;
        SEAL_PROBE_RAN.with(|ran| ran.set(false));
        SEAL_DURING_COPY.with(|slot| *slot.borrow_mut() = Some(std::sync::Arc::clone(&store)));
        rbitcoin_store::Store::arm_spend_fold_probe(seal_during_copy_probe);
        store.checkpoint_spend_through(8).unwrap();
        assert!(
            SEAL_PROBE_RAN.with(|ran| ran.get()),
            "the cooled copy must run while the tip is still unsealed"
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A disconnect to 7 during the cooled copy still folds a generation sealed at 5.
///
/// The early freeze must not stamp tip 8. `hold_fold_above(7)` then sees no
/// height above 7, and the quiet page is copied.
#[test]
fn disconnect_during_the_cooled_copy_still_folds_the_sealed_generation() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [1u8, 1, 2, 3, 4, 5, 6, 7];
    let fresh = [2u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.note_slots(&[(4096, fresh)]).unwrap();
    log.widen_for_checkpoint(8);
    log.freeze_before_cooled_copy();
    log.hold_fold_above(7).unwrap();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        writes, 1,
        "a quiet page sealed at 5 must fold when the tip falls to 7"
    );
    assert_eq!(log.slot(16), None);
    assert_eq!(log.slot(4096), Some(fresh));
    let _ = std::fs::remove_dir_all(&d);
}

/// The early swap leaves `frozen` occupied and does not stage a snapshot.
///
/// Restored slots parked in `open` fill the edge cap, and the callback freeze
/// is a reuse that does not reset it. They stay in `frozen` instead.
#[test]
fn held_restore_after_the_early_swap_does_not_fill_the_edge_cap() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let mut noted = Vec::with_capacity(4);
    for rel in 0..4u16 {
        noted.push((4096 + u64::from(rel) * 8, slot));
    }
    log.note_slots(&noted).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.hold_fold_above(0).unwrap();
    let fresh = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, fresh)]).unwrap();
    log.freeze_before_cooled_copy();
    assert_eq!(
        log.open_edge_count(),
        0,
        "the early swap moves the open note out of the edge cap"
    );
    log.prepare_commit_in(
        0,
        8192,
        zeros,
        |_| Ok(true),
        |page| {
            assert_eq!(
                page.open_edge_count(),
                0,
                "restored slots must stay out of the edge cap when the early freeze only swapped"
            );
            page.freeze_open();
            Ok(())
        },
        |_| Ok(0),
    )
    .unwrap();
    log.install_commit().unwrap();
    assert_eq!(log.slot(4096), Some(slot));
    assert_eq!(log.slot(16), Some(fresh));
    let _ = std::fs::remove_dir_all(&d);
}

/// An empty early freeze leaves `frozen` empty, so the callback freeze is a
/// fresh swap and drops the edge count.
#[test]
fn empty_early_freeze_still_resets_edges_on_the_callback_freeze() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [7u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(5, 8192, zeros, |_| Ok(true)).unwrap();
    log.freeze_before_cooled_copy();
    log.note_slots(&[(4096, slot)]).unwrap();
    assert_eq!(log.open_edge_count(), 1);
    log.freeze_open();
    assert_eq!(
        log.open_edge_count(),
        0,
        "an empty early freeze leaves the callback freeze as a fresh swap"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A freeze already staged must not count a held restore toward the edge cap.
///
/// The first freeze leaves `frozen` non-empty, so the callback freeze is a
/// reuse and does not reset `open_edges`. Restored slots parked in `open`
/// fill that cap until install, and confirm blocks for the whole compose.
#[test]
fn held_restore_after_a_staged_freeze_does_not_fill_the_edge_cap() {
    use std::sync::{Arc, Condvar, Mutex};

    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [5u8, 1, 2, 3, 4, 5, 6, 7];
    let mut batch = Vec::with_capacity(4096);
    let mut n = 0u64;
    while n < OPEN_EDGE_CAP {
        let page = 1 + n / 512;
        let rel = n % 512;
        batch.push((page * 4096 + rel * 8, slot));
        n += 1;
        if batch.len() == 4096 {
            log.note_slots(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        log.note_slots(&batch).unwrap();
    }
    let pages = OPEN_EDGE_CAP.div_ceil(512);
    let published = (1 + pages) * 4096;
    log.commit_open(5, published, |_off, len| Ok(vec![0u8; len]), |_| Ok(true))
        .unwrap();
    log.hold_fold_above(0).unwrap();
    let fresh = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, fresh)]).unwrap();
    log.freeze_open();
    let wake = Arc::new((Mutex::new(false), Condvar::new()));
    log.bind_sync_wake(Arc::clone(&wake));
    log.prepare_commit_in(
        0,
        published,
        |_off, len| Ok(vec![0u8; len]),
        |_| Ok(true),
        |page| {
            assert!(
                page.open_edge_count() < OPEN_EDGE_CAP,
                "restored slots must not fill the edge cap before the callback freeze"
            );
            page.wait_open_edge_room();
            assert_eq!(page.slot(4096), Some(slot));
            page.freeze_open();
            Ok(())
        },
        |_| Ok(0),
    )
    .unwrap();
    log.install_commit().unwrap();
    assert_eq!(log.slot(4096), Some(slot));
    assert_eq!(log.slot(16), Some(fresh));
    let _ = std::fs::remove_dir_all(&d);
}

/// A spender sync that fails after the freeze must still wake confirm.
///
/// The freeze has already moved the open notes and dropped the edge count.
/// Confirm is asleep on that cap. The error return never reaches install.
#[test]
fn review_1501_failed_freeze_wakes_the_edge_wait() {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    let d = dir();
    let log = Arc::new(SpendPageLog::create(&d));
    let slot = [8u8, 1, 2, 3, 4, 5, 6, 7];
    let mut batch = Vec::with_capacity(4096);
    let mut n = 0u64;
    while n < OPEN_EDGE_CAP {
        let page = n / 512;
        let rel = n % 512;
        batch.push((page * 4096 + rel * 8, slot));
        n += 1;
        if batch.len() == 4096 {
            log.note_slots(&batch).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        log.note_slots(&batch).unwrap();
    }
    let wake = Arc::new((Mutex::new(false), Condvar::new()));
    log.bind_sync_wake(Arc::clone(&wake));
    let log_wait = Arc::clone(&log);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (lock, cv) = &*wake;
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let worker = std::thread::spawn(move || {
        log_wait.wait_open_edge_room();
        let _ = done_tx.send(());
    });
    let (_guard, waited) = cv
        .wait_timeout(guard, Duration::from_secs(5))
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        !waited.timed_out(),
        "confirm must reach the edge wait before the freeze fails"
    );
    let err = log.prepare_commit_in(
        1,
        8192,
        zeros,
        |_| Ok(true),
        |page| {
            page.freeze_open();
            Err(StoreError::io(
                std::path::Path::new("spent.ovf"),
                std::io::Error::other("sync failed"),
            ))
        },
        |_| Ok(1),
    );
    assert!(err.is_err(), "the sync after the freeze must fail");
    assert_eq!(
        log.slot(16),
        Some(slot),
        "a failed freeze must keep the spend it already took"
    );
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("a failed freeze must wake confirm waiting at the edge cap");
    worker.join().unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// A checkpoint that returns without freezing must not freeze on its own.
///
/// The second freeze would run outside the utxo view. That is corrupt.
#[test]
fn review_1501_unstaged_freeze_is_corrupt() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = dir();
    let log = std::sync::Arc::new(SpendPageLog::create(&d));
    let log_wait = std::sync::Arc::clone(&log);
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let err =
            log_wait.prepare_commit_in(1, 8192, zeros, |_| Ok(true), |_page| Ok(()), |_| Ok(1));
        let _ = tx.send(err);
    });
    let got = rx.recv_timeout(Duration::from_secs(2));
    match got {
        Ok(Err(StoreError::Corrupt("spend freeze was not staged"))) => {}
        other => panic!("an unstaged freeze must be corrupt, got {other:?}"),
    }
    worker.join().unwrap();
    log.prepare_commit_in(
        1,
        8192,
        zeros,
        |_| Ok(true),
        |page| {
            page.freeze_open();
            Ok(())
        },
        |_| Ok(1),
    )
    .expect("an empty freeze is a real stage");
    let _ = std::fs::remove_dir_all(&d);
}

/// A commit read that fails after the tip is already down must still hold.
///
/// The next checkpoint would fold that generation onto `spent.body`.
#[test]
fn review_1501_hold_patch_failure_still_blocks_the_fold() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [9u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(9, 8192, zeros, |_| Ok(true)).unwrap();
    let commit = d.join(SPEND_PAGES_COMMIT_NAME);
    std::fs::remove_file(&commit).unwrap();
    std::fs::create_dir(&commit).unwrap();
    let err = log.hold_fold_above(1).unwrap_err();
    assert!(
        matches!(err, StoreError::Io { .. }),
        "a failed hold patch must surface the io error, got {err:?}"
    );
    log.widen_cool_through();
    let mut writes = 0u32;
    log.fold_into(
        |_off, _image| {
            writes += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(writes, 0, "a failed hold patch must still block the fold");
    assert_eq!(log.slot(16), Some(slot));
    let _ = std::fs::remove_dir_all(&d);
}

/// Page files stay when the commit unlink fails.
///
/// A directory sync after that does not mean the commit file is gone.
#[test]
fn review_1501_commit_unlink_failure_keeps_the_pages() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [4u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(3, 8192, zeros, |_| Ok(true)).unwrap();
    log.widen_cool_through();
    let commit = d.join(SPEND_PAGES_COMMIT_NAME);
    let pages = d.join(SPEND_PAGES_NAME);
    assert!(pages.is_file());
    std::fs::remove_file(&commit).unwrap();
    std::fs::create_dir(&commit).unwrap();
    log.fold_into(|_off, _image| Ok(()), || Ok(())).unwrap();
    assert!(
        pages.is_file(),
        "a failed commit unlink must leave the page file"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn spend_commit_reload_serves_the_slot_and_rejects_short() {
    let d = dir();
    let log = SpendPageLog::create(&d);
    let slot = [6u8, 1, 2, 3, 4, 5, 6, 7];
    log.note_slots(&[(16, slot)]).unwrap();
    log.commit_open(2, 64, zeros, |_| Ok(true)).unwrap();
    let loaded = SpendPageLog::load(&d).unwrap();
    assert_eq!(loaded.slot(16), Some(slot));
    std::fs::write(d.join(SPEND_PAGES_COMMIT_NAME), b"tiny").unwrap();
    match SpendPageLog::load(&d) {
        Err(StoreError::Corrupt("spend.pages.commit short")) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("a short commit loaded"),
    }
    let _ = std::fs::remove_dir_all(&d);
}
