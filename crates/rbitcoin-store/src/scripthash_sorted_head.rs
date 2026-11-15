//! Sealed sorted Class B head: packed `(key16 ‖ pack8)` + `.idx`.
//!
//! Record count is immutable after seal. Existing keys update pack8 in
//! place. A key that is not on this file is **not inserted** (caller uses ovf).
//!
//! **Main shards** are idx-only (misses pay one 4 KiB data pread). **Sealed
//! ovf** still writes BF8R so a miss walk can skip the page.

use crate::error::StoreError;
use crate::fuse8_filter::{fuse_key_from_mixed, SealedFuse8};
use crate::io_handle::IoHandle;
use crate::scripthash_layout::{
    pack8_bytes, unpack8_bytes, ShHeadKey, ShHeadValue, SH_HEAD_KEY_LEN, SH_HEAD_SLOT_SIZE,
    SH_HEAD_VALUE_LEN,
};
use rbitcoin_primitives::{bytes, le_u16, le_u32, le_u64};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Records per data page (170 × 24 B = 4080 B).
pub const SH_SORTED_RECS_PER_PAGE: usize = 170;
#[doc(hidden)]
pub const DATA_MAGIC: &[u8; 4] = b"SHSR";
#[doc(hidden)]
pub const IDX_MAGIC: &[u8; 4] = b"SHIX";
#[doc(hidden)]
pub const FORMAT_VER: u16 = 2;
#[doc(hidden)]
pub const DATA_HEADER_LEN: u64 = 32;
#[doc(hidden)]
pub const IDX_HEADER_LEN: usize = 16;
#[doc(hidden)]
pub const IDX_ENT_LEN: usize = SH_HEAD_KEY_LEN + 8;

/// Sealed sorted head file (one shard or one global ovf segment).
pub struct SortedHead {
    pub path: PathBuf,
    pub file: File,
    pub count: u64,
    /// First key of each 4 KiB data page + file offset of that page.
    pub idx: Vec<(ShHeadKey, u64)>,
    pub fuse: Option<SealedFuse8>,
    pub preads: AtomicU64,
}

