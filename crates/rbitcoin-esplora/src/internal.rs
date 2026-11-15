//! mempool/electrs `/internal/*` bulk REST and `/mempool/txids/page`.

use crate::handlers::{confirmed_output_count, spawn_join, write_outspends};
#[cfg(unix)]
use crate::handlers::{outspend_json, resolve_address_sh, sh_info_reply};
use crate::server::{
    block_hash_hex, json_bytes_reply, json_reply_here, parse_hash32, pin_or_reject, store_err,
    AppState,
};
#[cfg(unix)]
use crate::server::{not_found, AsOf};
#[cfg(unix)]
use crate::tx_json::{
    build_tx_json, build_tx_json_from_tx, build_tx_json_from_tx_with_status, tx_status_json,
};
#[cfg(unix)]
use axum::body::Bytes;
use axum::extract::{Path, Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bitcoin::hashes::Hash;
use bitcoin::Txid;
#[cfg(unix)]
use rbitcoin_net::{MempoolHub, MempoolTxSnapEntry};
use rbitcoin_query::ChainViewKind;
#[cfg(unix)]
use rbitcoin_store::StoreError;
use serde::Deserialize;
#[cfg(unix)]
use serde_json::{json, Value};

#[doc(hidden)]
pub const DEFAULT_MAX_TXS: usize = 10_000;

#[derive(Deserialize)]
pub struct MaxTxs {
    pub max_txs: Option<usize>,
}

#[doc(hidden)]
pub fn cap_max_txs(q: &MaxTxs) -> usize {
    q.max_txs.unwrap_or(DEFAULT_MAX_TXS).min(DEFAULT_MAX_TXS)
}

#[doc(hidden)]
pub fn bad_request(msg: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, msg).into_response()
}

#[cfg(unix)]
#[allow(clippy::result_large_err)] // axum Response
#[doc(hidden)]
pub fn parse_txid_array(body: &[u8]) -> Result<Vec<[u8; 32]>, Response> {
    let v: Value = serde_json::from_slice(body).map_err(|_| bad_request("invalid json"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| bad_request("body must be a JSON array of txid hex"))?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let s = x
            .as_str()
            .ok_or_else(|| bad_request("txid must be a hex string"))?;
        let id = parse_hash32(s).map_err(|_| bad_request("unparseable txid"))?;
        out.push(id);
    }
    Ok(out)
}

#[cfg(unix)]
#[doc(hidden)]
pub fn cache_json_str(
    slot: &std::sync::OnceLock<Box<str>>,
    build: impl FnOnce() -> Result<String, ()>,
) -> Option<&str> {
    if let Some(s) = slot.get() {
        return Some(s.as_ref());
    }
    match build() {
        Ok(s) => {
            let _ = slot.set(s.into());
            slot.get().map(|b| b.as_ref())
        }
        Err(()) => None,
    }
}

#[cfg(unix)]
#[doc(hidden)]
pub fn join_cached_objects(parts: &[Option<&str>]) -> String {
    let mut body = String::from("[");
    let mut first = true;
    for p in parts {
        let Some(s) = p else {
            continue;
        };
        if !first {
            body.push(',');
        }
        first = false;
        body.push_str(s);
    }
    body.push(']');
    body
}

#[cfg(unix)]
#[doc(hidden)]
pub fn entry_json_str<'a>(
    st: &AppState,
    mp: &MempoolHub,
    e: &'a MempoolTxSnapEntry,
) -> Option<&'a str> {
    cache_json_str(&e.json, || {
        let v = build_tx_json_from_tx(
            &st.query,
            &e.tx,
            st.network,
            Some(e.fee_sat as i64),
            Some(mp),
        )
        .map_err(|_| ())?;
        serde_json::to_string(&v).map_err(|_| ())
    })
}

