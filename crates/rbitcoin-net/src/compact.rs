//! BIP152 compact block reconstruction from mempool short-ids.
//!
//! Version 2 (witness) short-ids are preferred; callers pass a short-id map
//! built from live mempool (and optional extra txs). On incomplete fill,
//! returns missing absolute indexes for `getblocktxn`.

use bitcoin::bip152::{
    BlockTransactions, BlockTransactionsRequest, HeaderAndShortIds, PrefilledTransaction, ShortId,
};
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::Magic;
use bitcoin::{Block, BlockHash, Transaction, Wtxid};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Build siphash short-id → transaction map for compact fill (version 1 = txid, 2 = wtxid).
pub fn shortid_map_from_txs<'a>(
    header: &Header,
    nonce: u64,
    version: u32,
    txs: impl IntoIterator<Item = &'a Transaction>,
) -> HashMap<ShortId, Vec<&'a Transaction>> {
    let keys = ShortId::calculate_siphash_keys(header, nonce);
    let mut map: HashMap<ShortId, Vec<&Transaction>> = HashMap::new();
    for tx in txs {
        let id = short_id_for_tx(tx, version, keys);
        map.entry(id).or_default().push(tx);
    }
    map
}

#[doc(hidden)]
pub fn short_id_for_tx(tx: &Transaction, version: u32, keys: (u64, u64)) -> ShortId {
    match version {
        1 => ShortId::with_siphash_keys(&tx.compute_txid().to_raw_hash(), keys),
        _ => ShortId::with_siphash_keys(&tx.compute_wtxid().to_raw_hash(), keys),
    }
}

/// The hash each transaction's short id is taken from, in block order:
/// txid for version 1, wtxid otherwise.
pub fn short_id_hashes(block: &Block, version: u32) -> Vec<[u8; 32]> {
    block
        .txdata
        .iter()
        .map(|tx| match version {
            1 => tx.compute_txid().to_byte_array(),
            _ => tx.compute_wtxid().to_byte_array(),
        })
        .collect()
}

/// `HeaderAndShortIds::from_block` from hashes already in hand: `ids` is
/// [`short_id_hashes`] for `version`. The coinbase is always prefilled.
/// `None` for a version other than 1 or 2, an `ids` length that is not the
/// block's transaction count, a `prefill` that is not ascending indexes
/// into the block, or a gap between prefilled transactions that does not
/// fit the `u16` differential index. On `None` an announce retries with
/// the coinbase alone and a compact `getdata` is served the full block.
pub fn header_and_short_ids(
    block: &Block,
    ids: &[[u8; 32]],
    nonce: u64,
    version: u32,
    prefill: &[usize],
) -> Option<HeaderAndShortIds> {
    if !(1..=2).contains(&version) || ids.len() != block.txdata.len() {
        return None;
    }
    let keys = ShortId::calculate_siphash_keys(&block.header, nonce);
    let mut rest = prefill;
    let mut next_idx = 0usize;
    let mut prefilled_txs = Vec::with_capacity(prefill.len() + 1);
    let mut short_ids = Vec::with_capacity(block.txdata.len().saturating_sub(prefill.len()));
    for (idx, (tx, id)) in block.txdata.iter().zip(ids).enumerate() {
        let listed = rest.first() == Some(&idx);
        if listed {
            rest = &rest[1..];
        }
        if !listed && idx != 0 {
            short_ids.push(ShortId::with_siphash_keys(id, keys));
            continue;
        }
        let mut tx = tx.clone();
        if version == 1 {
            tx.input.iter_mut().for_each(|i| i.witness.clear());
        }
        prefilled_txs.push(PrefilledTransaction {
            idx: u16::try_from(idx - next_idx).ok()?,
            tx,
        });
        next_idx = idx + 1;
    }
    if !rest.is_empty() {
        return None;
    }
    Some(HeaderAndShortIds {
        header: block.header,
        nonce,
        short_ids,
        prefilled_txs,
    })
}

