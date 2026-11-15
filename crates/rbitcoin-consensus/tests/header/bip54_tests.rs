use super::bip94_tests::{cheap, header_at, is_timewarp, on_parent_with, PARENT_HASH, PARENT_TIME};
use bitcoin::hashes::Hash;
use rbitcoin_consensus::error::ConsensusError;
use rbitcoin_consensus::header::*;
use rbitcoin_consensus::params::ChainParams;
use rbitcoin_primitives::Height;

fn first_at(params: &ChainParams, time: u32) -> Option<PeriodFirst> {
    Some(PeriodFirst {
        time,
        bits: params.pow_limit.to_compact_lossy(),
    })
}

fn is_negative_interval(err: &ConsensusError) -> bool {
    matches!(err, ConsensusError::BadHeader("time-negative-interval"))
        && rbitcoin_consensus::block_reject_reason(err) == "time-negative-interval"
}

/// BIP 54 is on for Testnet 5 only.
#[test]
fn bip54_is_enforced_on_testnet5_only() {
    assert!(ChainParams::testnet5().enforce_bip54);
    for other in [
        ChainParams::mainnet(),
        ChainParams::testnet(),
        ChainParams::testnet4(),
        ChainParams::signet(),
        ChainParams::regtest(),
    ] {
        assert!(!other.enforce_bip54, "{:?}", other.network);
    }
}

/// The header a period-edge rule reads. Every retargeting network reads the
/// ended period's first header at a retarget height. With BIP 54 the last
/// height of a period reads the first header of its own period.
#[test]
fn period_first_height_names_the_header_each_rule_reads() {
    let mainnet = ChainParams::mainnet();
    assert_eq!(mainnet.period_first_height(2016), Some(0));
    assert_eq!(mainnet.period_first_height(4032), Some(2016));
    for height in [0, 1, 2015, 2017, 4031] {
        assert_eq!(mainnet.period_first_height(height), None, "{height}");
    }
    let t5 = ChainParams::testnet5();
    assert_eq!(t5.period_first_height(2015), Some(0));
    assert_eq!(t5.period_first_height(2016), Some(0));
    assert_eq!(t5.period_first_height(4031), Some(2016));
    assert_eq!(t5.period_first_height(4032), Some(2016));
    for height in [0, 1, 2014, 2017, 4030] {
        assert_eq!(t5.period_first_height(height), None, "{height}");
    }
    let regtest = ChainParams::regtest();
    assert_eq!(
        regtest.period_first_height(2016),
        None,
        "no retargeting, no BIP 54"
    );
    assert_eq!(regtest.period_first_height(2015), None);
}

/// Core `MAX_TIMEWARP_BIP54`: a period's first header is at most two hours
/// before its parent. Testnet4 keeps BIP94's 600 seconds.
#[test]
fn timewarp_floor_is_two_hours_before_the_parent_with_bip54() {
    let t5 = cheap(ChainParams::testnet5());
    assert_eq!(t5.max_timewarp(), Some(7200));
    on_parent_with(&t5, 2016, PARENT_TIME - 7200, None).expect("two hours before the parent");
    let err = on_parent_with(&t5, 4032, PARENT_TIME - 7201, None).unwrap_err();
    assert!(is_timewarp(&err), "{err:?}");
    on_parent_with(&t5, 2017, PARENT_TIME - 7201, first_at(&t5, 0))
        .expect("only a period's first header");
    assert_eq!(ChainParams::testnet4().max_timewarp(), Some(600));
    assert_eq!(ChainParams::mainnet().max_timewarp(), None);
}

/// BIP 54: the last header of a period is not before the period's first
/// header (`time-negative-interval`). Other heights and other networks have
/// no such rule.
#[test]
fn period_last_header_is_not_before_the_period_first_with_bip54() {
    let t5 = cheap(ChainParams::testnet5());
    let first_time = PARENT_TIME + 100;
    let first = first_at(&t5, first_time);
    on_parent_with(&t5, 2015, first_time, first).expect("the first header's own time");
    for height in [2015, 4031] {
        let err = on_parent_with(&t5, height, first_time - 1, first).unwrap_err();
        assert!(is_negative_interval(&err), "{height}: {err:?}");
    }
    on_parent_with(&t5, 2014, first_time - 1, first).expect("only a period's last header");
    let err = on_parent_with(&t5, 2015, first_time, None).unwrap_err();
    assert!(
        matches!(
            err,
            ConsensusError::BadHeader("missing retarget first header")
        ),
        "{err:?}"
    );
    let t4 = cheap(ChainParams::testnet4());
    on_parent_with(&t4, 2015, first_time - 1, first_at(&t4, first_time))
        .expect("testnet4 has no such rule");
    on_parent_with(&t4, 2015, first_time - 1, None).expect("and reads no first header");
}

/// The confirm path (`check_header`) applies both period-edge rules.
#[test]
fn check_header_applies_both_bip54_time_rules() {
    let t5 = cheap(ChainParams::testnet5());
    let limit = t5.pow_limit.to_compact_lossy();
    let check = |height: u32, time: u32, first_time: u32| {
        let h = header_at(time);
        rbitcoin_consensus::clock::with_now(u64::from(PARENT_TIME), || {
            check_header(
                &t5,
                Height(height),
                &h,
                h.block_hash().to_byte_array(),
                Some(ParentHeader {
                    hash: PARENT_HASH,
                    time: PARENT_TIME,
                    bits: limit,
                }),
                &[PARENT_TIME - 10_000; 11],
                first_at(&t5, first_time),
                |_| Some(limit),
            )
        })
    };
    let two_weeks_ago = PARENT_TIME - 1_209_600;
    let err = check(2016, PARENT_TIME - 7201, two_weeks_ago).unwrap_err();
    assert!(is_timewarp(&err), "{err:?}");
    let err = check(2015, PARENT_TIME - 1, PARENT_TIME).unwrap_err();
    assert!(is_negative_interval(&err), "{err:?}");
    check(2015, PARENT_TIME, PARENT_TIME).expect("the first header's own time");
}
