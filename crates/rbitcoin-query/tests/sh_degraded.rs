//! A fault in the scripthash index must not wedge the chain.
//!
//! The fault: the head of a big (extent) key
//! points at a page whose `ver` byte is gone, so every full read of the key
//! fails with `scripthash extent last page not ver=2`.

use rbitcoin_primitives::{Fk, Height};
use rbitcoin_query::testutil::{
    fixture_header as header, fixture_pay as pay, heavy_sh_extent_query, tear_sh_extent_head,
    tiny_query_labeled, FixtureChain, TempDir, SH_HEAVY_CREATES, SH_HEAVY_SCRIPT as KEY_SCRIPT,
};
use rbitcoin_query::{Query, TxApply};
use rbitcoin_store::HeaderRecord;

/// Creates on one key that move it from a slab to an extent chain
/// (`SH_MEGAKEY_MIN_FKS`). `tear_sh_extent_head` fails if the key is not one.
const HEAVY: u32 = SH_HEAVY_CREATES;

struct Chain {
    prev_fk: Fk,
    prev_hash: Option<[u8; 32]>,
    height: u32,
    seed: u32,
}

impl Chain {
    fn new() -> Self {
        Self {
            prev_fk: Fk::NULL,
            prev_hash: None,
            height: 0,
            seed: 1,
        }
    }

    /// Continue from the current confirmed tip.
    fn resync_to_tip(&mut self, q: &Query) {
        let tip = q.tip_height().unwrap();
        let (fk, rec) = q.header_at_height(tip).unwrap().unwrap();
        self.prev_fk = fk;
        self.prev_hash = Some(rec.hash);
        self.height = tip.0 + 1;
    }

    fn block(&mut self, q: &Query, n_txs: u32, nonce: u32) {
        let (h, txs) = self.make(n_txs, nonce);
        self.apply(q, &h, &txs);
    }

    /// Build the next block without connecting it.
    fn make(&mut self, n_txs: u32, nonce: u32) -> (HeaderRecord, Vec<TxApply>) {
        let txs: Vec<TxApply> = (0..n_txs)
            .map(|_| {
                self.seed += 1;
                pay(self.seed, KEY_SCRIPT)
            })
            .collect();
        (
            header(self.height, nonce, self.prev_fk, self.prev_hash, &txs),
            txs,
        )
    }

    fn apply(&mut self, q: &Query, h: &HeaderRecord, txs: &[TxApply]) {
        self.prev_fk = q.connect_block(Height(self.height), h, txs).unwrap();
        self.prev_hash = Some(h.hash);
        self.height += 1;
    }
}

fn tear_extent_head(q: &Query, key: &[u8; 32]) {
    tear_sh_extent_head(q, key);
}

/// Extent key, then 3 more txs on top (see [`heavy_sh_extent_query`]).
fn heavy_chain(label: &str) -> (TempDir, Query, Chain, [u8; 32]) {
    chain_over(heavy_sh_extent_query(label, 3))
}

/// Extent key as the tip block, so a test can connect its own next block.
fn heavy_head(label: &str) -> (TempDir, Query, Chain, [u8; 32]) {
    chain_over(heavy_sh_extent_query(label, 0))
}

fn chain_over((dir, q, key): (TempDir, Query, [u8; 32])) -> (TempDir, Query, Chain, [u8; 32]) {
    let mut c = Chain::new();
    c.resync_to_tip(&q);
    // Past the txids the fixture used.
    c.seed = 10_000;
    (dir, q, c, key)
}

#[test]
fn torn_head_disconnect_degrades_instead_of_wedging_and_rebuild_restores() {
    let (_dir, q, mut c, key) = heavy_chain("sh-degraded");
    assert!(q.sh_history_available());
    tear_extent_head(&q, &key);
    // The error a direct full read of the key returns.
    let err = q.store().scripthash.entries(&key).unwrap_err().to_string();
    assert!(err.contains("extent last page not ver=2"), "{err}");

    // The disconnect that used to wedge the chain now succeeds.
    q.disconnect_tip().expect("disconnect with torn SH head");
    assert_eq!(q.tip_height(), Some(Height(c.height - 2)));
    assert!(q.sh_degraded());
    assert!(q.sh_degraded_reason().unwrap().contains("not ver=2"));
    assert!(!q.sh_history_available());
    assert_eq!(q.sh_unavailable_message(), "scripthash index rebuilding");
    assert!(q.sh_degraded_marker_present());

    // The chain keeps moving both ways with SH deferred: a competing branch
    // off block 1 (the 2-block reorg that used to wedge the chain), then a disconnect.
    c.resync_to_tip(&q);
    c.block(&q, 2, 7);
    c.block(&q, 2, 7);
    assert_eq!(q.tip_height(), Some(Height(3)));
    q.disconnect_tip()
        .expect("second disconnect while degraded");
    assert!(q.sh_degraded());
    c.resync_to_tip(&q);

    // Rebuild restores a readable index that serves again.
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    assert!(!q.sh_degraded());
    assert!(q.sh_history_available());
    assert!(!q.sh_degraded_marker_present());
    let n = q.scripthash_create_count(&key).unwrap();
    assert!(n >= HEAVY, "rebuilt key holds the history, got {n}");
    q.store().scripthash.entries(&key).expect("key readable");

    // Appends flow again after the rebuild.
    c.block(&q, 4, 9);
    assert_eq!(q.scripthash_create_count(&key).unwrap(), n + 4);
    assert!(!q.rebuild_sh_if_degraded(None).unwrap(), "nothing to do");
}

