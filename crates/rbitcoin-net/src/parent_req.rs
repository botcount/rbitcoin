//! Missing-parent GETDATA tracker.
//!
//! One mutex owns the hash map and a per-peer time index. A heartbeat walks
//! only that peer's due entries. The cap is announcements per peer, as Core
//! `MAX_PEER_TX_ANNOUNCEMENTS`. There is no process-wide cap, so one peer's
//! rows never refuse another peer's. The table is at most the cap times the
//! live sessions, because `forget_peer` runs when a session unregisters.
//!
//! Every live peer can therefore wait on the same hashes. When a request
//! ends early, the work is a pass over that hash's rows and one index move
//! per waiter, with no scan of a waiter's other rows. A heartbeat reads one
//! index entry per due row of a hash another peer is being asked for.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroU64;

use super::{GETDATA_TX_INTERVAL_SECS, MAX_PARENTS_PER_PARK};

/// Announcements one peer may add. Past this the map does not grow.
pub const MAX_PARENT_ANN_PER_PEER: usize = 5_000;

/// One parent GETDATA the sweep should send.
pub struct DueParent {
    pub hash: [u8; 32],
    pub wtxid: bool,
}

/// Why `note_inv` did or did not ask this peer for the transaction now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentNote {
    /// This peer holds the in-flight request. Send getdata now.
    RequestNow,
    /// Recorded, or already recorded. Another request for these bytes is
    /// in flight, or this peer's own request is already live or failed.
    Deferred,
    /// This peer's own counter is full.
    PeerCapped,
}

#[doc(hidden)]
pub struct ParentAnn {
    pub peer: u64,
    pub preferred: bool,
    pub reqtime: u64,
    /// Bucket in `due_by_peer` while this ann is waiting to be selected.
    pub due_at: Option<u64>,
    pub requested_until: Option<u64>,
    pub failed: bool,
    /// This announcement's getdata type. A wtxid inv does not retarget
    /// another peer's txid row.
    pub wtxid: bool,
}

#[doc(hidden)]
pub struct ParentSlot {
    pub anns: Vec<ParentAnn>,
    /// The earliest `requested_until` in `anns`. `take_due` skips a hash
    /// whose request is still in flight without reading its rows.
    pub inflight_until: Option<NonZeroU64>,
}

impl ParentSlot {
    /// Call after a row's `requested_until` changes or a row leaves.
    fn sync_inflight(&mut self) {
        self.inflight_until = self
            .anns
            .iter()
            .filter_map(|a| a.requested_until)
            .min()
            .and_then(NonZeroU64::new);
    }
}

/// The slots by hash. Code in this file reaches a hash's rows only through
/// this type. Each fetch of a slot is charged that slot's row count, so a
/// test can pin how often a call fetches the rows of one hash. A loop that
/// scans one fetched slot more than once is not counted.
mod slots {
    use super::{ParentAnn, ParentSlot};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::num::NonZeroU64;

    #[derive(Default)]
    pub struct Slots {
        map: HashMap<[u8; 32], ParentSlot>,
        steps: Cell<u64>,
    }

    impl Slots {
        fn count(&self, rows: usize) {
            self.steps.set(self.steps.get().wrapping_add(rows as u64));
        }

        /// Charges since the last call: a slot's row count per fetch or
        /// removal, one per [`Self::inflight_until`], and for
        /// [`Self::push`] one plus the rescan of a requested row.
        pub fn take_steps(&self) -> u64 {
            self.steps.replace(0)
        }

        pub fn get(&self, hash: &[u8; 32]) -> Option<&ParentSlot> {
            let slot = self.map.get(hash)?;
            self.count(slot.anns.len());
            Some(slot)
        }

        pub fn get_mut(&mut self, hash: &[u8; 32]) -> Option<&mut ParentSlot> {
            let slot = self.map.get_mut(hash)?;
            self.steps
                .set(self.steps.get().wrapping_add(slot.anns.len() as u64));
            Some(slot)
        }

        /// The slot's earliest request expiry, without its rows.
        pub fn inflight_until(&self, hash: &[u8; 32]) -> Option<NonZeroU64> {
            self.count(1);
            self.map.get(hash)?.inflight_until
        }

        /// Append a row. The slot is not handed out. A requested row
        /// rescans the slot for its earliest request and is charged for it.
        pub fn push(&mut self, hash: [u8; 32], ann: ParentAnn) {
            let requested = ann.requested_until.is_some();
            let slot = self.map.entry(hash).or_insert_with(|| ParentSlot {
                anns: Vec::with_capacity(1),
                inflight_until: None,
            });
            slot.anns.push(ann);
            let mut rows = 1;
            if requested {
                slot.sync_inflight();
                rows += slot.anns.len();
            }
            self.count(rows);
        }

