use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use bitcoin::CompactTarget;
use bitcoin::ScriptBuf;
use bitcoin::TxMerkleNode;
use rbitcoin_consensus::{Milestone, MilestoneAnchor};
use rbitcoin_ibd::ibd::header_walk::*;
use rbitcoin_ibd::ibd::peer_io::{solicit_track, PeerCmd, PeerSlot};
use rbitcoin_ibd::ibd::state::IbdWorkState;
use rbitcoin_ibd::ibd::{MAX_ORDERED_HEADERS, ORDERED_HEADERS_SOFT_CAP};
use rbitcoin_net::chain::ChainHub;
use rbitcoin_net::NetAddr;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::mpsc;

fn mine(prev: BlockHash, n: u32) -> Header {
    let mut h = Header {
        version: bitcoin::block::Version::from_consensus(4),
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::from_byte_array([n as u8; 32]),
        time: 1_600_000_000 + n,
        bits: CompactTarget::from_consensus(0x207fffff),
        nonce: n,
    };
    rbitcoin_consensus::grind_regtest_pow(&mut h);
    h
}

fn slot(id: usize) -> (PeerSlot, mpsc::UnboundedReceiver<PeerCmd>) {
    let (cmd_tx, rx) = mpsc::unbounded_channel();
    let task = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .spawn(async {});
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 9, 0, id as u8)), 8333);
    (
        PeerSlot {
            id,
            addr,
            net: NetAddr::from_socket(addr),
            cmd_tx,
            in_flight: Default::default(),
            requested: solicit_track().0,
            solicited_bytes: solicit_track().1,
            solicited_ms: solicit_track().2,
            open_block_bytes: Arc::new(AtomicU64::new(0)),
            any_rx_ms: Arc::new(AtomicU64::new(0)),
            getdata_wire: rbitcoin_ibd::ibd::peer_io::GetdataWire::new(),
            peer_height: 50_000,
            connected_ms: 1,
            first_data_ms: 0,
            bytes_rx_total: Arc::new(AtomicU64::new(0)),
            rate: Default::default(),
            chain: Default::default(),
            conn_type: rbitcoin_net::peers::PeerConnType::OutboundFullRelay,
            block_download_paused_until: None,
            late_answers: Default::default(),
            alive: true,
            task,
        },
        rx,
    )
}

struct ResetClock;

impl Drop for ResetClock {
    fn drop(&mut self) {
        set_walk_unix_for_test(None);
    }
}

fn bare(label: &str) -> (rbitcoin_query::testutil::TempDir, ChainHub, Header) {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled(label);
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    let gen_hdr = hub.header_of(&gen).unwrap();
    (dir, hub, gen_hdr)
}

fn opened(
    label: &str,
) -> (
    rbitcoin_query::testutil::TempDir,
    ChainHub,
    IbdWorkState,
    Header,
) {
    let (dir, hub, gen_hdr) = bare(label);
    let gen = gen_hdr.block_hash();
    let (s0, rx0) = slot(0);
    let (s1, rx1) = slot(1);
    std::mem::forget(rx0);
    std::mem::forget(rx1);
    let mut st = IbdWorkState::new(vec![s0, s1], Some(gen), Some(0));
    ensure_origin(&mut st, &hub);
    (dir, hub, st, gen_hdr)
}

fn fresh(hub: &ChainHub) -> IbdWorkState {
    let gen = hub.tip_hash().unwrap();
    let (s0, rx0) = slot(0);
    let (s1, rx1) = slot(1);
    std::mem::forget(rx0);
    std::mem::forget(rx1);
    IbdWorkState::new(vec![s0, s1], Some(gen), Some(0))
}

fn checkpoint(height: u32, hash: BlockHash, supplier: Option<NetAddr>) -> Checkpoint {
    Checkpoint {
        hash,
        height,
        height_wide: u64::from(height),
        work: [0; 32],
        header: None,
        times: Vec::new(),
        diff: DiffSnap::default(),
        supplied_by: supplier,
    }
}

