use rbitcoin_primitives::Fk;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::testutil::TempDir;
use rbitcoin_store::txstat::*;

fn tiny(fee: u64, base: u32, wit: u32) -> TxStatRow {
    TxStatRow {
        fee_sat: fee,
        base,
        wit_extra: wit,
    }
}

#[test]
fn txstat_blob_longer_than_the_file_is_corrupt() {
    let err = checked_blob_len(1 << 30, 32).expect_err("huge len");
    match err {
        StoreError::Corrupt(msg) => assert!(msg.contains("txstat"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(checked_blob_len(4, 32).unwrap(), 4);
}

#[test]
fn create_empty_then_extend_zeros() {
    let dir = TempDir::labeled("txstat-extend").unwrap();
    let t = TxStat::create(&dir).unwrap();
    assert_eq!(t.count(), 0);
    t.extend_or_truncate_to(2).unwrap();
    assert_eq!(t.count(), 2);
    assert_eq!(t.get_cell(Fk(1)).unwrap(), [0u8; 8]);
    assert_eq!(t.get_cell(Fk(2)).unwrap(), [0u8; 8]);
    t.truncate_to_count(1).unwrap();
    assert_eq!(t.count(), 1);
    assert!(t.get_cell(Fk(2)).is_err());
}

#[test]
fn uleb_typical_fits_in_cell() {
    let row = tiny(1000, 110, 112);
    let (cell, tail) = pack_cell(&row).unwrap();
    assert!(tail.is_none());
    assert_eq!(parse_cell(cell).unwrap(), CellParse::Complete(row));
    assert_eq!(row.size(), 222);
    assert_eq!(row.weight(), 4 * 110 + 112);
}

#[test]
fn former_n_in_byte_no_longer_forces_a_tail() {
    let row = tiny(50_000, 200, 200_000);
    let (cell, tail) = pack_cell(&row).unwrap();
    assert!(tail.is_none(), "three fields of this row fit in 8 B");
    assert_eq!(parse_cell(cell).unwrap(), CellParse::Complete(row));
}

#[test]
fn overflow_is_remaining_bytes_only() {
    let row = tiny(u64::from(u32::MAX), 4_000_000, 4_000_000);
    let stream = encode_stream(&row).unwrap();
    assert!(stream.len() > 8, "wide fee and sizes must miss 8 B");
    let (cell, tail) = pack_cell(&row).unwrap();
    let tail = tail.expect("tail");
    assert_eq!(tail.as_slice(), &stream[8..]);
    assert_eq!(parse_cell(cell).unwrap(), CellParse::NeedTail);
    assert_eq!(parse_with_tail(cell, &tail).unwrap(), row);
}

#[test]
fn header_blob_roundtrip() {
    let dir = TempDir::labeled("txstat-ovf").unwrap();
    let t = TxStat::create(&dir).unwrap();
    let small = tiny(1000, 110, 0);
    let fat = tiny(u64::from(u32::MAX), 4_000_000, 4_000_000);
    t.append_batch(0, &[small, fat]).unwrap();
    t.write_block_rows(Fk(1), 1, &[small, fat]).unwrap();
    let blob = t.header_blob(Fk(1)).unwrap();
    assert!(!blob.is_empty());
    let got = t.get_range(1, 2, Some(&blob)).unwrap();
    assert_eq!(got[0], Some(small));
    assert_eq!(got[1], Some(fat));
    assert_eq!(t.get_row_merged(Fk(2), 1, &blob).unwrap(), Some(fat));
}

#[test]
fn leftover_txfixed_body_unlinked_on_open() {
    let dir = TempDir::labeled("txstat-leftover").unwrap();
    std::fs::write(dir.join("txfixed.body"), b"junk").unwrap();
    let _t = TxStat::open(&dir).unwrap();
    assert!(!dir.join("txfixed.body").exists());
    assert!(dir.join("txstat.body").is_file());
}

#[test]
fn range_reads_the_cell_and_header_blob_reads_the_slot() {
    let dir = TempDir::labeled("txstat-slot").unwrap();
    let t = TxStat::create(&dir).unwrap();
    let small = tiny(1000, 110, 0);
    let fat = tiny(u64::from(u32::MAX), 4_000_000, 4_000_000);
    t.append_batch(0, &[small, fat]).unwrap();
    assert_eq!(t.get_range(1, 1, None).unwrap(), vec![Some(small)]);
    let err = match t.get_range(1, 4, None) {
        Ok(_) => panic!("range accepted a fk past the count"),
        Err(err) => err,
    };
    assert!(matches!(err, StoreError::NotFound), "{err:?}");

    assert!(t.header_blob(Fk(1)).unwrap().is_empty());
    t.write_block_rows(Fk(1), 1, &[small, fat]).unwrap();
    let blob = t.header_blob(Fk(1)).unwrap();
    assert!(!blob.is_empty());
    let got = t.get_range(1, 2, Some(&blob)).unwrap();
    assert_eq!(got[0], Some(small));
    assert_eq!(got[1], Some(fat));
    assert!(t.header_blob(Fk(2)).unwrap().is_empty());
}