#[test]
fn restart_with_degraded_marker_wipes_sh_at_open() {
    let (dir, q, _c, key) = heavy_chain("sh-degraded-restart");
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded_marker_present());
    drop(q);

    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert!(
        q.sh_degraded_marker_present(),
        "marker survives the restart"
    );
    assert!(q.startup_sh_rebuild(true).unwrap());
    assert!(!q.sh_degraded());
    assert!(q.store().scripthash.head_is_empty(), "head wiped at open");
    assert_eq!(q.store().scripthash.create_count(&key).unwrap(), 0);
    // The marker outlives the wipe: the replacement head is not sealed yet,
    // so a crash now must wipe again (see the next test for the seal).
    assert!(q.sh_degraded_marker_present(), "marker kept after the wipe");
}

/// The open-time wipe leaves `scripthash.degraded` in place. It goes only once
/// the cold rebuild has sealed a head and `include_hwm` is durable, so a kill
/// between the wipe and the seal wipes again instead of trusting a half head.
#[test]
fn open_wipe_keeps_the_marker_until_the_cold_rebuild_is_sealed() {
    let (dir, q, _c, key) = heavy_chain("sh-degraded-marker-life");
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    drop(q);

    // First open: wipe, then die before tip entry.
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert!(q.startup_sh_rebuild(true).unwrap());
    assert!(q.sh_degraded_marker_present());
    drop(q);

    // Second open: still marked, wipes again, and this time finishes.
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert!(q.sh_degraded_marker_present(), "marker survives the kill");
    assert!(q.startup_sh_rebuild(true).unwrap(), "wipes again");
    q.enter_direct_index_mode().unwrap();
    assert!(
        q.sh_degraded_marker_present(),
        "marker stays through the Direct start"
    );
    q.finalize_sh_runs().unwrap();
    assert!(
        !q.sh_degraded_marker_present(),
        "sealed head: marker removed"
    );
    assert!(
        q.store().scripthash.include_hwm() > 0,
        "include_hwm durable"
    );
    assert!(!q.startup_sh_rebuild(true).unwrap(), "healthy third start");
}

#[test]
fn force_rebuild_wipes_at_open_not_only_at_tip_entry() {
    let (_dir, q, _c, key) = heavy_chain("sh-force-open");
    assert!(q.store().scripthash.create_count(&key).unwrap() >= HEAVY);
    assert!(!q.startup_sh_rebuild(true).unwrap(), "no flag, no wipe");
    q.set_sh_force_rebuild(true);
    assert!(
        !q.startup_sh_rebuild(false).unwrap(),
        "shindex off: untouched"
    );
    assert!(q.startup_sh_rebuild(true).unwrap());
    assert_eq!(q.store().scripthash.create_count(&key).unwrap(), 0);
}

#[test]
fn sh_disabled_message_stays_for_shindex_off() {
    let (_dir, q) = tiny_query_labeled("sh-off");
    q.set_sh_index_enabled(false);
    assert!(!q.sh_history_available());
    assert_eq!(q.sh_unavailable_message(), "scripthash index disabled");
}

/// The write-behind append hits the same torn head. It used to halt the node
/// (`request_sh_writebehind_halt`); now the block connects and SH degrades.
#[test]
fn torn_head_append_degrades_instead_of_failing_the_block() {
    let (_dir, q, mut c, key) = heavy_chain("sh-degraded-append");
    tear_extent_head(&q, &key);
    c.block(&q, 2, 3);
    assert_eq!(
        q.tip_height(),
        Some(Height(c.height - 1)),
        "block connected"
    );
    assert!(q.sh_degraded());
    assert!(q.sh_degraded_reason().unwrap().contains("write-behind"));
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    assert!(q.sh_history_available());
    assert!(q.scripthash_create_count(&key).unwrap() >= HEAVY);
}

