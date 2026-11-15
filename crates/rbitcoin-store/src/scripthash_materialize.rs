//! Two-scan SH tip extract: unique key16 per shard, then fuse-hit postings.
//!
//! Pass 1: each worker owns a contiguous create-fk span and `n_shards` unsized
//! identity maps (`key16 → pack8 word`; `0` = multi). Cap is
//! [`SH_EXTRACT_WORKER_RAM_BYTES`] (1.5 GiB estimate, 64 B/key). After each
//! 64 k-fk loc/body batch, spill the largest shard map while over budget
//! (`SHKSP01` under `keys/NN/`, one writer, 1-slot queue). Status is
//! `scanned=` finished fks. Merge folds those spill files into one map,
//! one walk to `scripthash.head/NN` and `multi/NN.fuse8`, then unlinks
//! `keys/NN/`. A `keys/NN` file with no valid `DONE.keys` is wiped with the
//! previous layout. A spill whose magic is not `SHKSP01` refuses.
//! Pass 2 keeps fuse8 only (no BDZ): same static spans and per-worker maps
//! (`key16 → Vec<fk>`, estimate `80n+8f`), spill-largest as `SHPST01` under
//! `post/NN/`. Pack folds those spills, then `slot_for_key16` + 2+ bodies;
//! 1-fk after fold is `fp_singles`. A `post/NN` file refuses.

use crate::error::StoreError;
use crate::fuse8_filter::SealedFuse8;
use crate::scripthash::{sorted_main_shard_path, ColdProgress, ScriptHashTable, ShShardPack};
use crate::scripthash_head::prefix_shard_of;
use crate::scripthash_layout::{head_key_from_full, ShHeadKey, SH_HEAD_KEY_LEN};
use crate::scripthash_mphf::{mix_key16, mphf_path, val_path, MphfHead};
use crate::store::Store;
use crate::tx_table::TxTable;
use rbitcoin_primitives::{le_u32, le_u64, read_uleb128, write_uleb128, Fk};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[doc(hidden)]
pub const MATERIALIZE_STATUS_INTERVAL: Duration = Duration::from_secs(10);

pub const UNSORTED_SHARD_DIR: &str = "scripthash.unsorted";
#[doc(hidden)]
pub const KEYS_SUBDIR: &str = "keys";
#[doc(hidden)]
pub const POST_SUBDIR: &str = "post";
#[doc(hidden)]
pub const MULTI_SUBDIR: &str = "multi";
#[cfg(debug_assertions)]
#[doc(hidden)]
pub const MPHF_SUBDIR: &str = "mphf";
#[doc(hidden)]
pub const KEYS_DONE_NAME: &str = "DONE.keys";
#[doc(hidden)]
pub const POST_DONE_NAME: &str = "DONE.post";
#[doc(hidden)]
pub const KEYS_DONE_MAGIC: &[u8; 8] = b"SHKEYS02";
#[doc(hidden)]
pub const POST_DONE_MAGIC: &[u8; 8] = b"SHPOST02";
#[doc(hidden)]
pub const LEGACY_DONE_NAME: &str = "DONE";

#[doc(hidden)]
pub const KEY16_LEN: usize = SH_HEAD_KEY_LEN;
/// One keys-body codec. Anything else is leftover (refuse, do not open).
#[doc(hidden)]
pub const KEYS_SPILL_MAGIC: &[u8; 8] = b"SHKSP01\0";
#[doc(hidden)]
pub const KEYS_SPILL_HDR: usize = 16;
/// Collect-map estimate: `64 × n_keys` (HashMap overhead, not a prefault).
#[doc(hidden)]
pub const KEYS_COLLECT_BYTES_PER_KEY: u64 = 64;
#[doc(hidden)]
pub const INDEX_REFUSE_KEYS_LEFTOVER: &str = "scripthash unsorted keys leftover from an older extract; wipe store/scripthash.unsorted and rematerialize";
#[doc(hidden)]
pub const INDEX_REFUSE_POST_LEFTOVER: &str = "scripthash unsorted post leftover from an older extract; wipe store/scripthash.unsorted and rematerialize";
/// Inner loc/body batch inside a static worker fk span.
pub const CLASS_A_CHUNK_FKS: u64 = 1 << 16;
/// One posts-body codec. Anything else is leftover (refuse, do not open).
#[doc(hidden)]
pub const POSTS_SPILL_MAGIC: &[u8; 8] = b"SHPST01\0";
#[doc(hidden)]
pub const POSTS_SPILL_HDR: usize = 12;
/// Collect-map estimate: `80 × n_keys + 8 × n_fks`.
#[doc(hidden)]
pub const POST_MAP_KEY_BYTES: usize = 80;
#[doc(hidden)]
pub const POST_MAP_FK_BYTES: usize = 8;

/// One extract worker: BDZ `g` + fuse + maps (~1.5 GiB).
pub const SH_EXTRACT_WORKER_RAM_BYTES: u64 = 3 << 29;

#[doc(hidden)]
pub struct MaterializeProgress {
    pub recs_packed: AtomicU64,
    pub keys_packed: AtomicU64,
    pub shards_published: AtomicU32,
    pub creates_published: AtomicU64,
    pub merge_ns: AtomicU64,
    pub pack_ns: AtomicU64,
    pub mphf_ns: AtomicU64,
    pub body_flush_ns: AtomicU64,
}

impl MaterializeProgress {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            recs_packed: AtomicU64::new(0),
            keys_packed: AtomicU64::new(0),
            shards_published: AtomicU32::new(0),
            creates_published: AtomicU64::new(0),
            merge_ns: AtomicU64::new(0),
            pack_ns: AtomicU64::new(0),
            mphf_ns: AtomicU64::new(0),
            body_flush_ns: AtomicU64::new(0),
        }
    }

    #[doc(hidden)]
    pub fn stages(&self) -> MaterializeStageNs {
        MaterializeStageNs {
            merge_ns: self.merge_ns.load(Ordering::Relaxed),
            pack_ns: self.pack_ns.load(Ordering::Relaxed),
            mphf_ns: self.mphf_ns.load(Ordering::Relaxed),
            body_flush_ns: self.body_flush_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MaterializeStageNs {
    pub merge_ns: u64,
    pub pack_ns: u64,
    pub mphf_ns: u64,
    pub body_flush_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ShShardMaterialize {
    pub creates: u64,
    pub keys: u64,
    pub max_fk: u64,
    pub merge_ns: u64,
    pub pack_ns: u64,
    pub mphf_ns: u64,
    pub body_flush_ns: u64,
    pub head_fill_ns: u64,
}

impl ShShardMaterialize {
    #[doc(hidden)]
    pub fn with_stages(mut self, stages: MaterializeStageNs) -> Self {
        self.merge_ns = stages.merge_ns;
        self.pack_ns = stages.pack_ns;
        self.mphf_ns = stages.mphf_ns;
        self.body_flush_ns = stages.body_flush_ns;
        self.head_fill_ns = 0;
        self
    }
}

#[doc(hidden)]
pub fn check_cancel(cancel: Option<&AtomicBool>, what: &'static str) -> Result<(), StoreError> {
    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
        Err(StoreError::Cancelled(what))
    } else {
        Ok(())
    }
}

pub fn unsorted_shard_dir(store_dir: &Path) -> PathBuf {
    store_dir.join(UNSORTED_SHARD_DIR)
}

pub fn unsorted_shard_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(format!("{shard:02x}"))
}

pub fn unsorted_keys_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(KEYS_SUBDIR).join(format!("{shard:02x}"))
}

pub fn unsorted_keys_spill_path(dir: &Path, shard: usize, seq: u32) -> PathBuf {
    unsorted_keys_path(dir, shard).join(format!("{seq:06}"))
}

pub fn unsorted_post_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(POST_SUBDIR).join(format!("{shard:02x}"))
}

pub fn unsorted_post_spill_path(dir: &Path, shard: usize, seq: u32) -> PathBuf {
    unsorted_post_path(dir, shard).join(format!("{seq:06}"))
}

pub fn unsorted_multi_fuse_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(MULTI_SUBDIR).join(format!("{shard:02x}.fuse8"))
}

#[cfg(debug_assertions)]
pub fn unsorted_mphf_base(dir: &Path, shard: usize) -> PathBuf {
    dir.join(MPHF_SUBDIR).join(format!("{shard:02x}"))
}

pub fn unsorted_collect_workers() -> usize {
    sh_extract_workers()
}

/// `min(nCPU, max(1, free_RAM / 1.5 GiB))`. `RBITCOIN_SH_MERGE_WORKERS` overrides.
pub fn sh_extract_workers() -> usize {
    if let Ok(s) = std::env::var("RBITCOIN_SH_MERGE_WORKERS") {
        if let Ok(n) = s.parse::<usize>() {
            return n.clamp(1, 256);
        }
    }
    crate::sorted_run::workers_for_free_ram(
        crate::sorted_run::logical_cpus(),
        crate::sorted_run::mem_available_bytes().unwrap_or(0),
        SH_EXTRACT_WORKER_RAM_BYTES,
    )
}

pub fn unsorted_pack_workers() -> usize {
    sh_extract_workers()
}

