use rbitcoin_ibd::ibd::exit::*;
use rbitcoin_ibd::ibd::state::IbdWorkState;
use std::time::Duration;

#[test]
fn redial_budget_spends_only_dark_empty_rounds_and_waits_out_a_started_dial() {
    assert_eq!(
        dark_empty_after(MAX_DARK_EMPTY_REDIALS - 1, 0, true),
        0,
        "an empty top-up while a peer is up does not spend the dark budget"
    );
    assert_eq!(dark_empty_after(MAX_DARK_EMPTY_REDIALS, 1, false), 0);

    let mut dark = 0;
    for _ in 0..MAX_DARK_EMPTY_REDIALS {
        assert!(may_spawn_redial(0, dark, Peerless::GiveUp));
        dark = dark_empty_after(dark, 0, false);
    }
    assert!(
        !may_spawn_redial(0, dark, Peerless::GiveUp),
        "dark empty rounds stop the next dial"
    );
    assert!(
        may_spawn_redial(1, dark, Peerless::GiveUp),
        "a live peer still tops up"
    );

    let mut st = IbdWorkState::new(Vec::new(), None, Some(21_792));
    st.max_peer_height = 326_676;
    assert_eq!(
        all_peers_dead_action(&st, 21_792, true, Peerless::GiveUp),
        AllPeersDead::WaitRedial,
        "a dial already started is waited out"
    );
    assert_eq!(
        all_peers_dead_action(&st, 21_792, false, Peerless::GiveUp),
        AllPeersDead::GiveUpMidCatchup
    );

    let redial = Peerless::Redial {
        cap: PEERLESS_REDIAL_CAP,
    };
    assert!(
        may_spawn_redial(0, u32::MAX, redial),
        "redial keeps dialing however many dark rounds came back empty"
    );
    assert_eq!(
        all_peers_dead_action(&st, 21_792, false, redial),
        AllPeersDead::WaitRedial,
        "redial waits mid catch-up between dial rounds"
    );
    st.max_peer_height = 21_792;
    st.max_ready_height = 21_792;
    st.headers_done = true;
    assert_eq!(
        all_peers_dead_action(&st, 21_792, false, redial),
        AllPeersDead::CatchupComplete,
        "a caught-up catch-up still completes without peers"
    );

    let delays = [0, 1, 2, 3, 7, 8, u32::MAX].map(|dark| redial.dark_delay(dark).as_secs());
    assert_eq!(delays, [0, 1, 2, 4, 60, 60, 60]);
    assert_eq!(Peerless::GiveUp.dark_delay(3), Duration::ZERO);
}

