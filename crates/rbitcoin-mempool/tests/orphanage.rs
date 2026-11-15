use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness};
use bitcoin::{Transaction, Txid};
use rbitcoin_mempool::orphanage::*;
use std::collections::BTreeSet;

fn txid_n(n: u8) -> Txid {
    Txid::from_byte_array([n; 32])
}

fn make_orphan(parent: Txid, mark: u8) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: parent,
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1000 + mark as u64),
            script_pubkey: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array(
                [mark; 20],
            )),
        }],
    }
}

#[test]
fn one_peer_cannot_fill_the_orphanage() {
    let mut o = Orphanage::new();
    let parent = txid_n(8);
    let tx2 = {
        let mut tx = make_orphan(parent, 1);
        tx.input[0].witness = Witness::from_slice(&[vec![2u8; 64]]);
        tx
    };
    let mut miss = BTreeSet::new();
    miss.insert(parent);
    assert!(o.insert_from(tx2.clone(), miss.clone(), Some(2)));
    let mut first_peer = None;
    let mut last_peer = None;
    for i in 0..80u8 {
        let mut tx = make_orphan(parent, i.wrapping_add(3));
        tx.input[0].witness = Witness::from_slice(&[vec![i.wrapping_add(3); 20_000]]);
        tx.lock_time = LockTime::from_height(i as u32).unwrap();
        let tid = tx.compute_txid();
        if first_peer.is_none() {
            first_peer = Some(tid);
        }
        last_peer = Some(tid);
        o.insert_from(tx, miss.clone(), Some(1));
    }
    assert!(
        o.peer_orphan_weight(1) <= ORPHAN_RESERVED_WEIGHT_PER_PEER,
        "peer weight {}",
        o.peer_orphan_weight(1)
    );
    assert!(o.contains(&last_peer.expect("inserted")));
    assert!(!o.contains(&first_peer.expect("inserted")));
    let mut exact = make_orphan(txid_n(4), 1);
    let base = exact.weight().to_wu();
    let mut pad = ORPHAN_RESERVED_WEIGHT_PER_PEER.saturating_sub(base) as usize;
    for _ in 0..6 {
        exact.input[0].witness = Witness::from_slice(&[vec![1u8; pad]]);
        let got = exact.weight().to_wu();
        if got == ORPHAN_RESERVED_WEIGHT_PER_PEER {
            break;
        }
        if got > ORPHAN_RESERVED_WEIGHT_PER_PEER {
            pad = pad.saturating_sub((got - ORPHAN_RESERVED_WEIGHT_PER_PEER) as usize);
        } else {
            pad = pad.saturating_add((ORPHAN_RESERVED_WEIGHT_PER_PEER - got) as usize);
        }
    }
    assert_eq!(exact.weight().to_wu(), ORPHAN_RESERVED_WEIGHT_PER_PEER);
    let mut alone = Orphanage::new();
    let mut exact_miss = BTreeSet::new();
    exact_miss.insert(txid_n(4));
    assert!(alone.insert_from(exact, exact_miss, Some(9)));
    let peer2_kept = o.announcers_of(&tx2.compute_wtxid()).contains(&2);
    assert!(peer2_kept, "peer 2 must keep its orphan");
}

/// Peer B re-announces peer A's orphan, then overflows its own reserve.
/// The eviction drops B only. The parent still delivers A's orphan.
#[test]
fn shared_announcer_eviction_keeps_the_other_peer() {
    let mut o = Orphanage::new();
    let parent = txid_n(8);
    let mut miss = BTreeSet::new();
    miss.insert(parent);
    let tx_a = make_orphan(parent, 1);
    let tid = tx_a.compute_txid();
    let wid = tx_a.compute_wtxid();
    assert!(o.insert_from(tx_a, miss.clone(), Some(1)));
    assert!(o.add_announcer(&wid, 2));
    for i in 0..80u8 {
        let mut tx = make_orphan(parent, i.wrapping_add(3));
        tx.input[0].witness = Witness::from_slice(&[vec![i.wrapping_add(3); 20_000]]);
        tx.lock_time = LockTime::from_height(i as u32).unwrap();
        o.insert_from(tx, miss.clone(), Some(2));
    }
    assert!(o.peer_orphan_weight(2) <= ORPHAN_RESERVED_WEIGHT_PER_PEER);
    assert!(
        o.contains(&tid),
        "peer 1's orphan must survive peer 2's reserve"
    );
    assert!(o.announcers_of(&wid).contains(&1));
    let kids = o.take_children_of(&parent, |_| false);
    assert!(
        kids.iter().any(|tx| tx.tx.compute_txid() == tid),
        "the parent still delivers peer 1's orphan"
    );
}

