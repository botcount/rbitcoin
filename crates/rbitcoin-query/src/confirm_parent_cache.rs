//! Process-local confirm parent cache (header plans).
//!
//! **Header plans** (`headers` / `hash_to_height`): tip-GCed header + tx_fks.
//! Create outs / denserels / body_range are **pipeline-local** (plan `batch_pin`,
//! [`crate::BatchParents`]). Thin edges are batch-local.
//!
//! Header plans stay **always on** — multi-block wire prep needs tip-ahead
//! header plans for MTP / bits (mainnet tip freeze: `parent header plan
//! missing above tip`).

use crate::U32Map;
use rbitcoin_primitives::Fk;
use rbitcoin_store::HeaderRecord;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Cached header + body fk list for one cache height (avoids header.head/body
/// and header_txs page faults on confirm resolve).
#[derive(Debug)]
pub struct HeaderPlanCache {
    pub header_fk: Fk,
    pub header_rec: HeaderRecord,
    pub tx_fks: Vec<Fk>,
    /// Previous block hash (zeros at genesis). Filled at cache so wire rebuild
    /// never `store.get_header(prev_fk)`.
    pub prev_hash: [u8; 32],
}

#[doc(hidden)]
pub struct Inner {
    /// Highest confirmed tip we have pruned to.
    pub tip: u32,
    /// height → immutable header + tx list (Arc-publish; tip GC drops Arc).
    pub headers: U32Map<Arc<HeaderPlanCache>>,
    /// hash → height for O(1) header resolve on confirm.
    pub hash_to_height: HashMap<[u8; 32], u32>,
}

/// Process-local confirm parent cache (header plans).
///
/// **Always active** — multi-block wire prep needs tip-ahead header plans for
/// MTP / bits (mainnet tip freeze: `parent header plan missing above tip`).
pub struct ConfirmParentCache {
    pub inner: Mutex<Inner>,
}

impl Default for ConfirmParentCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmParentCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                tip: 0,
                headers: U32Map::default(),
                hash_to_height: HashMap::new(),
            }),
        }
    }

    /// Advance tip: drop headers at/below tip.
    ///
    /// Thin edges / sparse pins are batch-local. Called from load
    /// `on_load_pack` (polls store tip; write does not lock this cache).
    pub fn advance_tip(&self, tip: u32) {
        let mut g = self.inner.lock().unwrap();
        g.tip = tip;
        let drop_hdr: Vec<u32> = g.headers.keys().copied().filter(|h| *h <= tip).collect();
        for h in drop_hdr {
            if let Some(plan) = g.headers.remove(&h) {
                g.hash_to_height.remove(&plan.header_rec.hash);
            }
        }
    }

    /// Cache header + tx list for a cache height (tip-GCed; required for multi-block MTP).
    pub fn put_header_plan(
        &self,
        height: u32,
        header_fk: Fk,
        header_rec: HeaderRecord,
        tx_fks: Vec<Fk>,
        prev_hash: [u8; 32],
    ) {
        let mut g = self.inner.lock().unwrap();
        // Tip-GCed window only — never re-insert at/below tip (would stick until
        // a later advance and inflate conf_plans / RSS with full tx_fks vectors).
        if height <= g.tip {
            return;
        }
        let hash = header_rec.hash;
        // Replace supersedes prior hash at this height — drop stale reverse key.
        let stale_hash = g
            .headers
            .get(&height)
            .map(|old| old.header_rec.hash)
            .filter(|old_hash| *old_hash != hash);
        if let Some(old_hash) = stale_hash {
            g.hash_to_height.remove(&old_hash);
        }
        g.hash_to_height.insert(hash, height);
        g.headers.insert(
            height,
            Arc::new(HeaderPlanCache {
                header_fk,
                header_rec,
                tx_fks,
                prev_hash,
            }),
        );
    }

    pub fn get_header_by_hash(&self, hash: &[u8; 32]) -> Option<(Fk, HeaderRecord)> {
        let g = self.inner.lock().unwrap();
        let h = *g.hash_to_height.get(hash)?;
        let plan = g.headers.get(&h)?;
        Some((plan.header_fk, plan.header_rec.clone()))
    }

    /// The cached plan. The `Arc` is the one in the map, so a time or bits
    /// read does not copy `tx_fks`.
    pub fn get_header_plan(&self, height: u32) -> Option<Arc<HeaderPlanCache>> {
        self.inner.lock().unwrap().headers.get(&height).cloned()
    }

    pub fn get_tx_fks_for_hash(&self, hash: &[u8; 32]) -> Option<Vec<Fk>> {
        let g = self.inner.lock().unwrap();
        let h = *g.hash_to_height.get(hash)?;
        g.headers.get(&h).map(|p| p.tx_fks.clone())
    }

    /// Number of cached header + tx_fks plans (IBD `conf_plans=` occupancy).
    pub fn header_plan_count(&self) -> usize {
        self.inner.lock().unwrap().headers.len()
    }
}

impl crate::Query {
    pub fn advance_parent_cache_tip(&self, tip: u32) {
        self.confirm_parents.advance_tip(tip);
    }
}
