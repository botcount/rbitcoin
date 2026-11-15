use bitcoin::hashes::Hash;
use bitcoin::CompactTarget;
use rbitcoin_consensus::error::ConsensusError;
use rbitcoin_consensus::header::*;
use rbitcoin_consensus::params::{genesis_block, ChainParams};
use rbitcoin_primitives::Height;

const TWO_WEEKS: u32 = 14 * 24 * 60 * 60;
const PARENT_TIME: u32 = 1_794_632_593 + TWO_WEEKS;

/// The mined genesis header passes the height-0 rules: its hash, its
/// `nBits`, and proof of work under the raised pow limit.
#[test]
fn testnet5_genesis_header_is_valid() {
    let t5 = ChainParams::testnet5();
    let genesis = genesis_block(&t5).header;
    let hash = genesis.block_hash().to_byte_array();
    check_header(&t5, Height(0), &genesis, hash, None, &[], None, |_| None)
        .expect("testnet5 genesis");
    let mut other = genesis;
    other.nonce += 1;
    let err = check_header(
        &t5,
        Height(0),
        &other,
        other.block_hash().to_byte_array(),
        None,
        &[],
        None,
        |_| None,
    )
    .unwrap_err();
    assert!(
        matches!(err, ConsensusError::BadHeader("genesis hash mismatch")),
        "{err:?}"
    );
}

/// BIP 95: no block may use the minimum difficulty because its parent is
/// old. Testnet4 lets a block more than 20 minutes after its parent do so.
#[test]
fn testnet5_has_no_minimum_difficulty_exception() {
    let hard = CompactTarget::from_consensus(0x1903_a30c);
    let late = |params: &ChainParams| {
        next_work_bits(
            params,
            2017,
            hard,
            PARENT_TIME,
            PARENT_TIME + 20 * 60 + 1,
            None,
            |_| Some(hard),
        )
    };
    assert_eq!(late(&ChainParams::testnet5()), Some(hard));
    assert_eq!(
        late(&ChainParams::testnet4()),
        Some(CompactTarget::from_consensus(0x1d00_ffff)),
        "testnet4 keeps the exception"
    );
}

/// BIP 95: a retarget cannot go below a difficulty of about 1,000,000
/// (`0x1a0fffff`). A period four times too slow from that limit stays
/// there, and a period four times too fast quarters the target.
#[test]
fn testnet5_retarget_stops_at_the_raised_pow_limit() {
    let t5 = ChainParams::testnet5();
    let limit = CompactTarget::from_consensus(0x1a0f_ffff);
    let retarget = |timespan: u32| {
        next_work_bits(
            &t5,
            2016,
            limit,
            PARENT_TIME,
            PARENT_TIME + 600,
            Some(PeriodFirst {
                time: PARENT_TIME - timespan,
                bits: limit,
            }),
            |_| None,
        )
    };
    assert_eq!(retarget(4 * TWO_WEEKS), Some(limit));
    assert_eq!(retarget(TWO_WEEKS), Some(limit));
    assert_eq!(
        retarget(TWO_WEEKS / 4),
        Some(CompactTarget::from_consensus(0x1a03_ffff))
    );
}

/// A header easier than the raised limit is `bad-diffbits`, whatever its hash.
#[test]
fn testnet5_rejects_a_target_above_the_pow_limit() {
    let t5 = ChainParams::testnet5();
    let err = pow_hash_meets_target(
        [0; 32],
        CompactTarget::from_consensus(0x1d00_ffff),
        t5.pow_limit,
    )
    .unwrap_err();
    assert_eq!(
        rbitcoin_consensus::block_reject_reason(&err),
        "bad-diffbits"
    );
    pow_hash_meets_target(
        [0; 32],
        CompactTarget::from_consensus(0x1a0f_ffff),
        t5.pow_limit,
    )
    .expect("the pow limit itself");
}
