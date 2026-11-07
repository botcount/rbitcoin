//! Thin BIP-352 tweak index: Query join of `sp_tweaks.*` + Class A.

use super::*;
use rbitcoin_store::{SpTweaksTable, TrimLast};

/// Eligible tx after thin-index join (P2TR outs only).
#[derive(Clone, Debug)]
pub struct ThinTweakRow {
    pub txid: [u8; 32],
    pub tweak: [u8; 33],
    pub p2tr: Vec<(u32, [u8; 32], u64)>,
}

/// Budgets for [`Query::load_thin_tweaks_range`] (serve-side multi-height wave).
///
/// Cost is eligible txs / Class A body, not height count alone — pair `max_heights`
/// with `max_eligible` so busy post-taproot blocks do not explode RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinTweakRangeLimits {
    /// Cap on contiguous heights in one wave (default 128).
    pub max_heights: u32,
    /// Cap on total eligible txs across the wave (default 16384).
    pub max_eligible: usize,
    /// Drop confirmed-spent P2TR outs (and txs with none left). Cake
    /// `historicalMode=false` / param `[2]=false`.
    pub cut_through: bool,
}

impl Default for ThinTweakRangeLimits {
    fn default() -> Self {
        Self {
            max_heights: 128,
            max_eligible: 16384,
            cut_through: false,
        }
    }
}

#[doc(hidden)]
pub fn require_thin_body_range(r: Option<(u64, u64)>) -> Result<(u64, u64), StoreError> {
    match r {
        None => Err(StoreError::Corrupt(
            "invariant: thin tweak eligible body missing",
        )),
        Some((_, 0)) => Err(StoreError::Corrupt(
            "invariant: thin tweak eligible body empty",
        )),
        Some((off, len)) => Ok((off, len)),
    }
}

#[doc(hidden)]
pub fn wave_join_is_dense(elig_count: usize, first_id: u64, last_id: u64) -> bool {
    if elig_count == 0 || last_id < first_id {
        return false;
    }
    let Ok(span) = usize::try_from(last_id - first_id + 1) else {
        return false;
    };
    elig_count.saturating_mul(4) >= span
}

#[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
#[doc(hidden)]
pub fn thin_join_txids_and_loc(
    store: &Store,
    elig_fks: &[Fk],
) -> Result<
    (
        Vec<Option<[u8; 32]>>,
        Vec<Option<rbitcoin_store::CreateLocPair>>,
    ),
    StoreError,
> {
    let Some(first_id) = elig_fks.first().and_then(|f| f.get()) else {
        return Err(StoreError::InvalidFk);
    };
    let Some(last_id) = elig_fks.last().and_then(|f| f.get()) else {
        return Err(StoreError::InvalidFk);
    };
    if wave_join_is_dense(elig_fks.len(), first_id, last_id) {
        let all_txids = store.txs.txid_sidefile().get_range(first_id, last_id)?;
        let span_fks: Vec<Fk> = (first_id..=last_id).map(Fk).collect();
        let all_loc = store.tx_create_loc_range_batch(&span_fks)?;
        let n = (last_id - first_id + 1) as usize;
        if all_txids.len() != n || all_loc.len() != n {
            return Err(StoreError::Corrupt(
                "invariant: thin tweak dense join length mismatch",
            ));
        }
        let mut txids = Vec::with_capacity(elig_fks.len());
        let mut loc = Vec::with_capacity(elig_fks.len());
        for fk in elig_fks {
            let id = fk.get().ok_or(StoreError::InvalidFk)?;
            let i = (id - first_id) as usize;
            if i >= n {
                return Err(StoreError::Corrupt(
                    "invariant: thin tweak dense join fk outside span",
                ));
            }
            txids.push(Some(all_txids[i]));
            loc.push(all_loc[i]);
        }
        Ok((txids, loc))
    } else {
        Ok((
            store.txs.txid_sidefile().get_many(elig_fks)?,
            store.tx_create_loc_range_batch(elig_fks)?,
        ))
    }
}

