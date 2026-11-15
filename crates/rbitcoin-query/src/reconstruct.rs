//! Reconstruct wire blocks/txs and merkle proofs.

use super::*;
use crate::U64Map;
use rbitcoin_consensus::block::{
    check_witness_commitment_with_wtxids, coinbase_has_witness_commitment,
    witness_commitment_script, witness_commitment_vout_index,
};
use std::time::Instant;

#[doc(hidden)]
pub const TXSTAT_BODY_ROW_BYTES: u64 = 8;

/// Stamped fee/weight rows from `txstat.body`; `None` means at least one row
/// in the block is unstamped. Byte count includes the coinbase cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTxStatRows {
    pub hash: [u8; 32],
    pub rows: Option<Vec<(u64, u64)>>,
    pub txstat_bytes: u64,
}

/// Stamped `txstat.body` rows for one stored block (every row non-zero).
#[derive(Debug, Clone)]
pub struct StampedTxstatBlock {
    pub rec: HeaderRecord,
    pub fks: Vec<Fk>,
    pub rows: Vec<rbitcoin_store::TxStatRow>,
    pub n_outs: Vec<u32>,
}

/// One transaction of [`Query::block_tx_summary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTxSummary {
    pub txid: [u8; 32],
    pub fee_sat: u64,
    pub weight: u64,
    pub value_sat: u64,
}

#[doc(hidden)]
pub fn block_size_weight_from_txstat(
    rows: &[rbitcoin_store::TxStatRow],
) -> Result<(u32, u32), StoreError> {
    let n = rows.len() as u64;
    let vi = bitcoin::consensus::encode::VarInt(n).size() as u64;
    let overhead = 80u64.saturating_add(vi);
    let mut tx_size = 0u64;
    let mut tx_wu = 0u64;
    for row in rows {
        tx_size = tx_size.saturating_add(row.size());
        tx_wu = tx_wu.saturating_add(row.weight());
    }
    let total = overhead.saturating_add(tx_size);
    let weight = overhead.saturating_mul(4).saturating_add(tx_wu);
    let size =
        u32::try_from(total).map_err(|_| StoreError::Corrupt("invariant: block size/weight"))?;
    let weight =
        u32::try_from(weight).map_err(|_| StoreError::Corrupt("invariant: block size/weight"))?;
    Ok((size, weight))
}

impl Query {
    /// Read confirmed block fee/weight rows from txstat only, without spent data.
    pub fn block_txstat_rows(&self, height: Height) -> Result<Option<BlockTxStatRows>, QueryError> {
        let Some((header_fk, header)) = self.header_at_height(height)? else {
            return Ok(None);
        };
        let hash = header.hash;
        let Some((first, n)) = self.store.header_txs.get_range(header_fk)? else {
            return Ok(None);
        };
        if n == 0 {
            return Ok(Some(BlockTxStatRows {
                hash,
                rows: None,
                txstat_bytes: 0,
            }));
        }
        let last = first
            .0
            .checked_add(u64::from(n - 1))
            .ok_or(StoreError::Corrupt("invariant: header_txs last fk"))?;
        let packed = self.store.txstat_range(header_fk, first.0, last)?;
        let txstat_bytes = u64::from(n).saturating_mul(TXSTAT_BODY_ROW_BYTES);
        if packed.len() != n as usize || packed.iter().any(Option::is_none) {
            return Ok(Some(BlockTxStatRows {
                hash,
                rows: None,
                txstat_bytes,
            }));
        }
        let rows = packed
            .into_iter()
            .skip(1)
            .flatten()
            .map(|r| (r.fee_sat, r.weight()))
            .collect();
        Ok(Some(BlockTxStatRows {
            hash,
            rows: Some(rows),
            txstat_bytes,
        }))
    }

