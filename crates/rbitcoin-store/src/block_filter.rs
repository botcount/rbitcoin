//! BIP158 basic filter index (`blockfilter.idx/` + `blockfilter.body/`).
//!
//! Additive side table: Class A is unchanged and a missing dir is "not built".
//! Slots are dense from height 0 on the best chain. A reorg truncates above
//! the new tip and those heights are written again.
//!
//! One fixed idx slot per height carries the record's offset and length, so
//! any height's hash or header is one idx pread, and any run of filters is one
//! idx pread plus one body pread per segment. The slot keeps `header_fk` so
//! open can drop slots a crash left on a stale branch.
//!
//! ```text
//! blockfilter.idx/meta      fmt:u32=1
//! blockfilter.idx/NNNNNN    slot[i] = off:u32 ‖ len:u32 ‖ header_fk:u64
//!                                     ‖ filter_hash:[u8;32] ‖ filter_header:[u8;32]
//! blockfilter.body/NNNNNN   filter bytes (Core `BlockFilter` content), records back to back
//! ```
//!
//! When the next record's **start** would exceed `u32::MAX`, a new `NNNNNN`
//! pair starts (the sp_tweaks rule).
//!
//! **Commit order:** body pwrite → body `sync_data` → idx pwrite → idx
//! `sync_data`, all past the visible end, then the slot count is published.
//! Readers never wait on commit IO. After power loss the idx HWM may cover a
//! torn slot and the body may hold bytes of an uncommitted record: open keeps
//! the longest prefix of slots whose records run back to back inside the body,
//! cuts the body to its end, and the caller checks `header_fk` against
//! `confirmed[]`.

use crate::error::StoreError;
use crate::file::{TableFile, FILE_HEADER_LEN};
use crate::seg_pair::{locate, SegDirs, SegPair};
use rbitcoin_primitives::{bytes, le_u32, le_u64, Fk, Height, TableKind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

#[doc(hidden)]
pub const SLOT: u64 = 80;
#[doc(hidden)]
pub const IDX_FMT: u32 = 1;
#[doc(hidden)]
pub const HDR: u64 = FILE_HEADER_LEN as u64;

/// Per-height idx facts (no filter bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockFilterSlot {
    pub header_fk: Fk,
    pub filter_hash: [u8; 32],
    pub filter_header: [u8; 32],
}

/// One height to append.
pub struct BlockFilterRecord<'a> {
    pub height: Height,
    pub slot: BlockFilterSlot,
    pub filter: &'a [u8],
}

#[doc(hidden)]
pub struct Inner {
    pub segs: Vec<SegPair>,
    /// End of the last committed record in the last pair (`HDR` when empty).
    pub tail_end: u64,
}

impl Inner {
    #[doc(hidden)]
    pub fn next(&self) -> u64 {
        self.segs.iter().map(|s| s.n_slots).sum()
    }

    #[doc(hidden)]
    pub fn locate(&self, height: Height) -> Option<(usize, u64)> {
        locate(&self.segs, u64::from(height.0))
    }
}

/// Height-dense basic filter table.
pub struct BlockFilterTable {
    pub dirs: SegDirs,
    pub inner: RwLock<Inner>,
    /// Serializes `put` and `truncate_through` (one writer at a time).
    pub writer: Mutex<()>,
}

/// A filter's bytes and idx facts.
pub type StoredBlockFilter = (Vec<u8>, BlockFilterSlot);
/// Idx rows grouped by segment index.
#[doc(hidden)]
pub type SegRows = Vec<(usize, Vec<SlotRow>)>;

#[doc(hidden)]
pub struct SlotRow {
    pub off: u32,
    pub len: u32,
    pub slot: BlockFilterSlot,
}

#[doc(hidden)]
pub fn encode_slot(off: u32, len: u32, s: &BlockFilterSlot) -> [u8; SLOT as usize] {
    let mut b = [0u8; SLOT as usize];
    b[0..4].copy_from_slice(&off.to_le_bytes());
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[8..16].copy_from_slice(&s.header_fk.0.to_le_bytes());
    b[16..48].copy_from_slice(&s.filter_hash);
    b[48..80].copy_from_slice(&s.filter_header);
    b
}