#[doc(hidden)]
pub struct HeightPlan {
    pub height: Height,
    /// `confirmed[height]` as read with `elig`, under the `sp_tweaks` lock.
    pub header_fk: Fk,
    pub first_id: u64,
    pub elig: Vec<(u32, [u8; 33])>,
}

#[doc(hidden)]
pub fn thin_tweak_height_plans(
    store: &Store,
    t: &SpTweaksTable,
    start: Height,
    limits: ThinTweakRangeLimits,
) -> Result<Vec<HeightPlan>, QueryError> {
    let mut meta: Vec<(Height, u64, u32, Fk)> = Vec::new();
    for step in 0..limits.max_heights {
        let h = Height(start.0.saturating_add(step));
        let Some(header_fk) = store.confirmed.get(h)? else {
            break;
        };
        let Some((first_fk, n_tx)) = store.header_txs.get_range(header_fk)? else {
            break;
        };
        let Some(first_id) = first_fk.get() else {
            return Err(StoreError::InvalidFk);
        };
        meta.push((h, first_id, n_tx, header_fk));
    }
    if meta.is_empty() {
        return Ok(Vec::new());
    }
    let n_txs: Vec<u32> = meta.iter().map(|m| m.2).collect();
    let Some(eligs) = t.get_eligible_range(meta[0].0, &n_txs)? else {
        return Ok(Vec::new());
    };
    let mut plans = Vec::new();
    let mut elig_total = 0usize;
    for (i, elig) in eligs.into_iter().enumerate() {
        let add = elig.len();
        if !plans.is_empty()
            && limits.max_eligible != usize::MAX
            && elig_total.saturating_add(add) > limits.max_eligible
        {
            break;
        }
        elig_total = elig_total.saturating_add(add);
        plans.push(HeightPlan {
            height: meta[i].0,
            header_fk: meta[i].3,
            first_id: meta[i].1,
            elig,
        });
    }
    Ok(plans)
}

impl Query {
    pub fn sptweaks_enabled(&self) -> bool {
        self.sptweaks_enabled.load(AtomicOrdering::Acquire)
    }

    /// Enable persist + serve-from-index + backfill. Creates empty files if needed.
    ///
    /// Does **not** gate Electrum: naive walk remains when off / hole.
    /// Under `--prune-seqsigwit` the index has to be on before `origin`
    /// (taproot), or already sealed through every dropped height.
    pub fn set_sptweaks_enabled(&self, on: bool, origin: Height) -> Result<(), QueryError> {
        if on {
            self.require_sp_tweaks_start(origin)?;
        }
        self.sptweaks_origin
            .store(origin.0, AtomicOrdering::Release);
        if on {
            self.ensure_sp_tweaks(origin)?;
        }
        self.sptweaks_enabled.store(on, AtomicOrdering::Release);
        Ok(())
    }

    /// Next height the table will accept, or `origin` when no table exists.
    fn tweak_watermark(&self, origin: Height) -> Result<u32, QueryError> {
        if let Some(h) = self.sptweaks_next_height() {
            return Ok(h.0.max(origin.0));
        }
        let dir = self.store.cold_files_dir();
        if SpTweaksTable::files_present(dir) {
            return Ok(SpTweaksTable::open(dir)?.next_height().0.max(origin.0));
        }
        Ok(origin.0)
    }

    /// Prune drops scriptSig and witness at or below [`Self::pruneheight`].
    /// A new index can start only while the tip is still below `origin`.
    /// A restart is allowed when the watermark is already past that line
    /// (or past the tip, before any height has been dropped).
    pub(crate) fn require_sp_tweaks_start(&self, origin: Height) -> Result<(), QueryError> {
        if !self.prune_seqsigwit() {
            return Ok(());
        }
        let tip = self.tip_height().map(|h| h.0);
        if tip.is_none_or(|t| t < origin.0) {
            return Ok(());
        }
        let next = self.tweak_watermark(origin)?;
        let covered = match self.pruneheight() {
            Some(ph) => next > ph.0,
            None => next > tip.unwrap_or(0),
        };
        if covered {
            return Ok(());
        }
        Err(StoreError::Layout(
            "silent payment tweaks on --prune-seqsigwit must be enabled before taproot activation"
                .into(),
        ))
    }

