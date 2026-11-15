//! Sealed SH shard: compact BDZ MPHF + dense 8 B pack8 locators.
//!
//! `base.mphf` is BDZ3 (2-bit `g` + occupancy) then `n` mix64(key16) tags
//! (not loaded into RAM). `base.val` is `n × 8` pack8. A key not in the set
//! fails the tag check.

use rbitcoin_primitives::le_u64;

use crate::bdz::BdzMphf;
use crate::error::StoreError;
use crate::scripthash_layout::{pack8, unpack8, ShHeadKey, ShHeadValue, SH_HEAD_KEY_LEN};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct MphfHead {
    pub base: PathBuf,
    pub mphf_file: File,
    pub val_file: File,
    pub mphf: BdzMphf,
    pub tags_off: u64,
}

pub fn mphf_path(base: &Path) -> PathBuf {
    sidecar(base, ".mphf")
}

pub fn val_path(base: &Path) -> PathBuf {
    sidecar(base, ".val")
}

#[doc(hidden)]
pub fn sidecar(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(ext);
    PathBuf::from(s)
}

pub fn mix_key16(key: &ShHeadKey) -> u64 {
    let lo = le_u64!(key, 0);
    let hi = le_u64!(key, 8);
    lo ^ hi
}

pub fn mix64_keys(recs: &[(ShHeadKey, u64)]) -> Vec<u64> {
    recs.iter().map(|(k, _)| mix_key16(k)).collect()
}

impl MphfHead {
    pub fn exists(base: &Path) -> bool {
        mphf_path(base).is_file() && val_path(base).is_file()
    }

    pub fn is_empty(&self) -> bool {
        self.mphf.n() == 0
    }

    pub fn g_bytes_resident(&self) -> usize {
        self.mphf.g_bytes_resident()
    }

    pub fn occ_bytes_resident(&self) -> usize {
        self.mphf.occ_bytes_resident()
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.val_file
            .sync_data()
            .map_err(|e| StoreError::io(val_path(&self.base), e))
    }

    pub fn write_pack8(
        base: impl AsRef<Path>,
        recs: &[(ShHeadKey, u64)],
    ) -> Result<Self, StoreError> {
        let keys = mix64_keys(recs);
        Self::write_pack8_mixed(base, recs, &keys)
    }

    pub fn write_pack8_mixed(
        base: impl AsRef<Path>,
        recs: &[(ShHeadKey, u64)],
        keys: &[u64],
    ) -> Result<Self, StoreError> {
        let base = base.as_ref();
        if let Some(parent) = base.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mphf = BdzMphf::build_compact(keys)?;
        let n = recs.len();
        let mut val = vec![0u8; n.saturating_mul(8)];
        let mut tags = vec![0u8; n.saturating_mul(8)];
        for (i, (_k, w)) in recs.iter().enumerate() {
            let ku = keys[i];
            let slot = mphf.index(ku)? as usize;
            tags[slot * 8..slot * 8 + 8].copy_from_slice(&ku.to_le_bytes());
            val[slot * 8..slot * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        let mp = mphf_path(base);
        let staging = crate::file::tmp_sidecar_path(&mp);
        mphf.write_compact_to(&staging)?;
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&staging)
                .map_err(|e| StoreError::io(&staging, e))?;
            f.write_all(&tags)
                .map_err(|e| StoreError::io(&staging, e))?;
            f.sync_all().map_err(|e| StoreError::io(&staging, e))?;
        }
        std::fs::rename(&staging, &mp).map_err(|e| StoreError::io(&mp, e))?;
        let vp = val_path(base);
        crate::file::write_synced_tmp_rename(&vp, &val)?;
        Self::open(base)
    }

    /// MPHF slot for a key known to be in the set (no tag pread).
    pub fn slot_for_key16(&self, key: &ShHeadKey) -> Result<u32, StoreError> {
        if self.mphf.n() == 0 {
            return Err(StoreError::Corrupt("sh mphf: empty shard has key"));
        }
        self.mphf.index(mix_key16(key))
    }

