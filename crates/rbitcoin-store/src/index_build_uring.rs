//! Completion-session reader for the post-IBD block index builder (BIP158
//! basic filters and BIP-352 tweaks).
//!
//! One call reads a **window** of consecutive heights through four stages on
//! one held session, thousands of reads in flight per stage:
//!
//! 1. `create.loc` windows for every create in the window, each block's
//!    `input.loc` span, and (tweak heights) `seqsigwit.loc` windows.
//! 2. Each block's `txout.body` span and `input.body` span; each block's
//!    `txid.body` span when any height wants tweaks.
//! 3. `create.loc` windows of parents outside the window; `seqsigwit.body`
//!    of tweak-height txs with a P2TR output; parent txids for tweaks.
//! 4. Parent `txout.body` records.
//!
//! Offsets come from RAM checkpoints, so no stage opens another session.
//! Parents are read once per window however many blocks spend them. A per-op
//! short read or errno on a live session completes with a libc pread; with no
//! session the same stages run as serial preads.

use crate::create_loc::CreateLocPair;
use crate::error::StoreError;
use crate::input::{decode_edges_span, EdgesPlan, InputEdge};
use crate::io_handle::IoHandle;
use crate::sp_tweaks_uring::LoadedTweakTx;
use crate::tx_table::{
    apply_input_edges, decode_packed_tx_outs_with_spender_rels_secret, decode_seqsigwit_secret,
    OutputRecord, TxTable,
};
use crate::txid_body::{TxidBody, TXID_ENTRY_LEN};
use crate::uring_session::{self, UringSession};
use crate::U64Map;
use rbitcoin_primitives::{Fk, Height};
use std::path::Path;

/// Ring depth for the builder's session.
#[doc(hidden)]
pub const ENTRIES: u32 = 256;

/// One height to read: its confirmed header and contiguous creates.
#[derive(Clone, Copy, Debug)]
pub struct IndexHeight {
    pub height: Height,
    pub header_fk: Fk,
    pub first: Fk,
    pub n: u32,
    /// Also read what tweaks need (txids, `seqsigwit` of P2TR-output txs).
    pub tweaks: bool,
}

/// One block's creates, in block order, with their input edges.
pub struct IndexBlock {
    pub height: Height,
    pub header_fk: Fk,
    /// Block hash, internal byte order. Zero until the index IO thread fills
    /// it from the header; assemble reads this and does not touch the store.
    pub hash: [u8; 32],
    /// `inputs` is `Some` only for tweak-height txs with a P2TR output.
    pub txs: Vec<LoadedTweakTx>,
    pub edges: Vec<Vec<InputEdge>>,
}

/// A window of blocks plus the outputs of every parent they spend that is
/// not itself in the window (`txid` is zero unless the window wants tweaks).
pub struct IndexWindow {
    pub blocks: Vec<IndexBlock>,
    pub parents: Parents,
    /// Create fk → `(block, tx)` for creates in `blocks`.
    pub in_window: InWindow,
}

impl IndexWindow {
    /// Outputs of create `fk`, from this window's blocks or parents.
    pub fn outs(&self, fk: Fk) -> Option<&[OutputRecord]> {
        match self.in_window.get(&fk.0) {
            Some(&(bi, ti)) => Some(&self.blocks[bi].txs[ti].outs),
            None => Some(&self.parents.get(&fk.0)?.1),
        }
    }

    /// Output `vout` of create `fk`, from this window's blocks or parents.
    pub fn prevout(&self, fk: Fk, vout: u32) -> Option<&OutputRecord> {
        self.outs(fk)?.get(vout as usize)
    }

    /// Txid of create `fk` (zero when the window did not read txids).
    pub fn txid(&self, fk: Fk) -> Option<[u8; 32]> {
        match self.in_window.get(&fk.0) {
            Some(&(bi, ti)) => Some(self.blocks[bi].txs[ti].rec.txid),
            None => self.parents.get(&fk.0).map(|p| p.0),
        }
    }
}

#[doc(hidden)]
pub struct ReadJob<'p> {
    pub fd: IoHandle,
    pub path: &'p Path,
    pub off: u64,
    pub buf: Vec<u8>,
}

