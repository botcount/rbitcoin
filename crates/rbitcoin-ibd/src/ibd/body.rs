//! Process-local Class A body presence cache for IBD assign/status.

use bitcoin::BlockHash;
use rbitcoin_net::chain::ChainHub;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// O(1) occupancy of each [`BodyPresence`] set (for `ibd: sizes` logs).
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct BodyPresenceSizes {
    pub known: usize,
    pub pending: usize,
    pub missing: usize,
    pub rejected: usize,
}

/// One hash in the presence cache.
///
/// `KnownPending` is an archived hash that is also waiting on confirm.
/// `mark_pending` does not clear the archived bit.
#[derive(Clone, Copy)]
enum Slot {
    Known,
    Missing,
    Rejected,
    Pending(Instant),
    KnownPending(Instant),
}

impl Slot {
    fn pending_at(self) -> Option<Instant> {
        match self {
            Self::Pending(since) | Self::KnownPending(since) => Some(since),
            _ => None,
        }
    }

    fn is_known(self) -> bool {
        matches!(self, Self::Known | Self::KnownPending(_))
    }
}

/// Process-local Class A body presence cache.
///
/// Assign/status used to call `is_archived` (store) for every ordered hash every
/// loop — that contended with the archive writer and delayed getdata top-up
/// (spiky receive). Probe each hash at most once until archive marks it ready.
///
/// `pending` covers the wire→confirm gap: we clear getdata inflight on body
/// receipt, but must not re-request until confirm advances tip (or re-get after
/// stale expire).
///
/// `rejected` covers Class C permanent failures: never re-offer or re-download.
pub struct BodyPresence {
    slots: HashMap<BlockHash, Slot>,
    /// Pending hashes only, so a stale check does not walk archived rows.
    pending_keys: HashSet<BlockHash>,
    known: usize,
    pending: usize,
    missing: usize,
    rejected: usize,
}

impl Default for BodyPresence {
    fn default() -> Self {
        Self::new()
    }
}