fn high_floor() -> Milestone {
    Milestone {
        height: 1_000_000,
        anchor: Some(MilestoneAnchor {
            hash: BlockHash::from_byte_array([0xab; 32]),
            min_work_be: [0xff; 32],
        }),
    }
}

fn one_more_work(work: [u8; 32]) -> [u8; 32] {
    let mut out = work;
    for byte in out.iter_mut().rev() {
        let (next, wrapped) = byte.overflowing_add(1);
        *byte = next;
        if !wrapped {
            break;
        }
    }
    out
}

fn fork_from(prev: BlockHash, n: u32) -> Vec<Header> {
    let first = mine(prev, n);
    let second = mine(first.block_hash(), n + 1);
    vec![first, second]
}

#[test]
fn a_clock_two_hours_behind_the_median_still_starts_the_walk() {
    let _clock = ResetClock;
    let (_dir, hub, gen) = bare("clock-two-hours");
    let genesis_time = u64::from(gen.time);
    let mut behind = fresh(&hub);
    set_walk_unix_for_test(Some(genesis_time - 7_201));
    ensure_origin(&mut behind, &hub);
    assert!(behind.header_walk.clock_refused);
    assert!(behind.header_walk.max_headers.is_none());
    set_walk_unix_for_test(Some(genesis_time - 7_200));
    ensure_origin(&mut behind, &hub);
    assert!(!behind.header_walk.clock_refused);
    assert!(behind.header_walk.max_headers.is_some());
    assert!(behind.header_walk.origin);
}

#[test]
fn eleven_header_times_use_the_middle_time_as_the_walk_median() {
    let _clock = ResetClock;
    let (_dir, hub, _gen) = bare("clock-eleven");
    let mut st = fresh(&hub);
    set_walk_unix_for_test(Some(0));
    ensure_origin(&mut st, &hub);
    assert!(st.header_walk.clock_refused);
    let mut times = vec![0u32];
    times.extend([10_000; 5]);
    times.extend([50_000; 5]);
    st.header_walk.tip.times = times;
    set_walk_unix_for_test(Some(10_000 - 7_200));
    ensure_origin(&mut st, &hub);
    assert!(
        !st.header_walk.clock_refused,
        "the middle of the eleven times is inside the two-hour window"
    );
    assert!(st.header_walk.max_headers.is_some());
}

#[test]
fn the_indexed_band_is_the_origin_tip() {
    let mut walk = HeaderWalk::default();
    walk.origin = true;
    walk.tip.height = 4;
    assert!(walk.indexed_band_end().is_none());
    walk.tip.hash = Some(BlockHash::from_byte_array([0x11; 32]));
    walk.origin = false;
    assert!(walk.indexed_band_end().is_none());
    walk.origin = true;
    assert_eq!(walk.indexed_band_end(), Some(4));
}

#[test]
fn checkpoints_report_when_the_walk_has_one() {
    let mut walk = HeaderWalk::default();
    assert!(!walk.has_checkpoints());
    walk.checkpoints
        .push(checkpoint(1, BlockHash::from_byte_array([0x22; 32]), None));
    assert!(walk.has_checkpoints());
}

#[test]
fn a_full_window_below_the_floor_keeps_the_peer_that_moved_the_queue() {
    let (_dir, mut hub, mut st, _gen) = opened("settle-window");
    hub.milestone = high_floor();
    let tip = st.header_walk.tip_hash();
    let window = rbitcoin_net::codec::MAX_HEADERS_RESULTS;
    st.ordered.push_back(BlockHash::from_byte_array([0x41; 32]));
    st.header_walk.reserved = Some(0);
    settle_walk_peer(&mut st, &hub, 0, tip, 0, None, window);
    assert!(
        !st.header_walk.walk_quiet.contains(&0),
        "a full window that grew the queue keeps the peer"
    );
    assert_eq!(st.header_walk.reserved, Some(0));

    st.ordered.clear();
    st.header_walk.walk_quiet.clear();
    st.header_walk.reserved = Some(0);
    settle_walk_peer(&mut st, &hub, 0, tip, 0, None, 1);
    assert!(st.header_walk.walk_quiet.contains(&0));
    assert!(st.header_walk.reserved.is_none());

    st.header_walk.walk_quiet.clear();
    st.header_walk.reserved = Some(1);
    settle_walk_peer(&mut st, &hub, 0, tip, 0, None, 1);
    assert!(st.header_walk.walk_quiet.contains(&0));
    assert_eq!(st.header_walk.reserved, Some(1));

    st.ordered.clear();
    st.header_walk.walk_quiet.clear();
    st.header_walk.reserved = Some(0);
    settle_walk_peer(&mut st, &hub, 0, tip, 0, None, window);
    assert!(st.header_walk.walk_quiet.contains(&0));
    assert!(st.header_walk.reserved.is_none());
}

