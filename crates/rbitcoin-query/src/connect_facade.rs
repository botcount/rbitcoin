//! Connect façade. Query loads and writes; consensus decides the block.

use crate::ConfirmError;
use bitcoin::hashes::Hash;
use bitcoin::Block;
use rbitcoin_consensus::block::{validate_block_structure_hashed, ValidationContext};
use rbitcoin_consensus::{ChainParams, Milestone};
use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::HeaderRecord;
use std::sync::Arc;

use crate::confirm_run;
use crate::connect::ConfirmPrepared;
use crate::convert::{block_to_apply, block_to_apply_with_txids_prev, header_to_record};
use crate::{Query, TxApply, TxPrecompute, WirePlanNeed};
use rbitcoin_store::StoreError;

/// Confirm a contiguous tip-extension run of wire blocks (sync all stages).
///
/// See [`confirm_wire_run`]: lookup → load → scripts → write. IBD uses the split
/// phases for pipeline overlap.
pub use crate::confirm_run::{
    confirm_bq_resolve_wave_capped, confirm_scripts_phase, confirm_wire_load_from_plan,
    confirm_wire_load_phase, confirm_wire_load_phase_pipelined, confirm_wire_lookup_stamp,
    confirm_wire_run, confirm_wire_run_preverified, confirm_write_phase,
    confirm_write_phase_or_return, drive_script_waves_with, finish_post_commit_hashes,
    parent_missing_from_chain, replay_spend_annotations, take_wave_items_for_load, BqResolveWave,
    ConfirmLoadOutcome, ConfirmScriptOutcome, ConfirmWriteMsg, LoadedBatch, PlanStampOutcome,
    ScriptOkBatch, ScriptPreverified, WireBlockIn, WireLoadPipeline, BQ_RESOLVE_WAVE_MAX_BLOCKS,
    BQ_RESOLVE_WAVE_MAX_INPUTS,
};

/// Wake the IBD scripts publisher (`ibd-confirm`) after `scriptq` send or close.
pub use rbitcoin_consensus::unpark_script_publisher;

/// Accept + archive + confirm in one step (genesis / tip extension / tests).
///
/// **Same path as IBD confirm:** structure + header checks, then Class A
/// archive, then [`confirm_wire_run`] (lookup → load pin denserels → scripts →
/// structural → Class C → abs spend annotate). No separate
/// `put_spend_batch_by_create`.
///
/// Idempotent when `height` is already confirmed for this block hash.
/// Full script verify (no mempool skip) — use
/// [`accept_and_connect_block_preverified`] on tip follow with a live mempool.
pub fn accept_and_connect_block(
    query: &Query,
    params: &ChainParams,
    height: Height,
    block: &Block,
    milestone: Milestone,
) -> Result<rbitcoin_primitives::Fk, ConfirmError> {
    accept_and_connect_block_preverified(
        query,
        params,
        height,
        Arc::new(block.clone()),
        milestone,
        &ScriptPreverified::new(),
        None,
    )
}

/// Like [`accept_and_connect_block`], skipping script verify for `preverified`
/// txids (tip follow: live mempool after accept). Reorg disconnect stays outside.
pub fn accept_and_connect_block_preverified(
    query: &Query,
    params: &ChainParams,
    height: Height,
    block: Arc<Block>,
    milestone: Milestone,
    preverified: &ScriptPreverified,
    pres: Option<Arc<[TxPrecompute]>>,
) -> Result<rbitcoin_primitives::Fk, ConfirmError> {
    let hash = block.block_hash().to_byte_array();
    if let Some(h) = query.height_of_hash(&hash).map_err(ConfirmError::from)? {
        if h == height {
            if let Some((fk, _)) = query
                .get_header_by_hash(&hash)
                .map_err(ConfirmError::from)?
            {
                if query.pruned_spill_covers(fk).map_err(ConfirmError::from)? {
                    confirm_run::finish_post_commit(query, height.0, &hash)?;
                    return Ok(fk);
                }
                // Already this tip, but the keep file is gone. The block
                // hash does not cover witness, so check the commitment
                // before this block becomes the kept copy.
                let ctx = ValidationContext::at(params, height, milestone);
                validate_block_structure_hashed(&block, &ctx)?;
                query
                    .note_pruned_archived_seqsigwit(fk, &block)
                    .map_err(ConfirmError::from)?;
                let tx_fks = query
                    .store()
                    .header_txs
                    .get_list(fk)
                    .map_err(ConfirmError::from)?
                    .ok_or(ConfirmError::Store(StoreError::Corrupt(
                        "seqsigwit window miss",
                    )))?;
                query
                    .seal_missing_pruned_spills(&[ConfirmPrepared {
                        height,
                        header_fk: fk,
                        tx_fks,
                        hash,
                    }])
                    .map_err(ConfirmError::from)?;
                confirm_run::finish_post_commit(query, height.0, &hash)?;
                return Ok(fk);
            }
        }
    }

    // Unified height-ordered path: wire → lookup → load (pin+assemble) → scripts →
    // write (Class A + structural + Class C + annotate). No archive-then-reload.
    let fks = confirm_wire_run_preverified(
        query,
        params,
        milestone,
        &[(height, block, pres)],
        preverified,
    )?;
    if let Some(fk) = fks.into_iter().next() {
        return Ok(fk);
    }
    // Write skipped heights ≤ tip (idempotent race). A body we just ran
    // through lookup/load must have a header — missing is an invariant, not
    // a soft NotFound (inflated confirmed[] used to hit this on tip+1).
    query
        .get_header_by_hash(&hash)
        .map_err(ConfirmError::from)?
        .map(|(fk, _)| fk)
        .ok_or(ConfirmError::Store(rbitcoin_store::StoreError::Corrupt(
            "invariant: confirm write skipped but header missing",
        )))
}