impl<'p> ReadJob<'p> {
    #[doc(hidden)]
    pub fn new((fd, path): (IoHandle, &'p Path), off: u64, len: u64) -> Result<Self, StoreError> {
        if len > crate::var_table::VarTable::BODY_READ_MAX {
            return Err(StoreError::Rejected("body read too large"));
        }
        let n = usize::try_from(len).map_err(|_| StoreError::Corrupt("body span too large"))?;
        Ok(Self {
            fd,
            path,
            off,
            buf: vec![0u8; n],
        })
    }
}

/// Complete `buf[done..]` with libc preads.
#[doc(hidden)]
pub fn libc_complete(job: &mut ReadJob<'_>, mut done: usize) -> Result<(), StoreError> {
    while done < job.buf.len() {
        let rc = job.fd.pread(job.off + done as u64, &mut job.buf[done..]);
        if rc <= 0 {
            let err = if rc == 0 {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "index build pread short")
            } else {
                std::io::Error::from_raw_os_error(-rc)
            };
            return Err(StoreError::io(job.path, err));
        }
        done += rc as usize;
    }
    Ok(())
}

#[doc(hidden)]
pub enum Reader<'s> {
    Session(&'s mut UringSession),
    Serial,
}

impl Reader<'_> {
    #[doc(hidden)]
    pub fn run(&mut self, jobs: &mut [ReadJob<'_>]) -> Result<(), StoreError> {
        match self {
            Reader::Serial => jobs.iter_mut().try_for_each(|j| libc_complete(j, 0)),
            Reader::Session(session) => run_on_session(session, jobs),
        }
    }
}

