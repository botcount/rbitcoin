//! Private mempool durability under `{datadir}/mempool/` (not Class A).
//!
//! # Namespace (important)
//!
//! | Path | Role |
//! |------|------|
//! | `{datadir}/store/tx.body` | **Class A** confirmed archive — confirm commit sole writer |
//! | `{datadir}/mempool/tx.body` | **This file** — unconfirmed live set only |
//!
//! Schema **4** packed live records (`fee‖weight‖sigops‖accept_secs‖…`); schema 1–3
//! recode on open. Body is append-only (`body_persisted_len`);
//! `persist_due` syncs the dirty tail, then syncs new LIVE slots, then writes meta.
//! Compact copies packed payload ranges. DEAD of a durable slot is a one-byte
//! `pwrite`. A batch syncs `slots` once, then `meta`.
//! Open keeps LIVE rows inside the logical body and returns [`MempoolError::Corrupt`]
//! for an in-range payload that does not decode. The hub moves that image aside.
//!
//! # Transport (phase 5b M2)
//!
//! Process-owned buffers (`meta` fields + `slots` / `body` `Vec`s) are the
//! source of truth. Sidecar files are updated with normal `read`/`write` /
//! `pwrite`-style IO — **no `memmap2`**. Flush bumps generation and `sync_data`
//! of the body before slots, then slots before meta.

use crate::error::MempoolError;
use crate::packed::{
    decode_packed_live, decode_packed_live_v2, decode_packed_live_v3, encode_packed_live,
    PackedLive, VinAux, ACCEPT_SECS_OFF, SIGOP_COST_OFF,
};
use bitcoin::consensus::encode::deserialize;
use bitcoin::hashes::Hash;
use bitcoin::{Transaction, Txid, Wtxid};
use rbitcoin_primitives::{bytes, le_u16, le_u32, le_u64};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// File magic: `rBMP` (rbitcoin mempool).
pub const MEM_MAGIC: [u8; 4] = *b"rBMP";
/// Schema version for the meta header.
pub const MEM_SCHEMA: u16 = 4;

#[doc(hidden)]
pub const META_LEN: usize = 64;
/// Initial slot table capacity (records).
///
/// Sized for mainnet tip mempool under a ~300 MvB weight budget: many small
/// txs fit weight-wise long before 4k slots fill (overnight tip stall). Fixed
/// constant — no env. Existing datadirs with smaller caps grow on demand.
#[doc(hidden)]
pub const DEFAULT_SLOT_CAP: u32 = 131_072;
/// Hard ceiling when doubling the slot table (DoS / RAM bound).
#[doc(hidden)]
pub const MAX_SLOT_CAP: u32 = 1_048_576;
/// Slot record: status(1) + trust(1) + pad(2) + body_off(8) + body_len(4) + txid(32) = 48.
///
/// `trust` is 1 when this live tx was warn-and-applied from the trusted
/// session. Schema 4 already had these bytes. Older files leave them 0.
#[doc(hidden)]
pub const SLOT_REC: usize = 48;

/// One slot record. `off` names a record inside a table of
/// `SLOTS_HEADER + cap * SLOT_REC` bytes.
pub(crate) fn slot_rec(slots: &[u8], off: usize) -> [u8; SLOT_REC] {
    let mut rec = [0u8; SLOT_REC];
    rec.copy_from_slice(&slots[off..off + SLOT_REC]);
    rec
}
#[doc(hidden)]
pub const SLOTS_HEADER: usize = 16;
#[doc(hidden)]
pub const BODY_HEADER: usize = 16;
/// Prefix before each packed live record in `mempool/tx.body` (min size).
#[doc(hidden)]
pub const BODY_TX_PREFIX: usize = 88;
/// Schema 1 payload: `fee(8)‖weight(8)‖bitcoin-serialize`.
#[doc(hidden)]
pub const V1_BODY_PREFIX: usize = 16;

#[doc(hidden)]
pub const SLOT_FREE: u8 = 0;
#[doc(hidden)]
pub const SLOT_LIVE: u8 = 1;
#[doc(hidden)]
pub const SLOT_DEAD: u8 = 2;

/// Snapshot of durable meta fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolMeta {
    /// Last committed generation (bumped on flush).
    pub generation: u64,
    /// Slot table capacity (record count).
    pub slot_cap: u32,
    /// Number of LIVE slots.
    pub live_count: u32,
}

/// One LIVE payload from [`Mempool::load_live_txs`].
#[derive(Debug, Clone)]
pub struct LiveTx {
    pub slot: u32,
    pub packed: PackedLive,
    /// Slot trust byte. Set for a warn-and-applied tx from the trusted session.
    pub trust_kept: bool,
}

/// Time-based sidecar persist interval (admits). Crash may lose ≤ this window.
///
/// DEAD of an already-durable slot is an immediate `pwrite` (not this timer).
/// Structural ops (slot grow, compact) and [`Mempool::flush`] persist immediately.
pub const PERSIST_INTERVAL_MS: u64 = 5_000;

/// Durable mempool under `dir` (`{datadir}/mempool`) — InRam buffers + file IO.
pub struct Mempool {
    pub dir: PathBuf,
    pub meta_file: File,
    pub slots_file: File,
    pub body_file: File,
    /// Full slots image (header + records).
    pub slots: Vec<u8>,
    /// Full body image including header; logical length in `body[8..16]`.
    pub body: Vec<u8>,
    pub generation: u64,
    pub slot_cap: u32,
    pub live_count: u32,
    /// True when `tx.body` grew since last persist (admits).
    pub body_dirty: bool,
    /// On-disk body length that slots may legally index (header + durable payloads).
    pub body_persisted_len: u64,
    pub opened: Instant,
    pub mock_now_ms: Option<u64>,
    pub last_persist_ms: u64,
    /// Tests pin the table at the live count so the next admit evicts.
    pub grow_pinned: bool,
    /// Next [`Self::append_live_tx`] returns [`MempoolError::Full`] once.
    #[cfg(debug_assertions)]
    pub fail_next_append: bool,
    /// A durable DEAD `pwrite` is not yet covered by [`Self::sync_deaths`].
    pub deaths_unsynced: bool,
    /// Successful `meta` `sync_data` calls (tests).
    #[cfg(debug_assertions)]
    pub meta_syncs: u64,
    /// Successful `slots` `sync_data` calls from [`Self::sync_deaths`] (tests).
    #[cfg(debug_assertions)]
    pub slots_syncs: u64,
}