/// Consensus-decode BIP152 `HeaderAndShortIds`.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn decode_cmpct_hsi(raw: &[u8]) -> Option<HeaderAndShortIds> {
    bitcoin::consensus::encode::deserialize(raw).ok()
}

/// Core v2 frame after we send `cmpctblock`.
#[derive(Debug, PartialEq, Eq)]
pub enum CmpctPeerFrame {
    GetBlockTxn(Vec<u64>),
    Ping(u64),
    Pong(u64),
    Other,
}

/// Classify decrypted application contents (regtest magic).
pub fn classify_v2_cmpct_peer(contents: &[u8]) -> CmpctPeerFrame {
    match crate::v2::parse_v2_contents(Magic::REGTEST, contents) {
        Ok(frame) => match frame.decode().payload() {
            NetworkMessage::GetBlockTxn(r) => {
                CmpctPeerFrame::GetBlockTxn(r.txs_request.indexes.clone())
            }
            NetworkMessage::Ping(n) => CmpctPeerFrame::Ping(*n),
            NetworkMessage::Pong(n) => CmpctPeerFrame::Pong(*n),
            _ => CmpctPeerFrame::Other,
        },
        Err(_) => CmpctPeerFrame::Other,
    }
}

/// Header hashes from a decrypted `headers` payload. Other messages are `None`.
pub fn v2_header_hashes(contents: &[u8]) -> Option<Vec<BlockHash>> {
    let frame = crate::v2::parse_v2_contents(Magic::REGTEST, contents).ok()?;
    match frame.decode().payload() {
        NetworkMessage::Headers(headers) => Some(headers.iter().map(|h| h.block_hash()).collect()),
        _ => None,
    }
}

/// Core `PartiallyDownloadedBlock::InitData` shape. A failure is a malformed
/// `cmpctblock`: no txs, more than [`rbitcoin_consensus::MAX_BLOCK_TX_COUNT`],
/// a null prefilled tx (no inputs and no outputs), or a prefilled index that
/// is out of range or not increasing. The 65,535 tx-count limit belongs to
/// the `cmpctblock` decoder, as in Core.
pub fn cmpct_init_data_ok(hsi: &HeaderAndShortIds) -> bool {
    let total = hsi.short_ids.len().saturating_add(hsi.prefilled_txs.len());
    if total == 0 || total > rbitcoin_consensus::MAX_BLOCK_TX_COUNT {
        return false;
    }
    if hsi
        .prefilled_txs
        .iter()
        .any(|p| p.tx.input.is_empty() && p.tx.output.is_empty())
    {
        return false;
    }
    let mut last: Option<usize> = None;
    for (abs, _) in prefilled_absolute_indexes(hsi) {
        if abs >= total {
            return false;
        }
        if last.is_some_and(|p| abs <= p) {
            return false;
        }
        last = Some(abs);
    }
    true
}

/// Absolute block indexes of prefilled txs (decode differential encoding).
pub fn prefilled_absolute_indexes(hsi: &HeaderAndShortIds) -> Vec<(usize, &Transaction)> {
    let mut out = Vec::with_capacity(hsi.prefilled_txs.len());
    let mut abs: usize = 0;
    for p in &hsi.prefilled_txs {
        abs = abs.saturating_add(p.idx as usize);
        out.push((abs, &p.tx));
        abs = abs.saturating_add(1);
    }
    out
}

/// First-pass compact fill still waiting on `blocktxn` for `missing` slots.
///
/// Matched mempool / extra / orphan bodies are owned here. Apply overlays
/// the peer response onto those holes and must not re-walk a live short-id
/// map (a later `try_read` miss or eviction would drop first-pass hits).
/// `txids[i]` is the digest already hashed for a filled `slots[i]`, or
/// `None` on a hole. It is the same length as `slots`.
#[derive(Clone, Debug)]
pub struct CmpctPartial {
    pub slots: Vec<Option<Transaction>>,
    pub txids: Vec<Option<[u8; 32]>>,
    pub missing: Vec<u64>,
}