#[test]
fn a_probe_starts_for_two_live_peers_and_not_while_one_is_open() {
    let (_dir, hub, mut st, _gen) = opened("probe-two");
    st.header_walk.probe_open = Some(1);
    probe_unprobed_peer(&mut st, &hub);
    assert!(st.header_walk.probe.is_none());
    st.header_walk.probe_open = None;
    probe_unprobed_peer(&mut st, &hub);
    assert_eq!(st.header_walk.probe, Some(0));
    st.header_walk.probe = None;
    st.slots[1].alive = false;
    probe_unprobed_peer(&mut st, &hub);
    assert!(st.header_walk.probe.is_none());
}

#[test]
fn forgetting_a_peer_clears_only_that_peers_probe() {
    let mut st = IbdWorkState::new(vec![], None, None);
    st.header_walk.probe = Some(0);
    forget_walk_peer(&mut st, 1);
    assert_eq!(st.header_walk.probe, Some(0));
    forget_walk_peer(&mut st, 0);
    assert!(st.header_walk.probe.is_none());
}

#[test]
fn a_split_is_settled_only_for_a_height_already_read() {
    let (_dir, _hub, mut st, gen) = opened("split-confirm");
    let hdr = mine(gen.block_hash(), 3);
    let height = 3u32;
    st.slots[0].chain.note_headers(
        &st.height_to_hash,
        |_| Some(BlockHash::from_byte_array([8; 32])),
        5,
        height,
        &[hdr],
    );
    assert_eq!(
        st.slots[0].chain.off_path_split_height(&st.height_to_hash),
        Some(height)
    );
    assert!(st.slots[0].chain.left_path_by(&st.height_to_hash, height));
    apply_split_confirms(&mut st, &HashMap::new());
    assert!(
        st.slots[0].chain.left_path_by(&st.height_to_hash, height),
        "a height not in this confirm read keeps the split"
    );
    let mut confirmed = HashMap::new();
    confirmed.insert(height, Some(hdr.block_hash()));
    apply_split_confirms(&mut st, &confirmed);
    assert!(
        !st.slots[0].chain.left_path_by(&st.height_to_hash, height),
        "the confirmed hash at the split clears it"
    );
}

#[test]
fn header_poll_waits_at_the_ordered_cap_and_latches_an_empty_queue() {
    let mut st = IbdWorkState::new(vec![], None, None);
    let ask = |st: &IbdWorkState, live: usize, min_cache: usize, fan: usize| {
        header_poll(
            st,
            HeaderPollIn {
                live,
                need_ready_headroom: false,
                lag: 0,
                min_cache,
                fan,
            },
        )
    };
    assert!(matches!(ask(&st, 0, 0, 0), HeaderPoll::LatchDone));
    assert!(matches!(ask(&st, 0, 0, 1), HeaderPoll::Ask));
    st.header_walk.origin = true;
    st.header_walk.tip.height = 4;
    assert!(matches!(ask(&st, 0, 0, 0), HeaderPoll::Ask));
    st.header_walk.origin = false;
    st.header_walk.tip.height = 0;
    assert!(matches!(ask(&st, 0, 0, 0), HeaderPoll::LatchDone));
    assert!(matches!(ask(&st, 1, 2, 0), HeaderPoll::Ask));
    st.header_walk.origin = true;
    st.header_walk.tip.height = 4;
    assert!(matches!(
        ask(&st, MAX_ORDERED_HEADERS, 0, 0),
        HeaderPoll::Wait
    ));
    assert!(matches!(
        ask(&st, MAX_ORDERED_HEADERS - 1, 0, 0),
        HeaderPoll::Ask
    ));
    st.header_walk.origin = false;
    st.header_walk.tip.height = 0;
    assert!(matches!(
        ask(
            &st,
            ORDERED_HEADERS_SOFT_CAP,
            ORDERED_HEADERS_SOFT_CAP + 1,
            1
        ),
        HeaderPoll::Wait
    ));
    assert!(matches!(ask(&st, 1, 2, 0), HeaderPoll::Ask));
}

