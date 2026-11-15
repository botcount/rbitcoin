//! Session results for operators cargo-mutants missed in `events`.
use bitcoin::absolute::LockTime;
use bitcoin::block::{Header, Version};
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash;
use bitcoin::script::ScriptBuf;
use bitcoin::transaction::Version as TxVersion;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, Sequence, Target, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use rbitcoin_ibd::ibd::confirm::{ConfirmFeed, ConfirmRejectClass, RejectParent};
use rbitcoin_ibd::ibd::events::{
    apply_block_framed, apply_cascade_reject, apply_confirm_reject, apply_consensus_invalid_reject,
    apply_soft_wire_reject, batch_header_height, body_drain_budget_spent, complete_stored_headers,
    ctrl_drain_continues, disconnect_all_peers, drain_ready_peer_and_body_events,
    ensure_accepted_prefix, on_empty_headers, on_known_headers_batch, ordered_live_has_header_room,
    plan_header_batch, try_enqueue_ordered_header, FramedBody, HeaderPlan, BODY_DRAIN_TIME_BUDGET,
    CTRL_DRAIN_EVENT_BUDGET, CTRL_DRAIN_TIME_BUDGET,
};
use rbitcoin_ibd::ibd::header_walk::{Checkpoint, InFlight};
use rbitcoin_ibd::ibd::peer_io::{PeerCmd, PeerEvent, PeerSlot};
use rbitcoin_ibd::ibd::state::{HeaderCmd, IbdWorkState, InflightReq};
use rbitcoin_ibd::ibd::{MAX_ORDERED_HEADERS, ORDERED_HEADERS_SOFT_CAP};
use rbitcoin_net::codec::MAX_HEADERS_RESULTS;
use rbitcoin_net::seeds::AddrMan;
use rbitcoin_primitives::Fk;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn hash_n(n: u32) -> BlockHash {
    let mut raw = [0u8; 32];
    raw[..4].copy_from_slice(&n.to_le_bytes());
    BlockHash::from_byte_array(raw)
}

fn open(
    label: &str,
) -> (
    rbitcoin_query::testutil::TempDir,
    rbitcoin_net::chain::ChainHub,
) {
    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled(label);
    hub.ensure_genesis().unwrap();
    (dir, hub)
}