        pub fn remove(&mut self, hash: &[u8; 32]) -> Option<ParentSlot> {
            let slot = self.map.remove(hash)?;
            self.count(slot.anns.len());
            Some(slot)
        }

        pub fn contains_key(&self, hash: &[u8; 32]) -> bool {
            self.map.contains_key(hash)
        }

        pub fn is_empty(&self) -> bool {
            self.map.is_empty()
        }

        pub fn iter(&self) -> impl Iterator<Item = (&[u8; 32], &ParentSlot)> {
            self.map.iter()
        }
    }
}
pub use slots::Slots;

/// One peer's rows by time: `(time, hash, wtxid)`, so a row is removed
/// without a scan. The kind is in the key because one peer can hold a txid
/// row and a wtxid row for the same hash at the same time.
pub type TimeIndex = HashMap<u64, BTreeSet<(u64, [u8; 32], bool)>>;

/// Hash map plus per-peer due and in-flight indexes. The indexes are one
/// entry per announcement and stop at the same caps (extra RAM, not a scan
/// of every peer on the heartbeat).
pub struct ParentTracker {
    pub by_hash: Slots,
    pub due_by_peer: TimeIndex,
    pub inflight_by_peer: TimeIndex,
    pub hashes_by_peer: HashMap<u64, HashSet<[u8; 32]>>,
    pub ann_per_peer: HashMap<u64, usize>,
    /// Peers that took a row and have not been forgotten. Only these take a
    /// row as another transaction's announcer, so a row added after
    /// `forget_peer` cannot outlive its session.
    pub sessions: HashSet<u64>,
    pub announcements: usize,
}

impl Default for ParentTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentTracker {
    pub fn new() -> Self {
        Self {
            by_hash: Slots::default(),
            due_by_peer: HashMap::new(),
            inflight_by_peer: HashMap::new(),
            hashes_by_peer: HashMap::new(),
            ann_per_peer: HashMap::new(),
            sessions: HashSet::new(),
            announcements: 0,
        }
    }

    #[doc(hidden)]
    pub fn peer_at_cap(&self, peer: u64) -> bool {
        self.ann_per_peer.get(&peer).copied().unwrap_or(0) >= MAX_PARENT_ANN_PER_PEER
    }

    pub fn note_inv(
        &mut self,
        peer: u64,
        hash: [u8; 32],
        inbound: bool,
        now: u64,
        wtxid: bool,
    ) -> ParentNote {
        if let Some(note) = self.rearm_existing(peer, hash, now, wtxid) {
            return note;
        }
        if self.peer_at_cap(peer) {
            return ParentNote::PeerCapped;
        }
        let exp = now.saturating_add(GETDATA_TX_INTERVAL_SECS);
        // Same-kind in-flight stays the one request. This announcer waits
        // until that window ends and keeps its own getdata type.
        let (due_at, requested_until) = match self.kind_inflight_until(&hash, wtxid) {
            Some(until) => (Some(until), None),
            None => (None, Some(exp)),
        };
        let note = if requested_until.is_some() {
            ParentNote::RequestNow
        } else {
            ParentNote::Deferred
        };
        self.add_ann(
            hash,
            ParentAnn {
                peer,
                preferred: !inbound,
                reqtime: now,
                due_at,
                requested_until,
                failed: false,
                wtxid,
            },
        );
        note
    }

    pub fn schedule(&mut self, hash: [u8; 32], peer: u64, preferred: bool, reqtime: u64) {
        if self
            .by_hash
            .get(&hash)
            .is_some_and(|slot| slot.anns.iter().any(|a| a.peer == peer && !a.wtxid))
        {
            return;
        }
        if self.peer_at_cap(peer) {
            return;
        }
        self.add_ann(
            hash,
            ParentAnn {
                peer,
                preferred,
                reqtime,
                due_at: Some(reqtime),
                requested_until: None,
                failed: false,
                wtxid: false,
            },
        );
    }