fn history_txids(q: &Query, key: &[u8; 32]) -> Vec<[u8; 32]> {
    let mut v: Vec<[u8; 32]> = q
        .scripthash_history(key)
        .unwrap()
        .into_iter()
        .map(|r| r.txid)
        .collect();
    v.sort();
    v
}

fn txids(txs: &[TxApply]) -> Vec<[u8; 32]> {
    let mut v: Vec<[u8; 32]> = txs.iter().map(|t| t.tx.txid).collect();
    v.sort();
    v
}

fn unique(v: &[[u8; 32]]) -> bool {
    let mut w = v.to_vec();
    w.dedup();
    w.len() == v.len()
}

/// Gap 3: a block disconnected while degraded leaves its creates in the
/// rebuilt index (Class A keeps the orphan tx). Readers must not see them, and
/// reconnecting the same block must not double them.
#[test]
fn orphan_creates_from_degraded_disconnect_are_invisible_and_reconnect_is_exact() {
    let (_dir, q, mut c, key) = heavy_head("sh-degraded-orphan");
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    let base = history_txids(&q, &key).len();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());
    assert!(q.rebuild_sh_if_degraded(None).unwrap());

    // Orphans: stored in the rebuilt index, invisible to every reader.
    let after = history_txids(&q, &key);
    assert_eq!(after.len(), base - 3, "orphans leaked into history");
    for t in txids(&txs2) {
        assert!(!after.contains(&t), "orphan tx in history");
    }
    let tip = q.tip_height().unwrap();
    assert_eq!(
        q.scripthash_listunspent(&key).unwrap().len(),
        base - 3,
        "listunspent leaked orphans"
    );

    // Reconnect the very same block.
    c.height = tip.0 + 1;
    let (fk, rec) = q.header_at_height(tip).unwrap().unwrap();
    c.prev_fk = fk;
    c.prev_hash = Some(rec.hash);
    c.apply(&q, &h2, &txs2);
    let again = history_txids(&q, &key);
    assert!(unique(&again), "reconnect duplicated history rows");
    assert_eq!(again.len(), base, "history after reconnect");
    for t in txids(&txs2) {
        assert!(again.contains(&t), "reconnected tx missing");
    }
    assert_eq!(
        q.scripthash_listunspent(&key).unwrap().len(),
        base,
        "listunspent after reconnect"
    );
    assert!(q.store().scripthash.entries(&key).is_ok());
}

/// Gap 3b: the orphaned txs re-confirm in a competing block (the usual reorg
/// shape: same txids, different header). The rebuilt index holds the orphan
/// fk; the reconnect may allocate another. Each tx must show up once.
#[test]
fn orphan_txs_reconfirmed_in_competing_block_show_once() {
    let (_dir, q, mut c, key) = heavy_head("sh-degraded-orphan2");
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    let base = history_txids(&q, &key).len();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());
    // Competing block at the same height, same txs, different nonce.
    c.resync_to_tip(&q);
    let h2b = header(c.height, 77, c.prev_fk, c.prev_hash, &txs2);
    c.apply(&q, &h2b, &txs2);
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    let h = history_txids(&q, &key);
    assert!(unique(&h), "duplicate rows after competing-block reconfirm");
    assert_eq!(h.len(), base);
    assert_eq!(q.scripthash_listunspent(&key).unwrap().len(), base);
    // One more block still appends cleanly after the rebuild.
    c.block(&q, 2, 5);
    assert_eq!(history_txids(&q, &key).len(), base + 2);
    // And a reorg off that block once healthy goes through the normal unlink.
    q.disconnect_tip().unwrap();
    assert!(!q.sh_degraded());
    assert_eq!(history_txids(&q, &key).len(), base);
}