impl Mempool {
    /// Create `dir` if needed and open (or initialize) meta/slots/body into RAM.
    pub fn open_or_create(dir: impl Into<PathBuf>) -> Result<Self, MempoolError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| MempoolError::io(&dir, e))?;
        finish_pending_compact(&dir)?;

        let meta_path = dir.join("meta");
        let slots_path = dir.join("slots");
        let body_path = dir.join("tx.body");

        let (meta_file, generation, slot_cap, live_count, meta_schema) =
            open_or_init_meta(&meta_path)?;
        let (slots_file, slots) = open_or_init_slots(&slots_path, slot_cap)?;
        let (body_file, body) = open_or_init_body(&body_path)?;
        let body_persisted_len = body_logical_len(&body)? as u64;

        let mut mp = Self {
            dir,
            meta_file,
            slots_file,
            body_file,
            slots,
            body,
            generation,
            slot_cap,
            live_count,
            body_dirty: false,
            body_persisted_len,
            opened: Instant::now(),
            mock_now_ms: None,
            last_persist_ms: 0,
            grow_pinned: false,
            #[cfg(debug_assertions)]
            fail_next_append: false,
            deaths_unsynced: false,
            #[cfg(debug_assertions)]
            meta_syncs: 0,
            #[cfg(debug_assertions)]
            slots_syncs: 0,
        };
        let mut body_hdr = [0u8; BODY_HEADER];
        body_hdr.copy_from_slice(&mp.body[..BODY_HEADER]);
        let body_schema = le_u16!(body_hdr, 4);
        let mut slots_hdr = [0u8; SLOTS_HEADER];
        slots_hdr.copy_from_slice(&mp.slots[..SLOTS_HEADER]);
        let slots_schema = le_u16!(slots_hdr, 4);
        if body_schema != MEM_SCHEMA {
            mp.migrate_to_current(body_schema)?;
        } else if meta_schema != MEM_SCHEMA || slots_schema != MEM_SCHEMA {
            // Crash after the migrate renamed body+slots, before meta.
            mp.slots[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
            mp.body[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
            mp.persist_slots_and_meta()?;
        }
        // Validate before deleting abandoned compact tmps so a corrupt image
        // can still be moved aside with those tmps.
        mp.load_live_txs()?;
        mp.drop_slots_past_body()?;
        discard_abandoned_tmps(&mp.dir)?;
        Ok(mp)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn meta(&self) -> MempoolMeta {
        MempoolMeta {
            generation: self.generation,
            slot_cap: self.slot_cap,
            live_count: self.live_count,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn live_count(&self) -> u32 {
        self.live_count
    }

    /// Test / rebuild helper: set live_count without scanning.
    pub fn set_live_count(&mut self, n: u32) {
        self.live_count = n;
    }

    /// Persist buffers, bump generation, and fsync sidecar files.
    ///
    /// Admits persist on [`Self::persist_due`] (5 s, body `sync_data` before
    /// LIVE slots). A crash may lose admits since the last persist.
    /// [`Self::flush`] is the durable checkpoint (generation + the same order).
    pub fn flush(&mut self) -> Result<(), MempoolError> {
        self.sync_deaths()?;
        self.generation = self.generation.saturating_add(1);
        self.persist_body_then_slots()?;
        Ok(())
    }

    /// Time-based body persist: dirty admits wait [`PERSIST_INTERVAL_MS`].
    ///
    /// Body tail is `sync_data`'d, then new LIVE slot records are written and
    /// synced, then meta. One `fdatasync` of the dirty body per dirty interval.
    /// DEAD of durable slots is a one-byte `pwrite` in [`Self::mark_slot_dead`];
    /// [`Self::sync_deaths`] flushes those bytes once. Flush / grow / compact
    /// still rewrite the full slot table.
    pub fn persist_due(&mut self) -> Result<(), MempoolError> {
        if self.deaths_unsynced {
            self.sync_deaths()?;
        }
        if !self.body_dirty {
            return Ok(());
        }
        if self.now_ms().saturating_sub(self.last_persist_ms) < PERSIST_INTERVAL_MS {
            return Ok(());
        }
        let old_persisted = self.body_persisted_len;
        self.persist_body_tail()?;
        self.pwrite_live_slots_since(old_persisted)?;
        self.persist_meta()?;
        self.clear_dirty();
        self.last_persist_ms = self.now_ms();
        Ok(())
    }

    /// Best-effort alias of [`Self::persist_due`] (no generation bump).
    pub fn persist_if_dirty(&mut self) -> Result<(), MempoolError> {
        self.persist_due()
    }

    #[doc(hidden)]
    pub fn now_ms(&self) -> u64 {
        self.mock_now_ms
            .unwrap_or_else(|| self.opened.elapsed().as_millis() as u64)
    }

    /// Test clock. Production uses [`Instant`] elapsed from open.
    pub fn set_now_ms(&mut self, ms: u64) {
        self.mock_now_ms = Some(ms);
    }

    pub fn body_persisted_len(&self) -> u64 {
        self.body_persisted_len
    }

    #[doc(hidden)]
    pub fn clear_dirty(&mut self) {
        self.body_dirty = false;
    }

    #[doc(hidden)]
    pub fn persist_body_then_slots(&mut self) -> Result<(), MempoolError> {
        self.persist_body_tail()?;
        self.persist_slots_and_meta()?;
        self.clear_dirty();
        self.last_persist_ms = self.now_ms();
        Ok(())
    }

    /// Two slot records share one `pwrite` when the next record starts where the last ended.
    #[doc(hidden)]
    pub fn adjacent_slot_records(prev_end: usize, off: usize) -> bool {
        prev_end == off
    }

    /// `pwrite` LIVE rows whose payload starts at or past `old_persisted`.
    ///
    /// Adjacent new slots are one write. Does not rewrite the rest of the table.
    #[doc(hidden)]
    pub fn pwrite_live_slots_since(&mut self, old_persisted: u64) -> Result<(), MempoolError> {
        let path = self.dir.join("slots");
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            if body_off < old_persisted {
                continue;
            }
            let rec_end = off + SLOT_REC;
            if let Some((_, end)) = ranges.last_mut() {
                if Self::adjacent_slot_records(*end, off) {
                    *end = rec_end;
                    continue;
                }
            }
            ranges.push((off, rec_end));
        }
        for (start, end) in ranges {
            self.slots_file
                .seek(SeekFrom::Start(start as u64))
                .map_err(|e| MempoolError::io(&path, e))?;
            self.slots_file
                .write_all(&self.slots[start..end])
                .map_err(|e| MempoolError::io(&path, e))?;
        }
        self.slots_file
            .sync_data()
            .map_err(|e| MempoolError::io(&path, e))?;
        Ok(())
    }

    /// Append a packed live record; mark a FREE slot LIVE.
    ///
    /// Returns the slot index. RAM is updated immediately; sidecar write waits
    /// for [`Self::persist_due`].
    #[allow(clippy::too_many_arguments)] // one arg per packed-record field
    pub fn append_live_tx(
        &mut self,
        tx: &Transaction,
        txid: &Txid,
        wtxid: &Wtxid,
        fee_sat: u64,
        weight: u64,
        sigop_cost: u64,
        accept_secs: u64,
        vins: &[VinAux],
    ) -> Result<u32, MempoolError> {
        let payload = encode_packed_live(
            tx,
            txid,
            wtxid,
            fee_sat,
            weight,
            Some(sigop_cost),
            accept_secs,
            vins,
        )?;
        #[cfg(debug_assertions)]
        if self.fail_next_append {
            self.fail_next_append = false;
            return Err(MempoolError::Full);
        }
        if u32::try_from(payload.len()).is_err() {
            return Err(MempoolError::Corrupt("tx body too large"));
        }
        let payload_len = payload.len();
        let body_off = self.reserve_body(payload_len)?;
        let off = body_off as usize;
        self.body[off..off + payload_len].copy_from_slice(&payload);
        let slot = self.alloc_slot()?;
        self.write_slot(slot, SLOT_LIVE, body_off, payload_len as u32, txid)?;
        self.live_count = self.live_count.saturating_add(1);
        self.body_dirty = true;
        Ok(slot)
    }

    /// Drop every LIVE slot (Core `-persistmempool=0` start: do not reload).
    pub fn abandon_live(&mut self) -> Result<u32, MempoolError> {
        let mut n = 0u32;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] == SLOT_LIVE {
                self.slots[off] = SLOT_DEAD;
                n = n.saturating_add(1);
            }
        }
        if n > 0 {
            self.live_count = 0;
            self.flush()?;
        }
        Ok(n)
    }

    /// Mark slot DEAD and decrement live_count (confirm / RBF / eviction).
    ///
    /// If the slot is already on disk, `pwrite` that one status byte and leave
    /// the sync to [`Self::sync_deaths`]. An admit that was never durable stays
    /// RAM-only. This path must not dump the full slot table (LIVE rows whose
    /// body is still in the unpersisted tail).
    pub fn mark_slot_dead(&mut self, slot: u32) -> Result<(), MempoolError> {
        if slot >= self.slot_cap {
            return Err(MempoolError::Corrupt("slot OOB"));
        }
        let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
        if self.slots[off] != SLOT_LIVE {
            return Ok(());
        }
        let rec = slot_rec(&self.slots, off);
        let body_off = le_u64!(rec, 4);
        let body_len = le_u32!(rec, 12) as u64;
        self.slots[off] = SLOT_DEAD;
        self.live_count = self.live_count.saturating_sub(1);
        if body_off.saturating_add(body_len) <= self.body_persisted_len {
            self.pwrite_slot_status(slot, SLOT_DEAD)?;
            self.deaths_unsynced = true;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn pwrite_slot_status(&mut self, slot: u32, status: u8) -> Result<(), MempoolError> {
        let path = self.dir.join("slots");
        let off = (SLOTS_HEADER + (slot as usize) * SLOT_REC) as u64;
        self.slots_file
            .seek(SeekFrom::Start(off))
            .map_err(|e| MempoolError::io(&path, e))?;
        self.slots_file
            .write_all(&[status])
            .map_err(|e| MempoolError::io(&path, e))?;
        Ok(())
    }

    /// Count FREE / LIVE / DEAD slots (for compaction triggers).
    pub fn slot_stats(&self) -> (u32, u32, u32) {
        let mut free = 0u32;
        let mut live = 0u32;
        let mut dead = 0u32;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            match self.slots[off] {
                SLOT_LIVE => live += 1,
                SLOT_DEAD => dead += 1,
                _ => free += 1,
            }
        }
        (free, live, dead)
    }

    /// Logical body length in bytes (includes header).
    pub fn body_logical_len(&self) -> Result<usize, MempoolError> {
        body_logical_len(&self.body)
    }

    /// Rewrite body/slots to contain only LIVE payloads packed from the header.
    ///
    /// Copies existing packed payload ranges (does not re-serialize). Returns
    /// `(live_after, body_bytes_after)`. Callers must rebuild RAM indexes with
    /// the new slot numbers from `load_live_txs`.
    pub fn compact(&mut self) -> Result<(u32, usize), MempoolError> {
        let logical = body_logical_len(&self.body)?;
        let mut new_body = vec![0u8; BODY_HEADER];
        new_body[0..4].copy_from_slice(&MEM_MAGIC);
        new_body[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        let mut new_slots = vec![0u8; SLOTS_HEADER + (self.slot_cap as usize) * SLOT_REC];
        new_slots[0..4].copy_from_slice(&MEM_MAGIC);
        new_slots[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        new_slots[8..12].copy_from_slice(&self.slot_cap.to_le_bytes());

        let mut next_slot = 0u32;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            let body_len = le_u32!(rec, 12) as usize;
            if body_off as usize + body_len > logical || body_len < BODY_TX_PREFIX {
                return Err(MempoolError::Corrupt("live slot body range"));
            }
            let start = body_off as usize;
            let new_off = new_body.len() as u64;
            new_body.extend_from_slice(&self.body[start..start + body_len]);
            let dst = SLOTS_HEADER + (next_slot as usize) * SLOT_REC;
            new_slots[dst] = SLOT_LIVE;
            new_slots[dst + 1..dst + 4].copy_from_slice(&self.slots[off + 1..off + 4]);
            new_slots[dst + 4..dst + 12].copy_from_slice(&new_off.to_le_bytes());
            new_slots[dst + 12..dst + 16].copy_from_slice(&(body_len as u32).to_le_bytes());
            new_slots[dst + 16..dst + 48].copy_from_slice(&self.slots[off + 16..off + 48]);
            next_slot += 1;
        }
        let packed_len = new_body.len();
        new_body[8..16].copy_from_slice(&(packed_len as u64).to_le_bytes());

        self.body = new_body;
        self.slots = new_slots;
        self.live_count = next_slot;
        self.install_packed_images()?;
        self.clear_dirty();
        Ok((self.live_count, packed_len))
    }

    /// Recode leftover schema-1 (`fee‖weight‖raw_tx`), schema-2 or schema-3
    /// packed LIVE slots into schema 4.
    ///
    /// Same install as compact (tmp+rename). Older records carry no accept
    /// time, so each gets the migrate's wall-clock second: expiry counts from
    /// this open. Schema ≤ 2 sigop cost is written unknown;
    /// [`crate::ActiveMempool::recompute_missing_sigops`] fills it after open.
    /// Schema-1 vin aux is empty (SH reindex batch-fills). Other schemas refuse
    /// at header read ([`accepted_schema`]).
    #[doc(hidden)]
    pub fn migrate_to_current(&mut self, from: u16) -> Result<(), MempoolError> {
        let accept_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let logical = body_logical_len(&self.body)?;
        let mut new_body = vec![0u8; BODY_HEADER];
        new_body[0..4].copy_from_slice(&MEM_MAGIC);
        new_body[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        let mut new_slots = vec![0u8; SLOTS_HEADER + (self.slot_cap as usize) * SLOT_REC];
        new_slots[0..4].copy_from_slice(&MEM_MAGIC);
        new_slots[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        new_slots[8..12].copy_from_slice(&self.slot_cap.to_le_bytes());

        let mut next_slot = 0u32;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            let body_len = le_u32!(rec, 12) as usize;
            if body_off as usize + body_len > logical || body_len < V1_BODY_PREFIX {
                return Err(MempoolError::Corrupt("legacy live slot body range"));
            }
            let rec = &self.body[body_off as usize..body_off as usize + body_len];
            let old = if from == 1 {
                let tx: Transaction = deserialize(&rec[V1_BODY_PREFIX..])
                    .map_err(|_| MempoolError::Corrupt("v1 tx deserialize"))?;
                let mut head = [0u8; V1_BODY_PREFIX];
                head.copy_from_slice(&rec[..V1_BODY_PREFIX]);
                PackedLive {
                    fee_sat: le_u64!(head, 0),
                    weight: le_u64!(head, 8),
                    sigop_cost: None,
                    accept_secs: 0,
                    txid: tx.compute_txid(),
                    wtxid: tx.compute_wtxid(),
                    tx,
                    vins: Vec::new(),
                }
            } else if from == 2 {
                decode_packed_live_v2(rec)?
            } else {
                decode_packed_live_v3(rec)?
            };
            let payload = encode_packed_live(
                &old.tx,
                &old.txid,
                &old.wtxid,
                old.fee_sat,
                old.weight,
                old.sigop_cost,
                accept_secs,
                &old.vins,
            )?;
            let new_off = new_body.len() as u64;
            let plen = payload.len() as u32;
            new_body.extend_from_slice(&payload);
            let dst = SLOTS_HEADER + (next_slot as usize) * SLOT_REC;
            new_slots[dst] = SLOT_LIVE;
            new_slots[dst + 4..dst + 12].copy_from_slice(&new_off.to_le_bytes());
            new_slots[dst + 12..dst + 16].copy_from_slice(&plen.to_le_bytes());
            new_slots[dst + 16..dst + 48].copy_from_slice(old.txid.as_byte_array());
            next_slot += 1;
        }
        let packed_len = new_body.len();
        new_body[8..16].copy_from_slice(&(packed_len as u64).to_le_bytes());
        self.body = new_body;
        self.slots = new_slots;
        self.live_count = next_slot;
        self.install_packed_images()?;
        self.clear_dirty();
        rbitcoin_log::info!(
            "mempool: converted schema {from} sidecar to schema {MEM_SCHEMA} (live={next_slot})"
        );
        Ok(())
    }

    /// Fill a LIVE record's sigop cost (post-migrate recompute).
    ///
    /// RAM image plus an 8-byte `pwrite` (no fsync) so the next open does not
    /// recompute it. The field may straddle a sector, so a power-loss tear can
    /// mix new and old 0xFF bytes; decode treats any cost above the block limit
    /// as unknown, so a torn value is recomputed. A record still in the
    /// unpersisted tail is rewritten from RAM by the next tail persist anyway.
    pub fn set_sigop_cost(&mut self, slot: u32, cost: u64) -> Result<(), MempoolError> {
        if slot >= self.slot_cap {
            return Err(MempoolError::Corrupt("slot OOB"));
        }
        let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
        if self.slots[off] != SLOT_LIVE {
            return Err(MempoolError::Corrupt("sigops on non-live slot"));
        }
        let rec = slot_rec(&self.slots, off);
        let body_off = le_u64!(rec, 4);
        let at = body_off as usize + SIGOP_COST_OFF;
        self.body[at..at + 8].copy_from_slice(&cost.to_le_bytes());
        let path = self.dir.join("tx.body");
        self.body_file
            .seek(SeekFrom::Start(at as u64))
            .map_err(|e| MempoolError::io(&path, e))?;
        self.body_file
            .write_all(&cost.to_le_bytes())
            .map_err(|e| MempoolError::io(&path, e))?;
        Ok(())
    }

    /// `accept_secs` of a LIVE slot's record, read from RAM.
    pub fn live_accept_secs(&self, slot: u32) -> Option<u64> {
        if slot >= self.slot_cap {
            return None;
        }
        let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
        if self.slots[off] != SLOT_LIVE {
            return None;
        }
        let rec = slot_rec(&self.slots, off);
        let body_off = le_u64!(rec, 4);
        let at = body_off as usize + ACCEPT_SECS_OFF;
        let bytes = self.body.get(at..at + 8)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(bytes);
        Some(le_u64!(word, 0))
    }

    /// Load LIVE txs whose packed bytes sit inside the logical body.
    ///
    /// A slot past that length is the issue 859 tear (slots reached disk, the
    /// body tail did not). Those rows are skipped. An in-range record that does
    /// not decode, or whose txid does not match the slot, fails the whole open.
    pub fn load_live_txs(&self) -> Result<Vec<LiveTx>, MempoolError> {
        let mut out = Vec::new();
        let logical = body_logical_len(&self.body)? as u64;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            let body_len = le_u32!(rec, 12) as u64;
            if live_slot_past_body(body_off, body_len, logical) {
                continue;
            }
            if (body_len as usize) < BODY_TX_PREFIX {
                return Err(MempoolError::Corrupt("live slot body range"));
            }
            let start = body_off as usize;
            let packed = decode_packed_live(&self.body[start..start + body_len as usize])?;
            let slot_txid = Txid::from_byte_array(bytes!(rec, 16, 32));
            if packed.txid != slot_txid {
                return Err(MempoolError::Corrupt("live slot txid"));
            }
            out.push(LiveTx {
                slot,
                packed,
                trust_kept: self.slots[off + 1] != 0,
            });
        }
        Ok(out)
    }

    /// Mark LIVE rows past the durable body FREE and publish that slot table.
    ///
    /// [`Self::load_live_txs`] already skips them. Clearing the bytes keeps a
    /// later compact from refusing the same tear.
    #[doc(hidden)]
    pub fn drop_slots_past_body(&mut self) -> Result<(), MempoolError> {
        let logical = self.body_persisted_len;
        let mut changed = false;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            let body_len = le_u32!(rec, 12) as u64;
            if !live_slot_past_body(body_off, body_len, logical) {
                continue;
            }
            self.slots[off] = SLOT_FREE;
            self.live_count = self.live_count.saturating_sub(1);
            changed = true;
        }
        if changed {
            self.persist_slots_and_meta()?;
        }
        Ok(())
    }

    /// True if at least one FREE or DEAD slot can be reused.
    pub fn has_free_slot(&self) -> bool {
        self.find_free_slot().is_some()
    }

    #[doc(hidden)]
    pub fn find_free_slot(&self) -> Option<u32> {
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] == SLOT_FREE || self.slots[off] == SLOT_DEAD {
                return Some(slot);
            }
        }
        None
    }

    #[doc(hidden)]
    pub fn alloc_slot(&mut self) -> Result<u32, MempoolError> {
        if let Some(s) = self.find_free_slot() {
            return Ok(s);
        }
        // Grow once so a full live set under a small legacy cap is not a hard fail.
        self.grow_slots()?;
        self.find_free_slot().ok_or(MempoolError::Full)
    }

    /// Pretend the slot table is at its cap and already full of LIVE rows.
    /// The next admit takes the slot-eviction path instead of growing.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn testing_pin_full_slot_table(&mut self) {
        self.slot_cap = self.live_count.max(1);
        self.grow_pinned = true;
    }

    /// Tests set the logical slot cap. The slots image must already cover it.
    #[cfg(debug_assertions)]
    pub fn testing_set_slot_cap(&mut self, cap: u32) {
        let need = SLOTS_HEADER + (cap as usize) * SLOT_REC;
        assert!(
            cap >= 1 && self.slots.len() >= need,
            "slot image is shorter than cap {cap}"
        );
        self.slots[8..12].copy_from_slice(&cap.to_le_bytes());
        self.slot_cap = cap;
    }

    /// Double slot capacity (up to [`MAX_SLOT_CAP`]) and extend the slots image with FREE records.
    pub fn grow_slots(&mut self) -> Result<(), MempoolError> {
        if self.grow_pinned || self.slot_cap >= MAX_SLOT_CAP {
            return Err(MempoolError::Full);
        }
        let new_cap = self
            .slot_cap
            .saturating_mul(2)
            .max(self.slot_cap.saturating_add(DEFAULT_SLOT_CAP.min(16_384)))
            .min(MAX_SLOT_CAP);
        if new_cap <= self.slot_cap {
            return Err(MempoolError::Full);
        }
        let old_cap = self.slot_cap;
        let need = SLOTS_HEADER + (new_cap as usize) * SLOT_REC;
        self.slots.resize(need, 0);
        self.slots[8..12].copy_from_slice(&new_cap.to_le_bytes());
        self.slot_cap = new_cap;
        self.persist_body_then_slots()?;
        rbitcoin_log::info!(
            "mempool: grew slot table {old_cap} → {new_cap} (live={})",
            self.live_count
        );
        Ok(())
    }

    #[doc(hidden)]
    pub fn write_slot(
        &mut self,
        slot: u32,
        status: u8,
        body_off: u64,
        body_len: u32,
        txid: &Txid,
    ) -> Result<(), MempoolError> {
        let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
        self.slots[off] = status;
        self.slots[off + 1..off + 4].fill(0);
        self.slots[off + 4..off + 12].copy_from_slice(&body_off.to_le_bytes());
        self.slots[off + 12..off + 16].copy_from_slice(&body_len.to_le_bytes());
        self.slots[off + 16..off + 48].copy_from_slice(txid.as_byte_array());
        Ok(())
    }

    /// Mark a LIVE slot as warn-and-applied from the trusted session.
    ///
    /// The byte sits in the schema-4 pad. Compact copies it. A dead slot is
    /// not reloaded, so clearing it is unnecessary.
    pub fn set_slot_trust_kept(&mut self, slot: u32, on: bool) {
        let off = SLOTS_HEADER + (slot as usize) * SLOT_REC + 1;
        self.slots[off] = u8::from(on);
    }

    /// Ensure body has `need` free bytes; return offset of free region. Updates logical len.
    #[doc(hidden)]
    pub fn reserve_body(&mut self, need: usize) -> Result<u64, MempoolError> {
        let logical = body_logical_len(&self.body)?;
        let end = logical.saturating_add(need);
        if end > self.body.len() {
            self.body.resize(end, 0);
        }
        self.body[8..16].copy_from_slice(&(end as u64).to_le_bytes());
        Ok(logical as u64)
    }

    /// Write the dirty body tail and `sync_data` it before any LIVE slot names it.
    ///
    /// Append-safe: a crash after a grown body and before new slots loses admits;
    /// old LIVE ranges stay a prefix of the new body. Packed compact must not
    /// use this order (see [`Self::install_packed_images`]).
    #[doc(hidden)]
    pub fn persist_body_tail(&mut self) -> Result<(), MempoolError> {
        let body_path = self.dir.join("tx.body");
        let logical = body_logical_len(&self.body)? as u64;
        let start = self.body_persisted_len.min(logical);
        if logical != start {
            self.body_file
                .seek(SeekFrom::Start(8))
                .map_err(|e| MempoolError::io(&body_path, e))?;
            self.body_file
                .write_all(&self.body[8..16])
                .map_err(|e| MempoolError::io(&body_path, e))?;
        }
        self.body_file
            .seek(SeekFrom::Start(start))
            .map_err(|e| MempoolError::io(&body_path, e))?;
        self.body_file
            .write_all(&self.body[start as usize..logical as usize])
            .map_err(|e| MempoolError::io(&body_path, e))?;
        self.body_file
            .set_len(logical)
            .map_err(|e| MempoolError::io(&body_path, e))?;
        self.body_file
            .sync_data()
            .map_err(|e| MempoolError::io(&body_path, e))?;
        self.body_persisted_len = logical;
        Ok(())
    }

    /// Slots + live_count. LIVE rows whose body is not yet durable are written
    /// as FREE so a slots-ahead-of-body crash cannot load offsets past EOF.
    #[doc(hidden)]
    pub fn persist_slots_and_meta(&mut self) -> Result<(), MempoolError> {
        let slots_path = self.dir.join("slots");
        if let Some(disk) = self.demoted_slots_image() {
            self.write_slots_bytes(&slots_path, &disk)?;
        } else {
            let buf = std::mem::take(&mut self.slots);
            let w = Self::write_slots_file(&mut self.slots_file, &slots_path, &buf);
            self.slots = buf;
            w?;
        }
        self.persist_meta()
    }

    #[doc(hidden)]
    pub fn write_slots_bytes(&mut self, path: &Path, bytes: &[u8]) -> Result<(), MempoolError> {
        Self::write_slots_file(&mut self.slots_file, path, bytes)
    }

    #[doc(hidden)]
    pub fn write_slots_file(
        file: &mut File,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), MempoolError> {
        file.set_len(bytes.len() as u64)
            .map_err(|e| MempoolError::io(path, e))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| MempoolError::io(path, e))?;
        file.write_all(bytes)
            .map_err(|e| MempoolError::io(path, e))?;
        file.sync_data().map_err(|e| MempoolError::io(path, e))?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn demoted_slots_image(&self) -> Option<Vec<u8>> {
        let mut disk = None;
        for slot in 0..self.slot_cap {
            let off = SLOTS_HEADER + (slot as usize) * SLOT_REC;
            if self.slots[off] != SLOT_LIVE {
                continue;
            }
            let rec = slot_rec(&self.slots, off);
            let body_off = le_u64!(rec, 4);
            let body_len = le_u32!(rec, 12) as u64;
            if body_off.saturating_add(body_len) > self.body_persisted_len {
                let img = disk.get_or_insert_with(|| self.slots.clone());
                img[off] = SLOT_FREE;
            }
        }
        disk
    }

    /// Test pin: write slots without a prior body tail. Must not introduce
    /// LIVE offsets ≥ [`Self::body_persisted_len`].
    pub fn persist_slots_without_body(&mut self) -> Result<(), MempoolError> {
        self.persist_slots_and_meta()
    }

    #[doc(hidden)]
    pub fn persist_meta(&mut self) -> Result<(), MempoolError> {
        let meta_path = self.dir.join("meta");
        let mut meta = [0u8; META_LEN];
        write_meta_bytes(&mut meta, self.generation, self.slot_cap, self.live_count);
        self.meta_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| MempoolError::io(&meta_path, e))?;
        self.meta_file
            .write_all(&meta)
            .map_err(|e| MempoolError::io(&meta_path, e))?;
        self.meta_file
            .sync_data()
            .map_err(|e| MempoolError::io(&meta_path, e))?;
        #[cfg(debug_assertions)]
        {
            self.meta_syncs = self.meta_syncs.saturating_add(1);
        }
        Ok(())
    }

    /// How many times `meta` has been `sync_data`'d since open.
    #[cfg(debug_assertions)]
    pub fn meta_syncs(&self) -> u64 {
        self.meta_syncs
    }

    /// How many times [`Self::sync_deaths`] has `sync_data`'d `slots`.
    #[cfg(debug_assertions)]
    pub fn slots_syncs(&self) -> u64 {
        self.slots_syncs
    }

    /// `fdatasync` DEAD `pwrite`s, then publish `live_count`.
    ///
    /// Slot status is what open reloads. `meta` is written after that sync so a
    /// crash between them reloads the DEAD rows and reconciles the count.
    /// `deaths_unsynced` stays set until both syncs return, so a failed call
    /// retries from [`Self::persist_due`] or [`Self::flush`]. Does not rewrite
    /// the slot table (unpersisted LIVE rows stay off disk).
    pub fn sync_deaths(&mut self) -> Result<(), MempoolError> {
        if !self.deaths_unsynced {
            return Ok(());
        }
        let path = self.dir.join("slots");
        self.slots_file
            .sync_data()
            .map_err(|e| MempoolError::io(&path, e))?;
        #[cfg(debug_assertions)]
        {
            self.slots_syncs = self.slots_syncs.saturating_add(1);
        }
        self.persist_meta()?;
        self.deaths_unsynced = false;
        Ok(())
    }

    /// Packed body+slots: both tmps `sync_all`'d, then rename body then slots.
    ///
    /// After rename, persist meta only. Rewriting packed body/slots in place
    /// (`set_len` / `write_all`) reopens the crash window tmp+rename closed.
    /// Crash after both renames, before meta: packed images + stale `live_count`
    /// still load (`load_live_txs` scans slots).
    #[doc(hidden)]
    pub fn install_packed_images(&mut self) -> Result<(), MempoolError> {
        let body_path = self.dir.join("tx.body");
        let slots_path = self.dir.join("slots");
        let body_tmp = self.dir.join("tx.body.tmp");
        let slots_tmp = self.dir.join("slots.tmp");
        let logical = body_logical_len(&self.body)?;
        write_file_synced(&body_tmp, &self.body[..logical])?;
        write_file_synced(&slots_tmp, &self.slots)?;
        fs::rename(&body_tmp, &body_path).map_err(|e| MempoolError::io(&body_path, e))?;
        fs::rename(&slots_tmp, &slots_path).map_err(|e| MempoolError::io(&slots_path, e))?;
        self.reopen_body_slots()?;
        self.body_persisted_len = logical as u64;
        self.last_persist_ms = self.now_ms();
        self.persist_meta()
    }

    #[doc(hidden)]
    pub fn reopen_body_slots(&mut self) -> Result<(), MempoolError> {
        let body_path = self.dir.join("tx.body");
        let slots_path = self.dir.join("slots");
        self.body_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&body_path)
            .map_err(|e| MempoolError::io(&body_path, e))?;
        self.slots_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slots_path)
            .map_err(|e| MempoolError::io(&slots_path, e))?;
        Ok(())
    }
}