#[test]
fn insert_and_take_by_parent() {
    let mut o = Orphanage::new();
    let p = txid_n(1);
    let tx = make_orphan(p, 2);
    let tid = tx.compute_txid();
    let mut miss = BTreeSet::new();
    miss.insert(p);
    let wtxid = tx.compute_wtxid();
    assert!(o.insert_from(tx, miss, None));
    assert!(o.contains(&tid));
    assert!(o.contains_wtxid(&wtxid));
    assert_eq!(o.missing_of(&wtxid).map(|s| s.len()), Some(1));
    assert_eq!(o.len(), 1);
    let kids = o.take_children_of(&p, |_| false);
    assert_eq!(kids.len(), 1);
    assert!(o.is_empty());
    assert!(!o.contains_wtxid(&wtxid));
}

#[test]
fn fifo_evicts_under_count_cap() {
    let mut o = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 2);
    let p = txid_n(9);
    for i in 0..3u8 {
        let tx = make_orphan(p, i + 1);
        let mut miss = BTreeSet::new();
        miss.insert(p);
        o.insert_from(tx, miss, None);
    }
    assert!(o.len() <= 2);
    assert!(o.total_weight() <= DEFAULT_ORPHAN_MAX_WEIGHT);
}

/// A removed orphan leaves no place in the eviction order. Parked again, it
/// is the newest, so the next eviction takes the older one.
#[test]
fn reparked_orphan_counts_as_newest() {
    let mut o = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 2);
    let p = txid_n(9);
    let miss = || BTreeSet::from([p]);
    let a = make_orphan(p, 1);
    let b = make_orphan(p, 2);
    let c = make_orphan(p, 3);
    assert!(o.insert_from(a.clone(), miss(), None));
    assert!(o.remove_wtxid(&a.compute_wtxid()).is_some());
    assert!(o.insert_from(b.clone(), miss(), None));
    assert!(o.insert_from(a.clone(), miss(), None));
    assert!(o.insert_from(c.clone(), miss(), None));
    assert!(!o.contains_wtxid(&b.compute_wtxid()), "the oldest goes");
    assert!(
        o.contains_wtxid(&a.compute_wtxid()),
        "the reparked one stays"
    );
    assert!(o.contains_wtxid(&c.compute_wtxid()));
    assert_eq!(o.fifo.len(), o.len(), "one fifo id per orphan");
}

#[test]
fn core_like_budget_constants() {
    assert_eq!(ORPHAN_RESERVED_WEIGHT_PER_PEER, 404_000);
    assert_eq!(DEFAULT_ORPHAN_MAX_WEIGHT, 404_000 * 25);
    const {
        assert!(DEFAULT_ORPHAN_MAX_WEIGHT > 10_000_000);
        assert!(DEFAULT_ORPHAN_MAX_WEIGHT < 11_000_000);
    }
}

#[test]
fn reject_oversize_duplicate_and_remove() {
    let mut o = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 100);
    let p = txid_n(3);
    let tx = make_orphan(p, 1);
    assert!(!o.insert_from(tx.clone(), BTreeSet::new(), None));
    let mut miss = BTreeSet::new();
    miss.insert(p);
    assert!(o.insert_from(tx.clone(), miss.clone(), None));
    assert!(!o.insert_from(tx.clone(), miss.clone(), None));
    let tid = tx.compute_txid();
    o.remove_wtxid(&tx.compute_wtxid());
    assert!(!o.contains(&tid));
    assert!(o.take_children_of(&txid_n(99), |_| false).is_empty());
    // Count-cap eviction (with_limits floors max_weight at MAX_ORPHAN_TX_WEIGHT).
    let mut o2 = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 3);
    for i in 0..8u8 {
        let t = make_orphan(p, i + 1);
        let mut m = BTreeSet::new();
        m.insert(p);
        o2.insert_from(t, m, None);
    }
    assert!(o2.len() <= 3);
    let t3 = make_orphan(p, 50);
    let tid3 = t3.compute_txid();
    let mut m = BTreeSet::new();
    m.insert(p);
    o2.insert_from(t3, m, None);
    o2.erase_for_block(&[tid3]);
    assert!(!o2.contains(&tid3));
}

