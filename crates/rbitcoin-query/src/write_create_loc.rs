//! Write-thread loc pairs from Class A append (just-written parents).
//!
//! Lookup TipOnly reads `create.loc` after [`crate::Query::note_lookup_tiponly_start`].
//! At note, `keep_until` is the last height whose TipOnly had started
//! (`lookup_started_hi`, at least the noting pack). That value is never bumped.
//! Prune drops a pack when that height has finished write (`written_hi ≥
//! keep_until`) and the pack is not the one just written (`pack_height <
//! written_hi`). Fill of that write runs first. Later-wave spends take TipOnly
//! loc on the in-flight identity; RAM loc is only for same-wave holes.
//!
//! This window is **write-thread TLS** (one writer per thread — same ownership
//! as load's [`crate::InFlight`], not a `Query` mutex). Disconnect is polled
//! via [`crate::Query::take_disconnect`] on the write thread.

use rbitcoin_primitives::Fk;
use rbitcoin_store::CreateLocPair;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use crate::Query;

#[doc(hidden)]
pub const PAIR_BYTES: u64 = std::mem::size_of::<CreateLocPair>() as u64;

/// Last TipOnly'd height at note, floored at `pack_height`.
pub fn keep_until_at_note(pack_height: u32, started_hi: Option<u32>) -> u32 {
    started_hi.unwrap_or(pack_height).max(pack_height)
}

#[derive(Debug)]
#[doc(hidden)]
pub struct LocPack {
    pub keep_until: u32,
    pub base: u64,
    pub pairs: Vec<CreateLocPair>,
    pub approx_bytes: u64,
}

impl LocPack {
    #[doc(hidden)]
    pub fn get(&self, id: u64) -> Option<CreateLocPair> {
        let off = id.checked_sub(self.base)?;
        self.pairs.get(off as usize).copied()
    }
}

/// One pack's create-id span, ordered by `base` then height.
#[derive(Clone, Copy, Debug)]
struct LocCover {
    base: u64,
    len: u64,
    height: u32,
    /// Highest id covered by this cover or any cover earlier in `(base, height)` order.
    max_id_through: u64,
}

/// Write-owned loc window. Note after append; prune after that write batch
/// (and later overlapping batches) finish. A read takes the lowest pack whose
/// create range covers that id.
#[derive(Debug, Default)]
pub struct WriteCreateLocRam {
    pub by_height: BTreeMap<u32, LocPack>,
    pub approx_bytes: u64,
    covers: Vec<LocCover>,
}

impl WriteCreateLocRam {
    pub fn note(&mut self, pack_height: u32, keep_until: u32, fks: &[Fk], loc: &[CreateLocPair]) {
        if fks.is_empty() || loc.len() != fks.len() {
            return;
        }
        let Some(start) = fks[0].get() else {
            return;
        };
        let bytes = (loc.len() as u64).saturating_mul(PAIR_BYTES);
        if let Some(old) = self.by_height.insert(
            pack_height,
            LocPack {
                keep_until,
                base: start,
                pairs: loc.to_vec(),
                approx_bytes: bytes,
            },
        ) {
            self.approx_bytes = self.approx_bytes.saturating_sub(old.approx_bytes);
        }
        self.approx_bytes = self.approx_bytes.saturating_add(bytes);
        self.rebuild_covers();
    }

    pub fn get(&self, fk: Fk) -> Option<CreateLocPair> {
        let id = fk.get()?;
        let height = self.covering_height(id)?;
        self.by_height.get(&height)?.get(id)
    }

    /// Height of the pack [`Self::get`] would read for `fk`.
    #[doc(hidden)]
    pub fn cover_height(&self, fk: Fk) -> Option<u32> {
        self.covering_height(fk.get()?)
    }

    /// Lowest height whose range covers `id`.
    ///
    /// A lower height can start later and still overlap, so the rightmost
    /// base is not the owner. Walk left while an earlier cover's highest id
    /// still reaches `id`. The span is the same offset check as [`LocPack::get`].
    fn covering_height(&self, id: u64) -> Option<u32> {
        let right = self.covers.partition_point(|cover| cover.base <= id);
        let mut index = right.checked_sub(1)?;
        let mut best: Option<u32> = None;
        loop {
            let cover = self.covers[index];
            if id
                .checked_sub(cover.base)
                .is_some_and(|off| off < cover.len)
            {
                best = Some(best.map_or(cover.height, |height| height.min(cover.height)));
            }
            if index == 0 || self.covers[index - 1].max_id_through < id {
                break;
            }
            index -= 1;
        }
        best
    }

