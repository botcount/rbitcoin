//! A burst of self-connection or header-reject events logs fewer than one line each.

use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use rbitcoin_consensus::ConsensusError;
use rbitcoin_log::Level;
use rbitcoin_query::ConfirmError;

#[test]
fn self_connect_and_header_reject_bursts_log_fewer_lines_than_events() {
    const N: u32 = 8;
    rbitcoin_log::reset_repeated_info();
    rbitcoin_log::capture_logs(true);
    let _ = rbitcoin_log::take_logs();
    for i in 0..N {
        rbitcoin_net::peer::log_connected_to_self(format!("127.0.0.1:{}", 20_000 + i));
    }
    let err = ConfirmError::Rules(ConsensusError::BadHeader("timestamp too far in future"));
    for i in 0..N {
        let header = Header {
            version: Version::from_consensus(4),
            prev_blockhash: BlockHash::from_byte_array([i as u8; 32]),
            merkle_root: TxMerkleNode::from_byte_array([0; 32]),
            time: 1_700_000_000,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce: i,
        };
        let _ = rbitcoin_net::chain::header_reject(&header, &err);
    }
    let lines = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    let self_hits = lines
        .iter()
        .filter(|(level, msg)| *level == Level::Info && msg.contains("connected to self"))
        .count();
    let header_hits = lines
        .iter()
        .filter(|(level, msg)| *level == Level::Info && msg.contains("time-too-new"))
        .count();
    assert!(
        self_hits >= 1 && self_hits < N as usize,
        "self-connection lines {self_hits} for {N} events: {lines:?}"
    );
    assert!(
        header_hits >= 1 && header_hits < N as usize,
        "header-reject lines {header_hits} for {N} distinct hashes: {lines:?}"
    );
}