    /// Overwrite pack8 words by MPHF slot (pass-2 pack; does not rebuild BDZ).
    ///
    /// Every slot is checked before any write. `.val` up to 512 MiB is patched
    /// as one image; larger files use 1 MiB windows.
    pub fn rewrite_val_slots(&self, words: &[(u32, u64)]) -> Result<(), StoreError> {
        let n = self.mphf.n();
        for &(slot, _) in words {
            if slot >= n {
                return Err(StoreError::Corrupt("sh mphf: slot OOB"));
            }
        }
        let len = u64::from(n).saturating_mul(8);
        let path = val_path(&self.base);
        if words.is_empty() || len == 0 {
            self.val_file
                .sync_data()
                .map_err(|e| StoreError::io(&path, e))?;
            return Ok(());
        }
        if len <= VAL_REWRITE_FULL_MAX {
            let mut buf = vec![0u8; len as usize];
            pread_file_exact(&self.val_file, 0, &mut buf).map_err(|e| StoreError::io(&path, e))?;
            patch_val_image(&mut buf, 0, words);
            pwrite_file(&self.val_file, 0, &buf).map_err(|e| StoreError::io(&path, e))?;
        } else {
            rewrite_val_windows(&self.val_file, words, VAL_REWRITE_WINDOW)
                .map_err(|e| StoreError::io(&path, e))?;
        }
        self.val_file
            .sync_data()
            .map_err(|e| StoreError::io(&path, e))?;
        Ok(())
    }