#[test]
fn an_unlinked_or_empty_batch_is_not_a_competing_chain() {
    let (_dir, hub, mut st, gen) = opened("suppress-unlinked");
    let tip = st.header_walk.tip_hash();
    assert!(!suppress_competing_chain(&mut st, &hub, 0, &[]));
    assert_eq!(st.header_walk.tip_hash(), tip);
    let kept = mine(gen.block_hash(), 1);
    assert!(extend_tip(&mut st, &hub, &[kept], None));
    let walked = st.header_walk.tip_hash();
    st.header_walk.checkpoints.clear();
    let linked = mine(gen.block_hash(), 2);
    let broken = mine(gen.block_hash(), 3);
    assert!(!suppress_competing_chain(
        &mut st,
        &hub,
        0,
        &[linked, broken]
    ));
    assert_eq!(st.header_walk.tip_hash(), walked);
}

#[test]
fn a_short_continuation_above_the_floor_is_not_adopted_as_a_fork() {
    let (_dir, hub, mut st, gen) = opened("suppress-above");
    let kept = mine(gen.block_hash(), 1);
    assert!(extend_tip(&mut st, &hub, &[kept], None));
    let walked = kept.block_hash();
    st.header_walk.checkpoints.clear();
    let fork = fork_from(gen.block_hash(), 20);
    assert!(suppress_competing_chain(&mut st, &hub, 0, &fork));
    assert_eq!(st.header_walk.tip_hash(), Some(walked));

    let (_dir, mut hub, mut st, gen) = opened("suppress-cross");
    let kept = mine(gen.block_hash(), 1);
    assert!(extend_tip(&mut st, &hub, &[kept], None));
    let walked = kept.block_hash();
    st.header_walk.checkpoints.clear();
    hub.milestone = Milestone {
        height: 1_000_000,
        anchor: Some(MilestoneAnchor {
            hash: BlockHash::from_byte_array([0xcd; 32]),
            min_work_be: one_more_work(st.header_walk.tip_work()),
        }),
    };
    let fork = fork_from(gen.block_hash(), 30);
    let adopted = fork[1].block_hash();
    assert!(!suppress_competing_chain(&mut st, &hub, 0, &fork));
    assert_eq!(st.header_walk.tip_hash(), Some(adopted));
    assert_ne!(st.header_walk.tip_hash(), Some(walked));
}

#[test]
fn the_oldest_refused_tip_stays_until_the_cap_is_passed() {
    let (_dir, _hub, mut st, gen) = opened("invalid-cap");
    let bad = mine(gen.block_hash(), 1);
    st.reorg.invalid.mark(bad.block_hash().to_byte_array());
    let mut prev = bad;
    let mut oldest = None;
    for n in 0..1024 {
        let hdr = mine(prev.block_hash(), n + 2);
        assert!(refuse_invalid_batch(&mut st, 0, &[hdr]));
        if n == 0 {
            oldest = Some(hdr);
        }
        prev = hdr;
    }
    let oldest = oldest.unwrap();
    let still = mine(oldest.block_hash(), 9_000);
    assert!(
        refuse_invalid_batch(&mut st, 0, &[still]),
        "the first refused tip is still refused at the cap"
    );
    let after = mine(oldest.block_hash(), 9_001);
    assert!(
        !refuse_invalid_batch(&mut st, 0, &[after]),
        "the next refusal of a new tip drops the oldest"
    );
}

