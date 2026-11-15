use rbitcoin_primitives::Height;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::FILE_HEADER_LEN;
use rbitcoin_store::sp_tweaks::*;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

fn tmp_dir() -> rbitcoin_store::testutil::TempDir {
    rbitcoin_store::testutil::TempDir::labeled("sptweaks").unwrap()
}

fn set_file_hwm(path: &Path, hwm: u64) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let hwm_len = hwm.max(FILE_HEADER_LEN as u64);
    f.set_len(hwm_len).unwrap();
    // Grow fallocates from offset 0, so leave sparse capacity past the HWM
    // or a write near u32::MAX allocates the whole 4 GiB hole.
    f.set_len(hwm_len + (1 << 20)).unwrap();
    let mut hdr = [0u8; FILE_HEADER_LEN];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut hdr).unwrap();
    hdr[8..16].copy_from_slice(&hwm.to_le_bytes());
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&hdr).unwrap();
}

#[test]
fn put_get_len_tweak_no_txid() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut tweak = [0u8; 33];
    tweak[0] = 0x02;
    tweak[32] = 0xab;
    t.put_block(Height(0), &[None, Some(tweak), None]).unwrap();

    let got = t.get_block(Height(0), 3).unwrap().expect("present");
    assert_eq!(got, vec![None, Some(tweak), None]);

    let want = SpTweaksTable::encode_records(&[None, Some(tweak), None]);
    assert_eq!(want.len(), 1 + 1 + 33 + 1);
    assert_eq!(want[0], 0);
    assert_eq!(want[1], 33);
    let raw = fs::read(SpTweaksTable::seg_dirs(&dir).body_path(0)).unwrap();
    let pub_len = t.body_logical_len() as usize;
    assert_eq!(pub_len, FILE_HEADER_LEN + want.len());
    assert_eq!(&raw[FILE_HEADER_LEN..pub_len], want.as_slice());

    // Idx slot is u32 off only (no header_fk).
    let idx = fs::read(SpTweaksTable::seg_dirs(&dir).idx_path(0)).unwrap();
    let idx_hwm = u64::from_le_bytes(idx[8..16].try_into().unwrap()) as usize;
    assert_eq!(idx_hwm, FILE_HEADER_LEN + 4);
    assert_eq!(
        u32::from_le_bytes(
            idx[FILE_HEADER_LEN..FILE_HEADER_LEN + 4]
                .try_into()
                .unwrap()
        ),
        FILE_HEADER_LEN as u32
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_syncs_body_then_idx() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_blocks(&[
        (Height(0), &[None] as &[Option<[u8; 33]>]),
        (Height(1), &[None, None]),
    ])
    .unwrap();
    let g = t.lock();
    let seg = &g.segs[0];
    assert!(!seg.body.pending_sync(), "body synced before its idx slots");
    assert!(!seg.idx.pending_sync(), "idx synced before put returns");
    drop(g);
    let _ = fs::remove_dir_all(&dir);
}

