//! Live filter and tweak bytes for one confirm batch.
//!
//! The scripts stage publishes these jobs on their own steal wave, after the
//! script wave. One job is one block filter or one range of at most
//! [`TWEAK_TXS_PER_JOB`] eligible transactions. Claim size is one job.
//! No store read. [`Query::index_live`] is set at startup, after a short
//! restart gap is sealed, so load does not sample `next_height`. A write that
//! finds the watermark is not this batch is corrupt.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::{ConfirmError, ParentPinView, Query};
use bitcoin::hashes::Hash;
use bitcoin::{Amount, Block, ScriptBuf, Transaction, TxOut};
use rbitcoin_primitives::{Fk, Height};

use crate::index_rows::{self, IndexRows};
use rbitcoin_consensus::silent_payments::tweak_from_tx;
use rbitcoin_consensus::ConsensusError;
use rbitcoin_store::StoreError;

use super::{LoadedBatch, Prepared};

/// Eligible transactions per tweak job. The index wave claims one job at a time.
pub const TWEAK_TXS_PER_JOB: usize = 32;

/// Which indexes this batch should assemble. Set at load from [`Query::index_live`].
#[derive(Clone, Debug, Default)]
pub struct IndexWant {
    pub filters: bool,
    /// Tweaks for heights at or above this origin. `None` when tweaks are off.
    pub tweak_origin: Option<u32>,
}

/// One confirm batch of filter bytes and tweak vecs. Dropped with the batch.
pub type IndexSeal = IndexRows;

#[doc(hidden)]
pub type TweakSlots = Arc<[OnceLock<Option<[u8; 33]>>]>;
#[doc(hidden)]
pub type PrevoutSlots = Arc<[OnceLock<Vec<TxOut>>]>;

#[derive(Clone, Copy)]
#[doc(hidden)]
pub struct SpendRef {
    pub prev_txid: [u8; 32],
    pub vout: u32,
    pub create_fk: Fk,
    pub vin: u32,
}

pub struct BlockWork {
    pub block: Arc<Block>,
    pub hash: [u8; 32],
    pub parents: Arc<ParentPinView>,
    /// Spends grouped by tx index, sorted by `vin`. Parallel jobs read their own slot.
    pub spends_by_tx: Arc<[Vec<SpendRef>]>,
    pub same_block: OnceLock<HashMap<[u8; 32], usize>>,
    /// One cell per transaction. The filter and a tweak range share it.
    pub prevouts: PrevoutSlots,
    pub tweaks: TweakSlots,
    pub filter: Option<Arc<OnceLock<bitcoin::bip158::BlockFilter>>>,
    /// Shared by every block in the batch. The first index job wins.
    pub started: Arc<OnceLock<Instant>>,
}

pub struct HeightSlots {
    pub height: Height,
    pub header_fk: Fk,
    pub want_tweaks: bool,
    pub tweaks: TweakSlots,
    pub filter: Option<Arc<OnceLock<bitcoin::bip158::BlockFilter>>>,
}

pub enum IndexJob {
    Filter(Arc<BlockWork>),
    Tweaks(Arc<BlockWork>, Vec<usize>),
}

pub struct IndexJobs {
    pub jobs: Vec<IndexJob>,
    pub slots: Vec<HeightSlots>,
    /// First index job writes this. The publisher reads it when the wave completes.
    pub started: Arc<OnceLock<Instant>>,
}

pub fn index_want(query: &Query) -> IndexWant {
    let live = query.index_live();
    let follow = query.tweak_follow();
    if !live && !follow {
        return IndexWant::default();
    }
    IndexWant {
        filters: live && query.filter_index_next().is_some(),
        tweak_origin: (live || follow)
            .then(|| query.tweak_index_next().map(|_| query.sptweaks_origin().0))
            .flatten(),
    }
}