#[doc(hidden)]
pub fn decode_slot(b: &[u8; SLOT as usize]) -> SlotRow {
    SlotRow {
        off: le_u32!(b, 0),
        len: le_u32!(b, 4),
        slot: BlockFilterSlot {
            header_fk: Fk(le_u64!(b, 8)),
            filter_hash: bytes!(b, 16, 32),
            filter_header: bytes!(b, 48, 32),
        },
    }
}

impl SlotRow {
    #[doc(hidden)]
    pub fn end(&self) -> u64 {
        u64::from(self.off) + u64::from(self.len)
    }
}

#[doc(hidden)]
pub fn read_rows(seg: &SegPair, local: u64, n: u64) -> Result<Vec<SlotRow>, StoreError> {
    let mut b = vec![0u8; (n * SLOT) as usize];
    seg.idx.read_at(HDR + local * SLOT, &mut b)?;
    Ok(b.as_chunks::<{ SLOT as usize }>()
        .0
        .iter()
        .map(decode_slot)
        .collect())
}

/// End of the last record in `seg` (`HDR` when empty).
#[doc(hidden)]
pub fn seg_end(seg: &SegPair) -> Result<u64, StoreError> {
    match seg.n_slots.checked_sub(1) {
        None => Ok(HDR),
        Some(last) => Ok(read_rows(seg, last, 1)?[0].end()),
    }
}

impl BlockFilterTable {
    #[doc(hidden)]
    pub fn idx_dir(dir: &Path) -> PathBuf {
        dir.join("blockfilter.idx")
    }

    #[doc(hidden)]
    pub fn body_dir(dir: &Path) -> PathBuf {
        dir.join("blockfilter.body")
    }

    #[doc(hidden)]
    pub fn meta_path(dir: &Path) -> PathBuf {
        Self::idx_dir(dir).join("meta")
    }

    #[doc(hidden)]
    pub fn seg_dirs(dir: &Path) -> SegDirs {
        SegDirs::new(
            Self::idx_dir(dir),
            Self::body_dir(dir),
            TableKind::BlockFilter,
        )
    }

    #[doc(hidden)]
    pub fn with_segs(dir: &Path, segs: Vec<SegPair>, tail_end: u64) -> Self {
        Self {
            dirs: Self::seg_dirs(dir),
            inner: RwLock::new(Inner { segs, tail_end }),
            writer: Mutex::new(()),
        }
    }

    #[doc(hidden)]
    pub fn create(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(Self::idx_dir(dir)).map_err(|e| StoreError::io(dir, e))?;
        std::fs::create_dir_all(Self::body_dir(dir)).map_err(|e| StoreError::io(dir, e))?;
        let meta = TableFile::create(Self::meta_path(dir), TableKind::ArrayLink)?;
        meta.write_at_pwrite(HDR, &IDX_FMT.to_le_bytes())?;
        meta.flush()?;
        Ok(Self::with_segs(
            dir,
            vec![Self::seg_dirs(dir).create(0, 0)?],
            HDR,
        ))
    }

