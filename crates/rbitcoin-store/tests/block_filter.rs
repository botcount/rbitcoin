use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::block_filter::*;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::FILE_HEADER_LEN;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

fn tmp_dir() -> rbitcoin_store::testutil::TempDir {
    rbitcoin_store::testutil::TempDir::labeled("blockfilter").unwrap()
}

fn slot(h: u32) -> BlockFilterSlot {
    BlockFilterSlot {
        header_fk: Fk(u64::from(h) + 100),
        filter_hash: [h as u8; 32],
        filter_header: [h as u8 ^ 0xff; 32],
    }
}

fn put_range(t: &BlockFilterTable, from: u32, to: u32) {
    let bodies: Vec<Vec<u8>> = (from..=to).map(|h| vec![h as u8; 3 + h as usize]).collect();
    let recs: Vec<BlockFilterRecord<'_>> = (from..=to)
        .zip(&bodies)
        .map(|(h, b)| BlockFilterRecord {
            height: Height(h),
            slot: slot(h),
            filter: b,
        })
        .collect();
    t.put(&recs).unwrap();
}

fn set_file_hwm(path: &Path, hwm: u64) {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.set_len(hwm.max(FILE_HEADER_LEN as u64)).unwrap();
    let mut hdr = [0u8; FILE_HEADER_LEN];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut hdr).unwrap();
    hdr[8..16].copy_from_slice(&hwm.to_le_bytes());
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&hdr).unwrap();
}

#[test]
fn filter_at_height_reads_only_its_record_and_survives_reopen() {
    let dir = tmp_dir();
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    assert_eq!(t.next_height(), Height(0));
    assert!(t.filter(Height(0)).unwrap().is_none());
    put_range(&t, 0, 9);
    assert_eq!(t.next_height(), Height(10));
    drop(t);

    // Scribble over every other record's body bytes: a lookup must not
    // walk earlier records to find its own.
    let body = BlockFilterTable::seg_dirs(&dir).body_path(0);
    let mut raw = fs::read(&body).unwrap();
    let start7 = FILE_HEADER_LEN + (0..7).map(|h| 3 + h).sum::<usize>();
    let len7 = 3 + 7;
    for (i, b) in raw.iter_mut().enumerate().skip(FILE_HEADER_LEN) {
        if !(start7..start7 + len7).contains(&i) {
            *b = 0xee;
        }
    }
    fs::write(&body, &raw).unwrap();

    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    assert_eq!(t.next_height(), Height(10));
    let (bytes, s) = t.filter(Height(7)).unwrap().unwrap();
    assert_eq!(bytes, vec![7u8; 10]);
    assert_eq!(s, slot(7));
    assert_eq!(t.slot(Height(9)).unwrap(), Some(slot(9)));
    assert_eq!(
        t.slots(Height(3), Height(9)).unwrap().unwrap(),
        (3..=9).map(slot).collect::<Vec<_>>()
    );
    assert!(t.slots(Height(3), Height(10)).unwrap().is_none());
    let run = t.filters(Height(6), Height(8)).unwrap().unwrap();
    assert_eq!(
        run[1],
        (vec![7u8; 10], slot(7)),
        "range read matches single"
    );
    assert!(t.slot(Height(10)).unwrap().is_none());
    let gap = t.put(&[BlockFilterRecord {
        height: Height(11),
        slot: slot(11),
        filter: &[1],
    }]);
    assert!(matches!(gap, Err(StoreError::Corrupt(m)) if m.contains("next height")));
}

/// Checkpoint headers are heights 1000, 2000, … and nothing in between.
#[test]
fn checkpoint_headers_are_every_thousandth_slot() {
    let dir = tmp_dir();
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    put_range(&t, 0, 2_500);
    assert_eq!(
        t.checkpoint_headers(Height(999)).unwrap(),
        Some(Vec::<[u8; 32]>::new())
    );
    assert_eq!(
        t.checkpoint_headers(Height(2_500)).unwrap(),
        Some(vec![slot(1_000).filter_header, slot(2_000).filter_header])
    );
    assert!(t.checkpoint_headers(Height(2_501)).unwrap().is_none());
}

/// Crash states a live session cannot produce on demand: a commit that
/// synced body bytes but not its slots, then an idx HWM covering a slot
/// whose record is cut short. Open keeps the committed prefix each time.
#[test]
fn open_keeps_the_committed_prefix_after_torn_commits() {
    let dir = tmp_dir();
    let body = BlockFilterTable::seg_dirs(&dir).body_path(0);
    let end_of = |h: u64| FILE_HEADER_LEN as u64 + (0..=h).map(|i| 3 + i).sum::<u64>();
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    put_range(&t, 0, 4);
    drop(t);

    set_file_hwm(&body, end_of(4) + 50);
    {
        let mut f = fs::OpenOptions::new().write(true).open(&body).unwrap();
        f.seek(SeekFrom::Start(end_of(4))).unwrap();
        f.write_all(&[0xab; 50]).unwrap();
    }
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    assert_eq!(t.next_height(), Height(5));
    assert_eq!(t.filter(Height(4)).unwrap().unwrap().0, vec![4u8; 7]);
    put_range(&t, 5, 5);
    assert_eq!(t.filter(Height(5)).unwrap().unwrap().0, vec![5u8; 8]);
    drop(t);

    set_file_hwm(&body, end_of(2) + 1);
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    assert_eq!(t.next_height(), Height(3), "record 3 is torn");
    put_range(&t, 3, 4);
    assert_eq!(t.filter(Height(4)).unwrap().unwrap().0, vec![4u8; 7]);
}

#[test]
fn rolls_a_segment_when_the_next_start_passes_u32() {
    let dir = tmp_dir();
    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    put_range(&t, 0, 0);
    drop(t);
    // Record 0 now ends one past u32::MAX (sparse body, slot len widened).
    let big = u32::MAX - FILE_HEADER_LEN as u32 + 1;
    let idx = BlockFilterTable::seg_dirs(&dir).idx_path(0);
    let mut raw = fs::read(&idx).unwrap();
    raw[FILE_HEADER_LEN + 4..FILE_HEADER_LEN + 8].copy_from_slice(&big.to_le_bytes());
    fs::write(&idx, &raw).unwrap();
    set_file_hwm(
        &BlockFilterTable::seg_dirs(&dir).body_path(0),
        u64::from(u32::MAX) + 1,
    );

    let t = BlockFilterTable::open_or_create(&dir).unwrap();
    assert_eq!(t.next_height(), Height(1));
    put_range(&t, 1, 2);
    assert!(BlockFilterTable::seg_dirs(&dir).body_path(1).is_file());
    assert_eq!(
        t.filters(Height(1), Height(2)).unwrap().unwrap(),
        vec![(vec![1u8; 4], slot(1)), (vec![2u8; 5], slot(2))]
    );
    assert_eq!(
        t.slots(Height(0), Height(2)).unwrap().unwrap(),
        (0..=2).map(slot).collect::<Vec<_>>(),
        "range read crosses the segment roll"
    );
    t.truncate_through(Some(Height(0))).unwrap();
    assert!(!BlockFilterTable::seg_dirs(&dir).body_path(1).exists());
    assert_eq!(t.next_height(), Height(1));
}

#[test]
fn block_filter_slot_roundtrip_keeps_the_row() {
    let row = decode_slot(&encode_slot(9, 40, &slot(3)));
    assert_eq!(row.off, 9);
    assert_eq!(row.len, 40);
    assert_eq!(row.slot, slot(3));
}
