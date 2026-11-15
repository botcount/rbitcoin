//! Schema v5 spend annotations on create outputs (replaces v4 point multimap).
//!
//! - Sole spender: `output.spender_field = spending_tx_fk` (MULTI clear).
//! - Multi: MULTI set, `spender_field` = head of [`crate::spender_table::SpenderTable`].
//!
//! Best-chain views filter with `is_confirmed_strong` (leave annotations on reorg).

use crate::error::StoreError;
use crate::spender_table::SpenderTable;
use crate::tx_table::TxTable;
use rbitcoin_primitives::Fk;

/// Query-facing spend edge (outpoint filled by caller args).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointRecord {
    pub out_txid: [u8; 32],
    pub out_index: u32,
    pub spending_tx_fk: Fk,
    pub spending_vin: u32,
}

/// Mark create outpoint spent by `spending_tx_fk` at `spending_vin` (promote to multi-list if needed).
pub fn put_spend_on_create(
    txs: &TxTable,
    spenders: &SpenderTable,
    create_tx_fk: Fk,
    vout: u32,
    spending_tx_fk: Fk,
    spending_vin: u32,
) -> Result<(), StoreError> {
    put_spend_on_create_at(
        txs,
        spenders,
        create_tx_fk,
        vout,
        spending_tx_fk,
        spending_vin,
        None,
    )
}

/// Like [`put_spend_on_create`] with optional cache-held body `(offset, len)` — **no idx**.
pub fn put_spend_on_create_at(
    txs: &TxTable,
    spenders: &SpenderTable,
    create_tx_fk: Fk,
    vout: u32,
    spending_tx_fk: Fk,
    spending_vin: u32,
    body_range: Option<(u64, u64)>,
) -> Result<(), StoreError> {
    if create_tx_fk.is_null() || spending_tx_fk.is_null() {
        return Err(StoreError::InvalidFk);
    }
    let (body_off, body_len) = match body_range {
        Some(range) => range,
        None => txs.spent_range(create_tx_fk)?,
    };
    let (multi, field, field_vin) = txs.get_output_spender_meta_at(body_off, body_len, vout)?;

    let set = |multi: bool, field: Fk, vin: u32| -> Result<(), StoreError> {
        txs.store_spend_slot_at(body_off, body_len, vout, multi, field, vin)
    };

    if !multi && field.is_null() {
        return set(false, spending_tx_fk, spending_vin);
    }
    if !multi && field == spending_tx_fk && field_vin == spending_vin {
        return Ok(());
    }
    if !multi {
        // IBD first-spend path is sole-only; multi is rare (reorg / double annotate).
        let e1 = spenders.append(field, field_vin, Fk::NULL)?;
        let e2 = spenders.append(spending_tx_fk, spending_vin, e1)?;
        return set(true, e2, 0);
    }
    let cap = spenders.count();
    let mut cur = Some(field);
    let mut steps = 0u64;
    while let Some(fk) = cur {
        steps = steps.saturating_add(1);
        if steps > cap {
            return Err(StoreError::Corrupt("invariant: spender multi-list cycle"));
        }
        let (spend_tx, spend_vin, next) = spenders.get(fk)?;
        if spend_tx == spending_tx_fk && spend_vin == spending_vin {
            return Ok(());
        }
        cur = if next.is_null() { None } else { Some(next) };
    }
    let e = spenders.append(spending_tx_fk, spending_vin, field)?;
    set(true, e, 0)
}

/// Visit `(spending_tx_fk, vin)` for a create outpoint (no Class C filter).
pub fn for_each_spender_create<F>(
    txs: &TxTable,
    spenders: &SpenderTable,
    create_tx_fk: Fk,
    vout: u32,
    mut visit: F,
) -> Result<(), StoreError>
where
    F: FnMut(Fk, u32) -> Result<bool, StoreError>,
{
    if create_tx_fk.is_null() {
        return Ok(());
    }
    let Some(loc) = txs
        .create_loc_range_batch(&[create_tx_fk])?
        .into_iter()
        .next()
        .flatten()
    else {
        return Ok(());
    };
    // Callers pass a request's vout. Past `n_out` there is no output, so no
    // spender; the slot read would report it as corrupt.
    if vout >= loc.n_out {
        return Ok(());
    }
    let (off, len) = loc.spent;
    let (multi, field, vin) = txs.get_output_spender_meta_at(off, len, vout)?;
    if field.is_null() {
        return Ok(());
    }
    if !multi {
        let _ = visit(field, vin)?;
        return Ok(());
    }
    let cap = spenders.count();
    let mut cur = Some(field);
    let mut steps = 0u64;
    while let Some(fk) = cur {
        steps = steps.saturating_add(1);
        if steps > cap {
            return Err(StoreError::Corrupt("invariant: spender multi-list cycle"));
        }
        let (spend_tx, spend_vin, next) = spenders.get(fk)?;
        if !visit(spend_tx, spend_vin)? {
            return Ok(());
        }
        cur = if next.is_null() { None } else { Some(next) };
    }
    Ok(())
}
