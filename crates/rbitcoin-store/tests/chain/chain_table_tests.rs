use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::chain::*;
use rbitcoin_store::error::StoreError;

fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("chain");
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// `get_range` samples first and count together. A paired update or a clear
/// returns the previous pair, the new pair, or no body. Never a new first
/// with the old count, and never a zeroed first with the old count.
#[test]
fn get_range_during_put_and_clear_is_one_pair() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = tmp();
    let ht = Arc::new(HeaderTxsTable::create(&dir).unwrap());
    let write = rbitcoin_store::ClassCWrite::for_test();
    ht.put_range(&write, Fk(1), Fk(10), 4).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (ht, stop) = (Arc::clone(&ht), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match ht.get_range(Fk(1)).unwrap() {
                        Some((Fk(10), 4)) => {}
                        Some((Fk(50), 9)) => {}
                        None => {}
                        other => panic!("torn header range {other:?}"),
                    }
                }
            })
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        let write = rbitcoin_store::ClassCWrite::for_test();
        ht.put_range(&write, Fk(1), Fk(50), 9).unwrap();
        ht.clear_body(&write, Fk(1)).unwrap();
        ht.put_range(&write, Fk(1), Fk(10), 4).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        assert!(r.join().is_ok(), "get_range panicked");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(clippy::cognitive_complexity)] // one fixture, many chain surface arms