pub fn clear_unsorted_shard_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[derive(Debug, Clone)]
pub struct UnsortedCollect {
    pub recs: u64,
    pub last_fk: u64,
    pub per_shard: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum UnsortedCollectAction {
    Full,
    Append { first: u64, last: u64 },
    Skip,
}

#[doc(hidden)]
pub fn plan_unsorted_collect(
    done_last: Option<u64>,
    class_a_last: u64,
    any_sealed_shards: bool,
    keys_present: bool,
) -> UnsortedCollectAction {
    match done_last {
        None => UnsortedCollectAction::Full,
        Some(d) if d >= class_a_last => UnsortedCollectAction::Skip,
        Some(_) if any_sealed_shards => UnsortedCollectAction::Skip,
        Some(_) if !keys_present => UnsortedCollectAction::Full,
        Some(d) => UnsortedCollectAction::Append {
            first: d.saturating_add(1).max(1),
            last: class_a_last,
        },
    }
}

#[doc(hidden)]
pub fn any_unsorted_keys(dir: &Path, n_shards: usize) -> bool {
    (0..n_shards).any(|si| {
        let p = unsorted_keys_path(dir, si);
        p.is_file() || p.is_dir()
    })
}

#[doc(hidden)]
pub struct PhaseDone {
    pub last_fk: u64,
    pub counts: Vec<u64>,
}

#[doc(hidden)]
pub fn write_phase_done(
    dir: &Path,
    name: &str,
    magic: &[u8; 8],
    last_fk: u64,
    per_shard: &[u64],
) -> Result<(), StoreError> {
    let n = per_shard.len() as u32;
    let mut buf = Vec::with_capacity(20 + per_shard.len() * 8);
    buf.extend_from_slice(magic);
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&last_fk.to_le_bytes());
    for c in per_shard {
        buf.extend_from_slice(&c.to_le_bytes());
    }
    let tmp = dir.join(format!("{name}.tmp"));
    let dst = dir.join(name);
    fs::write(&tmp, &buf).map_err(|e| StoreError::io(&tmp, e))?;
    {
        let f = OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(|e| StoreError::io(&tmp, e))?;
        f.sync_all().map_err(|e| StoreError::io(&tmp, e))?;
    }
    fs::rename(&tmp, &dst).map_err(|e| StoreError::io(&dst, e))?;
    Ok(())
}

#[doc(hidden)]
pub fn read_phase_done(
    dir: &Path,
    name: &str,
    magic: &[u8; 8],
    n_shards: usize,
) -> Option<PhaseDone> {
    let buf = fs::read(dir.join(name)).ok()?;
    if buf.len() < 20 || &buf[0..8] != magic {
        return None;
    }
    let mut n_bytes = [0u8; 4];
    n_bytes.copy_from_slice(&buf[8..12]);
    let n = le_u32!(n_bytes, 0) as usize;
    if n != n_shards {
        return None;
    }
    let mut last = [0u8; 8];
    last.copy_from_slice(&buf[12..20]);
    let last_fk = le_u64!(last, 0);
    if buf.len() != 20 + n * 8 {
        return None;
    }
    let mut counts = Vec::with_capacity(n);
    for row in buf[20..].as_chunks::<8>().0 {
        counts.push(le_u64!(row, 0));
    }
    Some(PhaseDone { last_fk, counts })
}

pub fn unsorted_done_last_fk(dir: &Path, n_shards: usize) -> Option<u64> {
    read_phase_done(dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, n_shards).map(|d| d.last_fk)
}

#[doc(hidden)]
pub fn leftover_legacy_unsorted(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if dir.join(LEGACY_DONE_NAME).is_file() {
        return true;
    }
    for i in 0..256 {
        if unsorted_shard_path(dir, i).is_file() {
            return true;
        }
        if unsorted_keys_path(dir, i).is_file() {
            return true;
        }
        if unsorted_post_path(dir, i).is_file() {
            return true;
        }
    }
    false
}