    /// `Some` when this peer already had this kind of announcement for `hash`.
    #[doc(hidden)]
    pub fn rearm_existing(
        &mut self,
        peer: u64,
        hash: [u8; 32],
        now: u64,
        wtxid: bool,
    ) -> Option<ParentNote> {
        let slot = self.by_hash.get_mut(&hash)?;
        let pos = slot
            .anns
            .iter()
            .position(|a| a.peer == peer && a.wtxid == wtxid)?;
        let other_inflight = slot
            .anns
            .iter()
            .any(|a| a.wtxid == wtxid && a.peer != peer && a.requested_until.is_some());
        let due_at = {
            if slot.anns[pos].requested_until.is_some() || slot.anns[pos].failed || other_inflight {
                return Some(ParentNote::Deferred);
            }
            let exp = now.saturating_add(GETDATA_TX_INTERVAL_SECS);
            let due_at = slot.anns[pos].due_at;
            slot.anns[pos].requested_until = Some(exp);
            slot.anns[pos].due_at = None;
            slot.sync_inflight();
            (due_at, exp)
        };
        if let Some(t) = due_at.0 {
            unindex(&mut self.due_by_peer, peer, t, &hash, wtxid);
        }
        index_at(&mut self.inflight_by_peer, peer, due_at.1, hash, wtxid);
        Some(ParentNote::RequestNow)
    }

    #[doc(hidden)]
    pub fn add_ann(&mut self, hash: [u8; 32], ann: ParentAnn) {
        let peer = ann.peer;
        let wtxid = ann.wtxid;
        let due_at = ann.due_at;
        let exp = ann.requested_until;
        self.by_hash.push(hash, ann);
        self.sessions.insert(peer);
        *self.ann_per_peer.entry(peer).or_default() += 1;
        self.announcements += 1;
        self.hashes_by_peer.entry(peer).or_default().insert(hash);
        if let Some(exp) = exp {
            index_at(&mut self.inflight_by_peer, peer, exp, hash, wtxid);
        } else if let Some(t) = due_at {
            index_at(&mut self.due_by_peer, peer, t, hash, wtxid);
        }
    }

    /// The peer answered notfound for a hash we asked it for. That request
    /// is over, and a waiter of the same kind is due at its announcement.
    pub fn note_not_found(&mut self, peer: u64, hash: [u8; 32]) {
        let inflight: Vec<(u64, bool)> = self
            .by_hash
            .get(&hash)
            .map(|slot| {
                slot.anns
                    .iter()
                    .filter(|a| a.peer == peer)
                    .filter_map(|a| a.requested_until.map(|exp| (exp, a.wtxid)))
                    .collect()
            })
            .unwrap_or_default();
        for (exp, wtxid) in inflight {
            self.fail_inflight(&hash, peer, exp, wtxid);
            self.release_waiters(&hash, wtxid);
        }
    }

    pub fn forget_peer(&mut self, peer: u64) {
        self.sessions.remove(&peer);
        self.due_by_peer.remove(&peer);
        self.inflight_by_peer.remove(&peer);
        let Some(hashes) = self.hashes_by_peer.remove(&peer) else {
            return;
        };
        for hash in hashes {
            self.remove_peer_ann_kind(&hash, peer, None);
        }
    }

    pub fn announcer_peers(&self, hashes: [[u8; 32]; 2]) -> Vec<u64> {
        let mut peers = Vec::new();
        for hash in hashes {
            let Some(slot) = self.by_hash.get(&hash) else {
                continue;
            };
            for a in &slot.anns {
                if !a.failed && !peers.contains(&a.peer) {
                    peers.push(a.peer);
                }
            }
        }
        peers
    }

    pub fn resolve(&mut self, hashes: [[u8; 32]; 2], admitted: bool) {
        for hash in hashes {
            if admitted {
                self.remove_hash(&hash);
                continue;
            }
            let inflight = self.inflight_peers(&hash);
            for (peer, exp, wtxid) in inflight {
                self.fail_inflight(&hash, peer, exp, wtxid);
            }
            if self.slot_all_failed(&hash) {
                self.remove_hash(&hash);
            }
        }
    }

    pub fn take_due(
        &mut self,
        peer: u64,
        now: u64,
        mut already_have: impl FnMut(&[u8; 32], bool) -> bool,
    ) -> Vec<DueParent> {
        self.expire_peer_inflight(peer, now);
        let due_hashes = self.due_hashes(peer, now);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for hash in due_hashes {
            if out.len() >= MAX_PARENTS_PER_PARK {
                break;
            }
            // A request that has not ended blocks every row of this hash,
            // and nothing below would change one.
            if self
                .by_hash
                .inflight_until(&hash)
                .is_some_and(|until| until.get() > now)
            {
                continue;
            }
            if !seen.insert(hash) {
                continue;
            }
            self.expire_hash_inflight(&hash, now);
            if !self.by_hash.contains_key(&hash) {
                continue;
            }
            if self.slot_all_failed(&hash) {
                self.remove_hash(&hash);
                continue;
            }
            let Some(wtxid) = self.select(peer, now, &hash) else {
                continue;
            };
            if already_have(&hash, wtxid) {
                self.drop_kind(&hash, wtxid);
                continue;
            }
            out.push(DueParent { hash, wtxid });
        }
        out
    }