#[test]
fn a_refused_batch_releases_only_the_sender_reservation() {
    let (_dir, _hub, mut st, gen) = opened("refuse-reserve");
    let bad = mine(gen.block_hash(), 1);
    st.reorg.invalid.mark(bad.block_hash().to_byte_array());
    let first = mine(bad.block_hash(), 2);
    st.header_walk.reserved = Some(0);
    assert!(refuse_invalid_batch(&mut st, 0, &[first]));
    assert!(st.header_walk.reserved.is_none());
    let second = mine(first.block_hash(), 3);
    st.header_walk.reserved = Some(1);
    assert!(refuse_invalid_batch(&mut st, 0, &[second]));
    assert_eq!(st.header_walk.reserved, Some(1));
}

#[test]
fn retiring_a_block_drops_the_queue_above_its_parent_and_keeps_the_base() {
    let (_dir, hub, mut st, gen) = opened("retire-bounds");
    let h1 = mine(gen.block_hash(), 1);
    let h2 = mine(h1.block_hash(), 2);
    hub.ensure_header(&h1).unwrap();
    hub.ensure_header(&h2).unwrap();
    let parent = h1.block_hash();
    let failed = h2.block_hash();
    let sentinel = BlockHash::from_byte_array([0x5a; 32]);
    let arm = |st: &mut IbdWorkState| {
        st.record_height(parent, 1);
        st.record_height(failed, 2);
    };
    arm(&mut st);
    hub.query
        .block_queue_enqueue(1, parent.to_byte_array(), 1, b"a")
        .unwrap();
    hub.query
        .block_queue_enqueue(2, failed.to_byte_array(), 2, b"b")
        .unwrap();

    st.header_walk.origin = false;
    st.header_walk.tip.hash = Some(sentinel);
    st.header_walk.tip.height = 5;
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.tip.hash, Some(sentinel));
    let queued = hub.query.block_queue_queued_heights();
    assert!(queued.contains(&1));
    assert!(!queued.contains(&2));

    arm(&mut st);
    st.header_walk.origin = true;
    st.header_walk.tip.hash = Some(sentinel);
    st.header_walk.tip.height = 0;
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.tip.hash, Some(sentinel));

    arm(&mut st);
    st.header_walk.origin = true;
    st.header_walk.tip.hash = Some(sentinel);
    st.header_walk.tip.height = 2;
    st.header_walk.base_height = 1;
    st.header_walk.base_hash = Some(sentinel);
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.tip.hash, Some(parent));
    assert_eq!(st.header_walk.base_hash, Some(sentinel));
    assert_eq!(st.header_walk.base_height, 1);

    arm(&mut st);
    st.header_walk.tip.hash = Some(failed);
    st.header_walk.tip.height = 2;
    st.header_walk.base_height = 5;
    st.header_walk.base_hash = Some(sentinel);
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.base_height, 1);
    assert_eq!(st.header_walk.base_hash, Some(parent));

    arm(&mut st);
    st.header_walk.tip.hash = Some(failed);
    st.header_walk.tip.height = 2;
    st.header_walk.base_height = 0;
    st.header_walk.checkpoints = vec![checkpoint(1, sentinel, None)];
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.checkpoints.len(), 1);
    assert_eq!(st.header_walk.checkpoints[0].hash, sentinel);

    arm(&mut st);
    st.header_walk.tip.hash = Some(failed);
    st.header_walk.tip.height = 2;
    st.header_walk.checkpoints = vec![checkpoint(0, gen.block_hash(), None)];
    retire_invalid(&mut st, &hub, failed, 2);
    assert_eq!(st.header_walk.checkpoints.len(), 2);
    assert_eq!(st.header_walk.checkpoints.last().unwrap().hash, parent);
    assert_eq!(st.header_walk.checkpoints.last().unwrap().height, 1);
}

