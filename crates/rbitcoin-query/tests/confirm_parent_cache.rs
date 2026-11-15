use rbitcoin_primitives::Fk;
use rbitcoin_query::confirm_parent_cache::*;
use rbitcoin_store::HeaderRecord;
use std::sync::Arc;

fn header_rec(hash: [u8; 32]) -> HeaderRecord {
    HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x1d00ffff,
        nonce: 0,
        merkle_root: [0u8; 32],
        hash,
        size: 0,
        weight: 0,
    }
}

#[test]
fn advance_tip_prunes_headers() {
    let c = ConfirmParentCache::new();
    c.advance_tip(0);
    c.put_header_plan(1, Fk(1), header_rec([1u8; 32]), vec![Fk(10)], [0u8; 32]);
    assert!(c.get_header_plan(1).is_some());
    c.advance_tip(1);
    assert!(c.get_header_plan(1).is_none());
    assert_eq!(c.header_plan_count(), 0);
}

/// Header plans are replaced, not mutated in place; tip GC drops them.
#[test]
fn header_plan_replace_and_tip_gc() {
    let c = ConfirmParentCache::new();
    c.advance_tip(0);
    c.put_header_plan(1, Fk(1), header_rec([1u8; 32]), vec![Fk(10)], [0u8; 32]);
    let a1 = c.get_header_plan(1).expect("plan");
    assert_eq!(a1.tx_fks, vec![Fk(10)]);
    c.put_header_plan(
        1,
        Fk(1),
        header_rec([1u8; 32]),
        vec![Fk(11), Fk(12)],
        [0u8; 32],
    );
    let a2 = c.get_header_plan(1).expect("replaced");
    assert_eq!(a2.tx_fks, vec![Fk(11), Fk(12)]);
    assert_eq!(c.header_plan_count(), 1);
    c.advance_tip(1);
    assert!(c.get_header_plan(1).is_none());
    assert_eq!(c.header_plan_count(), 0);
}

/// Header plans must work regardless of denserels cache env (regression for
/// mainnet tip freeze: `parent header plan missing above tip`).
#[test]
fn put_header_plan_always_stores_for_mtp() {
    let c = ConfirmParentCache::new();
    c.put_header_plan(1, Fk(1), header_rec([9u8; 32]), vec![Fk(1)], [0u8; 32]);
    let p = c
        .get_header_plan(1)
        .expect("header plan required for multi-block MTP");
    assert_eq!(p.header_rec.hash, [9u8; 32]);
    assert_eq!(c.header_plan_count(), 1);
}

/// Two reads of one height are the cached plan, not a copy of its tx ids.
#[test]
fn header_plan_read_shares_the_cached_tx_list() {
    let c = ConfirmParentCache::new();
    c.put_header_plan(4, Fk(4), header_rec([4u8; 32]), vec![Fk(9); 64], [0u8; 32]);
    let a = c.get_header_plan(4).expect("plan");
    let b = c.get_header_plan(4).expect("plan");
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(a.header_rec.hash, [4u8; 32]);
    assert_eq!(a.header_rec.timestamp, 1);
    assert_eq!(a.header_rec.bits, 0x1d00ffff);
    assert_eq!(a.tx_fks.len(), 64);
}

/// At/below tip must not re-enter the header map (process-owned RSS).
#[test]
fn put_header_plan_skips_at_or_below_tip() {
    let c = ConfirmParentCache::new();
    c.advance_tip(10);
    c.put_header_plan(
        10,
        Fk(10),
        header_rec([10u8; 32]),
        vec![Fk(1); 100],
        [0u8; 32],
    );
    c.put_header_plan(5, Fk(5), header_rec([5u8; 32]), vec![Fk(1); 100], [0u8; 32]);
    assert_eq!(c.header_plan_count(), 0);
    c.put_header_plan(11, Fk(11), header_rec([11u8; 32]), vec![Fk(1)], [0u8; 32]);
    assert_eq!(c.header_plan_count(), 1);
    c.advance_tip(11);
    assert_eq!(c.header_plan_count(), 0);
}

/// Replacing a height drops the stale hash reverse index (no hash_to_height leak).
#[test]
fn put_header_plan_replaces_stale_hash_reverse() {
    let c = ConfirmParentCache::new();
    c.put_header_plan(1, Fk(1), header_rec([1u8; 32]), vec![Fk(1)], [0u8; 32]);
    c.put_header_plan(1, Fk(1), header_rec([2u8; 32]), vec![Fk(2)], [0u8; 32]);
    assert!(c.get_header_by_hash(&[1u8; 32]).is_none());
    assert!(c.get_header_by_hash(&[2u8; 32]).is_some());
    assert_eq!(c.header_plan_count(), 1);
}
