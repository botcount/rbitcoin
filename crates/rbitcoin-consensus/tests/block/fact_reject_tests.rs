use rbitcoin_consensus::block::{
    fee_with_waived_coinbase, reject_bip30_unspent_prior, reject_one_create,
    require_retarget_first, waived_output_residue,
};
use rbitcoin_consensus::error::ConsensusError;
use rbitcoin_consensus::header::PeriodFirst;

#[test]
fn bip30_and_immature_coinbase_reject_from_facts() {
    assert!(reject_bip30_unspent_prior(2, 2, true).is_ok());
    let bip30 = reject_bip30_unspent_prior(5, 1, true).unwrap_err();
    assert!(
        matches!(bip30, ConsensusError::BadTx(s) if s.contains("BIP30")),
        "{bip30:?}"
    );
    assert!(reject_bip30_unspent_prior(5, 1, false).is_ok());

    let missing = reject_one_create(10, 100, None, false).unwrap_err();
    assert!(
        matches!(missing, ConsensusError::BadTx(s) if s.contains("missingorspent")),
        "{missing:?}"
    );
    let forward = reject_one_create(10, 100, Some(11), false).unwrap_err();
    assert!(
        matches!(forward, ConsensusError::MissingPrevout),
        "{forward:?}"
    );
    let immature = reject_one_create(10, 100, Some(1), true).unwrap_err();
    assert!(
        matches!(immature, ConsensusError::BadTx(s) if s.contains("immature")),
        "{immature:?}"
    );
    assert!(reject_one_create(101, 100, Some(1), true).is_ok());
}

#[test]
fn waived_coinbase_fee_is_zero_when_outputs_spend_the_waiver() {
    assert_eq!(fee_with_waived_coinbase(50, 50, 80).unwrap(), 0);
    assert_eq!(fee_with_waived_coinbase(50, 50, 50).unwrap(), 0);
    assert_eq!(fee_with_waived_coinbase(50, 50, 40).unwrap(), 10);
    assert!(fee_with_waived_coinbase(50, 50, 101).is_err());
    assert_eq!(waived_output_residue(50, 10, &[30, 20]), vec![0, 10]);
}

#[test]
fn retarget_boundary_requires_the_period_first_header() {
    let err = require_retarget_first(true, false, None).unwrap_err();
    assert!(
        matches!(err, ConsensusError::BadHeader(s) if s.contains("retarget")),
        "{err:?}"
    );
    assert!(require_retarget_first(true, true, None).unwrap().is_none());
    let first = PeriodFirst {
        time: 1,
        bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
    };
    assert!(require_retarget_first(true, false, Some(first))
        .unwrap()
        .is_some());
    assert!(require_retarget_first(false, false, None)
        .unwrap()
        .is_none());
}