#[doc(hidden)]
pub fn chunk_ranges(first: u64, last: u64, chunk: u64) -> Vec<(u64, u64)> {
    if last < first {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = first;
    while cur <= last {
        let hi = cur.saturating_add(chunk.saturating_sub(1)).min(last);
        out.push((cur, hi));
        if hi == last {
            break;
        }
        cur = hi.saturating_add(1);
    }
    out
}

/// Contiguous, covering, disjoint create-fk spans. Remainder on the last span.
/// `n_workers` clamps to `[1, span]`.
#[doc(hidden)]
pub fn worker_fk_spans(first: u64, last: u64, n_workers: usize) -> Vec<(u64, u64)> {
    if last < first {
        return Vec::new();
    }
    let span = last.saturating_sub(first).saturating_add(1);
    let n = (n_workers.max(1) as u64).min(span);
    let base = span / n;
    let rem = span % n;
    let mut out = Vec::with_capacity(n as usize);
    let mut cur = first;
    for i in 0..n {
        let len = if i + 1 == n {
            base.saturating_add(rem)
        } else {
            base
        };
        let hi = cur.saturating_add(len.saturating_sub(1));
        out.push((cur, hi));
        cur = hi.saturating_add(1);
    }
    out
}

/// Finished create-fks, not the max fk. A late span must not look like the scan is done.
#[doc(hidden)]
pub fn note_scanned_fks(scanned: &AtomicU64, lo: u64, hi: u64) -> u64 {
    let n = hi.saturating_sub(lo).saturating_add(1);
    scanned.fetch_add(n, Ordering::Relaxed).saturating_add(n)
}

#[doc(hidden)]
pub fn key16_halves_bytes(bytes: &[u8; KEY16_LEN]) -> (u64, u64) {
    (le_u64!(bytes, 0), le_u64!(bytes, 8))
}

/// Identity hasher for uniform key16: `finish()` is the xor of the two LE u64 halves.
/// Does not FNV-mix bytes ([`crate::int_map::U64IdentityHasher::write`]).
#[derive(Default, Clone, Copy)]
#[doc(hidden)]
pub struct Key16IdentityHasher(u64);

impl Hasher for Key16IdentityHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() == KEY16_LEN {
            let mut key = [0u8; KEY16_LEN];
            key.copy_from_slice(bytes);
            let (a, b) = key16_halves_bytes(&key);
            self.0 = a ^ b;
        }
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        if i > KEY16_LEN as u64 {
            self.0 = i;
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[doc(hidden)]
pub type Key16PackMap = HashMap<ShHeadKey, u64, BuildHasherDefault<Key16IdentityHasher>>;
#[doc(hidden)]
pub type KeysSpillJob = (usize, Key16PackMap);
#[doc(hidden)]
pub type KeysSpillTx = SyncSender<KeysSpillJob>;
#[doc(hidden)]
pub type KeysSpillRx = Receiver<KeysSpillJob>;

#[doc(hidden)]
pub fn keys_map_estimated_bytes(map: &Key16PackMap) -> u64 {
    (map.len() as u64).saturating_mul(KEYS_COLLECT_BYTES_PER_KEY)
}

#[doc(hidden)]
pub fn keys_maps_estimated_bytes(maps: &[Key16PackMap]) -> u64 {
    maps.iter().map(keys_map_estimated_bytes).sum()
}

#[doc(hidden)]
pub fn largest_keys_shard(maps: &[Key16PackMap]) -> Option<usize> {
    maps.iter()
        .enumerate()
        .filter(|(_, m)| !m.is_empty())
        .max_by_key(|(_, m)| m.len())
        .map(|(i, _)| i)
}

#[doc(hidden)]
pub fn refuse_keys_leftover() -> StoreError {
    StoreError::Corrupt(INDEX_REFUSE_KEYS_LEFTOVER)
}

/// Vacant stores `word`; occupied nonzero becomes `0` (multi) unless `word` matches.
#[doc(hidden)]
pub fn insert_key_word(map: &mut Key16PackMap, key: ShHeadKey, word: u64) {
    match map.entry(key) {
        Entry::Vacant(v) => {
            v.insert(word);
        }
        Entry::Occupied(mut o) => {
            if *o.get() != 0 && (*o.get() != word || word == 0) {
                o.insert(0);
            }
        }
    }
}

#[doc(hidden)]
pub struct CollectTally {
    pub recs: u64,
    pub hits: u64,
    pub per: Vec<u64>,
}

impl CollectTally {
    #[doc(hidden)]
    pub fn new(n_shards: usize) -> Self {
        Self {
            recs: 0,
            hits: 0,
            per: vec![0; n_shards],
        }
    }
}

/// Add a worker's batch counts once. A zero tally does not touch the atomics.
#[doc(hidden)]
pub fn flush_collect_tally(
    tally: &mut CollectTally,
    recs: &AtomicU64,
    hits: Option<&AtomicU64>,
    per: Option<&[AtomicU64]>,
) {
    if tally.recs != 0 {
        recs.fetch_add(tally.recs, Ordering::Relaxed);
        tally.recs = 0;
    }
    if tally.hits != 0 {
        if let Some(hits) = hits {
            hits.fetch_add(tally.hits, Ordering::Relaxed);
        }
        tally.hits = 0;
    }
    if let Some(per) = per {
        for (slot, n) in per.iter().zip(tally.per.iter_mut()) {
            if *n != 0 {
                slot.fetch_add(*n, Ordering::Relaxed);
                *n = 0;
            }
        }
    }
}

#[doc(hidden)]
pub fn insert_key_pack(map: &mut Key16PackMap, key: ShHeadKey, fk: Fk) {
    if fk.is_null() {
        return;
    }
    insert_key_word(map, key, fk.0);
}

#[doc(hidden)]
pub fn encode_keys_spill(map: &Key16PackMap) -> Vec<u8> {
    let mut multis = Vec::new();
    let mut singles = Vec::new();
    for (k, w) in map {
        if *w == 0 {
            multis.push(*k);
        } else {
            singles.push((*w, *k));
        }
    }
    singles.sort_unstable_by_key(|a| a.0);
    let mut bytes = Vec::with_capacity(
        KEYS_SPILL_HDR + (multis.len() + singles.len()).saturating_mul(KEY16_LEN + 8),
    );
    bytes.extend_from_slice(KEYS_SPILL_MAGIC);
    bytes.extend_from_slice(&(multis.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(singles.len() as u32).to_le_bytes());
    for k in &multis {
        bytes.extend_from_slice(k);
    }
    let mut prev = 0u64;
    for &(fk, k) in &singles {
        write_uleb128(&mut bytes, fk.saturating_sub(prev));
        bytes.extend_from_slice(&k);
        prev = fk;
    }
    bytes
}

#[doc(hidden)]
pub fn fold_keys_spill_bytes(map: &mut Key16PackMap, bytes: &[u8]) -> Result<(), StoreError> {
    let Some(head) = bytes.first_chunk::<KEYS_SPILL_HDR>() else {
        return Err(refuse_keys_leftover());
    };
    if head[0..8] != KEYS_SPILL_MAGIC[..] {
        return Err(refuse_keys_leftover());
    }
    let n_multi = le_u32!(head, 8) as usize;
    let n_single = le_u32!(head, 12) as usize;
    let mut i = KEYS_SPILL_HDR;
    for _ in 0..n_multi {
        if bytes.len().saturating_sub(i) < KEY16_LEN {
            return Err(StoreError::Corrupt("scripthash keys spill truncated"));
        }
        let mut k = [0u8; KEY16_LEN];
        k.copy_from_slice(&bytes[i..i + KEY16_LEN]);
        i += KEY16_LEN;
        insert_key_word(map, k, 0);
    }
    let mut prev = 0u64;
    for _ in 0..n_single {
        let (delta, n) = read_uleb128(&bytes[i..])
            .map_err(|_| StoreError::Corrupt("scripthash keys spill uleb"))?;
        i = i.saturating_add(n);
        if bytes.len().saturating_sub(i) < KEY16_LEN {
            return Err(StoreError::Corrupt("scripthash keys spill truncated"));
        }
        let mut k = [0u8; KEY16_LEN];
        k.copy_from_slice(&bytes[i..i + KEY16_LEN]);
        i += KEY16_LEN;
        let fk = prev.saturating_add(delta);
        insert_key_word(map, k, fk);
        prev = fk;
    }
    if i != bytes.len() {
        return Err(StoreError::Corrupt("scripthash keys spill trailing bytes"));
    }
    Ok(())
}

#[doc(hidden)]
pub fn parse_spill_seq(name: &str) -> Option<u32> {
    if name.len() == 6 && name.bytes().all(|b| b.is_ascii_digit()) {
        name.parse().ok()
    } else {
        None
    }
}

#[doc(hidden)]
pub fn list_keys_spill_paths(dir: &Path, shard: usize) -> Result<Vec<PathBuf>, StoreError> {
    let p = unsorted_keys_path(dir, shard);
    if p.is_file() {
        return Err(refuse_keys_leftover());
    }
    if !p.is_dir() {
        return Ok(Vec::new());
    }
    let rd = fs::read_dir(&p).map_err(|e| StoreError::io(&p, e))?;
    let mut seqs = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| StoreError::io(&p, e))?;
        let name = ent.file_name();
        let s = name.to_string_lossy();
        if s.ends_with(".tmp") {
            continue;
        }
        let Some(seq) = parse_spill_seq(&s) else {
            return Err(refuse_keys_leftover());
        };
        seqs.push((seq, ent.path()));
    }
    seqs.sort_unstable_by_key(|a| a.0);
    Ok(seqs.into_iter().map(|(_, path)| path).collect())
}

#[doc(hidden)]
pub fn load_keys_shard_map(dir: &Path, shard: usize) -> Result<Key16PackMap, StoreError> {
    let mut map = Key16PackMap::default();
    for path in list_keys_spill_paths(dir, shard)? {
        let bytes = fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
        fold_keys_spill_bytes(&mut map, &bytes)?;
    }
    Ok(map)
}

#[doc(hidden)]
pub fn write_keys_spill(
    dir: &Path,
    shard: usize,
    seq: u32,
    map: &Key16PackMap,
) -> Result<(), StoreError> {
    let path = unsorted_keys_spill_path(dir, shard, seq);
    crate::file::write_tmp_rename(&path, &encode_keys_spill(map))
}

#[doc(hidden)]
pub fn next_keys_spill_seq(dir: &Path, shard: usize) -> Result<u32, StoreError> {
    let paths = list_keys_spill_paths(dir, shard)?;
    let mut max_seq = None;
    for p in &paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(seq) = parse_spill_seq(name) {
            max_seq = Some(max_seq.map_or(seq, |m: u32| m.max(seq)));
        }
    }
    Ok(max_seq.map(|s| s.saturating_add(1)).unwrap_or(0))
}

#[cfg(debug_assertions)]
pub fn write_keys_spill_entries(
    dir: &Path,
    shard: usize,
    seq: u32,
    items: &[(ShHeadKey, Option<Fk>)],
) -> Result<(), StoreError> {
    let mut map = Key16PackMap::default();
    for &(k, first) in items {
        match first {
            Some(fk) => insert_key_pack(&mut map, k, fk),
            None => insert_key_word(&mut map, k, 0),
        }
    }
    write_keys_spill(dir, shard, seq, &map)
}

#[doc(hidden)]
pub fn unlink_keys_shard(dir: &Path, shard: usize) {
    let p = unsorted_keys_path(dir, shard);
    if p.is_dir() {
        let _ = fs::remove_dir_all(&p);
    } else if p.is_file() {
        let _ = fs::remove_file(&p);
    }
}

#[doc(hidden)]
pub type Key16FkMap = HashMap<ShHeadKey, Vec<u64>, BuildHasherDefault<Key16IdentityHasher>>;

#[derive(Default, Debug)]
#[doc(hidden)]
pub struct PostPackMap {
    pub map: Key16FkMap,
    pub n_fks: usize,
}

impl PostPackMap {
    #[doc(hidden)]
    pub fn estimated_bytes(&self) -> usize {
        self.map
            .len()
            .saturating_mul(POST_MAP_KEY_BYTES)
            .saturating_add(self.n_fks.saturating_mul(POST_MAP_FK_BYTES))
    }

    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[doc(hidden)]
pub fn refuse_posts_leftover() -> StoreError {
    StoreError::Corrupt(INDEX_REFUSE_POST_LEFTOVER)
}

#[doc(hidden)]
pub fn posts_maps_estimated_bytes(maps: &[PostPackMap]) -> u64 {
    maps.iter().map(|m| m.estimated_bytes() as u64).sum()
}

#[doc(hidden)]
pub fn largest_posts_shard(maps: &[PostPackMap]) -> Option<usize> {
    maps.iter()
        .enumerate()
        .filter(|(_, m)| !m.is_empty())
        .max_by_key(|(_, m)| m.estimated_bytes())
        .map(|(i, _)| i)
}

#[doc(hidden)]
pub fn fks_strictly_increasing(fks: &[u64]) -> bool {
    fks.windows(2).all(|w| w[0] < w[1])
}

/// Keep `Vec<fk>` strictly increasing. Scan order appends; an older fk inserts.
#[doc(hidden)]
pub fn insert_post_fk(map: &mut PostPackMap, key: ShHeadKey, fk: u64) {
    if fk == 0 {
        return;
    }
    match map.map.entry(key) {
        Entry::Vacant(v) => {
            v.insert(vec![fk]);
            map.n_fks = map.n_fks.saturating_add(1);
        }
        Entry::Occupied(mut o) => {
            let v = o.get_mut();
            if let Some(last) = v.last().copied() {
                if fk > last {
                    v.push(fk);
                    map.n_fks = map.n_fks.saturating_add(1);
                    return;
                }
                if fk == last {
                    return;
                }
            }
            match v.binary_search(&fk) {
                Ok(_) => {}
                Err(i) => {
                    v.insert(i, fk);
                    map.n_fks = map.n_fks.saturating_add(1);
                }
            }
        }
    }
}

/// Linear merge of two strictly increasing fk runs. Equals collapse.
#[doc(hidden)]
pub fn merge_sorted_unique_fks(dst: &mut Vec<u64>, src: Vec<u64>) {
    if src.is_empty() {
        return;
    }
    if dst.is_empty() {
        *dst = src;
        dst.dedup();
        return;
    }
    debug_assert!(fks_strictly_increasing(dst));
    debug_assert!(fks_strictly_increasing(&src));
    let mut out = Vec::with_capacity(dst.len().saturating_add(src.len()));
    let mut i = 0usize;
    let mut j = 0usize;
    while i < dst.len() || j < src.len() {
        let take = match (dst.get(i), src.get(j)) {
            (Some(&a), Some(&b)) if a < b => {
                i += 1;
                a
            }
            (Some(&a), Some(&b)) if a > b => {
                j += 1;
                b
            }
            (Some(&a), Some(_)) => {
                i += 1;
                j += 1;
                a
            }
            (Some(&a), None) => {
                i += 1;
                a
            }
            (None, Some(&b)) => {
                j += 1;
                b
            }
            (None, None) => break,
        };
        if out.last().copied() != Some(take) {
            out.push(take);
        }
    }
    *dst = out;
}

#[doc(hidden)]
pub fn fold_post_map(dst: &mut PostPackMap, src: PostPackMap) {
    for (k, fks) in src.map {
        match dst.map.entry(k) {
            Entry::Vacant(v) => {
                let mut f = fks;
                f.dedup();
                debug_assert!(fks_strictly_increasing(&f));
                dst.n_fks = dst.n_fks.saturating_add(f.len());
                v.insert(f);
            }
            Entry::Occupied(mut o) => {
                let before = o.get().len();
                merge_sorted_unique_fks(o.get_mut(), fks);
                dst.n_fks = dst
                    .n_fks
                    .saturating_sub(before)
                    .saturating_add(o.get().len());
            }
        }
    }
}

#[doc(hidden)]
pub fn encode_posts_spill(map: &PostPackMap) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(
        POSTS_SPILL_HDR
            + map
                .map
                .len()
                .saturating_mul(KEY16_LEN + 8)
                .saturating_add(map.n_fks.saturating_mul(8)),
    );
    bytes.extend_from_slice(POSTS_SPILL_MAGIC);
    bytes.extend_from_slice(&(map.map.len() as u32).to_le_bytes());
    for (k, fks) in &map.map {
        debug_assert!(fks_strictly_increasing(fks));
        bytes.extend_from_slice(k);
        write_uleb128(&mut bytes, fks.len() as u64);
        let mut prev = 0u64;
        for &fk in fks {
            write_uleb128(&mut bytes, fk.saturating_sub(prev));
            prev = fk;
        }
    }
    bytes
}