impl CmpctPartial {
    pub fn missing(&self) -> &[u64] {
        &self.missing
    }
}

#[derive(Debug)]
pub enum Reconstruct {
    Block(Block),
    Partial(CmpctPartial),
    Fail,
}

/// Attempt to reconstruct a full block from compact data + available txs.
///
/// On success the txs merkle to `hsi.header`. Incomplete fill returns the
/// remaining absolute indexes (`getblocktxn`). A complete fill that does not
/// merkle (wrong unique short-id body) returns an empty vec (`getdata`).
pub fn try_reconstruct<T: Borrow<Transaction>>(
    hsi: &HeaderAndShortIds,
    available: &HashMap<ShortId, Vec<T>>,
) -> Result<Block, Vec<u64>> {
    match reconstruct(hsi, available) {
        Reconstruct::Block(b) => Ok(b),
        Reconstruct::Partial(p) => Err(p.missing),
        Reconstruct::Fail => Err(Vec::new()),
    }
}

pub fn reconstruct<T: Borrow<Transaction>>(
    hsi: &HeaderAndShortIds,
    available: &HashMap<ShortId, Vec<T>>,
) -> Reconstruct {
    if !cmpct_init_data_ok(hsi) {
        return Reconstruct::Fail;
    }
    let total = hsi.short_ids.len() + hsi.prefilled_txs.len();

    let mut slots: Vec<Option<Transaction>> = vec![None; total];
    let mut txid_at: Vec<Option<[u8; 32]>> = vec![None; total];
    let mut prefilled_set = std::collections::HashSet::new();
    let mut placed: std::collections::HashSet<bitcoin::Txid> = std::collections::HashSet::new();
    for (abs, tx) in prefilled_absolute_indexes(hsi) {
        let txid = tx.compute_txid();
        placed.insert(txid);
        txid_at[abs] = Some(txid.to_byte_array());
        slots[abs] = Some(tx.clone());
        prefilled_set.insert(abs);
    }

    let mut short_i = 0usize;
    let mut missing = Vec::new();
    for (abs, slot) in slots.iter_mut().enumerate() {
        if prefilled_set.contains(&abs) {
            continue;
        }
        if short_i >= hsi.short_ids.len() {
            missing.push(abs as u64);
            continue;
        }
        let sid = hsi.short_ids[short_i];
        short_i += 1;
        match available.get(&sid) {
            Some(cands) if cands.len() == 1 => {
                let txid = cands[0].borrow().compute_txid();
                // Repeat short-id / same candidate in two slots → duplicate
                // txid block. Mark missing so getblocktxn / getdata can recover.
                if !placed.insert(txid) {
                    missing.push(abs as u64);
                    continue;
                }
                txid_at[abs] = Some(txid.to_byte_array());
                *slot = Some(cands[0].borrow().clone());
            }
            // One candidate fills the slot. Every other count, including a
            // short-id collision, is missing for getblocktxn.
            _ => missing.push(abs as u64),
        }
    }

    if !missing.is_empty() {
        return Reconstruct::Partial(CmpctPartial {
            slots,
            txids: txid_at,
            missing,
        });
    }

    let mut txdata = Vec::with_capacity(total);
    let mut leaves = Vec::with_capacity(total);
    for (slot, leaf) in slots.into_iter().zip(txid_at) {
        match (slot, leaf) {
            (Some(tx), Some(id)) => {
                txdata.push(tx);
                leaves.push(id);
            }
            _ => return Reconstruct::Fail,
        }
    }

    match finish_with_txids(hsi.header, txdata, &leaves) {
        Ok(block) => Reconstruct::Block(block),
        Err(_) => Reconstruct::Fail,
    }
}