/// An idx slot that reached disk ahead of its body (pre-sync builds):
/// the short record's slot drops and the one before it is clean.
#[test]
fn trim_last_record_drops_a_short_record() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut tw = [0u8; 33];
    tw[0] = 0x02;
    t.put_block(Height(0), &[None]).unwrap();
    t.put_block(Height(1), &[Some(tw), None]).unwrap();
    drop(t);
    let body = SpTweaksTable::seg_dirs(&dir).body_path(0);
    set_file_hwm(&body, FILE_HEADER_LEN as u64 + 1 + 10);

    let t = SpTweaksTable::open(&dir).unwrap();
    assert_eq!(t.trim_last_record(2).unwrap(), TrimLast::Dropped);
    assert_eq!(t.next_height(), Height(1));
    assert_eq!(t.trim_last_record(1).unwrap(), TrimLast::Clean);
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![None]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_blocks_concatenates_consecutive_heights() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut a = [0u8; 33];
    a[0] = 0x02;
    a[32] = 0xaa;
    let mut b = [0u8; 33];
    b[0] = 0x03;
    b[32] = 0xbb;
    t.put_blocks(&[
        (Height(0), &[None, Some(a)] as &[Option<[u8; 33]>]),
        (Height(1), &[None]),
        (Height(2), &[Some(b), None]),
    ])
    .unwrap();

    assert_eq!(t.next_height(), Height(3));
    assert_eq!(
        t.get_block(Height(0), 2).unwrap().unwrap(),
        vec![None, Some(a)]
    );
    assert_eq!(t.get_block(Height(1), 1).unwrap().unwrap(), vec![None]);
    assert_eq!(
        t.get_block(Height(2), 2).unwrap().unwrap(),
        vec![Some(b), None]
    );

    let blob0 = SpTweaksTable::encode_records(&[None, Some(a)]);
    let blob1 = SpTweaksTable::encode_records(&[None]);
    let blob2 = SpTweaksTable::encode_records(&[Some(b), None]);
    let mut want = Vec::new();
    want.extend_from_slice(&blob0);
    want.extend_from_slice(&blob1);
    want.extend_from_slice(&blob2);
    let raw = fs::read(SpTweaksTable::seg_dirs(&dir).body_path(0)).unwrap();
    let pub_len = t.body_logical_len() as usize;
    assert_eq!(pub_len, FILE_HEADER_LEN + want.len());
    assert_eq!(&raw[FILE_HEADER_LEN..pub_len], want.as_slice());

    let idx = fs::read(SpTweaksTable::seg_dirs(&dir).idx_path(0)).unwrap();
    let idx_hwm = u64::from_le_bytes(idx[8..16].try_into().unwrap()) as usize;
    assert_eq!(idx_hwm, FILE_HEADER_LEN + 12);
    let off0 = u32::from_le_bytes(
        idx[FILE_HEADER_LEN..FILE_HEADER_LEN + 4]
            .try_into()
            .unwrap(),
    );
    let off1 = u32::from_le_bytes(
        idx[FILE_HEADER_LEN + 4..FILE_HEADER_LEN + 8]
            .try_into()
            .unwrap(),
    );
    let off2 = u32::from_le_bytes(
        idx[FILE_HEADER_LEN + 8..FILE_HEADER_LEN + 12]
            .try_into()
            .unwrap(),
    );
    assert_eq!(off0, FILE_HEADER_LEN as u32);
    assert_eq!(off1, off0 + blob0.len() as u32);
    assert_eq!(off2, off1 + blob1.len() as u32);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rolls_new_segment_when_next_start_exceeds_u32() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.flush().unwrap();
    drop(t);

    // Last (only) record lives at u32::MAX; HWM is one past so the next start rolls.
    let body = SpTweaksTable::seg_dirs(&dir).body_path(0);
    let idx = SpTweaksTable::seg_dirs(&dir).idx_path(0);
    let start = u32::MAX;
    set_file_hwm(&body, u64::from(start) + 1);
    {
        let mut f = fs::OpenOptions::new().write(true).open(&body).unwrap();
        f.seek(SeekFrom::Start(u64::from(start))).unwrap();
        f.write_all(&[0u8]).unwrap();
    }
    {
        let mut raw = fs::read(&idx).unwrap();
        raw[FILE_HEADER_LEN..FILE_HEADER_LEN + 4].copy_from_slice(&start.to_le_bytes());
        fs::write(&idx, &raw).unwrap();
    }

    let t = SpTweaksTable::open(&dir).unwrap();
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![None]);
    t.put_block(Height(1), &[None, None])
        .expect("must roll instead of u32-off Corrupt");
    assert!(SpTweaksTable::seg_dirs(&dir).body_path(1).is_file());
    assert_eq!(
        t.get_block(Height(1), 2).unwrap().unwrap(),
        vec![None, None]
    );
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![None]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_block_body_may_pass_u32_if_start_fits() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.flush().unwrap();
    drop(t);

    let body = SpTweaksTable::seg_dirs(&dir).body_path(0);
    let near = u32::MAX - 8;
    set_file_hwm(&body, u64::from(near));
    let t = SpTweaksTable::open(&dir).unwrap();
    let fat: Vec<Option<[u8; 33]>> = vec![None; 32];
    t.put_block(Height(1), &fat)
        .expect("start < 2^32 may extend body past u32::MAX");
    assert!(
        t.body_logical_len() > u32::MAX as u64,
        "this height's blob must be allowed to pass 4 GiB"
    );
    assert_eq!(t.get_block(Height(1), 32).unwrap().unwrap(), fat);
    t.put_block(Height(2), &[None])
        .expect("next start > u32::MAX must roll, not Corrupt");
    assert!(SpTweaksTable::seg_dirs(&dir).body_path(1).is_file());
    assert_eq!(t.get_block(Height(2), 1).unwrap().unwrap(), vec![None]);
    assert_eq!(t.get_block(Height(1), 32).unwrap().unwrap(), fat);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_blocks_rolls_mid_batch_when_next_start_exceeds_u32() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.flush().unwrap();
    drop(t);

    let body = SpTweaksTable::seg_dirs(&dir).body_path(0);
    let near = u32::MAX - 4;
    set_file_hwm(&body, u64::from(near));
    let t = SpTweaksTable::open(&dir).unwrap();
    let fat: Vec<Option<[u8; 33]>> = vec![None; 20];
    t.put_blocks(&[
        (Height(1), fat.as_slice()),
        (Height(2), &[None] as &[Option<[u8; 33]>]),
        (Height(3), &[None, None]),
    ])
    .expect("mid-batch start past u32 must roll, not Corrupt");
    assert!(SpTweaksTable::seg_dirs(&dir).body_path(1).is_file());
    assert_eq!(t.get_block(Height(1), 20).unwrap().unwrap(), fat);
    assert_eq!(t.get_block(Height(2), 1).unwrap().unwrap(), vec![None]);
    assert_eq!(
        t.get_block(Height(3), 2).unwrap().unwrap(),
        vec![None, None]
    );
    assert_eq!(t.next_height(), Height(4));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reorg_truncates_and_regenerates_tip_height() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut a = [0u8; 33];
    a[0] = 0x02;
    let mut b = [0u8; 33];
    b[0] = 0x03;
    t.put_block(Height(0), &[Some(a)]).unwrap();
    t.put_block(Height(1), &[None, Some(a)]).unwrap();
    t.truncate_above(Height(0)).unwrap();
    assert_eq!(t.next_height(), Height(1));
    assert!(t.get_block(Height(1), 2).unwrap().is_none());
    t.put_block(Height(1), &[Some(b)]).unwrap();
    assert_eq!(t.get_block(Height(1), 1).unwrap().unwrap(), vec![Some(b)]);
    assert_eq!(t.get_eligible(Height(1), 1).unwrap().unwrap(), vec![(0, b)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn get_eligible_range_matches_singles_and_stops_on_hole() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut a = [0u8; 33];
    a[0] = 0x02;
    let mut b = [0u8; 33];
    b[0] = 0x03;
    t.put_block(Height(0), &[None, Some(a)]).unwrap();
    t.put_block(Height(1), &[None]).unwrap();
    t.put_block(Height(2), &[Some(b), None]).unwrap();

    let got = t
        .get_eligible_range(Height(0), &[2, 1, 2])
        .unwrap()
        .expect("indexed");
    assert_eq!(
        got,
        vec![
            t.get_eligible(Height(0), 2).unwrap().unwrap(),
            t.get_eligible(Height(1), 1).unwrap().unwrap(),
            t.get_eligible(Height(2), 2).unwrap().unwrap(),
        ]
    );

    assert!(t.get_eligible_range(Height(3), &[1]).unwrap().is_none());
    assert!(t
        .get_eligible_range(Height(0), &[])
        .unwrap()
        .unwrap()
        .is_empty());

    let prefix = t
        .get_eligible_range(Height(2), &[2, 1])
        .unwrap()
        .expect("start indexed");
    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix[0], vec![(0, b)]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn hole_vs_empty_eligible() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(10)).unwrap();
    t.put_block(Height(10), &[None, None]).unwrap();
    let empty = t.get_block(Height(10), 2).unwrap().expect("present");
    assert_eq!(empty, vec![None, None]);
    assert!(t.get_block(Height(11), 1).unwrap().is_none());
    assert!(t.get_block(Height(9), 1).unwrap().is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_reads_origin_and_refuses_a_short_meta() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(11)).unwrap();
    drop(t);
    let t = SpTweaksTable::open(&dir).unwrap();
    assert_eq!(t.origin_height(), Height(11));
    drop(t);

    set_file_hwm(
        &SpTweaksTable::meta_path(dir.path()),
        FILE_HEADER_LEN as u64 + 4,
    );
    let err = match SpTweaksTable::open(&dir) {
        Ok(_) => panic!("open accepted a short meta"),
        Err(err) => err,
    };
    assert!(
        matches!(err, StoreError::Corrupt(m) if m.contains("meta short")),
        "{err:?}"
    );
}

