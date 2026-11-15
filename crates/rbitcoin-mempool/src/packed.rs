//! MEM_SCHEMA 4 packed live records
//! (`fee‖weight‖sigops‖accept_secs‖txid‖wtxid‖tx‖vin aux`).
//!
//! Schema 3 had no `accept_secs` and schema 2 had no `sigops` either;
//! [`decode_packed_live_v3`] / [`decode_packed_live_v2`] read them for the soft
//! migrate. Every upgraded record takes the migrate time as its accept time.
//! Schema 3 keeps its sigop cost; schema ≤ 2 writes [`SIGOP_COST_UNKNOWN`]
//! until the hub recomputes.
//!
//! Vin codec mirrors Class A packed flags/witness but writes `prev_txid` when
//! there is no Class A `create_fk`. Outputs reuse [`OutputRecord`] unspent
//! encode. Packed decode → `Transaction` → consensus serialize must equal the
//! admitted wire.

use crate::error::MempoolError;
use bitcoin::hashes::Hash;
use bitcoin::{Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, Wtxid};
use rbitcoin_primitives::{le_u64, read_compact_size, write_compact_size, Fk};
use rbitcoin_store::OutputRecord;

#[doc(hidden)]
pub const VIN_SEQ_FINAL: u8 = 1;
#[doc(hidden)]
pub const VIN_EMPTY_SCRIPT: u8 = 1 << 1;
#[doc(hidden)]
pub const VIN_EMPTY_WITNESS: u8 = 1 << 2;
#[doc(hidden)]
pub const VIN_HAS_CREATE_FK: u8 = 1 << 3;
#[doc(hidden)]
pub const VIN_HAS_SCRIPT_HASH: u8 = 1 << 4;

/// `sigops` value for a record migrated from schema ≤ 2 (cost not yet known).
#[doc(hidden)]
pub const SIGOP_COST_UNKNOWN: u64 = u64::MAX;
/// Byte offset of `sigops` in a schema-3 or schema-4 record.
pub const SIGOP_COST_OFF: usize = 16;
/// Byte offset of `accept_secs` in a schema-4 record.
pub const ACCEPT_SECS_OFF: usize = SIGOP_COST_OFF + 8;

/// Per-vin aux persisted with the packed body (SH index / tip-entry purge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VinAux {
    pub prev_txid: Txid,
    pub vout: u32,
    pub script_hash: Option<[u8; 32]>,
    pub create_fk: Option<Fk>,
}

/// One LIVE body payload (schema 4).
#[derive(Debug, Clone)]
pub struct PackedLive {
    pub fee_sat: u64,
    pub weight: u64,
    /// Full sigop cost; `None` for a record migrated from schema ≤ 2.
    pub sigop_cost: Option<u64>,
    /// Node-time seconds at accept (Core `nTime`). A record read from schema
    /// ≤ 3 has none and reads 0; the migrate stamps it.
    pub accept_secs: u64,
    pub txid: Txid,
    pub wtxid: Wtxid,
    pub tx: Transaction,
    pub vins: Vec<VinAux>,
}

#[allow(clippy::too_many_arguments)] // one field per record column
pub fn encode_packed_live(
    tx: &Transaction,
    txid: &Txid,
    wtxid: &Wtxid,
    fee_sat: u64,
    weight: u64,
    sigop_cost: Option<u64>,
    accept_secs: u64,
    vins: &[VinAux],
) -> Result<Vec<u8>, MempoolError> {
    if vins.len() != tx.input.len() && !vins.is_empty() {
        return Err(MempoolError::Corrupt("vin aux count"));
    }
    let mut out = Vec::with_capacity(80 + tx.input.len() * 80 + tx.output.len() * 40);
    out.extend_from_slice(&fee_sat.to_le_bytes());
    out.extend_from_slice(&weight.to_le_bytes());
    let sigops = sigop_cost.unwrap_or(SIGOP_COST_UNKNOWN);
    out.extend_from_slice(&sigops.to_le_bytes());
    out.extend_from_slice(&accept_secs.to_le_bytes());
    out.extend_from_slice(txid.as_byte_array());
    out.extend_from_slice(wtxid.as_byte_array());
    out.extend_from_slice(&tx.version.0.to_le_bytes());
    out.extend_from_slice(&tx.lock_time.to_consensus_u32().to_le_bytes());
    write_compact_size(&mut out, tx.input.len() as u64);
    write_compact_size(&mut out, tx.output.len() as u64);
    for (i, inp) in tx.input.iter().enumerate() {
        let aux = vins.get(i);
        encode_vin(&mut out, inp, aux);
    }
    for o in &tx.output {
        OutputRecord::encode_unspent_into(o.value.to_sat(), o.script_pubkey.as_bytes(), &mut out);
    }
    Ok(out)
}