/// Gap 4: a reorg while the rebuild is running. `stage` is the window in the
/// rebuild where the hook disconnects the tip and connects a competing block
/// with different txs. The rebuilt index must equal the final best chain.
fn reorg_during_rebuild(stage: &'static str) {
    use std::cell::RefCell;
    use std::rc::Rc;
    let (_dir, q, mut c, key) = heavy_head(&format!("sh-degraded-reorg-{stage}"));
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    let before = history_txids(&q, &key).len();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());

    // Competing block at height 2 (new txs), then one on top, from the hook.
    c.resync_to_tip(&q);
    let comp: Rc<RefCell<Vec<TxApply>>> = Rc::new(RefCell::new(Vec::new()));
    let cc = Rc::new(RefCell::new(c));
    let (comp2, cc2) = (comp.clone(), cc.clone());
    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |s, q| {
        if s != stage {
            return Ok(());
        }
        let mut c = cc2.borrow_mut();
        // Connect two blocks, then reorg the top one away for a third.
        let (ha, ta) = c.make(2, 11);
        c.apply(q, &ha, &ta);
        let (hb, tb) = c.make(2, 12);
        c.apply(q, &hb, &tb);
        q.disconnect_tip().unwrap();
        c.resync_to_tip(q);
        let (hc, tc) = c.make(4, 13);
        c.apply(q, &hc, &tc);
        let mut keep = comp2.borrow_mut();
        keep.extend(ta);
        keep.extend(tc);
        Ok(())
    })));
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);

    let want = {
        // Best chain now: blocks 0, 1 (HEAVY+1 txs) + comp (2 + 4).
        before - 3 + comp.borrow().len()
    };
    let got = history_txids(&q, &key);
    assert!(unique(&got), "duplicate rows");
    assert_eq!(
        got.len(),
        want,
        "history after reorg during rebuild ({stage})"
    );
    for t in txids(&comp.borrow()) {
        assert!(got.contains(&t), "competing tx missing");
    }
    assert!(!q.sh_degraded());
    assert_eq!(q.scripthash_listunspent(&key).unwrap().len(), want);
    // Tip-follow still works afterwards.
    cc.borrow_mut().block(&q, 2, 21);
    assert_eq!(history_txids(&q, &key).len(), want + 2);
}

#[test]
fn reorg_after_wipe_during_rebuild() {
    reorg_during_rebuild("after_wipe");
}

#[test]
fn reorg_after_collect_during_rebuild() {
    reorg_during_rebuild("after_collect");
}

/// Gap 5: SIGKILL mid-rebuild. The hook aborts the rebuild at the two points
/// where it is between steps (tables wiped; tables wiped and refilled), which
/// leaves the marker and the half-built tables exactly as a kill would. The
/// restart path (`startup_sh_rebuild` wipes on the marker, then the cold build
/// runs) must come back to the exact chain history. That the marker survives a
/// real reopen is `restart_with_degraded_marker_wipes_sh_at_open`.
fn crash_mid_rebuild(stage: &'static str) {
    let (_dir, q, mut c, key) = heavy_chain(&format!("sh-degraded-crash-{stage}"));
    let want = history_txids(&q, &key).len();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    c.resync_to_tip(&q);
    c.block(&q, 3, 5);
    assert!(q.sh_degraded());

    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |s, q| {
        if s != stage {
            return Ok(());
        }
        assert!(q.sh_degraded_marker_present(), "marker present mid-rebuild");
        Err(rbitcoin_store::StoreError::Corrupt("simulated kill"))
    })));
    assert!(q.rebuild_sh_if_degraded(None).is_err());
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);
    // Killed: still degraded, marker still there, nothing served.
    assert!(q.sh_degraded());
    assert!(q.sh_degraded_marker_present());
    assert!(!q.sh_history_available());

    // Restart: wipe on the marker, then the cold build.
    assert!(q.startup_sh_rebuild(true).unwrap(), "wipes at open");
    assert!(q.sh_degraded_marker_present(), "marker kept until sealed");
    assert!(!q.sh_degraded());
    q.enter_direct_index_mode().unwrap();
    q.finalize_sh_runs().unwrap();
    assert!(!q.sh_degraded_marker_present());
    q.enter_tip_index_mode();
    assert!(q.sh_history_available());
    let got = history_txids(&q, &key);
    assert!(unique(&got));
    assert_eq!(got.len(), want, "history after kill + restart");
    q.store().scripthash.entries(&key).expect("readable");
    c.block(&q, 2, 31);
    assert_eq!(history_txids(&q, &key).len(), want + 2);
}

#[test]
fn crash_after_wipe_mid_rebuild_recovers_on_restart() {
    crash_mid_rebuild("after_wipe");
}

#[test]
fn crash_after_collect_mid_rebuild_recovers_on_restart() {
    crash_mid_rebuild("after_collect");
}