#[test]
fn open_refuses_a_foreign_idx_format() {
    let dir = tmp_dir();
    SpTweaksTable::create(&dir, Height(11)).unwrap();
    let meta = SpTweaksTable::meta_path(dir.path());
    let mut raw = fs::read(&meta).unwrap();
    raw[FILE_HEADER_LEN + 4..FILE_HEADER_LEN + 8].copy_from_slice(&9u32.to_le_bytes());
    fs::write(&meta, &raw).unwrap();
    let err = match SpTweaksTable::open(&dir) {
        Ok(_) => panic!("open accepted a foreign idx format"),
        Err(err) => err,
    };
    assert!(
        matches!(err, StoreError::Corrupt(m) if m.contains("idx format")),
        "{err:?}"
    );
}

#[test]
fn truncate_above_and_reopen() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut a = [0u8; 33];
    a[0] = 0x03;
    t.put_block(Height(0), &[Some(a)]).unwrap();
    t.put_block(Height(1), &[None]).unwrap();
    t.put_block(Height(2), &[None, None]).unwrap();
    assert_eq!(t.next_height(), Height(3));
    t.truncate_above(Height(0)).unwrap();
    assert_eq!(t.next_height(), Height(1));
    assert!(t.get_block(Height(1), 1).unwrap().is_none());
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![Some(a)]);

    t.flush().unwrap();
    drop(t);
    let t = SpTweaksTable::open(&dir).unwrap();
    assert_eq!(t.origin_height(), Height(0));
    assert_eq!(t.next_height(), Height(1));
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![Some(a)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_files_open_or_create_not_corrupt() {
    let dir = tmp_dir();
    let t = SpTweaksTable::open_or_create(&dir, Height(5)).unwrap();
    assert_eq!(t.origin_height(), Height(5));
    assert_eq!(t.slot_count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_or_create_discards_legacy_files() {
    let dir = tmp_dir();
    fs::write(SpTweaksTable::idx_dir(&dir), b"old-idx").unwrap();
    fs::write(SpTweaksTable::body_dir(&dir), b"old-body").unwrap();
    assert!(SpTweaksTable::legacy_files_present(&dir));
    let t = SpTweaksTable::open_or_create(&dir, Height(0)).unwrap();
    assert!(!SpTweaksTable::legacy_files_present(&dir));
    assert!(SpTweaksTable::files_present(&dir));
    t.put_block(Height(0), &[None]).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn refuse_fat_len() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.flush().unwrap();
    drop(t);
    let mut raw = fs::read(SpTweaksTable::seg_dirs(&dir).body_path(0)).unwrap();
    raw[FILE_HEADER_LEN] = 32;
    fs::write(SpTweaksTable::seg_dirs(&dir).body_path(0), &raw).unwrap();
    let t = SpTweaksTable::open(&dir).unwrap();
    let err = t.get_block(Height(0), 1).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(m) if m.contains("bad len")),
        "got {err:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_not_next_height_errors() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let err = t.put_block(Height(2), &[None]).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_or_create_repairs_incomplete_and_checks_origin() {
    let dir = tmp_dir();
    fs::create_dir_all(SpTweaksTable::idx_dir(&dir)).unwrap();
    fs::write(SpTweaksTable::idx_dir(&dir).join("partial"), b"x").unwrap();
    let t = SpTweaksTable::open_or_create(&dir, Height(3)).unwrap();
    assert_eq!(t.origin_height(), Height(3));
    drop(t);
    let Err(err) = SpTweaksTable::open_or_create(&dir, Height(9)) else {
        panic!("expected origin mismatch");
    };
    assert!(
        matches!(err, StoreError::Corrupt(m) if m.contains("origin")),
        "{err:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_below_origin_and_n_tx_mismatch() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(5)).unwrap();
    assert!(matches!(
        t.put_block(Height(4), &[None]),
        Err(StoreError::Corrupt(_))
    ));
    t.put_block(Height(5), &[None]).unwrap();
    let err = t.get_block(Height(5), 2).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
    t.truncate_through_tip(None).unwrap();
    assert_eq!(t.slot_count(), 0);
    t.truncate_above(Height(99)).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_short_tweak_and_n_tx_mismatch() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    let mut tw = [0u8; 33];
    tw[0] = 0x02;
    t.put_block(Height(0), &[Some(tw)]).unwrap();
    t.flush().unwrap();
    drop(t);
    let p = SpTweaksTable::seg_dirs(&dir).body_path(0);
    let mut raw = fs::read(&p).unwrap();
    raw.truncate(FILE_HEADER_LEN + 1 + 8);
    fs::write(&p, &raw).unwrap();
    let t = SpTweaksTable::open(&dir).unwrap();
    let err = t.get_block(Height(0), 1).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn get_two_heights_uses_next_slot_off() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.put_block(Height(1), &[None, None]).unwrap();
    assert_eq!(t.get_block(Height(0), 1).unwrap().unwrap(), vec![None]);
    assert_eq!(
        t.get_block(Height(1), 2).unwrap().unwrap(),
        vec![None, None]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_offs_run_reads_the_next_slot() {
    let dir = tmp_dir();
    let t = SpTweaksTable::create(&dir, Height(0)).unwrap();
    t.put_block(Height(0), &[None]).unwrap();
    t.put_block(Height(1), &[None, None]).unwrap();
    let g = t.lock();
    let seg = &g.segs[0];
    let (starts, end) = SpTweaksTable::read_offs_run(seg, 0, 1).unwrap();
    assert_eq!(starts, vec![FILE_HEADER_LEN as u32]);
    assert_eq!(end, FILE_HEADER_LEN as u64 + 1);
    let (starts, end) = SpTweaksTable::read_offs_run(seg, 1, 1).unwrap();
    assert_eq!(starts, vec![FILE_HEADER_LEN as u32 + 1]);
    assert_eq!(end, seg.body.logical_len());
    let err = match SpTweaksTable::read_offs_run(seg, 0, 3) {
        Ok(_) => panic!("run accepted a past-segment read"),
        Err(err) => err,
    };
    assert!(
        matches!(err, StoreError::Corrupt(m) if m.contains("run past segment")),
        "{err:?}"
    );
}
