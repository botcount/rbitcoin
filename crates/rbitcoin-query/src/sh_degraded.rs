//! Degraded scripthash index.
//!
//! The SH index is optional. A fault in it (a torn extent page after ENOSPC,
//! a failed unlink, a failed write-behind append, a history read that trips
//! over a torn key) must not stop the chain. [`Query::mark_sh_degraded`]
//! records the fault, publishes a durable marker next to the store, and turns
//! SH work into a no-op: connect and disconnect keep going with SH deferred
//! (the `IndexMode::Direct` behaviour), address routes answer 503
//! ([`SCRIPTHASH_INDEX_REBUILDING`]), and [`Query::rebuild_sh_if_degraded`]
//! waits for in-flight reads, wipes the index and rebuilds it from Class A in
//! the background. The marker also makes a restart wipe the broken index at
//! open, so a node that cannot reach tip never has to. It goes only after the
//! replacement head's SEAL and `include_hwm` are durable.
//!
//! Readers hold a [`ShReadGuard`] for the whole read. The guard carries a
//! generation that the fault, the rebuild and every unlink batch bump, so a
//! read that overlapped any of them answers 503 even if the flag is clear by
//! the time it returns.
//!
//! No schema change: the marker is a plain file, not part of the store format.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How often a wait that must end at `stop` looks at it.
const STOP_POLL: Duration = Duration::from_millis(100);
/// First retry wait after a failed rebuild; doubles up to [`REBUILD_BACKOFF_MAX`].
const REBUILD_BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const REBUILD_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Pause when degraded but there is nothing the supervisor may do.
const IDLE_WAIT: Duration = Duration::from_millis(250);

/// Reader bookkeeping shared by the guards and the rebuild.
#[derive(Default)]
struct ShReaders {
    /// Requests that passed the SH gate and are still reading.
    n: AtomicUsize,
    /// Bumped whenever the index changes under readers: degrade, rebuild end,
    /// each unlink batch's start and end.
    generation: AtomicU64,
    /// Unlink batches in progress: readers are refused while it is not 0.
    frozen: AtomicUsize,
    m: Mutex<()>,
    cv: Condvar,
}

/// Held for the whole SH read of one request. The rebuild's wipe waits for
/// every guard taken before the index degraded, so no reader walks the head
/// while it is reset and refilled underneath it. `still_valid` says whether
/// anything changed under the read: when it does not, the answer is a 503.
/// `Send`: an async handler can hold it across awaits.
pub struct ShReadGuard {
    readers: Arc<ShReaders>,
    seen: u64,
}

impl ShReadGuard {
    /// True when no fault, rebuild step or unlink batch began or ended since
    /// this guard was taken. A read judged after this returns false must not
    /// be answered.
    pub fn still_valid(&self) -> bool {
        self.readers.generation.load(Ordering::SeqCst) == self.seen
    }
}

impl Drop for ShReadGuard {
    fn drop(&mut self) {
        if self.readers.n.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _g = self.readers.m.lock().unwrap_or_else(|e| e.into_inner());
            self.readers.cv.notify_all();
        }
    }
}

/// Readers are refused while an unlink batch rewrites keys, and a read that
/// overlapped it fails its guard. Dropping it ends the refusal.
pub(crate) struct ShFreeze<'a> {
    query: &'a Query,
}