pub fn decode_packed_live(buf: &[u8]) -> Result<PackedLive, MempoolError> {
    decode_packed(buf, true, true)
}

/// Schema-3 record (no `accept_secs` field) for the 3 → 4 soft migrate.
pub fn decode_packed_live_v3(buf: &[u8]) -> Result<PackedLive, MempoolError> {
    decode_packed(buf, true, false)
}

/// Schema-2 record (no `sigops` or `accept_secs`) for the 2 → 4 soft migrate.
pub fn decode_packed_live_v2(buf: &[u8]) -> Result<PackedLive, MempoolError> {
    decode_packed(buf, false, false)
}

#[doc(hidden)]
pub fn decode_packed(
    buf: &[u8],
    has_sigops: bool,
    has_accept: bool,
) -> Result<PackedLive, MempoolError> {
    let ids = 16 + if has_sigops { 8 } else { 0 } + if has_accept { 8 } else { 0 };
    if buf.len() < ids + 32 + 32 + 8 {
        return Err(MempoolError::Corrupt("packed live short"));
    }
    let mut head = [0u8; 16];
    head.copy_from_slice(&buf[..16]);
    let fee_sat = le_u64!(head, 0);
    let weight = le_u64!(head, 8);
    let sigop_cost = if has_sigops {
        let mut word = [0u8; 8];
        word.copy_from_slice(&buf[16..24]);
        let v = u64::from_le_bytes(word);
        // Above the block limit is unknown: covers SIGOP_COST_UNKNOWN and a
        // torn in-place write-back (0xFF.. partly overwritten, see
        // `Mempool::set_sigop_cost`), which always overestimates.
        (v <= crate::graph::MAX_BLOCK_SIGOPS_COST).then_some(v)
    } else {
        None
    };
    let accept_secs = if has_accept {
        let mut word = [0u8; 8];
        word.copy_from_slice(&buf[ids - 8..ids]);
        u64::from_le_bytes(word)
    } else {
        0
    };
    let mut txid_bytes = [0u8; 32];
    txid_bytes.copy_from_slice(&buf[ids..ids + 32]);
    let mut wtxid_bytes = [0u8; 32];
    wtxid_bytes.copy_from_slice(&buf[ids + 32..ids + 64]);
    let txid = Txid::from_byte_array(txid_bytes);
    let wtxid = Wtxid::from_byte_array(wtxid_bytes);
    let mut off = ids + 64;
    let version = i32::from_le_bytes(take_arr(buf, &mut off)?);
    let lock_time = u32::from_le_bytes(take_arr(buf, &mut off)?);
    let (n_in, n) = read_compact_size(&buf[off..])?;
    off += n;
    let (n_out, n) = read_compact_size(&buf[off..])?;
    off += n;
    if n_in > 100_000 || n_out > 100_000 {
        return Err(MempoolError::Corrupt("packed live counts"));
    }
    let mut input = Vec::with_capacity(n_in as usize);
    let mut vins = Vec::with_capacity(n_in as usize);
    for _ in 0..n_in {
        let (txin, aux, n) = decode_vin(&buf[off..])?;
        off += n;
        input.push(txin);
        vins.push(aux);
    }
    let mut output = Vec::with_capacity(n_out as usize);
    for _ in 0..n_out {
        let (rec, n) = OutputRecord::decode_at(&buf[off..])
            .map_err(|_| MempoolError::Corrupt("packed vout"))?;
        off += n;
        let value = Amount::from_sat(rec.value);
        output.push(TxOut {
            value,
            script_pubkey: ScriptBuf::from_bytes(rec.script),
        });
    }
    if off != buf.len() {
        return Err(MempoolError::Corrupt("packed live trailing"));
    }
    let tx = Transaction {
        version: bitcoin::transaction::Version(version),
        lock_time: bitcoin::absolute::LockTime::from_consensus(lock_time),
        input,
        output,
    };
    Ok(PackedLive {
        fee_sat,
        weight,
        sigop_cost,
        accept_secs,
        txid,
        wtxid,
        tx,
        vins,
    })
}

