//! Load-thread in-flight create material (confirm pipeline).
//!
//! IBD load is sequential: stamp/pin read this map, **then**
//! [`InFlight::note_pins`] / [`InFlight::note_creates`] insert the current pack
//! so that stamp cannot see it. One HashMap for `txid → fk`. Outs are a dense
//! vec from the lowest live id; a hole stays empty. A height index drops keys
//! without scanning `creates`. Same-txid overwrite (BIP30) records a count on a
//! side map; prune is the current path when that map is empty.
//!
//! **Prune:** lookup snapshots [`rbitcoin_store::HeightFence::drain_and_fence_hi`]
//! before a wave's TipOnly read and passes it on the last load batch. After
//! that batch finishes its in-flight read, [`InFlight::prune_below_height`]
//! drops tagged packs with pack height **below** that snapshot (equality
//! keeps). Class C tip and `class_a_hi` are not drop gates. Disconnect still
//! [`InFlight::drop_from_height`] on pack height.

use crate::archive::CreatePin;
use crate::TxidHasher;
use rbitcoin_primitives::Fk;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;
use std::sync::Arc;

#[doc(hidden)]
pub type TxidFkMap = HashMap<[u8; 32], Fk, BuildHasherDefault<TxidHasher>>;
#[doc(hidden)]
pub type TxidEvictMap = HashMap<[u8; 32], u32, BuildHasherDefault<TxidHasher>>;

#[derive(Debug, Default)]
#[doc(hidden)]
pub struct HeightKeys {
    /// `(txid, fk)` as noted by this pack: removal only drops a `creates`
    /// entry this pack still owns (exact fk match), so packs can retire in
    /// any order (ascending prune, descending disconnect drop).
    pub txids: Vec<([u8; 32], Fk)>,
    pub out_ids: Vec<u64>,
    pub approx_bytes: u64,
    /// Lowest id in `out_ids`. A drop reads this before walking the vec.
    lowest_out: Option<u64>,
}

impl HeightKeys {
    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.txids.is_empty() && self.out_ids.is_empty()
    }

    /// Lowest create id noted on this pack.
    #[doc(hidden)]
    pub fn lowest_out(&self) -> Option<u64> {
        self.lowest_out
    }

    fn push_out(&mut self, id: u64) {
        self.out_ids.push(id);
        self.lowest_out = Some(match self.lowest_out {
            Some(cur) if cur < id => cur,
            _ => id,
        });
    }

    fn absorb_lowest(&mut self, low: Option<u64>) {
        let Some(id) = low else {
            return;
        };
        self.lowest_out = Some(match self.lowest_out {
            Some(cur) if cur < id => cur,
            _ => id,
        });
    }
}

/// Create pins from the lowest live id. A middle id with no pin stays `None`.
///
/// `start` is the first slot still in the window. A prefix drop advances it
/// and leaves the dead slots in place, so one pin does not move the tail.
/// The tail is moved once the dead prefix is at least as long as what remains.
#[derive(Debug, Default)]
struct OutWindow {
    base: Option<u64>,
    slots: Vec<Option<CreatePin>>,
    start: usize,
    occupied: usize,
}

impl OutWindow {
    fn len(&self) -> usize {
        self.occupied
    }

    fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    fn contains(&self, id: u64) -> bool {
        self.get(id).is_some()
    }

    fn get(&self, id: u64) -> Option<&CreatePin> {
        let physical = self.physical(id)?;
        self.slots.get(physical)?.as_ref()
    }

    fn physical(&self, id: u64) -> Option<usize> {
        let base = self.base?;
        let index = id.checked_sub(base)? as usize;
        self.start.checked_add(index)
    }

    fn insert(&mut self, id: u64, pin: CreatePin) {
        let Some(base) = self.base else {
            self.base = Some(id);
            self.slots.push(Some(pin));
            self.start = 0;
            self.occupied = 1;
            return;
        };
        if id < base {
            let gap = (base - id) as usize;
            if gap <= self.start {
                self.start -= gap;
                self.slots[self.start] = Some(pin);
            } else {
                self.compact();
                self.slots.splice(0..0, (0..gap).map(|_| None));
                self.slots[0] = Some(pin);
                self.start = 0;
            }
            self.base = Some(id);
            self.occupied += 1;
            return;
        }
        let physical = self.start + (id - base) as usize;
        if physical >= self.slots.len() {
            self.slots.resize_with(physical + 1, || None);
        }
        if self.slots[physical].is_none() {
            self.occupied += 1;
        }
        self.slots[physical] = Some(pin);
    }