    fn rebuild_covers(&mut self) {
        let mut covers = Vec::with_capacity(self.by_height.len());
        for (&height, pack) in &self.by_height {
            let len = pack.pairs.len() as u64;
            if len == 0 {
                continue;
            }
            covers.push(LocCover {
                base: pack.base,
                len,
                height,
                max_id_through: 0,
            });
        }
        covers.sort_unstable_by(|a, b| a.base.cmp(&b.base).then(a.height.cmp(&b.height)));
        let mut max_id = 0u64;
        for cover in &mut covers {
            let last = cover.base.saturating_add(cover.len - 1);
            max_id = max_id.max(last);
            cover.max_id_through = max_id;
        }
        self.covers = covers;
    }

    /// Drop packs whose last-started-at-note height has finished write.
    ///
    /// `keep_until` is fixed at note. Equality drops once `written_hi` covers
    /// that height, except the noting pack (`pack_height < written_hi`).
    pub fn prune_written_through(&mut self, written_hi: u32) {
        let drop: Vec<u32> = self
            .by_height
            .iter()
            .filter(|(h, p)| **h < written_hi && p.keep_until <= written_hi)
            .map(|(h, _)| *h)
            .collect();
        for h in drop {
            if let Some(p) = self.by_height.remove(&h) {
                self.approx_bytes = self.approx_bytes.saturating_sub(p.approx_bytes);
            }
        }
        self.rebuild_covers();
    }

    /// Disconnect: drop packs at/above `height`. Remaining keep-until cannot
    /// wait for disconnected lookup batches.
    pub fn drop_from_height(&mut self, height: u32) {
        let drop = self.by_height.split_off(&height);
        for p in drop.into_values() {
            self.approx_bytes = self.approx_bytes.saturating_sub(p.approx_bytes);
        }
        let cap = height.saturating_sub(1);
        for p in self.by_height.values_mut() {
            if p.keep_until > cap {
                p.keep_until = cap;
            }
        }
        self.rebuild_covers();
    }

    /// Packs, pair count, approx bytes (IBD `wloc=`).
    pub fn size_snapshot(&self) -> (usize, usize, u64) {
        let pairs: usize = self.by_height.values().map(|p| p.pairs.len()).sum();
        (self.by_height.len(), pairs, self.approx_bytes)
    }
}

thread_local! {
    static RAM: RefCell<WriteCreateLocRam> = RefCell::new(WriteCreateLocRam::default());
    static DISCONNECT_SEEN: Cell<u64> = const { Cell::new(0) };
}

#[doc(hidden)]
pub fn publish(ram: &WriteCreateLocRam) {
    let (packs, pairs, bytes) = ram.size_snapshot();
    crate::process_mem_stats::note_wloc(packs, pairs, bytes);
}

/// Drop this thread's loc window (Query open / test isolation). Does not publish
/// zeros — `wloc=` atomics are process-wide.
pub fn clear() {
    RAM.with(|c| {
        *c.borrow_mut() = WriteCreateLocRam::default();
    });
    DISCONNECT_SEEN.with(|c| c.set(0));
}

pub fn with_ram<R>(query: &Query, f: impl FnOnce(&mut WriteCreateLocRam) -> R) -> R {
    RAM.with(|c| {
        let mut ram = c.borrow_mut();
        poll_disconnect(query, &mut ram);
        let r = f(&mut ram);
        publish(&ram);
        r
    })
}

#[doc(hidden)]
pub fn poll_disconnect(query: &Query, ram: &mut WriteCreateLocRam) {
    let mut seen = DISCONNECT_SEEN.with(|c| c.get());
    if let Some(h) = query.take_disconnect(&mut seen) {
        ram.drop_from_height(h);
        DISCONNECT_SEEN.with(|c| c.set(seen));
    }
}