#[doc(hidden)]
pub fn encode_vin(out: &mut Vec<u8>, inp: &TxIn, aux: Option<&VinAux>) {
    let mut flags = 0u8;
    if inp.sequence == Sequence::MAX {
        flags |= VIN_SEQ_FINAL;
    }
    if inp.script_sig.is_empty() {
        flags |= VIN_EMPTY_SCRIPT;
    }
    if inp.witness.is_empty() {
        flags |= VIN_EMPTY_WITNESS;
    }
    if aux.is_some_and(|a| a.create_fk.is_some()) {
        flags |= VIN_HAS_CREATE_FK;
    }
    if aux.is_some_and(|a| a.script_hash.is_some()) {
        flags |= VIN_HAS_SCRIPT_HASH;
    }
    out.push(flags);
    out.extend_from_slice(inp.previous_output.txid.as_byte_array());
    write_compact_size(out, u64::from(inp.previous_output.vout));
    if flags & VIN_SEQ_FINAL == 0 {
        out.extend_from_slice(&inp.sequence.to_consensus_u32().to_le_bytes());
    }
    if flags & VIN_EMPTY_SCRIPT == 0 {
        let s = inp.script_sig.as_bytes();
        write_compact_size(out, s.len() as u64);
        out.extend_from_slice(s);
    }
    if flags & VIN_EMPTY_WITNESS == 0 {
        write_compact_size(out, inp.witness.len() as u64);
        for item in inp.witness.iter() {
            write_compact_size(out, item.len() as u64);
            out.extend_from_slice(item);
        }
    }
    if let Some(a) = aux {
        if let Some(fk) = a.create_fk {
            out.extend_from_slice(&fk.0.to_le_bytes());
        }
        if let Some(sh) = a.script_hash {
            out.extend_from_slice(&sh);
        }
    }
}

#[doc(hidden)]
pub fn decode_vin(buf: &[u8]) -> Result<(TxIn, VinAux, usize), MempoolError> {
    if buf.is_empty() {
        return Err(MempoolError::Corrupt("packed vin short"));
    }
    let flags = buf[0];
    let mut off = 1usize;
    if flags & 0xE0 != 0 {
        return Err(MempoolError::Corrupt("packed vin flags"));
    }
    let prev_txid = Txid::from_byte_array(take_arr(buf, &mut off)?);
    let (vout64, n) = read_compact_size(&buf[off..])?;
    off += n;
    if vout64 > u64::from(u32::MAX) {
        return Err(MempoolError::Corrupt("packed vout"));
    }
    let vout = vout64 as u32;
    let sequence = if flags & VIN_SEQ_FINAL != 0 {
        Sequence::MAX
    } else {
        Sequence::from_consensus(u32::from_le_bytes(take_arr(buf, &mut off)?))
    };
    let script_sig = if flags & VIN_EMPTY_SCRIPT != 0 {
        ScriptBuf::new()
    } else {
        let (len, n) = read_compact_size(&buf[off..])?;
        off += n;
        let end = off
            .checked_add(len as usize)
            .ok_or(MempoolError::Corrupt("packed script"))?;
        if end > buf.len() {
            return Err(MempoolError::Corrupt("packed script"));
        }
        let s = ScriptBuf::from_bytes(buf[off..end].to_vec());
        off = end;
        s
    };
    let witness = if flags & VIN_EMPTY_WITNESS != 0 {
        Witness::new()
    } else {
        let (n_items, n) = read_compact_size(&buf[off..])?;
        off += n;
        let mut items = Vec::with_capacity(n_items as usize);
        for _ in 0..n_items {
            let (len, n) = read_compact_size(&buf[off..])?;
            off += n;
            let end = off
                .checked_add(len as usize)
                .ok_or(MempoolError::Corrupt("packed witness"))?;
            if end > buf.len() {
                return Err(MempoolError::Corrupt("packed witness"));
            }
            items.push(buf[off..end].to_vec());
            off = end;
        }
        Witness::from_slice(&items)
    };
    let create_fk = if flags & VIN_HAS_CREATE_FK != 0 {
        Some(Fk(u64::from_le_bytes(take_arr(buf, &mut off)?)))
    } else {
        None
    };
    let script_hash = if flags & VIN_HAS_SCRIPT_HASH != 0 {
        Some(take_arr::<32>(buf, &mut off)?)
    } else {
        None
    };
    let txin = TxIn {
        previous_output: bitcoin::OutPoint {
            txid: prev_txid,
            vout,
        },
        script_sig,
        sequence,
        witness,
    };
    let aux = VinAux {
        prev_txid,
        vout,
        script_hash,
        create_fk,
    };
    Ok((txin, aux, off))
}

#[doc(hidden)]
pub fn take_arr<const N: usize>(buf: &[u8], off: &mut usize) -> Result<[u8; N], MempoolError> {
    let end = off
        .checked_add(N)
        .ok_or(MempoolError::Corrupt("packed short"))?;
    if end > buf.len() {
        return Err(MempoolError::Corrupt("packed short"));
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&buf[*off..end]);
    *off = end;
    Ok(arr)
}