    #[doc(hidden)]
    pub fn ensure_sp_tweaks(&self, origin: Height) -> Result<(), QueryError> {
        let mut g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let t = SpTweaksTable::open_or_create(self.store.cold_files_dir(), origin)?;
            self.repair_sp_tweaks(&t)?;
            *g = Some(t);
            self.sptweaks_origin
                .store(origin.0, AtomicOrdering::Release);
        }
        Ok(())
    }

    /// Trim the tweak table to what the chain backs after a crash: drop
    /// heights above the tip (a disconnect whose truncate never ran), then
    /// fit the last record to its block's tx count.
    pub fn repair_sp_tweaks(&self, t: &SpTweaksTable) -> Result<(), QueryError> {
        let before = t.next_height();
        t.truncate_through_tip(self.tip_height())?;
        while let Some(h) = t
            .next_height()
            .0
            .checked_sub(1)
            .filter(|&h| h >= t.origin_height().0)
        {
            let header_fk = self
                .store
                .confirmed
                .get(Height(h))?
                .ok_or(StoreError::Corrupt(
                    "invariant: sp_tweaks height below tip not confirmed",
                ))?;
            let (_, n_tx) = self
                .store
                .header_txs
                .get_range(header_fk)?
                .ok_or(StoreError::Corrupt("confirmed header missing body list"))?;
            match t.trim_last_record(n_tx)? {
                TrimLast::Clean => break,
                TrimLast::Cut => {
                    rbitcoin_log::warn!("sp_tweaks: cut uncommitted body bytes after height {h}");
                    break;
                }
                TrimLast::Dropped => {
                    rbitcoin_log::warn!("sp_tweaks: dropped torn record at height {h}");
                }
            }
        }
        let after = t.next_height();
        if after != before {
            rbitcoin_log::warn!(
                "sp_tweaks: repaired to the chain: next {} → {}",
                before.0,
                after.0
            );
        }
        Ok(())
    }

    /// The naive walk reads scriptSig and witness. A pruned hole cannot.
    pub fn require_sp_tweaks_unpruned(&self) -> Result<(), QueryError> {
        if self.prune_seqsigwit() {
            return Err(StoreError::Layout(
                "silent payment tweaks are unavailable with --prune-seqsigwit".into(),
            ));
        }
        Ok(())
    }

    /// Next tweak height to seal (`None` when the tweak index is off).
    pub fn tweak_index_next(&self) -> Option<u32> {
        if !self.sptweaks_enabled() {
            return None;
        }
        let origin = self.sptweaks_origin().0;
        Some(self.sptweaks_next_height()?.0.max(origin))
    }

    #[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
    /// Commit tweak records for consecutive heights (a window) under the
    /// index write-behind lock. Returns heights committed; 0 when the table's
    /// next height or a `confirmed[h]` moved (a reorg).
    pub fn commit_window_tweaks(
        &self,
        items: &[(Height, Fk, Vec<Option<[u8; 33]>>)],
    ) -> Result<u32, QueryError> {
        let _appender = self.bf_wb.lock_appender();
        let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_ref() else {
            return Ok(0);
        };
        if items.first().is_none_or(|i| i.0 != t.next_height()) {
            return Ok(0);
        }
        for (h, header_fk, _) in items {
            if self.store.confirmed.get(*h)? != Some(*header_fk) {
                return Ok(0);
            }
        }
        let refs: Vec<(Height, &[Option<[u8; 33]>])> =
            items.iter().map(|(h, _, r)| (*h, r.as_slice())).collect();
        t.put_blocks(&refs)?;
        Ok(items.len() as u32)
    }

    pub fn sptweaks_origin(&self) -> Height {
        Height(self.sptweaks_origin.load(AtomicOrdering::Acquire))
    }

    pub fn sptweaks_next_height(&self) -> Option<Height> {
        let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|t| t.next_height())
    }

    /// Write one height of aligned per-tx tweaks (`None` = ineligible).
    ///
    /// No-op when the flag is off, the table is missing, `height` is not next,
    /// or `height` is not yet confirmed. `header_fk` must be the confirmed tip
    /// header at `height` (the idx does not store it).
    pub fn put_sp_tweaks_block(
        &self,
        height: Height,
        header_fk: Fk,
        records: &[Option<[u8; 33]>],
    ) -> Result<(), QueryError> {
        if !self.sptweaks_enabled() {
            return Ok(());
        }
        let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_ref() else {
            return Ok(());
        };
        if height != t.next_height() {
            return Ok(());
        }
        match self.store.confirmed.get(height)? {
            None => return Ok(()),
            Some(fk) if fk != header_fk => {
                return Err(StoreError::Corrupt(
                    "sp_tweaks put header is not confirmed tip",
                ));
            }
            Some(_) => {}
        }
        t.put_block(height, records)
    }

    #[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
    /// Consecutive heights: one body pwrite + one idx pwrite. Same checks as
    /// [`Self::put_sp_tweaks_block`] on each item; no-op if the first is not next.
    pub fn put_sp_tweaks_blocks(
        &self,
        items: &[(Height, Fk, Vec<Option<[u8; 33]>>)],
    ) -> Result<(), QueryError> {
        if items.is_empty() {
            return Ok(());
        }
        if !self.sptweaks_enabled() {
            return Ok(());
        }
        let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_ref() else {
            return Ok(());
        };
        if items[0].0 != t.next_height() {
            return Ok(());
        }
        for (height, header_fk, _) in items {
            match self.store.confirmed.get(*height)? {
                None => return Ok(()),
                Some(fk) if fk != *header_fk => {
                    return Err(StoreError::Corrupt(
                        "sp_tweaks put header is not confirmed tip",
                    ));
                }
                Some(_) => {}
            }
        }
        let refs: Vec<(Height, &[Option<[u8; 33]>])> =
            items.iter().map(|(h, _, r)| (*h, r.as_slice())).collect();
        t.put_blocks(&refs)
    }

    pub fn truncate_sp_tweaks_through_tip(&self, tip: Option<Height>) -> Result<(), QueryError> {
        let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_ref() else {
            return Ok(());
        };
        t.truncate_through_tip(tip)
    }

    /// Indexed height, or `None` if hole / no table / missing header.
    ///
    /// Returns **only eligible** txs (`len=33`). See [`Self::load_thin_tweaks_range`].
    pub fn load_thin_tweaks(
        &self,
        height: Height,
    ) -> Result<Option<Vec<ThinTweakRow>>, QueryError> {
        // One height: the eligible cap applies only when a later height is added.
        let mut batch = self.load_thin_tweaks_range(
            height,
            ThinTweakRangeLimits {
                max_heights: 1,
                ..ThinTweakRangeLimits::default()
            },
        )?;
        Ok(batch.pop().map(|(_, rows)| rows))
    }

    /// Contiguous thin-index heights starting at `start`, stopped by tip hole,
    /// index hole, or [`ThinTweakRangeLimits`].
    ///
    /// Empty `Ok(vec![])` means the first height is not indexed (caller falls
    /// back per-height). Eligible Class A join is **one sequential `txout`
    /// span** from first..=last eligible fk in the wave (ineligible txout in
    /// the hole is included; `seqsigwit` is not). `sp_tweaks` mutex is not held
    /// during Class A IO. `limits.cut_through` drops confirmed-spent P2TR
    /// outs after the join (spent range from the same `create.loc` pair as the
    /// body join, then one spent-body walk per create; txs with none left are
    /// omitted; the height remains).
    pub fn load_thin_tweaks_range(
        &self,
        start: Height,
        limits: ThinTweakRangeLimits,
    ) -> Result<Vec<(Height, Vec<ThinTweakRow>)>, QueryError> {
        Ok(self
            .load_thin_tweaks_range_with_header(start, limits)?
            .into_iter()
            .map(|(h, _, rows)| (h, rows))
            .collect())
    }

    /// [`Self::load_thin_tweaks_range`], with the header fk each height's rows
    /// were planned under, so a caller can tell which block they belong to.
    pub fn load_thin_tweaks_range_with_header(
        &self,
        start: Height,
        limits: ThinTweakRangeLimits,
    ) -> Result<Vec<(Height, Fk, Vec<ThinTweakRow>)>, QueryError> {
        if limits.max_heights == 0 {
            return Ok(Vec::new());
        }
        let plans: Vec<HeightPlan> = {
            let g = self.sp_tweaks.lock().unwrap_or_else(|e| e.into_inner());
            let Some(t) = g.as_ref() else {
                return Ok(Vec::new());
            };
            thin_tweak_height_plans(&self.store, t, start, limits)?
        };

        if plans.is_empty() {
            return Ok(Vec::new());
        }

        let mut elig_fks: Vec<Fk> = Vec::new();
        let mut tag: Vec<(usize, usize)> = Vec::new();
        for (pi, p) in plans.iter().enumerate() {
            for (ei, &(tx_i, _)) in p.elig.iter().enumerate() {
                elig_fks.push(Fk(p.first_id.saturating_add(u64::from(tx_i))));
                tag.push((pi, ei));
            }
        }

        let mut out_rows: Vec<Vec<ThinTweakRow>> = plans
            .iter()
            .map(|p| Vec::with_capacity(p.elig.len()))
            .collect();

        let loc_pairs: Vec<Option<rbitcoin_store::CreateLocPair>> = if elig_fks.is_empty() {
            Vec::new()
        } else {
            let (txids, loc) = thin_join_txids_and_loc(&self.store, &elig_fks)?;
            let mut span_off = u64::MAX;
            let mut span_end = 0u64;
            for p in &loc {
                let (off, len) = require_thin_body_range(p.as_ref().map(|x| x.txout))?;
                span_off = span_off.min(off);
                span_end = span_end.max(off.saturating_add(len));
            }
            let span_len = span_end - span_off;
            let mut span_buf = Vec::new();
            self.store
                .txs
                .with_body_span_into(span_off, span_len, &mut span_buf, |raw| {
                    for (i, p) in loc.iter().enumerate() {
                        let pair = p.as_ref().ok_or(StoreError::Corrupt(
                            "invariant: thin tweak eligible loc missing",
                        ))?;
                        let (off, len) = pair.txout;
                        let rel = (off - span_off) as usize;
                        let sl = raw.get(rel..rel.saturating_add(len as usize)).ok_or(
                            StoreError::Corrupt(
                                "invariant: thin tweak eligible body span truncated",
                            ),
                        )?;
                        let Some(txid) = txids.get(i).copied().flatten() else {
                            return Err(StoreError::Corrupt(
                                "invariant: thin tweak eligible txid missing",
                            ));
                        };
                        let (pi, ei) = tag[i];
                        let p2tr = self.store.txs.packed_p2tr_from_raw(sl, pair.n_out)?;
                        out_rows[pi].push(ThinTweakRow {
                            txid,
                            tweak: plans[pi].elig[ei].1,
                            p2tr,
                        });
                    }
                    Ok(())
                })?;
            self.note_thin_tweak_body_bytes(span_len);
            loc
        };

        if limits.cut_through && !elig_fks.is_empty() {
            let n_rows: usize = out_rows.iter().map(Vec::len).sum();
            if n_rows != elig_fks.len() {
                return Err(StoreError::Corrupt(
                    "invariant: thin cut_through row/fk count",
                ));
            }
            if loc_pairs.len() != elig_fks.len() {
                return Err(StoreError::Corrupt("invariant: spent_range_batch length"));
            }
            let mut i = 0usize;
            let mut vouts = Vec::new();
            for rows in &mut out_rows {
                let mut w = 0;
                for r in 0..rows.len() {
                    vouts.clear();
                    vouts.extend(rows[r].p2tr.iter().map(|p| p.0));
                    let spent = loc_pairs[i].as_ref().map(|p| p.spent);
                    let live = self
                        .store
                        .unspent_create_vouts(elig_fks[i], &vouts, spent)?;
                    i += 1;
                    rbitcoin_store::keep_unspent_vout_subsequence(&mut rows[r].p2tr, &live, |p| {
                        p.0
                    });
                    if !rows[r].p2tr.is_empty() {
                        if w != r {
                            rows.swap(w, r);
                        }
                        w += 1;
                    }
                }
                rows.truncate(w);
            }
            if i != elig_fks.len() {
                return Err(StoreError::Corrupt("invariant: thin cut_through live rows"));
            }
        }

        Ok(plans
            .into_iter()
            .zip(out_rows)
            .map(|(p, rows)| (p.height, p.header_fk, rows))
            .collect())
    }
}