#[test]
fn a_match_above_the_base_is_the_confirm_lead() {
    let (_dir, hub, mut st, gen) = opened("witness-lead");
    let supplier = Some(st.slots[0].net);
    st.header_walk.proven = false;
    st.header_walk.base_height = 4;
    st.header_walk.base_hash = Some(gen.block_hash());
    st.header_walk.witness_height = Some(9);
    st.header_walk.checkpoints = vec![checkpoint(
        9,
        BlockHash::from_byte_array([0x91; 32]),
        supplier,
    )];
    rewind_to(&mut st, &hub, 100, None);
    let bounds = unproven_offer_bounds(&st);
    assert_eq!(bounds.lead_from, 9);
    assert_eq!(st.header_walk.witness_height, Some(9));

    st.header_walk.proven = false;
    st.header_walk.base_height = 4;
    st.header_walk.witness_height = Some(4);
    st.header_walk.checkpoints = vec![checkpoint(
        4,
        BlockHash::from_byte_array([0x44; 32]),
        supplier,
    )];
    rewind_to(&mut st, &hub, 100, None);
    assert!(st.header_walk.witness_height.is_none());
    assert_eq!(unproven_offer_bounds(&st).lead_from, 4);
}

#[test]
fn the_milestone_header_is_latched_only_at_the_milestone_height() {
    let (_dir, mut hub, _gen) = bare("milestone-latch");
    let gen = hub.tip_hash().unwrap();
    let hdr = mine(gen, 1);
    hub.milestone = Milestone::height(5);
    let mut tip = WalkTip {
        hash: Some(gen),
        ..WalkTip::default()
    };
    advance_tip(&mut tip, &hub, &[hdr]);
    assert!(tip.milestone_hash.is_none());
    hub.milestone = Milestone::height(1);
    let mut tip = WalkTip {
        hash: Some(gen),
        ..WalkTip::default()
    };
    advance_tip(&mut tip, &hub, &[hdr]);
    assert_eq!(tip.milestone_hash, Some(hdr.block_hash()));
}

#[test]
fn a_challenger_fork_is_known_at_its_fork_height() {
    let mut st = IbdWorkState::new(vec![], None, None);
    let fork = BlockHash::from_byte_array([0x31; 32]);
    let other = BlockHash::from_byte_array([0x33; 32]);
    st.header_walk.challenger = Some(Challenger {
        fork_hash: fork,
        fork_height: 7,
        tip: WalkTip {
            hash: Some(BlockHash::from_byte_array([0x32; 32])),
            height: 9,
            ..WalkTip::default()
        },
        checkpoints: Vec::new(),
    });
    assert_eq!(known(&st, fork).height, Some(7));
    assert_eq!(known(&st, other).height, None);
}

#[test]
fn promoting_a_challenger_adds_a_checkpoint_only_past_the_last_one() {
    let (_dir, hub, mut st, _gen) = opened("promote-ckpt");
    let plant = |st: &mut IbdWorkState, tip_h: u32, last_h: u32| {
        st.header_walk.checkpoints.clear();
        st.header_walk.challenger = Some(Challenger {
            fork_hash: BlockHash::from_byte_array([0x46; 32]),
            fork_height: 0,
            tip: WalkTip {
                hash: Some(BlockHash::from_byte_array([0x44; 32])),
                height: tip_h,
                height_wide: u64::from(tip_h),
                ..WalkTip::default()
            },
            checkpoints: vec![checkpoint(
                last_h,
                BlockHash::from_byte_array([0x45; 32]),
                None,
            )],
        });
    };
    plant(&mut st, 4, 4);
    promote_challenger(&mut st, &hub);
    assert_eq!(st.header_walk.checkpoints.len(), 1);
    plant(&mut st, 4, 1);
    promote_challenger(&mut st, &hub);
    assert_eq!(st.header_walk.checkpoints.len(), 2);
    assert_eq!(st.header_walk.checkpoints.last().unwrap().height, 4);
    plant(&mut st, 4, 6);
    promote_challenger(&mut st, &hub);
    assert_eq!(st.header_walk.checkpoints.len(), 1);
    assert_eq!(st.header_walk.checkpoints[0].height, 6);
}