/// Gap 2: requests that passed the 503 gate keep reading while the index
/// tears, degrades, wipes and rebuilds underneath them.
///
/// The contract, with no fault window: a reader that holds the read guard for
/// its whole read and finds the guard still valid afterwards saw a state the
/// chain really had (a length from `allowed`, no duplicate rows). A read the
/// fault overlapped, whether it errored on the torn key, saw the unlink batch, or
/// finished after the flag was set, gets an invalid guard, which the Esplora
/// and Electrum handlers answer as a 503. A valid guard with an `Err` is a bug.
#[test]
fn readers_through_degrade_wipe_and_rebuild_see_a_real_state_or_503() {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    let (_dir, q, mut c, key) = heavy_chain("sh-degraded-stress");
    let q = Arc::new(q);
    let h = history_txids(&q, &key).len();
    // Healthy, after the disconnect and rebuild, then each block connected on top.
    let allowed: Arc<HashSet<usize>> = Arc::new(HashSet::from([h, h - 3, h - 1, h + 2]));
    let stop = Arc::new(AtomicBool::new(false));
    let (judged, refused) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let bad = Arc::new(Mutex::new(Vec::<String>::new()));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (q, stop, allowed) = (q.clone(), stop.clone(), allowed.clone());
            let (judged, refused, bad) = (judged.clone(), refused.clone(), bad.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // The server gate: a request holds the guard for its whole read.
                    let Some(guard) = q.sh_read_enter() else {
                        refused.fetch_add(1, Ordering::Relaxed);
                        std::thread::yield_now();
                        continue;
                    };
                    let res = q.scripthash_history(&key);
                    if !guard.still_valid() {
                        refused.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    judged.fetch_add(1, Ordering::Relaxed);
                    match res {
                        Err(e) => bad.lock().unwrap().push(format!("valid guard, error {e}")),
                        Ok(rows) => {
                            let mut ids: Vec<[u8; 32]> = rows.iter().map(|r| r.txid).collect();
                            ids.sort();
                            let n = ids.len();
                            ids.dedup();
                            if ids.len() != n {
                                bad.lock().unwrap().push(format!("duplicate rows n={n}"));
                            } else if !allowed.contains(&n) {
                                bad.lock().unwrap().push(format!("torn length {n}"));
                            }
                        }
                    }
                }
            })
        })
        .collect();

    // Run until the readers have judged some healthy reads, then break the
    // index, then until they have judged some after the rebuild.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let judged_past = |n: u64| {
        while judged.load(Ordering::Relaxed) < n {
            assert!(std::time::Instant::now() < deadline, "readers stalled");
            std::thread::yield_now();
        }
    };
    judged_past(20);
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    let at_rebuild = judged.load(Ordering::Relaxed);
    c.resync_to_tip(&q);
    c.block(&q, 2, 40);
    c.block(&q, 3, 50);
    judged_past(at_rebuild + 20);
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().expect("reader thread panicked");
    }
    eprintln!(
        "STRESS judged={} refused_or_overlapped={} bad={:?}",
        judged.load(Ordering::Relaxed),
        refused.load(Ordering::Relaxed),
        bad.lock().unwrap()
    );
    assert!(bad.lock().unwrap().is_empty(), "{:?}", bad.lock().unwrap());
}

/// Gap 2, deterministic: a request that passed the gate before the index
/// degraded holds a read guard. The wipe waits for it (it never runs with a
/// reader in flight), and the reader's guard says the read overlapped the
/// fault. New requests are refused meanwhile and nothing leaks.
#[test]
fn rebuild_wipe_waits_for_in_flight_readers() {
    use std::sync::Arc;
    let (_dir, q, _c, key) = heavy_chain("sh-degraded-drain");
    let q = Arc::new(q);
    let guard = q.sh_read_enter().expect("healthy index admits a reader");
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());
    assert!(q.sh_read_enter().is_none(), "degraded: refused");
    assert_eq!(
        q.sh_readers_in_flight(),
        1,
        "refused attempt leaked a count"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let q2 = q.clone();
    let t = std::thread::spawn(move || {
        rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |stage, q| {
            match stage {
                // The drain found the reader and is about to park on its condvar.
                "drain_wait" => tx.send(()).unwrap(),
                "after_wipe" => assert_eq!(
                    q.sh_readers_in_flight(),
                    0,
                    "the wipe ran with a reader in flight"
                ),
                _ => {}
            }
            Ok(())
        })));
        let r = q2.rebuild_sh_if_degraded(None);
        rbitcoin_query::set_sh_rebuild_hook_for_test(None);
        r
    });
    rx.recv().expect("rebuild reached the reader drain");
    assert!(
        !guard.still_valid(),
        "a read that spans the degrade must not be answered"
    );
    drop(guard);
    assert!(t.join().unwrap().unwrap());
    let g = q.sh_read_enter().expect("serves again after the rebuild");
    assert!(g.still_valid(), "a read after the rebuild is valid");
    drop(g);
    // Neither the refused attempt nor the drain leaked a count: a second
    // degrade and rebuild goes straight through.
    q.mark_sh_degraded("again");
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
}

