use rbitcoin_primitives::TableKind;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::{TableFile, FILE_HEADER_LEN, TRAILING_FOOTER_LEN};
use rbitcoin_store::testutil::TempDir;

/// The leading header is 16 bytes. Reopen reads the logical length from it.
/// A file shorter than that header still fails before any field is read.
#[test]
fn leading_logical_survives_open() {
    let dir = TempDir::labeled("leading-logical").unwrap();
    let path = dir.path().join("txout");
    let logical = FILE_HEADER_LEN as u64 + 24;
    {
        let f = TableFile::create(&path, TableKind::TxOut).unwrap();
        f.set_logical_len(logical).unwrap();
        assert_eq!(f.logical_len(), logical);
    }
    let opened = TableFile::open(&path, TableKind::TxOut).unwrap();
    assert_eq!(opened.logical_len(), logical);

    let short = dir.path().join("short");
    std::fs::write(&short, [0u8; 8]).unwrap();
    assert!(matches!(
        TableFile::open(&short, TableKind::TxOut),
        Err(StoreError::Io { .. })
    ));
}

/// The trailer is 32 bytes at the published end. The data length is the
/// logical field minus that trailer. A short file stays corrupt.
#[test]
fn trailing_footer_names_the_data_len() {
    let dir = TempDir::labeled("trailing-logical").unwrap();
    let path = dir.path().join("txout");
    let data = 1000u64;
    let logical = data + TRAILING_FOOTER_LEN as u64;
    {
        let f = TableFile::create_trailing_header(&path, TableKind::TxOut).unwrap();
        f.ensure_capacity(logical).unwrap();
        f.set_logical_len(logical).unwrap();
    }
    let (_path, got) = TableFile::trailing_footer_data_bytes(&path, TableKind::TxOut).unwrap();
    assert_eq!(got, data);
    let (opened, _) = TableFile::open_trailing_header_from_end(&path, TableKind::TxOut).unwrap();
    assert_eq!(opened.logical_len(), logical);

    let short = dir.path().join("short");
    std::fs::write(&short, b"tiny").unwrap();
    assert!(matches!(
        TableFile::trailing_footer_data_bytes(&short, TableKind::TxOut),
        Err(StoreError::Corrupt("trailing-header table short"))
    ));
}
