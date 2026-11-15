//! Segmented Class A `tx.head`: fixed-bits open-address tables + seal-time fuse8.
//!
//! Layout:
//! ```text
//! store/
//!   tx.head/
//!     meta                       # segment descriptors
//!     000000                     # open OA only (unlinked after seal)
//!     000000.mphf + .fuse8     # sealed: value-assigned MPHF + fuse8
//!     …
//! ```
//!
//! Leftover flat `tx.head.meta` **refuses** (wipe `store/tx.head`; Class A kept).
//!
//! **Relative fks:** slot stores `rel` where `0` = empty and
//! `fk = first_fk + rel - 1` (1-based relative within the segment).
//!
//! **Capacity:** open segment ends at `floor(slots × HEAD_LOAD_START)` (80%);
//! then open a new head and seal the previous OA on a sidecar (MPHF + fuse8).
//! Lookup probes every unsealed OA until publish.
//!
//! **Lookup:** unsealed OAs newest-first; then sealed newest→oldest gated by fuse8;
//! candidates are absolute fks for body-verify by the caller.

use crate::address_head::{AddressHead, HeadLayout, HEAD_LOAD_START, MAINNET_BITS};
use crate::error::StoreError;
use crate::fuse8_filter::{fuse_key_from_mixed, open_file, SealedFuse8};
use crate::tx_head_mphf::TxHeadMphf;
use rbitcoin_primitives::{le_u32, le_u64, Fk, SCHEMA_VERSION, STORE_MAGIC};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// v2 adds [`FLAG_TAGGED`]. A v1 meta still opens: its unsealed OAs predate
/// slot fingerprints and get them written on that open.
const META_VERSION: u32 = 2;
const META_V1_UNTAGGED: u32 = 1;
const META_HEADER_LEN: usize = 24;
const SEG_DESC_LEN: usize = 32;
const FLAG_SEALED: u32 = 1;
/// Every slot of this unsealed OA carries its key's fingerprint.
const FLAG_TAGGED: u32 = 2;

/// Collect `(fuse_key, rel)` for a sealed range (`first_fk`, `count`).
///
/// Runs on the seal sidecar (or crash-reopen). `'static` so the worker can
/// pread `txid.body` after the write thread has already rolled.
pub type SealCollect = Arc<dyn Fn(u64, u64) -> Result<Vec<(u64, u32)>, StoreError> + Send + Sync>;

/// Product default head width (2²⁵ slots × 4 B = 128 MiB per segment).
pub const SEGMENT_HEAD_BITS: u32 = MAINNET_BITS;

#[doc(hidden)]
pub struct Segment {
    pub first_fk: u64,
    pub count: AtomicU64,
    pub file_id: u32,
    pub sealed: bool,
    /// Unsealed OA whose slots all carry fingerprints (false only for a
    /// meta v1 head until `TxTable` writes them).
    pub tagged: AtomicBool,
    pub head: Option<Arc<AddressHead>>,
    pub pack: Option<Arc<TxHeadMphf>>,
    pub fuse: Option<SealedFuse8>,
}

pub struct SealPublish {
    pub file_id: u32,
    pub pack: crate::tx_head_mphf::TxHeadMphf,
}

/// Multi-segment keyless address head with seal-time binary fuse8.
pub struct SegmentedTxHead {
    pub dir: PathBuf,
    pub layout: HeadLayout,
    pub segments: RwLock<Arc<Vec<Arc<Segment>>>>,
    pub next_file_id: AtomicU32,
    pub max_keys: u64,
    /// Serializes seal/roll + inserts (sole Class A appender still the rule).
    pub write: Mutex<()>,
    /// Background seal of the previous open OA (not joined on insert).
    pub seal_rx: Mutex<Option<Receiver<Result<SealPublish, StoreError>>>>,
}