/// The wipe waits for a reader as long as it takes, but not past a stop: a
/// shutdown must not hang behind a stuck request, and the wipe must not run
/// with the reader still there.
#[test]
fn rebuild_waiting_for_a_reader_gives_way_to_stop() {
    use std::cell::Cell;
    use std::rc::Rc;
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-drain-stop");
    let _guard = q.sh_read_enter().unwrap();
    q.mark_sh_degraded("fault");
    let wiped = Rc::new(Cell::new(false));
    let w2 = wiped.clone();
    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |stage, _| {
        w2.set(w2.get() || stage == "after_wipe");
        Ok(())
    })));
    let stop = std::sync::atomic::AtomicBool::new(true);
    let err = q.rebuild_sh_if_degraded(Some(&stop)).unwrap_err();
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);
    assert!(
        matches!(err, rbitcoin_store::StoreError::Cancelled(_)),
        "{err}"
    );
    assert!(!wiped.get(), "wiped with a reader in flight");
    assert!(q.sh_degraded(), "cancel leaves the index degraded");
}

/// Gap 1: a history, listunspent or count read that trips over the torn key
/// latches the degraded state itself. Until then every other address answered
/// and the supervisor never started. The 503 holds until the replacement head
/// is published.
#[test]
fn read_of_a_torn_key_latches_degraded_and_keeps_the_503_until_rebuilt() {
    type Read = fn(&Query, &[u8; 32]) -> bool;
    let reads: [(&str, Read); 3] = [
        ("history", |q, k| q.scripthash_history(k).is_err()),
        ("listunspent", |q, k| q.scripthash_listunspent(k).is_err()),
        ("create_count", |q, k| q.scripthash_create_count(k).is_err()),
    ];
    for (name, read_fails) in reads {
        // A cold pack stores the rebuilt key as a slab, so each read tears a fresh extent.
        let (_dir, q, _c, key) = heavy_chain(&format!("sh-degraded-read-latch-{name}"));
        let healthy = history_txids(&q, &key).len();
        tear_extent_head(&q, &key);
        let guard = q
            .sh_read_enter()
            .expect("admitted before the read finds the tear");
        assert!(!q.sh_degraded(), "{name}: nothing has touched the key yet");
        assert!(read_fails(&q, &key), "{name}: the torn key must fail");
        assert!(q.sh_degraded(), "{name}: the failed read latches");
        assert!(
            !guard.still_valid(),
            "{name}: the reader that hit it is refused"
        );
        assert!(q.sh_degraded_reason().unwrap().contains("not ver=2"));
        assert!(q.sh_degraded_marker_present(), "{name}");
        drop(guard);
        assert!(q.sh_read_enter().is_none(), "{name}: every address is 503");
        assert_eq!(q.sh_unavailable_message(), "scripthash index rebuilding");
        assert!(q.rebuild_sh_if_degraded(None).unwrap(), "{name}");
        assert_eq!(history_txids(&q, &key).len(), healthy, "{name}: rebuilt");
    }
}

/// A fault latches only when a second read, with the appender held out, fails
/// too. An append rewrites a slab in place, so a read can land in it and see
/// a torn row that is gone on the next read; wiping the index for that would
/// be a rebuild per race. Errors that are not the tables' are not retried.
#[test]
fn a_transient_read_fault_does_not_degrade_but_a_persistent_one_does() {
    use rbitcoin_store::StoreError;
    use std::cell::Cell;
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-transient");
    let calls = Cell::new(0u32);
    let flaky = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Err(StoreError::Corrupt("fk stream null first fk"))
        } else {
            Ok(7u32)
        }
    };
    assert_eq!(q.sh_read_latching(flaky).unwrap(), 7);
    assert_eq!(calls.get(), 2, "read again after the fault");
    assert!(!q.sh_degraded(), "a fault that cleared is not the index's");

    calls.set(0);
    let broken = || -> Result<u32, StoreError> {
        calls.set(calls.get() + 1);
        Err(StoreError::Corrupt("extent last page not ver=2"))
    };
    assert!(q.sh_read_latching(broken).is_err());
    assert_eq!(calls.get(), 2);
    assert!(q.sh_degraded(), "a fault that holds still latches");
    assert!(q.rebuild_sh_if_degraded(None).unwrap());

    calls.set(0);
    let refused = || -> Result<u32, StoreError> {
        calls.set(calls.get() + 1);
        Err(StoreError::Rejected("cap"))
    };
    assert!(q.sh_read_latching(refused).is_err());
    assert_eq!(calls.get(), 1, "not a table fault: no retry");
    assert!(!q.sh_degraded());
}