#[test]
fn announcers_and_erase_for_peer() {
    let mut o = Orphanage::new();
    let p = txid_n(4);
    let tx = make_orphan(p, 7);
    let tid = tx.compute_txid();
    let wtxid = tx.compute_wtxid();
    let mut miss = BTreeSet::new();
    miss.insert(p);
    assert!(o.insert_from(tx.clone(), miss, Some(3)));
    assert!(o.has_announcer(3));
    assert!(!o.has_announcer(9));
    assert_eq!(o.announcers_of(&wtxid), vec![3]);
    assert!(o.add_announcer(&wtxid, 9));
    assert_eq!(o.announcers_of(&wtxid), vec![3, 9]);
    o.erase_for_peer(3);
    assert_eq!(o.announcers_of(&wtxid), vec![9]);
    assert!(o.contains(&tid));
    o.erase_for_peer(9);
    assert!(!o.contains(&tid));
    assert!(o.is_empty());
}
#[test]
fn mempool_under_pressure() {
    assert_eq!(ORPHAN_EXPIRE_MS, 1_200_000);

    let mut o = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 2);
    let parent = txid_n(11);
    let mut miss = BTreeSet::new();
    miss.insert(parent);

    let first = make_orphan(parent, 1);
    let second = make_orphan(parent, 2);
    assert!(o.insert_from_at(first, miss.clone(), Some(7), 0));
    let one = o.peer_orphan_weight(7);
    assert!(one > 0);
    assert!(o.insert_from_at(second.clone(), miss.clone(), Some(7), 0));
    assert!(o.peer_orphan_weight(7) > one);
    o.erase_for_block(&[second.compute_txid()]);
    assert_eq!(o.peer_orphan_weight(7), one);

    let filler = make_orphan(parent, 4);
    assert!(o.insert_from_at(filler, miss.clone(), Some(2), 0));
    assert_eq!(o.len(), 2);
    let later = make_orphan(parent, 3);
    let later_id = later.compute_txid();
    assert!(
        o.insert_from_at(later, miss.clone(), Some(3), 0),
        "a full per-peer reserve must still admit a newer orphan"
    );
    assert!(o.contains(&later_id));
    assert!(o.len() <= 2);

    let held = make_orphan(parent, 5);
    let held_id = held.compute_txid();
    assert!(o.insert_from_at(held, miss.clone(), Some(7), ORPHAN_EXPIRE_MS));
    assert!(
        !o.contains(&later_id),
        "an orphan parked for ORPHAN_EXPIRE_MS is gone"
    );
    assert!(o.contains(&held_id));
    let tail = make_orphan(parent, 6);
    assert!(o.insert_from_at(tail, miss, Some(7), ORPHAN_EXPIRE_MS + ORPHAN_EXPIRE_MS));
    assert!(
        !o.contains(&held_id),
        "the next expiry bound drops the orphan that survived the first"
    );
}

/// Orphan count, the per-peer reserve, and taking children of one parent.
/// A peer tx parks an orphan. The reserve choice (drop the unannounced
/// tx, keep the peer's) is not a field `getmempoolinfo` returns.
#[test]
fn orphan_counters_limits_and_child_take() {
    let parent = txid_n(21);
    let other = txid_n(22);
    let tx = make_orphan(parent, 1);
    let w = tx.weight().to_wu();
    assert!(w > 1);
    let mut miss = BTreeSet::new();
    miss.insert(parent);
    miss.insert(other);

    let mut empty = Orphanage::with_limits(w, 4);
    assert!(empty.is_empty());
    assert_eq!(empty.total_weight(), 0);
    assert!(empty.insert_from_at(tx.clone(), miss.clone(), None, 0));
    assert!(!empty.is_empty());
    assert_eq!(empty.total_weight(), w);

    let mut counted = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 1);
    assert!(counted.insert_from_at(tx.clone(), miss.clone(), None, 0));
    let extra = make_orphan(parent, 3);
    assert!(
        counted.insert_from_at(extra, miss_one_parent(parent), None, 0),
        "count cap evicts the older orphan"
    );
    assert_eq!(counted.len(), 1);

    let mut linked = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 4);
    let body = make_orphan(parent, 4);
    let id = body.compute_wtxid();
    assert!(linked.insert_from_at(body, miss.clone(), None, 0));
    let taken = linked.take_children_of(&parent, |_| false);
    assert_eq!(taken.len(), 1);
    assert!(
        !linked
            .by_parent
            .get(&other)
            .is_some_and(|set| set.contains(&id)),
        "the other missing parent drops the taken child"
    );

    let mut reserved = Orphanage::with_limits(DEFAULT_ORPHAN_MAX_WEIGHT, 2);
    let protected = make_orphan(parent, 5);
    let protected_id = protected.compute_txid();
    let loose = make_orphan(parent, 6);
    let loose_id = loose.compute_txid();
    assert!(reserved.insert_from_at(protected, miss_one_parent(parent), Some(1), 0));
    assert!(reserved.insert_from_at(loose, miss_one_parent(parent), None, 0));
    let newer = make_orphan(parent, 7);
    let newer_id = newer.compute_txid();
    assert!(reserved.insert_from_at(newer, miss_one_parent(parent), Some(2), 0));
    assert!(
        reserved.contains(&protected_id),
        "peer reserve keeps the older announcer"
    );
    assert!(reserved.contains(&newer_id));
    assert!(
        !reserved.contains(&loose_id),
        "the unannounced orphan is the one evicted"
    );
}

