//! In-memory wire block cache for P2P serve and ordered sync.
//!
//! Full block bodies are kept only for a recent tip window (default
//! [`DEFAULT_BODY_DEPTH`], compact/`getblocktxn` serve). Best-chain **hashes**
//! are retained for locator construction without holding decoded IBD history.

use bitcoin::block::{Block, Header};
use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use std::collections::HashMap;
use std::sync::RwLock;

/// How many recent full block bodies to retain (compact/`getblocktxn` serve
/// depths are 5 and 10; hashes stay for locators).
pub const DEFAULT_BODY_DEPTH: usize = 16;

#[derive(Debug)]
pub struct BlockCache {
    pub inner: RwLock<Inner>,
    /// Max full bodies in `by_hash` (hashes in `chain` are always kept).
    pub body_depth: usize,
}

#[derive(Debug, Default)]
#[doc(hidden)]
pub struct Inner {
    pub by_hash: HashMap<BlockHash, Block>,
    /// Best-chain hashes, contiguous: `chain[i]` is at height `base + i`.
    pub chain: Vec<BlockHash>,
    /// Height of `chain[0]`. A reopened store starts the list at its first
    /// connect, not at genesis.
    base: u32,
    /// Unconnected bodies a compact announce may serve. Not best-chain.
    servable: HashMap<BlockHash, Block>,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self::with_body_depth(DEFAULT_BODY_DEPTH)
    }
}

impl BlockCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_body_depth(body_depth: usize) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            body_depth: body_depth.max(1),
        }
    }

    pub fn tip_height(&self) -> Option<u32> {
        self.inner.read().unwrap().tip_height()
    }

    pub fn tip_hash(&self) -> Option<BlockHash> {
        self.inner.read().unwrap().chain.last().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().chain.is_empty()
    }

    pub fn stored_bodies(&self) -> usize {
        self.inner.read().unwrap().by_hash.len()
    }

    pub fn get_block(&self, hash: &BlockHash) -> Option<Block> {
        self.inner.read().unwrap().by_hash.get(hash).cloned()
    }

    /// Whether the cache retains the block body, without copying it.
    #[doc(hidden)]
    pub fn has_body(&self, hash: &BlockHash) -> bool {
        self.inner.read().unwrap().by_hash.contains_key(hash)
    }

    pub fn get_header(&self, hash: &BlockHash) -> Option<Header> {
        self.inner
            .read()
            .unwrap()
            .by_hash
            .get(hash)
            .map(|b| b.header)
    }

    pub fn hash_at_height(&self, height: u32) -> Option<BlockHash> {
        self.inner.read().unwrap().hash_at_height(height)
    }

    pub fn header_at_height(&self, height: u32) -> Option<Header> {
        let g = self.inner.read().unwrap();
        let h = g.hash_at_height(height)?;
        g.by_hash.get(&h).map(|b| b.header)
    }

    /// Drop all blocks above `height`. No-op if the cache tip is not above it.
    pub fn truncate_to_height(&self, height: u32) {
        self.inner.write().unwrap().truncate_to_height(height);
    }

    /// Clear entire cache (e.g. after full reorg from genesis).
    pub fn clear(&self) {
        let mut g = self.inner.write().unwrap();
        g.by_hash.clear();
        g.chain.clear();
        g.servable.clear();
    }

    /// Park a body so `getblocktxn` can serve it before connect. Best-chain
    /// lookup does not see this map.
    pub fn stage_servable(&self, block: Block) {
        let hash = block.block_hash();
        self.inner.write().unwrap().servable.insert(hash, block);
    }

    pub fn servable(&self, hash: &BlockHash) -> Option<Block> {
        self.inner.read().unwrap().servable.get(hash).cloned()
    }

    pub fn drop_servable(&self, hash: &BlockHash) {
        self.inner.write().unwrap().servable.remove(hash);
    }

    /// Record `block` as the best-chain block at `height`.
    ///
    /// Anything cached at or above `height` is dropped first. When the cached
    /// block at `height - 1` is not its parent, the list restarts at `height`,
    /// so the cache never names a block the store did not connect there.
    /// Evicts full bodies older than `body_depth` while keeping the hash chain.
    pub fn push_best(&self, height: u32, block: Block) {
        let hash = block.block_hash();
        let mut g = self.inner.write().unwrap();
        let parent = height.checked_sub(1).and_then(|p| g.hash_at_height(p));
        if parent == Some(block.header.prev_blockhash) {
            g.truncate_to_height(height - 1);
        } else {
            g.by_hash.clear();
            g.chain.clear();
            g.base = height;
        }
        g.by_hash.insert(hash, block);
        g.chain.push(hash);
        let extra = g.chain.len().saturating_sub(self.body_depth);
        if extra != 0 {
            let stale: Vec<BlockHash> = g.chain[..extra].to_vec();
            for h in stale {
                g.by_hash.remove(&h);
            }
        }
    }

    /// Headers after the best common locator hash, up to Core `MAX_HEADERS_RESULTS` (2000).
    ///
    /// Only returns headers still present as full bodies in the tip window; callers
    /// that need deeper history should use the store reconstruct path.
    pub fn headers_after_locator(&self, locator: &[BlockHash], stop: BlockHash) -> Vec<Header> {
        let g = self.inner.read().unwrap();
        if g.chain.is_empty() {
            return Vec::new();
        }
        let mut start = 0usize;
        'outer: for loc in locator {
            for (i, h) in g.chain.iter().enumerate() {
                if h == loc {
                    start = i + 1;
                    break 'outer;
                }
            }
        }
        let mut out = Vec::new();
        for h in g
            .chain
            .iter()
            .skip(start)
            .take(crate::codec::MAX_HEADERS_RESULTS)
        {
            if let Some(b) = g.by_hash.get(h) {
                out.push(b.header);
                if *h == stop && stop.to_byte_array() != [0u8; 32] {
                    break;
                }
            }
        }
        out
    }
}

impl Inner {
    fn tip_height(&self) -> Option<u32> {
        let top = u32::try_from(self.chain.len().checked_sub(1)?).ok()?;
        Some(self.base.saturating_add(top))
    }

    fn hash_at_height(&self, height: u32) -> Option<BlockHash> {
        let i = height.checked_sub(self.base)?;
        self.chain.get(i as usize).copied()
    }

    fn truncate_to_height(&mut self, height: u32) {
        let keep = match height.checked_sub(self.base) {
            Some(i) => (i as usize).saturating_add(1),
            None => 0,
        };
        if self.chain.len() <= keep {
            return;
        }
        for h in self.chain.split_off(keep) {
            self.by_hash.remove(&h);
        }
    }
}
