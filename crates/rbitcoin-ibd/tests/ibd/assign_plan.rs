use bitcoin::hashes::Hash;
use bitcoin::BlockHash;
use rbitcoin_ibd::ibd::assign_plan::*;
use std::collections::{HashSet, VecDeque};

fn h(n: u16) -> BlockHash {
    let mut b = [0u8; 32];
    b[0] = (n & 0xff) as u8;
    b[1] = (n >> 8) as u8;
    BlockHash::from_byte_array(b)
}

/// far_slots / feed_cap scale / soft-cap density (pure policy helpers).
#[test]
fn assign_policy_helpers_surface() {
    assert_eq!(far_slots_per_peer(16, true), 0);
    assert_eq!(far_slots_per_peer(16, false), 8);
    assert_eq!(far_slots_per_peer(8, false), 4);

    // Sparse: 4k claim-ready of 120k live → no bypass
    assert!(!want_headers_beyond_soft_cap(120_000, 4_000, 100, 2048));
    // Dense + short ready_gap → bypass
    assert!(want_headers_beyond_soft_cap(64_000, 50_000, 100, 2048));
    // Dense but long ready_gap → no need
    assert!(!want_headers_beyond_soft_cap(64_000, 50_000, 10_000, 2048));
    // ready_gap == gap_need is not short enough (`<`, not `<=`).
    assert!(!want_headers_beyond_soft_cap(64_000, 50_000, 2048, 2048));
    // Empty path is not soft-cap headroom (tip 1-header storm).
    assert!(!want_headers_beyond_soft_cap(0, 0, 0, 2048));
    let dense = want_headers_beyond_soft_cap(64_000, 50_000, 100, 2048);
    assert!(need_ready_headroom(dense, false));
    assert!(!need_ready_headroom(dense, true));
    assert!(!need_ready_headroom(false, false));
}

#[test]
fn densify_slots_tip_hole_is_zero_for_all() {
    assert_eq!(
        densify_slots_for_peer(16, true, Some(10_000_000), Some(1_000_000), false),
        0
    );
    assert_eq!(densify_slots_for_peer(16, true, None, None, true), 0);
}

#[test]
fn densify_slots_tight_pack_stays_half() {
    assert_eq!(
        densify_slots_for_peer(16, false, Some(1_500_000), Some(1_000_000), true),
        8
    );
    assert_eq!(
        densify_slots_for_peer(16, false, Some(2_000_000), Some(1_000_000), true),
        8
    );
}

#[test]
fn densify_slots_fast_outlier_gets_full() {
    assert_eq!(
        densify_slots_for_peer(16, false, Some(2_000_000), Some(1_000_000), false),
        16
    );
    assert_eq!(
        densify_slots_for_peer(16, false, Some(1_500_000), Some(1_000_000), false),
        8
    );
    assert_eq!(
        densify_slots_for_peer(16, false, None, Some(1_000_000), false),
        8
    );
    assert_eq!(
        densify_slots_for_peer(16, false, Some(2_000_000), None, false),
        8
    );
}

/// 292k re-admit vs tip-chatter skip.
#[test]
fn enqueue_header_readmit_skips_inflight_pending_and_past_tip() {
    // Known, drained, still needed above tip.
    assert!(should_enqueue_header(
        false,
        false,
        false,
        false,
        false,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        true,
        false,
        false,
        false,
        false,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        false,
        true,
        false,
        false,
        false,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        false,
        false,
        true,
        false,
        false,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        false,
        false,
        false,
        true,
        false,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        false,
        false,
        false,
        false,
        true,
        Some(1),
        Some(0),
    ));
    assert!(!should_enqueue_header(
        false,
        false,
        false,
        false,
        false,
        Some(5),
        Some(5),
    ));
    assert!(!should_enqueue_header(
        false,
        false,
        false,
        false,
        false,
        Some(4),
        Some(5),
    ));
}

/// ordered set remove + compact ghosts (one surface).
#[test]
fn ordered_set_remove_and_compact_surface() {
    let mut ordered: VecDeque<_> = [h(1), h(2), h(3)].into_iter().collect();
    let mut set: HashSet<_> = ordered.iter().copied().collect();
    remove_from_ordered(&mut ordered, &mut set, h(1));
    assert_eq!(ordered.front().copied(), Some(h(2)));
    assert!(!set.contains(&h(1)));

    let mut ordered: VecDeque<_> = [h(1), h(2), h(3)].into_iter().collect();
    let mut set: HashSet<_> = ordered.iter().copied().collect();
    remove_from_ordered(&mut ordered, &mut set, h(2));
    assert_eq!(ordered.len(), 3);
    assert!(!set.contains(&h(2)));
    remove_from_ordered(&mut ordered, &mut set, h(1));
    assert_eq!(ordered.front().copied(), Some(h(3)));
    assert_eq!(ordered.len(), 1);

    let mut ordered = VecDeque::new();
    let mut set = HashSet::new();
    for i in 1u16..=100 {
        ordered.push_back(h(i));
        set.insert(h(i));
        ordered.push_back(h(1000 + i));
    }
    assert_eq!(ordered.len(), 200);
    compact_ordered(&mut ordered, &set);
    assert_eq!(ordered.len(), 100);
    for x in &ordered {
        assert!(set.contains(x));
    }

    // Small deques are left alone (compact threshold).
    let mut ordered: VecDeque<_> = [h(1), h(2)].into_iter().collect();
    let set: HashSet<_> = [h(1)].into_iter().collect();
    compact_ordered(&mut ordered, &set);
    assert_eq!(ordered.len(), 2);

    // Ghost budget: live=80, len=100 → within budget, no compact.
    let mut ordered = VecDeque::new();
    let mut set = HashSet::new();
    for i in 1u16..=80 {
        ordered.push_back(h(i));
        set.insert(h(i));
    }
    for i in 1u16..=20 {
        ordered.push_back(h(2000 + i)); // ghosts
    }
    let before = ordered.len();
    compact_ordered(&mut ordered, &set);
    assert_eq!(ordered.len(), before);

    // Ghost budget is half the live set once that half exceeds 64.
    // live=200 → budget 100; 80 ghosts stay. A remainder budget of 64 compacts.
    let mut ordered = VecDeque::new();
    let mut set = HashSet::new();
    for i in 1u16..=200 {
        ordered.push_back(h(i));
        set.insert(h(i));
    }
    for i in 1u16..=80 {
        ordered.push_back(h(3000 + i));
    }
    assert_eq!(ordered.len(), 280);
    compact_ordered(&mut ordered, &set);
    assert_eq!(ordered.len(), 280);

    // remove front chain of ghosts after middle remove already did set.
    let mut ordered: VecDeque<_> = [h(1), h(2), h(3), h(4)].into_iter().collect();
    let mut set: HashSet<_> = [h(3), h(4)].into_iter().collect();
    remove_from_ordered(&mut ordered, &mut set, h(3));
    // Front 1,2 not in set → popped; 3 removed; 4 remains.
    assert_eq!(ordered.front().copied(), Some(h(4)));
    assert_eq!(ordered.len(), 1);

    assert_eq!(far_slots_per_peer(1, false), 1); // max(0,1)=1
    assert_eq!(far_slots_per_peer(3, false), 1);
}