impl SortedHead {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Durability barrier (shutdown / SH flush). Data already pwrite'd.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.file
            .sync_data()
            .map_err(|e| StoreError::io(&self.path, e))
    }

    /// Write a sealed sorted head. `recs` must be unique and sorted by key16.
    pub fn write(
        path: impl AsRef<Path>,
        recs: &[(ShHeadKey, [u8; SH_HEAD_VALUE_LEN])],
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        for w in recs.windows(2) {
            if w[1].0 <= w[0].0 {
                return Err(StoreError::Corrupt(
                    "scripthash sorted head: recs not strictly increasing",
                ));
            }
        }
        let count = recs.len() as u64;
        let mut header = [0u8; DATA_HEADER_LEN as usize];
        header[0..4].copy_from_slice(DATA_MAGIC);
        header[4..6].copy_from_slice(&FORMAT_VER.to_le_bytes());
        header[6..14].copy_from_slice(&count.to_le_bytes());
        header[14..16].copy_from_slice(&(SH_SORTED_RECS_PER_PAGE as u16).to_le_bytes());

        {
            let mut f = File::create(path).map_err(|e| StoreError::io(path, e))?;
            f.write_all(&header).map_err(|e| StoreError::io(path, e))?;
            for (k, v) in recs {
                f.write_all(k).map_err(|e| StoreError::io(path, e))?;
                f.write_all(v).map_err(|e| StoreError::io(path, e))?;
            }
            f.sync_all().map_err(|e| StoreError::io(path, e))?;
        }

        let mut idx: Vec<(ShHeadKey, u64)> = Vec::new();
        let mut i = 0usize;
        while i < recs.len() {
            let off = DATA_HEADER_LEN + (i as u64) * SH_HEAD_SLOT_SIZE as u64;
            idx.push((recs[i].0, off));
            i += SH_SORTED_RECS_PER_PAGE;
        }
        write_idx(&idx_path(path), &idx)?;

        let mut fuse_keys: Vec<u64> = recs.iter().map(|(k, _)| fuse_key16(k)).collect();
        fuse_keys.sort_unstable();
        fuse_keys.dedup();
        let fuse = SealedFuse8::build(&fuse_keys)?;
        fuse.write_to(&fuse_path(path))?;

        Self::open(path)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| StoreError::io(&path, e))?;
        let mut header = [0u8; DATA_HEADER_LEN as usize];
        let _ = pread_file(&file, 0, &mut header).map_err(|e| StoreError::io(&path, e))?;
        if header[0..4] != *DATA_MAGIC {
            return Err(StoreError::BadMagic);
        }
        let ver = le_u16!(header, 4);
        if ver != FORMAT_VER {
            return Err(StoreError::Corrupt(
                "scripthash sorted head: unsupported version",
            ));
        }
        let count = le_u64!(header, 6);
        let idx = read_idx(&idx_path(&path))?;
        let fuse = Some(SealedFuse8::read_from(&fuse_path(&path))?);
        Ok(Self {
            path,
            file,
            count,
            idx,
            fuse,
            preads: AtomicU64::new(0),
        })
    }

    pub fn get(&self, key: &ShHeadKey) -> Result<Option<ShHeadValue>, StoreError> {
        if let Some(ref fuse) = self.fuse {
            if !fuse.contains(fuse_key16(key)) {
                return Ok(None);
            }
        }
        let Some((_slot, rec)) = self.locate_rec(key)? else {
            return Ok(None);
        };
        let mut val = [0u8; SH_HEAD_VALUE_LEN];
        val.copy_from_slice(&rec[SH_HEAD_KEY_LEN..]);
        Ok(Some(unpack8_bytes(&val)?))
    }

    /// Byte offset of the pack8 word, when the key is on this file.
    pub fn value_word_offset(&self, key: &ShHeadKey) -> Result<Option<u64>, StoreError> {
        let Some((slot, _)) = self.locate_rec(key)? else {
            return Ok(None);
        };
        Ok(Some(
            DATA_HEADER_LEN + slot * SH_HEAD_SLOT_SIZE as u64 + SH_HEAD_KEY_LEN as u64,
        ))
    }

    pub fn read_value_word(&self, off: u64) -> Result<[u8; 8], StoreError> {
        let mut buf = [0u8; 8];
        pread_file_exact(&self.file, off, &mut buf).map_err(|e| StoreError::io(&self.path, e))?;
        Ok(buf)
    }

    pub fn write_value_word(&self, off: u64, bytes: &[u8; 8]) -> Result<(), StoreError> {
        pwrite_file(&self.file, off, bytes).map_err(|e| StoreError::io(&self.path, e))
    }

    /// In-place `value16` update. `Ok(false)` if the key is not on this file.
    pub fn update_value(&self, key: &ShHeadKey, value: &ShHeadValue) -> Result<bool, StoreError> {
        if let Some(ref fuse) = self.fuse {
            if !fuse.contains(fuse_key16(key)) {
                return Ok(false);
            }
        }
        let Some(slot) = self.locate_rec(key)?.map(|(s, _)| s) else {
            return Ok(false);
        };
        let enc = pack8_bytes(value)?;
        let off = DATA_HEADER_LEN + slot * SH_HEAD_SLOT_SIZE as u64 + SH_HEAD_KEY_LEN as u64;
        pwrite_file(&self.file, off, &enc).map_err(|e| StoreError::io(&self.path, e))?;
        Ok(true)
    }

    /// Visit every record (cold walks / seal-merge). Sequential reads, not the get path.
    pub fn for_each_occupied(
        &self,
        mut f: impl FnMut(ShHeadKey, ShHeadValue) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut rec = [0u8; SH_HEAD_SLOT_SIZE];
        for slot in 0..self.count {
            let off = DATA_HEADER_LEN + slot * SH_HEAD_SLOT_SIZE as u64;
            pread_file_exact(&self.file, off, &mut rec)
                .map_err(|e| StoreError::io(&self.path, e))?;
            let k = bytes!(rec, 0, 16);
            let val = unpack8_bytes(&bytes!(rec, 16, 8))?;
            if !val.is_empty() {
                f(k, val)?;
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn locate_rec(
        &self,
        key: &ShHeadKey,
    ) -> Result<Option<(u64, [u8; SH_HEAD_SLOT_SIZE])>, StoreError> {
        if self.count == 0 || self.idx.is_empty() {
            return Ok(None);
        }
        let i = match self.idx.binary_search_by(|probe| probe.0.cmp(key)) {
            Ok(exact) => exact,
            Err(0) => return Ok(None),
            Err(ins) => ins - 1,
        };
        let page_off = self.idx[i].1;
        let page_slot0 = (page_off - DATA_HEADER_LEN) / SH_HEAD_SLOT_SIZE as u64;
        let remain = self.count.saturating_sub(page_slot0);
        let n = remain.min(SH_SORTED_RECS_PER_PAGE as u64) as usize;
        let mut page = vec![0u8; n * SH_HEAD_SLOT_SIZE];
        self.preads.fetch_add(1, Ordering::Relaxed);
        pread_file_exact(&self.file, page_off, &mut page)
            .map_err(|e| StoreError::io(&self.path, e))?;
        for (s, rec) in page.as_chunks::<SH_HEAD_SLOT_SIZE>().0.iter().enumerate() {
            let k = bytes!(rec, 0, 16);
            match k.cmp(key) {
                std::cmp::Ordering::Equal => {
                    return Ok(Some((page_slot0 + s as u64, *rec)));
                }
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => {}
            }
        }
        Ok(None)
    }
}

#[doc(hidden)]
pub fn pread_file(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = IoHandle::from_file(file).pread(offset, buf);
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
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

#[doc(hidden)]
pub fn fuse_key16(key: &ShHeadKey) -> u64 {
    let mut pad = [0u8; 32];
    pad[..SH_HEAD_KEY_LEN].copy_from_slice(key);
    fuse_key_from_mixed(&pad)
}

#[doc(hidden)]
pub fn idx_path(data: &Path) -> PathBuf {
    let mut s = data.as_os_str().to_os_string();
    s.push(".idx");
    PathBuf::from(s)
}

#[doc(hidden)]
pub fn fuse_path(data: &Path) -> PathBuf {
    let mut s = data.as_os_str().to_os_string();
    s.push(".fuse8");
    PathBuf::from(s)
}

#[doc(hidden)]
pub fn write_idx(path: &Path, idx: &[(ShHeadKey, u64)]) -> Result<(), StoreError> {
    let mut buf = vec![0u8; IDX_HEADER_LEN + idx.len() * IDX_ENT_LEN];
    buf[0..4].copy_from_slice(IDX_MAGIC);
    buf[4..6].copy_from_slice(&FORMAT_VER.to_le_bytes());
    buf[6..10].copy_from_slice(&(idx.len() as u32).to_le_bytes());
    let mut off = IDX_HEADER_LEN;
    for (k, rec_off) in idx {
        buf[off..off + SH_HEAD_KEY_LEN].copy_from_slice(k);
        off += SH_HEAD_KEY_LEN;
        buf[off..off + 8].copy_from_slice(&rec_off.to_le_bytes());
        off += 8;
    }
    crate::file::write_synced_tmp_rename(path, &buf)?;
    Ok(())
}

#[doc(hidden)]
pub fn read_idx(path: &Path) -> Result<Vec<(ShHeadKey, u64)>, StoreError> {
    let buf = std::fs::read(path).map_err(|e| StoreError::io(path, e))?;
    if buf.len() < IDX_HEADER_LEN || buf[0..4] != *IDX_MAGIC {
        return Err(StoreError::Corrupt("scripthash sorted head: bad idx"));
    }
    let mut hdr = [0u8; IDX_HEADER_LEN];
    hdr.copy_from_slice(&buf[..IDX_HEADER_LEN]);
    let n = le_u32!(hdr, 6) as usize;
    if buf.len() < IDX_HEADER_LEN + n * IDX_ENT_LEN {
        return Err(StoreError::Corrupt("scripthash sorted head: short idx"));
    }
    let ents = &buf[IDX_HEADER_LEN..IDX_HEADER_LEN + n * IDX_ENT_LEN];
    let mut out = Vec::with_capacity(n);
    for row in ents.as_chunks::<IDX_ENT_LEN>().0 {
        out.push((bytes!(row, 0, 16), le_u64!(row, 16)));
    }
    Ok(out)
}
