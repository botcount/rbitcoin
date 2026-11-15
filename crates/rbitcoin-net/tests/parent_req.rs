use std::collections::BTreeSet;

use rbitcoin_net::tx_relay::parent_req::*;
use rbitcoin_net::tx_relay::{GETDATA_TX_INTERVAL_SECS, MAX_PARENTS_PER_PARK};

#[test]
fn txid_parent_already_have_does_not_follow_another_peers_wtxid() {
    let mut t = ParentTracker::new();
    let hash = [0xcd; 32];
    t.schedule(hash, 2, true, 1_000);
    assert_eq!(
        t.note_inv(1, hash, false, 1_000, true),
        ParentNote::RequestNow
    );
    assert_eq!(
        t.note_inv(3, hash, false, 1_000, true),
        ParentNote::Deferred
    );
    let now = 1_000 + GETDATA_TX_INTERVAL_SECS;
    let mut saw_txid = false;
    let dropped = t.take_due(2, now, |h, wtxid| {
        assert_eq!(*h, hash);
        assert!(!wtxid, "txid parent is checked as a txid");
        saw_txid = true;
        true
    });
    assert!(saw_txid);
    assert!(
        dropped.is_empty(),
        "a txid parent already in the mempool is not requested"
    );
    let follow = t.take_due(3, now, |_, wtxid| {
        assert!(wtxid, "wtxid follow-up is not checked as a txid");
        false
    });
    assert_eq!(follow.len(), 1);
    assert!(follow[0].wtxid);
}

#[test]
fn inflight_wtxid_of_the_same_hash_is_not_requested_again_as_txid() {
    let mut t = ParentTracker::new();
    let inflight = [0x11; 32];
    let missing = [0x22; 32];
    assert_eq!(
        t.note_inv(1, inflight, true, 1_000, true),
        ParentNote::RequestNow
    );
    t.schedule(inflight, 2, false, 1_000);
    t.schedule(missing, 2, false, 1_000);
    let due = t.take_due(2, 1_000, |_, _| false);
    assert_eq!(
        due.len(),
        1,
        "the in-flight hash must not join this getdata"
    );
    assert_eq!(due[0].hash, missing);
    assert!(!due[0].wtxid, "the other parent stays a txid request");
}

/// RAM bound: rows stop at the per-peer cap, and a peer's rows and index
/// entries leave with it, so the table is at most the cap times the live
/// sessions. A hash one peer announced holds one announcement, not spare
/// capacity.
#[test]
fn rows_are_bounded_per_live_peer() {
    let mut t = ParentTracker::new();
    let peers = 3u64;
    for p in 0..peers {
        for i in 0..=MAX_PARENT_ANN_PER_PEER as u32 {
            let mut h = [0u8; 32];
            h[0] = p as u8;
            h[1..5].copy_from_slice(&i.to_le_bytes());
            t.note_inv(p, h, true, 1_000 + u64::from(i / 1_000), true);
        }
    }
    assert_eq!(t.announcements, peers as usize * MAX_PARENT_ANN_PER_PEER);
    assert!(t.by_hash.iter().all(|(_, slot)| slot.anns.capacity() == 1));
    for p in 0..peers {
        t.forget_peer(p);
    }
    assert_eq!(t.announcements, 0);
    assert!(t.by_hash.is_empty());
    assert!(t.due_by_peer.is_empty() && t.inflight_by_peer.is_empty());
    assert!(t.hashes_by_peer.is_empty() && t.ann_per_peer.is_empty());
    assert!(t.sessions.is_empty());
}

fn shared_hash(i: u32) -> [u8; 32] {
    let mut h = [0xa5; 32];
    h[..4].copy_from_slice(&i.to_le_bytes());
    h
}

/// Peer 0 holds the request for every hash and every other peer waits on it.
fn tracker_with_shared_hashes(peers: u64, hashes: u32) -> ParentTracker {
    let mut t = ParentTracker::new();
    for p in 0..peers {
        for i in 0..hashes {
            let note = t.note_inv(p, shared_hash(i), true, 1_000 + p.min(1), true);
            let want = if p == 0 {
                ParentNote::RequestNow
            } else {
                ParentNote::Deferred
            };
            assert_eq!(note, want);
        }
    }
    t.by_hash.take_steps();
    t
}