impl Drop for ShFreeze<'_> {
    fn drop(&mut self) {
        let r = &self.query.sh_degraded.readers;
        // Generation first: a reader that sees the batch over has the new one.
        r.generation.fetch_add(1, Ordering::SeqCst);
        r.frozen.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 503 body while the SH index is degraded and being rebuilt.
pub const SCRIPTHASH_INDEX_REBUILDING: &str = "scripthash index rebuilding";

/// 503 body for a read that overlapped a disconnect's unlink batch.
pub const SCRIPTHASH_INDEX_UPDATING: &str = "scripthash index updating";

/// Marker file name inside the store directory.
pub const SH_DEGRADED_MARKER: &str = "scripthash.degraded";

/// A store error that says the scripthash tables are bad (a torn page, a
/// failed read or write), not that a request was refused or cancelled.
pub(crate) fn is_sh_fault(e: &StoreError) -> bool {
    matches!(e, StoreError::Corrupt(_) | StoreError::Io { .. })
}

/// In-memory degraded state.
pub struct ShDegraded {
    flag: AtomicBool,
    reason: Mutex<Option<String>>,
    wake: Mutex<()>,
    cv: Condvar,
    auto: AtomicBool,
    readers: Arc<ShReaders>,
    #[cfg(debug_assertions)]
    fail_rebuilds: AtomicUsize,
}

impl Default for ShDegraded {
    fn default() -> Self {
        Self {
            flag: AtomicBool::new(false),
            reason: Mutex::new(None),
            wake: Mutex::new(()),
            cv: Condvar::new(),
            auto: AtomicBool::new(true),
            readers: Arc::new(ShReaders::default()),
            #[cfg(debug_assertions)]
            fail_rebuilds: AtomicUsize::new(0),
        }
    }
}

impl Query {
    /// True while the SH index is known bad (or being rebuilt).
    #[inline]
    pub fn sh_degraded(&self) -> bool {
        self.sh_degraded.flag.load(Ordering::Acquire)
    }

    /// True while a disconnect's unlink batch is rewriting scripthash keys.
    #[inline]
    pub fn sh_frozen(&self) -> bool {
        self.sh_degraded.readers.frozen.load(Ordering::SeqCst) > 0
    }

    fn sh_bump_generation(&self) {
        self.sh_degraded
            .readers
            .generation
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Admit one SH read. `None` when the index is off, degraded or frozen
    /// (answer 503). The count goes up and the generation is read before the
    /// flags, and the rebuild sets the flag before it waits for the count, so
    /// a reader is either refused, waited for, or finds its guard stale.
    pub fn sh_read_enter(&self) -> Option<ShReadGuard> {
        let readers = Arc::clone(&self.sh_degraded.readers);
        readers.n.fetch_add(1, Ordering::SeqCst);
        let seen = readers.generation.load(Ordering::SeqCst);
        let g = ShReadGuard { readers, seen };
        self.sh_history_available().then_some(g)
    }

    /// Freeze readers for one unlink batch. The count and generation move
    /// before the batch touches a key.
    pub(crate) fn freeze_sh_readers(&self) -> ShFreeze<'_> {
        let r = &self.sh_degraded.readers;
        r.frozen.fetch_add(1, Ordering::SeqCst);
        r.generation.fetch_add(1, Ordering::SeqCst);
        ShFreeze { query: self }
    }

    /// Readers that passed the gate and have not finished.
    #[doc(hidden)]
    pub fn sh_readers_in_flight(&self) -> usize {
        self.sh_degraded.readers.n.load(Ordering::SeqCst)
    }

    /// Fail the next `n` rebuild passes at their start, as a broken disk would.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn inject_sh_rebuild_failures_for_test(&self, n: usize) {
        self.sh_degraded.fail_rebuilds.store(n, Ordering::SeqCst);
    }

    /// Injected failures not yet consumed.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn sh_rebuild_failures_left_for_test(&self) -> usize {
        self.sh_degraded.fail_rebuilds.load(Ordering::SeqCst)
    }

    /// Wait until no reader is in flight. The flag is already set, so no new
    /// one is admitted and this ends when the last one drops its guard. There
    /// is no give-up: a wipe under a reader would hand it garbage. `cancel`
    /// ends the wait with `Cancelled`.
    fn drain_sh_readers(&self, cancel: Option<&AtomicBool>) -> Result<(), QueryError> {
        let r = &self.sh_degraded.readers;
        let mut announced = false;
        loop {
            let g = r.m.lock().unwrap_or_else(|e| e.into_inner());
            if r.n.load(Ordering::SeqCst) == 0 {
                return Ok(());
            }
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(StoreError::Cancelled(
                    "scripthash rebuild stopped while waiting for readers",
                ));
            }
            if !announced {
                announced = true;
                drop(g);
                #[cfg(debug_assertions)]
                sh_stage_hook("drain_wait", self);
                continue;
            }
            let _ = r.cv.wait_timeout(g, STOP_POLL);
        }
    }

    /// Why the index was marked degraded.
    pub fn sh_degraded_reason(&self) -> Option<String> {
        self.sh_degraded
            .reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the node supervisor rebuilds on its own (default on).
    pub fn sh_auto_rebuild(&self) -> bool {
        self.sh_degraded.auto.load(Ordering::Relaxed)
    }

    pub fn set_sh_auto_rebuild(&self, on: bool) {
        self.sh_degraded.auto.store(on, Ordering::Relaxed);
    }

    fn sh_degraded_marker(&self) -> std::path::PathBuf {
        self.store.path().join(SH_DEGRADED_MARKER)
    }

    /// True when a previous run left the marker.
    pub fn sh_degraded_marker_present(&self) -> bool {
        self.sh_degraded_marker().exists()
    }

    /// Mark the SH index degraded. Idempotent. Drops queued SH work; the chain
    /// carries on without SH until the rebuild flips the flag back.
    pub fn mark_sh_degraded(&self, reason: &str) {
        let first = !self.sh_degraded.flag.swap(true, Ordering::AcqRel);
        if first {
            // After the flag: a reader that reads this generation also reads
            // the flag set, so it is refused rather than admitted unchecked.
            self.sh_bump_generation();
            rbitcoin_log::error!(
                "scripthash index DEGRADED: {reason}; chain continues without SH, \
                 address routes answer 503 until the index is rebuilt"
            );
            *self
                .sh_degraded
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(reason.to_owned());
            let path = self.sh_degraded_marker();
            if let Err(e) = rbitcoin_store::file::write_synced_tmp_rename(&path, reason.as_bytes())
            {
                rbitcoin_log::error!(
                    "scripthash degraded marker {}: {e}; a restart will not wipe the index",
                    path.display()
                );
            }
        }
        // Queued jobs would append onto a bad head.
        self.sh.pending.lock().unwrap().clear();
        *self.sh.applying.lock().unwrap() = None;
        self.sh.ram_head.lock().unwrap().clear();
        let _w = self
            .sh_degraded
            .wake
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.sh_degraded.cv.notify_all();
        self.sh.pending_cv.notify_all();
    }

    /// Run one scripthash read for a request. A fault is latched only when it
    /// survives a second read with the appender held out: an append rewrites a
    /// slab in place, so a read that lands inside it can see a torn row that
    /// is gone a moment later, and that must not wipe the index. The error of
    /// a fault that holds still goes back to the caller, whose guard is then
    /// stale.
    #[doc(hidden)]
    pub fn sh_read_latching<T>(
        &self,
        read: impl Fn() -> Result<T, StoreError>,
    ) -> Result<T, QueryError> {
        let first = read();
        if !first.as_ref().is_err_and(is_sh_fault) {
            return first;
        }
        let _quiet = self.sh.appender.lock().unwrap();
        let second = read();
        if let Err(e) = &second {
            if is_sh_fault(e) {
                self.mark_sh_degraded(&format!("read: {e}"));
            }
        }
        second
    }

    /// `ScriptHashTable::create_fks` for a request: a torn key degrades the
    /// index instead of failing every read of it until something else notices.
    pub(crate) fn sh_create_fks(&self, scripthash: &[u8; 32]) -> Result<Vec<Fk>, QueryError> {
        self.sh_read_latching(|| self.store.scripthash.create_fks(scripthash))
    }

    /// `ScriptHashTable::create_count` for a request, latching like
    /// [`Self::sh_create_fks`].
    pub(crate) fn sh_create_count(&self, scripthash: &[u8; 32]) -> Result<u32, QueryError> {
        self.sh_read_latching(|| self.store.scripthash.create_count(scripthash))
    }

    /// Settle one write-behind append. A scripthash `Corrupt` or I/O error
    /// degrades the index and drops the job (`Ok(false)`); any other error goes
    /// back to the write-behind loop, which requeues the job and halts.
    #[doc(hidden)]
    pub fn settle_sh_apply(
        &self,
        height: Height,
        r: Result<(), QueryError>,
    ) -> Result<bool, QueryError> {
        match r {
            Ok(()) => Ok(true),
            Err(e) if is_sh_fault(&e) => {
                self.mark_sh_degraded(&format!("write-behind h={}: {e}", height.0));
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Block until degraded or `timeout`. For the rebuild supervisor.
    pub fn wait_sh_degraded(&self, timeout: Duration) -> bool {
        let g = self
            .sh_degraded
            .wake
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.sh_degraded() {
            return true;
        }
        let _ = self.sh_degraded.cv.wait_timeout(g, timeout);
        self.sh_degraded()
    }

    /// Wait `d` on the degrade condvar, looking at `stop` every
    /// `STOP_POLL`, and return as soon as it is set. Wakes from a new
    /// degrade do not cut the wait short.
    pub fn sh_wait_unless_stopped(&self, d: Duration, stop: &AtomicBool) {
        let end = std::time::Instant::now() + d;
        while !stop.load(Ordering::Relaxed) {
            let Some(left) = end.checked_duration_since(std::time::Instant::now()) else {
                return;
            };
            let g = self
                .sh_degraded
                .wake
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _ = self.sh_degraded.cv.wait_timeout(g, left.min(STOP_POLL));
        }
    }

    /// SH 503 / error text for the current state.
    pub fn sh_unavailable_message(&self) -> &'static str {
        if self.sh_degraded() {
            SCRIPTHASH_INDEX_REBUILDING
        } else if self.sh_index_enabled() {
            SCRIPTHASH_INDEX_UPDATING
        } else {
            SCRIPTHASH_INDEX_DISABLED
        }
    }

    fn reset_sh_memory(&self) {
        self.set_sh_indexed_through_height(None);
        self.sh.applied_max_fk.store(0, Ordering::Release);
        self.sh.pending.lock().unwrap().clear();
        *self.sh.applying.lock().unwrap() = None;
        self.sh.ram_head.lock().unwrap().clear();
    }

    fn wipe_sh_for_rebuild(&self) -> Result<(), QueryError> {
        self.sh_run.prepare_force_full_rebuild(&self.store)?;
        rbitcoin_store::clear_unsorted_shard_dir(&rbitcoin_store::unsorted_shard_dir(
            self.store.path(),
        ));
        self.reset_sh_memory();
        Ok(())
    }

    /// Make the head's SEAL and `include_hwm` durable: sync the SH tables,
    /// then the directories their renames landed in.
    fn sync_sh_publish(&self) -> Result<(), QueryError> {
        self.store.scripthash.flush()?;
        for dir in [self.store.path(), self.sh_run.runs_dir.as_path()] {
            rbitcoin_store::file::fsync_parent_dir(dir).map_err(|e| StoreError::io(dir, e))?;
        }
        Ok(())
    }

    /// Remove the marker and sync its removal. Missing is fine.
    fn clear_sh_degraded_marker(&self) -> Result<(), QueryError> {
        let path = self.sh_degraded_marker();
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(StoreError::io(&path, e)),
        }
        let dir = self.store.path();
        rbitcoin_store::file::fsync_parent_dir(dir).map_err(|e| StoreError::io(dir, e))
    }

    /// A packed head with a durable inclusion floor (or no creates to cover).
    fn sh_replacement_head_published(&self) -> bool {
        if !self.store.scripthash.has_durable_index() {
            return false;
        }
        self.sh_run.refresh_seal();
        self.store.txs.count() == 0
            || crate::sh_builder::durable_sh_inclusion_floor(
                self.store.scripthash.include_hwm(),
                self.sh_run.sealed_max_create_fk(),
            ) > 0
    }

    /// Drop a marker left by a previous run once the cold rebuild it caused
    /// has sealed a head: the head's SEAL and `include_hwm` are synced first.
    /// Runs at the end of tip-entry finalize. A marker kept until then means a
    /// kill between the open wipe and this point wipes again at the next open.
    pub(crate) fn settle_sh_marker_after_finalize(&self) -> Result<(), QueryError> {
        if self.sh_degraded() || !self.sh_degraded_marker_present() {
            return Ok(());
        }
        if !self.sh_replacement_head_published() {
            return Ok(());
        }
        self.sync_sh_publish()?;
        self.clear_sh_degraded_marker()
    }

    /// Open-time wipe. Runs before IBD / resume, so a node whose SH head is
    /// broken (marker left by a previous run, or `RBITCOIN_SH_FORCE_REBUILD`)
    /// never has to reach tip to start the rebuild. The start then proceeds as
    /// a cold SH build: Direct, then Class A collect + pack at tip entry. The
    /// marker stays until that build has sealed a head
    /// (`settle_sh_marker_after_finalize`). Returns true when it wiped.
    pub fn startup_sh_rebuild(&self, shindex: bool) -> Result<bool, QueryError> {
        if !shindex {
            return Ok(false);
        }
        let marked = self.sh_degraded_marker_present();
        if !marked && !self.sh_force_rebuild() {
            return Ok(false);
        }
        rbitcoin_log::warn!(
            "scripthash index: wiping at open ({}); cold rebuild follows at tip entry",
            if marked {
                "degraded marker from a previous run"
            } else {
                "RBITCOIN_SH_FORCE_REBUILD"
            }
        );
        self.wipe_sh_for_rebuild()?;
        self.sh_degraded.flag.store(false, Ordering::Release);
        Ok(true)
    }

    /// Rebuild a degraded index in place while the node keeps running.
    ///
    /// Waits for every in-flight reader, wipes the SH tables under the
    /// appender lock, runs the Class A collect + pack (SH enqueue is off while
    /// degraded), syncs the new head's SEAL and `include_hwm`, then re-arms
    /// write-behind from the new `include_hwm` exactly as a restart does and
    /// removes the marker. Returns `Ok(false)` when there was nothing to do.
    /// On error the node stays degraded and the caller may retry. Cancel
    /// leaves it degraded.
    pub fn rebuild_sh_if_degraded(&self, cancel: Option<&AtomicBool>) -> Result<bool, QueryError> {
        if !self.sh_degraded() || !self.sh_index_enabled() {
            return Ok(false);
        }
        rbitcoin_log::warn!("scripthash index: rebuild start (wipe, then Class A collect + pack)");
        let t0 = std::time::Instant::now();
        #[cfg(debug_assertions)]
        if take_injected_failure(&self.sh_degraded.fail_rebuilds) {
            return Err(StoreError::Corrupt("injected rebuild failure"));
        }
        // The flag is set, so no new reader is admitted; wait out the others.
        self.drain_sh_readers(cancel)?;
        {
            let _appender = self.sh.appender.lock().unwrap();
            self.wipe_sh_for_rebuild()?;
        }
        #[cfg(debug_assertions)]
        rebuild_test_hook("after_wipe", self)?;
        self.finalize_sh_unsorted_cancellable(cancel)?;
        #[cfg(debug_assertions)]
        rebuild_test_hook("after_collect", self)?;
        {
            let _commit = self.lock_tip_commit()?;
            let appender = self.sh.appender.lock().unwrap();
            self.sh_run.refresh_seal();
            self.sync_sh_seal_from_include_hwm()?;
            self.sync_sh_publish()?;
            let through = self.recover_sh_indexed_through()?;
            self.set_sh_indexed_through_height(through);
            self.sh_bump_generation();
            self.sh_degraded.flag.store(false, Ordering::Release);
            drop(appender);
            if let Err(e) = self.recover_sh_writebehind() {
                // Re-arm failed: serve nothing and let the next pass retry.
                self.mark_sh_degraded(&format!("rebuild re-arm failed: {e}"));
                return Err(e);
            }
            if let Err(e) = self.clear_sh_degraded_marker() {
                // Kept: the next open wipes a healthy index, which is safe.
                rbitcoin_log::warn!("scripthash degraded marker not removed: {e}");
            }
            *self
                .sh_degraded
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
        rbitcoin_log::warn!(
            "scripthash index: rebuild done in {:?}; address routes serve again",
            t0.elapsed()
        );
        Ok(true)
    }
}

#[cfg(debug_assertions)]
type RebuildHook = Box<dyn FnMut(&str, &Query) -> Result<(), QueryError>>;

#[cfg(debug_assertions)]
thread_local! {
    static REBUILD_HOOK: std::cell::RefCell<Option<RebuildHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(debug_assertions)]
fn rebuild_test_hook(stage: &str, q: &Query) -> Result<(), QueryError> {
    // Taken out while it runs so a hook that re-enters cannot double-borrow.
    if let Some(mut h) = REBUILD_HOOK.with(|c| c.borrow_mut().take()) {
        let r = h(stage, q);
        REBUILD_HOOK.with(|c| *c.borrow_mut() = Some(h));
        return r;
    }
    Ok(())
}

/// Run `hook(stage, query)` at `"after_wipe"` and `"after_collect"` inside
/// [`Query::rebuild_sh_if_degraded`] on this thread, so a test can connect or
/// disconnect blocks in the windows where the rebuild holds no tip lock. An
/// `Err` from the hook aborts the rebuild there, as a kill would: the marker
/// and the half-built tables stay as they are. Two more stages ignore the
/// hook's result: `"drain_wait"` (the rebuild found a reader in flight and is
/// about to wait for it) and `"unlink_frozen"` (a disconnect froze readers
/// and is about to run its unlink batch).
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn set_sh_rebuild_hook_for_test(hook: Option<RebuildHook>) {
    REBUILD_HOOK.with(|c| *c.borrow_mut() = hook);
}

/// Run the test hook at `stage` where its result is not wanted.
#[cfg(debug_assertions)]
pub(crate) fn sh_stage_hook(stage: &str, q: &Query) {
    let _ = rebuild_test_hook(stage, q);
}

/// One supervisor pass while the index is degraded. `backoff` is the wait
/// after the next failure: it doubles on a failure and resets on a rebuild.
/// Every wait ends at `stop`.
#[doc(hidden)]
pub fn sh_rebuild_pass(query: &Query, stop: &AtomicBool, backoff: &mut Duration) {
    if !query.sh_auto_rebuild() {
        query.sh_wait_unless_stopped(IDLE_WAIT, stop);
        return;
    }
    match query.rebuild_sh_if_degraded(Some(stop)) {
        Ok(true) => *backoff = REBUILD_BACKOFF_INITIAL,
        // Flagged but not rebuildable (index switched off): do not spin.
        Ok(false) => query.sh_wait_unless_stopped(IDLE_WAIT, stop),
        Err(StoreError::Cancelled(_)) => {}
        Err(e) => {
            rbitcoin_log::error!("scripthash index rebuild failed: {e}; retrying");
            query.sh_wait_unless_stopped(*backoff, stop);
            *backoff = (*backoff * 2).min(REBUILD_BACKOFF_MAX);
        }
    }
}

/// Supervisor: rebuild whenever the index degrades. One thread per node. It
/// is joined at shutdown, so every wait here ends at `stop`.
pub fn spawn_sh_rebuild_supervisor(
    query: std::sync::Arc<Query>,
    stop: std::sync::Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rbtc-sh-rebuild".into())
        .spawn(move || {
            let mut backoff = REBUILD_BACKOFF_INITIAL;
            while !stop.load(Ordering::Relaxed) {
                if query.wait_sh_degraded(STOP_POLL) {
                    sh_rebuild_pass(&query, &stop, &mut backoff);
                }
            }
        })
        .expect("spawn sh rebuild supervisor")
}

/// Consume one armed injected failure, if any (test-only fault arm). Written as a
/// compare-exchange loop because `fetch_update` is deprecated on the forward
/// compiler and `try_update` is not available at the product MSRV.
#[cfg(debug_assertions)]
fn take_injected_failure(arm: &std::sync::atomic::AtomicUsize) -> bool {
    let mut cur = arm.load(Ordering::SeqCst);
    while cur > 0 {
        match arm.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => cur = seen,
        }
    }
    false
}