impl BodyPresence {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            pending_keys: HashSet::new(),
            known: 0,
            pending: 0,
            missing: 0,
            rejected: 0,
        }
    }

    fn note(&mut self, slot: Slot) {
        match slot {
            Slot::Known => self.known += 1,
            Slot::Missing => self.missing += 1,
            Slot::Rejected => self.rejected += 1,
            Slot::Pending(_) => self.pending += 1,
            Slot::KnownPending(_) => {
                self.known += 1;
                self.pending += 1;
            }
        }
    }

    fn unnote(&mut self, slot: Slot) {
        match slot {
            Slot::Known => self.known -= 1,
            Slot::Missing => self.missing -= 1,
            Slot::Rejected => self.rejected -= 1,
            Slot::Pending(_) => self.pending -= 1,
            Slot::KnownPending(_) => {
                self.known -= 1;
                self.pending -= 1;
            }
        }
    }

    fn put(&mut self, hash: BlockHash, next: Option<Slot>) {
        if let Some(prev) = self.slots.remove(&hash) {
            if prev.pending_at().is_some() {
                self.pending_keys.remove(&hash);
            }
            self.unnote(prev);
        }
        if let Some(slot) = next {
            if slot.pending_at().is_some() {
                self.pending_keys.insert(hash);
            }
            self.note(slot);
            self.slots.insert(hash, slot);
        }
    }

    #[inline]
    #[doc(hidden)]
    pub fn is_pending_hash(&self, h: &BlockHash) -> bool {
        self.slots
            .get(h)
            .and_then(|slot| slot.pending_at())
            .is_some()
    }

    pub fn mark_pending(&mut self, h: BlockHash) {
        match self.slots.get(&h).copied() {
            Some(Slot::Rejected | Slot::Pending(_) | Slot::KnownPending(_)) => {}
            Some(Slot::Known) => self.put(h, Some(Slot::KnownPending(Instant::now()))),
            Some(Slot::Missing) | None => self.put(h, Some(Slot::Pending(Instant::now()))),
        }
    }

    /// Set the pending instant. An archived hash stays archived.
    #[doc(hidden)]
    pub fn set_pending_since(&mut self, h: BlockHash, since: Instant) {
        match self.slots.get(&h).copied() {
            Some(Slot::Rejected) => {}
            Some(Slot::Known | Slot::KnownPending(_)) => {
                self.put(h, Some(Slot::KnownPending(since)));
            }
            Some(Slot::Pending(_) | Slot::Missing) | None => {
                self.put(h, Some(Slot::Pending(since)));
            }
        }
    }

    pub fn mark_archived(&mut self, h: BlockHash) {
        if matches!(self.slots.get(&h), Some(Slot::Rejected)) {
            return;
        }
        self.put(h, Some(Slot::Known));
    }

    pub fn mark_missing(&mut self, h: BlockHash) {
        if matches!(self.slots.get(&h), Some(Slot::Rejected)) {
            return;
        }
        self.put(h, Some(Slot::Missing));
    }

    /// Stop asking for `h` without blacklisting it. A halted tip-child retry
    /// must not stay on the missing set from the earlier attempts.
    pub(crate) fn stop_reget(&mut self, h: BlockHash) {
        match self.slots.get(&h).copied() {
            Some(Slot::Known | Slot::KnownPending(_)) => self.put(h, Some(Slot::Known)),
            Some(Slot::Pending(_) | Slot::Missing) => self.put(h, None),
            Some(Slot::Rejected) | None => {}
        }
    }

    /// Drop optimistic Class A cache only — next [`Self::ready`] re-probes the store.
    pub fn demote_known(&mut self, h: BlockHash) {
        match self.slots.get(&h).copied() {
            Some(Slot::Known) => self.put(h, None),
            Some(Slot::KnownPending(since)) => self.put(h, Some(Slot::Pending(since))),
            _ => {}
        }
    }

    /// Permanent confirm failure: never re-offer this hash to the confirm engine
    /// and never re-getdata it.
    pub fn mark_rejected(&mut self, h: BlockHash) {
        self.put(h, Some(Slot::Rejected));
    }

    /// Pending hashes older than `max_age` matching `pred` → mark missing (re-get).
    pub fn expire_stale_pending_if(
        &mut self,
        max_age: Duration,
        mut pred: impl FnMut(&BlockHash) -> bool,
    ) -> Vec<BlockHash> {
        let now = Instant::now();
        let stale: Vec<BlockHash> = self
            .pending_keys
            .iter()
            .filter(|hash| {
                self.slots
                    .get(*hash)
                    .and_then(|slot| slot.pending_at())
                    .is_some_and(|since| pred(hash) && now.duration_since(since) >= max_age)
            })
            .copied()
            .collect();
        for h in &stale {
            self.mark_missing(*h);
        }
        stale
    }

    pub fn is_rejected(&self, h: &BlockHash) -> bool {
        matches!(self.slots.get(h), Some(Slot::Rejected))
    }

    /// True if we already know Class A is present (no store probe).
    pub fn is_known_archived(&self, h: &BlockHash) -> bool {
        self.slots.get(h).is_some_and(|slot| slot.is_known())
    }

    pub fn known_len(&self) -> usize {
        self.known
    }

    pub fn size_snapshot(&self) -> BodyPresenceSizes {
        BodyPresenceSizes {
            known: self.known,
            pending: self.pending,
            missing: self.missing,
            rejected: self.rejected,
        }
    }

    pub fn is_pending(&self, h: &BlockHash) -> bool {
        self.is_pending_hash(h)
    }

    pub fn is_missing(&self, h: &BlockHash) -> bool {
        matches!(self.slots.get(h), Some(Slot::Missing))
    }

    /// Hot path: local sets first. Do **not** call `has_block` before checking
    /// `missing` — assign walks tens of thousands of known-missing far hashes.
    ///
    /// Does **not** treat `known` (Class A cache) as skip — that set is densify
    /// bookkeeping; tip-hole re-get after resume still needs peer wire into BQ.
    pub fn skip_download(&mut self, hub: &ChainHub, h: &BlockHash) -> bool {
        if self.is_rejected(h) || self.is_pending_hash(h) {
            return true;
        }
        if self.is_missing(h) {
            return false;
        }
        if hub.has_block(h) {
            self.put(*h, Some(Slot::Known));
            return true;
        }
        false
    }

    /// Drop cache entries that are no longer on the live work path.
    /// Rejected entries are never pruned.
    pub fn hygiene_retain(&mut self, mut keep: impl FnMut(&BlockHash) -> bool) {
        let mut drop_known = 0;
        let mut drop_pending = 0;
        let mut drop_missing = 0;
        let mut drop_pending_keys = Vec::new();
        self.slots.retain(|hash, slot| {
            if matches!(slot, Slot::Rejected) || keep(hash) {
                return true;
            }
            match slot {
                Slot::Known => drop_known += 1,
                Slot::Pending(_) => {
                    drop_pending += 1;
                    drop_pending_keys.push(*hash);
                }
                Slot::KnownPending(_) => {
                    drop_known += 1;
                    drop_pending += 1;
                    drop_pending_keys.push(*hash);
                }
                Slot::Missing => drop_missing += 1,
                Slot::Rejected => {}
            }
            false
        });
        for hash in drop_pending_keys {
            self.pending_keys.remove(&hash);
        }
        self.known -= drop_known;
        self.pending -= drop_pending;
        self.missing -= drop_missing;
    }
}