fn peer(id: usize, peer_height: u32) -> (PeerSlot, mpsc::UnboundedReceiver<PeerCmd>) {
    let (cmd_tx, rx) = mpsc::unbounded_channel();
    let task = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .spawn(async {});
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 9, 0, id as u8)), 8333);
    let (requested, solicited_bytes, solicited_ms) = rbitcoin_ibd::ibd::peer_io::solicit_track();
    (
        PeerSlot {
            id,
            addr,
            net: rbitcoin_net::NetAddr::from_socket(addr),
            cmd_tx,
            in_flight: Default::default(),
            requested,
            solicited_bytes,
            solicited_ms,
            open_block_bytes: Arc::new(AtomicU64::new(0)),
            any_rx_ms: Arc::new(AtomicU64::new(0)),
            getdata_wire: rbitcoin_ibd::ibd::peer_io::GetdataWire::new(),
            peer_height,
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

fn header_on(prev: BlockHash, n: u32) -> Header {
    let mut header = Header {
        version: Version::from_consensus(4),
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::from_byte_array({
            let mut raw = [1u8; 32];
            raw[..4].copy_from_slice(&n.to_le_bytes());
            raw
        }),
        time: 1_600_000_000 + n,
        bits: CompactTarget::from_consensus(0x207f_ffff),
        nonce: 0,
    };
    rbitcoin_consensus::grind_regtest_pow(&mut header);
    header
}

fn chain_on(prev: BlockHash, n: usize) -> Vec<Header> {
    let mut out = Vec::with_capacity(n);
    let mut prev = prev;
    for i in 0..n {
        let header = header_on(prev, i as u32);
        prev = header.block_hash();
        out.push(header);
    }
    out
}

fn mine(prev: BlockHash, time: u32, height: u32) -> Block {
    let bits = CompactTarget::from_consensus(0x207f_ffff);
    let mut script = if height == 0 {
        vec![0x00]
    } else {
        rbitcoin_consensus::bip34_height_script(height)
    };
    while script.len() < 2 {
        script.push(0x00);
    }
    let coinbase = Transaction {
        version: TxVersion::ONE,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(script),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    };
    let header = Header {
        version: Version::from_consensus(4),
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::from_byte_array([0u8; 32]),
        time,
        bits,
        nonce: 0,
    };
    let mut block = Block {
        header,
        txdata: vec![coinbase],
    };
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    let target = Target::from_compact(bits);
    for nonce in 0..u32::MAX {
        block.header.nonce = nonce;
        if block.header.validate_pow(target).is_ok() {
            break;
        }
    }
    block
}

fn asked(rx: &mut mpsc::UnboundedReceiver<PeerCmd>) -> bool {
    matches!(rx.try_recv(), Ok(PeerCmd::GetHeaders { .. }))
}

fn drain(
    st: &mut IbdWorkState,
    hub: &rbitcoin_net::chain::ChainHub,
    body_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
    ctrl_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
) {
    let stats = rbitcoin_ibd::ibd::status::LoopStats::default();
    let next = AtomicU32::new(0);
    let mut book = AddrMan::new();
    let local = SocketAddr::from(([127, 0, 0, 1], 18444));
    drain_ready_peer_and_body_events(
        st, hub, body_rx, ctrl_rx, &next, &stats, &mut book, local, None,
    )
    .unwrap();
}

fn plant(st: &mut IbdWorkState, hash: BlockHash, height: u32) {
    st.record_height(hash, height);
    st.hash_height.insert(hash, height);
}

#[test]
fn disconnect_one_peer_sends_shutdown() {
    let (slot, mut rx) = peer(1, 1);
    let mut st = IbdWorkState::new(vec![slot], None, None);
    disconnect_all_peers(&mut st);
    assert!(matches!(rx.try_recv(), Ok(PeerCmd::Shutdown)));
    assert!(st.slots.is_empty());

    let mut empty = IbdWorkState::new(Vec::new(), None, None);
    disconnect_all_peers(&mut empty);
    assert!(empty.slots.is_empty());
}

#[test]
fn body_drain_stops_when_elapsed_reaches_budget() {
    assert!(!body_drain_budget_spent(Duration::ZERO));
    assert!(body_drain_budget_spent(BODY_DRAIN_TIME_BUDGET));
}

#[test]
fn ctrl_drain_continues_at_count_and_time_bounds() {
    let budget = CTRL_DRAIN_TIME_BUDGET;
    let cap = CTRL_DRAIN_EVENT_BUDGET;
    let under = budget - Duration::from_nanos(1);
    assert!(ctrl_drain_continues(0, Duration::ZERO));
    assert!(ctrl_drain_continues(cap - 1, under));
    assert!(!ctrl_drain_continues(cap, Duration::ZERO));
    assert!(!ctrl_drain_continues(0, budget));
    assert!(!ctrl_drain_continues(cap + 1, under));
}

#[test]
fn ctrl_drain_applies_512_and_leaves_the_next_event() {
    let (dir, hub) = open("drain-budget");
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let n = CTRL_DRAIN_EVENT_BUDGET as usize;
    let mut hashes = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let hash = hash_n(i as u32);
        st.body.mark_pending(hash);
        hashes.push(hash);
    }
    let (_body_tx, mut body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    for hash in &hashes {
        ctrl_tx
            .send(PeerEvent::BlockDecodeFailed {
                peer: 0,
                hash: *hash,
            })
            .unwrap();
    }
    drop(ctrl_tx);
    drain(&mut st, &hub, &mut body_rx, &mut ctrl_rx);
    assert!(
        st.body.is_pending(&hashes[n]),
        "event past the 512 cap stays unapplied"
    );
    assert!(ctrl_rx.try_recv().is_ok());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn full_header_cmd_queue_stops_ctrl_drain() {
    let (dir, hub) = open("drain-header-cmd");
    let tip = hub.tip_hash();
    let height = hub.tip_height();

    let mut open_st = IbdWorkState::new(Vec::new(), tip, height);
    let ready = hash_n(1);
    open_st.body.mark_pending(ready);
    let (tx, _rx) = mpsc::channel(1);
    open_st.header_cmd = Some(tx);
    let (_body_tx, mut body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    ctrl_tx
        .send(PeerEvent::BlockDecodeFailed {
            peer: 0,
            hash: ready,
        })
        .unwrap();
    drop(ctrl_tx);
    drain(&mut open_st, &hub, &mut body_rx, &mut ctrl_rx);
    assert!(open_st.body.is_missing(&ready));

    let mut full_st = IbdWorkState::new(Vec::new(), tip, height);
    let held = hash_n(2);
    full_st.body.mark_pending(held);
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(HeaderCmd::Ask).unwrap();
    assert_eq!(tx.capacity(), 0);
    full_st.header_cmd = Some(tx);
    let (_body_tx, mut body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    ctrl_tx
        .send(PeerEvent::BlockDecodeFailed {
            peer: 0,
            hash: held,
        })
        .unwrap();
    drop(ctrl_tx);
    drain(&mut full_st, &hub, &mut body_rx, &mut ctrl_rx);
    assert!(full_st.body.is_pending(&held));
    assert!(ctrl_rx.try_recv().is_ok());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn batch_header_height_uses_matching_batch_prev() {
    let (dir, hub) = open("batch-prev");
    let st = IbdWorkState::new(Vec::new(), None, None);
    let prev = hash_n(4);
    let other = hash_n(5);
    assert_eq!(
        batch_header_height(&st, &hub, prev, Some((prev, 10))),
        Some(11)
    );
    assert_eq!(
        batch_header_height(&st, &hub, prev, Some((other, 10))),
        None
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn arm_next(st: &mut IbdWorkState, hash: BlockHash, height: u32) {
    st.record_height(hash, height);
    st.known_headers.insert(hash_n(0xffff));
}

#[test]
fn ordered_header_rejects_bad_hash_and_unlinked_prev() {
    let (dir, hub) = open("enqueue-prev");
    let gen = hub.tip_hash().unwrap();
    let mut st = IbdWorkState::new(Vec::new(), Some(gen), Some(0));
    let good = hash_n(2);
    arm_next(&mut st, good, 1);
    st.body.mark_rejected(good);
    assert!(!try_enqueue_ordered_header(&mut st, &hub, good, gen));
    assert!(st.ordered.is_empty());

    let marked = hash_n(3);
    arm_next(&mut st, marked, 1);
    st.reorg.invalid.mark(marked.to_byte_array());
    assert!(!try_enqueue_ordered_header(&mut st, &hub, marked, gen));
    assert!(st.ordered.is_empty());

    let zero = BlockHash::from_byte_array([0u8; 32]);
    let from_zero = hash_n(4);
    arm_next(&mut st, from_zero, 1);
    assert!(try_enqueue_ordered_header(&mut st, &hub, from_zero, zero));
    assert_eq!(st.ordered.back().copied(), Some(from_zero));

    let phantom = hash_n(0xabc);
    {
        let mut cache = hub.cache.inner.write().unwrap();
        cache.chain = vec![gen, phantom];
        cache.by_hash.remove(&phantom);
    }
    assert_eq!(hub.tip_hash(), Some(phantom));
    assert!(!hub.has_block(&phantom));
    let mut tipped = IbdWorkState::new(Vec::new(), Some(gen), Some(0));
    tipped.known_headers.remove(&phantom);
    let child = hash_n(5);
    arm_next(&mut tipped, child, 1);
    tipped.known_headers.remove(&phantom);
    assert!(try_enqueue_ordered_header(
        &mut tipped,
        &hub,
        child,
        phantom
    ));
    assert_eq!(tipped.ordered.back().copied(), Some(child));

    let mut loose = IbdWorkState::new(Vec::new(), Some(gen), Some(0));
    let stray = hash_n(6);
    arm_next(&mut loose, stray, 1);
    let dangling = hash_n(7);
    assert!(!try_enqueue_ordered_header(
        &mut loose, &hub, stray, dangling
    ));
    assert!(loose.ordered.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ordered_header_stops_above_the_next_height() {
    let (dir, hub) = open("enqueue-next");
    let gen = hub.tip_hash().unwrap();
    let mut st = IbdWorkState::new(Vec::new(), Some(gen), Some(0));
    let ahead = hash_n(8);
    arm_next(&mut st, ahead, 3);
    assert!(!try_enqueue_ordered_header(&mut st, &hub, ahead, gen));
    assert!(st.ordered.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn accepted_prefix_is_empty_for_zero_or_one_header() {
    let (dir, hub) = open("prefix-empty");
    hub.ensure_genesis().unwrap();
    let before = hub.query.store().header_count();
    assert!(ensure_accepted_prefix(&hub, &[]).is_empty());
    let one = header_on(hub.tip_hash().unwrap(), 1);
    assert!(ensure_accepted_prefix(&hub, &[one]).is_empty());
    assert_eq!(hub.query.store().header_count(), before);
    let _ = std::fs::remove_dir_all(dir);
}

fn fresh_ask(
    hub: &rbitcoin_net::chain::ChainHub,
    peer_height: u32,
    lag: u32,
) -> (IbdWorkState, mpsc::UnboundedReceiver<PeerCmd>) {
    let (slot, rx) = peer(1, peer_height);
    let gen = hub.tip_hash();
    let mut st = IbdWorkState::new(vec![slot], gen, hub.tip_height());
    st.max_peer_height = lag;
    (st, rx)
}

#[allow(clippy::cognitive_complexity)] // one empty-headers session, one arm per lag and streak
#[test]
fn empty_headers_latch_and_ask() {
    let (dir, hub) = open("empty-headers");

    let (mut st, mut rx) = fresh_ask(&hub, 20, 10);
    let queued = hash_n(20);
    st.ordered.push_back(queued);
    st.ordered_set.insert(queued);
    st.empty_header_streak = 1;
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(!st.headers_done);
    assert!(!asked(&mut rx));

    let (mut st, _rx) = fresh_ask(&hub, 20, 10);
    assert!(on_empty_headers(&mut st, &hub));
    assert!(!st.headers_done);

    let (mut st, mut rx) = fresh_ask(&hub, 20, 2);
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(asked(&mut rx));
    assert!(!st.headers_done);

    let (mut st, mut rx) = fresh_ask(&hub, 20, 2);
    st.empty_header_streak = 6;
    st.inflight.insert(hash_n(23), InflightReq::new(1));
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(asked(&mut rx));
    assert!(!st.headers_done);

    let (mut st, mut rx) = fresh_ask(&hub, 20, 2);
    st.empty_header_streak = 7;
    st.inflight.insert(hash_n(21), InflightReq::new(1));
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(st.headers_done);
    assert!(!asked(&mut rx));

    let (mut st, mut rx) = fresh_ask(&hub, 20, 0);
    for i in 0..ORDERED_HEADERS_SOFT_CAP {
        st.ordered_set.insert(hash_n(1_000_000 + i as u32));
    }
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(!asked(&mut rx));
    assert!(!st.headers_done);
    st.ordered_set.insert(hash_n(2));
    st.empty_header_streak = 0;
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(!asked(&mut rx));

    let (mut st, mut rx) = fresh_ask(&hub, 20, 0);
    st.ordered_set.insert(hash_n(3));
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(asked(&mut rx));

    let (mut st, mut rx) = fresh_ask(&hub, 20, 0);
    for i in 0..ORDERED_HEADERS_SOFT_CAP {
        st.ordered_set.insert(hash_n(3_000_000 + i as u32));
    }
    st.empty_header_streak = 6;
    st.inflight.insert(hash_n(22), InflightReq::new(1));
    assert!(!on_empty_headers(&mut st, &hub));
    assert!(!st.headers_done);
    assert!(!asked(&mut rx));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ordered_live_header_room_bounds() {
    let soft = ORDERED_HEADERS_SOFT_CAP;
    let max = MAX_ORDERED_HEADERS;
    assert!(ordered_live_has_header_room(0, false));
    assert!(ordered_live_has_header_room(soft - 1, false));
    assert!(!ordered_live_has_header_room(soft, false));
    assert!(ordered_live_has_header_room(soft, true));
    assert!(!ordered_live_has_header_room(soft + 1, false));
    assert!(ordered_live_has_header_room(max - 1, true));
    assert!(!ordered_live_has_header_room(max, true));
    assert!(!ordered_live_has_header_room(max + 1, true));

    let (dir, hub) = open("stored-room");
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let mut inflight = InFlight::capture(&st);
    assert!(complete_stored_headers(
        &mut st,
        &hub,
        &mut inflight,
        1,
        MAX_HEADERS_RESULTS
    ));
    assert!(!complete_stored_headers(&mut st, &hub, &mut inflight, 1, 1));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn known_batch_asks_only_for_a_full_window() {
    let (dir, hub) = open("known-window");
    let (mut st, mut rx) = fresh_ask(&hub, 20, 0);
    for i in 0..4 {
        st.ordered_set.insert(hash_n(30 + i));
    }
    on_known_headers_batch(&mut st, &hub, 1, MAX_HEADERS_RESULTS - 1);
    assert!(!asked(&mut rx));
    on_known_headers_batch(&mut st, &hub, 1, MAX_HEADERS_RESULTS);
    assert!(asked(&mut rx));
    let _ = std::fs::remove_dir_all(dir);
}

fn offer_framed(
    st: &mut IbdWorkState,
    hub: &rbitcoin_net::chain::ChainHub,
    hash: BlockHash,
    height: u32,
    payload: &[u8],
    write_next: u32,
) {
    st.inflight.insert(hash, InflightReq::new(0));
    st.record_height(hash, height);
    st.header_fks.insert(hash, Fk(1));
    let next = AtomicU32::new(write_next);
    apply_block_framed(
        st,
        hub,
        &next,
        None,
        FramedBody {
            peer: 0,
            hash,
            payload: payload.to_vec(),
            header_fk: Some(Fk(1)),
        },
    );
}

#[test]
fn framed_body_keeps_the_height_window_and_a_side_block() {
    let (dir, hub) = open("framed-window");
    let gen = hub.tip_hash().unwrap();
    let parent = mine(gen, 1_600_000_100, 1);
    hub.accept_block(parent.clone()).unwrap();
    assert_eq!(hub.tip_height(), Some(1));
    let payload = serialize(&parent);
    let ahead = rbitcoin_ibd::ibd::CONTIG_DENSIFY_AHEAD;

    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let over = hash_n(40);
    offer_framed(&mut st, &hub, over, 1 + ahead + 1, &payload, 1);
    assert!(st.body.is_missing(&over));
    assert!(!hub.query.block_queue_has_hash(&over.to_byte_array()));

    let at_densify = hash_n(41);
    offer_framed(&mut st, &hub, at_densify, 1 + ahead + 1, &payload, 2);
    assert!(hub.query.block_queue_has_hash(&at_densify.to_byte_array()));
    assert!(!st.body.is_missing(&at_densify));

    let at_tip_hi = hash_n(42);
    offer_framed(&mut st, &hub, at_tip_hi, 1 + ahead, &payload, 0);
    assert!(hub.query.block_queue_has_hash(&at_tip_hi.to_byte_array()));

    let on_densify = hash_n(44);
    let on_densify_h = 3 + ahead;
    offer_framed(&mut st, &hub, on_densify, on_densify_h, &payload, 3);
    assert!(hub.query.block_queue_has_hash(&on_densify.to_byte_array()));
    assert!(!st.body.is_missing(&on_densify));
    assert!(!st.reorg.held_bodies.contains(&parent.block_hash()));

    let rejected = hash_n(43);
    st.body.mark_rejected(rejected);
    offer_framed(&mut st, &hub, rejected, 2, &payload, 0);
    assert!(!hub.query.block_queue_has_hash(&rejected.to_byte_array()));

    let genesis_block = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    let genesis = genesis_block.block_hash();
    assert_eq!(genesis, gen);
    offer_framed(&mut st, &hub, genesis, 0, &serialize(&genesis_block), 0);
    assert!(!st.reorg.held_bodies.contains(&genesis));
    assert!(!hub.query.block_queue_has_hash(&genesis.to_byte_array()));

    let side = mine(gen, 1_600_000_200, 1);
    let side_hash = side.block_hash();
    let side_bytes = serialize(&side);
    offer_framed(&mut st, &hub, side_hash, 1, &side_bytes, 0);
    assert!(st.reorg.held_bodies.contains(&side_hash));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn probe_requires_confirmed_prev_and_records_an_empty_tip() {
    let (dir, hub) = open("probe-shape");
    let (slot, _rx) = peer(0, 10);
    let gen = hub.tip_hash().unwrap();
    let mut st = IbdWorkState::new(vec![slot], Some(gen), Some(0));
    st.header_walk.probe_open = Some(0);
    st.header_walk.probe_top = None;
    let (_mark, plan) = plan_header_batch(&mut st, &hub, 0, Vec::new());
    assert!(matches!(plan, HeaderPlan::Done));
    assert!(st.header_walk.probe_open.is_none());
    assert!(st.slots[0].chain.left_path_by(&st.height_to_hash, 1));

    let (slot, _rx) = peer(0, 10);
    let mut st = IbdWorkState::new(vec![slot], Some(gen), Some(0));
    st.header_walk.probe_open = Some(0);
    st.header_walk.probe_top = None;
    hub.query.fail_next_header_read_cancelled();
    let hdr = header_on(gen, 9);
    let (_mark, _plan) = plan_header_batch(&mut st, &hub, 0, vec![hdr]);
    assert_eq!(st.header_walk.probe_open, Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unproved_probe_witnesses_another_peers_checkpoint() {
    let (dir, hub) = open("probe-witness");
    let (slot, _rx) = peer(0, 10);
    let mine = slot.net;
    let gen = hub.tip_hash().unwrap();
    let headers = chain_on(gen, MAX_HEADERS_RESULTS);
    let mut st = IbdWorkState::new(vec![slot], Some(gen), Some(0));
    st.header_walk.origin = true;
    st.header_walk.proven = false;
    st.header_walk.probe_open = Some(0);
    st.header_walk.probe_top = None;
    st.header_walk.lead_class_a.insert(1, 1);
    let other = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 2, 0, 2)), 8333);
    st.header_walk.checkpoints.push(Checkpoint {
        hash: headers[0].block_hash(),
        height: 1,
        height_wide: 1,
        work: [0; 32],
        header: Some(headers[0]),
        times: Vec::new(),
        diff: Default::default(),
        supplied_by: Some(rbitcoin_net::NetAddr::from_socket(other)),
    });
    assert_ne!(st.header_walk.checkpoints[0].supplied_by, Some(mine));
    let (_mark, plan) = plan_header_batch(&mut st, &hub, 0, headers);
    assert!(matches!(plan, HeaderPlan::Done));
    assert_eq!(st.header_walk.witness_height, Some(1));
    assert!(!st.header_walk.lead_class_a.contains_key(&1));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cascade_batch_retries_one_block() {
    let (dir, hub) = open("cascade-batch");
    let hash = hash_n(50);
    let mut single = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let one = ConfirmFeed::new();
    apply_confirm_reject(
        &mut single,
        10,
        hash,
        ConfirmRejectClass::Cascade,
        "fk mismatch",
        None,
        Some(&hub),
        1,
        Some(&one),
        None,
        RejectParent::default(),
    );
    assert!(!one.single_block());

    let mut wide = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let feed = ConfirmFeed::new();
    apply_confirm_reject(
        &mut wide,
        10,
        hash,
        ConfirmRejectClass::Cascade,
        "fk mismatch",
        None,
        Some(&hub),
        2,
        Some(&feed),
        None,
        RejectParent::default(),
    );
    assert!(feed.single_block());
    assert_eq!(feed.isolate_until(), 11);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tip_child_bad_prev_halts_only_for_the_same_hash() {
    let (dir, hub) = open("tip-child");
    let tip = hub.tip_hash().unwrap();
    let err = "unexpected previous header";
    let hash = hash_n(60);
    let mut st = IbdWorkState::new(Vec::new(), Some(tip), Some(0));
    st.note_path_prev(hash, tip);
    for _ in 0..2 {
        apply_confirm_reject(
            &mut st,
            1,
            hash,
            ConfirmRejectClass::SoftWire,
            err,
            None,
            Some(&hub),
            1,
            None,
            None,
            RejectParent::default(),
        );
        assert!(st.halt.is_none());
    }
    apply_confirm_reject(
        &mut st,
        1,
        hash,
        ConfirmRejectClass::SoftWire,
        err,
        None,
        Some(&hub),
        1,
        None,
        None,
        RejectParent::default(),
    );
    assert!(st.halt.is_some());

    let mut other = IbdWorkState::new(Vec::new(), Some(tip), Some(0));
    for i in 0..3 {
        let hash = hash_n(70 + i);
        other.note_path_prev(hash, tip);
        apply_confirm_reject(
            &mut other,
            1,
            hash,
            ConfirmRejectClass::SoftWire,
            err,
            None,
            Some(&hub),
            1,
            None,
            None,
            RejectParent::default(),
        );
    }
    assert!(other.halt.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mutated_clear_punishes_only_an_off_path_sender() {
    let (dir, hub) = open("punish-mutated");
    let tip = hub.tip_hash().unwrap();
    let (slot, mut rx) = peer(1, 10);
    let mut st = IbdWorkState::new(vec![slot], Some(tip), Some(0));
    apply_confirm_reject(
        &mut st,
        4,
        hash_n(80),
        ConfirmRejectClass::SoftWire,
        "bad-txnmrklroot",
        None,
        Some(&hub),
        1,
        None,
        Some(1),
        RejectParent::default(),
    );
    assert!(!st.slots[0].alive);
    assert!(matches!(rx.try_recv(), Ok(PeerCmd::Shutdown)));

    let (slot, mut rx) = peer(1, 10);
    let mut st = IbdWorkState::new(vec![slot], Some(tip), Some(0));
    let hash = hash_n(81);
    st.note_path_prev(hash, tip);
    apply_confirm_reject(
        &mut st,
        1,
        hash,
        ConfirmRejectClass::SoftWire,
        "unexpected previous merkle root",
        None,
        Some(&hub),
        1,
        None,
        Some(1),
        RejectParent::default(),
    );
    assert!(st.slots[0].alive);
    assert!(rx.try_recv().is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn soft_reject_defers_clear_only_for_a_mutated_body_with_a_query() {
    let (dir, hub) = open("soft-defer");
    let mut st = IbdWorkState::new(Vec::new(), hub.tip_hash(), hub.tip_height());
    let hash = hash_n(90);
    let deferred = apply_soft_wire_reject(
        &mut st,
        3,
        hash,
        "bad-txnmrklroot",
        Some(hub.query.as_ref()),
        Some(&hub),
        None,
    );
    assert!(deferred.is_some());
    assert!(!st.body.is_missing(&hash));

    let plain = hash_n(91);
    let finished = apply_soft_wire_reject(
        &mut st,
        3,
        plain,
        "soft wire",
        Some(hub.query.as_ref()),
        Some(&hub),
        None,
    );
    assert!(finished.is_none());
    assert!(st.body.is_missing(&plain));

    let no_query = hash_n(92);
    let finished =
        apply_soft_wire_reject(&mut st, 3, no_query, "bad-txnmrklroot", None, None, None);
    assert!(finished.is_none());
    assert!(st.body.is_missing(&no_query));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn off_path_suffix_drops_only_hashes_above_the_keep() {
    let keep_h = hash_n(100);
    let named = hash_n(101);
    let higher = hash_n(102);
    let below = hash_n(103);
    let unknown = hash_n(104);
    let mut st = IbdWorkState::new(Vec::new(), None, None);
    plant(&mut st, keep_h, 4);
    plant(&mut st, named, 5);
    plant(&mut st, higher, 6);
    plant(&mut st, below, 3);
    for hash in [keep_h, named, higher, below, unknown] {
        st.body.mark_pending(hash);
    }
    st.ordered.push_back(unknown);
    st.ordered.push_back(below);
    st.ordered.push_back(keep_h);
    st.ordered_set.insert(unknown);
    st.ordered_set.insert(below);
    st.ordered_set.insert(keep_h);
    let cleared = apply_soft_wire_reject(
        &mut st,
        5,
        named,
        "unexpected previous header",
        None,
        None,
        None,
    );
    assert!(cleared.is_none());
    assert!(!st.body.is_pending(&named));
    assert!(!st.body.is_pending(&higher));
    assert!(!st.body.is_pending(&unknown));
    assert!(st.body.is_pending(&keep_h));
    assert!(st.body.is_pending(&below));
}

#[test]
fn cascade_halts_only_for_the_same_hash_at_the_same_tip() {
    let hash = hash_n(110);
    let mut st = IbdWorkState::new(Vec::new(), None, None);
    for _ in 0..2 {
        apply_cascade_reject(&mut st, 4, hash, "fk mismatch", None);
        assert!(st.halt.is_none());
    }
    apply_cascade_reject(&mut st, 4, hash, "fk mismatch", None);
    assert!(st
        .halt
        .as_deref()
        .is_some_and(|halt| halt.contains("cascade repeated")));

    let mut st = IbdWorkState::new(Vec::new(), None, None);
    for i in 0..3 {
        apply_cascade_reject(&mut st, 4, hash_n(120 + i), "fk mismatch", None);
    }
    assert!(st.halt.is_none());
}

#[test]
fn consensus_invalid_drops_its_height_and_sticks_without_a_clean_child() {
    let (dir, hub) = open("consensus-invalid");
    let tip = hub.tip_hash();
    let height = hub.tip_height();

    let mut st = IbdWorkState::new(Vec::new(), tip, height);
    let other = hash_n(130);
    let failed = hash_n(131);
    plant(&mut st, other, 4);
    apply_consensus_invalid_reject(
        &mut st,
        4,
        failed,
        "bad block",
        Some(hub.query.as_ref()),
        Some(&hub),
    );
    assert_eq!(st.height_to_hash.get(&4).copied(), Some(other));

    let mut st = IbdWorkState::new(Vec::new(), tip, height);
    let failed = hash_n(132);
    plant(&mut st, failed, 5);
    apply_consensus_invalid_reject(
        &mut st,
        5,
        failed,
        "bad block",
        Some(hub.query.as_ref()),
        Some(&hub),
    );
    assert!(st.height_to_hash.get(&5).is_none());
    assert!(st.body.is_rejected(&failed));

    let mut st = IbdWorkState::new(Vec::new(), tip, height);
    let child = hash_n(133);
    plant(&mut st, child, 1);
    apply_consensus_invalid_reject(
        &mut st,
        9,
        hash_n(134),
        "bad block",
        Some(hub.query.as_ref()),
        Some(&hub),
    );
    assert!(st.confirm_stuck_since.is_none());
    assert_eq!(st.height_to_hash.get(&1).copied(), Some(child));

    let mut st = IbdWorkState::new(Vec::new(), tip, height);
    let child = hash_n(135);
    plant(&mut st, child, 1);
    st.reorg.invalid.mark(child.to_byte_array());
    apply_consensus_invalid_reject(
        &mut st,
        9,
        hash_n(136),
        "bad block",
        Some(hub.query.as_ref()),
        Some(&hub),
    );
    assert!(st.confirm_stuck_since.is_some());
    let _ = std::fs::remove_dir_all(dir);
}