/// `with_limits` floors the weight cap at one max-size orphan. A tx that
/// lands on that cap is admitted. One past it evicts a single orphan.
/// The default orphan pool is far above one max-size tx. A journey cannot
/// fill it. Every orphan a peer sends still runs this compare.
#[test]
fn weight_cap_admits_exact_and_evicts_one_past() {
    let parent = txid_n(21);
    let cap = MAX_ORPHAN_TX_WEIGHT;
    let mut exact = Orphanage::with_limits(cap, 100_000);
    let seed = make_orphan(parent, 1);
    let seed_w = seed.weight().to_wu();
    assert!(exact.insert_from_at(seed, miss_one_parent(parent), None, 0));
    let filler = orphan_of_weight(parent, cap - seed_w);
    assert_eq!(filler.weight().to_wu(), cap - seed_w);
    assert!(
        exact.insert_from_at(filler, miss_one_parent(parent), None, 0),
        "a tx that lands on the weight cap still fits"
    );
    assert_eq!(exact.len(), 2);
    assert_eq!(exact.total_weight(), cap);
    let spill = make_orphan(parent, 2);
    assert!(exact.total_weight().saturating_add(spill.weight().to_wu()) > cap);
    assert_ne!(
        exact.total_weight().saturating_add(spill.weight().to_wu()),
        cap
    );
    assert!(
        exact.insert_from_at(spill, miss_one_parent(parent), None, 0),
        "one eviction makes room past the cap"
    );
    assert_eq!(exact.len(), 2);
    assert!(exact.total_weight() <= cap);

    // Dropping only the oldest still leaves the new tx over the cap.
    let mut twice = Orphanage::with_limits(cap, 100_000);
    let mut oldest = orphan_of_weight(parent, 50_000);
    oldest.lock_time = LockTime::from_height(1).unwrap();
    let oldest_id = oldest.compute_txid();
    let mut second = orphan_of_weight(parent, 300_000);
    second.lock_time = LockTime::from_height(2).unwrap();
    let second_id = second.compute_txid();
    let mut incoming = orphan_of_weight(parent, 200_000);
    incoming.lock_time = LockTime::from_height(3).unwrap();
    assert!(twice.insert_from_at(oldest, miss_one_parent(parent), None, 0));
    assert!(twice.insert_from_at(second, miss_one_parent(parent), None, 0));
    assert_eq!(twice.total_weight(), 350_000);
    assert!(twice.total_weight().saturating_add(200_000) > cap);
    assert!(300_000u64.saturating_add(200_000) > cap);
    assert!(
        twice.insert_from_at(incoming.clone(), miss_one_parent(parent), None, 0),
        "the weight cap evicts until the incoming orphan fits"
    );
    assert_eq!(twice.len(), 1);
    assert_eq!(twice.total_weight(), incoming.weight().to_wu());
    assert!(twice.contains(&incoming.compute_txid()));
    assert!(!twice.contains(&oldest_id));
    assert!(!twice.contains(&second_id));
}

fn miss_one_parent(parent: Txid) -> BTreeSet<Txid> {
    let mut miss = BTreeSet::new();
    miss.insert(parent);
    miss
}

/// Pad a witness until the tx weighs `target` wu. Witness bytes move the
/// weight by one, so every target above the bare orphan is reachable.
fn orphan_of_weight(parent: Txid, target: u64) -> Transaction {
    let mut n = 1usize;
    for _ in 0..8 {
        let mut tx = make_orphan(parent, 9);
        tx.lock_time = LockTime::from_height(50_000).unwrap();
        tx.input[0].witness = Witness::from_slice(&[vec![0x11; n]]);
        let got = tx.weight().to_wu();
        if got == target {
            return tx;
        }
        if got > target {
            n -= (got - target) as usize;
        } else {
            n += (target - got) as usize;
        }
    }
    panic!("no witness length weighs {target}");
}