    #[doc(hidden)]
    pub fn due_hashes(&self, peer: u64, now: u64) -> Vec<[u8; 32]> {
        let Some(rows) = self.due_by_peer.get(&peer) else {
            return Vec::new();
        };
        rows.range(..=through(now)).map(|(_, h, _)| *h).collect()
    }

    /// When this kind is already in flight, the time that request ends.
    #[doc(hidden)]
    pub fn kind_inflight_until(&self, hash: &[u8; 32], wtxid: bool) -> Option<u64> {
        self.by_hash.get(hash).and_then(|slot| {
            slot.anns
                .iter()
                .filter(|a| a.wtxid == wtxid)
                .filter_map(|a| a.requested_until)
                .min()
        })
    }

    #[doc(hidden)]
    pub fn select(&mut self, peer: u64, now: u64, hash: &[u8; 32]) -> Option<bool> {
        let exp = now.saturating_add(GETDATA_TX_INTERVAL_SECS);
        let (due_at, wtxid) = {
            let slot = self.by_hash.get_mut(hash)?;
            let has_pref = slot
                .anns
                .iter()
                .any(|a| a.preferred && !a.failed && a.reqtime <= now);
            let pos = slot.anns.iter().position(|a| {
                !a.failed && a.reqtime <= now && a.peer == peer && (!has_pref || a.preferred)
            })?;
            let wtxid = slot.anns[pos].wtxid;
            // Any in-flight request for these bytes blocks a second getdata.
            // A non-segwit wtxid inv uses the txid, so the orphan parent is
            // already in flight. The kind we would send stays this ann's own.
            if slot.anns.iter().any(|a| a.requested_until.is_some()) {
                return None;
            }
            let ann = &mut slot.anns[pos];
            let due_at = ann.due_at.unwrap_or(ann.reqtime);
            ann.requested_until = Some(exp);
            ann.due_at = None;
            slot.sync_inflight();
            (due_at, wtxid)
        };
        unindex(&mut self.due_by_peer, peer, due_at, hash, wtxid);
        index_at(&mut self.inflight_by_peer, peer, exp, *hash, wtxid);
        Some(wtxid)
    }

