//! A fault in the scripthash index must not wedge the chain.
//!
//! The fault: the head of a big (extent) key
//! points at a page whose `ver` byte is gone, so every full read of the key
//! fails with `scripthash extent last page not ver=2`.

use rbitcoin_primitives::{merkle_root_from_txids, Fk, Height};
use rbitcoin_query::testutil::{tiny_query_labeled, FixtureChain, TempDir};
use rbitcoin_query::{Query, TxApply};
use rbitcoin_store::{
    block_header_hash, script_hash, HeaderRecord, InputRecord, OutputRecord, ShHeadValue, TxRecord,
};

const KEY_SCRIPT: &[u8] = &[0x51, 0x51];
/// Enough creates on one key to move it from a slab to an extent chain.
const HEAVY: u32 = 5000;

fn header(
    h: u32,
    nonce: u32,
    prev_fk: Fk,
    prev_hash: Option<[u8; 32]>,
    txs: &[TxApply],
) -> HeaderRecord {
    // A real merkle root, so a reopen (tip revalidate) keeps the test chain.
    let leaves: Vec<[u8; 32]> = txs.iter().map(|t| t.tx.txid).collect();
    let merkle = if leaves.is_empty() {
        let mut m = [0u8; 32];
        m[0..4].copy_from_slice(&h.to_le_bytes());
        m
    } else {
        merkle_root_from_txids(&leaves)
    };
    let hash = match prev_hash {
        None => block_header_hash(1, &[0u8; 32], &merkle, h + 1, 0x207f_ffff, nonce),
        Some(ph) => block_header_hash(1, &ph, &merkle, h + 1, 0x207f_ffff, nonce),
    };
    HeaderRecord {
        prev_fk,
        version: 1,
        timestamp: h + 1,
        bits: 0x207f_ffff,
        nonce,
        merkle_root: merkle,
        hash,
        size: 0,
        weight: 0,
    }
}

fn pay(txid_seed: u32, script: &[u8]) -> TxApply {
    let mut txid = [0u8; 32];
    txid[..4].copy_from_slice(&txid_seed.to_le_bytes());
    txid[31] = 0xd9;
    TxApply {
        tx: TxRecord {
            txid,
            version: 2,
            locktime: 0,
            input_count: 1,
            output_count: 1,
        },
        inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x01], vec![])],
        outputs: vec![OutputRecord::unspent(1, script.to_vec())],
    }
}

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

/// Zero the `ver` byte of the extent's last page: the torn head that wedged the chain.
fn tear_extent_head(q: &Query, key: &[u8; 32]) {
    let t = &q.store().scripthash;
    let last_page = match t.head_value(key).unwrap().unwrap() {
        ShHeadValue::Extent { last_page } => last_page,
        other => panic!("heavy key is not an extent: {other:?}"),
    };
    let mut page = [0u8; 4096];
    t.body().read_at(last_page, &mut page).unwrap();
    page[0] = 0;
    t.body().write_at(last_page, &page).unwrap();
}