pub fn prepare_index_jobs(batch: &LoadedBatch) -> Result<IndexJobs, ConfirmError> {
    let want = &batch.index_want;
    if !want.filters && want.tweak_origin.is_none() {
        return Ok(IndexJobs {
            jobs: Vec::new(),
            slots: Vec::new(),
            started: Arc::new(OnceLock::new()),
        });
    }
    if batch.prepared.len() != batch.wire_blocks.len() {
        return Err(corrupt("invariant: live index block count"));
    }
    let parents = Arc::new(batch.batch_parents.pin_view());
    let started = Arc::new(OnceLock::new());
    let mut jobs = Vec::new();
    let mut tweak_jobs = Vec::new();
    let mut slots = Vec::new();
    for (prep, block) in batch.prepared.iter().zip(batch.wire_blocks.iter()) {
        let want_tweaks = want.tweak_origin.is_some_and(|o| prep.height.0 >= o);
        if !want.filters && !want_tweaks {
            continue;
        }
        let spends_by_tx = group_spends(prep, block.txdata.len())?;
        let tweak_cells: TweakSlots = block
            .txdata
            .iter()
            .map(|tx| {
                let cell = OnceLock::new();
                if !want_tweaks || tx.is_coinbase() || !tx_has_taproot_out(tx) {
                    let _ = cell.set(None);
                }
                cell
            })
            .collect::<Vec<_>>()
            .into();
        let n_tx = block.txdata.len();
        let prevouts: PrevoutSlots = (0..n_tx)
            .map(|_| OnceLock::new())
            .collect::<Vec<_>>()
            .into();
        let filter = if want.filters {
            Some(Arc::new(OnceLock::new()))
        } else {
            None
        };
        let work = Arc::new(BlockWork {
            block: Arc::clone(block),
            hash: prep.hash,
            parents: Arc::clone(&parents),
            spends_by_tx: spends_by_tx.into(),
            same_block: OnceLock::new(),
            prevouts,
            tweaks: Arc::clone(&tweak_cells),
            filter: filter.clone(),
            started: Arc::clone(&started),
        });
        if filter.is_some() {
            jobs.push(IndexJob::Filter(Arc::clone(&work)));
        }
        if want_tweaks {
            let eligible: Vec<usize> = block
                .txdata
                .iter()
                .enumerate()
                .filter(|(_, tx)| !tx.is_coinbase() && tx_has_taproot_out(tx))
                .map(|(ti, _)| ti)
                .collect();
            for range in eligible.chunks(TWEAK_TXS_PER_JOB) {
                tweak_jobs.push(IndexJob::Tweaks(Arc::clone(&work), range.to_vec()));
            }
        }
        slots.push(HeightSlots {
            height: prep.height,
            header_fk: prep.header_fk,
            want_tweaks,
            tweaks: tweak_cells,
            filter,
        });
    }
    jobs.extend(tweak_jobs);
    Ok(IndexJobs {
        jobs,
        slots,
        started,
    })
}

pub fn apply_index_job(job: &IndexJob) -> Result<(), ConsensusError> {
    let result = match job {
        IndexJob::Filter(work) => {
            let _ = work.started.set(Instant::now());
            apply_filter(work)
        }
        IndexJob::Tweaks(work, txs) => {
            let _ = work.started.set(Instant::now());
            apply_tweak_range(work, txs)
        }
    };
    result.map_err(ConfirmError::into_pool)
}

