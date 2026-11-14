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

/// Regtest proof-of-work limit in compact form (hex 207fffff).
const REGTEST_POW_LIMIT_BITS: u32 = 545_259_519;

/// Header over `txs` at height `h`, chained on `prev_hash`. The merkle root is
/// real, so a reopen (tip revalidate) keeps the chain. An empty block gets a
/// height-keyed root.
pub fn fixture_header(
    h: u32,
    nonce: u32,
    prev_fk: Fk,
    prev_hash: Option<[u8; 32]>,
    txs: &[TxApply],
) -> HeaderRecord {
    let leaves: Vec<[u8; 32]> = txs.iter().map(|t| t.tx.txid).collect();
    let merkle = if leaves.is_empty() {
        let mut m = [0u8; 32];
        m[0..4].copy_from_slice(&h.to_le_bytes());
        m
    } else {
        rbitcoin_primitives::merkle_root_from_txids(&leaves)
    };
    let hash = rbitcoin_store::block_header_hash(
        1,
        &prev_hash.unwrap_or([0u8; 32]),
        &merkle,
        h + 1,
        REGTEST_POW_LIMIT_BITS,
        nonce,
    );
    HeaderRecord {
        prev_fk,
        version: 1,
        timestamp: h + 1,
        bits: REGTEST_POW_LIMIT_BITS,
        nonce,
        merkle_root: merkle,
        hash,
        size: 0,
        weight: 0,
    }
}

/// Coinbase-shaped tx with one unspent output to `script`.
pub fn fixture_pay(txid_seed: u32, script: &[u8]) -> TxApply {
    use rbitcoin_store::{InputRecord, OutputRecord, TxRecord};
    let mut txid = [0u8; 32];
    txid[..4].copy_from_slice(&txid_seed.to_le_bytes());
    txid[31] = 217;
    TxApply {
        tx: TxRecord {
            txid,
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(u32::MAX, vec![1], vec![])],
        outputs: vec![OutputRecord::unspent(1, script.to_vec())],
    }
}

/// Script `heavy_sh_extent_query` pays on every create.
pub const SH_HEAVY_SCRIPT: &[u8] = &[81, 81];

/// Creates in one tip block that move a scripthash key from a slab to an
/// extent chain (`SH_MEGAKEY_MIN_FKS`). A cold pack stores a key by the bytes
/// of its fk stream and stays a slab until about 2 KiB, so the extent comes
/// from tip-mode appends, not from the cold build.
pub const SH_HEAVY_CREATES: u32 = rbitcoin_store::scripthash_slabs::SH_MEGAKEY_MIN_FKS;

/// A tip-mode query whose SH index holds one extent key, built the node way:
/// block 0 (one tx) connects Direct, the cold build seals the head, then a
/// block of [`SH_HEAVY_CREATES`] txs and a block of `tail_txs` txs (none when
/// 0) paying [`SH_HEAVY_SCRIPT`] append in tip mode. Returns the key.
pub fn heavy_sh_extent_query(label: &str, tail_txs: u32) -> (TempDir, Query, [u8; 32]) {
    let (dir, q) = tiny_query_labeled(label);
    q.enter_direct_index_mode().expect("direct mode");
    let (mut prev_fk, mut prev_hash, mut seed) = (Fk::NULL, None, 1u32);
    let mut connect = |q: &Query, h: u32, n: u32| {
        let txs: Vec<TxApply> = (0..n)
            .map(|_| {
                seed += 1;
                fixture_pay(seed, SH_HEAVY_SCRIPT)
            })
            .collect();
        let hdr = fixture_header(h, 0, prev_fk, prev_hash, &txs);
        prev_fk = q.connect_block(Height(h), &hdr, &txs).expect("connect");
        prev_hash = Some(hdr.hash);
    };
    connect(&q, 0, 1);
    q.finalize_sh_runs().expect("cold SH build");
    q.enter_tip_index_mode();
    connect(&q, 1, SH_HEAVY_CREATES);
    if tail_txs > 0 {
        connect(&q, 2, tail_txs);
    }
    (dir, q, rbitcoin_store::script_hash(SH_HEAVY_SCRIPT))
}

/// Zero the `ver` byte of the key's last extent page: the torn head left by an
/// ENOSPC mid-rewrite. Every full read of the key then fails with
/// `scripthash extent last page not ver=2`.
pub fn tear_sh_extent_head(q: &Query, key: &[u8; 32]) {
    let t = &q.store().scripthash;
    let last_page = match t.head_value(key).expect("head").expect("key present") {
        rbitcoin_store::ShHeadValue::Extent { last_page } => last_page,
        other => panic!("heavy key is not an extent: {other:?}"),
    };
    let mut page = [0u8; 4096];
    t.body().read_at(last_page, &mut page).expect("read page");
    page[0] = 0;
    t.body().write_at(last_page, &page).expect("tear page");
}