/// all_peers_dead + catchup_complete edge matrix (mid-chain / caught-up / tip-0).
#[allow(clippy::cognitive_complexity)] // one fixture, many exit arms
#[test]
fn exit_and_catchup_complete_surface() {
    // Mid-chain, all dead, no redial → give up.
    let mut mid = IbdWorkState::new(Vec::new(), None, Some(161_249));
    mid.max_peer_height = 958_820;
    mid.max_ready_height = 161_000;
    assert_eq!(
        all_peers_dead_action(&mid, 161_249, false, Peerless::GiveUp),
        AllPeersDead::GiveUpMidCatchup
    );
    assert_eq!(
        all_peers_dead_action(&mid, 161_249, true, Peerless::GiveUp),
        AllPeersDead::WaitRedial
    );

    // Caught up with no peers → complete.
    let mut done = IbdWorkState::new(Vec::new(), None, Some(100));
    done.max_peer_height = 100;
    done.max_ready_height = 100;
    done.headers_done = true;
    assert_eq!(
        all_peers_dead_action(&done, 100, false, Peerless::GiveUp),
        AllPeersDead::CatchupComplete
    );

    // Path drain + empty-EOF completes even if advertised height is far
    // ahead (less-work fork / bogus LastBlock). Still asking if not EOF.
    let mut path = IbdWorkState::new(Vec::new(), None, Some(2000));
    path.max_peer_height = 313_000;
    path.max_ready_height = 2000;
    assert!(!ibd_caught_up(&path, 2000));
    path.headers_done = true;
    assert!(ibd_caught_up(&path, 2000));
    path.max_peer_height = 2001;
    assert!(ibd_caught_up(&path, 2000));

    // Regression: tip=0 + peer horizon must not look complete (false tip mode).
    let mut zero = IbdWorkState::new(Vec::new(), None, Some(0));
    zero.max_peer_height = 958_900;
    zero.max_ready_height = 0;
    zero.headers_done = false;
    assert!(!ibd_caught_up(&zero, 0));
    assert!(!ibd_caught_up(&zero, 0));
    assert_eq!(
        all_peers_dead_action(&zero, 0, false, Peerless::GiveUp),
        AllPeersDead::GiveUpMidCatchup
    );

    // path_drained false when ordered is non-empty.
    let mut busy = IbdWorkState::new(Vec::new(), None, Some(10));
    busy.max_peer_height = 10;
    busy.max_ready_height = 10;
    busy.headers_done = true;
    assert!(path_drained(&busy));
    use bitcoin::hashes::Hash;
    use bitcoin::BlockHash;
    busy.ordered
        .push_back(BlockHash::from_byte_array([1u8; 32]));
    assert!(!path_drained(&busy));
    assert!(!ibd_caught_up(&busy, 10));

    // ibd_caught_up: tip at/within 1 of horizon and not behind archive.
    let mut near = IbdWorkState::new(Vec::new(), None, Some(100));
    near.max_peer_height = 101;
    near.max_ready_height = 100;
    near.headers_done = true;
    assert!(ibd_caught_up(&near, 100));
    // Lag 0 is caught up even when the empty-headers latch is still open
    // and the walk has no proved origin. A larger gap stays open with it.
    let mut level = IbdWorkState::new(Vec::new(), None, Some(100));
    level.max_peer_height = 100;
    level.max_ready_height = 100;
    level.headers_done = false;
    assert!(ibd_caught_up(&level, 100));
    level.max_peer_height = 101;
    assert!(!ibd_caught_up(&level, 100));
    near.max_ready_height = 105;
    assert!(!ibd_caught_up(&near, 100));
    assert_eq!(header_lag_behind_peers(&near, 100), 0); // archived ≥ peer

    // Mainnet 08:16:23: ordered empty, headers_done, tip=horizon, 7 off-path
    // inflight. Remainder ignores leftover getdata — prune is hygiene only.
    use rbitcoin_ibd::ibd::assign::prune_off_path_inflight;
    use rbitcoin_ibd::ibd::state::InflightReq;
    let mut at_horizon = IbdWorkState::new(Vec::new(), None, Some(964_108));
    at_horizon.max_peer_height = 964_108;
    at_horizon.max_ready_height = 964_108;
    at_horizon.headers_done = true;
    for i in 1u8..=7 {
        at_horizon
            .inflight
            .insert(BlockHash::from_byte_array([i; 32]), InflightReq::new(0));
    }
    assert!(path_drained(&at_horizon));
    assert!(ibd_caught_up(&at_horizon, 964_108));
    prune_off_path_inflight(&mut at_horizon);
    assert!(at_horizon.inflight.is_empty());
    assert!(path_drained(&at_horizon));
    assert!(ibd_caught_up(&at_horizon, 964_108));

    // Mid-chain on-path inflight (tip+1 in h2h) must not complete after prune.
    let mut mid_inf = IbdWorkState::new(Vec::new(), None, Some(161_249));
    mid_inf.max_peer_height = 958_820;
    mid_inf.max_ready_height = 161_000;
    mid_inf.headers_done = true;
    let on_path = BlockHash::from_byte_array([0x2a; 32]);
    mid_inf.record_height(on_path, 161_250);
    mid_inf.inflight.insert(on_path, InflightReq::new(0));
    prune_off_path_inflight(&mut mid_inf);
    assert!(mid_inf.inflight.contains_key(&on_path));
    assert!(!path_drained(&mid_inf));
    assert!(!ibd_caught_up(&mid_inf, 161_249));
}