fn witnessed(tx: &Transaction, mark: u8) -> Transaction {
    let mut w = tx.clone();
    w.input[0].witness = Witness::from_slice(&[[mark]]);
    w
}

/// Two witnesses of one txid are two orphans. The txid index follows the
/// one that is left, and a block that confirms the txid drops both.
#[test]
fn two_witnesses_of_one_txid_are_two_orphans() {
    let mut o = Orphanage::new();
    let p = txid_n(20);
    let plain = make_orphan(p, 1);
    let marked = witnessed(&plain, 9);
    let tid = plain.compute_txid();
    assert_eq!(tid, marked.compute_txid());
    for tx in [&plain, &marked] {
        assert!(o.insert_from(tx.clone(), BTreeSet::from([p]), Some(1)));
    }
    assert_eq!(o.len(), 2);

    assert!(o.remove_wtxid(&plain.compute_wtxid()).is_some());
    assert!(o.contains(&tid), "the other witness is still parked");
    assert!(o.contains_wtxid(&marked.compute_wtxid()));
    assert_eq!(o.by_txid.get(&tid), Some(&vec![marked.compute_wtxid()]));
    assert!(o.remove_wtxid(&marked.compute_wtxid()).is_some());
    assert!(!o.contains(&tid));
    assert!(o.by_txid.is_empty() && o.by_parent.is_empty());

    for tx in [&plain, &marked] {
        assert!(o.insert_from(tx.clone(), BTreeSet::from([p]), Some(1)));
    }
    o.erase_for_block(&[tid]);
    assert!(o.is_empty(), "a confirmed txid drops every witness");
    assert!(o.by_txid.is_empty() && o.by_parent.is_empty());
    assert_eq!(o.peer_orphan_weight(1), 0);
}

/// A promote leaves a child whose txid is already pooled where it is,
/// still under the parent, and takes the rest.
#[test]
fn take_children_leaves_a_pooled_txid_parked() {
    let mut o = Orphanage::new();
    let p = txid_n(21);
    let pooled = make_orphan(p, 1);
    let other = make_orphan(p, 2);
    for tx in [&pooled, &other] {
        assert!(o.insert_from(tx.clone(), BTreeSet::from([p]), Some(1)));
    }
    let pooled_id = pooled.compute_txid();
    let taken = o.take_children_of(&p, |t| *t == pooled_id);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].tx.compute_txid(), other.compute_txid());
    assert!(o.contains_wtxid(&pooled.compute_wtxid()));
    assert!(o
        .by_parent
        .get(&p)
        .is_some_and(|set| set.len() == 1 && set.contains(&pooled.compute_wtxid())));
    assert!(o.take_children_of(&p, |_| false).len() == 1);
    assert!(o.by_parent.is_empty());
}

/// `erase_pooled_children` drops only the sending peer's orphans, and only
/// those whose txid is pooled.
#[test]
fn erase_pooled_children_keeps_other_peers_and_unpooled() {
    let mut o = Orphanage::new();
    let p = txid_n(22);
    let mine = make_orphan(p, 1);
    let mine_witness = witnessed(&mine, 9);
    let theirs = make_orphan(p, 2);
    let unpooled = make_orphan(p, 3);
    assert!(o.insert_from(mine.clone(), BTreeSet::from([p]), Some(1)));
    assert!(o.insert_from(mine_witness.clone(), BTreeSet::from([p]), Some(1)));
    assert!(o.insert_from(theirs.clone(), BTreeSet::from([p]), Some(2)));
    assert!(o.insert_from(unpooled.clone(), BTreeSet::from([p]), Some(1)));
    let pooled = [mine.compute_txid(), theirs.compute_txid()];
    o.erase_pooled_children(&p, 1, |t| pooled.contains(t));
    assert!(!o.contains(&mine.compute_txid()), "both witnesses go");
    assert!(
        o.contains_wtxid(&theirs.compute_wtxid()),
        "another peer's stays"
    );
    assert!(
        o.contains_wtxid(&unpooled.compute_wtxid()),
        "an unpooled one stays"
    );
    assert_eq!(o.len(), 2);
    assert_eq!(o.snapshot().len(), 2, "the fifo holds no erased id");
}
