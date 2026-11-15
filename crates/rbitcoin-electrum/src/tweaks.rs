//! `blockchain.tweaks.subscribe` (naive, uncached).
//!
//! Stream: JSON-RPC result is the first height; remaining heights are
//! notifications; `{"message":"done"}` ends the **chunk** (60s wall at a
//! wave boundary, or the requested `count` if that finishes first). Cake
//! resubscribes. kiss-bdk one-shot stops until it loops (kiss-bdk#10).
//! Pre-taproot empty maps collapse into one notify (≤1024 keys).
//! `historicalMode=false` omits confirmed-spent P2TR outs.
//! Serve-time `--sptweaks-dust` omits P2TR outs with `value <=` the floor.

use rbitcoin_consensus::{ChainParams, TxTweak};
use rbitcoin_primitives::Height;
use rbitcoin_query::tweaks_for_height;
use rbitcoin_query::{Query, ThinTweakRangeLimits, ThinTweakRow};
#[cfg(debug_assertions)]
use serde_json::Map;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Cake-compatible subscribe chunk: `done` at a wave boundary after this
/// wall time so Cake resubscribes (persist lags one event; cake_wallet#3574).
pub const SUBSCRIBE_CHUNK: Duration = Duration::from_secs(60);

/// Omit served P2TR `output_pubkeys` with `value <=` this many sats.
///
/// `0` serves every value. Default **1000**. **545** matches Cake electrs
/// `sp_min_dust` 546, which keeps `value >= 546`. Index still stores every eligible tweak.
pub const DEFAULT_TWEAKS_MIN_DUST: u64 = 1000;

#[doc(hidden)]
pub fn keep_p2tr_value(value: u64, min_dust: u64) -> bool {
    min_dust == 0 || value > min_dust
}

/// End this RPC (`done`) when heights remain and the chunk budget has elapsed.
pub fn seal_subscribe_chunk(elapsed: Duration, budget: Duration, more: bool) -> bool {
    more && elapsed >= budget
}

/// Another wave may run: heights remain and the 60s chunk has not elapsed.
pub fn chunk_still_open(elapsed: Duration, more: bool) -> bool {
    more && !seal_subscribe_chunk(elapsed, SUBSCRIBE_CHUNK, more)
}

/// Parsed `blockchain.tweaks.subscribe` window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TweakReq {
    pub start: u32,
    /// Requested height count (scan sends tip − restore). Served until
    /// `count`/tip **or** the 60s wave-boundary chunk, whichever first.
    pub count: u32,
    /// Cake `historicalMode`. `false` → cut-through spent P2TR outs.
    pub historical: bool,
}

pub fn parse_req(params: &Value) -> Result<TweakReq, String> {
    let start = param_u32(params, 0)?;
    let count = param_u32(params, 1).unwrap_or(1).max(1);
    let historical = param_bool(params, 2).unwrap_or(false);
    Ok(TweakReq {
        start,
        count,
        historical,
    })
}

/// Inclusive last height to serve (`start` if `count==1`), not past tip.
pub fn last_height(start: u32, count: u32, tip: Option<u32>) -> Option<u32> {
    let tip = tip?;
    if start > tip {
        return None;
    }
    Some(start.saturating_add(count.saturating_sub(1)).min(tip))
}

/// One height key → txs. Cake `fromJson` uses the **last** map key as `block`.
pub fn height_map(
    query: &Query,
    chain: &ChainParams,
    h: u32,
    cut_through: bool,
    min_dust: u64,
) -> Result<Value, String> {
    let s = height_map_json(query, chain, h, cut_through, min_dust)?;
    serde_json::from_str(&s).map_err(|e| e.to_string())
}

/// Cake height map as JSON text (no `serde_json::Value` tree).
pub fn height_map_json(
    query: &Query,
    chain: &ChainParams,
    h: u32,
    cut_through: bool,
    min_dust: u64,
) -> Result<String, String> {
    height_map_json_with_tip(
        query,
        chain,
        h,
        query.tip_height().map(|t| t.0),
        cut_through,
        min_dust,
    )
}