    fn take(&mut self, id: u64) -> Option<CreatePin> {
        let physical = self.physical(id)?;
        let pin = self.slots.get_mut(physical)?.take()?;
        self.occupied -= 1;
        if self.occupied == 0 {
            self.base = None;
            self.slots.clear();
            self.start = 0;
            return Some(pin);
        }
        while self.start < self.slots.len() && self.slots[self.start].is_none() {
            self.start += 1;
            if let Some(base) = self.base.as_mut() {
                *base = base.saturating_add(1);
            }
        }
        while self.slots.len() > self.start && matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
        }
        let live = self.slots.len() - self.start;
        if self.start > 0 && self.start >= live {
            self.compact();
        }
        Some(pin)
    }

    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        self.slots.drain(..self.start);
        self.start = 0;
    }
}

/// Load-owned map of prior uncommitted creates (O(1) get; drop by pack height).
#[derive(Debug, Default)]
pub struct InFlight {
    pub creates: TxidFkMap,
    outs: OutWindow,
    pub by_height: BTreeMap<u32, HeightKeys>,
    pub untagged: HeightKeys,
    /// Count of older packs still listing a txid after a later `creates` clobber.
    /// Empty on the IBD hot path; prune skips the per-key check when empty.
    pub evictions: TxidEvictMap,
    pub approx_bytes: u64,
    /// Lowest create id still in `outs`. Pin does not call [`Self::get_out`] below it.
    min_out: Option<u64>,
}