impl SegmentedTxHead {
    pub fn create(dir: &Path, layout: HeadLayout) -> Result<Self, StoreError> {
        let dir = dir.to_path_buf();
        refuse_legacy_mono_head(&dir)?;
        write_meta(&dir, layout.bits, &[])?;
        Ok(Self {
            dir,
            max_keys: max_keys_for_layout(layout),
            layout,
            segments: RwLock::new(Arc::new(Vec::new())),
            next_file_id: AtomicU32::new(0),
            write: Mutex::new(()),
            seal_rx: Mutex::new(None),
        })
    }

    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let dir = dir.to_path_buf();
        refuse_legacy_mono_head(&dir)?;
        let (bits, descs) = read_meta(&dir)?;
        let layout = HeadLayout::new(bits)?;
        let max_keys = max_keys_for_layout(layout);
        let mut segs = Vec::with_capacity(descs.len());
        let mut max_id = 0u32;
        for d in descs {
            let path = segment_head_path(&dir, d.file_id);
            let sealed = d.flags & FLAG_SEALED != 0;
            let tagged = sealed || d.flags & FLAG_TAGGED != 0;
            let (head, pack) = if sealed {
                if !TxHeadMphf::exists(&path) {
                    return Err(StoreError::Corrupt("tx.head sealed segment missing mphf"));
                }
                if path.is_file() {
                    // Crash between sealed-meta persist and OA unlink.
                    rbitcoin_log::warn!(
                        "store: tx.head discarding leftover OA for sealed segment file_id={}",
                        d.file_id
                    );
                    let _ = std::fs::remove_file(&path);
                }
                (None, Some(Arc::new(TxHeadMphf::open(&path)?)))
            } else {
                let head = AddressHead::open(&path)?;
                if head.bits() != bits {
                    return Err(StoreError::Corrupt("tx.head segment layout mismatch"));
                }
                (Some(Arc::new(head)), None)
            };
            let fuse = if sealed {
                let fp = segment_fuse_path(&dir, d.file_id);
                if !fp.exists() {
                    return Err(StoreError::Corrupt("tx.head sealed segment missing fuse8"));
                }
                Some(open_file(&fp)?)
            } else {
                None
            };
            max_id = max_id.max(d.file_id);
            segs.push(Arc::new(Segment {
                first_fk: d.first_fk,
                count: AtomicU64::new(d.count),
                file_id: d.file_id,
                sealed,
                tagged: AtomicBool::new(tagged),
                head,
                pack,
                fuse,
            }));
        }
        let unsealed_nontail = segs
            .iter()
            .enumerate()
            .filter(|(i, s)| i + 1 != segs.len() && !s.sealed)
            .count();
        if unsealed_nontail > 1 {
            return Err(StoreError::Corrupt(
                "tx.head multiple unsealed non-tail segments",
            ));
        }
        for w in segs.windows(2) {
            let a_end = w[0]
                .first_fk
                .saturating_add(w[0].count.load(Ordering::Relaxed));
            if w[1].first_fk != a_end {
                return Err(StoreError::Corrupt("tx.head segment fk gap/overlap"));
            }
        }
        // One summary for the whole head (not one line per segment).
        // Per-seg detail: `file_id@first_fk:count{s|o}` (s=sealed, o=open tail).
        let sealed_n = segs.iter().filter(|s| s.sealed).count();
        let open_n = segs.len().saturating_sub(sealed_n);
        let creates: u64 = segs.iter().map(|s| s.count.load(Ordering::Relaxed)).sum();
        let detail: String = segs
            .iter()
            .map(|s| {
                let c = s.count.load(Ordering::Relaxed);
                let flag = if s.sealed { 's' } else { 'o' };
                format!("{}@{}:{}{}", s.file_id, s.first_fk, c, flag)
            })
            .collect::<Vec<_>>()
            .join(" ");
        rbitcoin_log::info!(
            "store: tx.head open bits={bits} entry=4B slots={} segs={} sealed={sealed_n} \
             open={open_n} creates≈{creates} [{detail}]",
            layout.slots(),
            segs.len(),
        );
        Ok(Self {
            dir,
            layout,
            segments: RwLock::new(Arc::new(segs)),
            next_file_id: AtomicU32::new(max_id.saturating_add(1)),
            max_keys,
            write: Mutex::new(()),
            seal_rx: Mutex::new(None),
        })
    }

    pub fn bits(&self) -> u32 {
        self.layout.bits
    }

    pub fn slots(&self) -> u64 {
        self.layout.slots()
    }

    pub fn segment_count(&self) -> usize {
        self.segments_snapshot().len()
    }

    /// Per-segment `first_fk` (index 0 = oldest; last = open/tip).
    ///
    /// Used by head-resolve winner-age stats ([`crate::head_resolve_stats::sealed_age_for_fk`]).
    pub fn first_fks_snapshot(&self) -> Vec<u64> {
        self.segments_snapshot()
            .iter()
            .map(|s| s.first_fk)
            .collect()
    }

    pub fn sealed_segment_count(&self) -> usize {
        self.segments_snapshot().iter().filter(|s| s.sealed).count()
    }

    /// Sum of segment fk spans. Equals the number of indexed creates unless
    /// Class A holds bodies the head never received (those gap fks count too).
    pub fn occupied(&self) -> u64 {
        self.segments_snapshot()
            .iter()
            .map(|s| s.count.load(Ordering::Relaxed))
            .sum()
    }

    /// First fk of the oldest unsealed segment. Probes read every row at or
    /// above it from open-address slot pages, which a power loss can leave
    /// with holes under a synced `meta` count: insert syncs `meta` but not
    /// the pages. A roll syncs the pages it closes, but a hole already in
    /// them stays there until that segment's seal publishes, and the open tail
    /// and the in-flight seal are both unsealed. A seal rebuilds its rows from
    /// `txid.body` and syncs its files before `meta` names it sealed, so rows
    /// below this fk are complete. Never falls: a publish seals the oldest
    /// unsealed segment, and a roll opens past the last create. Past the last
    /// create when every segment is sealed.
    pub fn unsynced_first_fk(&self) -> u64 {
        let segs = self.segments_snapshot();
        match segs.iter().find(|s| !s.sealed) {
            Some(s) => s.first_fk,
            None => self.last_inserted_fk().saturating_add(1),
        }
    }

    /// Highest create_fk present in any segment (0 if empty).
    pub fn last_inserted_fk(&self) -> u64 {
        let segs = self.segments_snapshot();
        for s in segs.iter().rev() {
            let c = s.count.load(Ordering::Relaxed);
            if c > 0 {
                return s.first_fk.saturating_add(c).saturating_sub(1);
            }
        }
        0
    }

    pub fn sealed_mphf_g_resident_bytes(&self) -> u64 {
        self.segments_snapshot()
            .iter()
            .map(|s| s.pack.as_ref().map(|p| p.g_bytes_resident()).unwrap_or(0))
            .sum()
    }

    /// Mapped slot arrays of the unsealed segments (file-backed, not heap).
    pub fn open_map_bytes(&self) -> u64 {
        self.segments_snapshot()
            .iter()
            .map(|s| s.head.as_ref().map(|h| h.map_bytes()).unwrap_or(0))
            .sum()
    }

    /// Unsealed segments whose slots still lack fingerprints (meta v1 heads):
    /// `(file_id, first_fk, count)`.
    pub fn untagged_open_ranges(&self) -> Vec<(u32, u64, u64)> {
        self.segments_snapshot()
            .iter()
            .filter(|s| !s.sealed && !s.tagged.load(Ordering::Relaxed))
            .map(|s| (s.file_id, s.first_fk, s.count.load(Ordering::Relaxed)))
            .collect()
    }

    /// Record that every slot of unsealed segment `file_id` carries its
    /// fingerprint (after [`Self::fill_fingerprints`] and a flush).
    pub fn mark_tagged(&self, file_id: u32) -> Result<(), StoreError> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let segs = self.segments_snapshot();
        let seg = segs
            .iter()
            .find(|s| s.file_id == file_id)
            .ok_or(StoreError::Corrupt("tx.head mark tagged: file_id missing"))?;
        seg.tagged.store(true, Ordering::Relaxed);
        self.persist_meta_locked()
    }

    /// Write fingerprints into unsealed segment `file_id` for rows that
    /// predate tags. `keys` are `(mixed, absolute fk)`; fks before the segment
    /// are skipped.
    pub fn fill_fingerprints(
        &self,
        file_id: u32,
        keys: &[([u8; 32], Fk)],
    ) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        let seg = segs
            .iter()
            .find(|s| s.file_id == file_id)
            .ok_or(StoreError::Corrupt(
                "tx.head fill fingerprints: file_id missing",
            ))?;
        let Some(head) = seg.head.as_ref() else {
            return Ok(());
        };
        let rels: Vec<([u8; 32], Fk)> = keys
            .iter()
            .filter(|(_, fk)| fk.0 >= seg.first_fk)
            .map(|(m, fk)| (*m, Fk(fk.0 - seg.first_fk + 1)))
            .collect();
        head.fill_fingerprints(&rels)
    }

    /// Heap-owned sealed fuse8 fingerprints (**0** when mapped from `.fuse8`).
    pub fn sealed_fuse_resident_bytes(&self) -> u64 {
        self.segments_snapshot()
            .iter()
            .map(|s| {
                s.fuse
                    .as_ref()
                    .map(|f| f.fingerprint_heap_bytes() as u64)
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Open-tail page hop dump for leftover-miss diagnostics.
    pub fn leftover_open_hop(
        &self,
        mixed: &[u8; 32],
    ) -> Result<(u32, u64, crate::address_head::PageHopDump), StoreError> {
        let segs = self.segments_snapshot();
        let Some(last) = segs.last() else {
            return Ok((
                0,
                0,
                crate::address_head::PageHopDump {
                    scan: crate::address_head::ProbeRegionScan {
                        cands: crate::address_head::ProbeCands::default(),
                        hit_empty: true,
                        depth_end: 0,
                        empty_local: 0,
                    },
                    occupied: 0,
                },
            ));
        };
        let Some(head) = last.head.as_ref() else {
            return Err(StoreError::Corrupt("tx.head leftover hop: tail sealed"));
        };
        let dump = head.dump_page_hop(mixed);
        Ok((last.file_id, last.first_fk, dump))
    }

    #[doc(hidden)]
    pub fn segments_snapshot(&self) -> Arc<Vec<Arc<Segment>>> {
        Arc::clone(&self.segments.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Insert mixed probe keys → absolute create fks (sole writer).
    ///
    /// Relativizes `entries` fks in place (absolute → segment-relative) then
    /// sorts that same buffer in the open HashHead — no extra pair copy.
    ///
    /// Rolls when the open segment's fk span reaches `max_keys` (80% of slots),
    /// or the next fk lies past that span. Publish drains
    /// on the next `insert_many`, [`Self::flush`], or `Drop` — not joined on
    /// the roll that started it. Seal keys are collected on the sidecar from
    /// `txid.body` (crash-reopen stays on the open thread).
    #[cfg(debug_assertions)]
    pub fn insert_many(&self, entries: &mut [([u8; 32], Fk)]) -> Result<(), StoreError> {
        let snap: Vec<([u8; 32], u64)> = entries.iter().map(|(m, f)| (*m, f.0)).collect();
        let collect: SealCollect = Arc::new(move |first_fk, count| {
            let mut pairs = Vec::with_capacity(count as usize);
            for i in 0..count {
                let fk = first_fk + i;
                let mixed = snap.iter().find(|(_, f)| *f == fk).map(|(m, _)| *m).ok_or(
                    StoreError::Corrupt("tx.head seal collect: fk not in insert batch"),
                )?;
                pairs.push((fuse_key_from_mixed(&mixed), (i as u32) + 1));
            }
            Ok(pairs)
        });
        self.insert_many_with(entries, collect)
    }

    /// Insert; `collect` runs on the seal sidecar at roll (not on this thread).
    pub fn insert_many_with(
        &self,
        entries: &mut [([u8; 32], Fk)],
        collect: SealCollect,
    ) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());

        self.try_publish_seal_locked()?;

        // Callers pass non-decreasing absolute fks. `count` is the span, not the entry count.
        let mut i = 0usize;
        while i < entries.len() {
            self.ensure_open_for(entries[i].1 .0)?;
            let segs = self.segments_snapshot();
            let last = segs
                .last()
                .ok_or(StoreError::Corrupt("tx.head no open segment"))?;
            if last.sealed {
                return Err(StoreError::Corrupt("tx.head tail sealed unexpectedly"));
            }
            let count = last.count.load(Ordering::Relaxed);
            if count >= self.max_keys {
                self.roll_tail_background_locked(collect.clone())?;
                continue;
            }
            let first_fk = last.first_fk;
            let span_end = first_fk.saturating_add(self.max_keys);
            let take = entries[i..]
                .iter()
                .take_while(|(_, fk)| fk.0 < span_end)
                .count();
            if take == 0 {
                // The next fk lies past this segment's span (a gap at its end).
                // An empty segment wholly inside the gap claims its full span,
                // or the roll is a no-op (count == 0) and this loop never ends.
                // A gap of k whole spans costs k serial seals here; on mainnet
                // that needs a gap of max_keys (~26.8M) orphan bodies.
                if count == 0 {
                    last.count.store(self.max_keys, Ordering::Relaxed);
                }
                self.roll_tail_background_locked(collect.clone())?;
                continue;
            }
            let batch = &mut entries[i..i + take];

            let mut max_rel = count;
            for (_mixed, fk) in batch.iter_mut() {
                if fk.0 < first_fk {
                    return Err(StoreError::Corrupt("tx.head insert fk before segment"));
                }
                let rel = fk.0 - first_fk + 1;
                if rel == 0 || rel > u32::MAX as u64 {
                    return Err(StoreError::Corrupt("tx.head relative fk overflow"));
                }
                max_rel = max_rel.max(rel);
                *fk = Fk(rel);
            }
            last.head
                .as_ref()
                .ok_or(StoreError::Corrupt("tx.head insert: open missing OA"))?
                .insert_many_in_place(batch)?;
            last.count.fetch_max(max_rel, Ordering::Relaxed);
            i += take;

            if max_rel >= self.max_keys {
                self.roll_tail_background_locked(collect.clone())?;
            }
        }
        self.persist_meta_locked()?;
        Ok(())
    }
}

/// Which head segments a probe walks. Lookup uses [`HeadProbeWave::All`]
/// for a full probe and [`HeadProbeWave::Open`] for the unsealed tail.
/// Sealed segments are retired one at a time by the resolve machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadProbeWave {
    /// Unsealed OAs, then every sealed segment (newest first).
    All,
    /// Unsealed OAs only (insert tail + in-flight seal), newest first.
    Open,
}

impl SegmentedTxHead {
    /// Probe absolute create_fk candidates for a mixed key (open → sealed new→old).
    ///
    /// Order within each segment: deepest probe first is applied by reversing
    /// the page probe list. Across segments: open first, then sealed newest first.
    /// Caller body-verifies.
    pub fn probe_candidates(&self, mixed: &[u8; 32]) -> Result<Vec<Fk>, StoreError> {
        let mut out = self.probe_candidates_batch(std::slice::from_ref(mixed))?;
        Ok(out.pop().unwrap_or_default())
    }

    /// Batch probe: same order/results as N× [`Self::probe_candidates`], with
    /// **page-coalesced** loads inside each segment ([`AddressHead::probe_fks_batch`]).
    ///
    /// Sealed segments still fuse-gate per key; only keys that pass are batched
    /// for that segment's page loads. Page IO uses TLS bulk_io.
    pub fn probe_candidates_batch(&self, mixed: &[[u8; 32]]) -> Result<Vec<Vec<Fk>>, StoreError> {
        self.probe_candidates_batch_wave(mixed, HeadProbeWave::All, None, &mut crate::IoCtx::none())
    }

    /// One probe walk: `wave` + shared [`crate::IoCtx`] (held session or standalone).
    pub fn probe_candidates_batch_wave(
        &self,
        mixed: &[[u8; 32]],
        wave: HeadProbeWave,
        active: Option<&[bool]>,
        ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<Vec<Fk>>, StoreError> {
        let n = mixed.len();
        let mut out = vec![Vec::new(); n];
        if n == 0 {
            return Ok(out);
        }
        let segs = self.segments_snapshot();
        if segs.is_empty() {
            return Ok(out);
        }
        self.probe_unsealed_wave(mixed, active, &mut out);
        if matches!(wave, HeadProbeWave::All) {
            self.probe_sealed_wave(mixed, active, ctx, &mut out)?;
        }
        Ok(out)
    }

    /// One sealed segment, newest-first caller order. Unsealed `si` returns
    /// empty lists (the open wave already probed those OAs).
    pub fn probe_sealed_segment(
        &self,
        mixed: &[[u8; 32]],
        si: usize,
        active: Option<&[bool]>,
        ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<Vec<Fk>>, StoreError> {
        let n = mixed.len();
        let mut out = vec![Vec::new(); n];
        if n == 0 {
            return Ok(out);
        }
        let segs = self.segments_snapshot();
        if si >= segs.len() {
            return Ok(out);
        }
        self.fill_sealed_segment(&segs, si, mixed, active, ctx, &mut out)?;
        Ok(out)
    }

    #[doc(hidden)]
    pub fn probe_unsealed_wave(
        &self,
        mixed: &[[u8; 32]],
        active: Option<&[bool]>,
        out: &mut [Vec<Fk>],
    ) {
        let segs = self.segments_snapshot();
        let key_on = |i: usize| active.map(|a| a[i]).unwrap_or(true);
        for seg in segs.iter().rev() {
            if seg.sealed {
                continue;
            }
            let Some(head) = seg.head.as_ref() else {
                continue;
            };
            let mut pass_i: Vec<usize> = Vec::new();
            let mut pass_keys: Vec<[u8; 32]> = Vec::new();
            for (i, key) in mixed.iter().enumerate() {
                if !key_on(i) {
                    continue;
                }
                pass_i.push(i);
                pass_keys.push(*key);
            }
            if pass_keys.is_empty() {
                continue;
            }
            let rel_lists = head.probe_fks_batch(&pass_keys);
            for (orig_i, rels) in pass_i.into_iter().zip(rel_lists) {
                for r in rels.into_iter().rev() {
                    if let Some(fk) = rel_to_abs(seg.first_fk, r.0) {
                        out[orig_i].push(fk);
                    }
                }
            }
        }
    }

    #[doc(hidden)]
    pub fn probe_sealed_wave(
        &self,
        mixed: &[[u8; 32]],
        active: Option<&[bool]>,
        ctx: &mut crate::IoCtx<'_>,
        out: &mut [Vec<Fk>],
    ) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        for si in (0..segs.len()).rev() {
            if !segs[si].sealed {
                continue;
            }
            self.fill_sealed_segment(&segs, si, mixed, active, ctx, out)?;
        }
        Ok(())
    }

    /// Fuse-filter `active` keys, then one MPHF batch (or the sealed OA).
    #[doc(hidden)]
    pub fn fill_sealed_segment(
        &self,
        segs: &[Arc<Segment>],
        si: usize,
        mixed: &[[u8; 32]],
        active: Option<&[bool]>,
        ctx: &mut crate::IoCtx<'_>,
        out: &mut [Vec<Fk>],
    ) -> Result<(), StoreError> {
        let seg = &segs[si];
        if !seg.sealed {
            return Ok(());
        }
        let Some(fuse) = seg.fuse.as_ref() else {
            return Err(StoreError::Corrupt("sealed segment missing fuse"));
        };
        let key_on = |i: usize| active.map(|a| a[i]).unwrap_or(true);

        let mut pass_i: Vec<usize> = Vec::new();
        let mut pass_keys: Vec<[u8; 32]> = Vec::new();
        for (i, m) in mixed.iter().enumerate() {
            if !key_on(i) {
                continue;
            }
            let fuse_key = fuse_key_from_mixed(m);
            if !fuse.contains(fuse_key) {
                continue;
            }
            pass_i.push(i);
            pass_keys.push(*m);
        }
        if pass_keys.is_empty() {
            return Ok(());
        }
        if let Some(pack) = seg.pack.as_ref() {
            let mixed_u: Vec<u64> = pass_keys.iter().map(fuse_key_from_mixed).collect();
            let slots = pack.slots_for_ctx(&mixed_u, ctx)?;
            let rel_lists = pack.read_rels_batch(&slots, ctx)?;
            for (orig_i, rels) in pass_i.into_iter().zip(rel_lists) {
                for r in rels {
                    if let Some(fk) = rel_to_abs(seg.first_fk, u64::from(r)) {
                        out[orig_i].push(fk);
                    }
                }
            }
        } else {
            let rel_lists = seg
                .head
                .as_ref()
                .ok_or(StoreError::Corrupt("tx.head sealed probe: missing pack"))?
                .probe_fks_batch(&pass_keys);
            for (orig_i, rels) in pass_i.into_iter().zip(rel_lists) {
                for r in rels.into_iter().rev() {
                    if let Some(fk) = rel_to_abs(seg.first_fk, r.0) {
                        out[orig_i].push(fk);
                    }
                }
            }
        }
        Ok(())
    }

    /// Rewrite this head as a meta v1 head: untagged slots, no `FLAG_TAGGED`.
    #[cfg(debug_assertions)]
    pub fn downgrade_to_v1_for_test(&self) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        let mut descs = Vec::new();
        for s in segs.iter() {
            if let Some(head) = s.head.as_ref() {
                head.strip_tags_for_test()?;
            }
            let flags = if s.sealed { FLAG_SEALED } else { 0 };
            descs.push((
                s.first_fk,
                s.count.load(Ordering::Relaxed),
                s.file_id,
                flags,
            ));
        }
        write_meta_versioned(&self.dir, META_V1_UNTAGGED, self.layout.bits, &descs)
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        for s in segs.iter() {
            if let Some(h) = s.head.as_ref() {
                h.flush()?;
            }
        }
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.wait_seal_locked()?;
        self.persist_meta_locked()?;
        Ok(())
    }

    pub fn flush_async(&self) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        for s in segs.iter() {
            if let Some(h) = s.head.as_ref() {
                h.flush_async()?;
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn ensure_open_for(&self, first_fk: u64) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        let Some(last) = segs.last() else {
            return self.open_new_locked(first_fk);
        };
        if last.sealed {
            return self.open_new_locked(first_fk);
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn open_new_locked(&self, first_fk: u64) -> Result<(), StoreError> {
        if first_fk == 0 {
            return Err(StoreError::InvalidFk);
        }
        let file_id = self.next_file_id.fetch_add(1, Ordering::Relaxed);
        let path = segment_head_path(&self.dir, file_id);
        let _ = std::fs::remove_file(&path);
        let head = AddressHead::create_with_layout(&path, self.layout)?;
        let seg = Arc::new(Segment {
            first_fk,
            count: AtomicU64::new(0),
            file_id,
            sealed: false,
            tagged: AtomicBool::new(true),
            head: Some(Arc::new(head)),
            pack: None,
            fuse: None,
        });
        {
            let mut guard = self.segments.write().unwrap_or_else(|e| e.into_inner());
            let mut new_list = (**guard).clone();

            if let Some(last) = new_list.last() {
                if !last.sealed && last.count.load(Ordering::Relaxed) == 0 {
                    let fid = last.file_id;
                    new_list.pop();
                    let _ = std::fs::remove_file(segment_head_path(&self.dir, fid));
                }
            }
            new_list.push(seg);
            *guard = Arc::new(new_list);
        }

        rbitcoin_log::info!(
            "store: tx.head roll open file_id={file_id} first_fk={first_fk} bits={} slots={}",
            self.layout.bits,
            self.layout.slots(),
        );
        self.persist_meta_locked()?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn try_publish_seal_locked(&self) -> Result<(), StoreError> {
        let rx = {
            let mut g = self.seal_rx.lock().unwrap_or_else(|e| e.into_inner());
            g.take()
        };
        let Some(rx) = rx else {
            return Ok(());
        };
        match rx.try_recv() {
            Ok(Ok(p)) => self.apply_seal_publish_locked(p),
            Ok(Err(e)) => Err(e),
            Err(mpsc::TryRecvError::Empty) => {
                *self.seal_rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
                Ok(())
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(StoreError::Corrupt(
                "tx.head background seal worker disconnected",
            )),
        }
    }

    #[doc(hidden)]
    pub fn wait_seal_locked(&self) -> Result<(), StoreError> {
        let rx = {
            let mut g = self.seal_rx.lock().unwrap_or_else(|e| e.into_inner());
            g.take()
        };
        let Some(rx) = rx else {
            return Ok(());
        };
        match rx.recv() {
            Ok(Ok(p)) => self.apply_seal_publish_locked(p),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(StoreError::Corrupt(
                "tx.head background seal worker disconnected",
            )),
        }
    }

    #[doc(hidden)]
    pub fn apply_seal_publish_locked(&self, p: SealPublish) -> Result<(), StoreError> {
        let mut guard = self.segments.write().unwrap_or_else(|e| e.into_inner());
        let mut new_list = (**guard).clone();
        let mut found = false;
        for s in &mut new_list {
            if s.file_id != p.file_id {
                continue;
            }
            if s.sealed {
                return Err(StoreError::Corrupt("tx.head seal publish: already sealed"));
            }
            let count = s.count.load(Ordering::Relaxed);
            let first_fk = s.first_fk;
            let file_id = p.file_id;
            *s = Arc::new(Segment {
                first_fk,
                count: AtomicU64::new(count),
                file_id,
                sealed: true,
                tagged: AtomicBool::new(true),
                head: None,
                pack: Some(Arc::new(p.pack)),
                fuse: Some(map_segment_fuse(&self.dir, file_id)?),
            });
            found = true;
            break;
        }
        if !found {
            return Err(StoreError::Corrupt("tx.head seal publish: file_id missing"));
        }
        *guard = Arc::new(new_list);
        drop(guard);
        // Sealed meta must be durable before the OA is unlinked: a crash after
        // unlink with meta still "open" would force a full head rebuild. A
        // leftover OA after persist is discarded on open (Windows also lands
        // here when a lookup still holds the OA map).
        self.persist_meta_locked()?;
        let base = segment_head_path(&self.dir, p.file_id);
        let _ = std::fs::remove_file(&base);
        Ok(())
    }

    #[doc(hidden)]
    pub fn roll_tail_background_locked(&self, collect: SealCollect) -> Result<(), StoreError> {
        self.wait_seal_locked()?;
        let segs = self.segments_snapshot();
        let last = segs
            .last()
            .ok_or(StoreError::Corrupt("tx.head roll empty"))?;
        if last.sealed {
            return Ok(());
        }
        let count = last.count.load(Ordering::Relaxed);
        if count == 0 {
            return Ok(());
        }
        let file_id = last.file_id;
        let first_fk = last.first_fk;
        if let Some(h) = last.head.as_ref() {
            h.flush()?;
        }
        let next_fk = first_fk.saturating_add(count);
        self.open_new_locked(next_fk)?;
        self.persist_meta_locked()?;
        self.spawn_seal(file_id, first_fk, count, collect);
        Ok(())
    }

    #[doc(hidden)]
    pub fn spawn_seal(&self, file_id: u32, first_fk: u64, count: u64, collect: SealCollect) {
        let dir = self.dir.clone();
        let (tx, rx) = mpsc::channel();
        *self.seal_rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(collect_and_seal(&dir, file_id, first_fk, count, collect));
        });
    }

    #[doc(hidden)]
    pub fn seal_file_sync_locked(
        &self,
        file_id: u32,
        pairs: Vec<(u64, u32)>,
    ) -> Result<(), StoreError> {
        self.wait_seal_locked()?;
        let segs = self.segments_snapshot();
        let Some(seg) = segs.iter().find(|s| s.file_id == file_id) else {
            return Err(StoreError::Corrupt("tx.head seal_file: missing"));
        };
        if seg.sealed {
            return Ok(());
        }
        let count = seg.count.load(Ordering::Relaxed);
        if count == 0 {
            return Ok(());
        }
        if pairs.len() as u64 != count {
            return Err(StoreError::Corrupt(
                "tx.head seal open_keys incomplete (reopen mid-segment without rebuild)",
            ));
        }
        if let Some(h) = seg.head.as_ref() {
            h.flush()?;
        }
        let pubd = build_seal_publish(&self.dir, file_id, seg.first_fk, count, pairs)?;
        self.apply_seal_publish_locked(pubd)
    }

    /// Crash reopen: seal every unsealed non-tail (caller collected keys).
    pub fn seal_file_sync(&self, file_id: u32, pairs: Vec<(u64, u32)>) -> Result<(), StoreError> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.seal_file_sync_locked(file_id, pairs)
    }

    /// Unsealed non-tail `(file_id, first_fk, count)` (in-flight seal after crash).
    pub fn unsealed_nontail_ranges(&self) -> Vec<(u32, u64, u64)> {
        let segs = self.segments_snapshot();
        let n = segs.len();
        segs.iter()
            .enumerate()
            .filter(|(i, s)| i + 1 != n && !s.sealed)
            .map(|(_, s)| (s.file_id, s.first_fk, s.count.load(Ordering::Relaxed)))
            .collect()
    }

    /// Unsealed segments `(file_id, first_fk, count)` (insert tail + in-flight seal).
    #[cfg(debug_assertions)]
    pub fn unsealed_ranges(&self) -> Vec<(u32, u64, u64)> {
        self.segments_snapshot()
            .iter()
            .filter(|s| !s.sealed)
            .map(|s| (s.file_id, s.first_fk, s.count.load(Ordering::Relaxed)))
            .collect()
    }

    pub fn write_sealed_pairs(
        &self,
        file_id: u32,
        first_fk: u64,
        count: u64,
        pairs: Vec<(u64, u32)>,
    ) -> Result<SealPublish, StoreError> {
        build_seal_publish(&self.dir, file_id, first_fk, count, pairs)
    }

    /// Replace the segment list with sealed MPHF ranges and an empty open tail.
    pub fn install_rebuild_sealed(
        &self,
        sealed: Vec<(u64, u64, SealPublish)>,
        tail_first_fk: u64,
    ) -> Result<(), StoreError> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.wait_seal_locked()?;
        let mut max_id = 0u32;
        let mut list = Vec::with_capacity(sealed.len().saturating_add(1));
        for (first_fk, count, p) in sealed {
            max_id = max_id.max(p.file_id);
            let fuse = map_segment_fuse(&self.dir, p.file_id)?;
            list.push(Arc::new(Segment {
                first_fk,
                count: AtomicU64::new(count),
                file_id: p.file_id,
                sealed: true,
                tagged: AtomicBool::new(true),
                head: None,
                pack: Some(Arc::new(p.pack)),
                fuse: Some(fuse),
            }));
        }
        {
            let mut guard = self.segments.write().unwrap_or_else(|e| e.into_inner());
            *guard = Arc::new(list);
        }
        self.next_file_id
            .store(max_id.saturating_add(1), Ordering::Relaxed);
        self.open_new_locked(tail_first_fk)?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn persist_meta_locked(&self) -> Result<(), StoreError> {
        let segs = self.segments_snapshot();
        let descs: Vec<(u64, u64, u32, u32)> = segs
            .iter()
            .map(|s| {
                let mut flags = if s.sealed { FLAG_SEALED } else { 0 };
                if s.tagged.load(Ordering::Relaxed) {
                    flags |= FLAG_TAGGED;
                }
                (
                    s.first_fk,
                    s.count.load(Ordering::Relaxed),
                    s.file_id,
                    flags,
                )
            })
            .collect();
        write_meta(&self.dir, self.layout.bits, &descs)
    }
}

impl Drop for SegmentedTxHead {
    fn drop(&mut self) {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.wait_seal_locked();
    }
}

#[doc(hidden)]
pub fn collect_and_seal(
    dir: &Path,
    file_id: u32,
    first_fk: u64,
    count: u64,
    collect: SealCollect,
) -> Result<SealPublish, StoreError> {
    let t0 = Instant::now();
    let pairs = collect(first_fk, count)?;
    rbitcoin_log::info!(
        "store: tx.head seal collect file_id={file_id} count={count} duration_ms={}",
        t0.elapsed().as_millis()
    );
    if pairs.len() as u64 != count {
        return Err(StoreError::Corrupt(
            "tx.head seal collect pair count mismatch",
        ));
    }
    build_seal_publish(dir, file_id, first_fk, count, pairs)
}

#[doc(hidden)]
pub fn build_seal_publish(
    dir: &Path,
    file_id: u32,
    first_fk: u64,
    count: u64,
    mut pairs: Vec<(u64, u32)>,
) -> Result<SealPublish, StoreError> {
    let raw_n = pairs.len();
    let grouped = crate::tx_head_mphf::group_assigned_pairs(&mut pairs)?;
    drop(pairs);
    let unique_n = grouped.keys.len();
    rbitcoin_log::info!(
        "store: tx.head seal begin file_id={file_id} first_fk={first_fk} count={count} \
         fuse_keys_raw={raw_n} fuse_keys_unique={unique_n}"
    );
    let t0 = Instant::now();
    let fuse = SealedFuse8::build(&grouped.keys)?;
    let fuse_bytes = fuse.fingerprint_bytes();
    fuse.write_to(&segment_fuse_path(dir, file_id))?;
    let pack = TxHeadMphf::write_grouped(segment_head_path(dir, file_id), grouped)?;
    rbitcoin_log::info!(
        "store: tx.head seal done file_id={file_id} count={count} fuse_keys_unique={unique_n} \
         fuse_bytes={fuse_bytes} duration_ms={}",
        t0.elapsed().as_millis()
    );
    Ok(SealPublish { file_id, pack })
}

#[inline]
#[doc(hidden)]
pub fn rel_to_abs(first_fk: u64, rel: u64) -> Option<Fk> {
    if rel == 0 {
        return None;
    }
    Some(Fk(first_fk + rel - 1))
}

#[doc(hidden)]
pub fn max_keys_for_layout(layout: HeadLayout) -> u64 {
    let slots = layout.slots();
    ((slots as f64) * HEAD_LOAD_START).floor() as u64
}

/// `store/tx.head/` — segment files + meta live here.
#[inline]
#[doc(hidden)]
pub fn head_root(dir: &Path) -> PathBuf {
    dir.join("tx.head")
}

#[doc(hidden)]
pub fn refuse_legacy_mono_head(dir: &Path) -> Result<(), StoreError> {
    let mono = dir.join("tx.head");
    if mono.is_file() {
        return Err(StoreError::Corrupt(
            "legacy monolithic tx.head present — reindex required (segmented 25-bit heads)",
        ));
    }
    // Directory is the **new** segment home. Reject only non-empty dirs that are
    // not our layout (no `meta`, no pending flat migration).
    if mono.is_dir() {
        let new_meta = mono.join("meta");
        if !new_meta.is_file() && !dir.join("tx.head.meta").is_file() {
            let non_empty = std::fs::read_dir(&mono)
                .map(|rd| rd.filter_map(|e| e.ok()).next().is_some())
                .unwrap_or(false);
            if non_empty {
                return Err(StoreError::Corrupt(
                    "legacy sharded tx.head/ dir — reindex required",
                ));
            }
        }
    }
    for name in [
        "tx.head.new",
        "tx.head.resize",
        "tx.head.bak",
        "tx.head.overflow",
    ] {
        let p = dir.join(name);
        if p.exists() {
            rbitcoin_log::warn!(
                "store: removing obsolete mono-head artifact {}",
                p.display()
            );
            let _ = std::fs::remove_file(&p);
        }
    }
    ensure_head_layout(dir)?;
    Ok(())
}

/// Leftover flat `tx.head.meta` (pre-directory layout).
pub const INDEX_REFUSE_FLAT_HEAD: &str = "index refuses flat tx.head.meta; wipe store/tx.head then restart (Class A kept; tx.head rebuilds)";

/// Leftover **layouts** that must not auto-rebuild (v1 fuse, flat `tx.head.meta`).
/// Torn current-layout files (`bdz mphf:*`, short meta) rebuild from Class A.
pub fn is_index_open_refuse(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Corrupt(m)
            if *m == crate::fuse8_filter::INDEX_REFUSE_FUSE8_V1 || *m == INDEX_REFUSE_FLAT_HEAD
    )
}

/// Ensure `tx.head/` exists. Leftover flat `tx.head.meta` refuses.
#[doc(hidden)]
pub fn ensure_head_layout(dir: &Path) -> Result<(), StoreError> {
    let root = head_root(dir);
    let new_meta = meta_path(dir);
    let flat_meta = dir.join("tx.head.meta");
    if flat_meta.is_file() {
        return Err(StoreError::Corrupt(INDEX_REFUSE_FLAT_HEAD));
    }
    if new_meta.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(&root).map_err(|e| StoreError::io(&root, e))?;
    Ok(())
}

#[doc(hidden)]
pub fn segment_head_path(dir: &Path, file_id: u32) -> PathBuf {
    head_root(dir).join(format!("{file_id:06}"))
}

#[doc(hidden)]
pub fn segment_fuse_path(dir: &Path, file_id: u32) -> PathBuf {
    head_root(dir).join(format!("{file_id:06}.fuse8"))
}

#[doc(hidden)]
pub fn map_segment_fuse(dir: &Path, file_id: u32) -> Result<SealedFuse8, StoreError> {
    SealedFuse8::read_from(&segment_fuse_path(dir, file_id))
}

#[doc(hidden)]
pub fn meta_path(dir: &Path) -> PathBuf {
    head_root(dir).join("meta")
}

/// True when segmented head meta exists (subdir or pre-migration flat).
pub fn head_meta_exists(dir: &Path) -> bool {
    meta_path(dir).is_file() || dir.join("tx.head.meta").is_file()
}

/// Remove all segmented head files (subdir layout + any leftover flat files).
pub fn wipe_segmented_head_files(dir: &Path) {
    let root = head_root(dir);
    if root.is_dir() {
        let _ = std::fs::remove_dir_all(&root);
    }
    let _ = std::fs::remove_file(dir.join("tx.head.meta"));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let s = ent.file_name().to_string_lossy().into_owned();
            if s.starts_with("tx.head.") {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }
}

#[doc(hidden)]
pub fn write_meta(dir: &Path, bits: u32, segs: &[(u64, u64, u32, u32)]) -> Result<(), StoreError> {
    write_meta_versioned(dir, META_VERSION, bits, segs)
}

fn write_meta_versioned(
    dir: &Path,
    version: u32,
    bits: u32,
    segs: &[(u64, u64, u32, u32)],
) -> Result<(), StoreError> {
    let path = meta_path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
    }
    let mut buf = Vec::with_capacity(META_HEADER_LEN + segs.len() * SEG_DESC_LEN);
    buf.extend_from_slice(&STORE_MAGIC);
    buf.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&(segs.len() as u32).to_le_bytes());
    buf.extend_from_slice(&bits.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    for &(first_fk, count, file_id, flags) in segs {
        buf.extend_from_slice(&first_fk.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&file_id.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
    }
    crate::file::write_synced_tmp_rename(&path, &buf)?;
    Ok(())
}

#[doc(hidden)]
pub struct SegDesc {
    pub first_fk: u64,
    pub count: u64,
    pub file_id: u32,
    pub flags: u32,
}

#[doc(hidden)]
pub fn read_meta(dir: &Path) -> Result<(u32, Vec<SegDesc>), StoreError> {
    let path = meta_path(dir);
    if !path.exists() {
        return Ok((SEGMENT_HEAD_BITS, Vec::new()));
    }
    let buf = std::fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
    read_meta_buf(&buf)
}

#[doc(hidden)]
pub fn read_meta_buf(buf: &[u8]) -> Result<(u32, Vec<SegDesc>), StoreError> {
    let Some(head) = buf.first_chunk::<META_HEADER_LEN>() else {
        return Err(StoreError::Corrupt("tx.head.meta short"));
    };
    if head[0..4] != STORE_MAGIC {
        return Err(StoreError::Corrupt("tx.head.meta magic"));
    }
    let meta_ver = le_u32!(head, 8);
    if meta_ver != META_VERSION && meta_ver != META_V1_UNTAGGED {
        return Err(StoreError::Corrupt("tx.head.meta version"));
    }
    let seg_count = le_u32!(head, 12) as usize;
    let bits = le_u32!(head, 16);
    let need = META_HEADER_LEN + seg_count * SEG_DESC_LEN;
    if buf.len() < need {
        return Err(StoreError::Corrupt("tx.head.meta truncated"));
    }
    let (recs, _) = buf[META_HEADER_LEN..need].as_chunks::<SEG_DESC_LEN>();
    let mut descs = Vec::with_capacity(seg_count);
    for rec in recs {
        descs.push(SegDesc {
            first_fk: le_u64!(rec, 0),
            count: le_u64!(rec, 8),
            file_id: le_u32!(rec, 16),
            flags: le_u32!(rec, 20),
        });
    }
    Ok((bits, descs))
}