    #[doc(hidden)]
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let meta = TableFile::open(Self::meta_path(dir), TableKind::ArrayLink)?;
        if meta.logical_len() < HDR + 4 {
            return Err(StoreError::Corrupt("blockfilter.idx/meta short"));
        }
        let mut fmt = [0u8; 4];
        meta.read_at(HDR, &mut fmt)?;
        if u32::from_le_bytes(fmt) != IDX_FMT {
            return Err(StoreError::Corrupt("blockfilter idx format"));
        }
        let mut segs = Self::seg_dirs(dir).open(SLOT)?;
        // Commits only append to the last pair, so only it can be torn.
        let tail = segs
            .last_mut()
            .ok_or(StoreError::Corrupt("segment pairs missing"))?;
        let tail_end = Self::clamp_torn_tail(tail)?;
        Ok(Self::with_segs(dir, segs, tail_end))
    }

    /// Drop trailing slots until the last one chains onto its predecessor and
    /// ends inside the body, then cut the idx and body to it.
    ///
    /// Commits only append, so a crash can only tear the tail: this reads a
    /// few slots at the end, not the whole idx.
    #[doc(hidden)]
    pub fn clamp_torn_tail(seg: &mut SegPair) -> Result<u64, StoreError> {
        let body_len = seg.body.logical_len();
        let mut keep = seg.n_slots;
        let end = loop {
            let Some(last) = keep.checked_sub(1) else {
                break HDR;
            };
            let first = last.saturating_sub(1);
            let rows = read_rows(seg, first, last - first + 1)?;
            let row = &rows[rows.len() - 1];
            let start = if last == 0 { HDR } else { rows[0].end() };
            if u64::from(row.off) == start && row.end() <= body_len {
                break row.end();
            }
            keep = last;
        };
        let idx_len = HDR + keep * SLOT;
        if keep < seg.n_slots || seg.idx.logical_len() != idx_len || body_len != end {
            rbitcoin_log::warn!(
                "blockfilter: dropping torn tail file={:06} slots {}→{} body {}→{}",
                seg.file_id,
                seg.n_slots,
                keep,
                body_len,
                end
            );
            seg.idx.set_logical_len(idx_len)?;
            seg.idx.flush()?;
            seg.body.set_logical_len(end)?;
            seg.body.flush()?;
        }
        seg.n_slots = keep;
        Ok(end)
    }

    /// Open the table under `dir`, or create an empty one.
    pub fn open_or_create(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref();
        if Self::meta_path(dir).is_file() {
            return Self::open(dir);
        }
        let _ = std::fs::remove_dir_all(Self::idx_dir(dir));
        let _ = std::fs::remove_dir_all(Self::body_dir(dir));
        Self::create(dir)
    }

    #[doc(hidden)]
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    #[doc(hidden)]
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Next height this table accepts (0 when empty).
    pub fn next_height(&self) -> Height {
        Height(self.read().next() as u32)
    }

    /// Idx facts at `height` (one pread). `None` past the watermark.
    pub fn slot(&self, height: Height) -> Result<Option<BlockFilterSlot>, StoreError> {
        Ok(self.slots(height, height)?.and_then(|mut v| v.pop()))
    }

    /// Idx rows for `start..=end` grouped by segment (one pread each).
    #[doc(hidden)]
    pub fn rows(inner: &Inner, start: Height, end: Height) -> Result<Option<SegRows>, StoreError> {
        if start > end || inner.locate(end).is_none() {
            return Ok(None);
        }
        let Some((mut si, mut local)) = inner.locate(start) else {
            return Ok(None);
        };
        let mut left = u64::from(end.0 - start.0) + 1;
        let mut out = Vec::new();
        while left > 0 {
            let seg = &inner.segs[si];
            let run = left.min(seg.n_slots - local);
            out.push((si, read_rows(seg, local, run)?));
            left -= run;
            si += 1;
            local = 0;
        }
        Ok(Some(out))
    }

    /// Filter headers at heights 1,000, 2,000, … through `stop`.
    ///
    /// One idx pread per checkpoint. `None` when `stop` is past the watermark.
    pub fn checkpoint_headers(&self, stop: Height) -> Result<Option<Vec<[u8; 32]>>, StoreError> {
        let inner = self.read();
        if inner.locate(stop).is_none() {
            return Ok(None);
        }
        if stop.0 < 1000 {
            return Ok(Some(Vec::new()));
        }
        let mut out = Vec::new();
        let mut h = 1000u32;
        while h <= stop.0 {
            let Some((si, local)) = inner.locate(Height(h)) else {
                return Ok(None);
            };
            let rows = read_rows(&inner.segs[si], local, 1)?;
            let Some(row) = rows.first() else {
                return Err(StoreError::Corrupt("blockfilter checkpoint slot missing"));
            };
            out.push(row.slot.filter_header);
            h = h.saturating_add(1000);
        }
        Ok(Some(out))
    }

    /// Idx facts for `start..=end`. `None` when `end` is past the watermark.
    pub fn slots(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<BlockFilterSlot>>, StoreError> {
        let inner = self.read();
        Ok(Self::rows(&inner, start, end)?.map(|groups| {
            groups
                .into_iter()
                .flat_map(|(_, r)| r)
                .map(|r| r.slot)
                .collect()
        }))
    }

    /// Filters and idx facts for `start..=end`: one idx pread and one body
    /// pread per segment. `None` when `end` is past the watermark.
    pub fn filters(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<StoredBlockFilter>>, StoreError> {
        let inner = self.read();
        let Some(groups) = Self::rows(&inner, start, end)? else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity((end.0 - start.0 + 1) as usize);
        for (si, rows) in groups {
            let lo = u64::from(rows[0].off);
            let hi = rows[rows.len() - 1].end();
            let mut span = vec![0u8; (hi - lo) as usize];
            inner.segs[si].body.read_at(lo, &mut span)?;
            for r in rows {
                let at = (u64::from(r.off) - lo) as usize;
                let bytes = span
                    .get(at..at + r.len as usize)
                    .ok_or(StoreError::Corrupt(
                        "invariant: blockfilter slot outside span",
                    ))?;
                out.push((bytes.to_vec(), r.slot));
            }
        }
        Ok(Some(out))
    }

    /// Filter bytes and idx facts at `height`. `None` past the watermark.
    pub fn filter(&self, height: Height) -> Result<Option<StoredBlockFilter>, StoreError> {
        Ok(self.filters(height, height)?.and_then(|mut v| v.pop()))
    }

    /// Append consecutive heights starting at [`Self::next_height`], durably.
    pub fn put(&self, items: &[BlockFilterRecord<'_>]) -> Result<(), StoreError> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let next = self.read().next();
        if items
            .iter()
            .zip(next..)
            .any(|(it, want)| u64::from(it.height.0) != want)
        {
            return Err(StoreError::Corrupt(
                "invariant: blockfilter put not next height",
            ));
        }
        let mut i = 0usize;
        while i < items.len() {
            if self.read().tail_end > u64::from(u32::MAX) {
                let mut inner = self.write();
                self.dirs.roll(&mut inner.segs)?;
                inner.tail_end = HDR;
            }
            let (n, end) = {
                let inner = self.read();
                let tail = inner
                    .segs
                    .last()
                    .ok_or(StoreError::Corrupt("blockfilter no segments"))?;
                let mut body = Vec::new();
                let mut idx_blob = Vec::new();
                let mut at = inner.tail_end;
                while i < items.len() {
                    let (Ok(off), Ok(len)) =
                        (u32::try_from(at), u32::try_from(items[i].filter.len()))
                    else {
                        break;
                    };
                    idx_blob.extend_from_slice(&encode_slot(off, len, &items[i].slot));
                    body.extend_from_slice(items[i].filter);
                    at += u64::from(len);
                    i += 1;
                }
                if idx_blob.is_empty() {
                    return Err(StoreError::Corrupt("blockfilter body exceeds u32 off"));
                }
                tail.body.write_at_pwrite(inner.tail_end, &body)?;
                tail.body.flush()?;
                tail.idx
                    .write_at_pwrite(HDR + tail.n_slots * SLOT, &idx_blob)?;
                tail.idx.flush()?;
                (idx_blob.len() as u64 / SLOT, at)
            };
            let mut inner = self.write();
            let tail = inner
                .segs
                .last_mut()
                .ok_or(StoreError::Corrupt("blockfilter no segments"))?;
            tail.n_slots += n;
            inner.tail_end = end;
        }
        Ok(())
    }

    /// Drop heights above `tip` (`None` drops every slot).
    pub fn truncate_through(&self, tip: Option<Height>) -> Result<(), StoreError> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let keep = tip.map_or(0, |h| u64::from(h.0) + 1);
        let mut inner = self.write();
        if keep >= inner.next() {
            return Ok(());
        }
        let si = match keep.checked_sub(1) {
            None => 0,
            Some(last) => inner
                .segs
                .iter()
                .position(|s| last >= s.first_slot && last < s.first_slot + s.n_slots)
                .ok_or(StoreError::Corrupt("blockfilter truncate locate"))?,
        };
        let seg = &mut inner.segs[si];
        let local_keep = keep - seg.first_slot;
        if local_keep < seg.n_slots {
            let body_end = u64::from(read_rows(seg, local_keep, 1)?[0].off);
            seg.idx.set_logical_len(HDR + local_keep * SLOT)?;
            seg.idx.flush()?;
            seg.body.set_logical_len(body_end)?;
            seg.body.flush()?;
            seg.n_slots = local_keep;
        }
        inner.tail_end = seg_end(&inner.segs[si])?;
        self.dirs.drop_after(&mut inner.segs, si + 1);
        Ok(())
    }
}