/// Build a `getblocktxn` request for missing absolute indexes.
pub fn missing_request(block_hash: BlockHash, missing: &[u64]) -> BlockTransactionsRequest {
    BlockTransactionsRequest {
        block_hash,
        indexes: missing.to_vec(),
    }
}

/// Overlay `blocktxn` onto a first-pass [`CmpctPartial`].
///
/// Takes `slots` and `txids` after the three early returns. `missing` stays.
/// Those returns leave the partial unchanged.
///
/// `txn.transactions` is 1:1 with `partial.missing` (absolute indexes, request
/// order). Already-filled slots are kept; the live short-id map is not consulted.
/// Completes only when the filled txs merkle to `hsi.header`.
pub fn apply_block_transactions(
    hsi: &HeaderAndShortIds,
    partial: &mut CmpctPartial,
    txn: &BlockTransactions,
) -> Result<Block, Vec<u64>> {
    if txn.transactions.len() != partial.missing.len() {
        return Err(partial.missing.clone());
    }
    if partial.slots.is_empty() {
        return Err(Vec::new());
    }
    if partial.txids.len() != partial.slots.len() {
        return Err(Vec::new());
    }

    let mut slots = std::mem::take(&mut partial.slots);
    let mut leaves = std::mem::take(&mut partial.txids);
    let mut placed = HashSet::new();
    for (slot, leaf) in slots.iter().zip(leaves.iter()) {
        match (slot.is_some(), leaf) {
            (true, Some(id)) => {
                if !placed.insert(bitcoin::Txid::from_byte_array(*id)) {
                    return Err(partial.missing.clone());
                }
            }
            (false, None) => {}
            _ => return Err(Vec::new()),
        }
    }
    for (i, abs) in partial.missing.iter().enumerate() {
        let abs = *abs as usize;
        if abs >= slots.len() || slots[abs].is_some() {
            return Err(Vec::new());
        }
        let tx = &txn.transactions[i];
        let txid = tx.compute_txid();
        if !placed.insert(txid) {
            return Err(partial.missing.clone());
        }
        leaves[abs] = Some(txid.to_byte_array());
        slots[abs] = Some(tx.clone());
    }
    let mut txdata = Vec::with_capacity(slots.len());
    let mut ids = Vec::with_capacity(leaves.len());
    for (slot, leaf) in slots.into_iter().zip(leaves) {
        match (slot, leaf) {
            (Some(tx), Some(id)) => {
                txdata.push(tx);
                ids.push(id);
            }
            _ => return Err(partial.missing.clone()),
        }
    }
    finish_with_txids(hsi.header, txdata, &ids)
}

/// Header merkle root and the CVE-2012-2459 mutation bit from one txid list.
///
/// `None` when `txdata` is empty. That list has no root, including when the
/// header root is all zeros. Equal adjacent txids still match a root after
/// odd-node duplication. That pair is the mutation bit.
pub fn block_txid_merkle(txdata: &[Transaction]) -> Option<([u8; 32], bool)> {
    if txdata.is_empty() {
        return None;
    }
    let leaves: Vec<[u8; 32]> = txdata
        .iter()
        .map(|tx| tx.compute_txid().to_byte_array())
        .collect();
    Some(rbitcoin_store::merkle_root_mutated(&leaves))
}

/// Merkle check from txids already hashed while the slots were filled.
///
/// `leaves[i]` is the txid of `txdata[i]`.
#[doc(hidden)]
pub fn finish_with_txids(
    header: Header,
    txdata: Vec<Transaction>,
    leaves: &[[u8; 32]],
) -> Result<Block, Vec<u64>> {
    if leaves.len() != txdata.len() || leaves.is_empty() {
        return Err(Vec::new());
    }
    let (root, mutated) = rbitcoin_store::merkle_root_mutated(leaves);
    let block = Block { header, txdata };
    if root != block.header.merkle_root.to_byte_array()
        || mutated
        || rbitcoin_consensus::block_mutated_without_coinbase(&block)
    {
        return Err(Vec::new());
    }
    Ok(block)
}