/// Empty-headers lag WARN/reget cadence (mainnet log flood regression).
#[allow(clippy::cognitive_complexity)] // one fixture, many lag cadence arms
#[test]
fn empty_headers_lag_rate_limits() {
    assert!(should_log_empty_headers_lag(1));
    assert!(!should_log_empty_headers_lag(2));
    assert!(!should_log_empty_headers_lag(8));
    assert!(!should_log_empty_headers_lag(16));
    assert!(should_log_empty_headers_lag(64));
    assert!(should_log_empty_headers_lag(128));
    // 21 peers × 8 empties would have logged ~5× if streak reset every 8.
    let logs: u32 = (1..=200)
        .filter(|&s| should_log_empty_headers_lag(s))
        .count() as u32;
    assert!(logs <= 5, "expected sparse logs in 200 empties, got {logs}");

    assert!(should_rerequest_headers_on_empty_lag(1));
    assert!(!should_rerequest_headers_on_empty_lag(2));
    assert!(should_rerequest_headers_on_empty_lag(8));
    assert!(should_rerequest_headers_on_empty_lag(16));
    let regets: u32 = (1..=64)
        .filter(|&s| should_rerequest_headers_on_empty_lag(s))
        .count() as u32;
    assert_eq!(regets, 9); // 1,8,16,...,64

    // Full store reseed is sparser than getheaders (O(headers) walk).
    assert!(should_reseed_work_path_on_empty_lag(1, true));
    assert!(!should_reseed_work_path_on_empty_lag(8, true));
    assert!(should_reseed_work_path_on_empty_lag(64, true));
    let reseeds: u32 = (1..=64)
        .filter(|&s| should_reseed_work_path_on_empty_lag(s, true))
        .count() as u32;
    assert!(
        reseeds < regets,
        "reseed={reseeds} must be rarer than reget={regets}"
    );

    // Live ordered path already has locator tips — do not walk the header graph.
    assert!(!should_reseed_work_path_on_empty_lag(1, false));
    assert!(!should_reseed_work_path_on_empty_lag(64, false));
    assert!(!should_reseed_work_path_on_empty_lag(128, false));

    // Already-known 1-header at tip must not re-getheaders (storm).
    assert!(!should_advance_locator_after_known_batch(0, 0, false, true));
    assert!(!should_advance_locator_after_known_batch(
        0, 0, false, false
    ));
    // Two blocks of lag is still tip chatter. Three blocks advances.
    assert!(!should_advance_locator_after_known_batch(
        1, 2, false, false
    ));
    assert!(should_advance_locator_after_known_batch(1, 3, false, false));
    // Live path + full window still advances.
    assert!(should_advance_locator_after_known_batch(
        8_000, 0, true, false
    ));
    assert!(should_advance_locator_after_known_batch(
        8_000, 10, false, false
    ));

    // Empty path at horizon: no fan (finish sync / SH / tip follow).
    let mut at = IbdWorkState::new(Vec::new(), None, Some(100));
    at.max_peer_height = 100;
    at.max_ready_height = 100;
    assert_eq!(empty_path_header_fan(&at, 100, 29), 0);
    // Near tip with connecting header on path: getdata, not locators.
    let mut near = IbdWorkState::new(Vec::new(), None, Some(100));
    near.max_peer_height = 102;
    near.max_ready_height = 100;
    near.record_height(h(0x2a), 101);
    near.inflight
        .insert(h(0x2a), rbitcoin_ibd::ibd::state::InflightReq::new(0));
    assert_eq!(empty_path_header_fan(&near, 100, 29), 0);
    // Mid-sync empty: fan (cap 4); one peer if on-path getdata already inflight.
    let mut mid = IbdWorkState::new(Vec::new(), None, Some(100));
    mid.max_peer_height = 200;
    mid.max_ready_height = 100;
    assert_eq!(empty_path_header_fan(&mid, 100, 29), 4);
    mid.record_height(h(0x2a), 101);
    mid.inflight
        .insert(h(0x2a), rbitcoin_ibd::ibd::state::InflightReq::new(0));
    assert_eq!(empty_path_header_fan(&mid, 100, 29), 1);
    let mut few = IbdWorkState::new(Vec::new(), None, Some(100));
    few.max_peer_height = 200;
    few.max_ready_height = 100;
    assert_eq!(empty_path_header_fan(&few, 100, 2), 2);
}