    #[doc(hidden)]
    pub fn load_body_from_store(
        &self,
        fk: Fk,
    ) -> Result<(TxRecord, Vec<OutputRecord>, Vec<InputRecord>), QueryError> {
        let (tx, outs) = self.store.get_tx_meta_and_outputs(fk)?;
        if let Some(inputs) = self.seqsigwit_cached_inputs(fk, tx.input_count)? {
            if inputs.len() as u32 != tx.input_count {
                return Err(StoreError::Corrupt("packed input count mismatch"));
            }
            return Ok((tx, outs, inputs));
        }
        self.require_seqsigwit_fk(fk)?;
        let t0 = Instant::now();
        crate::note_confirm(&self.confirm_stats().wf_body_store, 1);
        let (tx, inputs, outs) = match self.store.get_tx_full(fk) {
            Ok(v) => v,
            Err(StoreError::NotFound) if self.prune_seqsigwit() => {
                let height = self.store.tx_height_get(fk)?.unwrap_or(0);
                return Err(StoreError::Pruned { height });
            }
            Err(e) => return Err(e),
        };
        crate::note_confirm(
            &self.confirm_stats().wf_body_store_ns,
            t0.elapsed().as_nanos() as u64,
        );
        Ok((tx, outs, inputs))
    }

    /// Create txids in block order from `txid.body` (no packed `txout` decode).
    pub fn block_txids(&self, height: Height) -> Result<Vec<[u8; 32]>, QueryError> {
        let header_fk = self
            .store
            .confirmed
            .get(height)?
            .ok_or(StoreError::NotFound)?;
        self.header_txids(header_fk)
    }

    /// [`Self::block_txids`] for one stored header, active or not.
    pub fn header_txids(&self, header_fk: Fk) -> Result<Vec<[u8; 32]>, QueryError> {
        let (first, n) = self
            .store
            .header_txs
            .get_range(header_fk)?
            .ok_or(StoreError::Corrupt("confirmed header missing body list"))?;
        if n == 0 {
            return Ok(Vec::new());
        }
        let last = first.0.saturating_add(u64::from(n.saturating_sub(1)));
        let ids = self.store.txs.body_txid_range(first.0, last)?;
        if ids.len() != n as usize {
            return Err(StoreError::Corrupt("invariant: block txid.body length"));
        }
        Ok(ids)
    }

    /// Create id at `index` in the confirmed block. A header with no Class A
    /// span is [`StoreError::NotFound`], not corrupt. Does not expand the span.
    pub fn tx_fk_at_height(&self, height: Height, index: u32) -> Result<Fk, QueryError> {
        let (first, n) = match self.tx_fk_span_at_height(height) {
            Ok(span) => span,
            Err(StoreError::Corrupt("confirmed header missing body list")) => {
                return Err(StoreError::NotFound);
            }
            Err(e) => return Err(e),
        };
        if index >= n {
            return Err(StoreError::NotFound);
        }
        Fk::new(first.0 + u64::from(index)).ok_or(StoreError::NotFound)
    }

    /// One create identity at `index` in the block (`txid.body` only).
    pub fn block_txid_at(&self, height: Height, index: usize) -> Result<[u8; 32], QueryError> {
        let index = u32::try_from(index).map_err(|_| StoreError::NotFound)?;
        let fk = self.tx_fk_at_height(height, index)?;
        self.store.txs.body_txid(fk)
    }

    pub fn merkle_proof(&self, height: Height, txid: &[u8; 32]) -> Result<MerkleProof, QueryError> {
        let txids = self.block_txids(height)?;
        let pos = txids
            .iter()
            .position(|t| t == txid)
            .ok_or(StoreError::NotFound)?;
        Ok(MerkleProof {
            block_height: height.0,
            pos,
            merkle: rbitcoin_store::merkle_branch(&txids, pos),
        })
    }

    pub fn block_tx_fks(&self, height: Height) -> Result<Vec<Fk>, QueryError> {
        let header_fk = self
            .store
            .confirmed
            .get(height)?
            .ok_or(StoreError::NotFound)?;
        self.store
            .header_txs
            .get_list(header_fk)?
            .ok_or(StoreError::Corrupt("confirmed header missing body list"))
    }

    /// Contiguous Class A fk span for the confirmed block at `height`.
    ///
    /// Position in the block is [`rbitcoin_store::HeaderTxsTable::index_in_span`].
    /// This does not expand the block's fk list. A confirmed header with no
    /// body span is the same corrupt miss as [`Self::block_tx_fks`].
    pub fn tx_fk_span_at_height(&self, height: Height) -> Result<(Fk, u32), QueryError> {
        let header_fk = self
            .store
            .confirmed
            .get(height)?
            .ok_or(StoreError::NotFound)?;
        self.store
            .header_txs
            .get_range(header_fk)?
            .ok_or(StoreError::Corrupt("confirmed header missing body list"))
    }

