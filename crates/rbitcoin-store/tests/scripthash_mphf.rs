use rbitcoin_store::bdz::BdzMphf;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::scripthash_layout::{pack8, ShHeadKey, ShHeadValue, SH_HEAD_KEY_LEN};
use rbitcoin_store::scripthash_mphf::*;
use std::fs::OpenOptions;
use std::path::PathBuf;

use rbitcoin_primitives::Fk;

fn tmp() -> PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("sh-mphf");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn key(tag: u8) -> ShHeadKey {
    let mut k = [0u8; SH_HEAD_KEY_LEN];
    k[0] = tag;
    k[1] = tag.wrapping_add(1);
    k
}

#[test]
fn pread_file_exact_reports_the_pread_errno() {
    let dir = tmp();
    // A directory fd reports EISDIR. Windows `File::open` of a directory
    // fails before pread, so this half stays on unix.
    #[cfg(unix)]
    {
        let file = std::fs::File::open(&dir).unwrap();
        let mut buf = [0u8; 4];
        let err = pread_file_exact(&file, 0, &mut buf).unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EISDIR),
            "positional read must surface the negated pread errno, got {err:?}"
        );
    }
    let rw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("empty"))
        .unwrap();
    let mut short = [0u8; 4];
    let short_err = pread_file_exact(&rw, 0, &mut short).unwrap_err();
    assert_eq!(short_err.kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(short_err.to_string().contains("pread short"), "{short_err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_two_keys_get_and_miss() {
    let dir = tmp();
    let base = dir.join("00");
    let a = ShHeadValue::inline_one(Fk(11));
    let b = ShHeadValue::inline_one(Fk(22));
    let recs = [(key(1), pack8(&a).unwrap()), (key(2), pack8(&b).unwrap())];
    let h = MphfHead::write_pack8(&base, &recs).unwrap();
    let raw = std::fs::read(mphf_path(&base)).unwrap();
    assert_eq!(&raw[0..4], b"BDZ3");
    let compact = BdzMphf::read_compact_from(&mphf_path(&base)).unwrap();
    assert_eq!(
        raw.len() as u64,
        compact.trailer_off() + (recs.len() as u64) * 8
    );
    assert_eq!(h.get(&key(1)).unwrap().unwrap(), a);
    assert_eq!(h.get(&key(2)).unwrap().unwrap(), b);
    assert!(h.get(&key(9)).unwrap().is_none());
    h.flush().unwrap();
    assert!(MphfHead::exists(&base));
    assert!(mphf_path(&base).is_file());
    assert!(val_path(&base).is_file());
    assert_eq!(std::fs::metadata(val_path(&base)).unwrap().len(), 16);
    let h2 = MphfHead::open(&base).unwrap();
    assert_eq!(h2.g_bytes_resident(), 0);
    assert!(
        h2.occ_bytes_resident() > 0 && h2.occ_bytes_resident() < 64,
        "open must keep supers, not copy occ"
    );
    assert_eq!(h2.get(&key(1)).unwrap().unwrap(), a);
    assert!(h2.get(&key(7)).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_update_inline_to_slab() {
    let dir = tmp();
    let base = dir.join("00");
    let one = ShHeadValue::inline_one(Fk(3));
    let h = MphfHead::write_pack8(&base, &[(key(4), pack8(&one).unwrap())]).unwrap();
    let slab = ShHeadValue::slab(0, 2, 4096);
    assert!(h.update_value(&key(4), &slab).unwrap());
    assert!(!h.update_value(&key(5), &slab).unwrap());
    match h.get(&key(4)).unwrap().unwrap() {
        ShHeadValue::Slab {
            class: 0,
            used: 2,
            off: 4096,
        } => {}
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_empty_and_extent_last_only() {
    let dir = tmp();
    let base = dir.join("00");
    let h = MphfHead::write_pack8(&base, &[]).unwrap();
    assert!(h.is_empty());
    assert!(h.get(&key(1)).unwrap().is_none());
    let extent = ShHeadValue::extent(8192);
    let h = MphfHead::write_pack8(&base, &[(key(1), pack8(&extent).unwrap())]).unwrap();
    match h.get(&key(1)).unwrap().unwrap() {
        ShHeadValue::Extent { last_page: 8192 } => {}
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_write_keys_zero_val_then_rewrite_slots() {
    let dir = tmp();
    let base = dir.join("00");
    let k1 = key(1);
    let k2 = key(2);
    let h = MphfHead::write_pack8(&base, &[(k1, 0), (k2, 0)]).unwrap();
    assert!(h.get(&k1).unwrap().unwrap().is_empty());
    let s1 = h.slot_for_key16(&k1).unwrap();
    let s2 = h.slot_for_key16(&k2).unwrap();
    let a = pack8(&ShHeadValue::inline_one(Fk(11))).unwrap();
    let b = pack8(&ShHeadValue::inline_one(Fk(22))).unwrap();
    h.rewrite_val_slots(&[(s1, a), (s2, b)]).unwrap();
    assert_eq!(
        h.get(&k1).unwrap().unwrap(),
        ShHeadValue::inline_one(Fk(11))
    );
    assert_eq!(
        h.get(&k2).unwrap().unwrap(),
        ShHeadValue::inline_one(Fk(22))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_rewrite_val_edges_keep_middle_and_length() {
    let dir = tmp();
    let base = dir.join("00");
    let keys: Vec<ShHeadKey> = (1..=4).map(key).collect();
    let recs: Vec<(ShHeadKey, u64)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            (
                *k,
                pack8(&ShHeadValue::inline_one(Fk(100 + i as u64))).unwrap(),
            )
        })
        .collect();
    let h = MphfHead::write_pack8(&base, &recs).unwrap();
    let n = h.mphf.n();
    assert_eq!(n, 4);
    assert_eq!(std::fs::metadata(val_path(&base)).unwrap().len(), 32);
    let mut by_slot = vec![None; n as usize];
    for (i, k) in keys.iter().enumerate() {
        let slot = h.slot_for_key16(k).unwrap() as usize;
        by_slot[slot] = Some((i, *k));
    }
    let (_i0, k0) = by_slot[0].unwrap();
    let (_il, kl) = by_slot[n as usize - 1].unwrap();
    let mid_slot = (1..n as usize - 1).find(|s| by_slot[*s].is_some()).unwrap();
    let (im, km) = by_slot[mid_slot].unwrap();
    let w0 = pack8(&ShHeadValue::inline_one(Fk(7))).unwrap();
    let wl = pack8(&ShHeadValue::inline_one(Fk(9))).unwrap();
    h.rewrite_val_slots(&[(0, w0), (n - 1, wl)]).unwrap();
    assert_eq!(h.get(&k0).unwrap().unwrap(), ShHeadValue::inline_one(Fk(7)));
    assert_eq!(h.get(&kl).unwrap().unwrap(), ShHeadValue::inline_one(Fk(9)));
    assert_eq!(
        h.get(&km).unwrap().unwrap(),
        ShHeadValue::inline_one(Fk(100 + im as u64))
    );
    assert_eq!(std::fs::metadata(val_path(&base)).unwrap().len(), 32);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_rewrite_val_oob_does_not_tear() {
    let dir = tmp();
    let base = dir.join("00");
    let k1 = key(1);
    let k2 = key(2);
    let a = ShHeadValue::inline_one(Fk(11));
    let b = ShHeadValue::inline_one(Fk(22));
    let h = MphfHead::write_pack8(&base, &[(k1, pack8(&a).unwrap()), (k2, pack8(&b).unwrap())])
        .unwrap();
    let s1 = h.slot_for_key16(&k1).unwrap();
    let before = std::fs::read(val_path(&base)).unwrap();
    let err = h
        .rewrite_val_slots(&[
            (s1, pack8(&ShHeadValue::inline_one(Fk(99))).unwrap()),
            (99, 1),
        ])
        .unwrap_err();
    assert!(matches!(err, StoreError::Corrupt(m) if m.contains("slot OOB")));
    assert_eq!(std::fs::read(val_path(&base)).unwrap(), before);
    assert_eq!(h.get(&k1).unwrap().unwrap(), a);
    assert_eq!(h.get(&k2).unwrap().unwrap(), b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn val_window_spans_split_on_gap() {
    let words = [(0u32, 1u64), (1, 2), (10, 3), (10, 4), (11, 5)];
    assert_eq!(val_window_spans(&words, 24), vec![(0, 1), (10, 11)]);
    assert_eq!(
        val_window_spans(&words, 8),
        vec![(0, 0), (1, 1), (10, 10), (11, 11)]
    );
}

#[test]
fn rewrite_val_windows_last_duplicate_wins() {
    let dir = tmp();
    let path = dir.join("val");
    let mut raw = vec![0u8; 16 * 8];
    for slot in 0..16u32 {
        let off = slot as usize * 8;
        raw[off..off + 8].copy_from_slice(&(u64::from(slot) + 1).to_le_bytes());
    }
    std::fs::write(&path, &raw).unwrap();
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    rewrite_val_windows(&f, &[(0, 9), (15, 7), (0, 4), (2, 8)], 16).unwrap();
    let got = std::fs::read(&path).unwrap();
    assert_eq!(u64::from_le_bytes(got[0..8].try_into().unwrap()), 4);
    assert_eq!(u64::from_le_bytes(got[8..16].try_into().unwrap()), 2);
    assert_eq!(u64::from_le_bytes(got[16..24].try_into().unwrap()), 8);
    assert_eq!(u64::from_le_bytes(got[24..32].try_into().unwrap()), 4);
    assert_eq!(
        u64::from_le_bytes(got[15 * 8..16 * 8].try_into().unwrap()),
        7
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sh_mphf_duplicate_key16_does_not_clone_sort_mix64() {
    let dir = tmp();
    let base = dir.join("00");
    let a = ShHeadValue::inline_one(Fk(11));
    let recs = [(key(1), pack8(&a).unwrap()), (key(1), pack8(&a).unwrap())];
    match MphfHead::write_pack8(&base, &recs) {
        Err(StoreError::Corrupt(m)) if m.contains("mix64 collision") => {
            panic!("mix64 clone-sort is not the collision path: {m}")
        }
        Err(StoreError::Corrupt(_)) | Ok(_) => {}
        Err(e) => panic!("unexpected {e}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