#[doc(hidden)]
pub fn body_logical_len(body: &[u8]) -> Result<usize, MempoolError> {
    if body.len() < BODY_HEADER {
        return Err(MempoolError::Corrupt("body too short"));
    }
    let mut hdr = [0u8; BODY_HEADER];
    hdr.copy_from_slice(&body[..BODY_HEADER]);
    let n = le_u64!(hdr, 8) as usize;
    if n < BODY_HEADER || n > body.len() {
        return Err(MempoolError::Corrupt("body logical len"));
    }
    Ok(n)
}

#[doc(hidden)]
pub fn write_file_synced(path: &Path, bytes: &[u8]) -> Result<(), MempoolError> {
    let mut f = File::create(path).map_err(|e| MempoolError::io(path, e))?;
    f.write_all(bytes).map_err(|e| MempoolError::io(path, e))?;
    f.sync_all().map_err(|e| MempoolError::io(path, e))?;
    Ok(())
}

/// Finish a compact install interrupted after the body rename.
///
/// `slots.tmp` only: body rename landed — finish slots before the image is
/// read. Both tmps, or a body tmp alone: leave them. A successful open deletes
/// them ([`discard_abandoned_tmps`]). An unreadable image is moved aside with
/// them still present.
#[doc(hidden)]
pub fn finish_pending_compact(dir: &Path) -> Result<(), MempoolError> {
    let body_tmp = dir.join("tx.body.tmp");
    let slots_tmp = dir.join("slots.tmp");
    if !body_tmp.exists() && slots_tmp.exists() {
        fs::rename(&slots_tmp, dir.join("slots")).map_err(|e| MempoolError::io(dir, e))?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn discard_abandoned_tmps(dir: &Path) -> Result<(), MempoolError> {
    for name in ["tx.body.tmp", "slots.tmp"] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| MempoolError::io(&path, e))?;
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn live_slot_past_body(body_off: u64, body_len: u64, logical: u64) -> bool {
    body_off.saturating_add(body_len) > logical
}

/// Move an unreadable mempool sidecar aside. Fee history stays in `dir`.
pub fn quarantine_sidecar(dir: &Path) -> Result<PathBuf, MempoolError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut dest = dir.join(format!("torn-{secs}"));
    let mut n = 0u32;
    while dest.exists() {
        n = n.saturating_add(1);
        dest = dir.join(format!("torn-{secs}-{n}"));
    }
    fs::create_dir(&dest).map_err(|e| MempoolError::io(&dest, e))?;
    for name in ["meta", "slots", "tx.body", "slots.tmp", "tx.body.tmp"] {
        let src = dir.join(name);
        if src.exists() {
            fs::rename(&src, dest.join(name)).map_err(|e| MempoolError::io(&src, e))?;
        }
    }
    Ok(dest)
}

#[doc(hidden)]
pub fn accepted_schema(schema: u16) -> Result<u16, MempoolError> {
    if matches!(schema, 1..=3) || schema == MEM_SCHEMA {
        Ok(schema)
    } else {
        Err(MempoolError::BadSchema(schema))
    }
}

#[doc(hidden)]
pub fn open_or_init_meta(path: &Path) -> Result<(File, u64, u32, u32, u16), MempoolError> {
    if path.exists() {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let mut buf = [0u8; META_LEN];
        file.read_exact(&mut buf)
            .map_err(|e| MempoolError::io(path, e))?;
        if buf[0..4] != MEM_MAGIC {
            return Err(MempoolError::BadMagic);
        }
        let schema = accepted_schema(u16::from_le_bytes([buf[4], buf[5]]))?;
        let generation = le_u64!(buf, 8);
        let slot_cap = le_u32!(buf, 16);
        let live_count = le_u32!(buf, 20);
        if slot_cap == 0 {
            return Err(MempoolError::Corrupt("slot_cap zero"));
        }
        Ok((file, generation, slot_cap, live_count, schema))
    } else {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let mut buf = [0u8; META_LEN];
        write_meta_bytes(&mut buf, 0, DEFAULT_SLOT_CAP, 0);
        file.write_all(&buf)
            .map_err(|e| MempoolError::io(path, e))?;
        file.flush().map_err(|e| MempoolError::io(path, e))?;
        Ok((file, 0, DEFAULT_SLOT_CAP, 0, MEM_SCHEMA))
    }
}

#[doc(hidden)]
pub fn write_meta_bytes(buf: &mut [u8; META_LEN], generation: u64, slot_cap: u32, live_count: u32) {
    buf[0..4].copy_from_slice(&MEM_MAGIC);
    buf[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
    buf[6..8].copy_from_slice(&0u16.to_le_bytes());
    buf[8..16].copy_from_slice(&generation.to_le_bytes());
    buf[16..20].copy_from_slice(&slot_cap.to_le_bytes());
    buf[20..24].copy_from_slice(&live_count.to_le_bytes());
}

#[doc(hidden)]
pub fn open_or_init_slots(path: &Path, slot_cap: u32) -> Result<(File, Vec<u8>), MempoolError> {
    let need = SLOTS_HEADER + (slot_cap as usize) * SLOT_REC;
    if path.exists() {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let len = file
            .metadata()
            .map_err(|e| MempoolError::io(path, e))?
            .len() as usize;
        if len < need {
            return Err(MempoolError::Corrupt("slots file short"));
        }
        let mut buf = vec![0u8; need];
        file.read_exact(&mut buf)
            .map_err(|e| MempoolError::io(path, e))?;
        if buf[0..4] != MEM_MAGIC {
            return Err(MempoolError::BadMagic);
        }
        accepted_schema(u16::from_le_bytes([buf[4], buf[5]]))?;
        Ok((file, buf))
    } else {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let mut buf = vec![0u8; need];
        buf[0..4].copy_from_slice(&MEM_MAGIC);
        buf[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        buf[8..12].copy_from_slice(&slot_cap.to_le_bytes());
        file.write_all(&buf)
            .map_err(|e| MempoolError::io(path, e))?;
        file.flush().map_err(|e| MempoolError::io(path, e))?;
        Ok((file, buf))
    }
}

#[doc(hidden)]
pub fn open_or_init_body(path: &Path) -> Result<(File, Vec<u8>), MempoolError> {
    if path.exists() {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let len = file
            .metadata()
            .map_err(|e| MempoolError::io(path, e))?
            .len() as usize;
        if len < BODY_HEADER {
            return Err(MempoolError::Corrupt("body too short"));
        }
        let mut buf = vec![0u8; len];
        file.seek(SeekFrom::Start(0))
            .map_err(|e| MempoolError::io(path, e))?;
        file.read_exact(&mut buf)
            .map_err(|e| MempoolError::io(path, e))?;
        if buf[0..4] != MEM_MAGIC {
            return Err(MempoolError::BadMagic);
        }
        accepted_schema(u16::from_le_bytes([buf[4], buf[5]]))?;
        let logical = body_logical_len(&buf)?;
        if logical > len {
            return Err(MempoolError::Corrupt("body logical past file"));
        }
        Ok((file, buf))
    } else {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| MempoolError::io(path, e))?;
        let mut buf = vec![0u8; BODY_HEADER];
        buf[0..4].copy_from_slice(&MEM_MAGIC);
        buf[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        buf[8..16].copy_from_slice(&(BODY_HEADER as u64).to_le_bytes());
        file.write_all(&buf)
            .map_err(|e| MempoolError::io(path, e))?;
        file.flush().map_err(|e| MempoolError::io(path, e))?;
        Ok((file, buf))
    }
}