/// Same map as [`height_map_json`], using `tip` instead of a second read.
#[doc(hidden)]
pub fn height_map_json_with_tip(
    query: &Query,
    chain: &ChainParams,
    h: u32,
    tip: Option<u32>,
    cut_through: bool,
    min_dust: u64,
) -> Result<String, String> {
    if tip.is_none_or(|t| h > t) || !chain.taproot_active_at(h) {
        return Ok(empty_height_json(h));
    }
    let limits = ThinTweakRangeLimits {
        max_heights: 1,
        max_eligible: usize::MAX,
        cut_through,
    };
    match query.load_thin_tweaks_range(Height(h), limits) {
        Ok(mut batch) if !batch.is_empty() => {
            let rows = batch.pop().map(|(_, r)| r).unwrap_or_default();
            Ok(encode_thin_height_json(h, &rows, min_dust))
        }
        Ok(_) if query.prune_seqsigwit() => match query.tweak_index_next() {
            // Below the sealed watermark the index skipped this height.
            Some(n) if h < n => Err("silent payment tweak index does not cover this height".into()),
            None => Err("silent payment tweaks are unavailable with --prune-seqsigwit".into()),
            // The tip moved before this height's row was sealed. The keep
            // window still has the witness. An empty map would tell the
            // client the height was scanned.
            Some(_) => {
                let mut tweaks = rbitcoin_query::tweaks_from_pruned_keep(query, Height(h))
                    .map_err(|e| e.to_string())?;
                if cut_through {
                    retain_unspent_taproot(query, Height(h), &mut tweaks)?;
                }
                Ok(encode_height_map(h, &tweaks, min_dust))
            }
        },
        Ok(_) => {
            let mut tweaks =
                tweaks_for_height(query, chain, Height(h)).map_err(|e| e.to_string())?;
            if cut_through {
                retain_unspent_taproot(query, Height(h), &mut tweaks)?;
            }
            Ok(encode_height_map(h, &tweaks, min_dust))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn encode_height_map(h: u32, tweaks: &BTreeMap<[u8; 32], TxTweak>, min_dust: u64) -> String {
    let mut s = String::new();
    s.push('{');
    push_quoted_u32(&mut s, h);
    s.push(':');
    push_height_object_json(&mut s, tweaks, min_dust);
    s.push('}');
    s
}

/// Electrum notify line for one height (json-rpc wrapper + newline not included).
pub fn height_notify_json(
    query: &Query,
    chain: &ChainParams,
    h: u32,
    cut_through: bool,
    min_dust: u64,
) -> Result<String, String> {
    let map = height_map_json(query, chain, h, cut_through, min_dust)?;
    Ok(wrap_height_notify(&map))
}

#[doc(hidden)]
pub fn wrap_height_notify(map_json: &str) -> String {
    let mut s = String::with_capacity(map_json.len() + 80);
    s.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"blockchain.tweaks.subscribe\",\"params\":[");
    s.push_str(map_json);
    s.push_str("]}");
    s
}

/// Serve budgets plus Cake cut-through when `historical` is false.
pub fn subscribe_serve_limits(historical: bool) -> ThinTweakRangeLimits {
    ThinTweakRangeLimits {
        cut_through: !historical,
        ..ThinTweakRangeLimits::default()
    }
}

/// First JSON-RPC height plus remaining notifies of the same thin wave.
pub struct FirstWave {
    pub result_json: String,
    pub rest_notifies: Vec<String>,
    pub consumed: u32,
}

/// One remaining-wave write: `consumed` heights, possibly fewer JSON lines
/// (pre-taproot empty maps collapse into one notify).
pub struct NotifyWave {
    pub lines: Vec<String>,
    pub consumed: u32,
}

/// Indexed: one thin batch starting at `start`. Pre-taproot / hole: first
/// height only (`consumed == 1`).
pub fn first_subscribe_wave(
    query: &Query,
    chain: &ChainParams,
    start: u32,
    last: u32,
    limits: ThinTweakRangeLimits,
    min_dust: u64,
) -> Result<FirstWave, String> {
    if start > last {
        return Ok(FirstWave {
            result_json: empty_height_json(start),
            rest_notifies: Vec::new(),
            consumed: 1,
        });
    }
    if !chain.taproot_active_at(start) {
        return Ok(FirstWave {
            result_json: empty_height_json(start),
            rest_notifies: Vec::new(),
            consumed: 1,
        });
    }
    let thin = load_thin_batch(query, start, last, limits)?;
    if thin.is_empty() {
        return Ok(FirstWave {
            result_json: height_map_json(query, chain, start, limits.cut_through, min_dust)?,
            rest_notifies: Vec::new(),
            consumed: 1,
        });
    }
    let (h0, rows0) = &thin[0];
    debug_assert_eq!(*h0, start);
    let rest_notifies = thin[1..]
        .iter()
        .map(|(h, rows)| thin_height_notify_json(*h, rows, min_dust))
        .collect();
    Ok(FirstWave {
        result_json: encode_thin_height_json(*h0, rows0, min_dust),
        rest_notifies,
        consumed: thin.len() as u32,
    })
}

/// Pre-taproot empty notifies per write (no store). Mainnet genesis→origin
/// is ~700k heights; one flush per height is the 10/s-class path. One notify
/// carries up to this many empty height keys (Cake `fromJson` last key = progress).
pub const EMPTY_WAVE_HEIGHTS: u32 = 1024;

/// Inclusive last height of a pre-taproot empty wave, if `start` is below
/// taproot. `None` when taproot is already active at `start` (incl. regtest).
pub fn pre_taproot_wave_last(start: u32, last: u32, taproot_h: u32) -> Option<u32> {
    if start >= taproot_h {
        return None;
    }
    let cap = start.saturating_add(EMPTY_WAVE_HEIGHTS.saturating_sub(1));
    Some(last.min(taproot_h.saturating_sub(1)).min(cap))
}

/// One subscribe wave of notify lines starting at `start`.
///
/// Pre-taproot: one notify whose params map has every empty height in the
/// wave (Cake last-key progress). Indexed: thin batch, one line per height.
/// Hole: one naive/thin height.
pub fn remaining_notify_lines(
    query: &Query,
    chain: &ChainParams,
    start: u32,
    last: u32,
    limits: ThinTweakRangeLimits,
    min_dust: u64,
) -> Result<NotifyWave, String> {
    if start > last {
        return Ok(NotifyWave {
            lines: Vec::new(),
            consumed: 0,
        });
    }
    if let Some(end) = pre_taproot_wave_last(start, last, chain.taproot_height()) {
        let consumed = end.saturating_sub(start).saturating_add(1);
        return Ok(NotifyWave {
            lines: vec![wrap_height_notify(&empty_heights_json(start, end))],
            consumed,
        });
    }
    let thin = load_thin_batch(query, start, last, limits)?;
    if thin.is_empty() {
        return Ok(NotifyWave {
            lines: vec![height_notify_json(
                query,
                chain,
                start,
                limits.cut_through,
                min_dust,
            )?],
            consumed: 1,
        });
    }
    let consumed = thin.len() as u32;
    Ok(NotifyWave {
        lines: thin
            .into_iter()
            .map(|(h, rows)| thin_height_notify_json(h, &rows, min_dust))
            .collect(),
        consumed,
    })
}

/// Load a budgeted contiguous thin-index batch starting at `start`.
///
/// Empty → first height not indexed (caller uses per-height naive/thin).
/// Otherwise each entry is an indexed height (possibly zero eligible rows).
pub fn load_thin_batch(
    query: &Query,
    start: u32,
    last: u32,
    limits: ThinTweakRangeLimits,
) -> Result<Vec<(u32, Vec<ThinTweakRow>)>, String> {
    if start > last {
        return Ok(Vec::new());
    }
    let max_h = last.saturating_sub(start).saturating_add(1);
    let limits = ThinTweakRangeLimits {
        max_heights: limits.max_heights.min(max_h),
        ..limits
    };
    let batch = query
        .load_thin_tweaks_range(Height(start), limits)
        .map_err(|e| e.to_string())?;
    Ok(batch.into_iter().map(|(h, rows)| (h.0, rows)).collect())
}

/// Notify JSON for one thin-index height map.
pub fn thin_height_notify_json(h: u32, rows: &[ThinTweakRow], min_dust: u64) -> String {
    wrap_height_notify(&encode_thin_height_json(h, rows, min_dust))
}

#[doc(hidden)]
pub fn empty_height_json(h: u32) -> String {
    empty_heights_json(h, h)
}

#[doc(hidden)]
pub fn empty_heights_json(first: u32, last: u32) -> String {
    let n = last.saturating_sub(first).saturating_add(1) as usize;
    let mut s = String::with_capacity(n.saturating_mul(12).saturating_add(2));
    s.push('{');
    for h in first..=last {
        if h > first {
            s.push(',');
        }
        push_quoted_u32(&mut s, h);
        s.push_str(":{}");
    }
    s.push('}');
    s
}

#[doc(hidden)]
pub fn retain_unspent_taproot(
    query: &Query,
    height: Height,
    tweaks: &mut BTreeMap<[u8; 32], TxTweak>,
) -> Result<(), String> {
    let fks = query.block_tx_fks(height).map_err(|e| e.to_string())?;
    for fk in fks {
        let txid = query.store().txs.body_txid(fk).map_err(|e| e.to_string())?;
        let drop = {
            let Some(t) = tweaks.get_mut(&txid) else {
                continue;
            };
            if !t.output_pubkeys.is_empty() {
                let vouts: Vec<u32> = t.output_pubkeys.iter().map(|o| o.vout).collect();
                let live = query
                    .unspent_create_vouts(fk, &vouts)
                    .map_err(|e| e.to_string())?;
                rbitcoin_store::keep_unspent_vout_subsequence(&mut t.output_pubkeys, &live, |o| {
                    o.vout
                });
            }
            t.output_pubkeys.is_empty()
        };
        if drop {
            tweaks.remove(&txid);
        }
    }
    Ok(())
}

pub fn encode_thin_height_json(
    h: u32,
    rows: &[rbitcoin_query::ThinTweakRow],
    min_dust: u64,
) -> String {
    let mut s = String::with_capacity(64 + rows.len() * 192);
    s.push('{');
    push_quoted_u32(&mut s, h);
    s.push_str(":{");
    let mut first_tx = true;
    for r in rows {
        let mut first_out = true;
        for (vout, xonly, value) in &r.p2tr {
            if !keep_p2tr_value(*value, min_dust) {
                continue;
            }
            if first_out {
                if !first_tx {
                    s.push(',');
                }
                first_tx = false;
                first_out = false;
                s.push('"');
                push_txid_display_hex(&mut s, &r.txid);
                s.push_str("\":{\"tweak\":\"");
                push_hex(&mut s, &r.tweak);
                s.push_str("\",\"output_pubkeys\":{");
            } else {
                s.push(',');
            }
            s.push('"');
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{vout}"));
            s.push_str("\":[\"");
            push_hex(&mut s, xonly);
            s.push_str("\",");
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{value}"));
            s.push(']');
        }
        if !first_out {
            s.push_str("}}");
        }
    }
    s.push_str("}}");
    s
}

fn push_height_object_json(
    s: &mut String,
    tweaks: &std::collections::BTreeMap<[u8; 32], TxTweak>,
    min_dust: u64,
) {
    s.push('{');
    let mut first_tx = true;
    for (txid, t) in tweaks {
        let mut first_out = true;
        for o in &t.output_pubkeys {
            if !keep_p2tr_value(o.value, min_dust) {
                continue;
            }
            if first_out {
                if !first_tx {
                    s.push(',');
                }
                first_tx = false;
                first_out = false;
                s.push('"');
                push_txid_display_hex(s, txid);
                s.push_str("\":{\"tweak\":\"");
                push_hex(s, &t.tweak);
                s.push_str("\",\"output_pubkeys\":{");
            } else {
                s.push(',');
            }
            s.push('"');
            let _ = core::fmt::Write::write_fmt(s, format_args!("{}", o.vout));
            s.push_str("\":[\"");
            push_hex(s, &o.xonly);
            s.push_str("\",");
            let _ = core::fmt::Write::write_fmt(s, format_args!("{}", o.value));
            s.push(']');
        }
        if !first_out {
            s.push_str("}}");
        }
    }
    s.push('}');
}

fn push_quoted_u32(s: &mut String, n: u32) {
    s.push('"');
    let _ = core::fmt::Write::write_fmt(s, format_args!("{n}"));
    s.push('"');
}

fn push_hex(s: &mut String, data: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in data {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
}

fn push_txid_display_hex(s: &mut String, txid: &[u8; 32]) {
    s.push_str(&rbitcoin_primitives::display_hash_hex(*(txid)));
}

/// Cake `noData` / resubscribe signal (`fromJson` catch path reads `message`).
pub fn done_notify() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "blockchain.tweaks.subscribe",
        "params": [{"message": "done"}],
    })
}