#[test]
fn confirmed_tx_height_header_txs_surface() {
    let dir = tmp();
    let c = ConfirmedTable::create(&dir).unwrap();
    assert!(c.tip_height().is_none());
    c.set(&rbitcoin_store::ClassCWrite::for_test(), Height(0), Fk(1))
        .unwrap();
    c.set(&rbitcoin_store::ClassCWrite::for_test(), Height(1), Fk(2))
        .unwrap();
    assert_eq!(c.tip_height(), Some(Height(1)));
    assert_eq!(c.get(Height(0)).unwrap(), Some(Fk(1)));
    c.set_many(&rbitcoin_store::ClassCWrite::for_test(), &[])
        .unwrap();
    c.set_many(
        &rbitcoin_store::ClassCWrite::for_test(),
        &[(Height(2), Fk(3)), (Height(3), Fk(4))],
    )
    .unwrap();
    assert_eq!(c.tip_height(), Some(Height(3)));
    assert!(matches!(
        c.set(
            &rbitcoin_store::ClassCWrite::for_test(),
            Height(4),
            Fk::NULL
        ),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(
        c.set_many(
            &rbitcoin_store::ClassCWrite::for_test(),
            &[(Height(5), Fk::NULL)]
        ),
        Err(StoreError::InvalidFk)
    ));
    c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(3))
        .unwrap();
    assert_eq!(c.tip_height(), Some(Height(2)));
    assert!(matches!(
        c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(0)),
        Err(StoreError::Corrupt(_))
    ));
    c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(2))
        .unwrap();
    c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(1))
        .unwrap();
    c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(0))
        .unwrap();
    assert!(c.tip_height().is_none());
    assert!(matches!(
        c.disconnect_tip(&rbitcoin_store::ClassCWrite::for_test(), Height(0)),
        Err(StoreError::Corrupt(_))
    ));
    c.flush().unwrap();
    c.flush_async().unwrap();

    let ht = HeaderTxsTable::create(&dir).unwrap();
    assert!(ht.get_range(Fk(1)).unwrap().is_none());
    ht.put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk(1), Fk(100), 3)
        .unwrap();
    assert_eq!(ht.get_range(Fk(1)).unwrap(), Some((Fk(100), 3)));
    assert_eq!(
        ht.get_list(Fk(1)).unwrap().unwrap(),
        vec![Fk(100), Fk(101), Fk(102)]
    );
    assert!(ht.has_body(Fk(1)).unwrap());
    assert!(!ht.has_body(Fk(2)).unwrap());
    ht.put_list(
        &rbitcoin_store::ClassCWrite::for_test(),
        Fk(2),
        &[Fk(200), Fk(201)],
    )
    .unwrap();
    assert!(matches!(
        ht.put_list(&rbitcoin_store::ClassCWrite::for_test(), Fk(3), &[]),
        Err(StoreError::Corrupt(_))
    ));
    // Non-contiguous triggers debug_assert in debug builds; only empty is a
    // stable Err path under RUSTFLAGS=-Dwarnings debug tests.
    assert!(matches!(
        ht.put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk::NULL, Fk(1), 1),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(
        ht.put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk(3), Fk::NULL, 1),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(
        ht.put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk(3), Fk(1), 0),
        Err(StoreError::InvalidFk)
    ));
    ht.put_lists_batch(&rbitcoin_store::ClassCWrite::for_test(), &[])
        .unwrap();
    ht.put_lists_batch(
        &rbitcoin_store::ClassCWrite::for_test(),
        &[(Fk(3), &[Fk(300)] as &[_]), (Fk(4), &[Fk(400), Fk(401)])],
    )
    .unwrap();
    assert!(matches!(
        ht.put_lists_batch(
            &rbitcoin_store::ClassCWrite::for_test(),
            &[(Fk(5), &[] as &[_])]
        ),
        Err(StoreError::Corrupt(_))
    ));
    ht.put_ranges_batch(&rbitcoin_store::ClassCWrite::for_test(), &[])
        .unwrap();
    ht.put_ranges_batch(
        &rbitcoin_store::ClassCWrite::for_test(),
        &[(Fk(5), Fk(500), 2)],
    )
    .unwrap();
    assert!(matches!(
        ht.put_ranges_batch(
            &rbitcoin_store::ClassCWrite::for_test(),
            &[(Fk::NULL, Fk(1), 1)]
        ),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(
        ht.put_ranges_batch(
            &rbitcoin_store::ClassCWrite::for_test(),
            &[(Fk(6), Fk::NULL, 1)]
        ),
        Err(StoreError::InvalidFk)
    ));
    assert_eq!(ht.count_bodies().unwrap(), 5);
    assert!(matches!(ht.get_range(Fk::NULL), Err(StoreError::InvalidFk)));

    // clear_body: drop association without freeing tx rows (merkle soft-reget path).
    assert!(matches!(
        ht.clear_body(&rbitcoin_store::ClassCWrite::for_test(), Fk::NULL),
        Err(StoreError::InvalidFk)
    ));
    assert!(
        !ht.clear_body(&rbitcoin_store::ClassCWrite::for_test(), Fk(99))
            .unwrap(),
        "no body → false"
    );
    assert!(
        ht.clear_body(&rbitcoin_store::ClassCWrite::for_test(), Fk(1))
            .unwrap(),
        "had body → true"
    );
    assert!(!ht.has_body(Fk(1)).unwrap());
    assert!(
        !ht.clear_body(&rbitcoin_store::ClassCWrite::for_test(), Fk(1))
            .unwrap(),
        "already clear → false"
    );
    assert_eq!(ht.count_bodies().unwrap(), 4);

    ht.flush().unwrap();
    ht.flush_async().unwrap();
    drop(ht);
    let ht = HeaderTxsTable::open(&dir).unwrap();
    assert_eq!(ht.get_range(Fk(5)).unwrap(), Some((Fk(500), 2)));
    assert!(
        !ht.has_body(Fk(1)).unwrap(),
        "clear_body durable after reopen"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn header_txs_contains_tx_range_edges() {
    let dir = tmp();
    let ht = HeaderTxsTable::create(&dir).unwrap();
    assert!(!ht.contains_tx(Fk(1), Fk::NULL).unwrap());
    assert!(!ht.contains_tx(Fk(1), Fk(1)).unwrap(), "no body");
    ht.put_range(&rbitcoin_store::ClassCWrite::for_test(), Fk(1), Fk(10), 3)
        .unwrap();
    assert!(ht.contains_tx(Fk(1), Fk(10)).unwrap());
    assert!(ht.contains_tx(Fk(1), Fk(12)).unwrap());
    assert!(!ht.contains_tx(Fk(1), Fk(9)).unwrap());
    assert!(!ht.contains_tx(Fk(1), Fk(13)).unwrap());
    assert!(!ht.contains_tx(Fk(2), Fk(10)).unwrap(), "other header");
    assert_eq!(ht.tx_index(Fk(1), Fk(10)).unwrap(), Some(0));
    assert_eq!(ht.tx_index(Fk(1), Fk(12)).unwrap(), Some(2));
    assert_eq!(ht.tx_index(Fk(1), Fk(9)).unwrap(), None);
    assert_eq!(ht.tx_index(Fk(1), Fk(13)).unwrap(), None);
    assert_eq!(ht.tx_index(Fk(1), Fk::NULL).unwrap(), None);
    assert_eq!(ht.tx_index(Fk(2), Fk(10)).unwrap(), None, "other header");
    let _ = std::fs::remove_dir_all(&dir);
}