#[test]
fn hashes_above_the_cut_leave_the_queue_unless_named() {
    let mut st = IbdWorkState::new(vec![], None, None);
    let above = BlockHash::from_byte_array([0x51; 32]);
    let equal = BlockHash::from_byte_array([0x52; 32]);
    let named = BlockHash::from_byte_array([0x53; 32]);
    st.record_height(equal, 4);
    st.record_height(above, 5);
    st.record_height(named, 6);
    let mut keep = HashSet::new();
    keep.insert(named);
    drop_queue_above(&mut st, 4, &keep);
    assert!(st.height_to_hash.get(&5).is_none());
    assert_eq!(st.height_to_hash.get(&4), Some(&equal));
    assert_eq!(st.height_to_hash.get(&6), Some(&named));
}

#[test]
fn a_heavier_fork_keeps_the_checkpoint_at_the_fork() {
    let (_dir, hub, mut st, gen) = opened("adopt-fork-ckpt");
    let fork_hash = gen.block_hash();
    let above = BlockHash::from_byte_array([0x61; 32]);
    st.header_walk.checkpoints = vec![checkpoint(0, fork_hash, None), checkpoint(5, above, None)];
    let hdr = mine(fork_hash, 7);
    let work = chain_work_after(st.header_walk.tip.work, &[hdr]);
    adopt_heavier(&mut st, &hub, fork_hash, &[hdr], work, None);
    assert!(st
        .header_walk
        .checkpoints
        .iter()
        .any(|c| c.height == 0 && c.hash == fork_hash));
    assert!(!st
        .header_walk
        .checkpoints
        .iter()
        .any(|c| c.height == 5 && c.hash == above));
    assert_eq!(st.header_walk.tip_hash(), Some(hdr.block_hash()));
}

#[test]
fn the_locator_names_the_confirmed_tip_only_between_the_anchor_and_the_queue() {
    let (_dir, hub, _gen) = bare("confirmed-between");
    let op_true = ScriptBuf::from_bytes(vec![0x51]);
    hub.generate_to_script(2, op_true, vec![]).unwrap();
    let tip = hub.tip_hash().unwrap();
    assert_eq!(hub.tip_height(), Some(2));
    let mut st = fresh(&hub);
    let front = BlockHash::from_byte_array([0x71; 32]);
    st.ordered.push_back(front);
    st.hash_height.insert(front, 4);
    assert_eq!(confirmed_tip_above_anchor(&st, &hub, 0), Some(tip));
    st.hash_height.insert(front, 2);
    assert!(confirmed_tip_above_anchor(&st, &hub, 0).is_none());
    st.hash_height.insert(front, 4);
    assert!(confirmed_tip_above_anchor(&st, &hub, 2).is_none());
    assert!(confirmed_tip_above_anchor(&st, &hub, 3).is_none());
}

#[test]
fn a_header_that_lands_on_the_length_cap_is_still_allowed() {
    let (_dir, hub, mut st, gen) = opened("length-cap");
    st.header_walk.tip.height = 3;
    st.header_walk.tip.height_wide = 3;
    st.header_walk.max_headers = Some(4);
    let hdr = mine(gen.block_hash(), 1);
    assert!(extend_tip(&mut st, &hub, &[hdr], None));
    assert_eq!(st.header_walk.tip_height(), 4);
    assert!(!st.header_walk.length_exceeded);
    assert!(!store_would_pass_length(&mut st, 0));
    assert!(!st.header_walk.length_exceeded);
    assert!(store_would_pass_length(&mut st, 1));
    assert!(st.header_walk.length_exceeded);
}