/// Wtxid membership at reconstruct time (live graph / extra-compact / orphanage).
#[derive(Clone, Debug, Default)]
pub struct CmpctFillSets {
    pub mempool: HashSet<Wtxid>,
    pub extra: HashSet<Wtxid>,
    pub orphan: HashSet<Wtxid>,
}

/// One compact reconstruct outcome for the operator INFO line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmpctReconstructStats {
    pub hash: BlockHash,
    pub tx_count: usize,
    pub getdata: bool,
    pub missing_n: usize,
    pub prefill_n: usize,
    pub prefill_bytes: usize,
    pub mempool_n: usize,
    pub mempool_bytes: usize,
    pub extra_n: usize,
    pub extra_bytes: usize,
    pub orphan_n: usize,
    pub orphan_bytes: usize,
    pub redundant_prefill_n: usize,
    pub redundant_prefill_bytes: usize,
    pub fetched_n: usize,
    pub fetched_bytes: usize,
}

impl Default for CmpctReconstructStats {
    fn default() -> Self {
        Self {
            hash: BlockHash::from_byte_array([0; 32]),
            tx_count: 0,
            getdata: false,
            missing_n: 0,
            prefill_n: 0,
            prefill_bytes: 0,
            mempool_n: 0,
            mempool_bytes: 0,
            extra_n: 0,
            extra_bytes: 0,
            orphan_n: 0,
            orphan_bytes: 0,
            redundant_prefill_n: 0,
            redundant_prefill_bytes: 0,
            fetched_n: 0,
            fetched_bytes: 0,
        }
    }
}

impl fmt::Display for CmpctReconstructStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.getdata {
            write!(
                f,
                "cmpct reconstruct {} getdata missing={} fetched=0/0",
                self.hash, self.missing_n
            )
        } else {
            write!(
                f,
                "cmpct reconstruct {} tx={} prefill={}/{} mempool={}/{} extra={}/{} orphan={}/{} redundant_prefill={}/{} fetched={}/{}",
                self.hash,
                self.tx_count,
                self.prefill_n,
                self.prefill_bytes,
                self.mempool_n,
                self.mempool_bytes,
                self.extra_n,
                self.extra_bytes,
                self.orphan_n,
                self.orphan_bytes,
                self.redundant_prefill_n,
                self.redundant_prefill_bytes,
                self.fetched_n,
                self.fetched_bytes,
            )
        }
    }
}

#[doc(hidden)]
pub fn tx_wire_len(tx: &Transaction) -> usize {
    tx.total_size()
}

/// Operator line for an outbound `cmpctblock` (announce or getdata serve).
pub fn cmpct_send_line(hash: BlockHash, tx_count: usize, hsi: &HeaderAndShortIds) -> String {
    let prefill_n = hsi.prefilled_txs.len();
    let prefill_bytes: usize = hsi.prefilled_txs.iter().map(|p| tx_wire_len(&p.tx)).sum();
    format!("cmpct announce {hash} tx={tx_count} prefill={prefill_n}/{prefill_bytes}")
}

/// Fallback to full `getdata` after compact reconstruct failed.
pub fn reconstruct_getdata_stats(hash: BlockHash, missing_n: usize) -> CmpctReconstructStats {
    CmpctReconstructStats {
        hash,
        getdata: true,
        missing_n,
        ..CmpctReconstructStats::default()
    }
}

/// One wtxid per transaction, in block order.
pub fn block_wtxids(block: &Block) -> Vec<Wtxid> {
    block.txdata.iter().map(|tx| tx.compute_wtxid()).collect()
}

/// Classify a completed reconstruct: fill sources plus `blocktxn` fetch size.
pub fn reconstruct_stats(
    hsi: &HeaderAndShortIds,
    block: &Block,
    fill: &CmpctFillSets,
    fetched_indexes: &[u64],
) -> CmpctReconstructStats {
    let wtxids = block_wtxids(block);
    reconstruct_stats_from_wtxids(hsi, block, fill, fetched_indexes, &wtxids)
}

