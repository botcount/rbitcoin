//! Degraded scripthash index.
//!
//! The SH index is optional. A fault in it (a torn extent page after ENOSPC,
//! a failed unlink, a failed write-behind append) must not stop the chain.
//! [`Query::mark_sh_degraded`] records the fault, writes a durable marker next
//! to the store, and turns SH work into a no-op: connect and disconnect keep
//! going with SH deferred (the `IndexMode::Direct` behaviour), address routes
//! answer 503 ([`SCRIPTHASH_INDEX_REBUILDING`]), and
//! [`Query::rebuild_sh_if_degraded`] wipes the index and rebuilds it from
//! Class A in the background. The marker also makes a restart wipe the broken
//! index at open, so a node that cannot reach tip never has to.
//!
//! No schema change: the marker is a plain file, not part of the store format.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// How long a rebuild waits for requests that passed the gate before the wipe.
const READER_DRAIN_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Requests that passed the SH gate and are still reading.
#[derive(Default)]
struct ShInflight {
    n: AtomicUsize,
    m: Mutex<()>,
    cv: Condvar,
}

/// Held for the whole SH read of one request. The rebuild's wipe waits for
/// every guard taken before the index degraded, so no reader walks the head
/// while it is reset and refilled underneath it. `Send`: an async handler can
/// hold it across awaits.
pub struct ShReadGuard {
    inflight: Arc<ShInflight>,
}

impl Drop for ShReadGuard {
    fn drop(&mut self) {
        if self.inflight.n.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _g = self.inflight.m.lock().unwrap_or_else(|e| e.into_inner());
            self.inflight.cv.notify_all();
        }
    }
}

/// 503 body while the SH index is degraded and being rebuilt.
pub const SCRIPTHASH_INDEX_REBUILDING: &str = "scripthash index rebuilding";

/// Marker file name inside the store directory.
pub const SH_DEGRADED_MARKER: &str = "scripthash.degraded";

/// In-memory degraded state.
pub struct ShDegraded {
    flag: AtomicBool,
    reason: Mutex<Option<String>>,
    wake: Mutex<()>,
    cv: Condvar,
    auto: AtomicBool,
    inflight: Arc<ShInflight>,
}

impl Default for ShDegraded {
    fn default() -> Self {
        Self {
            flag: AtomicBool::new(false),
            reason: Mutex::new(None),
            wake: Mutex::new(()),
            cv: Condvar::new(),
            auto: AtomicBool::new(true),
            inflight: Arc::new(ShInflight::default()),
        }
    }
}

impl Query {
    /// True while the SH index is known bad (or being rebuilt).
    #[inline]
    pub fn sh_degraded(&self) -> bool {
        self.sh_degraded.flag.load(Ordering::Acquire)
    }

    /// Admit one SH read. `None` when the index is off or degraded (answer
    /// 503). The count goes up before the flag is read, and the rebuild sets
    /// the flag before it waits for the count, so a reader is either refused
    /// or waited for.
    pub fn sh_read_enter(&self) -> Option<ShReadGuard> {
        let inflight = Arc::clone(&self.sh_degraded.inflight);
        inflight.n.fetch_add(1, Ordering::SeqCst);
        let g = ShReadGuard { inflight };
        (self.sh_history_available()).then_some(g)
    }

    fn drain_sh_readers(&self) {
        let inf = &self.sh_degraded.inflight;
        let deadline = std::time::Instant::now() + READER_DRAIN_MAX;
        let mut g = inf.m.lock().unwrap_or_else(|e| e.into_inner());
        while inf.n.load(Ordering::SeqCst) > 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                rbitcoin_log::warn!(
                    "scripthash rebuild: {} reader(s) still in flight after {:?}; wiping anyway",
                    inf.n.load(Ordering::SeqCst),
                    READER_DRAIN_MAX
                );
                return;
            }
            g = inf
                .cv
                .wait_timeout(
                    g,
                    (deadline - now).min(std::time::Duration::from_millis(100)),
                )
                .unwrap_or_else(|e| e.into_inner())
                .0;
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
            if let Err(e) = std::fs::write(&path, reason.as_bytes()) {
                rbitcoin_log::warn!("scripthash degraded marker {}: {e}", path.display());
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

    /// Block until degraded or `timeout`. For the rebuild supervisor.
    pub fn wait_sh_degraded(&self, timeout: std::time::Duration) -> bool {
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

    /// SH 503 / error text for the current state.
    pub fn sh_unavailable_message(&self) -> &'static str {
        if self.sh_degraded() {
            SCRIPTHASH_INDEX_REBUILDING
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

    /// Open-time wipe. Runs before IBD / resume, so a node whose SH head is
    /// broken (marker left by a previous run, or `RBITCOIN_SH_FORCE_REBUILD`)
    /// never has to reach tip to start the rebuild. The start then proceeds as
    /// a cold SH build: Direct, then Class A collect + pack at tip entry.
    /// Returns true when it wiped.
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
        let _ = std::fs::remove_file(self.sh_degraded_marker());
        self.sh_degraded.flag.store(false, Ordering::Release);
        Ok(true)
    }

    /// Rebuild a degraded index in place while the node keeps running.
    ///
    /// Wipes the SH tables under the appender lock, runs the Class A collect +
    /// pack (SH enqueue is off while degraded), then re-arms write-behind from
    /// the new `include_hwm` exactly as a restart does. Returns `Ok(false)` when
    /// there was nothing to do. On error the node stays degraded and the caller
    /// may retry. Cancel leaves it degraded.
    pub fn rebuild_sh_if_degraded(
        &self,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, QueryError> {
        if !self.sh_degraded() || !self.sh_index_enabled() {
            return Ok(false);
        }
        rbitcoin_log::warn!("scripthash index: rebuild start (wipe, then Class A collect + pack)");
        let t0 = std::time::Instant::now();
        // The flag is set, so no new reader is admitted; let the others finish.
        self.drain_sh_readers();
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
            let through = self.recover_sh_indexed_through()?;
            self.set_sh_indexed_through_height(through);
            self.sh_degraded.flag.store(false, Ordering::Release);
            drop(appender);
            if let Err(e) = self.recover_sh_writebehind() {
                // Re-arm failed: serve nothing and let the next pass retry.
                self.mark_sh_degraded(&format!("rebuild re-arm failed: {e}"));
                return Err(e);
            }
            let _ = std::fs::remove_file(self.sh_degraded_marker());
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
/// and the half-built tables stay as they are.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn set_sh_rebuild_hook_for_test(hook: Option<RebuildHook>) {
    REBUILD_HOOK.with(|c| *c.borrow_mut() = hook);
}

/// Supervisor: rebuild whenever the index degrades. One thread per node.
pub fn spawn_sh_rebuild_supervisor(
    query: std::sync::Arc<Query>,
    stop: std::sync::Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rbtc-sh-rebuild".into())
        .spawn(move || {
            let mut backoff = std::time::Duration::from_secs(5);
            while !stop.load(Ordering::Relaxed) {
                if !query.wait_sh_degraded(std::time::Duration::from_secs(5)) {
                    continue;
                }
                if !query.sh_auto_rebuild() {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                match query.rebuild_sh_if_degraded(Some(&stop)) {
                    Ok(_) => backoff = std::time::Duration::from_secs(5),
                    Err(e) => {
                        rbitcoin_log::error!("scripthash index rebuild failed: {e}; retrying");
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(300));
                    }
                }
            }
        })
        .expect("spawn sh rebuild supervisor")
}