#[test]
fn a_batch_that_does_not_link_does_not_extend_or_mark_the_length_cap() {
    let (_dir, hub, mut st, gen) = opened("extend-link");
    let before = st.header_walk.tip_hash();
    let mut bad = mine(gen.block_hash(), 1);
    bad.bits = CompactTarget::from_consensus(0x1d00ffff);
    assert!(!extend_tip(&mut st, &hub, &[bad], None));
    assert_eq!(st.header_walk.tip_hash(), before);
    assert!(!st.header_walk.length_exceeded);
    st.header_walk.tip.height = 5;
    st.header_walk.tip.height_wide = 5;
    st.header_walk.max_headers = Some(4);
    assert!(!extend_tip(&mut st, &hub, &[], None));
    assert!(!st.header_walk.length_exceeded);
    assert_eq!(st.header_walk.tip_hash(), before);
}

#[test]
fn an_adopt_rewrite_is_due_for_an_unproved_walk_and_a_full_window_gap() {
    assert!(adopt_checkpoint_due(false, true, Some(10), 10, 1));
    assert!(adopt_checkpoint_due(true, false, Some(10), 10, 1));
    assert!(!adopt_checkpoint_due(true, true, Some(1_500), 2_000, 1));
    assert!(adopt_checkpoint_due(true, true, Some(2), 3_999, 1));
}

#[test]
fn an_unproved_resume_keeps_the_tip_until_the_next_header_is_indexed() {
    let (_dir, mut hub, mut st, gen) = opened("resume-keep");
    let hdr = mine(gen.block_hash(), 1);
    let saved = hdr.block_hash();
    assert!(extend_tip(&mut st, &hub, &[hdr], Some(0)));
    assert!(adopt_path(&hub).exists());
    hub.milestone = high_floor();
    let mut resumed = fresh(&hub);
    assert!(restore_adopt(&mut resumed, &hub));
    assert!(!resumed.header_walk.proven);
    assert_eq!(resumed.header_walk.tip_hash(), Some(saved));
    assert!(send_getheaders(&mut resumed, &hub).unwrap());
    assert_eq!(resumed.header_walk.tip_hash(), Some(saved));
    assert!(adopt_path(&hub).exists());

    let mut reset = fresh(&hub);
    assert!(restore_adopt(&mut reset, &hub));
    let next = reset.header_walk.tip_height().saturating_add(1);
    reset.record_height(BlockHash::from_byte_array([0x81; 32]), next);
    assert!(send_getheaders(&mut reset, &hub).unwrap());
    assert_eq!(reset.header_walk.tip_hash(), Some(gen.block_hash()));
    assert!(!adopt_path(&hub).exists());
}

#[test]
fn the_locator_names_a_challenger_only_when_it_is_taller_than_the_tip() {
    let (_dir, hub, _gen) = bare("locator-challenger");
    let op_true = ScriptBuf::from_bytes(vec![0x51]);
    hub.generate_to_script(2, op_true, vec![]).unwrap();
    let mut st = fresh(&hub);
    let low = BlockHash::from_byte_array([0x91; 32]);
    let high = BlockHash::from_byte_array([0x92; 32]);
    st.header_walk.challenger = Some(Challenger {
        fork_hash: BlockHash::from_byte_array([0x93; 32]),
        fork_height: 1,
        tip: WalkTip {
            hash: Some(low),
            height: 2,
            ..WalkTip::default()
        },
        checkpoints: Vec::new(),
    });
    assert!(!header_ask_locator(&st, &hub).refill.contains(&low));
    let ch = st.header_walk.challenger.as_mut().unwrap();
    ch.tip.height = 3;
    ch.tip.hash = Some(high);
    assert!(header_ask_locator(&st, &hub).refill.contains(&high));
}

#[test]
fn the_checkpoint_supplier_is_the_peer_that_sent_the_headers() {
    let (_dir, hub, mut st, gen) = opened("supplier-peer");
    let hdr = mine(gen.block_hash(), 1);
    assert!(extend_tip(&mut st, &hub, &[hdr], Some(0)));
    let supplied = st.header_walk.checkpoints.last().unwrap().supplied_by;
    assert_eq!(supplied, Some(st.slots[0].net));
    assert_ne!(supplied, Some(st.slots[1].net));
}