    #[doc(hidden)]
    pub fn expire_peer_inflight(&mut self, peer: u64, now: u64) {
        let expired = self
            .inflight_by_peer
            .get(&peer)
            .map(|rows| rows.range(..=through(now)).copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for (exp, hash, wtxid) in expired {
            self.fail_inflight(&hash, peer, exp, wtxid);
            if self.slot_all_failed(&hash) {
                self.remove_hash(&hash);
            }
        }
    }

    #[doc(hidden)]
    pub fn expire_hash_inflight(&mut self, hash: &[u8; 32], now: u64) {
        let expired = self.inflight_peers(hash);
        for (peer, exp, wtxid) in expired {
            if exp <= now {
                self.fail_inflight(hash, peer, exp, wtxid);
            }
        }
        if self.slot_all_failed(hash) {
            self.remove_hash(hash);
        }
    }

    #[doc(hidden)]
    pub fn inflight_peers(&self, hash: &[u8; 32]) -> Vec<(u64, u64, bool)> {
        self.by_hash
            .get(hash)
            .map(|slot| {
                slot.anns
                    .iter()
                    .filter_map(|a| a.requested_until.map(|exp| (a.peer, exp, a.wtxid)))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn fail_inflight(&mut self, hash: &[u8; 32], peer: u64, exp: u64, wtxid: bool) {
        // Expire the ann whose window is `exp`. A same-peer txid parent is a
        // different row and must stay eligible once this window ends.
        if let Some(slot) = self.by_hash.get_mut(hash) {
            if let Some(ann) = slot
                .anns
                .iter_mut()
                .find(|a| a.peer == peer && a.wtxid == wtxid && a.requested_until == Some(exp))
            {
                ann.requested_until = None;
                ann.failed = true;
                slot.sync_inflight();
            }
        }
        unindex(&mut self.inflight_by_peer, peer, exp, hash, wtxid);
    }

    #[doc(hidden)]
    pub fn slot_all_failed(&self, hash: &[u8; 32]) -> bool {
        self.by_hash
            .get(hash)
            .is_some_and(|slot| !slot.anns.is_empty() && slot.anns.iter().all(|a| a.failed))
    }

    #[doc(hidden)]
    pub fn remove_hash(&mut self, hash: &[u8; 32]) {
        let Some(slot) = self.by_hash.remove(hash) else {
            return;
        };
        for ann in slot.anns {
            self.note_removed_ann(&ann, hash);
        }
    }

    #[doc(hidden)]
    pub fn drop_kind(&mut self, hash: &[u8; 32], wtxid: bool) {
        let peers: Vec<u64> = self
            .by_hash
            .get(hash)
            .map(|slot| {
                slot.anns
                    .iter()
                    .filter(|a| a.wtxid == wtxid)
                    .map(|a| a.peer)
                    .collect()
            })
            .unwrap_or_default();
        for peer in peers {
            self.remove_peer_ann_kind(hash, peer, Some(wtxid));
        }
    }

    #[doc(hidden)]
    pub fn remove_peer_ann_kind(&mut self, hash: &[u8; 32], peer: u64, kind: Option<bool>) {
        loop {
            let Some(ann) = self.by_hash.get_mut(hash).and_then(|slot| {
                let pos = slot
                    .anns
                    .iter()
                    .position(|a| a.peer == peer && kind.is_none_or(|k| a.wtxid == k))?;
                let ann = slot.anns.swap_remove(pos);
                if ann.requested_until.is_some() {
                    slot.sync_inflight();
                }
                Some(ann)
            }) else {
                return;
            };
            if self
                .by_hash
                .get(hash)
                .is_some_and(|slot| slot.anns.is_empty())
            {
                self.by_hash.remove(hash);
            }
            self.note_removed_ann(&ann, hash);
            if kind.is_some() {
                return;
            }
        }
    }

    #[doc(hidden)]
    pub fn note_removed_ann(&mut self, ann: &ParentAnn, hash: &[u8; 32]) {
        self.announcements = self.announcements.saturating_sub(1);
        if let Some(c) = self.ann_per_peer.get_mut(&ann.peer) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.ann_per_peer.remove(&ann.peer);
            }
        }
        if let Some(set) = self.hashes_by_peer.get_mut(&ann.peer) {
            let still = self
                .by_hash
                .get(hash)
                .is_some_and(|slot| slot.anns.iter().any(|a| a.peer == ann.peer));
            if !still {
                set.remove(hash);
                if set.is_empty() {
                    self.hashes_by_peer.remove(&ann.peer);
                }
            }
        }
        if let Some(exp) = ann.requested_until {
            unindex(&mut self.inflight_by_peer, ann.peer, exp, hash, ann.wtxid);
        }
        if let Some(t) = ann.due_at {
            unindex(&mut self.due_by_peer, ann.peer, t, hash, ann.wtxid);
        }
        if ann.requested_until.is_some() {
            self.release_waiters(hash, ann.wtxid);
        }
    }

    /// The in-flight request is gone. Waiters of this kind were due when
    /// that window ended. They are due at their own announcement time.
    #[doc(hidden)]
    pub fn release_waiters(&mut self, hash: &[u8; 32], wtxid: bool) {
        if self.kind_inflight_until(hash, wtxid).is_some() {
            return;
        }
        let Some(slot) = self.by_hash.get_mut(hash) else {
            return;
        };
        let mut moved = Vec::new();
        for ann in &mut slot.anns {
            if ann.wtxid != wtxid || ann.failed {
                continue;
            }
            let Some(due_at) = ann.due_at.filter(|due| *due > ann.reqtime) else {
                continue;
            };
            ann.due_at = Some(ann.reqtime);
            moved.push((ann.peer, due_at, ann.reqtime));
        }
        for (peer, due_at, reqtime) in moved {
            unindex(&mut self.due_by_peer, peer, due_at, hash, wtxid);
            index_at(&mut self.due_by_peer, peer, reqtime, *hash, wtxid);
        }
    }
}

/// Upper bound of every index entry at or before `now`.
fn through(now: u64) -> (u64, [u8; 32], bool) {
    (now, [u8::MAX; 32], true)
}

#[doc(hidden)]
pub fn index_at(index: &mut TimeIndex, peer: u64, time: u64, hash: [u8; 32], wtxid: bool) {
    index.entry(peer).or_default().insert((time, hash, wtxid));
}

#[doc(hidden)]
pub fn unindex(index: &mut TimeIndex, peer: u64, time: u64, hash: &[u8; 32], wtxid: bool) {
    let Some(rows) = index.get_mut(&peer) else {
        return;
    };
    rows.remove(&(time, *hash, wtxid));
    if rows.is_empty() {
        index.remove(&peer);
    }
}
