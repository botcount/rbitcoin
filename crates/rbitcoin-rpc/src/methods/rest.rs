//! Core REST on the same HTTP server as JSON-RPC.
//!
//! TCP `/rest/` skips Bearer. The unix socket is filesystem auth for every path.

use super::*;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::Hash;
use rbitcoin_primitives::Height;

#[doc(hidden)]
pub const MAX_REST_HEADERS: u32 = 2000;
#[doc(hidden)]
pub const MAX_GETUTXOS: usize = 15;
/// Core `MEMPOOL_HEIGHT`.
#[doc(hidden)]
pub const MEMPOOL_HEIGHT: u32 = 0x7fff_ffff;

#[derive(Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum RestFmt {
    Bin,
    Hex,
    Json,
}

/// Body encoding after JSON has already returned.
#[doc(hidden)]
pub enum RestWire {
    Bin,
    Hex,
}

#[doc(hidden)]
pub fn rest_wire(fmt: RestFmt) -> Option<RestWire> {
    match fmt {
        RestFmt::Bin => Some(RestWire::Bin),
        RestFmt::Hex => Some(RestWire::Hex),
        RestFmt::Json => None,
    }
}

pub struct RestReply {
    pub status: axum::http::StatusCode,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

pub fn dispatch_rest(ctx: &RpcContext, path: &str, query: &str, _body: &[u8]) -> RestReply {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path
        .strip_prefix("/rest/")
        .or_else(|| path.strip_prefix("/rest"))
        .unwrap_or(path);
    let rest = rest.trim_start_matches('/');
    let Some((fmt, param)) = split_format(rest) else {
        return err(
            axum::http::StatusCode::NOT_FOUND,
            "output format not found (available: .bin, .hex, .json)",
        );
    };
    match route(ctx, fmt, param, query) {
        Ok(r) => r,
        Err(r) => r,
    }
}

#[doc(hidden)]
pub fn route(
    ctx: &RpcContext,
    fmt: RestFmt,
    param: &str,
    query: &str,
) -> Result<RestReply, RestReply> {
    if param == "chaininfo" {
        return json_only(fmt, getblockchaininfo(ctx));
    }
    if param == "deploymentinfo" || param.starts_with("deploymentinfo/") {
        let hash = param.strip_prefix("deploymentinfo/").unwrap_or("");
        let params = if hash.is_empty() {
            RpcParams::empty()
        } else {
            RpcParams::positional(vec![json!(hash)])
        };
        return json_only(fmt, getdeploymentinfo(ctx, &params));
    }
    if let Some(which) = param.strip_prefix("mempool/") {
        if fmt != RestFmt::Json {
            return Err(err(
                axum::http::StatusCode::NOT_FOUND,
                "output format not found (available: json)",
            ));
        }
        if which == "info" {
            return Ok(json_body(getmempoolinfo(ctx)?));
        }
        if which == "contents" {
            return Ok(json_body(mempool_contents(ctx, query)?));
        }
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid URI format. Expected /rest/mempool/<info|contents>.json",
        ));
    }
    if let Some(height) = param.strip_prefix("blockhashbyheight/") {
        return blockhash_by_height(ctx, fmt, height);
    }
    if let Some(rest) = param.strip_prefix("headers/") {
        return headers(ctx, fmt, rest);
    }
    if let Some(hash) = param.strip_prefix("block/notxdetails/") {
        return block(ctx, fmt, hash, 1);
    }
    if let Some(hash) = param.strip_prefix("block/") {
        return block(ctx, fmt, hash, 2);
    }
    if let Some(txid) = param.strip_prefix("tx/") {
        return tx(ctx, fmt, txid);
    }
    if param == "getutxos" || param.starts_with("getutxos/") {
        return getutxos(ctx, fmt, param);
    }
    if let Some(rest) = param.strip_prefix("blockfilter/") {
        return blockfilter(ctx, fmt, rest);
    }
    Err(err(axum::http::StatusCode::NOT_FOUND, "not found"))
}

#[doc(hidden)]
pub fn blockhash_by_height(
    ctx: &RpcContext,
    fmt: RestFmt,
    height: &str,
) -> Result<RestReply, RestReply> {
    let h: u32 = height.parse().map_err(|_| {
        err(
            axum::http::StatusCode::BAD_REQUEST,
            &format!("Invalid height: {height}"),
        )
    })?;
    let tip = ctx.query.tip_height().map(|t| t.0).unwrap_or(0);
    if h > tip {
        return Err(err(
            axum::http::StatusCode::NOT_FOUND,
            "Block height out of range",
        ));
    }
    let (_, rec) = ctx
        .query
        .header_at_height(Height(h))
        .map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?
        .ok_or_else(|| {
            err(
                axum::http::StatusCode::NOT_FOUND,
                "Block height out of range",
            )
        })?;
    let display = hash_hex_display(&rec.hash);
    match fmt {
        RestFmt::Bin => Ok(bin_body(rec.hash.to_vec())),
        RestFmt::Hex => Ok(hex_body(&display)),
        RestFmt::Json => Ok(json_body(json!({ "blockhash": display }))),
    }
}

