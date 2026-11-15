//! Thin BIP-352 tweak index (`sp_tweaks.idx/` + `sp_tweaks.body/`).
//!
//! Schema **17** side product. Missing dirs are not [`StoreError::Corrupt`].
//! Persist is **tweaks only** — no txids, outs, or parent scripts.
//!
//! **Tip / strong height only.** Slots are dense from `origin`. A reorg
//! truncates above the new tip and those heights are written again. The idx
//! does not store `header_fk`.
//!
//! **Original body:** `u8 len` then `len` bytes (`0` = none; `33` + compressed
//! `A_tweak`). Each body file stays addressable by `u32` off. When the next
//! record’s **start** would exceed `u32::MAX`, we roll a new `NNNNNN` pair.
//!
//! ```text
//! sp_tweaks.idx/meta      origin:u32 ‖ fmt:u32=3
//! sp_tweaks.idx/NNNNNN    slot[i] = off:u32     // start in that body file
//! sp_tweaks.body/NNNNNN   u8 len ‖ [u8; len]
//! ```
//!
//! Leftover **files** `sp_tweaks.idx` / `sp_tweaks.body` (pre-17 single-file)
//! are unlinked on store open; `--sptweaks` backfill regenerates.

use crate::error::StoreError;
use crate::file::{TableFile, FILE_HEADER_LEN};
use crate::seg_pair::{locate, SegDirs, SegPair as Seg};
use rbitcoin_primitives::{le_u32, Height, TableKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What [`SpTweaksTable::trim_last_record`] did to the last record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimLast {
    Clean,
    /// Cut body bytes past the record.
    Cut,
    /// Dropped the record's slot (short or bad length).
    Dropped,
}

/// Compressed BIP-352 server tweak (Electrum tweaks wire).
pub const TWEAK_LEN: u8 = 33;
#[doc(hidden)]
pub const SLOT: u64 = 4;
/// Schema 17: segmented tip-only idx, original 0/33 body.
#[doc(hidden)]
pub const IDX_FMT_SEG_TIP: u32 = 3;

#[doc(hidden)]
pub struct Inner {
    pub segs: Vec<Seg>,
}

/// Height-dense thin tweak table (tip / strong heights only).
pub struct SpTweaksTable {
    pub dirs: SegDirs,
    pub origin: u32,
    pub inner: Mutex<Inner>,
}

impl SpTweaksTable {
    pub fn idx_dir(dir: &Path) -> PathBuf {
        dir.join("sp_tweaks.idx")
    }

    pub fn body_dir(dir: &Path) -> PathBuf {
        dir.join("sp_tweaks.body")
    }

    #[doc(hidden)]
    pub fn meta_path(dir: &Path) -> PathBuf {
        Self::idx_dir(dir).join("meta")
    }

    #[doc(hidden)]
    pub fn seg_dirs(dir: &Path) -> SegDirs {
        SegDirs::new(Self::idx_dir(dir), Self::body_dir(dir), TableKind::SpTweaks)
    }

    pub fn files_present(dir: &Path) -> bool {
        Self::idx_dir(dir).is_dir()
            && Self::body_dir(dir).is_dir()
            && Self::meta_path(dir).is_file()
    }

    /// Pre-17 single files (not the schema-17 directories).
    pub fn legacy_files_present(dir: &Path) -> bool {
        Self::idx_dir(dir).is_file() || Self::body_dir(dir).is_file()
    }

    /// Unlink leftover single-file idx/body. Returns true if anything was removed.
    pub fn discard_legacy_files(dir: &Path) -> bool {
        let idx = Self::idx_dir(dir);
        let body = Self::body_dir(dir);
        let mut dropped = false;
        if idx.is_file() {
            let _ = std::fs::remove_file(&idx);
            dropped = true;
        }
        if body.is_file() {
            let _ = std::fs::remove_file(&body);
            dropped = true;
        }
        dropped
    }

    pub fn create(dir: impl AsRef<Path>, origin: Height) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(Self::idx_dir(&dir)).map_err(|e| StoreError::io(&dir, e))?;
        std::fs::create_dir_all(Self::body_dir(&dir)).map_err(|e| StoreError::io(&dir, e))?;
        Self::write_meta(&dir, origin.0)?;
        let dirs = Self::seg_dirs(&dir);
        let seg = dirs.create(0, 0)?;
        Ok(Self {
            dirs,
            origin: origin.0,
            inner: Mutex::new(Inner { segs: vec![seg] }),
        })
    }

    #[doc(hidden)]
    pub fn write_meta(dir: &Path, origin: u32) -> Result<(), StoreError> {
        let meta = TableFile::create(Self::meta_path(dir), TableKind::ArrayLink)?;
        let mut prefix = [0u8; 8];
        prefix[..4].copy_from_slice(&origin.to_le_bytes());
        prefix[4..8].copy_from_slice(&IDX_FMT_SEG_TIP.to_le_bytes());
        meta.write_at_pwrite(FILE_HEADER_LEN as u64, &prefix)
    }

    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        if Self::legacy_files_present(&dir) {
            return Err(StoreError::Corrupt(
                "sp_tweaks: leftover single-file idx/body (schema 17 uses dirs)",
            ));
        }
        if !Self::files_present(&dir) {
            return Err(StoreError::Corrupt("sp_tweaks.idx/ missing meta"));
        }
        let meta = TableFile::open(Self::meta_path(&dir), TableKind::ArrayLink)?;
        if meta.logical_len() < FILE_HEADER_LEN as u64 + 8 {
            return Err(StoreError::Corrupt("sp_tweaks.idx/meta short"));
        }
        let mut prefix = [0u8; 8];
        meta.read_at(FILE_HEADER_LEN as u64, &mut prefix)?;
        let origin = le_u32!(prefix, 0);
        let fmt = le_u32!(prefix, 4);
        if fmt != IDX_FMT_SEG_TIP {
            return Err(StoreError::Corrupt("sp_tweaks idx format"));
        }
        let dirs = Self::seg_dirs(&dir);
        let segs = dirs.open(SLOT)?;
        if segs.iter().any(|s| {
            !(s.idx.logical_len().saturating_sub(FILE_HEADER_LEN as u64)).is_multiple_of(SLOT)
        }) {
            return Err(StoreError::Corrupt("sp_tweaks.idx size"));
        }
        Ok(Self {
            dirs,
            origin,
            inner: Mutex::new(Inner { segs }),
        })
    }

    /// Open existing dirs, or create empty ones. Leftover single files are dropped.
    pub fn open_or_create(dir: impl AsRef<Path>, origin: Height) -> Result<Self, StoreError> {
        let dir = dir.as_ref();
        if Self::discard_legacy_files(dir) {
            rbitcoin_log::warn!(
                "sp_tweaks: dropped leftover single-file idx/body; recreating origin={}",
                origin.0
            );
        }
        let idx_d = Self::idx_dir(dir);
        let body_d = Self::body_dir(dir);
        match (Self::files_present(dir), idx_d.exists() || body_d.exists()) {
            (true, _) => {
                let t = Self::open(dir)?;
                if t.origin != origin.0 {
                    return Err(StoreError::Corrupt("sp_tweaks origin mismatch"));
                }
                Ok(t)
            }
            (false, false) => {
                rbitcoin_log::info!(
                    "sp_tweaks: creating empty index origin={} dir={}",
                    origin.0,
                    dir.display()
                );
                Self::create(dir, origin)
            }
            (false, true) => {
                rbitcoin_log::warn!("sp_tweaks: incomplete dirs; recreating origin={}", origin.0);
                let _ = std::fs::remove_dir_all(&idx_d);
                let _ = std::fs::remove_dir_all(&body_d);
                Self::create(dir, origin)
            }
        }
    }

    pub fn origin_height(&self) -> Height {
        Height(self.origin)
    }

    #[doc(hidden)]
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn slot_count(&self) -> u64 {
        self.lock().segs.iter().map(|s| s.n_slots).sum()
    }

    /// Next height this table will accept (`origin` when empty).
    pub fn next_height(&self) -> Height {
        Height(self.origin.saturating_add(self.slot_count() as u32))
    }

    pub fn body_logical_len(&self) -> u64 {
        self.lock()
            .segs
            .last()
            .map(|s| s.body.logical_len())
            .unwrap_or(FILE_HEADER_LEN as u64)
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        for s in self.lock().segs.iter() {
            s.body.flush()?;
            s.idx.flush()?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn read_off(seg: &Seg, local: u64) -> Result<u32, StoreError> {
        let mut buf = [0u8; SLOT as usize];
        seg.idx
            .read_at(FILE_HEADER_LEN as u64 + local * SLOT, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    #[doc(hidden)]
    pub fn read_offs_run(seg: &Seg, local: u64, n: u64) -> Result<(Vec<u32>, u64), StoreError> {
        if n == 0 {
            return Ok((Vec::new(), 0));
        }
        if local.saturating_add(n) > seg.n_slots {
            return Err(StoreError::Corrupt("sp_tweaks run past segment"));
        }
        let need_next = local + n < seg.n_slots;
        let n_offs = n + u64::from(need_next);
        let mut buf = vec![0u8; (n_offs as usize) * SLOT as usize];
        seg.idx
            .read_at(FILE_HEADER_LEN as u64 + local * SLOT, &mut buf)?;
        let (raw, _) = buf.as_chunks::<{ SLOT as usize }>();
        let n_rows = n as usize;
        let mut starts = Vec::with_capacity(n_rows);
        for row in &raw[..n_rows] {
            starts.push(le_u32!(row, 0));
        }
        let end = if need_next {
            u64::from(le_u32!(raw[n_rows], 0))
        } else {
            seg.body.logical_len()
        };
        Ok((starts, end))
    }

    #[doc(hidden)]
    pub fn locate(inner: &Inner, origin: u32, height: Height) -> Option<(usize, u64)> {
        let g = u64::from(height.0.checked_sub(origin)?);
        locate(&inner.segs, g)
    }

    /// Encode one tx: `0` or `33 ‖ tweak`.
    pub fn encode_records(records: &[Option<[u8; 33]>]) -> Vec<u8> {
        let mut out = Vec::with_capacity(records.len().saturating_mul(2));
        for r in records {
            match r {
                None => out.push(0),
                Some(t) => {
                    out.push(TWEAK_LEN);
                    out.extend_from_slice(t);
                }
            }
        }
        out
    }

    #[doc(hidden)]
    pub fn decode_records(bytes: &[u8], n_tx: u32) -> Result<Vec<Option<[u8; 33]>>, StoreError> {
        let mut recs = Vec::with_capacity(n_tx as usize);
        let mut i = 0usize;
        for _ in 0..n_tx {
            if i >= bytes.len() {
                return Err(StoreError::Corrupt("sp_tweaks short body"));
            }
            let len = bytes[i];
            i += 1;
            match len {
                0 => recs.push(None),
                TWEAK_LEN => {
                    if i + 33 > bytes.len() {
                        return Err(StoreError::Corrupt("sp_tweaks short tweak"));
                    }
                    let mut t = [0u8; 33];
                    t.copy_from_slice(&bytes[i..i + 33]);
                    recs.push(Some(t));
                    i += 33;
                }
                _ => return Err(StoreError::Corrupt("sp_tweaks bad len (want 0 or 33)")),
            }
        }
        if i != bytes.len() {
            return Err(StoreError::Corrupt("sp_tweaks n_tx/body mismatch"));
        }
        Ok(recs)
    }

    #[doc(hidden)]
    pub fn decode_eligible(bytes: &[u8], n_tx: u32) -> Result<Vec<(u32, [u8; 33])>, StoreError> {
        let mut elig = Vec::new();
        let mut i = 0usize;
        for tx_i in 0..n_tx {
            if i >= bytes.len() {
                return Err(StoreError::Corrupt("sp_tweaks short body"));
            }
            let len = bytes[i];
            i += 1;
            match len {
                0 => {}
                TWEAK_LEN => {
                    if i + 33 > bytes.len() {
                        return Err(StoreError::Corrupt("sp_tweaks short tweak"));
                    }
                    let mut t = [0u8; 33];
                    t.copy_from_slice(&bytes[i..i + 33]);
                    elig.push((tx_i, t));
                    i += 33;
                }
                _ => return Err(StoreError::Corrupt("sp_tweaks bad len (want 0 or 33)")),
            }
        }
        if i != bytes.len() {
            return Err(StoreError::Corrupt("sp_tweaks n_tx/body mismatch"));
        }
        Ok(elig)
    }

    #[doc(hidden)]
    pub fn read_height_bytes_range(
        &self,
        start: Height,
        count: u32,
    ) -> Result<Option<Vec<Vec<u8>>>, StoreError> {
        if count == 0 {
            return Ok(Some(Vec::new()));
        }
        let inner = self.lock();
        let Some((mut si, mut local)) = Self::locate(&inner, self.origin, start) else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(count as usize);
        let mut left = u64::from(count);
        while left > 0 {
            if si >= inner.segs.len() {
                break;
            }
            let seg = &inner.segs[si];
            if local >= seg.n_slots {
                break;
            }
            let run = left.min(seg.n_slots - local);
            let (starts, end) = Self::read_offs_run(seg, local, run)?;
            let body_start = u64::from(*starts.first().unwrap_or(&0));
            if end < body_start {
                return Err(StoreError::Corrupt("sp_tweaks off order"));
            }
            if body_start < FILE_HEADER_LEN as u64 {
                return Err(StoreError::Corrupt("sp_tweaks off in header"));
            }
            let mut blob = vec![0u8; (end - body_start) as usize];
            if !blob.is_empty() {
                seg.body.read_at(body_start, &mut blob)?;
            }
            for i in 0..starts.len() {
                let s = u64::from(starts[i]);
                let e = if i + 1 < starts.len() {
                    u64::from(starts[i + 1])
                } else {
                    end
                };
                if e < s {
                    return Err(StoreError::Corrupt("sp_tweaks off order"));
                }
                let rel = (s - body_start) as usize;
                let len = (e - s) as usize;
                let slice = blob
                    .get(rel..rel.saturating_add(len))
                    .ok_or(StoreError::Corrupt("sp_tweaks range body truncated"))?;
                out.push(slice.to_vec());
            }
            left -= run;
            local += run;
            if local >= seg.n_slots {
                si += 1;
                local = 0;
            }
        }
        Ok(Some(out))
    }

    #[doc(hidden)]
    pub fn read_height_bytes(&self, height: Height) -> Result<Option<Vec<u8>>, StoreError> {
        match self.read_height_bytes_range(height, 1)? {
            None => Ok(None),
            Some(mut v) => Ok(v.pop()),
        }
    }

    /// Append the next **tip** height. `records.len()` is `header_txs` count.
    pub fn put_block(
        &self,
        height: Height,
        records: &[Option<[u8; 33]>],
    ) -> Result<(), StoreError> {
        self.put_blocks(&[(height, records)])
    }

    /// Append consecutive heights. One body pwrite + one idx pwrite per
    /// segment group (not a pwrite per tx). `items[0].0` must be
    /// [`Self::next_height`].
    ///
    /// Idx `off` is the record **start**. A blob may extend past `u32::MAX`;
    /// when the next start would not fit in `u32`, we roll a new segment.
    pub fn put_blocks(&self, items: &[(Height, &[Option<[u8; 33]>])]) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let mut expect = self.next_height();
        for (h, _) in items {
            if h.0 < self.origin {
                return Err(StoreError::Corrupt("sp_tweaks put below origin"));
            }
            if *h != expect {
                return Err(StoreError::Corrupt("sp_tweaks put not next height"));
            }
            expect = Height(expect.0.saturating_add(1));
        }
        let mut inner = self.lock();
        let mut i = 0usize;
        while i < items.len() {
            let start0 = inner
                .segs
                .last()
                .map(|s| s.body.logical_len())
                .unwrap_or(FILE_HEADER_LEN as u64);
            if start0 > u32::MAX as u64 {
                self.dirs.roll(&mut inner.segs)?;
            }
            let tail = inner
                .segs
                .last_mut()
                .ok_or(StoreError::Corrupt("sp_tweaks no segments"))?;
            let start = tail.body.logical_len();
            if start > u32::MAX as u64 {
                return Err(StoreError::Corrupt("sp_tweaks body exceeds u32 off"));
            }
            let mut body = Vec::new();
            let mut offs: Vec<u32> = Vec::new();
            while i < items.len() {
                let rec_start = start.saturating_add(body.len() as u64);
                if rec_start > u32::MAX as u64 {
                    break;
                }
                offs.push(rec_start as u32);
                body.extend_from_slice(&Self::encode_records(items[i].1));
                i += 1;
            }
            if offs.is_empty() {
                return Err(StoreError::Corrupt("sp_tweaks body exceeds u32 off"));
            }
            // Body before idx, each synced: an idx slot on disk never points
            // at body bytes a power cut can lose.
            if !body.is_empty() {
                tail.body.write_at_pwrite(start, &body)?;
                tail.body.flush()?;
            }
            let mut idx_blob = Vec::with_capacity(offs.len() * SLOT as usize);
            for o in &offs {
                idx_blob.extend_from_slice(&o.to_le_bytes());
            }
            let idx_at = FILE_HEADER_LEN as u64 + tail.n_slots * SLOT;
            if !idx_blob.is_empty() {
                tail.idx.write_at_pwrite(idx_at, &idx_blob)?;
                tail.idx.flush()?;
            }
            tail.n_slots = tail.n_slots.saturating_add(offs.len() as u64);
        }
        Ok(())
    }

    /// `None` = not indexed (below origin or past next).
    pub fn get_block(
        &self,
        height: Height,
        n_tx: u32,
    ) -> Result<Option<Vec<Option<[u8; 33]>>>, StoreError> {
        let Some(buf) = self.read_height_bytes(height)? else {
            return Ok(None);
        };
        Ok(Some(Self::decode_records(&buf, n_tx)?))
    }

    #[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
    /// Eligible tweaks only: `(tx_index_in_block, tweak)`. Not indexed → `None`.
    pub fn get_eligible(
        &self,
        height: Height,
        n_tx: u32,
    ) -> Result<Option<Vec<(u32, [u8; 33])>>, StoreError> {
        match self.get_eligible_range(height, &[n_tx])? {
            None => Ok(None),
            Some(mut v) => Ok(v.pop()),
        }
    }

    #[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
    /// Contiguous eligible tweaks from `start`. `n_tx[i]` is `header_txs` count
    /// at `start + i`. Missing first height → `None`. Index hole → prefix.
    pub fn get_eligible_range(
        &self,
        start: Height,
        n_tx: &[u32],
    ) -> Result<Option<Vec<Vec<(u32, [u8; 33])>>>, StoreError> {
        let Some(bufs) = self.read_height_bytes_range(start, n_tx.len() as u32)? else {
            return Ok(None);
        };
        let n = bufs.len().min(n_tx.len());
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(Self::decode_eligible(&bufs[i], n_tx[i])?);
        }
        Ok(Some(out))
    }

    /// Drop heights **above** `new_tip` (inclusive keep `origin..=new_tip`).
    pub fn truncate_above(&self, new_tip: Height) -> Result<(), StoreError> {
        let keep = if new_tip.0 < self.origin {
            0
        } else {
            u64::from(new_tip.0 - self.origin + 1)
        };
        self.truncate_keep_slots(keep)
    }

    /// Fit the last record to its block's `n_tx` after a crash.
    ///
    /// The last record ends at the body's logical end, so body bytes of a put
    /// whose idx slot never landed would be read as part of it: those are
    /// cut. A record shorter than `n_tx` entries or with a bad length byte (an
    /// idx that reached disk ahead of its body) drops its slot; the caller
    /// then checks the new last record.
    pub fn trim_last_record(&self, n_tx: u32) -> Result<TrimLast, StoreError> {
        let mut inner = self.lock();
        let Some(seg) = inner.segs.iter_mut().rev().find(|s| s.n_slots > 0) else {
            return Ok(TrimLast::Clean);
        };
        let last = seg.n_slots - 1;
        let off = u64::from(Self::read_off(seg, last)?);
        let body_len = seg.body.logical_len();
        let mut bytes = vec![0u8; body_len.saturating_sub(off) as usize];
        seg.body.read_at(off, &mut bytes)?;
        let mut used = Some(0usize);
        for _ in 0..n_tx {
            used = used.and_then(|at| match bytes.get(at) {
                Some(0) => Some(at + 1),
                Some(&TWEAK_LEN) if at + 1 + 33 <= bytes.len() => Some(at + 34),
                _ => None,
            });
        }
        match used {
            Some(n) if n == bytes.len() => Ok(TrimLast::Clean),
            Some(n) => {
                seg.body.set_logical_len(off + n as u64)?;
                seg.body.flush()?;
                Ok(TrimLast::Cut)
            }
            None => {
                seg.idx
                    .set_logical_len(FILE_HEADER_LEN as u64 + last * SLOT)?;
                seg.idx.flush()?;
                seg.body.set_logical_len(off.max(FILE_HEADER_LEN as u64))?;
                seg.body.flush()?;
                seg.n_slots = last;
                Ok(TrimLast::Dropped)
            }
        }
    }

    /// `tip == None` drops every slot (empty chain).
    pub fn truncate_through_tip(&self, tip: Option<Height>) -> Result<(), StoreError> {
        match tip {
            None => self.truncate_keep_slots(0),
            Some(h) => self.truncate_above(h),
        }
    }

    #[doc(hidden)]
    pub fn truncate_keep_slots(&self, keep: u64) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let total: u64 = inner.segs.iter().map(|s| s.n_slots).sum();
        if keep >= total {
            return Ok(());
        }
        if keep == 0 {
            let first = &mut inner.segs[0];
            first.idx.set_logical_len(FILE_HEADER_LEN as u64)?;
            first.body.set_logical_len(FILE_HEADER_LEN as u64)?;
            first.n_slots = 0;
            first.first_slot = 0;
            self.dirs.drop_after(&mut inner.segs, 1);
            return Ok(());
        }
        let last_keep = keep - 1;
        let Some(si) = inner.segs.iter().position(|s| {
            last_keep >= s.first_slot && last_keep < s.first_slot.saturating_add(s.n_slots)
        }) else {
            return Err(StoreError::Corrupt("sp_tweaks truncate locate"));
        };
        let local_keep = last_keep - inner.segs[si].first_slot + 1;
        if local_keep < inner.segs[si].n_slots {
            let new_body = u64::from(Self::read_off(&inner.segs[si], local_keep)?);
            inner.segs[si]
                .idx
                .set_logical_len(FILE_HEADER_LEN as u64 + local_keep * SLOT)?;
            inner.segs[si]
                .body
                .set_logical_len(new_body.max(FILE_HEADER_LEN as u64))?;
            inner.segs[si].n_slots = local_keep;
        }
        self.dirs.drop_after(&mut inner.segs, si + 1);
        Ok(())
    }
}