#[doc(hidden)]
pub fn class_a_header_and_txids(
    query: &Query,
    params: &ChainParams,
    block: &Block,
) -> Result<(HeaderRecord, Vec<[u8; 32]>), ConfirmError> {
    let ctx = ValidationContext::archive_structure(params);
    let txids = validate_block_structure_hashed(block, &ctx)?;
    let hash = block.header.block_hash().to_byte_array();
    rbitcoin_consensus::header::pow_hash_meets_target(hash, block.header.bits, params.pow_limit)?;
    let prev = block.header.prev_blockhash;
    let prev_fk = if prev.to_byte_array() == [0u8; 32] {
        Fk::NULL
    } else {
        rbitcoin_consensus::block::require_linked_parent(
            query
                .get_header_by_hash(prev.as_byte_array())
                .map_err(ConfirmError::from)?
                .map(|(fk, _)| fk),
        )?
    };
    Ok((header_to_record(prev_fk, &block.header, hash), txids))
}

/// Class A only (no tip / Class C). Crash and `plan=None` tests.
///
/// Not a production IBD API — confirm write uses `archive_plan_batch_from_wire`
/// + commit (wire `seqsigwit` is encoded from the block and plan edges).
pub fn commit_class_a_block(
    query: &Query,
    params: &ChainParams,
    height: Height,
    block: &Block,
    milestone: Milestone,
) -> Result<(), ConfirmError> {
    let _ = (height, milestone);
    let (header, txids) = class_a_header_and_txids(query, params, block)?;
    let fk = query.ensure_header(&header).map_err(ConfirmError::from)?;
    let arc = Arc::new(block.clone());
    query
        .archive_class_a_from_wire(&[(fk, &arc, txids.as_slice())])
        .map_err(ConfirmError::from)?;
    Ok(())
}

/// Class A for a contiguous run in one plan (same-batch parent stamp).
///
/// Use this instead of N×[`commit_class_a_block`] when later blocks spend
/// earlier unconfirmed creates.
pub fn commit_class_a_run(
    query: &Query,
    params: &ChainParams,
    blocks: &[(Height, Block)],
    milestone: Milestone,
) -> Result<(), ConfirmError> {
    let _ = milestone;
    let mut owned = Vec::with_capacity(blocks.len());
    for (_, block) in blocks {
        let (header, txids) = class_a_header_and_txids(query, params, block)?;
        let fk = query.ensure_header(&header).map_err(ConfirmError::from)?;
        owned.push((fk, Arc::new(block.clone()), txids));
    }
    let refs: Vec<WirePlanNeed<'_>> = owned
        .iter()
        .map(|(fk, b, ids)| (*fk, b, ids.as_slice()))
        .collect();
    query
        .archive_class_a_from_wire(&refs)
        .map_err(ConfirmError::from)?;
    Ok(())
}

/// CPU-side prep for Class A archive.
pub fn prepare_block_for_archive(
    query: &Query,
    params: &ChainParams,
    block: &Block,
) -> Result<(HeaderRecord, Vec<TxApply>), ConfirmError> {
    let hash = block.block_hash().to_byte_array();
    if query.is_block_archived(&hash).map_err(ConfirmError::from)? {
        // Standalone archive helper (not confirm pipeline): one hash pass here.
        return block_to_apply(query, &block.header, &block.txdata);
    }
    prepare_block_for_archive_new(query, params, block)
}

#[doc(hidden)]
pub fn prepare_block_for_archive_new(
    query: &Query,
    params: &ChainParams,
    block: &Block,
) -> Result<(HeaderRecord, Vec<TxApply>), ConfirmError> {
    let (header, txids) = class_a_header_and_txids(query, params, block)?;
    let (_, txs) =
        block_to_apply_with_txids_prev(header.prev_fk, &block.header, &block.txdata, &txids)?;
    Ok((header, txs))
}