/// With no process-wide cap, every live peer can wait on the same hashes.
/// The peer that holds their requests going away, its notfounds, and a
/// heartbeat over hashes another peer is asking for all run under the
/// tracker mutex. The first two fetch the rows of a hash at most eight
/// times per hash, not once per waiter, and the heartbeat fetches no rows
/// of a hash another peer is being asked for. Pins fetches (`Slots`
/// charges each one the slot's row count), not time.
#[test]
fn work_on_shared_hashes_is_linear_in_rows() {
    let (peers, hashes) = (40u64, 400u32);
    let rows = peers * u64::from(hashes);

    let mut t = tracker_with_shared_hashes(peers, hashes);
    t.forget_peer(0);
    let read = t.by_hash.take_steps();
    assert!(
        read <= 8 * rows,
        "the holder leaving read {read} rows for {rows} announcements"
    );
    let got = t.take_due(1, 1_002, |_, _| false);
    assert_eq!(
        got.len(),
        MAX_PARENTS_PER_PARK,
        "a waiter is asked once the holder is gone"
    );
    for p in 2..peers {
        t.take_due(p, 1_002, |_, _| false);
    }
    t.by_hash.take_steps();
    let blocked = t.take_due(peers - 1, 1_003, |_, _| false);
    assert!(
        blocked.is_empty(),
        "every hash is in flight at another peer"
    );
    assert_eq!(
        t.by_hash.take_steps(),
        u64::from(hashes),
        "a heartbeat reads one entry for each hash another peer is asked for"
    );

    let mut t = tracker_with_shared_hashes(peers, hashes);
    for i in 0..hashes {
        t.note_not_found(0, shared_hash(i));
    }
    let read = t.by_hash.take_steps();
    assert!(
        read <= 8 * rows,
        "the holder's notfounds read {read} rows for {rows} announcements"
    );
    assert_eq!(
        t.take_due(1, 1_002, |_, _| false).len(),
        MAX_PARENTS_PER_PARK,
        "a waiter is asked after the notfound"
    );
}

/// Every waiting row has one due entry at its `due_at`, every requested row
/// has one in-flight entry at its `requested_until`, and a slot's
/// `inflight_until` is its earliest request.
fn assert_indexes_match_rows(t: &ParentTracker) {
    let mut due = BTreeSet::new();
    let mut inflight = BTreeSet::new();
    let mut rows = 0;
    for (hash, slot) in t.by_hash.iter() {
        assert!(!slot.anns.is_empty(), "an empty slot is removed");
        assert_eq!(
            slot.inflight_until.map(|until| until.get()),
            slot.anns.iter().filter_map(|a| a.requested_until).min(),
        );
        for a in &slot.anns {
            rows += 1;
            match (a.requested_until, a.due_at) {
                (Some(exp), None) => assert!(inflight.insert((a.peer, exp, *hash, a.wtxid))),
                (None, Some(at)) => assert!(due.insert((a.peer, at, *hash, a.wtxid))),
                (None, None) => assert!(a.failed, "a row waits, is asked, or failed"),
                (Some(_), Some(_)) => panic!("a requested row is not also waiting"),
            }
        }
    }
    assert_eq!(rows, t.announcements);
    let entries = |index: &TimeIndex| -> BTreeSet<(u64, u64, [u8; 32], bool)> {
        index
            .iter()
            .flat_map(|(peer, rows)| {
                rows.iter()
                    .map(move |(at, h, wtxid)| (*peer, *at, *h, *wtxid))
            })
            .collect()
    };
    assert_eq!(entries(&t.due_by_peer), due);
    assert_eq!(entries(&t.inflight_by_peer), inflight);
    assert!(t.due_by_peer.values().all(|rows| !rows.is_empty()));
    assert!(t.inflight_by_peer.values().all(|rows| !rows.is_empty()));
}

/// A fixed sequence of mixed calls over a few peers and hashes, so one
/// peer holds a txid row and a wtxid row for the same hash and requests
/// end every way they can. The indexes and each slot's earliest request
/// match the rows after every call.
#[test]
fn indexes_match_rows_after_every_call() {
    let mut t = ParentTracker::new();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    let hash = |i: u64| [i as u8 + 1; 32];
    let mut now = 1_000u64;
    let mut asked = 0usize;
    for _ in 0..6_000 {
        let (peer, h) = (next(6), hash(next(8)));
        match next(10) {
            0..=2 => {
                t.note_inv(peer, h, next(2) == 0, now, next(2) == 0);
            }
            3 => t.schedule(h, peer, next(2) == 0, now + next(5)),
            4 => t.note_not_found(peer, h),
            5 | 6 => {
                let have = next(7) == 0;
                asked += t.take_due(peer, now, |_, _| have).len();
            }
            7 => t.resolve([h, hash(next(8))], next(3) == 0),
            8 if next(4) == 0 => t.forget_peer(peer),
            _ => now += next(25),
        }
        assert_indexes_match_rows(&t);
    }
    assert!(asked > 100, "the sequence asks peers: {asked}");
    for peer in 0..6 {
        t.forget_peer(peer);
    }
    assert!(t.by_hash.is_empty() && t.due_by_peer.is_empty() && t.inflight_by_peer.is_empty());
}
