use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use rbitcoin_ibd::ibd::peer_io::empty_header_fk_bus;
use rbitcoin_ibd::ibd::state::IbdWorkState;

use rbitcoin_ibd::ibd::events::run_header_batch;

use std::sync::Mutex;

/// A batch classification drops is not written. The header thread used to
/// persist every batch before it knew the batch was stray.
#[test]
fn stray_batch_is_not_persisted() {
    let (dir, mut hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("stray-batch");
    hub.ensure_genesis().unwrap();
    hub.milestone = rbitcoin_consensus::Milestone {
        height: 1,
        anchor: Some(rbitcoin_consensus::MilestoneAnchor {
            hash: BlockHash::from_byte_array([0xcd; 32]),
            min_work_be: [0xff; 32],
        }),
    };
    let header = {
        let mut h = bitcoin::block::Header {
            version: bitcoin::block::Version::from_consensus(4),
            prev_blockhash: BlockHash::from_byte_array([0xab; 32]),
            merkle_root: bitcoin::TxMerkleNode::from_byte_array([1; 32]),
            time: 1_600_000_000,
            bits: bitcoin::CompactTarget::from_consensus(0x207fffff),
            nonce: 0,
        };
        rbitcoin_consensus::grind_regtest_pow(&mut h);
        h
    };
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let _ = rbitcoin_ibd::ibd::header_walk::send_getheaders(&mut st, &hub);
    let shared = Mutex::new(st);
    run_header_batch(&shared, &hub, 0, vec![header], &empty_header_fk_bus());
    assert_eq!(side_count(&hub), 0);
    let _ = std::fs::remove_dir_all(dir);
}

fn side_count(hub: &rbitcoin_net::chain::ChainHub) -> usize {
    hub.chaintips()
        .iter()
        .filter(|t| t.status != "active")
        .count()
}