    pub fn open(base: impl AsRef<Path>) -> Result<Self, StoreError> {
        let base = base.as_ref().to_path_buf();
        let mp = mphf_path(&base);
        let vp = val_path(&base);
        let mphf = BdzMphf::read_compact_from(&mp)?;
        let tags_off = mphf.trailer_off();
        let mphf_file = File::open(&mp).map_err(|e| StoreError::io(&mp, e))?;
        let val_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&vp)
            .map_err(|e| StoreError::io(&vp, e))?;
        let n = mphf.n() as u64;
        let meta = val_file.metadata().map_err(|e| StoreError::io(&vp, e))?;
        if meta.len() != n.saturating_mul(8) {
            return Err(StoreError::Corrupt("sh mphf: val length"));
        }
        let mphf_len = mphf_file
            .metadata()
            .map_err(|e| StoreError::io(&mp, e))?
            .len();
        if mphf_len != tags_off + n.saturating_mul(8) {
            return Err(StoreError::Corrupt("sh mphf: tag length"));
        }
        Ok(Self {
            base,
            mphf_file,
            val_file,
            mphf,
            tags_off,
        })
    }

    pub fn get(&self, key: &ShHeadKey) -> Result<Option<ShHeadValue>, StoreError> {
        let Some(slot) = self.slot_if_present(key)? else {
            return Ok(None);
        };
        Ok(Some(self.read_val(slot)?))
    }

    pub fn update_value(&self, key: &ShHeadKey, value: &ShHeadValue) -> Result<bool, StoreError> {
        let Some(slot) = self.slot_if_present(key)? else {
            return Ok(false);
        };
        let w = pack8(value)?;
        let off = slot.saturating_mul(8);
        pwrite_file(&self.val_file, off, &w.to_le_bytes())
            .map_err(|e| StoreError::io(val_path(&self.base), e))?;
        Ok(true)
    }

    /// Byte offset of the pack8 word in `.val`, when the key is present.
    pub fn value_word_offset(&self, key: &ShHeadKey) -> Result<Option<u64>, StoreError> {
        Ok(self
            .slot_if_present(key)?
            .map(|slot| slot.saturating_mul(8)))
    }

    pub fn read_value_word(&self, off: u64) -> Result<[u8; 8], StoreError> {
        let mut buf = [0u8; 8];
        pread_file_exact(&self.val_file, off, &mut buf)
            .map_err(|e| StoreError::io(val_path(&self.base), e))?;
        Ok(buf)
    }

    pub fn write_value_word(&self, off: u64, bytes: &[u8; 8]) -> Result<(), StoreError> {
        pwrite_file(&self.val_file, off, bytes).map_err(|e| StoreError::io(val_path(&self.base), e))
    }

    pub fn for_each_occupied(
        &self,
        mut f: impl FnMut(ShHeadKey, ShHeadValue) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let n = self.mphf.n() as u64;
        let dummy = [0u8; SH_HEAD_KEY_LEN];
        for slot in 0..n {
            let v = self.read_val(slot)?;
            if !v.is_empty() {
                f(dummy, v)?;
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn slot_if_present(&self, key: &ShHeadKey) -> Result<Option<u64>, StoreError> {
        if self.mphf.n() == 0 {
            return Ok(None);
        }
        let ku = mix_key16(key);
        let slot = u64::from(self.mphf.index(ku)?);
        let mut tag = [0u8; 8];
        pread_file_exact(&self.mphf_file, self.tags_off + slot * 8, &mut tag)
            .map_err(|e| StoreError::io(mphf_path(&self.base), e))?;
        if u64::from_le_bytes(tag) != ku {
            return Ok(None);
        }
        Ok(Some(slot))
    }

    #[doc(hidden)]
    pub fn read_val(&self, slot: u64) -> Result<ShHeadValue, StoreError> {
        let mut buf = [0u8; 8];
        pread_file_exact(&self.val_file, slot * 8, &mut buf)
            .map_err(|e| StoreError::io(val_path(&self.base), e))?;
        unpack8(u64::from_le_bytes(buf))
    }
}

/// Patch a whole `.val` in RAM up to this size. Above it, windowed RMW.
#[doc(hidden)]
pub const VAL_REWRITE_FULL_MAX: u64 = 512 << 20;
/// Byte cap of one read-modify-write span on a large `.val`.
#[doc(hidden)]
pub const VAL_REWRITE_WINDOW: u64 = 1 << 20;

#[doc(hidden)]
pub fn patch_val_image(buf: &mut [u8], base_slot: u32, words: &[(u32, u64)]) {
    for &(slot, w) in words {
        let rel = (slot - base_slot) as usize * 8;
        buf[rel..rel + 8].copy_from_slice(&w.to_le_bytes());
    }
}

/// Inclusive slot spans whose byte width is at most `window_bytes`.
/// `words` must be sorted by slot. Later duplicates of a slot stay in the
/// same span so the caller applies them in order.
#[doc(hidden)]
pub fn val_window_spans(words: &[(u32, u64)], window_bytes: u64) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let start = words[i].0;
        let mut end = start;
        let mut j = i + 1;
        while j < words.len() {
            let slot = words[j].0;
            let span = (u64::from(slot) - u64::from(start) + 1).saturating_mul(8);
            if span > window_bytes.max(8) {
                break;
            }
            end = slot;
            j += 1;
        }
        out.push((start, end));
        i = j;
    }
    out
}

#[doc(hidden)]
pub fn rewrite_val_windows(
    file: &File,
    words: &[(u32, u64)],
    window_bytes: u64,
) -> std::io::Result<()> {
    let mut order: Vec<usize> = (0..words.len()).collect();
    order.sort_by_key(|&i| (words[i].0, i));
    let sorted: Vec<(u32, u64)> = order.iter().map(|&i| words[i]).collect();
    let mut at = 0usize;
    for (start, end) in val_window_spans(&sorted, window_bytes) {
        let mut next = at;
        while next < sorted.len() && sorted[next].0 <= end {
            next += 1;
        }
        let span = ((end - start) as usize + 1) * 8;
        let off = u64::from(start) * 8;
        let mut buf = vec![0u8; span];
        pread_file_exact(file, off, &mut buf)?;
        patch_val_image(&mut buf, start, &sorted[at..next]);
        pwrite_file(file, off, &buf)?;
        at = next;
    }
    Ok(())
}

#[doc(hidden)]
pub fn pread_file_exact(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    crate::file::handle_pread_all(file, std::path::Path::new("scripthash"), offset, buf)
        .map_err(StoreError::into_io)
}

#[doc(hidden)]
pub fn pwrite_file(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<()> {
    crate::file::handle_pwrite_all(file, std::path::Path::new("scripthash"), offset, buf)
        .map_err(StoreError::into_io)
}