/// One block's P2TR output keys and spent outpoints, read without any parent
/// tx. See [`Query::block_sp_outpoints`].
#[derive(Clone, Debug, Default)]
pub struct BlockSpOutpoints {
    /// X-only key of every P2TR output, eligible or not, in tx order.
    pub p2tr_keys: Vec<[u8; 32]>,
    /// `(parent txid, vout)` of every non-coinbase input, P2TR or not: the
    /// edge does not carry the parent's script.
    pub spent: Vec<([u8; 32], u32)>,
}

impl Query {
    /// What the BlindBit v1 filters and spent index need from the block
    /// `header_fk`: one `txout` span of its own txs for the keys, and the
    /// input edges plus one page-grouped `txid.body` read for the spends.
    /// No parent txout, no `seqsigwit` (except for an unstamped create, whose
    /// prevouts are only there). `header_txs` keeps a disconnected header's
    /// range, so the result belongs to `header_fk` whatever `confirmed` says.
    pub fn block_sp_outpoints(
        &self,
        header_fk: Fk,
        keys: bool,
        spent: bool,
    ) -> Result<BlockSpOutpoints, QueryError> {
        let mut out = BlockSpOutpoints::default();
        let Some((first_fk, n_tx)) = self.store.header_txs.get_range(header_fk)? else {
            return Err(StoreError::NotFound);
        };
        let Some(first) = first_fk.get() else {
            return Err(StoreError::InvalidFk);
        };
        if n_tx == 0 {
            return Ok(out);
        }
        let last = first + u64::from(n_tx) - 1;
        let fks: Vec<Fk> = (first..=last).map(Fk).collect();

        if keys {
            let loc = self.store.tx_create_loc_range_batch(&fks)?;
            let mut span_off = u64::MAX;
            let mut span_end = 0u64;
            for p in &loc {
                let (off, len) = require_thin_body_range(p.as_ref().map(|x| x.txout))?;
                span_off = span_off.min(off);
                span_end = span_end.max(off.saturating_add(len));
            }
            let span_len = span_end - span_off;
            self.store.txs.with_body_span(span_off, span_len, |raw| {
                for p in loc.iter().flatten() {
                    let (off, len) = p.txout;
                    let rel = (off - span_off) as usize;
                    let sl = raw.get(rel..rel.saturating_add(len as usize)).ok_or(
                        StoreError::Corrupt("invariant: block sp body span truncated"),
                    )?;
                    for (_, key, _) in self.store.txs.packed_p2tr_from_raw(sl, p.n_out)? {
                        out.p2tr_keys.push(key);
                    }
                }
                Ok(())
            })?;
        }

        if spent {
            let edges = self.store.input_edges_span(first, last)?;
            let mut prevs: Vec<(Fk, u32)> = Vec::new();
            for (fk, e) in fks.iter().zip(edges) {
                match e {
                    Some(edges) => prevs.extend(
                        edges
                            .iter()
                            .filter(|e| !e.parent.is_null())
                            .map(|e| (e.parent, e.vout)),
                    ),
                    None => {
                        let (_, p) = self.store.get_tx_meta_and_prevouts(*fk)?;
                        prevs.extend(p.into_iter().filter(|(f, _)| !f.is_null()));
                    }
                }
            }
            let parents: Vec<Fk> = prevs.iter().map(|&(f, _)| f).collect();
            let txids = self.store.txids_get_many(&parents)?;
            out.spent.reserve(prevs.len());
            for ((_, vout), txid) in prevs.into_iter().zip(txids) {
                let txid = txid.ok_or(StoreError::Corrupt(
                    "invariant: block sp parent txid missing",
                ))?;
                out.spent.push((txid, vout));
            }
        }
        Ok(out)
    }
}