fn heavy_chain(label: &str) -> (TempDir, Query, Chain, [u8; 32]) {
    let (dir, q) = tiny_query_labeled(label);
    // Node shape: connect with SH deferred (Direct), then cold collect +
    // pack, which stores the big key as an extent chain. Then follow the tip.
    q.enter_direct_index_mode().unwrap();
    let mut c = Chain::new();
    c.block(&q, 1, 0);
    c.block(&q, HEAVY, 0);
    c.block(&q, 3, 0);
    q.finalize_sh_runs().unwrap();
    q.enter_tip_index_mode();
    let key = script_hash(KEY_SCRIPT);
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
    assert!(!q.sh_degraded_marker_present());
    assert!(!q.sh_degraded());
    assert!(q.store().scripthash.head_is_empty(), "head wiped at open");
    assert_eq!(q.store().scripthash.create_count(&key).unwrap(), 0);
    // Nothing to do on a healthy second start.
    assert!(!q.startup_sh_rebuild(true).unwrap());
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
    let (_dir, q) = tiny_query_labeled("sh-degraded-orphan");
    q.enter_direct_index_mode().unwrap();
    let mut c = Chain::new();
    c.block(&q, 1, 0);
    c.block(&q, HEAVY, 0);
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    q.finalize_sh_runs().unwrap();
    q.enter_tip_index_mode();
    let key = script_hash(KEY_SCRIPT);
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
    let (_dir, q) = tiny_query_labeled("sh-degraded-orphan2");
    q.enter_direct_index_mode().unwrap();
    let mut c = Chain::new();
    c.block(&q, 1, 0);
    c.block(&q, HEAVY, 0);
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    q.finalize_sh_runs().unwrap();
    q.enter_tip_index_mode();
    let key = script_hash(KEY_SCRIPT);
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
    let (_dir, q) = tiny_query_labeled(&format!("sh-degraded-reorg-{stage}"));
    q.enter_direct_index_mode().unwrap();
    let mut c = Chain::new();
    c.block(&q, 1, 0);
    c.block(&q, HEAVY, 0);
    let (h2, txs2) = c.make(3, 0);
    c.apply(&q, &h2, &txs2);
    q.finalize_sh_runs().unwrap();
    q.enter_tip_index_mode();
    let key = script_hash(KEY_SCRIPT);
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
    let (_dir, q) = tiny_query_labeled(&format!("sh-degraded-crash-{stage}"));
    q.enter_direct_index_mode().unwrap();
    let mut c = Chain::new();
    c.block(&q, 1, 0);
    c.block(&q, HEAVY, 0);
    c.block(&q, 3, 0);
    q.finalize_sh_runs().unwrap();
    q.enter_tip_index_mode();
    let key = script_hash(KEY_SCRIPT);
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
    assert!(!q.sh_degraded_marker_present());
    assert!(!q.sh_degraded());
    q.enter_direct_index_mode().unwrap();
    q.finalize_sh_runs().unwrap();
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

/// Gap 2: requests that passed the 503 gate keep reading while the wipe and
/// rebuild run underneath them. No panic, and an Ok answer is never a torn
/// read: every Ok history has one of the lengths the chain really had.
#[test]
fn readers_through_degrade_wipe_and_rebuild_see_no_garbage() {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    let (_dir, q, mut c, key) = heavy_chain("sh-degraded-stress");
    let q = Arc::new(q);
    let allowed: Arc<Mutex<HashSet<usize>>> = Arc::new(Mutex::new(HashSet::new()));
    let healthy_len = history_txids(&q, &key).len();
    allowed.lock().unwrap().insert(healthy_len);
    let stop = Arc::new(AtomicBool::new(false));
    let (oks, errs, bad) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(Mutex::new(Vec::<String>::new())),
    );
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let (q, stop, allowed, oks, errs, bad) = (
                q.clone(),
                stop.clone(),
                allowed.clone(),
                oks.clone(),
                errs.clone(),
                bad.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // The server gate: checked once, then the read runs free.
                    if !q.sh_history_available() {
                        std::thread::yield_now();
                        continue;
                    }
                    match q.scripthash_history(&key) {
                        Ok(rows) => {
                            let ids: Vec<[u8; 32]> = rows.iter().map(|r| r.txid).collect();
                            let n = ids.len();
                            let mut s = ids.clone();
                            s.sort();
                            s.dedup();
                            if s.len() != n {
                                bad.lock().unwrap().push(format!("duplicate rows n={n}"));
                            } else if !allowed.lock().unwrap().contains(&n) {
                                bad.lock().unwrap().push(format!("torn length {n}"));
                            }
                            oks.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errs.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    for round in 0..3u32 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Lengths after this round's disconnect and its replacement block.
        let len_now = history_txids(&q, &key).len();
        {
            let mut a = allowed.lock().unwrap();
            a.insert(len_now);
            a.insert(len_now - 3);
            a.insert(len_now - 3 + 2);
            a.insert(len_now + 2);
        }
        tear_extent_head(&q, &key);
        q.disconnect_tip().unwrap();
        assert!(q.sh_degraded());
        assert!(q.rebuild_sh_if_degraded(None).unwrap());
        c.resync_to_tip(&q);
        c.block(&q, 2, 40 + round);
        // After a rebuild the next tip block may carry fewer than 3 txs; the
        // next round's disconnect would then cut 2, so keep it to 3.
        c.block(&q, 3, 50 + round);
        allowed
            .lock()
            .unwrap()
            .insert(history_txids(&q, &key).len());
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().expect("reader thread panicked");
    }
    eprintln!(
        "STRESS ok_reads={} err_reads={} bad={:?}",
        oks.load(Ordering::Relaxed),
        errs.load(Ordering::Relaxed),
        bad.lock().unwrap()
    );
    assert!(bad.lock().unwrap().is_empty(), "{:?}", bad.lock().unwrap());
    assert!(oks.load(Ordering::Relaxed) > 0);
}

/// Gap 2, deterministic: a request that passed the gate before the index
/// degraded holds a read guard; the wipe waits for it instead of resetting the
/// head under its feet. New requests are refused meanwhile and nothing leaks.
#[test]
fn rebuild_wipe_waits_for_in_flight_readers() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let (_dir, q, _c, key) = heavy_chain("sh-degraded-drain");
    let q = Arc::new(q);
    let guard = q.sh_read_enter().expect("healthy index admits a reader");
    tear_extent_head(&q, &key);
    q.disconnect_tip().unwrap();
    assert!(q.sh_degraded());
    assert!(q.sh_read_enter().is_none(), "degraded: refused");

    let wiped = Arc::new(AtomicBool::new(false));
    let (q2, w2) = (q.clone(), wiped.clone());
    let t = std::thread::spawn(move || {
        rbitcoin_query::set_sh_rebuild_hook_for_test(Some(Box::new(move |s, _| {
            if s == "after_wipe" {
                w2.store(true, Ordering::SeqCst);
            }
            Ok(())
        })));
        let r = q2.rebuild_sh_if_degraded(None);
        rbitcoin_query::set_sh_rebuild_hook_for_test(None);
        r
    });
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        !wiped.load(Ordering::SeqCst),
        "wipe ran while a reader was in flight"
    );
    drop(guard);
    assert!(t.join().unwrap().unwrap());
    assert!(wiped.load(Ordering::SeqCst));
    let g = q.sh_read_enter().expect("serves again after the rebuild");
    drop(g);
    // The refused attempt above must not have leaked a count: a second
    // degrade and rebuild goes straight through.
    q.mark_sh_degraded("again");
    assert!(q.rebuild_sh_if_degraded(None).unwrap());
}