/// Gap 4: only a scripthash `Corrupt` or I/O error degrades the index. Any
/// other write-behind failure stays on the halt path, which requeues the job.
#[test]
fn only_a_scripthash_fault_degrades_the_write_behind() {
    use rbitcoin_store::StoreError;
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-classify");
    let io = || {
        StoreError::io(
            std::path::Path::new("scripthash.body"),
            std::io::Error::other("EIO"),
        )
    };
    for e in [StoreError::Corrupt("extent"), io()] {
        assert!(
            !q.settle_sh_apply(Height(1), Err(e)).unwrap(),
            "dropped job"
        );
        assert!(q.sh_degraded());
        q.mark_sh_degraded("reset");
        assert!(q.rebuild_sh_if_degraded(None).unwrap());
    }
    for e in [
        StoreError::Cancelled("stop"),
        StoreError::BudgetFull("sq"),
        StoreError::NotFound,
    ] {
        let msg = e.to_string();
        let err = q.settle_sh_apply(Height(1), Err(e)).unwrap_err();
        assert_eq!(err.to_string(), msg, "passed through to the halt path");
        assert!(!q.sh_degraded(), "{msg} must not degrade the index");
    }
    assert!(q.settle_sh_apply(Height(1), Ok(())).unwrap());
}

/// Gap 6: the unlink batch rewrites keys before it returns, so for its whole
/// run readers are held off, and a read that overlapped it fails its guard.
/// A batch that works lets readers back in. One that fails leaves them out.
#[test]
fn unlink_batch_holds_readers_off_and_fails_overlapping_reads() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let (_dir, q, _c, key) = heavy_chain("sh-degraded-freeze");
    let in_flight = q.sh_read_enter().expect("reader before the batch");
    type Seen = Rc<RefCell<Vec<(bool, bool, &'static str)>>>;
    let seen: Seen = Rc::new(RefCell::new(Vec::new()));
    let s2 = seen.clone();
    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |stage, q| {
        if stage == "unlink_frozen" {
            s2.borrow_mut().push((
                q.sh_read_enter().is_none(),
                q.sh_history_available(),
                q.sh_unavailable_message(),
            ));
        }
        Ok(())
    })));
    q.disconnect_tip().unwrap();
    let busy = (true, false, "scripthash index updating");
    assert_eq!(*seen.borrow(), vec![busy], "refused during the batch");
    assert!(!in_flight.still_valid(), "the read spanned the batch");
    drop(in_flight);
    assert!(!q.sh_degraded(), "the batch worked");
    let g = q
        .sh_read_enter()
        .expect("readers return after a good batch");
    assert!(g.still_valid());
    drop(g);

    // A failing batch keeps them out: the flag takes over from the freeze.
    seen.borrow_mut().clear();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);
    assert_eq!(*seen.borrow(), vec![busy]);
    assert!(q.sh_degraded());
    assert!(
        q.sh_read_enter().is_none(),
        "still refused after the failure"
    );
    assert_eq!(q.sh_unavailable_message(), "scripthash index rebuilding");
}

/// The freeze outlives the unlink batch: from the batch until the strong bits
/// are cleared the tip has shrunk but the creates are gone from the index, so a
/// reader entering then (with a guard that is still valid) must be refused, for
/// a good batch and a failed one alike. Readers return after a good one.
#[test]
fn readers_stay_out_until_the_strong_bits_are_cleared() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let (_dir, q, _c, key) = heavy_chain("sh-degraded-window");
    let seen: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
    let s2 = seen.clone();
    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |stage, q| {
        if stage == "before_unstrong" {
            s2.borrow_mut().push(q.sh_read_enter().is_none());
        }
        Ok(())
    })));
    q.disconnect_tip().unwrap();
    assert_eq!(*seen.borrow(), vec![true], "refused before set_unstrong");
    assert!(!q.sh_degraded());
    assert!(!q.sh_frozen(), "released after the strong bits");
    let g = q.sh_read_enter().expect("readers return");
    assert!(g.still_valid());
    drop(g);

    seen.borrow_mut().clear();
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);
    assert_eq!(*seen.borrow(), vec![true]);
    assert!(q.sh_degraded());
    assert!(q.sh_read_enter().is_none(), "a failed batch stays refused");
}

/// A freeze that ends without `release` (the disconnect bailed out between the
/// unlink and the strong-bit clear) leaves the index degraded.
#[test]
fn an_abandoned_unlink_window_degrades_the_index() {
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-abandon");
    let f = q.freeze_sh_readers();
    assert!(q.sh_frozen());
    drop(f);
    assert!(!q.sh_frozen());
    assert!(q.sh_degraded(), "unreleased freeze degrades");
    assert!(q.sh_read_enter().is_none());
}

