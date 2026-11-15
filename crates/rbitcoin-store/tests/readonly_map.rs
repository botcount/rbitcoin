use rbitcoin_store::error::StoreError;
use rbitcoin_store::readonly_map::*;

fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("romap");
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn map_prefix_excludes_file_tail() {
    let dir = tmp();
    let path = dir.join("t.bin");
    let mut body = vec![0u8; 64];
    body[0..4].copy_from_slice(b"HEAD");
    body[32..36].copy_from_slice(b"OCC!");
    body[48..52].copy_from_slice(b"TAIL");
    std::fs::write(&path, &body).unwrap();
    let map = ReadonlyMap::map_prefix(&path, 40).unwrap();
    let b = map.as_file_bytes();
    assert_eq!(b.len(), 40);
    assert_eq!(&b[0..4], b"HEAD");
    assert_eq!(&b[32..36], b"OCC!");
    assert!(!b.windows(4).any(|w| w == b"TAIL"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn map_prefix_longer_than_file_is_corrupt() {
    let dir = tmp();
    let path = dir.join("short.bin");
    std::fs::write(&path, b"abcd").unwrap();
    match ReadonlyMap::map_prefix(&path, 16) {
        Err(StoreError::Corrupt(m)) => assert_eq!(m, "mapped file truncated"),
        other => panic!("expected truncated, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