    #[doc(hidden)]
    pub fn contiguous_fk_run(fks: &[Fk]) -> Option<(u64, u64)> {
        let first = fks.first()?.get()?;
        if first == 0 {
            return None;
        }
        for (i, fk) in fks.iter().enumerate() {
            if fk.get() != Some(first + i as u64) {
                return None;
            }
        }
        Some((first, first + (fks.len() as u64).saturating_sub(1)))
    }

    /// Reconstruct a consensus `Transaction` from Class A rows (no stored raw).
    pub fn reconstruct_tx(&self, tx_fk: Fk) -> Result<Transaction, QueryError> {
        let (rec, stored_outputs, stored_inputs) = self.filled_body(tx_fk)?;
        Ok(Self::transaction_from_class_a(
            rec,
            stored_outputs,
            stored_inputs,
        ))
    }

    /// [`Self::reconstruct_tx`] plus the txid and parent edges that load already read.
    pub fn reconstruct_tx_with_inputs(
        &self,
        tx_fk: Fk,
    ) -> Result<(Transaction, [u8; 32], Vec<rbitcoin_store::InputParent>), QueryError> {
        let (rec, stored_outputs, stored_inputs) = self.filled_body(tx_fk)?;
        let txid = rec.txid;
        let parents = stored_inputs.iter().map(|inp| inp.parent).collect();
        let wire = Self::transaction_from_class_a(rec, stored_outputs, stored_inputs);
        Ok((wire, txid, parents))
    }

    fn filled_body(
        &self,
        tx_fk: Fk,
    ) -> Result<
        (
            TxRecord,
            Vec<OutputRecord>,
            Vec<rbitcoin_store::InputRecord>,
        ),
        QueryError,
    > {
        let (rec, stored_outputs, mut stored_inputs) = self.load_body_from_store(tx_fk)?;
        let mut cache = U64Map::default();
        self.fill_input_prev_txids_cached(&mut stored_inputs, &mut cache)?;
        Ok((rec, stored_outputs, stored_inputs))
    }