/// Lowest id on `keys` that is still in `outs`.
fn live_lowest(outs: &OutWindow, keys: &HeightKeys) -> Option<u64> {
    if let Some(id) = keys.lowest_out() {
        if outs.contains(id) {
            return Some(id);
        }
    }
    keys.out_ids
        .iter()
        .copied()
        .filter(|id| outs.contains(*id))
        .min()
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert planned fks + CreatePin outs. `height` is the pack's max height
    /// ([`None`] = untagged: disconnect and prune keep it).
    pub fn note_pins<'a>(
        &mut self,
        pins: impl IntoIterator<Item = (Fk, &'a CreatePin)>,
        height: Option<u32>,
    ) {
        let mut keys = HeightKeys::default();
        for (fk, pin) in pins {
            let Some(id) = fk.get() else {
                continue;
            };
            self.note_create_fk(pin.tx().txid, fk);
            self.outs.insert(id, Arc::clone(pin));
            keys.push_out(id);
            self.min_out = Some(match self.min_out {
                Some(min) if min < id => min,
                _ => id,
            });
            let pin_bytes =
                40u64.saturating_add(crate::archive::create_pin_approx_bytes(pin) as u64);
            keys.approx_bytes = keys.approx_bytes.saturating_add(pin_bytes);
            self.approx_bytes = self.approx_bytes.saturating_add(pin_bytes);
        }
        self.commit_keys(keys, height);
    }

    /// Creates-only rows (txid→fk, no outs) for already-archived packs.
    pub fn note_creates(
        &mut self,
        pairs: impl IntoIterator<Item = ([u8; 32], Fk)>,
        height: Option<u32>,
    ) {
        let mut keys = HeightKeys::default();
        for (txid, fk) in pairs {
            self.note_create_fk(txid, fk);
            keys.txids.push((txid, fk));
        }
        self.commit_keys(keys, height);
    }

    #[doc(hidden)]
    pub fn commit_keys(&mut self, keys: HeightKeys, height: Option<u32>) {
        if keys.is_empty() {
            return;
        }
        let slot = match height {
            Some(h) => self.by_height.entry(h).or_default(),
            None => &mut self.untagged,
        };
        slot.approx_bytes = slot.approx_bytes.saturating_add(keys.approx_bytes);
        slot.txids.extend(keys.txids);
        slot.out_ids.extend(keys.out_ids);
        slot.absorb_lowest(keys.lowest_out);
    }

    #[doc(hidden)]
    pub fn note_create_fk(&mut self, txid: [u8; 32], fk: Fk) {
        match self.creates.insert(txid, fk) {
            None => {
                self.approx_bytes = self.approx_bytes.saturating_add(40);
            }
            Some(old) if old != fk => {
                *self.evictions.entry(txid).or_insert(0) += 1;
            }
            Some(_) => {}
        }
    }

    #[doc(hidden)]
    pub fn consume_eviction(&mut self, tid: &[u8; 32]) -> bool {
        let Some(n) = self.evictions.get_mut(tid) else {
            return false;
        };
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.evictions.remove(tid);
        }
        true
    }

    fn retire_create(&mut self, txid: [u8; 32], fk: Fk, careful: bool) {
        if !careful {
            if self.creates.remove(&txid).is_some() {
                self.approx_bytes = self.approx_bytes.saturating_sub(40);
            }
            return;
        }
        if self.creates.get(&txid) == Some(&fk) {
            self.creates.remove(&txid);
            self.approx_bytes = self.approx_bytes.saturating_sub(40);
        } else {
            self.consume_eviction(&txid);
        }
    }

    #[doc(hidden)]
    pub fn remove_keys(&mut self, keys: HeightKeys) {
        // One choice for the whole pack. A mid-loop consume must not flip
        // the next txid onto the unconditional delete.
        let careful = !self.evictions.is_empty();
        for (tid, fk) in &keys.txids {
            self.retire_create(*tid, *fk, careful);
        }
        for id in &keys.out_ids {
            let Some(pin) = self.outs.take(*id) else {
                continue;
            };
            let txid = pin.tx().txid;
            self.retire_create(txid, Fk(*id), careful);
            self.approx_bytes = self
                .approx_bytes
                .saturating_sub(40)
                .saturating_sub(crate::archive::create_pin_approx_bytes(&pin) as u64);
        }
        if self.min_out.is_some_and(|id| !self.outs.contains(id)) {
            self.recompute_min_out();
        }
    }

    /// Lowest id still in `outs` on a surviving height, including untagged.
    ///
    /// Each pack stores its lowest id. That id is one lookup. The pack's id
    /// list is walked only when the stored id was removed with another pack.
    fn recompute_min_out(&mut self) {
        let mut min: Option<u64> = None;
        let mut consider = |id: Option<u64>| {
            if let Some(id) = id {
                min = Some(min.map_or(id, |cur| cur.min(id)));
            }
        };
        for keys in self.by_height.values() {
            consider(live_lowest(&self.outs, keys));
        }
        consider(live_lowest(&self.outs, &self.untagged));
        self.min_out = min;
    }

    /// Drop tagged packs at or above a disconnected height. Untagged stay.
    pub fn drop_from_height(&mut self, height: u32) {
        let drop = self.by_height.split_off(&height);
        for keys in drop.into_values() {
            self.remove_keys(keys);
        }
    }

    /// True when a [`Self::prune_below_height`] at `hi` would drop a tagged pack.
    ///
    /// `None` keeps every pack. Equality keeps, matching the prune.
    pub fn has_tagged_below(&self, hi: Option<u32>) -> bool {
        let Some(h) = hi else {
            return false;
        };
        self.by_height.range(..h).any(|(_, keys)| !keys.is_empty())
    }

    /// Drop tagged packs with height below `hi`.
    ///
    /// `None` hi keeps every pack. Untagged stay. Equality keeps
    /// (`hi == pack height`). Load calls this after the last batch of a lookup
    /// wave finishes its in-flight read, with the drain+fence height
    /// snapshotted before that wave's TipOnly.
    pub fn prune_below_height(&mut self, hi: Option<u32>) {
        let Some(h) = hi else {
            return;
        };
        let keep = self.by_height.split_off(&h);
        let drop = std::mem::replace(&mut self.by_height, keep);
        for keys in drop.into_values() {
            self.remove_keys(keys);
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn is_empty(&self) -> bool {
        self.creates.is_empty() && self.outs.is_empty()
    }

    pub fn pack_count(&self) -> usize {
        self.by_height.len() + usize::from(!self.untagged.is_empty())
    }

    pub fn entry_count(&self) -> usize {
        self.outs.len()
    }

    /// Occupancy for IBD `sizes`: height buckets, create-pin entries, approx bytes.
    pub fn size_snapshot(&self) -> (usize, usize, u64) {
        (self.pack_count(), self.outs.len(), self.approx_bytes)
    }

    /// Lowest create id in the out window, if any.
    #[inline]
    pub fn min_out_id(&self) -> Option<u64> {
        self.min_out
    }

    /// Lowest id the out window still addresses. `None` when no pin is live.
    #[doc(hidden)]
    #[inline]
    pub fn out_base(&self) -> Option<u64> {
        self.outs.base
    }

    #[inline]
    pub fn get_out(&self, id: u64) -> Option<&CreatePin> {
        self.outs.get(id)
    }

    #[inline]
    pub fn get_create_fk(&self, txid: &[u8; 32]) -> Option<Fk> {
        self.creates.get(txid).copied()
    }
}
