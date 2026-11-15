//! Frigate `blockchain.silentpayments.subscribe` (session-only scan key).

use bitcoin::bech32::{Bech32m, ByteIterExt, Fe32, Fe32IterExt, Hrp};
use bitcoin::secp256k1::{PublicKey, SecretKey};
use bitcoin::Network;
use rbitcoin_consensus::silent_payments::labeled_spend_key;
use rbitcoin_consensus::{taproot_matches_scan, ChainParams};
use rbitcoin_primitives::Height;
use rbitcoin_query::tweaks_at_height;
use rbitcoin_query::Query;
use serde_json::{json, Value};

#[doc(hidden)]
pub fn decode_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    crate::server::decode_hex_bytes(s)
}

/// Frigate, as nLockTime: a start above this is seconds since the epoch.
pub const SP_START_TIME_THRESHOLD: u32 = 500_000_000;
/// Bitcoin Core's `TIMESTAMP_WINDOW`: how far a block's time may trail the
/// clock that mined it. A timestamp start scans from this much earlier.
pub const SP_TIMESTAMP_WINDOW: u32 = 2 * 60 * 60;
/// Heights held under one scan permit before the next chunk.
pub const SP_SCAN_CHUNK: u32 = 256;
/// Process-wide historical scans. Not a knob.
pub const SP_SCAN_PERMITS: usize = 3;

#[derive(Clone)]
pub struct SpSub {
    pub scan: SecretKey,
    pub start: u32,
    /// Last height of a `"FROM-TO"` start. `None` scans to the tip.
    pub end: Option<u32>,
    pub labels: Vec<u32>,
    spend_keys: Vec<PublicKey>,
    pub address: String,
}

/// Parse a subscribe or unsubscribe. `block_time` is the header time at a
/// confirmed height, read only when the start is a timestamp.
pub fn parse_sub(
    params: &Value,
    network: Network,
    tip: Option<u32>,
    block_time: &dyn Fn(u32) -> Result<u32, String>,
) -> Result<SpSub, String> {
    let scan_hex = param_str(params, 0)?;
    let spend_hex = param_str(params, 1)?;
    let scan_bytes = decode_hex_bytes(scan_hex)?;
    if scan_bytes.len() != 32 {
        return Err("scan_private_key must be 32 bytes".into());
    }
    let scan = SecretKey::from_slice(&scan_bytes).map_err(|e| e.to_string())?;
    let spend_bytes = decode_hex_bytes(spend_hex)?;
    let spend = PublicKey::from_slice(&spend_bytes).map_err(|e| e.to_string())?;
    let (start, end) = match params.as_array().and_then(|a| a.get(2)) {
        Some(Value::String(s)) if s.contains('-') => {
            let (from, to) = height_range(s, tip)?;
            (from, Some(to))
        }
        None | Some(Value::Null) => (0, None),
        // Read as 0, a start that is not a height would scan from genesis.
        Some(other) => {
            let start = match other {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            };
            let start = start.ok_or(
                "start must be a block height, a Unix time, or a 'FROM-TO' range of heights",
            )?;
            (open_start(start, tip, block_time)?, None)
        }
    };
    let mut labels = vec![0u32];
    if let Some(arr) = params
        .as_array()
        .and_then(|a| a.get(3))
        .and_then(|v| v.as_array())
    {
        for v in arr {
            if let Some(n) = v.as_u64() {
                let n = n as u32;
                if !labels.contains(&n) {
                    labels.push(n);
                }
            }
        }
    }
    if labels.len() > 10 {
        return Err("too many silent payment labels".into());
    }
    let mut spend_keys = labels
        .iter()
        .map(|&label| labeled_spend_key(&scan, &spend, label))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid silent payment label key: {e}"))?;
    spend_keys.insert(0, spend);
    let address = encode_sp_address(network, &scan, &spend);
    Ok(SpSub {
        scan,
        start,
        end,
        labels,
        spend_keys,
        address,
    })
}

