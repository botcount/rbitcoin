//! Drop-cleaning Tiny [`crate::Query`] for tests. Not an operator API.
//!
//! TxApply → dummy [`bitcoin::Block`] conversion lives here (Q-21), not in
//! production [`Query`] archive.

pub use rbitcoin_store::testutil::TempDir;

use crate::{Query, QueryError, ShJoinSlot, TxApply};
use bitcoin::hashes::Hash;
use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::HeaderRecord;
use std::sync::Arc;

/// Tiny-head query in a drop-cleaning directory (not Mainnet GiB heads).
pub fn tiny_query() -> (TempDir, Query) {
    tiny_query_labeled("query")
}

pub fn tiny_query_labeled(label: &str) -> (TempDir, Query) {
    let dir = TempDir::labeled(label).expect("create temp dir");
    let q = Query::open_or_create_tiny(dir.path()).expect("open_or_create_tiny");
    (dir, q)
}

/// Synthetic header shared by tweak tests. `marker` is the merkle byte those tests key on.
pub fn header_with_merkle_marker(
    h: u32,
    prev_fk: Fk,
    prev_hash: Option<[u8; 32]>,
    marker: u8,
) -> HeaderRecord {
    let mut merkle = [0u8; 32];
    merkle[0..4].copy_from_slice(&h.to_le_bytes());
    merkle[5] = marker;
    let hash = match prev_hash {
        None => merkle,
        Some(ph) => rbitcoin_store::block_header_hash(1, &ph, &merkle, h + 1, 0x207f_ffff, h),
    };
    HeaderRecord {
        prev_fk,
        version: 1,
        timestamp: h + 1,
        bits: 0x207f_ffff,
        nonce: h,
        merkle_root: merkle,
        hash,
        size: 0,
        weight: 0,
    }
}

pub fn tx_apply_to_tx(ta: &TxApply) -> bitcoin::Transaction {
    bitcoin::Transaction {
        version: bitcoin::transaction::Version(ta.tx.version),
        lock_time: bitcoin::absolute::LockTime::from_consensus(ta.tx.locktime),
        input: ta
            .inputs
            .iter()
            .map(|inp| bitcoin::TxIn {
                previous_output: bitcoin::OutPoint {
                    txid: bitcoin::Txid::from_byte_array(inp.prev_txid),
                    vout: inp.prev_index(),
                },
                script_sig: bitcoin::script::ScriptBuf::from_bytes(inp.script_sig.clone()),
                sequence: bitcoin::Sequence::from_consensus(inp.sequence),
                witness: bitcoin::Witness::from_slice(&inp.witness),
            })
            .collect(),
        output: ta
            .outputs
            .iter()
            .map(|o| bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(o.value),
                script_pubkey: bitcoin::script::ScriptBuf::from_bytes(o.script.clone()),
            })
            .collect(),
    }
}

pub fn block_from_applies(txs: &[TxApply]) -> (bitcoin::Block, Vec<[u8; 32]>) {
    let txids: Vec<[u8; 32]> = txs.iter().map(|t| t.tx.txid).collect();
    let txdata: Vec<bitcoin::Transaction> = txs.iter().map(tx_apply_to_tx).collect();
    let block = bitcoin::Block {
        header: bitcoin::block::Header {
            version: bitcoin::block::Version::ONE,
            prev_blockhash: bitcoin::BlockHash::from_byte_array([0; 32]),
            merkle_root: bitcoin::TxMerkleNode::from_byte_array([0; 32]),
            time: 1,
            bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
            nonce: 0,
        },
        txdata,
    };
    (block, txids)
}

/// Fixture Class A / connect. Converts TxApply once, then shipped
/// [`Query::archive_class_a_from_wire`].
pub trait FixtureChain {
    fn commit_class_a_only(&self, header: &HeaderRecord, txs: &[TxApply])
        -> Result<Fk, QueryError>;
    fn connect_block(
        &self,
        height: Height,
        header: &HeaderRecord,
        txs: &[TxApply],
    ) -> Result<Fk, QueryError>;
}

impl FixtureChain for Query {
    fn commit_class_a_only(
        &self,
        header: &HeaderRecord,
        txs: &[TxApply],
    ) -> Result<Fk, QueryError> {
        let fk = self.ensure_header(header)?;
        if self.store().header_txs.has_body(fk)? {
            if self.prune_seqsigwit() && !txs.is_empty() {
                let (block, _) = block_from_applies(txs);
                self.note_pruned_archived_seqsigwit(fk, &block)?;
            }
            return Ok(fk);
        }
        if txs.is_empty() {
            return Ok(fk);
        }
        let (block, txids) = block_from_applies(txs);
        let block = std::sync::Arc::new(block);
        self.archive_class_a_from_wire(&[(fk, &block, txids.as_slice())])?;
        Ok(fk)
    }

    fn connect_block(
        &self,
        height: Height,
        header: &HeaderRecord,
        txs: &[TxApply],
    ) -> Result<Fk, QueryError> {
        self.commit_class_a_only(header, txs)?;
        let fk = self.confirm_block(height, &header.hash)?;
        self.apply_sh_pending()?;
        Ok(fk)
    }
}

/// One-output join that fits Esplora's last-1 cap.
pub fn sh_join_slot_small() -> Arc<ShJoinSlot> {
    ShJoinSlot::with_spender_fk_count(0)
}

/// SH join whose packed size exceeds Esplora's 16 MiB last-1 + last-bulk cap.
pub fn sh_join_slot_over_16mib() -> Arc<ShJoinSlot> {
    const CAP: usize = 16 * 1024 * 1024;
    let n = (CAP - 144) / 8 + 1;
    ShJoinSlot::with_spender_fk_count(n)
}