    #[doc(hidden)]
    pub fn transaction_from_class_a(
        rec: TxRecord,
        stored_outputs: Vec<OutputRecord>,
        stored_inputs: Vec<InputRecord>,
    ) -> Transaction {
        // Soft prev_txid may be zero after disk decode — fill from create body below
        // only when caller used fill_input_prev_txids first. Prefer non-zero soft.
        let mut input = Vec::with_capacity(stored_inputs.len());
        for inp in stored_inputs {
            let prev_txid = inp.prev_txid;
            input.push(TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::from_byte_array(prev_txid),
                    vout: inp.prev_index(),
                },
                script_sig: ScriptBuf::from_bytes(inp.script_sig),
                sequence: Sequence::from_consensus(inp.sequence),
                witness: Witness::from(inp.witness),
            });
        }
        let mut output = Vec::with_capacity(stored_outputs.len());
        for out in stored_outputs {
            output.push(TxOut {
                value: Amount::from_sat(out.value),
                script_pubkey: ScriptBuf::from_bytes(out.script),
            });
        }
        Transaction {
            version: TxVersion(rec.version),
            lock_time: LockTime::from_consensus(rec.locktime),
            input,
            output,
        }
    }

    /// Resolve soft `prev_txid` from create_fk without re-reading every parent body.
    ///
    /// Schema v10 stamps create_fk and leaves soft prev_txid zero on disk. Prefer:
    /// 1. already-filled soft prev_txid
    /// 2. same-block / prior-in-block cache
    /// 3. one `txids_get_many` for remaining creates
    pub fn fill_input_prev_txids_cached(
        &self,
        inputs: &mut [InputRecord],
        cache: &mut U64Map<[u8; 32]>,
    ) -> Result<(), QueryError> {
        let mut need: Vec<Fk> = Vec::new();
        for inp in inputs.iter() {
            if inp.is_coinbase() || inp.prev_txid != [0u8; 32] {
                continue;
            }
            let Some(id) = inp.create_fk().get() else {
                return Err(StoreError::Corrupt(
                    "input missing create_fk for wire rebuild",
                ));
            };
            if cache.get(&id).is_some() {
                continue;
            }
            if !need.iter().any(|fk| fk.get() == Some(id)) {
                need.push(Fk(id));
            }
        }
        if !need.is_empty() {
            let got = self.store.txids_get_many(&need)?;
            if got.len() != need.len() {
                return Err(StoreError::Corrupt("invariant: txids_get_many length"));
            }
            for (fk, txid) in need.iter().zip(got) {
                let Some(id) = fk.get() else {
                    continue;
                };
                let Some(txid) = txid else {
                    return Err(StoreError::Corrupt(
                        "wire rebuild: create identity missing from txid.body",
                    ));
                };
                if txid == [0u8; 32] {
                    return Err(StoreError::Corrupt(
                        "wire rebuild: create identity still zero after txid.body",
                    ));
                }
                cache.insert(id, txid);
            }
        }
        for inp in inputs.iter_mut() {
            if inp.is_coinbase() {
                inp.prev_txid = [0u8; 32];
                continue;
            }
            if inp.prev_txid != [0u8; 32] {
                continue;
            }
            let Some(id) = inp.create_fk().get() else {
                return Err(StoreError::Corrupt(
                    "input missing create_fk for wire rebuild",
                ));
            };
            let Some(&txid) = cache.get(&id) else {
                return Err(StoreError::Corrupt(
                    "wire rebuild: create identity not in prev_txid cache",
                ));
            };
            inp.prev_txid = txid;
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)] // packed (fk, range) / span row is the on-disk shape
    #[doc(hidden)]
    pub fn load_class_a_rows(
        &self,
        tx_fks: &[Fk],
    ) -> Result<Vec<(TxRecord, Vec<InputRecord>, Vec<OutputRecord>)>, QueryError> {
        if let Some(&fk) = tx_fks.first() {
            self.require_seqsigwit_fk(fk)?;
        }
        let mut prev_txid_cache: U64Map<[u8; 32]> = U64Map::default();
        if !self.prune_seqsigwit() {
            if let Some((first, last)) = Self::contiguous_fk_run(tx_fks) {
                let mut rows = self.store.get_tx_full_span(first, last)?;
                if rows.len() != tx_fks.len() {
                    return Err(StoreError::Corrupt("invariant: span reconstruct length"));
                }
                for (i, (rec_tx, stored_inputs, _)) in rows.iter_mut().enumerate() {
                    if let Some(id) = tx_fks[i].get() {
                        prev_txid_cache.insert(id, rec_tx.txid);
                    }
                    self.fill_input_prev_txids_cached(stored_inputs, &mut prev_txid_cache)?;
                }
                return Ok(rows);
            }
        }
        let mut rows = Vec::with_capacity(tx_fks.len());
        for &fk in tx_fks {
            let (rec_tx, stored_outputs, mut stored_inputs) = self.load_body_from_store(fk)?;
            self.fill_input_prev_txids_cached(&mut stored_inputs, &mut prev_txid_cache)?;
            rows.push((rec_tx, stored_inputs, stored_outputs));
        }
        Ok(rows)
    }

    /// Consensus block payload (header + witness txs) without a `bitcoin::Block` AST.
    pub fn witness_block_bytes_by_hash(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, QueryError> {
        if let Some(h) = self.height_of_hash(hash)? {
            self.require_seqsigwit_at(h)?;
        }
        let Some((header_fk, rec)) = self.get_header_by_hash(hash)? else {
            return Ok(None);
        };
        let Some(tx_fks) = self.store.header_txs.get_list(header_fk)? else {
            return Ok(None);
        };
        if tx_fks.is_empty() {
            return Err(StoreError::Corrupt("block has no transactions"));
        }
        let header = self.wire_header_from_record_prev(&rec, None)?;
        let rows = self.load_class_a_rows(&tx_fks)?;
        if let Some((first, rest)) = rows.split_first() {
            let coinbase =
                Self::transaction_from_class_a(first.0.clone(), first.2.clone(), first.1.clone());
            if !rows_match_commitment(&coinbase, rest) {
                return Ok(None);
            }
        }
        Ok(Some(encode_witness_block(&header, &rows)))
    }

    /// Consensus-encoded wire bytes for a stored tx (Electrum / RPC).
    pub fn tx_wire_bytes(&self, tx_fk: Fk) -> Result<Vec<u8>, QueryError> {
        use bitcoin::consensus::Encodable;
        let tx = self.reconstruct_tx(tx_fk)?;
        let mut raw = Vec::new();
        tx.consensus_encode(&mut raw)
            .map_err(|_| StoreError::Corrupt("tx encode"))?;
        Ok(raw)
    }

    pub fn reconstruct_archived_block(&self, hash: &[u8; 32]) -> Result<Option<Block>, QueryError> {
        self.note_reconstruct_archived();
        if let Some(h) = self.height_of_hash(hash)? {
            self.require_seqsigwit_at(h)?;
        }
        let Some((header_fk, rec)) = self.get_header_by_hash(hash)? else {
            return Ok(None);
        };
        let Some(tx_fks) = self.store.header_txs.get_list(header_fk)? else {
            return Ok(None);
        };
        self.reconstruct_archived_block_from_parts(rec, tx_fks)
            .map(Some)
    }

    /// BIP144 size and BIP141 weight for a header that has a Class A body.
    ///
    /// A fully stamped `txstat` range is one sequential read. Otherwise the
    /// block is reconstructed. Unknown `header_fk` or a header with no body
    /// is `None`.
    pub fn block_size_weight(&self, header_fk: Fk) -> Result<Option<(u32, u32)>, QueryError> {
        let rec = match self.store.headers.get(header_fk) {
            Ok(r) => r,
            Err(StoreError::NotFound) | Err(StoreError::InvalidFk) => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(size_weight) = self.stamped_block_size_weight(header_fk)? {
            return Ok(Some(size_weight));
        }
        let Some(tx_fks) = self.store.header_txs.get_list(header_fk)? else {
            return Ok(None);
        };
        self.note_reconstruct_archived();
        let block = self.reconstruct_archived_block_from_parts(rec, tx_fks)?;
        let size = u32::try_from(block.total_size())
            .map_err(|_| StoreError::Corrupt("invariant: block size/weight"))?;
        let weight = u32::try_from(block.weight().to_wu())
            .map_err(|_| StoreError::Corrupt("invariant: block size/weight"))?;
        Ok(Some((size, weight)))
    }

    /// BIP144 size and BIP141 weight from complete `txstat` rows only.
    /// Missing bodies or unstamped rows return `None`; no wire reconstruction.
    pub fn stamped_block_size_weight(
        &self,
        header_fk: Fk,
    ) -> Result<Option<(u32, u32)>, QueryError> {
        let Some((first, n)) = self.store.header_txs.get_range(header_fk)? else {
            return Ok(None);
        };
        if n == 0 {
            return Ok(None);
        }
        let last = first.0.saturating_add(u64::from(n) - 1);
        if let Ok(rows) = self.store.txstat_range(header_fk, first.0, last) {
            if rows.len() == n as usize && rows.iter().all(|row| row.is_some()) {
                let plain: Vec<_> = rows.into_iter().flatten().collect();
                return Ok(Some(block_size_weight_from_txstat(&plain)?));
            }
        }
        Ok(None)
    }

    /// Wire rebuild when header row + tx fk list are already known.
    pub fn reconstruct_archived_block_from_parts(
        &self,
        rec: HeaderRecord,
        tx_fks: Vec<Fk>,
    ) -> Result<Block, QueryError> {
        self.reconstruct_archived_block_from_parts_cached(rec, tx_fks, None)
    }

    /// `prev_hash`: when set (load header plan), wire header needs no store IO.
    pub fn reconstruct_archived_block_from_parts_cached(
        &self,
        rec: HeaderRecord,
        tx_fks: Vec<Fk>,
        prev_hash: Option<[u8; 32]>,
    ) -> Result<Block, QueryError> {
        if tx_fks.is_empty() {
            return Err(StoreError::Corrupt("block has no transactions"));
        }
        let header = self.wire_header_from_record_prev(&rec, prev_hash)?;
        let rows = self.load_class_a_rows(&tx_fks)?;
        let mut txdata = Vec::with_capacity(rows.len());
        for (rec_tx, stored_inputs, stored_outputs) in rows {
            txdata.push(Self::transaction_from_class_a(
                rec_tx,
                stored_outputs,
                stored_inputs,
            ));
        }
        let block = Block { header, txdata };
        if coinbase_has_witness_commitment(&block) && has_witness_reserved_value(&block) {
            check_rebuilt_witness_commitment(&block)?;
        }
        Ok(block)
    }

    /// Reconstruct a full wire block at a confirmed height from the relational archive.
    pub fn reconstruct_block_at_height(&self, height: Height) -> Result<Block, QueryError> {
        self.require_seqsigwit_at(height)?;
        let (_fk, rec) = self.header_at_height(height)?.ok_or(StoreError::NotFound)?;
        let tx_fks = self.block_tx_fks(height)?;
        let block = self.reconstruct_archived_block_from_parts_cached(rec.clone(), tx_fks, None)?;
        if block.block_hash().to_byte_array() != rec.hash {
            return Err(StoreError::Corrupt("reconstruct hash mismatch"));
        }
        Ok(block)
    }

    /// Reconstruct by block hash if the hash is on the best (confirmed) chain.
    pub fn reconstruct_block_by_hash(&self, hash: &[u8; 32]) -> Result<Option<Block>, QueryError> {
        match self.height_of_hash(hash)? {
            None => Ok(None),
            Some(h) => Ok(Some(self.reconstruct_block_at_height(h)?)),
        }
    }

    /// Dense confirm-time econ for a confirmed height, or `None` if any row is unstamped.
    ///
    /// Does not read seqsigwit. Missing `header_txs` is `None` (header-only).
    pub fn stamped_txstat_block(
        &self,
        height: Height,
    ) -> Result<Option<StampedTxstatBlock>, QueryError> {
        let Some((header_fk, rec)) = self.header_at_height(height)? else {
            return Ok(None);
        };
        self.stamped_txstat_header(header_fk, rec)
    }

    fn stamped_txstat_header(
        &self,
        header_fk: Fk,
        rec: HeaderRecord,
    ) -> Result<Option<StampedTxstatBlock>, QueryError> {
        let Some((first, n)) = self.store.header_txs.get_range(header_fk)? else {
            return Ok(None);
        };
        if n == 0 {
            return Ok(None);
        }
        let last = first
            .0
            .checked_add(u64::from(n.saturating_sub(1)))
            .ok_or(StoreError::Corrupt("invariant: header_txs last fk"))?;
        let packed = self.store.txstat_range(header_fk, first.0, last)?;
        if packed.len() != n as usize || packed.iter().any(Option::is_none) {
            return Ok(None);
        }
        let fks: Vec<Fk> = (first.0..=last).map(Fk).collect();
        let loc = self.store.tx_create_loc_range_batch(&fks)?;
        if loc.len() != fks.len() {
            return Err(StoreError::Corrupt("invariant: loc batch length"));
        }
        let mut n_outs = Vec::with_capacity(fks.len());
        for p in loc {
            let p = p.ok_or(StoreError::NotFound)?;
            n_outs.push(p.n_out);
        }
        Ok(Some(StampedTxstatBlock {
            rec,
            fks,
            rows: packed.into_iter().map(|r| r.unwrap()).collect(),
            n_outs,
        }))
    }

    /// Txid, fee, weight, and output value of every tx in stored block
    /// `hash`, coinbase first. Active or disconnected by this process only.
    /// `None` for unknown, header-only, never-connected, or reopened stale blocks.
    ///
    /// Reads `txstat`, `create.loc`, `txout.body`, and `txid.body` over the
    /// block's create span. An active block with unstamped `txstat` rows gives
    /// the same rows from the reconstructed block, which needs `seqsigwit`
    /// (`Pruned` when cut). Unstamped stale blocks return `None`. Nothing is written.
    pub fn block_tx_summary(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<Vec<BlockTxSummary>>, QueryError> {
        let Some((header_fk, rec)) = self.connected_block_header(hash)? else {
            return Ok(None);
        };
        let Some(stamped) = self.stamped_txstat_header(header_fk, rec)? else {
            if self.published_height_of_hash(hash).is_none() {
                return Ok(None);
            }
            return self.block_tx_summary_from_wire(hash);
        };
        let (Some(first), Some(last)) = (stamped.fks.first(), stamped.fks.last()) else {
            return Err(StoreError::Corrupt(
                "invariant: stamped block without creates",
            ));
        };
        let txids = self.store.txs.body_txid_range(first.0, last.0)?;
        let values = self.store.output_value_sums_in_fk_span(first.0, last.0)?;
        if txids.len() != stamped.rows.len() || values.len() != stamped.rows.len() {
            return Err(StoreError::Corrupt("invariant: block summary row count"));
        }
        let rows = stamped
            .rows
            .iter()
            .zip(txids)
            .zip(values)
            .map(|((row, txid), value_sat)| BlockTxSummary {
                txid,
                fee_sat: row.fee_sat,
                weight: row.weight(),
                value_sat,
            })
            .collect();
        Ok(Some(rows))
    }

    /// [`Self::block_tx_summary`] for a block whose `txstat` rows are
    /// unstamped: every row comes from the one reconstructed block. The
    /// block and its txids are read through the header `hash` names, never
    /// through a height, so a replace at that height cannot mix two blocks.
    #[doc(hidden)]
    pub fn block_tx_summary_from_wire(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<Vec<BlockTxSummary>>, QueryError> {
        let Some((header_fk, _)) = self.get_header_by_hash(hash)? else {
            return Ok(None);
        };
        let Some(block) = self.reconstruct_archived_block(hash)? else {
            return Ok(None);
        };
        let txids = self.header_txids(header_fk)?;
        let rows = self.txstat_rows_from_block(&block)?;
        if txids.len() != rows.len() {
            return Err(StoreError::Corrupt("invariant: block summary row count"));
        }
        Ok(Some(
            block
                .txdata
                .iter()
                .zip(txids)
                .zip(rows)
                .map(|((tx, txid), row)| BlockTxSummary {
                    txid,
                    fee_sat: row.fee_sat,
                    weight: row.weight(),
                    value_sat: tx.output.iter().map(|o| o.value.to_sat()).sum(),
                })
                .collect(),
        ))
    }

    /// One `txstat` row per tx of a reconstructed wire block, in block order.
    fn txstat_rows_from_block(
        &self,
        block: &Block,
    ) -> Result<Vec<rbitcoin_store::TxStatRow>, QueryError> {
        let mut same_block = std::collections::HashMap::new();
        for tx in &block.txdata {
            same_block.insert(tx.compute_txid().to_byte_array(), tx);
        }
        let mut rows = Vec::with_capacity(block.txdata.len());
        for tx in &block.txdata {
            let in_sum = txstat_in_sum_from_block(self, &same_block, tx)?;
            rows.push(crate::archive::txstat_row_from_tx(tx, in_sum)?);
        }
        Ok(rows)
    }

    /// Fill unstamped `txstat` rows from a reconstructed wire block.
    pub fn stamp_txstat_from_block(&self, height: Height, block: &Block) -> Result<(), QueryError> {
        let Some((header_fk, _)) = self.header_at_height(height)? else {
            return Err(StoreError::Corrupt(
                "invariant: stamp txstat missing header",
            ));
        };
        let fks = self.block_tx_fks(height)?;
        if fks.len() != block.txdata.len() {
            return Err(StoreError::Corrupt("invariant: stamp txstat fk count"));
        }
        let rows = self.txstat_rows_from_block(block)?;
        let first = fks
            .first()
            .and_then(|fk| fk.get())
            .ok_or(StoreError::Corrupt("invariant: stamp txstat first fk"))?;
        self.store.write_txstat_block(header_fk, first, &rows)?;
        Ok(())
    }
}

#[doc(hidden)]
pub fn txstat_in_sum_from_block(
    query: &Query,
    same_block: &std::collections::HashMap<[u8; 32], &bitcoin::Transaction>,
    tx: &bitcoin::Transaction,
) -> Result<Option<u64>, QueryError> {
    if tx.is_coinbase() {
        return Ok(None);
    }
    let mut sum = 0u64;
    for inp in &tx.input {
        let tid = inp.previous_output.txid.to_byte_array();
        let vout = inp.previous_output.vout;
        let val = if let Some(parent) = same_block.get(&tid) {
            let o = parent
                .output
                .get(vout as usize)
                .ok_or(StoreError::Corrupt("invariant: stamp same-block vout"))?;
            o.value.to_sat()
        } else {
            let pfk = query
                .tx_fk_by_txid(&tid)?
                .ok_or(StoreError::Corrupt("invariant: stamp missing prev tx"))?;
            let o = query.tx_output_at_fk(pfk, vout)?;
            o.value
        };
        sum = sum
            .checked_add(val)
            .ok_or(StoreError::Corrupt("txstat in_sum overflow"))?;
    }
    Ok(Some(sum))
}

#[doc(hidden)]
pub fn encode_witness_block(
    header: &BlockHeader,
    rows: &[(TxRecord, Vec<InputRecord>, Vec<OutputRecord>)],
) -> Vec<u8> {
    use bitcoin::consensus::encode::{Encodable, VarInt};
    let mut out = Vec::new();
    let _ = header.consensus_encode(&mut out);
    let _ = VarInt(rows.len() as u64).consensus_encode(&mut out);
    for (rec, ins, outs) in rows {
        encode_class_a_tx(&mut out, rec, ins, outs);
    }
    out
}

#[doc(hidden)]
pub fn encode_class_a_tx(
    out: &mut Vec<u8>,
    rec: &TxRecord,
    ins: &[InputRecord],
    outs: &[OutputRecord],
) {
    use bitcoin::consensus::encode::{Encodable, VarInt};
    let _ = rec.version.consensus_encode(&mut *out);
    let has_wit = ins.iter().any(|i| !i.witness.is_empty());
    if has_wit {
        out.push(0);
        out.push(1);
    }
    let _ = VarInt(ins.len() as u64).consensus_encode(&mut *out);
    for inp in ins {
        out.extend_from_slice(&inp.prev_txid);
        let _ = inp.prev_index().consensus_encode(&mut *out);
        let _ = VarInt(inp.script_sig.len() as u64).consensus_encode(&mut *out);
        out.extend_from_slice(&inp.script_sig);
        let _ = inp.sequence.consensus_encode(&mut *out);
    }
    let _ = VarInt(outs.len() as u64).consensus_encode(&mut *out);
    for o in outs {
        let _ = o.value.consensus_encode(&mut *out);
        let _ = VarInt(o.script.len() as u64).consensus_encode(&mut *out);
        out.extend_from_slice(&o.script);
    }
    if has_wit {
        for inp in ins {
            let _ = VarInt(inp.witness.len() as u64).consensus_encode(&mut *out);
            for w in &inp.witness {
                let _ = VarInt(w.len() as u64).consensus_encode(&mut *out);
                out.extend_from_slice(w);
            }
        }
    }
    let _ = rec.locktime.consensus_encode(&mut *out);
}

fn has_witness_reserved_value(block: &Block) -> bool {
    let Some(input) = block.txdata[0].input.first() else {
        return false;
    };
    let mut items = input.witness.iter();
    matches!((items.next(), items.next()), (Some(reserved), None) if reserved.len() == 32)
}

fn check_rebuilt_witness_commitment(block: &Block) -> Result<(), StoreError> {
    let wtxids: Vec<[u8; 32]> = block
        .txdata
        .iter()
        .skip(1)
        .map(|tx| tx.compute_wtxid().to_byte_array())
        .collect();
    check_witness_commitment_with_wtxids(block, &wtxids)
        .map_err(|_| StoreError::Corrupt("reconstruct: witness commitment mismatch"))
}

fn coinbase_commitment(coinbase: &Transaction) -> Option<([u8; 32], [u8; 38])> {
    let idx = witness_commitment_vout_index(coinbase)?;
    let mut items = coinbase.input.first()?.witness.iter();
    let reserved = items.next()?;
    if reserved.len() != 32 || items.next().is_some() {
        return None;
    }
    let mut value = [0u8; 32];
    value.copy_from_slice(reserved);
    let mut committed = [0u8; 38];
    committed.copy_from_slice(&coinbase.output[idx].script_pubkey.as_bytes()[..38]);
    Some((value, committed))
}

type ClassARow = (TxRecord, Vec<InputRecord>, Vec<OutputRecord>);

fn rows_match_commitment(coinbase: &Transaction, rows: &[ClassARow]) -> bool {
    let Some((reserved, committed)) = coinbase_commitment(coinbase) else {
        return true;
    };
    let mut wtxids = Vec::with_capacity(rows.len());
    let mut buf = Vec::new();
    for (rec, ins, outs) in rows {
        buf.clear();
        encode_class_a_tx(&mut buf, rec, ins, outs);
        wtxids.push(bitcoin::hashes::sha256d::Hash::hash(&buf).to_byte_array());
    }
    witness_commitment_script(wtxids, &reserved) == committed
}
