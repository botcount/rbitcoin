//! Transaction orphanage for txs missing in-mempool / chain parents.
//!
//! Weight budget: 404_000 WU reserved per peer × 25-peer budget → ~10.1M WU
//! unique orphans; unique-count cap 3000. Per-tx max 404_000 WU.
//!
//! Eviction: FIFO by insert order when over weight or count.
//!
//! Keyed by wtxid, as Core since bitcoin/bitcoin#30000: two witnesses of one
//! txid are two orphans. Lookups by txid go through the txid index.

use bitcoin::{Transaction, Txid, Wtxid};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// Reserved orphan weight per peer.
pub const ORPHAN_RESERVED_WEIGHT_PER_PEER: u64 = 404_000;
/// Peer multiplier for a fixed global weight budget.
pub const ORPHAN_PEER_BUDGET: u64 = 25;
/// Global unique orphan weight cap (404k × 25 ≈ 10.1M WU).
pub const DEFAULT_ORPHAN_MAX_WEIGHT: u64 = ORPHAN_RESERVED_WEIGHT_PER_PEER * ORPHAN_PEER_BUDGET;
/// Secondary unique-count cap.
pub const DEFAULT_ORPHAN_MAX_COUNT: usize = 3_000;
/// Refuse orphans heavier than a standard tx.
pub const MAX_ORPHAN_TX_WEIGHT: u64 = 404_000;
/// Drop an orphan this long after it was parked. Not a knob.
pub const ORPHAN_EXPIRE_MS: u64 = 20 * 60 * 1000;

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct OrphanEntry {
    pub tx: Transaction,
    pub txid: Txid,
    pub weight: u64,
    /// Parent txids it waits on: absent from the mempool and chain when it
    /// was parked. One may be pooled since, when this orphan's own txid is
    /// pooled too and promote left it parked.
    pub missing: BTreeSet<Txid>,
    /// P2P peer ids that announced this orphan.
    pub announcers: BTreeSet<u64>,
    pub arrived_ms: u64,
}

/// One parked orphan (tx + announcer peer ids).
#[derive(Debug, Clone)]
pub struct OrphanSnapshot {
    pub tx: Transaction,
    pub announcers: Vec<u64>,
    /// In-flight generation from `take_children_of`. Zero for any other snapshot.
    pub taken_gen: u64,
}

/// Side pool of not-yet-acceptable txs waiting on parent(s).
#[derive(Debug, Default)]
pub struct Orphanage {
    pub by_wtxid: HashMap<Wtxid, OrphanEntry>,
    /// txid → every parked witness of it, oldest first.
    pub by_txid: HashMap<Txid, Vec<Wtxid>>,
    /// parent txid → orphan children waiting on it.
    pub by_parent: HashMap<Txid, HashSet<Wtxid>>,
    /// Eviction order, oldest first: each key of `by_wtxid` once.
    pub fifo: VecDeque<Wtxid>,
    pub total_weight: u64,
    /// Announced weight per peer. Eviction reads this instead of scanning the set.
    peer_weight: HashMap<u64, u64>,
    max_weight: u64,
    max_count: usize,
    /// Orphans `take_children_of` removed and that are not parked or admitted yet.
    ///
    /// A disconnect in that gap cannot see `by_wtxid`. It edits this map, and
    /// the repark uses the result instead of the announcer list from the take.
    taken: HashMap<Wtxid, TakenOrphan>,
    /// Next generation for [`Self::taken`]. Zero is never assigned.
    next_taken_gen: u64,
}

/// Announcers copied at take time. `drop` means a disconnect removed the last one.
#[derive(Debug, Default)]
struct TakenOrphan {
    announcers: BTreeSet<u64>,
    drop: bool,
    gen: u64,
}

/// What the repark should do with an orphan that was taken.
pub(crate) enum TakenClaim {
    /// Park under these peers. Empty means the orphan never had an announcer.
    Keep(Vec<u64>),
    /// The last announcer disconnected while the orphan was out. Do not park.
    Drop,
}