/// Same classification as [`reconstruct_stats`], using wtxids already hashed.
pub fn reconstruct_stats_from_wtxids(
    hsi: &HeaderAndShortIds,
    block: &Block,
    fill: &CmpctFillSets,
    fetched_indexes: &[u64],
    wtxids: &[Wtxid],
) -> CmpctReconstructStats {
    let prefilled: HashSet<usize> = prefilled_absolute_indexes(hsi)
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    let fetched: HashSet<usize> = fetched_indexes.iter().map(|i| *i as usize).collect();
    let mut stats = CmpctReconstructStats {
        hash: block.block_hash(),
        tx_count: block.txdata.len(),
        ..CmpctReconstructStats::default()
    };
    for (abs, tx) in block.txdata.iter().enumerate() {
        let bytes = tx_wire_len(tx);
        let wtxid = wtxids.get(abs);
        if prefilled.contains(&abs) {
            stats.prefill_n += 1;
            stats.prefill_bytes += bytes;
            if wtxid.is_some_and(|id| fill.mempool.contains(id)) {
                stats.redundant_prefill_n += 1;
                stats.redundant_prefill_bytes += bytes;
            }
            continue;
        }
        if fetched.contains(&abs) {
            stats.fetched_n += 1;
            stats.fetched_bytes += bytes;
            continue;
        }
        let Some(wtxid) = wtxid else {
            continue;
        };
        if fill.mempool.contains(wtxid) {
            stats.mempool_n += 1;
            stats.mempool_bytes += bytes;
        } else if fill.extra.contains(wtxid) {
            stats.extra_n += 1;
            stats.extra_bytes += bytes;
        } else if fill.orphan.contains(wtxid) {
            stats.orphan_n += 1;
            stats.orphan_bytes += bytes;
        }
    }
    stats
}

/// Additional prefill (beyond coinbase) packed into outbound `cmpctblock`.
pub const EXTRA_PREFILL_BUDGET: usize = 10 * 1024;

/// Sorted prefill indexes: coinbase, then non-mempool primary, then extra-pool.
///
/// Packs greedily up to [`EXTRA_PREFILL_BUDGET`] additional witness bytes.
pub fn prefill_indexes(block: &Block, fill: &CmpctFillSets) -> Vec<usize> {
    let wtxids = block_wtxids(block);
    prefill_indexes_from_wtxids(block, fill, &wtxids)
}

/// Same indexes as [`prefill_indexes`], using wtxids already hashed.
pub fn prefill_indexes_from_wtxids(
    block: &Block,
    fill: &CmpctFillSets,
    wtxids: &[Wtxid],
) -> Vec<usize> {
    let mut primary = Vec::new();
    let mut extra = Vec::new();
    for (i, _tx) in block.txdata.iter().enumerate().skip(1) {
        let Some(w) = wtxids.get(i) else {
            primary.push(i);
            continue;
        };
        if fill.mempool.contains(w) {
            continue;
        }
        if fill.extra.contains(w) {
            extra.push(i);
        } else {
            primary.push(i);
        }
    }
    let mut out = vec![0];
    let mut used = 0usize;
    for i in primary.into_iter().chain(extra) {
        let sz = tx_wire_len(&block.txdata[i]);
        if used.saturating_add(sz) > EXTRA_PREFILL_BUDGET {
            continue;
        }
        used += sz;
        out.push(i);
    }
    out.sort_unstable();
    out
}

/// `None` when membership was not observed (mempool `try_read` missed).
pub fn outbound_prefill_indexes(block: &Block, fill: Option<&CmpctFillSets>) -> Option<Vec<usize>> {
    Some(prefill_indexes(block, fill?))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefillPlan {
    pub hash: BlockHash,
    pub indexes: Vec<usize>,
}