fn h(b: u8) -> bitcoin::BlockHash {
    use bitcoin::hashes::Hash;
    bitcoin::BlockHash::from_byte_array([b; 32])
}

/// IBD → tip-follow catch-up matrix (competing headers, explore leftovers, holes).
#[test]
fn ibd_caught_up_transition_matrix() {
    use rbitcoin_ibd::ibd::state::InflightReq;

    // Explore-need inflight at advertised horizon is not best-chain remainder.
    let tip = 964_243;
    let mut explore = IbdWorkState::new(Vec::new(), None, Some(tip));
    explore.max_peer_height = tip;
    explore.max_ready_height = tip;
    explore.headers_done = true;
    for i in 1u8..=7 {
        let hash = h(i);
        explore.inflight.insert(hash, InflightReq::new(0));
        explore
            .reorg
            .register_explore(std::iter::once(hash), Some(hash));
    }
    assert!(
        ibd_caught_up(&explore, tip),
        "explore inflight at horizon must not block catch-up"
    );

    // Orphan getdata at horizon is not remainder (no prune required).
    let mut orphans = IbdWorkState::new(Vec::new(), None, Some(tip));
    orphans.max_peer_height = tip;
    orphans.max_ready_height = tip;
    orphans.headers_done = true;
    for i in 1u8..=7 {
        orphans.inflight.insert(h(i), InflightReq::new(0));
    }
    assert!(
        ibd_caught_up(&orphans, tip),
        "orphan inflight at horizon must not block catch-up"
    );

    // Competing hash_height above tip must not zero lag or complete two short.
    let mut fork = IbdWorkState::new(Vec::new(), None, Some(100));
    fork.max_peer_height = 102;
    fork.max_ready_height = 100;
    fork.headers_done = true;
    fork.hash_height.insert(h(0xee), 110);
    assert_eq!(
        header_lag_behind_peers(&fork, 100),
        2,
        "lag from path high water, not competing hash_height"
    );
    assert!(
        !ibd_caught_up(&fork, 100),
        "empty h2h two short of peers is a header hole, not catch-up"
    );

    // Version LastBlock chatter: one ahead, empty headers, no tip+1 → complete.
    let mut chatter = IbdWorkState::new(Vec::new(), None, Some(100));
    chatter.max_peer_height = 101;
    chatter.max_ready_height = 100;
    chatter.headers_done = true;
    assert!(ibd_caught_up(&chatter, 100));

    // Live awaiting-reorg gather blocks exit even with empty ordered/inflight.
    let mut await_st = IbdWorkState::new(Vec::new(), None, Some(100));
    await_st.max_peer_height = 100;
    await_st.max_ready_height = 100;
    await_st.headers_done = true;
    await_st.reorg.register_explore([h(0xcc)], None);
    assert!(
        ibd_caught_up(&await_st, 100),
        "explore-need without path remainder must not block catch-up"
    );

    // tip+1 sitting in h2h (not yet ordered) is remainder.
    let mut hole = IbdWorkState::new(Vec::new(), None, Some(100));
    hole.max_peer_height = 100;
    hole.max_ready_height = 100;
    hole.headers_done = true;
    hole.record_height(h(0x2a), 101);
    assert!(!ibd_caught_up(&hole, 100));

    // Mid-chain on-path inflight.
    let mut mid_inf = IbdWorkState::new(Vec::new(), None, Some(161_249));
    mid_inf.max_peer_height = 958_820;
    mid_inf.max_ready_height = 161_000;
    mid_inf.headers_done = true;
    mid_inf.record_height(h(0x2a), 161_250);
    mid_inf.inflight.insert(h(0x2a), InflightReq::new(0));
    assert!(!ibd_caught_up(&mid_inf, 161_249));

    // tip=0 never complete; dead peers give up.
    let mut zero = IbdWorkState::new(Vec::new(), None, Some(0));
    zero.max_peer_height = 958_900;
    zero.max_ready_height = 0;
    assert!(!ibd_caught_up(&zero, 0));
    assert_eq!(
        all_peers_dead_action(&zero, 0, false, Peerless::GiveUp),
        AllPeersDead::GiveUpMidCatchup
    );

    // Dead peers at horizon with empty remainder → complete.
    let mut done = IbdWorkState::new(Vec::new(), None, Some(100));
    done.max_peer_height = 100;
    done.max_ready_height = 100;
    done.headers_done = true;
    assert_eq!(
        all_peers_dead_action(&done, 100, false, Peerless::GiveUp),
        AllPeersDead::CatchupComplete
    );

    // Still asking: inflated start_height, no empty-EOF yet.
    let mut signet = IbdWorkState::new(Vec::new(), None, Some(2000));
    signet.max_peer_height = 313_000;
    signet.max_ready_height = 2000;
    assert!(!ibd_caught_up(&signet, 2000));
    assert!(!rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(
        &signet, 2000
    ));

    // Empty-EOF at the most-work path: extra advertised height is not a
    // tip we chase (less-work high-height fork / bogus LastBlock).
    signet.headers_done = true;
    signet.header_walk.prove_through(2000);
    assert!(
        ibd_caught_up(&signet, 2000),
        "headers_done at path HWM completes even if start_height is far ahead"
    );
    assert!(
        !rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(&signet, 2000),
        "do not unlatch a 900-block less-work height claim"
    );
}