/// A start with no end: a height bounded by the tip, or a Unix time.
fn open_start(
    start: u64,
    tip: Option<u32>,
    block_time: &dyn Fn(u32) -> Result<u32, String>,
) -> Result<u32, String> {
    // Past the last second a header can carry is past every block.
    let start = u32::try_from(start).unwrap_or(u32::MAX);
    Ok(match tip {
        Some(tip) if start > SP_START_TIME_THRESHOLD => height_before_time(start, tip, block_time)?,
        Some(tip) => start.min(tip),
        None if start > SP_START_TIME_THRESHOLD => 0,
        None => start,
    })
}

/// Frigate `"FROM-TO"`: a closed range of confirmed heights.
fn height_range(range: &str, tip: Option<u32>) -> Result<(u32, u32), String> {
    let (from, to) = range.split_once('-').unwrap_or((range, ""));
    let (Ok(from), Ok(to)) = (from.parse::<u32>(), to.parse::<u32>()) else {
        return Err("start range must contain integer block heights".into());
    };
    if to < from {
        return Err("start range must satisfy 0 <= from <= to".into());
    }
    match tip {
        Some(tip) if to > tip => Err(format!("start range 'to' ({to}) exceeds tip ({tip})")),
        _ => Ok((from, to)),
    }
}

/// The height a timestamp start scans from: the last block whose header
/// time is at least [`SP_TIMESTAMP_WINDOW`] before `time`, or genesis when
/// there is none. Header times are not sorted, so the search lands within
/// their disorder, and the window is what covers it.
pub fn height_before_time(
    time: u32,
    tip: u32,
    block_time: &dyn Fn(u32) -> Result<u32, String>,
) -> Result<u32, String> {
    let target = time.saturating_sub(SP_TIMESTAMP_WINDOW);
    let (mut low, mut high, mut best) = (0u32, tip, 0u32);
    while low <= high {
        let mid = low + (high - low) / 2;
        if block_time(mid)? <= target {
            best = mid;
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    Ok(best)
}

pub fn subscribe_result(sub: &SpSub) -> Value {
    json!({
        "address": sub.address,
        "labels": sub.labels,
        "start_height": sub.start,
    })
}

pub fn scan_hits(
    query: &Query,
    chain: &ChainParams,
    sub: &SpSub,
    from: u32,
    to: u32,
) -> Result<Vec<Value>, String> {
    let mut hits = Vec::new();
    for h in from..=to {
        let map = tweaks_at_height(query, chain, Height(h)).map_err(|e| e.to_string())?;
        for (txid, tw) in map {
            if tx_matches(sub, &tw) {
                hits.push(json!({
                    "height": h,
                    "tx_hash": rbitcoin_primitives::display_hash_hex(txid),
                    "tweak_key": rbitcoin_primitives::hex_encode(tw.tweak),
                }));
            }
        }
    }
    Ok(hits)
}

#[doc(hidden)]
pub fn tx_matches(sub: &SpSub, tw: &rbitcoin_consensus::TxTweak) -> bool {
    taproot_matches_scan(&tw.tweak, &tw.output_pubkeys, &sub.scan, &sub.spend_keys)
}

#[doc(hidden)]
pub fn encode_sp_address(network: Network, scan_sk: &SecretKey, spend: &PublicKey) -> String {
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let scan_pk = PublicKey::from_secret_key(&secp, scan_sk);
    let hrp = match network {
        Network::Bitcoin => "sp",
        Network::Regtest | Network::Signet | Network::Testnet | Network::Testnet4 => "tsp",
    };
    let mut data = Vec::with_capacity(66);
    data.extend_from_slice(&scan_pk.serialize());
    data.extend_from_slice(&spend.serialize());
    // BIP-352: the data part starts with the version character (`q` is v0).
    data.iter()
        .copied()
        .bytes_to_fes()
        .with_checksum::<Bech32m>(&Hrp::parse_unchecked(hrp))
        .with_witness_version(Fe32::Q)
        .chars()
        .collect()
}

#[doc(hidden)]
pub fn param_str(params: &Value, i: usize) -> Result<&str, String> {
    crate::server::param_str(params, i)
}
