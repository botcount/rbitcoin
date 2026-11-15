//! Store-backed BIP-352 tweak readers. Pure tweak math stays in consensus.
use bitcoin::hashes::Hash;
use bitcoin::{OutPoint, Transaction, TxOut, Witness};
use rbitcoin_consensus::silent_payments::{tweak_from_tx, TaprootOut, TxTweak};
use rbitcoin_consensus::ChainParams;
use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::{IndexWindow, InputRecord, LoadedTweakTx, OutputRecord, StoreError};
use std::collections::{BTreeMap, HashMap};

use crate::{ConfirmError, Query};

/// Txid and outputs of a spent create.
#[doc(hidden)]
pub type ParentLookup<'a> = dyn Fn(Fk) -> Option<([u8; 32], &'a [OutputRecord])> + 'a;

/// Prefer the thin index (no parent peeks). Hole / no table → [`tweaks_for_height`].
pub fn tweaks_at_height(
    query: &Query,
    params: &ChainParams,
    height: Height,
) -> Result<BTreeMap<[u8; 32], TxTweak>, ConfirmError> {
    if !params.taproot_active_at(height.0) {
        return Ok(BTreeMap::new());
    }
    match query.load_thin_tweaks(height) {
        Ok(Some(rows)) => {
            let mut out = BTreeMap::new();
            for r in rows {
                out.insert(
                    r.txid,
                    TxTweak {
                        tweak: r.tweak,
                        output_pubkeys: r
                            .p2tr
                            .into_iter()
                            .map(|(vout, xonly, value)| TaprootOut { vout, xonly, value })
                            .collect(),
                    },
                );
            }
            return Ok(out);
        }
        Ok(None) if query.prune_seqsigwit() => return pruned_keep_tweaks(query, height),
        Ok(None) => {
            rbitcoin_log::debug!(
                "sp_tweaks: naive fallback h={} (hole or index off)",
                height.0
            );
        }
        Err(e) => return Err(e.into()),
    }
    tweaks_for_height(query, params, height)
}

/// No sealed tweak row under prune. A height still above the prune height
/// is built from the keep window. A dropped height is a hole.
fn pruned_keep_tweaks(
    query: &Query,
    height: Height,
) -> Result<BTreeMap<[u8; 32], TxTweak>, ConfirmError> {
    match query.tweak_index_next() {
        Some(n) if height.0 < n => Err(StoreError::Layout(
            "silent payment tweak index does not cover this height".into(),
        )
        .into()),
        None => Err(StoreError::Layout(
            "silent payment tweaks are unavailable with --prune-seqsigwit".into(),
        )
        .into()),
        Some(_) => tweaks_from_pruned_keep(query, height),
    }
}

/// Tweaks for one pruned height from the keep window.
pub fn tweaks_from_pruned_keep(
    query: &Query,
    height: Height,
) -> Result<BTreeMap<[u8; 32], TxTweak>, ConfirmError> {
    if query
        .witnessed_pruneheight()
        .is_some_and(|ph| height.0 <= ph)
    {
        return Err(StoreError::Layout(
            "silent payment tweak index does not cover this height".into(),
        )
        .into());
    }
    let heights = query.index_heights(height.0, height.0, Some(0))?;
    let window = query.read_index_window(&heights)?;
    let Some(block) = window.blocks.first() else {
        return Ok(BTreeMap::new());
    };
    let txs: Vec<&LoadedTweakTx> = block.txs.iter().collect();
    let tweaks = tweaks_for_txs(&txs, &|fk| Some((window.txid(fk)?, window.outs(fk)?)))?;
    Ok(txs
        .iter()
        .zip(tweaks)
        .filter_map(|(t, tw)| Some((t.rec.txid, tw?)))
        .collect())
}

