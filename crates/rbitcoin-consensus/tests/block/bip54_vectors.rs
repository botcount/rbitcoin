//! The BIP 54 test vectors (`bip-0054/test_vectors` in bitcoin/bips, CC0).
//!
//! They are mainnet headers, blocks, and transactions, meant to be checked
//! with the cleanup on from genesis. Mainnet parameters with
//! `enforce_bip54` set are that chain.
use bitcoin::block::{Block, Header};
use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::hashes::Hash;
use bitcoin::{Transaction, TxOut};
use rbitcoin_consensus::block::{
    reject_bip54_sigops, reject_bip54_tx_size, validate_block_structure, SpendSigopRules,
    ValidationContext,
};
use rbitcoin_consensus::error::ConsensusError;
use rbitcoin_consensus::header::{check_header, ParentHeader, PeriodFirst};
use rbitcoin_consensus::milestone::Milestone;
use rbitcoin_consensus::params::ChainParams;
use rbitcoin_primitives::Height;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

fn vectors(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bip54")
        .join(name);
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("fixture {path:?}: {e}"));
    let mut bytes = Vec::new();
    if name.ends_with(".zst") {
        ruzstd::decoding::StreamingDecoder::new(file)
            .unwrap_or_else(|e| panic!("zstd {path:?}: {e}"))
            .read_to_end(&mut bytes)
            .unwrap_or_else(|e| panic!("zstd read {path:?}: {e}"));
    } else {
        let mut file = file;
        file.read_to_end(&mut bytes)
            .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    }
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("json {path:?}: {e}"))
}

fn mainnet_with_bip54() -> ChainParams {
    let mut params = ChainParams::mainnet();
    params.enforce_bip54 = true;
    params
}

fn hex<T: bitcoin::consensus::Decodable>(v: &Value) -> T {
    deserialize_hex(v.as_str().expect("hex string")).expect("consensus hex")
}

fn rows(v: &Value) -> &[Value] {
    v.as_array().expect("array")
}

fn verdict(case: &Value) -> (bool, &str) {
    (
        case["valid"].as_bool().expect("valid"),
        case["comment"].as_str().expect("comment"),
    )
}

/// Check `new` on top of `chain` (genesis first) the way confirm does.
fn extend(
    params: &ChainParams,
    chain: &mut Vec<Header>,
    new: &[Header],
) -> Result<(), ConsensusError> {
    for header in new {
        let height = chain.len() as u32;
        let parent = chain.last().map(|p| ParentHeader {
            hash: p.block_hash().to_byte_array(),
            time: p.time,
            bits: p.bits,
        });
        let times: Vec<u32> = chain[chain.len().saturating_sub(11)..]
            .iter()
            .map(|h| h.time)
            .collect();
        let first = params
            .period_first_height(height)
            .map(|h| PeriodFirst::of(&chain[h as usize]));
        check_header(
            params,
            Height(height),
            header,
            header.block_hash().to_byte_array(),
            parent,
            &times,
            first,
            |h| chain.get(h as usize).map(|x| x.bits),
        )?;
        chain.push(*header);
    }
    Ok(())
}

/// One node of `timestamps.json`: a run of headers, then either alternative
/// continuations or the verdict for the whole chain. Returns the leaf count.
fn walk_timestamps(
    params: &ChainParams,
    node: &Value,
    mut chain: Vec<Header>,
    plain: &ChainParams,
) -> usize {
    let headers: Vec<Header> = rows(&node["block_headers"]).iter().map(hex).collect();
    let mut on_mainnet = chain.clone();
    extend(plain, &mut on_mainnet, &headers).expect("every chain is valid without BIP 54");
    let checked = extend(params, &mut chain, &headers);
    let Some(extensions) = node.get("extensions") else {
        let (valid, comment) = verdict(node);
        match &checked {
            Ok(()) => assert!(valid, "accepted: {comment}"),
            Err(err) => {
                assert!(!valid, "rejected with {err:?}: {comment}");
                let reason = rbitcoin_consensus::block_reject_reason(err);
                assert!(
                    reason == "time-timewarp-attack" || reason == "time-negative-interval",
                    "{reason}: {comment}"
                );
            }
        }
        return 1;
    };
    checked.expect("a shared prefix is valid");
    rows(extensions)
        .iter()
        .map(|next| walk_timestamps(params, next, chain.clone(), plain))
        .sum()
}

/// `timestamps.json`: mainnet header chains from genesis across two
/// retargets, with the timewarp and Murch-Zawy bounds at their edges.
#[test]
fn bip54_timestamp_vectors() {
    let leaves = walk_timestamps(
        &mainnet_with_bip54(),
        &vectors("timestamps.json.zst"),
        Vec::new(),
        &ChainParams::mainnet(),
    );
    assert_eq!(leaves, 9);
}