#[doc(hidden)]
pub fn fold_posts_spill_bytes(map: &mut PostPackMap, bytes: &[u8]) -> Result<(), StoreError> {
    if bytes.len() < POSTS_SPILL_HDR {
        return Err(refuse_posts_leftover());
    }
    let mut hdr = [0u8; POSTS_SPILL_HDR];
    hdr.copy_from_slice(&bytes[..POSTS_SPILL_HDR]);
    if hdr[0..8] != POSTS_SPILL_MAGIC[..] {
        return Err(refuse_posts_leftover());
    }
    let n_keys = le_u32!(hdr, 8) as usize;
    let mut i = POSTS_SPILL_HDR;
    let mut src = PostPackMap::default();
    for _ in 0..n_keys {
        if bytes.len().saturating_sub(i) < KEY16_LEN {
            return Err(StoreError::Corrupt("scripthash posts spill truncated"));
        }
        let mut k = [0u8; KEY16_LEN];
        k.copy_from_slice(&bytes[i..i + KEY16_LEN]);
        i += KEY16_LEN;
        let (n, nread) = read_uleb128(&bytes[i..])
            .map_err(|_| StoreError::Corrupt("scripthash posts spill uleb"))?;
        i = i.saturating_add(nread);
        let mut fks = Vec::with_capacity(n as usize);
        let mut prev = 0u64;
        for _ in 0..n {
            let (delta, nread) = read_uleb128(&bytes[i..])
                .map_err(|_| StoreError::Corrupt("scripthash posts spill uleb"))?;
            i = i.saturating_add(nread);
            let fk = prev.saturating_add(delta);
            fks.push(fk);
            prev = fk;
        }
        src.map.insert(k, fks);
        src.n_fks = src.n_fks.saturating_add(n as usize);
    }
    if i != bytes.len() {
        return Err(StoreError::Corrupt("scripthash posts spill trailing bytes"));
    }
    fold_post_map(map, src);
    Ok(())
}

#[doc(hidden)]
pub fn list_posts_spill_paths(dir: &Path, shard: usize) -> Result<Vec<PathBuf>, StoreError> {
    let p = unsorted_post_path(dir, shard);
    if p.is_file() {
        return Err(refuse_posts_leftover());
    }
    if !p.is_dir() {
        return Ok(Vec::new());
    }
    let rd = fs::read_dir(&p).map_err(|e| StoreError::io(&p, e))?;
    let mut seqs = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| StoreError::io(&p, e))?;
        let name = ent.file_name();
        let s = name.to_string_lossy();
        if s.ends_with(".tmp") {
            continue;
        }
        let Some(seq) = parse_spill_seq(&s) else {
            return Err(refuse_posts_leftover());
        };
        seqs.push((seq, ent.path()));
    }
    seqs.sort_unstable_by_key(|a| a.0);
    Ok(seqs.into_iter().map(|(_, path)| path).collect())
}

#[doc(hidden)]
pub fn load_post_shard_map(dir: &Path, shard: usize) -> Result<PostPackMap, StoreError> {
    let mut map = PostPackMap::default();
    for path in list_posts_spill_paths(dir, shard)? {
        let bytes = fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
        fold_posts_spill_bytes(&mut map, &bytes)?;
    }
    Ok(map)
}

#[doc(hidden)]
pub fn write_posts_spill(
    dir: &Path,
    shard: usize,
    seq: u32,
    map: &PostPackMap,
) -> Result<(), StoreError> {
    let path = unsorted_post_spill_path(dir, shard, seq);
    crate::file::write_tmp_rename(&path, &encode_posts_spill(map))
}

#[doc(hidden)]
pub fn next_post_spill_seq(dir: &Path, shard: usize) -> Result<u32, StoreError> {
    let paths = list_posts_spill_paths(dir, shard)?;
    let mut max_seq = None;
    for p in &paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(seq) = parse_spill_seq(name) {
            max_seq = Some(max_seq.map_or(seq, |m: u32| m.max(seq)));
        }
    }
    Ok(max_seq.map(|s| s.saturating_add(1)).unwrap_or(0))
}

#[doc(hidden)]
pub fn spill_post_shard_map(
    dir: &Path,
    shard: usize,
    seqs: &[AtomicU32],
    map: PostPackMap,
) -> Result<(), StoreError> {
    if map.is_empty() {
        return Ok(());
    }
    let seq = seqs[shard].fetch_add(1, Ordering::Relaxed);
    write_posts_spill(dir, shard, seq, &map)
}