#[cfg(unix)]
#[doc(hidden)]
pub fn entry_json(st: &AppState, mp: &MempoolHub, e: &MempoolTxSnapEntry) -> Option<Value> {
    let raw = entry_json_str(st, mp, e)?;
    serde_json::from_str(raw).ok()
}

#[cfg(unix)]
#[doc(hidden)]
pub fn confirmed_or_mempool_tx(st: &AppState, id: &[u8; 32]) -> Option<Value> {
    if let Ok(Some(fk)) = st.query.tx_fk_by_txid(id) {
        return build_tx_json(&st.query, fk, st.network, None).ok();
    }
    mempool_tx_json(st, id)
}

#[cfg(unix)]
#[doc(hidden)]
pub fn mempool_tx_json(st: &AppState, id: &[u8; 32]) -> Option<Value> {
    let mp = st.mempool.as_ref()?;
    let tid = Txid::from_byte_array(*id);
    if let Some(e) = mp.mempool_tx_snapshot().get(&tid) {
        return entry_json(st, mp, e);
    }
    let tx = mp.get_tx(&tid)?;
    let fee = mp.get_live_meta(&tid).map(|(f, _)| f as i64);
    build_tx_json_from_tx(&st.query, &tx, st.network, fee, Some(mp)).ok()
}

#[cfg(unix)]
async fn post_tx_batch(
    st: AppState,
    body: Bytes,
    lookup: fn(&AppState, &[u8; 32]) -> Option<Value>,
) -> Response {
    if body.len() > st.max_body {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
    }
    spawn_join(move || {
        let ids = match parse_txid_array(&body) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let mut out = Vec::new();
        for id in ids {
            if let Some(v) = lookup(&st, &id) {
                out.push(v);
            }
        }
        json_reply_here(out)
    })
    .await
}

#[cfg(unix)]
pub async fn post_internal_txs(State(st): State<AppState>, body: Bytes) -> Response {
    post_tx_batch(st, body, confirmed_or_mempool_tx).await
}

#[cfg(unix)]
pub async fn post_internal_mempool_txs(State(st): State<AppState>, body: Bytes) -> Response {
    post_tx_batch(st, body, mempool_tx_json).await
}

#[cfg(unix)]
#[doc(hidden)]
pub fn mempool_tx_page(st: &AppState, last: Option<&Txid>, max: usize) -> Response {
    let Some(mp) = st.mempool.as_ref() else {
        return json_reply_here(Value::Array(Vec::new()));
    };
    let snap = mp.mempool_tx_snapshot();
    let parts: Vec<Option<&str>> = snap
        .page(last, max)
        .iter()
        .map(|e| entry_json_str(st, mp, e))
        .collect();
    let body = join_cached_objects(&parts);
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(unix)]
pub async fn get_internal_mempool_txs(
    State(st): State<AppState>,
    AxumQuery(q): AxumQuery<MaxTxs>,
) -> Response {
    let max = cap_max_txs(&q);
    spawn_join(move || mempool_tx_page(&st, None, max)).await
}

#[cfg(unix)]
pub async fn get_internal_mempool_txs_all(State(st): State<AppState>) -> Response {
    spawn_join(move || mempool_tx_page(&st, None, usize::MAX)).await
}

#[cfg(unix)]
pub async fn get_internal_mempool_txs_cursor(
    State(st): State<AppState>,
    Path(last): Path<String>,
    AxumQuery(q): AxumQuery<MaxTxs>,
) -> Response {
    let Ok(id) = parse_hash32(&last) else {
        return bad_request("unparseable txid");
    };
    let last = Txid::from_byte_array(id);
    let max = cap_max_txs(&q);
    spawn_join(move || mempool_tx_page(&st, Some(&last), max)).await
}

#[doc(hidden)]
pub fn mempool_txid_page(st: &AppState, last: Option<&Txid>, max: usize) -> Vec<String> {
    let Some(mp) = st.mempool.as_ref() else {
        return Vec::new();
    };
    mp.mempool_tx_snapshot()
        .page(last, max)
        .iter()
        .map(|e| block_hash_hex(&e.txid.to_byte_array()))
        .collect()
}