/// `coinbases.json`: five mainnet blocks from genesis; the last one's
/// coinbase carries the `nLockTime` and `nSequence` under test.
#[test]
fn bip54_coinbase_vectors() {
    let params = mainnet_with_bip54();
    let cases = vectors("coinbases.json");
    assert_eq!(rows(&cases).len(), 7);
    for case in rows(&cases) {
        let (valid, comment) = verdict(case);
        let blocks: Vec<Block> = rows(&case["block_chain"]).iter().map(hex).collect();
        assert_eq!(blocks.len(), 5, "{comment}");
        let checked = blocks.iter().enumerate().try_for_each(|(height, block)| {
            let ctx = ValidationContext::at(&params, Height(height as u32), Milestone::NONE);
            validate_block_structure(block, &ctx).map_err(|err| (height, err))
        });
        match checked {
            Ok(()) => assert!(valid, "accepted: {comment}"),
            Err((height, err)) => {
                assert!(!valid, "rejected with {err:?}: {comment}");
                assert_eq!(height, 4, "{comment}");
                let reason = rbitcoin_consensus::block_reject_reason(&err);
                assert!(
                    reason == "bad-cb-locktime" || reason == "bad-cb-sequence",
                    "{reason}: {comment}"
                );
            }
        }
    }
}

/// `txsize.json`: transactions whose witness-stripped size is, or is next
/// to, 64 bytes.
#[test]
fn bip54_tx_size_vectors() {
    let cases = vectors("txsize.json");
    assert_eq!(rows(&cases).len(), 13);
    for case in rows(&cases) {
        let (valid, comment) = verdict(case);
        let tx: Transaction = hex(&case["tx"]);
        let checked = reject_bip54_tx_size(tx.base_size());
        assert_eq!(checked.is_ok(), valid, "{comment}");
        if let Err(err) = checked {
            assert_eq!(
                rbitcoin_consensus::block_reject_reason(&err),
                "bad-txns-size",
                "{comment}"
            );
        }
    }
}

/// `sigops.json`: transactions at and one past the 2,500 limit, by input
/// type, with the outputs they spend.
#[test]
fn bip54_sigops_vectors() {
    let cases = vectors("sigops.json.zst");
    assert_eq!(rows(&cases).len(), 39);
    for case in rows(&cases) {
        let (valid, comment) = verdict(case);
        let tx: Transaction = hex(&case["tx"]);
        let spent: Vec<TxOut> = rows(&case["spent_outputs"]).iter().map(hex).collect();
        let spks: Vec<&[u8]> = spent.iter().map(|o| o.script_pubkey.as_bytes()).collect();
        let checked = reject_bip54_sigops(&tx, &spks);
        assert_eq!(checked.is_ok(), valid, "{comment}");
        if let Err(err) = checked {
            assert_eq!(
                rbitcoin_consensus::block_reject_reason(&err),
                "bad-txns-legacy-sigops",
                "{comment}"
            );
        }
    }
}

/// Without `enforce_bip54` none of it is a rule. Every coinbase vector passes
/// the block checks, a block may hold a 64-byte transaction, and a spent
/// prevout adds nothing to a BIP 54 count.
#[test]
fn bip54_rules_are_off_without_the_switch() {
    let plain = ChainParams::mainnet();
    let cleanup = mainnet_with_bip54();
    let structure = |params: &ChainParams, block: &Block, height: u32| {
        validate_block_structure(
            block,
            &ValidationContext::at(params, Height(height), Milestone::NONE),
        )
    };
    let coinbases = vectors("coinbases.json");
    for case in rows(&coinbases) {
        let blocks: Vec<Block> = rows(&case["block_chain"]).iter().map(hex).collect();
        for (height, block) in blocks.iter().enumerate() {
            structure(&plain, block, height as u32).expect("no coinbase rule");
        }
    }

    let mut block: Block = hex(&rows(&coinbases)[0]["block_chain"][1]);
    let small = rows(&vectors("txsize.json"))
        .iter()
        .map(|case| hex::<Transaction>(&case["tx"]))
        .find(|tx| tx.base_size() == 64 && tx.input.iter().all(|i| i.witness.is_empty()))
        .expect("a 64-byte transaction without a witness");
    block.txdata.push(small);
    block.header.merkle_root = block.compute_merkle_root().expect("merkle root");
    structure(&plain, &block, 1).expect("no size rule");
    let err = structure(&cleanup, &block, 1).unwrap_err();
    assert_eq!(
        rbitcoin_consensus::block_reject_reason(&err),
        "bad-txns-size"
    );

    let sigops = vectors("sigops.json.zst");
    let case = &rows(&sigops)[0];
    let tx: Transaction = hex(&case["tx"]);
    let spent: Vec<TxOut> = rows(&case["spent_outputs"]).iter().map(hex).collect();
    let total = |bip54: bool| -> u64 {
        let rules = SpendSigopRules {
            bip16: true,
            witness: true,
            bip54,
        };
        tx.input
            .iter()
            .zip(&spent)
            .map(|(inp, out)| rules.count(inp, out.script_pubkey.as_bytes()).bip54)
            .sum()
    };
    assert_eq!(total(true), 2500);
    assert_eq!(total(false), 0);
}