impl Orphanage {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, DEFAULT_ORPHAN_MAX_COUNT)
    }

    pub fn with_limits(max_weight: u64, max_count: usize) -> Self {
        Self {
            by_wtxid: HashMap::new(),
            by_txid: HashMap::new(),
            by_parent: HashMap::new(),
            fifo: VecDeque::new(),
            total_weight: 0,
            peer_weight: HashMap::new(),
            max_weight: max_weight.max(MAX_ORPHAN_TX_WEIGHT),
            max_count: max_count.max(1),
            taken: HashMap::new(),
            next_taken_gen: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.by_wtxid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_wtxid.is_empty()
    }

    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// Any witness of `txid` is parked.
    pub fn contains(&self, txid: &Txid) -> bool {
        self.by_txid.contains_key(txid)
    }

    pub fn contains_wtxid(&self, wtxid: &Wtxid) -> bool {
        self.by_wtxid.contains_key(wtxid)
    }

    pub fn tx_by_wtxid(&self, wtxid: &Wtxid) -> Option<&Transaction> {
        self.by_wtxid.get(wtxid).map(|e| &e.tx)
    }

    pub fn missing_of(&self, wtxid: &Wtxid) -> Option<&BTreeSet<Txid>> {
        self.by_wtxid.get(wtxid).map(|e| &e.missing)
    }

    pub fn txs(&self) -> impl Iterator<Item = &Transaction> {
        self.by_wtxid.values().map(|e| &e.tx)
    }

    /// Insert (or add `from` as announcer if already parked).
    pub fn insert_from(
        &mut self,
        tx: Transaction,
        missing: BTreeSet<Txid>,
        from: Option<u64>,
    ) -> bool {
        self.insert_from_at(tx, missing, from, crate::accept::unix_ms())
    }

    /// [`Self::insert_from`] at an explicit unix millisecond clock.
    pub fn insert_from_at(
        &mut self,
        tx: Transaction,
        missing: BTreeSet<Txid>,
        from: Option<u64>,
        now_ms: u64,
    ) -> bool {
        if missing.is_empty() {
            return false;
        }
        self.expire(now_ms);
        let txid = tx.compute_txid();
        let wtxid = tx.compute_wtxid();
        if self.by_wtxid.contains_key(&wtxid) {
            if let Some(peer) = from {
                self.add_announcer(&wtxid, peer);
            }
            return false;
        }
        let weight = tx.weight().to_wu();
        if weight > MAX_ORPHAN_TX_WEIGHT {
            return false;
        }
        if let Some(peer) = from {
            while self.peer_orphan_weight(peer).saturating_add(weight)
                > ORPHAN_RESERVED_WEIGHT_PER_PEER
            {
                if !self.evict_one_from_peer(peer) {
                    return false;
                }
            }
        }
        while !self.by_wtxid.is_empty()
            && (self.by_wtxid.len() >= self.max_count
                || self.total_weight.saturating_add(weight) > self.max_weight)
        {
            if !self.evict_oldest() {
                break;
            }
        }
        if self.by_wtxid.len() >= self.max_count {
            return false;
        }
        // Room left is the cap minus what is already parked. A tx that lands
        // on the cap still fits; one past it does not.
        if self.max_weight.saturating_sub(self.total_weight) < weight {
            return false;
        }
        for p in &missing {
            self.by_parent.entry(*p).or_default().insert(wtxid);
        }
        let mut announcers = BTreeSet::new();
        if let Some(peer) = from {
            announcers.insert(peer);
        }
        self.by_txid.entry(txid).or_default().push(wtxid);
        self.by_wtxid.insert(
            wtxid,
            OrphanEntry {
                tx,
                txid,
                weight,
                missing,
                announcers,
                arrived_ms: now_ms,
            },
        );
        self.fifo.push_back(wtxid);
        self.total_weight = self.total_weight.saturating_add(weight);
        if let Some(peer) = from {
            self.add_peer_weight(peer, weight);
        }
        true
    }

    #[doc(hidden)]
    pub fn expire(&mut self, now_ms: u64) {
        let stale: Vec<Wtxid> = self
            .by_wtxid
            .iter()
            .filter(|(_, e)| now_ms.saturating_sub(e.arrived_ms) >= ORPHAN_EXPIRE_MS)
            .map(|(wtxid, _)| *wtxid)
            .collect();
        for wtxid in stale {
            self.remove_wtxid(&wtxid);
        }
    }

    #[doc(hidden)]
    pub fn add_peer_weight(&mut self, peer: u64, weight: u64) {
        let w = self.peer_weight.entry(peer).or_default();
        *w = w.saturating_add(weight);
    }

    #[doc(hidden)]
    pub fn sub_peer_weight(&mut self, peer: u64, weight: u64) {
        let next = self
            .peer_weight
            .get(&peer)
            .copied()
            .unwrap_or(0)
            .saturating_sub(weight);
        if next == 0 {
            self.peer_weight.remove(&peer);
        } else {
            self.peer_weight.insert(peer, next);
        }
    }

    #[doc(hidden)]
    pub fn unaccount(&mut self, e: &OrphanEntry) {
        self.total_weight = self.total_weight.saturating_sub(e.weight);
        for peer in &e.announcers {
            self.sub_peer_weight(*peer, e.weight);
        }
    }

    #[doc(hidden)]
    pub fn peer_orphan_weight(&self, peer: u64) -> u64 {
        self.peer_weight.get(&peer).copied().unwrap_or(0)
    }

    #[doc(hidden)]
    pub fn announcers_within_reserve(&self, wtxid: &Wtxid) -> bool {
        let Some(e) = self.by_wtxid.get(wtxid) else {
            return false;
        };
        if e.announcers.is_empty() {
            return false;
        }
        e.announcers
            .iter()
            .any(|p| self.peer_orphan_weight(*p) <= ORPHAN_RESERVED_WEIGHT_PER_PEER)
    }

    /// Drop `peer` from the oldest orphan it announced. The tx stays when
    /// another peer still announces it; only that peer's weight is released.
    #[doc(hidden)]
    pub fn evict_one_from_peer(&mut self, peer: u64) -> bool {
        let victim = self
            .fifo
            .iter()
            .find(|wtxid| {
                self.by_wtxid
                    .get(*wtxid)
                    .is_some_and(|e| e.announcers.contains(&peer))
            })
            .copied();
        let Some(wtxid) = victim else {
            return false;
        };
        let weight = {
            let Some(e) = self.by_wtxid.get_mut(&wtxid) else {
                return false;
            };
            if !e.announcers.remove(&peer) {
                return false;
            }
            e.weight
        };
        self.sub_peer_weight(peer, weight);
        let last = self
            .by_wtxid
            .get(&wtxid)
            .is_some_and(|e| e.announcers.is_empty());
        if last {
            self.remove_wtxid(&wtxid);
        }
        true
    }

    #[doc(hidden)]
    pub fn evict_oldest(&mut self) -> bool {
        if self.evict_oldest_filtered(true) {
            return true;
        }
        // The reserve is a preference. A set that is entirely protected still
        // makes room for one newer orphan.
        self.evict_oldest_filtered(false)
    }

    #[doc(hidden)]
    pub fn evict_oldest_filtered(&mut self, honor_reserve: bool) -> bool {
        let mut skipped = VecDeque::new();
        while let Some(wtxid) = self.fifo.pop_front() {
            if honor_reserve && self.announcers_within_reserve(&wtxid) {
                skipped.push_back(wtxid);
                continue;
            }
            self.remove_wtxid(&wtxid);
            skipped.append(&mut self.fifo);
            self.fifo = skipped;
            return true;
        }
        self.fifo = skipped;
        false
    }

    /// Remove one parked tx and return it. `None` when it is not parked.
    pub fn remove_wtxid(&mut self, wtxid: &Wtxid) -> Option<Transaction> {
        let e = self.unlink(wtxid)?;
        for p in &e.missing {
            self.unlink_parent(p, wtxid);
        }
        Some(e.tx)
    }

    /// Drop `wtxid` from every index but `by_parent`, the eviction order
    /// included, so a later park of the same wtxid is the newest.
    fn unlink(&mut self, wtxid: &Wtxid) -> Option<OrphanEntry> {
        let e = self.by_wtxid.remove(wtxid)?;
        self.fifo.retain(|w| w != wtxid);
        if let Some(versions) = self.by_txid.get_mut(&e.txid) {
            versions.retain(|w| w != wtxid);
            if versions.is_empty() {
                self.by_txid.remove(&e.txid);
            }
        }
        self.unaccount(&e);
        Some(e)
    }

    fn unlink_parent(&mut self, parent: &Txid, wtxid: &Wtxid) {
        if let Some(set) = self.by_parent.get_mut(parent) {
            set.remove(wtxid);
            if set.is_empty() {
                self.by_parent.remove(parent);
            }
        }
    }

    /// Take all orphans that listed `parent` as missing (for re-accept).
    ///
    /// The announcer list comes with the transaction. A child that still
    /// misses another parent is parked again under those same peers.
    ///
    /// A child whose txid `pooled` already holds stays parked: it cannot
    /// enter. Core leaves such an orphan when it entered the mempool other
    /// than by relay (an RPC package), or when a version with a different
    /// witness entered. `Find1P1CPackage`, a block, or expiry erases it.
    pub fn take_children_of(
        &mut self,
        parent: &Txid,
        pooled: impl Fn(&Txid) -> bool,
    ) -> Vec<OrphanSnapshot> {
        let Some(children) = self.by_parent.remove(parent) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(children.len());
        let mut stay = HashSet::new();
        for cid in children {
            if self.by_wtxid.get(&cid).is_some_and(|e| pooled(&e.txid)) {
                stay.insert(cid);
                continue;
            }
            if let Some(e) = self.unlink(&cid) {
                for p in &e.missing {
                    if p != parent {
                        self.unlink_parent(p, &cid);
                    }
                }
                let announcers = e.announcers;
                let gen = self.alloc_taken_gen();
                self.taken.insert(
                    cid,
                    TakenOrphan {
                        announcers: announcers.clone(),
                        drop: false,
                        gen,
                    },
                );
                out.push(OrphanSnapshot {
                    tx: e.tx,
                    announcers: announcers.into_iter().collect(),
                    taken_gen: gen,
                });
            }
        }
        if !stay.is_empty() {
            self.by_parent.insert(*parent, stay);
        }
        out
    }

    /// Core `Find1P1CPackage`: erase each of `peer`'s orphans of `parent`
    /// whose txid `pooled` already holds.
    pub fn erase_pooled_children(
        &mut self,
        parent: &Txid,
        peer: u64,
        pooled: impl Fn(&Txid) -> bool,
    ) {
        let Some(children) = self.by_parent.get(parent) else {
            return;
        };
        let gone: Vec<Wtxid> = children
            .iter()
            .filter(|w| {
                self.by_wtxid
                    .get(*w)
                    .is_some_and(|e| e.announcers.contains(&peer) && pooled(&e.txid))
            })
            .copied()
            .collect();
        for w in &gone {
            self.remove_wtxid(w);
        }
    }

    fn alloc_taken_gen(&mut self) -> u64 {
        self.next_taken_gen = self.next_taken_gen.wrapping_add(1);
        if self.next_taken_gen == 0 {
            self.next_taken_gen = 1;
        }
        self.next_taken_gen
    }

    /// Consume the in-flight record. `None` when this tx was not taken.
    pub(crate) fn claim_taken(&mut self, wtxid: &Wtxid) -> Option<TakenClaim> {
        let taken = self.taken.remove(wtxid)?;
        if taken.drop {
            Some(TakenClaim::Drop)
        } else {
            Some(TakenClaim::Keep(taken.announcers.into_iter().collect()))
        }
    }

    /// Drop the in-flight record when it is still the one `gen` inserted.
    ///
    /// A newer take of the same wtxid must survive. The repark already consumed
    /// its own record, so this only clears an admit or a reject that did not park.
    pub fn forget_taken_gen(&mut self, wtxid: &Wtxid, gen: u64) {
        if gen == 0 {
            return;
        }
        if self.taken.get(wtxid).is_some_and(|t| t.gen == gen) {
            self.taken.remove(wtxid);
        }
    }

    pub fn announcers_of(&self, wtxid: &Wtxid) -> Vec<u64> {
        self.by_wtxid
            .get(wtxid)
            .map(|e| e.announcers.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn has_announcer(&self, peer: u64) -> bool {
        self.by_wtxid.values().any(|e| e.announcers.contains(&peer))
            || self.taken.values().any(|t| t.announcers.contains(&peer))
    }

    pub fn add_announcer(&mut self, wtxid: &Wtxid, peer: u64) -> bool {
        let weight = {
            let Some(e) = self.by_wtxid.get_mut(wtxid) else {
                return false;
            };
            if !e.announcers.insert(peer) {
                return false;
            }
            e.weight
        };
        self.add_peer_weight(peer, weight);
        true
    }

    /// Drop `peer` as announcer; erase orphans with no remaining announcers.
    ///
    /// Also edits orphans that are out for a promote. The repark reads that.
    pub fn erase_for_peer(&mut self, peer: u64) {
        let hit: Vec<(Wtxid, u64, bool)> = self
            .by_wtxid
            .iter()
            .filter_map(|(wtxid, e)| {
                if !e.announcers.contains(&peer) {
                    return None;
                }
                Some((*wtxid, e.weight, e.announcers.len() == 1))
            })
            .collect();
        let mut drop = Vec::new();
        for (wtxid, weight, last) in hit {
            if let Some(e) = self.by_wtxid.get_mut(&wtxid) {
                e.announcers.remove(&peer);
            }
            self.sub_peer_weight(peer, weight);
            if last {
                drop.push(wtxid);
            }
        }
        for w in drop {
            self.remove_wtxid(&w);
        }
        for taken in self.taken.values_mut() {
            if taken.announcers.remove(&peer) && taken.announcers.is_empty() {
                taken.drop = true;
            }
        }
    }

    pub fn snapshot(&self) -> Vec<OrphanSnapshot> {
        let mut out = Vec::with_capacity(self.fifo.len());
        for wtxid in &self.fifo {
            if let Some(e) = self.by_wtxid.get(wtxid) {
                out.push(OrphanSnapshot {
                    tx: e.tx.clone(),
                    announcers: e.announcers.iter().copied().collect(),
                    taken_gen: 0,
                });
            }
        }
        out
    }

    /// Drop orphans that are themselves included in a confirmed block.
    ///
    /// Children of confirmed parents are **not** erased here — callers should
    /// `take_children_of` and re-accept (Core `AddChildrenToWorkSet` path).
    /// Spending a parent in the block does not invalidate the orphan; the parent
    /// create is now a chain UTXO.
    pub fn erase_for_block(&mut self, block_txids: &[Txid]) {
        let drop: Vec<Wtxid> = block_txids
            .iter()
            .filter_map(|t| self.by_txid.get(t))
            .flatten()
            .copied()
            .collect();
        for w in drop {
            self.remove_wtxid(&w);
        }
    }
}