pub async fn get_mempool_txids_page(
    State(st): State<AppState>,
    AxumQuery(q): AxumQuery<MaxTxs>,
) -> Response {
    let max = cap_max_txs(&q);
    spawn_join(move || json_reply_here(mempool_txid_page(&st, None, max))).await
}

pub async fn get_mempool_txids_page_cursor(
    State(st): State<AppState>,
    Path(last): Path<String>,
    AxumQuery(q): AxumQuery<MaxTxs>,
) -> Response {
    let Ok(id) = parse_hash32(&last) else {
        return bad_request("unparseable txid");
    };
    let last = Txid::from_byte_array(id);
    let max = cap_max_txs(&q);
    spawn_join(move || json_reply_here(mempool_txid_page(&st, Some(&last), max))).await
}

#[cfg(unix)]
pub async fn get_internal_block_txs(
    State(st): State<AppState>,
    Path(hash_hex): Path<String>,
) -> Response {
    spawn_join(move || {
        let Ok(hash) = parse_hash32(&hash_hex) else {
            return not_found();
        };
        let Some((header_fk, rec)) = (match st.query.get_header_by_hash(&hash) {
            Ok(v) => v,
            Err(e) => return store_err(e),
        }) else {
            return not_found();
        };
        let fks = match st.query.header_tx_fks(header_fk, Some(&hash)) {
            Ok(Some(fks)) => fks,
            Ok(None) => return not_found(),
            Err(e) => return store_err(e),
        };
        let block = match st
            .query
            .reconstruct_archived_block_from_parts(rec, fks.clone())
        {
            Ok(b) => b,
            Err(e) => return store_err(e),
        };
        if block.txdata.len() != fks.len() {
            return store_err(StoreError::Corrupt(
                "invariant: reconstruct tx count != header_txs",
            ));
        }
        let mut out = Vec::with_capacity(fks.len());
        for (fk, tx) in fks.into_iter().zip(block.txdata.iter()) {
            let status = match tx_status_json(&st.query, fk) {
                Ok(s) => s,
                Err(e) => return store_err(e),
            };
            match build_tx_json_from_tx_with_status(&st.query, tx, st.network, status, None, None) {
                Ok(v) => out.push(v),
                Err(e) => return store_err(e),
            }
        }
        json_reply_here(out)
    })
    .await
}

/// `GET /address/:a` totals without `--max-sh-creates`, for the socket's
/// trusted caller: an explorer seeding its own running totals for a large
/// script once, pinned with `?asof=`. `--heavy-sh-creates` still takes turns.
/// No join client: the lifted join never lands in a slot a public route
/// could reuse.
#[cfg(unix)]
pub async fn get_internal_address(
    State(st): State<AppState>,
    Path(addr_s): Path<String>,
    AsOf(asof): AsOf,
) -> Response {
    let Ok(sh) = resolve_address_sh(&addr_s, st.network) else {
        return not_found();
    };
    spawn_join(move || {
        let _lift = rbitcoin_query::scripthash::lift_max_sh_creates();
        sh_info_reply(&st, &sh, Some(&addr_s), None, asof, None)
    })
    .await
}

/// `GET /scripthash/:h` totals without `--max-sh-creates`. See
/// [`get_internal_address`].
#[cfg(unix)]
pub async fn get_internal_scripthash(
    State(st): State<AppState>,
    Path(sh_hex): Path<String>,
    AsOf(asof): AsOf,
) -> Response {
    let Ok(sh) = parse_hash32(&sh_hex) else {
        return not_found();
    };
    spawn_join(move || {
        let _lift = rbitcoin_query::scripthash::lift_max_sh_creates();
        sh_info_reply(&st, &sh, None, Some(&sh_hex), asof, None)
    })
    .await
}