/// Empty path with a connecting-header hole must keep getheaders, not latch.
#[test]
fn header_fan_unlatch_at_path_hole() {
    let mut hole = IbdWorkState::new(Vec::new(), None, Some(100));
    hole.max_peer_height = 102;
    hole.max_ready_height = 100;
    hole.headers_done = true;
    hole.hash_height.insert(h(0xee), 110);
    let lag = header_lag_behind_peers(&hole, 100);
    assert_eq!(lag, 2, "competing hash_height must not hide a 2-block hole");
    assert!(
        empty_path_header_fan(&hole, 100, 29) >= 1,
        "empty h2h with peers ahead must fan getheaders"
    );
    assert!(
        rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(&hole, 100),
        "headers_done must unlatch while the connecting header is missing"
    );

    // True horizon, no hole: do not storm locators.
    let mut at = IbdWorkState::new(Vec::new(), None, Some(100));
    at.max_peer_height = 100;
    at.max_ready_height = 100;
    assert_eq!(empty_path_header_fan(&at, 100, 29), 0);
    at.headers_done = true;
    assert!(!rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(
        &at, 100
    ));
}

/// An unproved empty queue unlatches for any real peer lead. A proved
/// walk does not chase a far start height.
#[test]
fn should_unlatch_headers_done() {
    use bitcoin::hashes::Hash;
    use bitcoin::BlockHash;
    use rbitcoin_ibd::ibd::state::InflightReq;

    let mut st = IbdWorkState::new(Vec::new(), None, Some(100));
    st.max_peer_height = 105;
    st.max_ready_height = 100;
    st.headers_done = true;
    for i in 1u8..=7 {
        st.inflight
            .insert(BlockHash::from_byte_array([i; 32]), InflightReq::new(0));
    }
    assert_eq!(header_lag_behind_peers(&st, 100), 5);
    assert!(
        rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(&st, 100),
        "an unproved latch asks again when a peer is more than two blocks ahead"
    );
    st.header_walk.prove_through(100);
    assert!(
        !rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(&st, 100),
        "a proved walk does not chase a far start height"
    );
    st.header_walk = rbitcoin_ibd::ibd::header_walk::HeaderWalk::default();

    st.max_peer_height = 100;
    assert_eq!(header_lag_behind_peers(&st, 100), 0);
    assert!(!rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(
        &st, 100
    ));

    st.max_peer_height = 105;
    st.ordered.push_back(BlockHash::from_byte_array([0xab; 32]));
    assert!(!rbitcoin_ibd::ibd::exit::should_unlatch_headers_done(
        &st, 100
    ));
}