#[doc(hidden)]
pub fn headers(ctx: &RpcContext, fmt: RestFmt, rest: &str) -> Result<RestReply, RestReply> {
    let (count_s, hash) = rest.split_once('/').ok_or_else(|| {
        err(
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid URI format. Expected /rest/headers/<count>/<hash>",
        )
    })?;
    let count: u32 = count_s.parse().unwrap_or(0);
    if !(1..=MAX_REST_HEADERS).contains(&count) {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            &format!("Header count is invalid or out of acceptable range (1-{MAX_REST_HEADERS}): {count_s}"),
        ));
    }
    let start = parse_hash(hash)?;
    let start_h = ctx
        .query
        .height_of_hash(&start)
        .map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?
        .ok_or_else(|| {
            err(
                axum::http::StatusCode::BAD_REQUEST,
                &format!("Invalid hash: {hash}"),
            )
        })?;
    let tip = ctx.query.tip_height().map(|t| t.0).unwrap_or(0);
    let last = start_h.0.saturating_add(count - 1).min(tip);
    let mut raw = Vec::new();
    let mut json_headers = Vec::new();
    for h in start_h.0..=last {
        let hdr = ctx.query.wire_header_at_height(Height(h)).map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?;
        if fmt != RestFmt::Json {
            hdr.consensus_encode(&mut raw).map_err(|_| {
                err(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "header encode",
                )
            })?;
        } else {
            let display = hash_hex_display(&hdr.block_hash().to_byte_array());
            let params = RpcParams::positional(vec![json!(display), json!(true)]);
            json_headers.push(getblockheader(ctx, &params)?);
        }
    }
    match fmt {
        RestFmt::Bin => Ok(bin_body(raw)),
        RestFmt::Hex => Ok(hex_body(&rbitcoin_primitives::hex_encode(&raw))),
        RestFmt::Json => Ok(json_body(json!(json_headers))),
    }
}

#[doc(hidden)]
pub fn block(
    ctx: &RpcContext,
    fmt: RestFmt,
    hash: &str,
    verbosity: u32,
) -> Result<RestReply, RestReply> {
    let parsed = parse_hash(hash)?;
    let Some(wire) = rest_wire(fmt) else {
        let params = RpcParams::positional(vec![json!(hash), json!(verbosity)]);
        return Ok(json_body(getblock(ctx, &params)?));
    };
    let height = ctx
        .query
        .height_of_hash(&parsed)
        .map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?
        .ok_or_else(|| {
            err(
                axum::http::StatusCode::NOT_FOUND,
                &format!("{hash} not found"),
            )
        })?;
    let blk = ctx
        .query
        .reconstruct_block_at_height(height)
        .map_err(|e| err(axum::http::StatusCode::NOT_FOUND, &e.to_string()))?;
    let mut raw = Vec::new();
    blk.consensus_encode(&mut raw).map_err(|_| {
        err(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "block encode",
        )
    })?;
    match wire {
        RestWire::Bin => Ok(bin_body(raw)),
        RestWire::Hex => Ok(hex_body(&rbitcoin_primitives::hex_encode(&raw))),
    }
}

#[doc(hidden)]
pub fn tx(ctx: &RpcContext, fmt: RestFmt, txid: &str) -> Result<RestReply, RestReply> {
    let want = parse_hash(txid)?;
    if want == genesis_coinbase_txid(ctx) {
        return Err(err(
            axum::http::StatusCode::NOT_FOUND,
            &format!("{txid} not found"),
        ));
    }
    let Some(wire) = rest_wire(fmt) else {
        let params = RpcParams::positional(vec![json!(txid), json!(true)]);
        return Ok(json_body(map_tx_missing(
            txid,
            getrawtransaction(ctx, &params),
        )?));
    };
    let Some(hit) = locate_raw_tx(ctx, want).map_err(rpc_to_rest)? else {
        return Err(err(
            axum::http::StatusCode::NOT_FOUND,
            &format!("{txid} not found"),
        ));
    };
    let tx = hit.tx();
    match wire {
        RestWire::Bin => Ok(bin_body(bitcoin::consensus::serialize(tx))),
        RestWire::Hex => Ok(hex_body(&serialize_hex(tx))),
    }
}