/// Gap 7: the marker is published like the store's other small durable files
/// (synced temp file, rename, directory fsync) and goes only after the
/// replacement head's SEAL and `include_hwm` are durable, which takes a
/// directory fsync on both directories those two files live in.
#[test]
fn marker_is_published_durably_and_removed_after_the_head_is_durable() {
    let (dir, q, _c, key) = heavy_chain("sh-degraded-durable");
    let store = q.store().path().to_path_buf();
    let runs = store.join("scripthash.runs");
    rbitcoin_store::file::arm_fsync_dir_trace();
    q.mark_sh_degraded("fault");
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    assert!(
        synced.contains(&store),
        "marker rename needs a directory fsync"
    );
    assert!(
        !store.join("scripthash.degraded.tmp").exists(),
        "no temp file left behind"
    );
    assert_eq!(
        std::fs::read(store.join("scripthash.degraded")).unwrap(),
        b"fault"
    );

    // The rebuild removes the marker only after both dirs were synced.
    rbitcoin_store::file::arm_fsync_dir_trace();
    rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |stage, q| {
        if stage == "after_collect" {
            assert!(q.sh_degraded_marker_present());
        }
        Ok(())
    })));
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
    rbitcoin_query::set_sh_rebuild_hook_for_test(None);
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    assert!(synced.contains(&runs), "SEAL dir synced: {synced:?}");
    assert_eq!(
        synced.last(),
        Some(&store),
        "the marker's removal is the last thing synced"
    );
    assert!(!q.sh_degraded_marker_present());
    assert!(q.store().scripthash.include_hwm() > 0);
    assert!(q.scripthash_create_count(&key).unwrap() >= HEAVY);
    drop(dir);
}

/// Gap 9: the supervisor's retry wait ends at `stop`. A failed rebuild used to
/// hold process exit for the whole backoff (5 s, growing to 300 s).
#[test]
fn supervisor_retry_wait_ends_at_stop() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-sup-stop");
    let q = Arc::new(q);
    q.inject_sh_rebuild_failures_for_test(1000);
    q.mark_sh_degraded("fault");
    let stop = Arc::new(AtomicBool::new(false));
    let t = rbitcoin_query::spawn_sh_rebuild_supervisor(q.clone(), stop.clone());
    // The first pass consumed an injected failure: the backoff wait is next.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while q.sh_rebuild_failures_left_for_test() == 1000 {
        assert!(std::time::Instant::now() < deadline, "no rebuild pass ran");
        std::thread::yield_now();
    }
    let t0 = std::time::Instant::now();
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "stop waited out the backoff: {:?}",
        t0.elapsed()
    );
    assert!(q.sh_degraded(), "a failed rebuild leaves it degraded");
}

/// The supervisor rebuilds a latched fault on its own and stops when told.
#[test]
fn supervisor_rebuilds_a_latched_fault_and_stops() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let (_dir, q, _c, key) = heavy_chain("sh-degraded-sup");
    let q = Arc::new(q);
    tear_extent_head(&q, &key);
    assert!(q.scripthash_history(&key).is_err(), "the read latches");
    let stop = Arc::new(AtomicBool::new(false));
    let t = rbitcoin_query::spawn_sh_rebuild_supervisor(q.clone(), stop.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while q.sh_degraded() {
        assert!(
            std::time::Instant::now() < deadline,
            "supervisor never rebuilt"
        );
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
    assert!(q.scripthash_create_count(&key).unwrap() >= HEAVY);
}

/// One supervisor pass: auto-rebuild off leaves the index for a restart, a
/// failure doubles the backoff, a success resets it, and a degraded flag with
/// no index to rebuild neither spins nor changes the backoff.
#[test]
fn supervisor_pass_branches() {
    use rbitcoin_query::sh_degraded::sh_rebuild_pass;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    let (_dir, q, _c, _key) = heavy_chain("sh-degraded-pass");
    let stop = AtomicBool::new(false);
    let mut backoff = Duration::from_millis(1);
    q.mark_sh_degraded("fault");
    q.set_sh_auto_rebuild(false);
    sh_rebuild_pass(&q, &stop, &mut backoff);
    assert!(q.sh_degraded(), "auto-rebuild off: left for a restart");
    q.set_sh_auto_rebuild(true);
    q.inject_sh_rebuild_failures_for_test(1);
    sh_rebuild_pass(&q, &stop, &mut backoff);
    assert!(q.sh_degraded());
    assert_eq!(
        backoff,
        Duration::from_millis(2),
        "failure doubles the wait"
    );
    sh_rebuild_pass(&q, &stop, &mut backoff);
    assert!(!q.sh_degraded());
    assert_eq!(backoff, Duration::from_secs(5), "success resets it");
    q.set_sh_index_enabled(false);
    q.mark_sh_degraded("no index");
    sh_rebuild_pass(&q, &stop, &mut backoff);
    assert!(q.sh_degraded());
    assert_eq!(backoff, Duration::from_secs(5));
}