/// Confirmed height → eligible txid (internal order) → tweak.
///
/// Pre-Taproot and missing heights return an empty map (no error). Does not
/// reconstruct a wire block. Walks packed bodies **and parent outs**.
pub fn tweaks_for_height(
    query: &Query,
    params: &ChainParams,
    height: Height,
) -> Result<BTreeMap<[u8; 32], TxTweak>, ConfirmError> {
    query.require_sp_tweaks_unpruned()?;
    if !params.taproot_active_at(height.0) {
        return Ok(BTreeMap::new());
    }
    let fks = match query.block_tx_fks(height) {
        Ok(f) => f,
        Err(StoreError::NotFound) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    if fks.is_empty() {
        return Ok(BTreeMap::new());
    }

    let wave = rbitcoin_store::load_tweak_wave(&query.store().txs, &fks)?;
    let mut by_fk: HashMap<Fk, ([u8; 32], &[OutputRecord])> = HashMap::new();
    for t in &wave.txs {
        by_fk.insert(t.fk, (t.rec.txid, &t.outs));
    }
    for (fk, (txid, outs)) in &wave.parents {
        by_fk.entry(Fk(*fk)).or_insert((*txid, outs));
    }
    let txs: Vec<&LoadedTweakTx> = wave.txs.iter().collect();
    let tweaks = tweaks_for_txs(&txs, &|fk| by_fk.get(&fk).copied())?;
    Ok(txs
        .iter()
        .zip(tweaks)
        .filter_map(|(t, tw)| Some((t.rec.txid, tw?)))
        .collect())
}

/// Tweak records of `window.blocks[i]` in block order (`None` = ineligible).
pub fn tweak_records_from_window(
    window: &IndexWindow,
    i: usize,
) -> Result<Vec<Option<[u8; 33]>>, ConfirmError> {
    let txs: Vec<&LoadedTweakTx> = window.blocks[i].txs.iter().collect();
    let tweaks = tweaks_for_txs(&txs, &|fk| Some((window.txid(fk)?, window.outs(fk)?)))?;
    Ok(tweaks.into_iter().map(|t| t.map(|t| t.tweak)).collect())
}

/// Per-tx tweak (`None` = ineligible) for `txs` in order. Only P2TR-output
/// txs with loaded inputs are candidates; EC math runs on the caller.
#[doc(hidden)]
pub fn tweaks_for_txs<'a>(
    txs: &[&'a LoadedTweakTx],
    parent: &ParentLookup<'a>,
) -> Result<Vec<Option<TxTweak>>, ConfirmError> {
    let mut out = vec![None; txs.len()];
    for (i, t) in txs.iter().enumerate() {
        if !t.need_seqsigwit {
            continue;
        }
        let Some(inputs) = t.inputs.as_ref() else {
            continue;
        };
        let (tx, prevouts) = build_tx_and_prevouts(inputs, &t.outs, parent)?;
        out[i] = tweak_from_tx(&tx, &prevouts);
    }
    Ok(out)
}

#[allow(clippy::type_complexity)] // packed row / pin / script-hash tuple is the on-disk shape
/// Tweaks-wire `TxTweak` from a stored 33-byte tweak + this tx’s packed outs.
///
/// No parent IO. `Some(tweak)` with missing packed outs is corrupt.
pub fn tweaks_from_thin_and_body(
    rows: &[([u8; 32], Option<[u8; 33]>, Option<&[OutputRecord]>)],
) -> Result<BTreeMap<[u8; 32], TxTweak>, StoreError> {
    let mut out = BTreeMap::new();
    for (txid, tweak, outs) in rows {
        let Some(tweak) = tweak else {
            continue;
        };
        let Some(outs) = outs else {
            return Err(StoreError::Corrupt(
                "invariant: thin tweak missing packed body",
            ));
        };
        out.insert(
            *txid,
            TxTweak {
                tweak: *tweak,
                output_pubkeys: taproot_outs_from_records(outs),
            },
        );
    }
    Ok(out)
}

#[doc(hidden)]
pub fn taproot_outs_from_records(outputs: &[OutputRecord]) -> Vec<TaprootOut> {
    let mut out = Vec::new();
    for (i, o) in outputs.iter().enumerate() {
        if !bitcoin::Script::from_bytes(&o.script).is_p2tr() {
            continue;
        }
        let mut xonly = [0u8; 32];
        xonly.copy_from_slice(&o.script[2..34]);
        let value = o.value;
        out.push(TaprootOut {
            vout: i as u32,
            xonly,
            value,
        });
    }
    out
}

/// A spent parent missing from the lookup is an invariant break, not an
/// ineligible tx.
#[doc(hidden)]
pub fn build_tx_and_prevouts(
    inputs: &[InputRecord],
    outputs: &[OutputRecord],
    parent: &ParentLookup<'_>,
) -> Result<(Transaction, Vec<TxOut>), StoreError> {
    let mut prevouts = Vec::with_capacity(inputs.len());
    let mut txins = Vec::with_capacity(inputs.len());
    for inp in inputs {
        let (prev_txid, prev_script, prev_value) = if inp.is_coinbase() {
            ([0u8; 32], Vec::new(), 0u64)
        } else {
            const MISSING: &str = "invariant: sp_tweaks spent parent missing";
            let (tid, outs) = parent(inp.create_fk()).ok_or(StoreError::Corrupt(MISSING))?;
            let o = outs
                .get(inp.prev_index() as usize)
                .ok_or(StoreError::Corrupt(MISSING))?;
            (tid, o.script.clone(), o.value)
        };
        let wit_refs: Vec<&[u8]> = inp.witness.iter().map(|w| w.as_slice()).collect();
        txins.push(bitcoin::TxIn {
            previous_output: OutPoint {
                txid: bitcoin::Txid::from_byte_array(prev_txid),
                vout: inp.prev_index(),
            },
            script_sig: bitcoin::ScriptBuf::from_bytes(inp.script_sig.clone()),
            sequence: bitcoin::Sequence::from_consensus(inp.sequence),
            witness: Witness::from_slice(&wit_refs),
        });
        let value = bitcoin::Amount::from_sat(prev_value);
        prevouts.push(TxOut {
            value,
            script_pubkey: bitcoin::ScriptBuf::from_bytes(prev_script),
        });
    }
    let mut txouts = Vec::with_capacity(outputs.len());
    for o in outputs {
        let value = bitcoin::Amount::from_sat(o.value);
        txouts.push(TxOut {
            value,
            script_pubkey: bitcoin::ScriptBuf::from_bytes(o.script.clone()),
        });
    }
    Ok((
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: txins,
            output: txouts,
        },
        prevouts,
    ))
}