#[doc(hidden)]
pub fn getutxos(ctx: &RpcContext, fmt: RestFmt, param: &str) -> Result<RestReply, RestReply> {
    if fmt != RestFmt::Json {
        return Err(err(
            axum::http::StatusCode::NOT_FOUND,
            "output format not found (available: json)",
        ));
    }
    let rest = param.strip_prefix("getutxos").unwrap_or("");
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            "Error: empty request",
        ));
    }
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let mut check_mempool = false;
    let mut specs = Vec::new();
    if let Some(first) = parts.next() {
        if first == "checkmempool" {
            check_mempool = true;
        } else {
            specs.push(first);
        }
    }
    specs.extend(parts);
    if specs.is_empty() {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            "Error: empty request",
        ));
    }
    if specs.len() > MAX_GETUTXOS {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            &format!(
                "Error: max outpoints exceeded (max: {MAX_GETUTXOS}, tried: {})",
                specs.len()
            ),
        ));
    }
    let (tip_hash, tip_height) = tip_hash_height(ctx)?;
    let mut bitmap = String::new();
    let mut utxos = Vec::new();
    for spec in specs {
        let (txid, n) = spec
            .rsplit_once('-')
            .ok_or_else(|| err(axum::http::StatusCode::BAD_REQUEST, "Parse error"))?;
        let n: u32 = n
            .parse()
            .map_err(|_| err(axum::http::StatusCode::BAD_REQUEST, "Parse error"))?;
        let _ = parse_hash(txid)?;
        let mut named = serde_json::Map::new();
        named.insert("txid".into(), json!(txid));
        named.insert("n".into(), json!(n));
        named.insert("include_mempool".into(), json!(check_mempool));
        let coin = gettxout(ctx, &RpcParams::named(named))?;
        if coin.is_null() {
            bitmap.push('0');
            continue;
        }
        bitmap.push('1');
        let confs = coin
            .get("confirmations")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let height = if confs == 0 {
            MEMPOOL_HEIGHT
        } else {
            tip_height.saturating_add(1).saturating_sub(confs)
        };
        utxos.push(json!({
            "height": height,
            "value": coin.get("value").cloned().unwrap_or(Value::Null),
            "scriptPubKey": coin.get("scriptPubKey").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(json_body(json!({
        "chainHeight": tip_height,
        "chaintipHash": tip_hash,
        "bitmap": bitmap,
        "utxos": utxos,
    })))
}

#[doc(hidden)]
pub fn blockfilter(ctx: &RpcContext, fmt: RestFmt, rest: &str) -> Result<RestReply, RestReply> {
    let (filtertype, hash) = rest.split_once('/').ok_or_else(|| {
        err(
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid URI format. Expected /rest/blockfilter/<filtertype>/<blockhash>",
        )
    })?;
    if filtertype != "basic" {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            &format!("Unknown filtertype {filtertype}"),
        ));
    }
    if !ctx.query.block_filter_enabled() {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            "Index is not enabled for filtertype basic",
        ));
    }
    let parsed = parse_hash(hash)?;
    let height = ctx
        .query
        .height_of_hash(&parsed)
        .map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?
        .ok_or_else(|| {
            err(
                axum::http::StatusCode::NOT_FOUND,
                &format!("{hash} not found"),
            )
        })?;
    let (body, _header) = ctx
        .query
        .basic_filter_at(height.0)
        .map_err(|e| {
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            )
        })?
        .ok_or_else(|| {
            err(
                axum::http::StatusCode::NOT_FOUND,
                "Filter not found. Block filters are still in the process of being indexed.",
            )
        })?;
    match fmt {
        RestFmt::Json => Ok(json_body(
            json!({ "filter": rbitcoin_primitives::hex_encode(&body) }),
        )),
        RestFmt::Bin | RestFmt::Hex => {
            let raw = serialize_rest_filter(&parsed, &body);
            if fmt == RestFmt::Bin {
                Ok(bin_body(raw))
            } else {
                Ok(hex_body(&rbitcoin_primitives::hex_encode(&raw)))
            }
        }
    }
}

/// Core `BlockFilter` wire: type byte, internal block hash, compact-size filter.
#[doc(hidden)]
pub fn serialize_rest_filter(hash: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 32 + body.len() + 3);
    out.push(0);
    out.extend_from_slice(hash);
    let _ = bitcoin::consensus::encode::VarInt(body.len() as u64).consensus_encode(&mut out);
    out.extend_from_slice(body);
    out
}