/// Most txids one `POST /internal/txs/outspends/by-txid` may name.
pub const MAX_OUTSPENDS_TXIDS: usize = 1_000;

/// Most outspend slots one bulk outspends request may produce, summed over
/// every txid (repeats included). Over it the request is a 400.
pub const MAX_OUTSPENDS_ENTRIES: u64 = 250_000;

/// One slot array per id, encoded slot by slot into the body. Output counts
/// are summed first, and the request is refused as soon as the sum passes
/// [`MAX_OUTSPENDS_ENTRIES`], before any slot is built.
pub fn outspends_for_txid_opts(st: &AppState, ids: Vec<Option<[u8; 32]>>) -> Response {
    let mp = st.mempool.as_deref();
    let mut nouts = Vec::with_capacity(ids.len());
    let mut total = 0u64;
    for id in &ids {
        let nout = match id {
            None => 0,
            Some(id) => match st.query.tx_fk_by_txid(id) {
                Ok(Some(fk)) => match confirmed_output_count(&st.query, fk) {
                    Ok(n) => n,
                    Err(e) => return store_err(e),
                },
                _ => mp
                    .and_then(|mp| mp.get_tx(&Txid::from_byte_array(*id)))
                    .map_or(0, |tx| tx.output.len() as u32),
            },
        };
        total += u64::from(nout);
        if total > MAX_OUTSPENDS_ENTRIES {
            return bad_request("Too many outputs requested");
        }
        nouts.push(nout);
    }
    let view = match pin_or_reject(&st.query, ChainViewKind::Tip, None) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut body = vec![b'['];
    for (i, (id, nout)) in ids.iter().zip(nouts).enumerate() {
        if i > 0 {
            body.push(b',');
        }
        // An unparseable id has `nout == 0`, so it writes `[]`.
        let id = id.as_ref().unwrap_or(&[0; 32]);
        if let Err(e) = write_outspends(&mut body, &st.query, mp, id, nout, view.as_ref()) {
            return store_err(e);
        }
    }
    body.push(b']');
    json_bytes_reply(body)
}

#[cfg(unix)]
pub fn outspends_for_txids(st: &AppState, ids: Vec<[u8; 32]>) -> Response {
    outspends_for_txid_opts(st, ids.into_iter().map(Some).collect())
}

#[cfg(unix)]
pub async fn post_outspends_by_txid(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > st.max_body {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
    }
    spawn_join(move || {
        let ids = match parse_txid_array(&body) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if ids.len() > MAX_OUTSPENDS_TXIDS {
            return bad_request("Too many txids requested");
        }
        outspends_for_txids(&st, ids)
    })
    .await
}

#[cfg(unix)]
pub async fn post_outspends_by_outpoint(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > st.max_body {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
    }
    spawn_join(move || {
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return bad_request("invalid json"),
        };
        let Some(arr) = v.as_array() else {
            return bad_request("body must be a JSON array of txid:vout");
        };
        let view = match pin_or_reject(&st.query, ChainViewKind::Tip, None) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let mp = st.mempool.as_deref();
        let mut out = Vec::with_capacity(arr.len());
        for x in arr {
            let Some(s) = x.as_str() else {
                out.push(json!({"spent": false}));
                continue;
            };
            let Some((tid_s, vout_s)) = s.rsplit_once(':') else {
                out.push(json!({"spent": false}));
                continue;
            };
            let Ok(txid) = parse_hash32(tid_s) else {
                out.push(json!({"spent": false}));
                continue;
            };
            let Ok(vout) = vout_s.parse::<u32>() else {
                out.push(json!({"spent": false}));
                continue;
            };
            match outspend_json(&st.query, mp, &txid, vout, view.as_ref()) {
                Ok(v) => out.push(v),
                Err(e) => return store_err(e),
            }
        }
        json_reply_here(out)
    })
    .await
}