#[doc(hidden)]
pub fn spill_largest_posts_while_over(
    maps: &mut [PostPackMap],
    budget: u64,
    mut spill: impl FnMut(usize, PostPackMap) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    while posts_maps_estimated_bytes(maps) >= budget {
        let Some(si) = largest_posts_shard(maps) else {
            break;
        };
        let taken = std::mem::take(&mut maps[si]);
        spill(si, taken)?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn spill_all_post_maps(
    maps: &mut [PostPackMap],
    mut spill: impl FnMut(usize, PostPackMap) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for (si, local) in maps.iter_mut().enumerate() {
        let taken = std::mem::take(local);
        spill(si, taken)?;
    }
    Ok(())
}

#[doc(hidden)]
pub type PostSpillJob = (usize, PostPackMap);
#[doc(hidden)]
pub type PostSpillTx = SyncSender<PostSpillJob>;
#[doc(hidden)]
pub type PostSpillRx = Receiver<PostSpillJob>;

#[doc(hidden)]
pub fn posts_spill_channel() -> (PostSpillTx, PostSpillRx) {
    mpsc::sync_channel(1)
}

#[doc(hidden)]
pub fn send_posts_spill(
    tx: &PostSpillTx,
    shard: usize,
    map: PostPackMap,
) -> Result<(), StoreError> {
    if map.is_empty() {
        return Ok(());
    }
    tx.send((shard, map))
        .map_err(|_| StoreError::Corrupt("scripthash post spill writer stopped"))
}

#[doc(hidden)]
pub fn posts_spill_writer(
    rx: PostSpillRx,
    dir: &Path,
    seqs: &[AtomicU32],
    err: &Mutex<Option<StoreError>>,
) {
    while let Ok((si, map)) = rx.recv() {
        if err.lock().unwrap().is_some() {
            continue;
        }
        if let Err(e) = spill_post_shard_map(dir, si, seqs, map) {
            let mut g = err.lock().unwrap();
            if g.is_none() {
                *g = Some(e);
            }
        }
    }
}

#[doc(hidden)]
pub fn unlink_post_shard(dir: &Path, shard: usize) {
    let p = unsorted_post_path(dir, shard);
    if p.is_dir() {
        let _ = fs::remove_dir_all(&p);
    } else if p.is_file() {
        let _ = fs::remove_file(&p);
    }
}

#[cfg(debug_assertions)]
pub fn write_post_spill_entries(
    dir: &Path,
    shard: usize,
    seq: u32,
    items: &[(ShHeadKey, &[u64])],
) -> Result<(), StoreError> {
    let mut map = PostPackMap::default();
    for &(k, fks) in items {
        for &fk in fks {
            insert_post_fk(&mut map, k, fk);
        }
    }
    write_posts_spill(dir, shard, seq, &map)
}

#[cfg(debug_assertions)]
pub fn load_post_shard_entries(
    dir: &Path,
    shard: usize,
) -> Result<Vec<(ShHeadKey, Vec<u64>)>, StoreError> {
    let map = load_post_shard_map(dir, shard)?;
    Ok(map.map.into_iter().collect())
}

#[doc(hidden)]
pub fn finish_key_shard(
    table: &ScriptHashTable,
    dir: &Path,
    shard: usize,
    n_shards: usize,
) -> Result<(u64, u64, u64), StoreError> {
    let map = load_keys_shard_map(dir, shard)?;
    let times = seal_pack_map_to_head_and_fuse(table, dir, shard, n_shards, map)?;
    unlink_keys_shard(dir, shard);
    Ok(times)
}

#[doc(hidden)]
pub fn spill_shard_map(
    dir: &Path,
    shard: usize,
    seqs: &[AtomicU32],
    map: Key16PackMap,
) -> Result<(), StoreError> {
    if map.is_empty() {
        return Ok(());
    }
    let seq = seqs[shard].fetch_add(1, Ordering::Relaxed);
    write_keys_spill(dir, shard, seq, &map)
}

#[doc(hidden)]
pub fn spill_largest_keys_while_over(
    maps: &mut [Key16PackMap],
    budget: u64,
    mut spill: impl FnMut(usize, Key16PackMap) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    while keys_maps_estimated_bytes(maps) >= budget {
        let Some(si) = largest_keys_shard(maps) else {
            break;
        };
        let taken = std::mem::take(&mut maps[si]);
        spill(si, taken)?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn spill_all_keys_maps(
    maps: &mut [Key16PackMap],
    mut spill: impl FnMut(usize, Key16PackMap) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for (si, local) in maps.iter_mut().enumerate() {
        let taken = std::mem::take(local);
        spill(si, taken)?;
    }
    Ok(())
}

/// One in-flight map. The next spill blocks until the writer takes this one.
#[doc(hidden)]
pub fn keys_spill_channel() -> (KeysSpillTx, KeysSpillRx) {
    mpsc::sync_channel(1)
}

#[doc(hidden)]
pub fn send_keys_spill(
    tx: &KeysSpillTx,
    shard: usize,
    map: Key16PackMap,
) -> Result<(), StoreError> {
    if map.is_empty() {
        return Ok(());
    }
    tx.send((shard, map))
        .map_err(|_| StoreError::Corrupt("scripthash keys spill writer stopped"))
}

#[doc(hidden)]
pub fn keys_spill_writer(
    rx: KeysSpillRx,
    dir: &Path,
    seqs: &[AtomicU32],
    err: &Mutex<Option<StoreError>>,
) {
    while let Ok((si, map)) = rx.recv() {
        if err.lock().unwrap().is_some() {
            continue;
        }
        if let Err(e) = spill_shard_map(dir, si, seqs, map) {
            let mut g = err.lock().unwrap();
            if g.is_none() {
                *g = Some(e);
            }
        }
    }
}

#[doc(hidden)]
pub fn collect_keys_from_txs(
    txs: &TxTable,
    table: &ScriptHashTable,
    dir: &Path,
    first: u64,
    last: u64,
    workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    let n_shards = table.head_shard_count().max(1);
    if last < first {
        return Ok(UnsortedCollect {
            recs: 0,
            last_fk: last,
            per_shard: vec![0; n_shards],
        });
    }
    fs::create_dir_all(dir.join(KEYS_SUBDIR)).map_err(|e| StoreError::io(dir, e))?;
    let budget = SH_EXTRACT_WORKER_RAM_BYTES;
    let seqs: Vec<AtomicU32> = {
        let mut v = Vec::with_capacity(n_shards);
        for si in 0..n_shards {
            v.push(AtomicU32::new(next_keys_spill_seq(dir, si)?));
        }
        v
    };
    let recs = AtomicU64::new(0);
    let scanned = AtomicU64::new(0);
    let span_fks = last.saturating_sub(first).saturating_add(1);
    let err = Mutex::new(None::<StoreError>);
    let t0 = Instant::now();
    let last_status = Mutex::new(t0);
    let spans = worker_fk_spans(first, last, workers);
    let live = rbitcoin_log::progress::begin("scripthash keys collect", span_fks);
    std::thread::scope(|scope| {
        let n_workers = spans.len().max(1);
        rbitcoin_log::info!(
            "store: scripthash keys collect start workers={n_workers} budget_MiB={} fk={first}..{last}",
            budget / (1 << 20)
        );
        let (tx, rx) = keys_spill_channel();
        scope.spawn(|| keys_spill_writer(rx, dir, &seqs, &err));
        for &(span_lo, span_hi) in &spans {
            let tx = tx.clone();
            let recs = &recs;
            let scanned = &scanned;
            let live = &live;
            let err = &err;
            let last_status = &last_status;
            scope.spawn(move || {
                let mut locals: Vec<Key16PackMap> =
                    (0..n_shards).map(|_| Key16PackMap::default()).collect();
                let mut tally = CollectTally::new(0);
                for (lo, hi) in chunk_ranges(span_lo, span_hi, CLASS_A_CHUNK_FKS) {
                    if check_cancel(cancel, "scripthash keys collect").is_err() {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(StoreError::Cancelled("scripthash keys collect"));
                        }
                        break;
                    }
                    if err.lock().unwrap().is_some() {
                        break;
                    }
                    let r = txs.for_each_script_hashes_in_fk_span(lo, hi, |fk, sh| {
                        let si = prefix_shard_of(&sh, n_shards);
                        tally.recs = tally.recs.saturating_add(1);
                        insert_key_pack(&mut locals[si], head_key_from_full(&sh), fk);
                        Ok(())
                    });
                    flush_collect_tally(&mut tally, recs, None, None);
                    if let Err(e) = r {
                        *err.lock().unwrap() = Some(e);
                        break;
                    }
                    if let Err(e) = spill_largest_keys_while_over(&mut locals, budget, |si, map| {
                        send_keys_spill(&tx, si, map)
                    }) {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                        break;
                    }
                    let done = note_scanned_fks(scanned, lo, hi);
                    live.set_done(done);
                    let now = Instant::now();
                    let mut st = last_status.lock().unwrap();
                    if now.duration_since(*st) >= MATERIALIZE_STATUS_INTERVAL {
                        *st = now;
                        rbitcoin_log::info!(
                            "store: scripthash keys collect scanned={done}/{span_fks} recs={} elapsed={:?}",
                            recs.load(Ordering::Relaxed),
                            t0.elapsed()
                        );
                    }
                }
                if err.lock().unwrap().is_none() {
                    rbitcoin_log::info!(
                        "store: scripthash keys collect flush span={span_lo}..{span_hi}"
                    );
                    if let Err(e) = spill_all_keys_maps(&mut locals, |si, map| {
                        send_keys_spill(&tx, si, map)
                    }) {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                    }
                }
            });
        }
        drop(tx);
    });
    if let Some(e) = err.lock().unwrap().take() {
        return Err(e);
    }
    drop(live);
    let merge_workers = workers.max(1).min(n_shards.max(1));
    fs::create_dir_all(dir.join(MULTI_SUBDIR)).map_err(|e| StoreError::io(dir, e))?;
    let live_merge = rbitcoin_log::progress::begin("scripthash keys merge", n_shards as u64);
    rbitcoin_log::info!(
        "store: scripthash keys merge start n_shards={n_shards} workers={merge_workers}"
    );
    let t_merge = Instant::now();
    let per_shard: Vec<AtomicU64> = (0..n_shards).map(|_| AtomicU64::new(0)).collect();
    if merge_workers <= 1 {
        for (si, slot) in per_shard.iter().enumerate() {
            check_cancel(cancel, "scripthash keys merge")?;
            let (n, fold_ns, bdz_ns) = finish_key_shard(table, dir, si, n_shards)?;
            slot.store(n, Ordering::Relaxed);
            live_merge.add_done(1);
            rbitcoin_log::info!(
                "store: scripthash keys merge shard={si:02x} keys={n} fold={:?} bdz={:?}",
                Duration::from_nanos(fold_ns),
                Duration::from_nanos(bdz_ns)
            );
        }
    } else {
        let merge_err = Mutex::new(None::<StoreError>);
        let merge_jobs = Mutex::new(VecDeque::from_iter(0..n_shards));
        std::thread::scope(|scope| {
            for _ in 0..merge_workers {
                let merge_jobs = &merge_jobs;
                let merge_err = &merge_err;
                let per_shard = &per_shard;
                let live_merge = &live_merge;
                scope.spawn(move || loop {
                    if check_cancel(cancel, "scripthash keys merge").is_err() {
                        let mut g = merge_err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(StoreError::Cancelled("scripthash keys merge"));
                        }
                        break;
                    }
                    if merge_err.lock().unwrap().is_some() {
                        break;
                    }
                    let si = merge_jobs.lock().unwrap().pop_front();
                    let Some(si) = si else {
                        break;
                    };
                    match finish_key_shard(table, dir, si, n_shards) {
                        Ok((n, fold_ns, bdz_ns)) => {
                            per_shard[si].store(n, Ordering::Relaxed);
                            live_merge.add_done(1);
                            rbitcoin_log::info!(
                                "store: scripthash keys merge shard={si:02x} keys={n} fold={:?} bdz={:?}",
                                Duration::from_nanos(fold_ns),
                                Duration::from_nanos(bdz_ns)
                            );
                        }
                        Err(e) => {
                            *merge_err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                });
            }
        });
        if let Some(e) = merge_err.lock().unwrap().take() {
            return Err(e);
        };
    }
    let per_shard: Vec<u64> = per_shard
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    rbitcoin_log::info!(
        "store: scripthash keys merge done elapsed={:?}",
        t_merge.elapsed()
    );
    write_phase_done(dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, last, &per_shard)?;
    Ok(UnsortedCollect {
        recs: recs.load(Ordering::Relaxed),
        last_fk: last,
        per_shard,
    })
}

pub fn collect_unsorted_shard_files(
    store: &Store,
    dir: &Path,
    n_shards: usize,
    workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    collect_unsorted_covering_class_a(store, dir, n_shards, workers, false, cancel)
}

pub fn collect_unsorted_covering_class_a(
    store: &Store,
    dir: &Path,
    n_shards: usize,
    workers: usize,
    any_sealed_shards: bool,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    collect_unsorted_covering_txs(
        &store.txs,
        &store.scripthash,
        dir,
        n_shards,
        workers,
        any_sealed_shards,
        cancel,
    )
}

pub fn collect_unsorted_covering_txs(
    txs: &TxTable,
    table: &ScriptHashTable,
    dir: &Path,
    n_shards: usize,
    workers: usize,
    any_sealed_shards: bool,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    let last = txs.count();
    if leftover_legacy_unsorted(dir)
        && read_phase_done(dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, n_shards).is_none()
    {
        clear_unsorted_shard_dir(dir);
    }
    let done = read_phase_done(dir, KEYS_DONE_NAME, KEYS_DONE_MAGIC, n_shards);
    let keys_present = any_unsorted_keys(dir, n_shards);
    let action = plan_unsorted_collect(
        done.as_ref().map(|d| d.last_fk),
        last,
        any_sealed_shards,
        keys_present,
    );
    check_cancel(cancel, "scripthash keys collect")?;
    match action {
        UnsortedCollectAction::Skip => {
            let d = done.expect("skip requires DONE.keys");
            Ok(UnsortedCollect {
                recs: d.counts.iter().sum(),
                last_fk: d.last_fk,
                per_shard: d.counts,
            })
        }
        UnsortedCollectAction::Full => {
            clear_unsorted_shard_dir(dir);
            collect_keys_from_txs(txs, table, dir, 1, last, workers, cancel)
        }
        UnsortedCollectAction::Append { first, last } => {
            collect_keys_from_txs(txs, table, dir, first, last, workers, cancel)
        }
    }
}

#[doc(hidden)]
pub fn write_multi_fuse_keys(dir: &Path, si: usize, dupes: &[u64]) -> Result<u64, StoreError> {
    let path = unsorted_multi_fuse_path(dir, si);
    if dupes.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
    }
    let fuse = SealedFuse8::build(dupes)?;
    fuse.write_to(&path)?;
    Ok(dupes.len() as u64)
}

#[doc(hidden)]
pub fn seal_pack_map_to_head_and_fuse(
    table: &ScriptHashTable,
    dir: &Path,
    si: usize,
    n_shards: usize,
    map: Key16PackMap,
) -> Result<(u64, u64, u64), StoreError> {
    let nkeys = map.len() as u64;
    let t_fold = Instant::now();
    let n = map.len();
    let mut recs = Vec::with_capacity(n);
    let mut mixed = Vec::with_capacity(n);
    let mut dupes = Vec::new();
    let mut n_singles = 0u64;
    // Consuming the map drops its buckets before fuse8 and BDZ.
    for (k, w) in map {
        let m = mix_key16(&k);
        mixed.push(m);
        if w == 0 {
            dupes.push(m);
        } else {
            n_singles += 1;
        }
        recs.push((k, w));
    }
    let _n_fuse = write_multi_fuse_keys(dir, si, &dupes)?;
    let fold_ns = t_fold.elapsed().as_nanos() as u64;
    let t_bdz = Instant::now();
    let head = sorted_main_shard_path(table.store_dir(), si, n_shards);
    let _ = fs::remove_file(mphf_path(&head));
    let _ = fs::remove_file(val_path(&head));
    MphfHead::write_pack8_mixed(&head, &recs, &mixed)?;
    if si < table.head_shard_count() {
        table.set_extract_inline_creates(si, n_singles)?;
    }
    let bdz_ns = t_bdz.elapsed().as_nanos() as u64;
    Ok((nkeys, fold_ns, bdz_ns))
}

pub fn seal_mphf_from_keys(
    table: &ScriptHashTable,
    dir: &Path,
    n_shards: usize,
    cancel: Option<&AtomicBool>,
) -> Result<(), StoreError> {
    fs::create_dir_all(dir.join(MULTI_SUBDIR)).map_err(|e| StoreError::io(dir, e))?;
    let unsealed: HashSet<usize> = table.unsealed_main_shards().into_iter().collect();
    let jobs: Vec<usize> = (0..n_shards)
        .filter(|&si| {
            if !unsealed.contains(&si) {
                unlink_keys_shard(dir, si);
                return false;
            }
            let p = unsorted_keys_path(dir, si);
            p.is_dir() || p.is_file()
        })
        .collect();
    let remaining = jobs.len();
    let t_all = Instant::now();
    let workers = sh_extract_workers().max(1).min(remaining.max(1));
    let live = (remaining > 0).then(|| {
        rbitcoin_log::progress::begin_at(
            "scripthash keys merge",
            n_shards as u64,
            (n_shards - remaining) as u64,
        )
    });
    if remaining > 0 {
        rbitcoin_log::info!(
            "store: scripthash keys merge start remaining={remaining} n_shards={n_shards} workers={workers}"
        );
    }
    let err = Mutex::new(None::<StoreError>);
    let jobq = Mutex::new(VecDeque::from(jobs));
    std::thread::scope(|scope| {
        for _ in 0..workers.min(remaining.max(1)) {
            if remaining == 0 {
                break;
            }
            let jobq = &jobq;
            let err = &err;
            let live = live.as_ref();
            scope.spawn(move || loop {
                if check_cancel(cancel, "scripthash mphf from keys").is_err() {
                    let mut g = err.lock().unwrap();
                    if g.is_none() {
                        *g = Some(StoreError::Cancelled("scripthash mphf from keys"));
                    }
                    break;
                }
                if err.lock().unwrap().is_some() {
                    break;
                }
                let Some(si) = jobq.lock().unwrap().pop_front() else {
                    break;
                };
                match seal_one_mphf_shard(table, dir, si, n_shards) {
                    Ok((nkeys, fold_ns, bdz_ns)) => {
                        if let Some(live) = live {
                            live.add_done(1);
                        }
                        rbitcoin_log::info!(
                            "store: scripthash keys merge shard={si:02x} keys={nkeys} fold={:?} bdz={:?}",
                            Duration::from_nanos(fold_ns),
                            Duration::from_nanos(bdz_ns)
                        );
                    }
                    Err(e) => {
                        *err.lock().unwrap() = Some(e);
                        break;
                    }
                }
            });
        }
    });
    if let Some(e) = err.lock().unwrap().take() {
        return Err(e);
    }
    if remaining > 0 {
        rbitcoin_log::info!(
            "store: scripthash keys merge done remaining={remaining} elapsed={:?}",
            t_all.elapsed()
        );
    }
    Ok(())
}

#[doc(hidden)]
pub fn seal_one_mphf_shard(
    table: &ScriptHashTable,
    dir: &Path,
    si: usize,
    n_shards: usize,
) -> Result<(usize, u64, u64), StoreError> {
    let map = load_keys_shard_map(dir, si)?;
    let (nkeys, fold_ns, bdz_ns) = seal_pack_map_to_head_and_fuse(table, dir, si, n_shards, map)?;
    unlink_keys_shard(dir, si);
    Ok((nkeys as usize, fold_ns, bdz_ns))
}

#[doc(hidden)]
pub fn open_multi_fuses(
    dir: &Path,
    n_shards: usize,
) -> Result<Vec<Option<SealedFuse8>>, StoreError> {
    let mut out = Vec::with_capacity(n_shards);
    for si in 0..n_shards {
        let p = unsorted_multi_fuse_path(dir, si);
        if p.is_file() {
            out.push(Some(SealedFuse8::read_from(&p)?));
        } else {
            out.push(None);
        }
    }
    Ok(out)
}

#[doc(hidden)]
pub fn leftover_post_file(dir: &Path) -> bool {
    (0..256).any(|i| unsorted_post_path(dir, i).is_file())
}

#[doc(hidden)]
pub fn collect_posts_from_txs(
    txs: &TxTable,
    table: &ScriptHashTable,
    dir: &Path,
    first: u64,
    last: u64,
    workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    let n_shards = table.head_shard_count().max(1);
    if last < first {
        return Ok(UnsortedCollect {
            recs: 0,
            last_fk: last,
            per_shard: vec![0; n_shards],
        });
    }
    if leftover_post_file(dir) {
        return Err(refuse_posts_leftover());
    }
    fs::create_dir_all(dir.join(POST_SUBDIR)).map_err(|e| StoreError::io(dir, e))?;
    let fuses = Arc::new(open_multi_fuses(dir, n_shards)?);
    let budget = SH_EXTRACT_WORKER_RAM_BYTES;
    let seqs: Vec<AtomicU32> = {
        let mut v = Vec::with_capacity(n_shards);
        for si in 0..n_shards {
            v.push(AtomicU32::new(next_post_spill_seq(dir, si)?));
        }
        v
    };
    let recs = AtomicU64::new(0);
    let hits = AtomicU64::new(0);
    let scanned = AtomicU64::new(0);
    let span_fks = last.saturating_sub(first).saturating_add(1);
    let per = (0..n_shards).map(|_| AtomicU64::new(0)).collect::<Vec<_>>();
    let err = Mutex::new(None::<StoreError>);
    let t0 = Instant::now();
    let last_status = Mutex::new(t0);
    let spans = worker_fk_spans(first, last, workers);
    let live = rbitcoin_log::progress::begin("scripthash postings collect", span_fks);
    std::thread::scope(|scope| {
        let n_workers = spans.len().max(1);
        rbitcoin_log::info!(
            "store: scripthash postings collect start workers={n_workers} budget_MiB={} fk={first}..{last}",
            budget / (1 << 20)
        );
        let (tx, rx) = posts_spill_channel();
        scope.spawn(|| posts_spill_writer(rx, dir, &seqs, &err));
        for &(span_lo, span_hi) in &spans {
            let tx = tx.clone();
            let fuses = Arc::clone(&fuses);
            let recs = &recs;
            let hits = &hits;
            let scanned = &scanned;
            let live = &live;
            let per = &per;
            let err = &err;
            let last_status = &last_status;
            scope.spawn(move || {
                let mut locals: Vec<PostPackMap> =
                    (0..n_shards).map(|_| PostPackMap::default()).collect();
                let mut tally = CollectTally::new(n_shards);
                for (lo, hi) in chunk_ranges(span_lo, span_hi, CLASS_A_CHUNK_FKS) {
                    if check_cancel(cancel, "scripthash postings collect").is_err() {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(StoreError::Cancelled("scripthash postings collect"));
                        }
                        break;
                    }
                    if err.lock().unwrap().is_some() {
                        break;
                    }
                    let r = txs.for_each_script_hashes_in_fk_span(lo, hi, |fk, sh| {
                        tally.recs = tally.recs.saturating_add(1);
                        let si = prefix_shard_of(&sh, n_shards);
                        let Some(fuse) = fuses[si].as_ref() else {
                            return Ok(());
                        };
                        let key = head_key_from_full(&sh);
                        if !fuse.contains(mix_key16(&key)) {
                            return Ok(());
                        }
                        insert_post_fk(&mut locals[si], key, fk.0);
                        tally.hits = tally.hits.saturating_add(1);
                        tally.per[si] = tally.per[si].saturating_add(1);
                        Ok(())
                    });
                    flush_collect_tally(&mut tally, recs, Some(hits), Some(per));
                    if let Err(e) = r {
                        *err.lock().unwrap() = Some(e);
                        break;
                    }
                    if let Err(e) =
                        spill_largest_posts_while_over(&mut locals, budget, |si, map| {
                            send_posts_spill(&tx, si, map)
                        })
                    {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                        break;
                    }
                    let done = note_scanned_fks(scanned, lo, hi);
                    live.set_done(done);
                    let now = Instant::now();
                    let mut st = last_status.lock().unwrap();
                    if now.duration_since(*st) >= MATERIALIZE_STATUS_INTERVAL {
                        *st = now;
                        rbitcoin_log::info!(
                            "store: scripthash postings collect scanned={done}/{span_fks} recs={} hits={} elapsed={:?}",
                            recs.load(Ordering::Relaxed),
                            hits.load(Ordering::Relaxed),
                            t0.elapsed()
                        );
                    }
                }
                if err.lock().unwrap().is_none() {
                    rbitcoin_log::info!(
                        "store: scripthash postings collect flush span={span_lo}..{span_hi}"
                    );
                    if let Err(e) = spill_all_post_maps(&mut locals, |si, map| {
                        send_posts_spill(&tx, si, map)
                    }) {
                        let mut g = err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                    }
                }
            });
        }
        drop(tx);
    });
    if let Some(e) = err.lock().unwrap().take() {
        return Err(e);
    }
    let per_shard: Vec<u64> = per.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    write_phase_done(dir, POST_DONE_NAME, POST_DONE_MAGIC, last, &per_shard)?;
    Ok(UnsortedCollect {
        recs: recs.load(Ordering::Relaxed),
        last_fk: last,
        per_shard,
    })
}

pub fn collect_posts_covering(
    txs: &TxTable,
    table: &ScriptHashTable,
    dir: &Path,
    workers: usize,
    any_sealed_shards: bool,
    cancel: Option<&AtomicBool>,
) -> Result<UnsortedCollect, StoreError> {
    if leftover_post_file(dir) {
        return Err(refuse_posts_leftover());
    }
    let n_shards = table.head_shard_count().max(1);
    let last = txs.count();
    let done = read_phase_done(dir, POST_DONE_NAME, POST_DONE_MAGIC, n_shards);
    let action = plan_unsorted_collect(
        done.as_ref().map(|d| d.last_fk),
        last,
        any_sealed_shards,
        true,
    );
    check_cancel(cancel, "scripthash postings collect")?;
    match action {
        UnsortedCollectAction::Skip => {
            let d = done.expect("skip requires DONE.post");
            Ok(UnsortedCollect {
                recs: d.counts.iter().sum(),
                last_fk: d.last_fk,
                per_shard: d.counts,
            })
        }
        UnsortedCollectAction::Full => {
            let post_dir = dir.join(POST_SUBDIR);
            let _ = fs::remove_dir_all(&post_dir);
            collect_posts_from_txs(txs, table, dir, 1, last, workers, cancel)
        }
        UnsortedCollectAction::Append { first, last } => {
            collect_posts_from_txs(txs, table, dir, first, last, workers, cancel)
        }
    }
}

#[doc(hidden)]
pub fn pack_post_shard(
    table: &ScriptHashTable,
    unsorted_dir: &Path,
    shard: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ShShardPack, StoreError> {
    check_cancel(cancel, "scripthash unsorted shard pack")?;
    let mut session = table.pack_shard_session(shard)?;
    let map = load_post_shard_map(unsorted_dir, shard)?;
    if map.is_empty() {
        return session.finish_pack();
    }
    let n_shards = table.head_shard_count().max(1);
    let mphf = MphfHead::open(sorted_main_shard_path(table.store_dir(), shard, n_shards))?;
    session.reserve_pack_recs(map.n_fks);
    let mut fp_singles = 0u64;
    for (k, fks) in map.map {
        check_cancel(cancel, "scripthash unsorted shard pack")?;
        debug_assert!(fks_strictly_increasing(&fks));
        if fks.len() <= 1 {
            if fks.len() == 1 {
                fp_singles = fp_singles.saturating_add(1);
            }
            continue;
        }
        let slot = mphf.slot_for_key16(&k)?;
        for fk in fks {
            session.push_sorted_slot_fk(slot, Fk(fk))?;
        }
    }
    let mut pack = session.finish_pack()?;
    pack.fp_singles = fp_singles;
    Ok(pack)
}

#[doc(hidden)]
pub fn seal_shard(
    table: &ScriptHashTable,
    shard: usize,
    pack: ShShardPack,
    max_fk: &AtomicU64,
    progress: &MaterializeProgress,
) -> Result<(), StoreError> {
    max_fk.fetch_max(pack.max_fk, Ordering::Relaxed);
    let creates = pack.creates;
    let t_mphf = Instant::now();
    table.publish_packed_shard(shard, pack)?;
    progress
        .mphf_ns
        .fetch_add(t_mphf.elapsed().as_nanos() as u64, Ordering::Relaxed);
    progress.shards_published.fetch_add(1, Ordering::Relaxed);
    progress
        .creates_published
        .fetch_add(creates, Ordering::Relaxed);
    table.store_sharded_cold_progress(
        progress.keys_packed.load(Ordering::Relaxed),
        progress.creates_published.load(Ordering::Relaxed),
    )?;
    Ok(())
}

#[doc(hidden)]
pub struct ShardPool {
    pub jobs: Mutex<VecDeque<usize>>,
    pub err: Mutex<Option<StoreError>>,
}

#[doc(hidden)]
pub struct PackShardDone {
    pub shard: usize,
    pub keys: u64,
    pub creates: u64,
    pub fp_singles: u64,
    pub elapsed: Duration,
}

#[doc(hidden)]
pub fn log_unsorted_pack_shard(n_shards: usize, published: u32, d: &PackShardDone) {
    rbitcoin_log::info!(
        "store: scripthash unsorted pack shard={:02x} keys={} creates={} fp_singles={} \
         shards={published}/{n_shards} elapsed={:?}",
        d.shard,
        d.keys,
        d.creates,
        d.fp_singles,
        d.elapsed
    );
}

#[doc(hidden)]
pub fn pack_and_seal_unsorted_shard(
    table: &ScriptHashTable,
    unsorted_dir: &Path,
    shard: usize,
    cancel: Option<&AtomicBool>,
    progress: &MaterializeProgress,
    max_fk: &AtomicU64,
) -> Result<PackShardDone, StoreError> {
    check_cancel(cancel, "scripthash unsorted shard pack")?;
    let t0 = Instant::now();
    let t_merge = Instant::now();
    let pack = pack_post_shard(table, unsorted_dir, shard, cancel)?;
    progress
        .merge_ns
        .fetch_add(t_merge.elapsed().as_nanos() as u64, Ordering::Relaxed);
    progress.pack_ns.fetch_add(pack.pack_ns, Ordering::Relaxed);
    progress
        .body_flush_ns
        .fetch_add(pack.body_flush_ns, Ordering::Relaxed);
    progress.keys_packed.fetch_add(pack.keys, Ordering::Relaxed);
    progress
        .recs_packed
        .fetch_add(pack.creates, Ordering::Relaxed);
    let done = PackShardDone {
        shard,
        keys: pack.keys,
        creates: pack.creates,
        fp_singles: pack.fp_singles,
        elapsed: t0.elapsed(),
    };
    seal_shard(table, shard, pack, max_fk, progress)?;
    unlink_post_shard(unsorted_dir, shard);
    Ok(done)
}

#[cfg(debug_assertions)]
pub fn pack_one_extract_shard(
    table: &ScriptHashTable,
    unsorted_dir: &Path,
    shard: usize,
) -> Result<(), StoreError> {
    let n = table.head_shard_count().max(1);
    let progress = MaterializeProgress::new();
    let max_fk = AtomicU64::new(0);
    let done = pack_and_seal_unsorted_shard(table, unsorted_dir, shard, None, &progress, &max_fk)?;
    log_unsorted_pack_shard(n, progress.shards_published.load(Ordering::Relaxed), &done);
    Ok(())
}

pub fn materialize_sh_from_unsorted(
    table: &ScriptHashTable,
    unsorted_dir: &Path,
    pack_workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ShShardMaterialize, StoreError> {
    let n_shards = table.head_shard_count().max(1);
    seal_mphf_from_keys(table, unsorted_dir, n_shards, cancel)?;
    let jobs: Vec<usize> = table
        .unsealed_main_shards()
        .into_iter()
        .filter(|s| *s < n_shards)
        .collect();
    if jobs.is_empty() {
        return Ok(ShShardMaterialize {
            creates: table.entry_count(),
            keys: 0,
            max_fk: 0,
            merge_ns: 0,
            pack_ns: 0,
            mphf_ns: 0,
            body_flush_ns: 0,
            head_fill_ns: 0,
        });
    }
    let workers = pack_workers.max(1).min(jobs.len());
    let t0 = Instant::now();
    let progress = MaterializeProgress::new();
    let already = (n_shards - jobs.len()) as u32;
    progress.shards_published.store(already, Ordering::Relaxed);
    progress
        .recs_packed
        .store(table.entry_count(), Ordering::Relaxed);
    progress
        .creates_published
        .store(table.entry_count(), Ordering::Relaxed);
    let max_fk = AtomicU64::new(0);
    let live =
        rbitcoin_log::progress::begin_at("scripthash pack", n_shards as u64, u64::from(already));
    rbitcoin_log::info!(
        "store: scripthash unsorted pack start unsealed={} n_shards={n_shards} workers={workers}",
        jobs.len()
    );

    let out = std::thread::scope(|scope| {
        let progress = &progress;
        let max_fk = &max_fk;
        let live = &live;
        if workers <= 1 {
            for shard in jobs {
                let d = pack_and_seal_unsorted_shard(
                    table,
                    unsorted_dir,
                    shard,
                    cancel,
                    progress,
                    max_fk,
                )?;
                let published = progress.shards_published.load(Ordering::Relaxed);
                live.set_done(u64::from(published));
                log_unsorted_pack_shard(n_shards, published, &d);
            }
        } else {
            let shared = Arc::new(ShardPool {
                jobs: Mutex::new(VecDeque::from(jobs)),
                err: Mutex::new(None),
            });
            let mut joins = Vec::with_capacity(workers);
            for _ in 0..workers {
                let shared = Arc::clone(&shared);
                joins.push(scope.spawn(move || loop {
                    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
                        let mut g = shared.err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(StoreError::Cancelled("scripthash unsorted shard pack"));
                        }
                        break;
                    }
                    if shared.err.lock().unwrap().is_some() {
                        break;
                    }
                    let shard = shared.jobs.lock().unwrap().pop_front();
                    let Some(shard) = shard else {
                        break;
                    };
                    match pack_and_seal_unsorted_shard(
                        table,
                        unsorted_dir,
                        shard,
                        cancel,
                        progress,
                        max_fk,
                    ) {
                        Ok(d) => {
                            let published = progress.shards_published.load(Ordering::Relaxed);
                            live.set_done(u64::from(published));
                            log_unsorted_pack_shard(n_shards, published, &d);
                        }
                        Err(e) => {
                            *shared.err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                }));
            }
            for j in joins {
                if j.join().is_err() {
                    return Err(StoreError::Corrupt(
                        "scripthash unsorted pack worker panicked",
                    ));
                }
            }
            let err = shared.err.lock().unwrap().take();
            if let Some(e) = err {
                return Err(e);
            }
        }
        Ok(ShShardMaterialize {
            creates: progress.creates_published.load(Ordering::Relaxed),
            keys: progress.keys_packed.load(Ordering::Relaxed),
            max_fk: max_fk.load(Ordering::Relaxed),
            merge_ns: 0,
            pack_ns: 0,
            mphf_ns: 0,
            body_flush_ns: 0,
            head_fill_ns: 0,
        }
        .with_stages(progress.stages()))
    })?;
    rbitcoin_log::info!(
        "store: scripthash unsorted pack done creates≈{} keys≈{} elapsed={:?}",
        out.creates,
        out.keys,
        t0.elapsed()
    );
    Ok(out)
}

pub fn materialize_sh_unsorted_from_class_a(
    store: &Store,
    collect_workers: usize,
    pack_workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ShShardMaterialize, StoreError> {
    let table = &store.scripthash;
    let n_shards = table.head_shard_count().max(1);
    let dir = unsorted_shard_dir(store.path());
    let collect_workers = if collect_workers == 0 {
        unsorted_collect_workers()
    } else {
        collect_workers
    };
    let pack_workers = if pack_workers == 0 {
        unsorted_pack_workers()
    } else {
        pack_workers
    };

    let unsealed = table.unsealed_main_shards();
    if unsealed.is_empty() && (!table.head_is_empty() || table.entry_count() > 0) {
        clear_unsorted_shard_dir(&dir);
        ColdProgress::clear(store.path());
        return Ok(ShShardMaterialize {
            creates: table.entry_count(),
            keys: 0,
            max_fk: 0,
            merge_ns: 0,
            pack_ns: 0,
            mphf_ns: 0,
            body_flush_ns: 0,
            head_fill_ns: 0,
        });
    }

    // Pass-1 sets `live_count` and leaves MPHF on disk without loading it, so
    // `head_is_empty` is true. Reinit would drop write-behind body rows.
    if table.head_is_empty() && table.entry_count() == 0 {
        table.reinit_empty_for_cold_materialize()?;
    }

    let any_sealed = unsealed.len() < n_shards;
    let collected = collect_unsorted_covering_class_a(
        store,
        &dir,
        n_shards,
        collect_workers,
        any_sealed,
        cancel,
    )?;
    let mut mat = materialize_sh_from_unsorted_from_txs(
        table,
        &store.txs,
        &dir,
        collect_workers,
        pack_workers,
        cancel,
    )?;
    mat.max_fk = mat.max_fk.max(collected.last_fk);
    if table.unsealed_main_shards().is_empty() {
        clear_unsorted_shard_dir(&dir);
        ColdProgress::clear(store.path());
    }
    Ok(mat)
}

/// Seal extract MPHF, scan Class A into postings, pack unsealed shards.
pub fn materialize_sh_from_unsorted_from_txs(
    table: &ScriptHashTable,
    txs: &TxTable,
    unsorted_dir: &Path,
    collect_workers: usize,
    pack_workers: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ShShardMaterialize, StoreError> {
    let n_shards = table.head_shard_count().max(1);
    seal_mphf_from_keys(table, unsorted_dir, n_shards, cancel)?;
    let any_sealed = table.unsealed_main_shards().len() < n_shards;
    collect_posts_covering(
        txs,
        table,
        unsorted_dir,
        collect_workers,
        any_sealed,
        cancel,
    )?;
    materialize_sh_from_unsorted(table, unsorted_dir, pack_workers, cancel)
}