#[doc(hidden)]
pub fn mempool_contents(ctx: &RpcContext, query: &str) -> Result<Value, RestReply> {
    let verbose = query_flag(query, "verbose", true)?;
    let seq = query_flag(query, "mempool_sequence", false)?;
    if verbose && seq {
        return Err(err(
            axum::http::StatusCode::BAD_REQUEST,
            "Verbose results cannot contain mempool sequence values. (hint: set \"verbose=false\")",
        ));
    }
    let mut named = serde_json::Map::new();
    named.insert("verbose".into(), json!(verbose));
    named.insert("mempool_sequence".into(), json!(seq));
    getrawmempool(ctx, &RpcParams::named(named)).map_err(rpc_to_rest)
}

#[doc(hidden)]
pub fn query_flag(query: &str, name: &str, default: bool) -> Result<bool, RestReply> {
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k != name {
            continue;
        }
        return match v {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(err(
                axum::http::StatusCode::BAD_REQUEST,
                &format!("The \"{name}\" query parameter must be either \"true\" or \"false\"."),
            )),
        };
    }
    Ok(default)
}

#[doc(hidden)]
pub fn map_tx_missing(txid: &str, r: Result<Value, Value>) -> Result<Value, RestReply> {
    r.map_err(|e| tx_missing_reply(txid, e))
}

fn tx_missing_reply(txid: &str, e: Value) -> RestReply {
    let msg = rpc_message(&e);
    if msg.contains("No such mempool") || msg.contains("not found") {
        err(
            axum::http::StatusCode::NOT_FOUND,
            &format!("{txid} not found"),
        )
    } else {
        rpc_to_rest(e)
    }
}

#[doc(hidden)]
pub fn json_only(fmt: RestFmt, v: Result<Value, Value>) -> Result<RestReply, RestReply> {
    if fmt != RestFmt::Json {
        return Err(err(
            axum::http::StatusCode::NOT_FOUND,
            "output format not found (available: json)",
        ));
    }
    Ok(json_body(v?))
}

#[doc(hidden)]
pub fn parse_hash(hex: &str) -> Result<[u8; 32], RestReply> {
    parse_hash32_display(hex).map_err(|_| {
        err(
            axum::http::StatusCode::BAD_REQUEST,
            &format!("Invalid hash: {hex}"),
        )
    })
}

#[doc(hidden)]
pub fn split_format(path: &str) -> Option<(RestFmt, &str)> {
    let (param, suffix) = path.rsplit_once('.')?;
    let fmt = match suffix {
        "bin" => RestFmt::Bin,
        "hex" => RestFmt::Hex,
        "json" => RestFmt::Json,
        _ => return None,
    };
    Some((fmt, param))
}

#[doc(hidden)]
pub fn json_body(v: Value) -> RestReply {
    let mut s = v.to_string();
    s.push('\n');
    RestReply {
        status: axum::http::StatusCode::OK,
        content_type: "application/json",
        body: s.into_bytes(),
    }
}

#[doc(hidden)]
pub fn hex_body(hex: &str) -> RestReply {
    let mut s = hex.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    RestReply {
        status: axum::http::StatusCode::OK,
        content_type: "text/plain",
        body: s.into_bytes(),
    }
}

#[doc(hidden)]
pub fn bin_body(raw: Vec<u8>) -> RestReply {
    RestReply {
        status: axum::http::StatusCode::OK,
        content_type: "application/octet-stream",
        body: raw,
    }
}

#[doc(hidden)]
pub fn err(status: axum::http::StatusCode, msg: &str) -> RestReply {
    RestReply {
        status,
        content_type: "text/plain",
        body: format!("{msg}\n").into_bytes(),
    }
}

#[doc(hidden)]
pub fn rpc_message(err: &Value) -> String {
    err.get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("error")
        .to_string()
}

#[doc(hidden)]
pub fn rpc_to_rest(v: Value) -> RestReply {
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(ERR_MISC);
    let msg = rpc_message(&v);
    let status = if code == ERR_INVALID_ADDRESS_OR_KEY || msg.contains("not found") {
        axum::http::StatusCode::NOT_FOUND
    } else if code == ERR_INVALID_PARAMETER || code == ERR_INVALID_PARAMS || code == ERR_TYPE_ERROR
    {
        axum::http::StatusCode::BAD_REQUEST
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    };
    err(status, &msg)
}

impl From<Value> for RestReply {
    fn from(v: Value) -> Self {
        rpc_to_rest(v)
    }
}