/// JSON-RPC **result** is the **first** height only. Further heights are
/// notifications from the server loop.
pub fn subscribe(
    query: &Query,
    params: &Value,
    chain: &ChainParams,
    min_dust: u64,
) -> Result<Value, String> {
    let req = parse_req(params)?;
    let t0 = Instant::now();
    let map = height_map(query, chain, req.start, !req.historical, min_dust)?;
    rbitcoin_log::trace!(
        "electrum: tweaks h={} count={} result_keys={} wall_ms={}",
        req.start,
        req.count,
        map.as_object().map(|o| o.len()).unwrap_or(0),
        t0.elapsed().as_millis()
    );
    Ok(map)
}

fn param_u32(params: &Value, idx: usize) -> Result<u32, String> {
    crate::server::param_u32(params, idx)
}

fn param_bool(params: &Value, idx: usize) -> Option<bool> {
    params.as_array().and_then(|a| a.get(idx)).and_then(|v| {
        v.as_bool()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

#[cfg(debug_assertions)]
pub fn height_object(tweaks: &std::collections::BTreeMap<[u8; 32], TxTweak>) -> Value {
    let mut txs = Map::new();
    for (txid, t) in tweaks {
        txs.insert(txid_display_hex(txid), encode_tx_tweak(t));
    }
    Value::Object(txs)
}

#[cfg(debug_assertions)]
pub fn encode_tx_tweak(t: &TxTweak) -> Value {
    let mut outs = Map::new();
    for o in &t.output_pubkeys {
        outs.insert(
            o.vout.to_string(),
            json!([rbitcoin_primitives::hex_encode(o.xonly), o.value]),
        );
    }
    json!({
        "tweak": rbitcoin_primitives::hex_encode(t.tweak),
        "output_pubkeys": Value::Object(outs),
    })
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn txid_display_hex(txid: &[u8; 32]) -> String {
    rbitcoin_primitives::display_hash_hex(*(txid))
}