/// Keep the ring full with `jobs` and harvest until all complete.
///
/// On any error this drains leftover SQEs before returning: `jobs` (and their
/// buffers) are dropped by the caller as soon as this returns.
#[doc(hidden)]
pub fn run_on_session(
    session: &mut UringSession,
    jobs: &mut [ReadJob<'_>],
) -> Result<(), StoreError> {
    let out = run_on_session_inner(session, jobs);
    if out.is_err() && session.in_flight() != 0 {
        if let Err(drain_err) = session.drain_all() {
            if session.in_flight() != 0 {
                // `jobs` still own the buffers these SQEs point at. Returning
                // now would free them under the kernel, so fail closed.
                uring_session::abort_uring_unusable("index build drain failed with in-flight SQEs");
            }
            return Err(drain_err);
        }
    }
    out
}

#[cfg(debug_assertions)]
thread_local! {
    static TEST_ERROR_AFTER_PUSH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam: make the index-build machine fail after its first push.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn with_index_error_after_push<R>(f: impl FnOnce() -> R) -> R {
    TEST_ERROR_AFTER_PUSH.with(|c| {
        let prev = c.replace(true);
        let out = f();
        c.set(prev);
        out
    })
}

fn run_on_session_inner(
    session: &mut UringSession,
    jobs: &mut [ReadJob<'_>],
) -> Result<(), StoreError> {
    session.begin_batch()?;
    let epoch = session.epoch();
    let mut next = 0usize;
    let mut inflight = 0usize;
    while next < jobs.len() || inflight > 0 {
        while next < jobs.len() && session.free_sq() > 0 {
            let i = next;
            next += 1;
            if jobs[i].buf.is_empty() {
                continue;
            }
            let ud = uring_session::pack_ud(uring_session::KIND_INDEX_BUILD, epoch, i as u32);
            let job = &mut jobs[i];
            // SAFETY: `job.buf` lives through `run_on_session`, which drains
            // every leftover SQE before returning an error.
            unsafe { session.push_pread(job.fd, job.off, &mut job.buf, ud) }?;
            inflight += 1;
            #[cfg(debug_assertions)]
            if TEST_ERROR_AFTER_PUSH.with(|c| c.get()) {
                return Err(StoreError::Corrupt("test: index build error after push"));
            }
        }
        if inflight == 0 {
            break;
        }
        session.sync_submission();
        let _ = session.submit();
        let cqes = session.harvest_ready()?;
        if cqes.is_empty() {
            session.submit_and_wait_one()?;
            continue;
        }
        for (ud, res) in cqes {
            let (kind, ep, slot) = uring_session::unpack_ud(ud);
            let i = slot as usize;
            if kind != uring_session::KIND_INDEX_BUILD || ep != epoch || i >= jobs.len() {
                return Err(StoreError::Corrupt(
                    "invariant: io_uring index build unexpected cqe",
                ));
            }
            inflight -= 1;
            let job = &mut jobs[i];
            if res < 0 || res as usize != job.buf.len() {
                libc_complete(job, res.max(0) as usize)?;
            }
        }
    }
    Ok(())
}

/// Move a plan's buffers into `jobs` (in the plan's read order).
#[doc(hidden)]
pub fn take_reads<'p, 'b>(
    jobs: &mut Vec<ReadJob<'p>>,
    file: (IoHandle, &'p Path),
    reads: impl Iterator<Item = (u64, &'b mut Vec<u8>)>,
) -> std::ops::Range<usize> {
    let start = jobs.len();
    for (off, buf) in reads {
        jobs.push(ReadJob {
            fd: file.0,
            path: file.1,
            off,
            buf: std::mem::take(buf),
        });
    }
    start..jobs.len()
}

/// Move filled buffers back into the plan (same read order as `take_reads`).
#[doc(hidden)]
pub fn give_back<'b>(
    jobs: &mut [ReadJob<'_>],
    range: std::ops::Range<usize>,
    reads: impl Iterator<Item = (u64, &'b mut Vec<u8>)>,
) {
    for ((_, buf), job) in reads.zip(&mut jobs[range]) {
        *buf = std::mem::take(&mut job.buf);
    }
}

/// Read `heights` (consecutive) and every parent their creates spend.
pub fn read_index_window(
    table: &TxTable,
    heights: &[IndexHeight],
) -> Result<IndexWindow, StoreError> {
    match uring_session::with_thread_local(ENTRIES, |s| {
        // `read_window` owns the SQE buffers and `run_on_session` drains them
        // before returning. This guard only aborts if that contract breaks.
        let mut s = s.abort_guard();
        read_window(table, heights, &mut Reader::Session(&mut s))
    }) {
        Ok(r) => r,
        Err(StoreError::Unavailable) => read_window(table, heights, &mut Reader::Serial),
        Err(e) => Err(e),
    }
}

#[doc(hidden)]
pub fn missing(what: &'static str) -> StoreError {
    StoreError::Corrupt(what)
}

/// `hi` is the exclusive end of a span in a published body.
#[doc(hidden)]
pub fn require_body_hwm(hwm: u64, hi: u64) -> Result<(), StoreError> {
    if hi > hwm {
        return Err(StoreError::Corrupt(
            "invariant: body read past published end",
        ));
    }
    Ok(())
}

/// Parent fk → (txid, outputs).
#[doc(hidden)]
pub type Parents = U64Map<([u8; 32], Vec<OutputRecord>)>;
/// Create fk → `(block, tx)` within a window.
#[doc(hidden)]
pub type InWindow = U64Map<(usize, usize)>;
/// Stage 3 results: parent locators and parent txids.
#[doc(hidden)]
pub type ParentLocs = (Vec<Option<CreateLocPair>>, Vec<[u8; 32]>);

/// Handles and paths every stage reads through.
#[doc(hidden)]
pub struct Files<'t> {
    pub body: (IoHandle, &'t Path),
    pub seqsigwit: (IoHandle, &'t Path),
    pub txid: (IoHandle, &'t Path),
    pub input_body: (IoHandle, &'t Path),
}

/// Stage 1 results.
#[doc(hidden)]
pub struct Locators {
    pub locs: Vec<Option<CreateLocPair>>,
    pub sws_ranges: Vec<Option<(u64, u64)>>,
    pub edge_plans: Vec<EdgesPlan>,
}

#[doc(hidden)]
pub fn read_window(
    table: &TxTable,
    heights: &[IndexHeight],
    reader: &mut Reader<'_>,
) -> Result<IndexWindow, StoreError> {
    let want_txids = heights.iter().any(|h| h.tweaks);
    let files = Files {
        body: (table.body.body_read_fd(), table.body.body_file_path()),
        seqsigwit: (
            table.seqsigwit.body_read_fd(),
            table.seqsigwit.body_file_path(),
        ),
        txid: (table.txids.body_read_fd(), table.txids.file_path()),
        input_body: table.input.files().1,
    };
    let locators = read_locators(table, heights, reader)?;
    let (mut blocks, in_window) =
        read_blocks(table, &files, heights, &locators, want_txids, reader)?;
    let parent_fks = outside_parents(&blocks, &in_window);
    let (plocs, parent_txids) = read_witnesses_and_parent_locs(
        table,
        &files,
        heights,
        &locators.sws_ranges,
        &mut blocks,
        &parent_fks,
        want_txids,
        reader,
    )?;
    let parents = read_parents(table, &files, &parent_fks, &plocs, parent_txids, reader)?;
    Ok(IndexWindow {
        blocks,
        parents,
        in_window,
    })
}

#[doc(hidden)]
pub fn creates_of<'h>(heights: impl Iterator<Item = &'h IndexHeight>) -> Vec<Fk> {
    heights
        .flat_map(|h| (0..u64::from(h.n)).map(move |i| Fk(h.first.0 + i)))
        .collect()
}

/// Stage 1: `create.loc` and `seqsigwit.loc` windows and `input.loc` spans.
#[doc(hidden)]
pub fn read_locators(
    table: &TxTable,
    heights: &[IndexHeight],
    reader: &mut Reader<'_>,
) -> Result<Locators, StoreError> {
    let mut loc_plan = table
        .create_loc
        .plan_range_batch(&creates_of(heights.iter()))?;
    // Prune keeps witnesses in the query window, not on this stem.
    let sws_creates = if table.prune_seqsigwit_mode() {
        Vec::new()
    } else {
        creates_of(heights.iter().filter(|h| h.tweaks))
    };
    let mut sws_plan = table.seqsigwit_loc.plan_range_batch(&sws_creates)?;
    let mut edge_plans = heights
        .iter()
        .map(|h| {
            table
                .input
                .plan_edges_span(h.first.0, h.first.0 + u64::from(h.n) - 1)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut jobs = Vec::new();
    let loc_r = take_reads(&mut jobs, table.create_loc.loc_file(), loc_plan.reads());
    let sws_r = take_reads(&mut jobs, table.seqsigwit_loc.loc_file(), sws_plan.reads());
    let edge_r = take_reads(
        &mut jobs,
        table.input.files().0,
        edge_plans.iter_mut().map(|p| (p.loc_off, &mut p.loc)),
    );
    reader.run(&mut jobs)?;
    give_back(&mut jobs, loc_r, loc_plan.reads());
    give_back(&mut jobs, sws_r, sws_plan.reads());
    give_back(
        &mut jobs,
        edge_r,
        edge_plans.iter_mut().map(|p| (p.loc_off, &mut p.loc)),
    );
    Ok(Locators {
        locs: table.create_loc.finish_range_batch(&loc_plan)?,
        sws_ranges: table.seqsigwit_loc.finish_range_batch(&sws_plan)?,
        edge_plans,
    })
}

/// Stage 2: each block's `txout`, `input.body`, and (tweaks) `txid.body` spans.
#[doc(hidden)]
pub fn read_blocks(
    table: &TxTable,
    files: &Files<'_>,
    heights: &[IndexHeight],
    locators: &Locators,
    want_txids: bool,
    reader: &mut Reader<'_>,
) -> Result<(Vec<IndexBlock>, InWindow), StoreError> {
    let mut jobs = Vec::new();
    let mut spans = Vec::with_capacity(heights.len());
    let mut at = 0usize;
    for (h, plan) in heights.iter().zip(&locators.edge_plans) {
        let pairs = locators.locs[at..at + h.n as usize]
            .iter()
            .map(|p| p.ok_or_else(|| missing("invariant: index build create loc missing")))
            .collect::<Result<Vec<_>, _>>()?;
        at += h.n as usize;
        let lo = pairs.iter().map(|p| p.txout.0).min().unwrap_or(0);
        let mut hi = 0u64;
        for p in &pairs {
            let end = p
                .txout
                .0
                .checked_add(p.txout.1)
                .ok_or(StoreError::Corrupt("body span too large"))?;
            if end > hi {
                hi = end;
            }
        }
        let span = hi
            .checked_sub(lo)
            .ok_or(StoreError::Corrupt("body span too large"))?;
        spans.push((jobs.len(), lo, pairs));
        // One compare against the body HWM. Not a fresh `published_meta` pair.
        require_body_hwm(table.body.body_logical_len(), hi)?;
        jobs.push(ReadJob::new(files.body, lo, span)?);
        let (abs, len) = table.input.edges_body_read(plan)?;
        jobs.push(ReadJob::new(files.input_body, abs, len)?);
        if want_txids {
            let off = TxidBody::entry_offset(h.first.0)?;
            jobs.push(ReadJob::new(
                files.txid,
                off,
                u64::from(h.n) * TXID_ENTRY_LEN,
            )?);
        }
    }
    reader.run(&mut jobs)?;
    let mut blocks = Vec::with_capacity(heights.len());
    let mut in_window = U64Map::default();
    for (bi, ((h, plan), (j, lo, pairs))) in heights
        .iter()
        .zip(&locators.edge_plans)
        .zip(spans)
        .enumerate()
    {
        let txids = want_txids.then(|| jobs[j + 2].buf.as_slice());
        let block = decode_block(
            table,
            h,
            plan,
            &jobs[j].buf,
            lo,
            &pairs,
            &jobs[j + 1].buf,
            txids,
        )?;
        for (ti, tx) in block.txs.iter().enumerate() {
            in_window.insert(tx.fk.0, (bi, ti));
        }
        blocks.push(block);
    }
    Ok((blocks, in_window))
}

#[allow(clippy::too_many_arguments)] // one block's already-read spans
#[doc(hidden)]
pub fn decode_block(
    table: &TxTable,
    h: &IndexHeight,
    plan: &EdgesPlan,
    txout_span: &[u8],
    lo: u64,
    pairs: &[CreateLocPair],
    edge_span: &[u8],
    txids: Option<&[u8]>,
) -> Result<IndexBlock, StoreError> {
    let edges = decode_edges_span(plan, edge_span)?
        .into_iter()
        .map(|e| e.ok_or_else(|| missing("invariant: index build input edges unstamped")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut txs = Vec::with_capacity(pairs.len());
    for (ti, pair) in pairs.iter().enumerate() {
        let rel = (pair.txout.0 - lo) as usize;
        let raw = &txout_span[rel..rel + pair.txout.1 as usize];
        let (mut rec, outs, _) =
            decode_packed_tx_outs_with_spender_rels_secret(raw, pair.n_out, Some(&table.secret))?;
        if let Some(ids) = txids {
            rec.txid.copy_from_slice(&ids[ti * 32..ti * 32 + 32]);
        }
        rec.input_count = edges[ti].len() as u32;
        let need_seqsigwit = h.tweaks && outs.iter().any(|o| crate::scripthash::is_p2tr(&o.script));
        txs.push(LoadedTweakTx {
            fk: Fk(h.first.0 + ti as u64),
            rec,
            outs,
            need_seqsigwit,
            inputs: None,
        });
    }
    Ok(IndexBlock {
        height: h.height,
        header_fk: h.header_fk,
        hash: [0u8; 32],
        txs,
        edges,
    })
}

/// Parent fks the window's inputs spend that are not themselves in it.
#[doc(hidden)]
pub fn outside_parents(blocks: &[IndexBlock], in_window: &InWindow) -> Vec<u64> {
    let mut fks: Vec<u64> = blocks
        .iter()
        .flat_map(|b| b.edges.iter().flatten())
        .map(|e| e.parent.0)
        .filter(|&fk| fk != 0 && !in_window.contains_key(&fk))
        .collect();
    fks.sort_unstable();
    fks.dedup();
    fks
}

/// Stage 3: parent `create.loc`, P2TR-output txs' `seqsigwit.body`, and
/// (tweaks) parent txids. Fills each such tx's `inputs`.
#[allow(clippy::too_many_arguments)] // stage inputs; one call site
#[doc(hidden)]
pub fn read_witnesses_and_parent_locs(
    table: &TxTable,
    files: &Files<'_>,
    heights: &[IndexHeight],
    sws_ranges: &[Option<(u64, u64)>],
    blocks: &mut [IndexBlock],
    parent_fks: &[u64],
    want_txids: bool,
    reader: &mut Reader<'_>,
) -> Result<ParentLocs, StoreError> {
    let parent_list: Vec<Fk> = parent_fks.iter().map(|&f| Fk(f)).collect();
    let mut ploc_plan = table.create_loc.plan_range_batch(&parent_list)?;
    let mut jobs = Vec::new();
    let ploc_r = take_reads(&mut jobs, table.create_loc.loc_file(), ploc_plan.reads());
    let mut sws_jobs = Vec::new();
    if !table.prune_seqsigwit_mode() {
        let tweak_txs = blocks
            .iter()
            .zip(heights)
            .enumerate()
            .filter(|(_, (_, h))| h.tweaks)
            .flat_map(|(bi, (b, _))| (0..b.txs.len()).map(move |ti| (bi, ti)));
        let sws_hwm = table.seqsigwit.body_logical_len();
        for ((bi, ti), range) in tweak_txs.zip(sws_ranges) {
            if blocks[bi].txs[ti].need_seqsigwit {
                let (off, len) =
                    range.ok_or_else(|| missing("invariant: index build seqsigwit loc missing"))?;
                // One compare against the seqsigwit HWM. Not a fresh `published_meta` pair.
                require_body_hwm(sws_hwm, off.saturating_add(len))?;
                sws_jobs.push((bi, ti, jobs.len()));
                jobs.push(ReadJob::new(files.seqsigwit, off, len)?);
            }
        }
    }
    let txid_j = jobs.len();
    if want_txids {
        for &fk in parent_fks {
            jobs.push(ReadJob::new(
                files.txid,
                TxidBody::entry_offset(fk)?,
                TXID_ENTRY_LEN,
            )?);
        }
    }
    reader.run(&mut jobs)?;
    give_back(&mut jobs, ploc_r, ploc_plan.reads());
    for (bi, ti, j) in sws_jobs {
        let n_in = blocks[bi].txs[ti].rec.input_count;
        let mut ins = decode_seqsigwit_secret(&jobs[j].buf, n_in, Some(&table.secret))?;
        apply_input_edges(&mut ins, &blocks[bi].edges[ti])?;
        blocks[bi].txs[ti].inputs = Some(ins);
    }
    let txids = (0..parent_fks.len())
        .map(|k| match jobs.get(txid_j + k) {
            Some(job) if want_txids => job.buf.as_slice().try_into().unwrap_or([0u8; 32]),
            _ => [0u8; 32],
        })
        .collect();
    Ok((table.create_loc.finish_range_batch(&ploc_plan)?, txids))
}

/// Stage 4: parent `txout` records.
#[doc(hidden)]
pub fn read_parents(
    table: &TxTable,
    files: &Files<'_>,
    parent_fks: &[u64],
    plocs: &[Option<CreateLocPair>],
    txids: Vec<[u8; 32]>,
    reader: &mut Reader<'_>,
) -> Result<Parents, StoreError> {
    let pairs = plocs
        .iter()
        .map(|p| p.ok_or_else(|| missing("invariant: index build parent loc missing")))
        .collect::<Result<Vec<_>, _>>()?;
    let hwm = table.body.body_logical_len();
    for p in &pairs {
        // One compare against the body HWM. Not a fresh `published_meta` pair.
        require_body_hwm(hwm, p.txout.0.saturating_add(p.txout.1))?;
    }
    let mut jobs = Vec::with_capacity(pairs.len());
    for p in &pairs {
        jobs.push(ReadJob::new(files.body, p.txout.0, p.txout.1)?);
    }
    reader.run(&mut jobs)?;
    let mut parents = U64Map::default();
    for (((fk, txid), job), p) in parent_fks.iter().zip(txids).zip(&jobs).zip(&pairs) {
        let (_, outs, _) =
            decode_packed_tx_outs_with_spender_rels_secret(&job.buf, p.n_out, Some(&table.secret))?;
        parents.insert(*fk, (txid, outs));
    }
    Ok(parents)
}