#[doc(hidden)]
pub fn apply_tweak_range(work: &BlockWork, txs: &[usize]) -> Result<(), ConfirmError> {
    for &ti in txs {
        let tx = &work.block.txdata[ti];
        let prevouts = prevouts_cached(work, ti)?;
        let tweak = tweak_from_tx(tx, prevouts).map(|t| t.tweak);
        if work.tweaks[ti].set(tweak).is_err() {
            return Err(corrupt("invariant: index slot"));
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn apply_filter(work: &BlockWork) -> Result<(), ConfirmError> {
    let mut prevouts = Vec::with_capacity(work.block.txdata.len());
    for ti in 0..work.block.txdata.len() {
        prevouts.push(prevouts_cached(work, ti)?.clone());
    }
    let filter = basic_filter_from_wire(&work.hash, &work.block, &prevouts)?;
    let Some(slot) = work.filter.as_ref() else {
        return Err(corrupt("invariant: filter job"));
    };
    if slot.set(filter).is_err() {
        return Err(corrupt("invariant: filter job"));
    }
    Ok(())
}

pub fn rows_from_heights(slots: &[HeightSlots]) -> Result<IndexRows, ConfirmError> {
    let mut rows = IndexRows::default();
    for height in slots {
        if let Some(slot) = &height.filter {
            let Some(filter) = slot.get().cloned() else {
                return Err(corrupt("invariant: index slot"));
            };
            rows.filters.push((height.height, filter, height.header_fk));
        }
        if !height.want_tweaks {
            continue;
        }
        let mut tweaks = Vec::with_capacity(height.tweaks.len());
        for cell in height.tweaks.iter() {
            let Some(tweak) = cell.get().copied() else {
                return Err(corrupt("invariant: index slot"));
            };
            tweaks.push(tweak);
        }
        rows.tweaks.push((height.height, height.header_fk, tweaks));
    }
    Ok(rows)
}

#[doc(hidden)]
pub fn tx_has_taproot_out(tx: &Transaction) -> bool {
    tx.output.iter().any(|out| out.script_pubkey.is_p2tr())
}

#[doc(hidden)]
pub fn group_spends(prep: &Prepared, n_tx: usize) -> Result<Vec<Vec<SpendRef>>, ConfirmError> {
    let mut index_of: HashMap<Fk, usize> = HashMap::with_capacity(prep.tx_fks.len());
    for (i, fk) in prep.tx_fks.iter().copied().enumerate() {
        if i >= n_tx {
            break;
        }
        index_of.entry(fk).or_insert(i);
    }
    let mut by_tx = vec![Vec::new(); n_tx];
    for &(prev_txid, vout, spending_fk, create_fk, vin) in &prep.spends {
        if spending_fk.is_null() {
            return Err(corrupt("invariant: live index spend fk"));
        }
        let Some(&ti) = index_of.get(&spending_fk) else {
            return Err(corrupt("invariant: live index tx fk"));
        };
        by_tx[ti].push(SpendRef {
            prev_txid,
            vout,
            create_fk,
            vin,
        });
    }
    for row in &mut by_tx {
        row.sort_by_key(|spend| spend.vin);
    }
    Ok(by_tx)
}

#[doc(hidden)]
pub fn basic_filter_from_wire(
    hash: &[u8; 32],
    block: &Block,
    prevouts: &[Vec<TxOut>],
) -> Result<bitcoin::bip158::BlockFilter, ConfirmError> {
    let outputs = block
        .txdata
        .iter()
        .flat_map(|tx| tx.output.iter().map(|o| o.script_pubkey.as_bytes()));
    let spent = prevouts
        .iter()
        .flatten()
        .map(|prev| prev.script_pubkey.as_bytes());
    crate::basic_filter_from_scripts(hash, outputs, spent).map_err(ConfirmError::from)
}

#[doc(hidden)]
pub fn prevouts_cached(work: &BlockWork, ti: usize) -> Result<&Vec<TxOut>, ConfirmError> {
    if let Some(got) = work.prevouts[ti].get() {
        return Ok(got);
    }
    let built = prevouts_for_tx(work, ti, &work.block.txdata[ti])?;
    let _ = work.prevouts[ti].set(built);
    work.prevouts[ti]
        .get()
        .ok_or_else(|| corrupt("invariant: blockfilter prevout missing"))
}

#[doc(hidden)]
pub fn prevouts_for_tx(
    work: &BlockWork,
    ti: usize,
    tx: &Transaction,
) -> Result<Vec<TxOut>, ConfirmError> {
    if tx.is_coinbase() {
        return Ok(Vec::new());
    }
    let rows = &work.spends_by_tx[ti];
    if rows.len() != tx.input.len() {
        return Err(corrupt("invariant: live index prevout count"));
    }
    let mut out = Vec::with_capacity(rows.len());
    for spend in rows {
        out.push(resolve_prevout(work, spend)?);
    }
    Ok(out)
}

#[doc(hidden)]
pub fn resolve_prevout(work: &BlockWork, spend: &SpendRef) -> Result<TxOut, ConfirmError> {
    if spend.create_fk.is_null() {
        let same_block = work.same_block.get_or_init(|| {
            let mut map = HashMap::with_capacity(work.block.txdata.len());
            for (i, tx) in work.block.txdata.iter().enumerate() {
                map.insert(tx.compute_txid().to_byte_array(), i);
            }
            map
        });
        let Some(&pi) = same_block.get(&spend.prev_txid) else {
            return Err(corrupt("invariant: blockfilter prevout missing"));
        };
        let Some(out) = work.block.txdata[pi].output.get(spend.vout as usize) else {
            return Err(corrupt("invariant: blockfilter prevout missing"));
        };
        return Ok(out.clone());
    }
    let Some((value, script, txid)) = work.parents.txout_parts(spend.create_fk, spend.vout) else {
        return Err(corrupt("invariant: blockfilter prevout missing"));
    };
    if txid != spend.prev_txid {
        return Err(corrupt("invariant: live index prevout txid"));
    }
    Ok(TxOut {
        value: Amount::from_sat(value),
        script_pubkey: ScriptBuf::from_bytes(script),
    })
}

/// After `confirmed[h]` is set: commit this batch. A watermark that is not
/// the first row is corrupt. An empty seal (live append off) is a no-op.
pub fn seal_live_indexes(query: &Query, seal: IndexSeal) -> Result<u64, ConfirmError> {
    if seal.filters.is_empty() && seal.tweaks.is_empty() {
        refresh_tweak_follow(query);
        return Ok(0);
    }
    let commit = index_rows::commit_index_rows(query, seal)?;
    refresh_tweak_follow(query);
    if commit.moved {
        return Err(corrupt("invariant: live index commit"));
    }
    Ok(commit.put_ns)
}

/// Prune follow stays on only while the tweak watermark is still contiguous
/// with the tip. A skipped batch turns it off until startup repair.
pub fn refresh_tweak_follow(query: &Query) {
    query.set_tweak_follow(crate::index_writebehind::tweak_follow_for(query));
}

#[doc(hidden)]
pub fn corrupt(msg: &'static str) -> ConfirmError {
    ConfirmError::Store(StoreError::Corrupt(msg))
}
