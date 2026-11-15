//! Tokio **reactor** threads must not wait on std locks, store IO, or scripts.
//!
//! Tokio names both multi-thread workers and `spawn_blocking` threads
//! `tokio-rt-worker`. Blocking-pool work must enter [`BlockingRegion`].

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

tokio::task_local! {
    static API_STORE_BUDGET: Arc<Semaphore>;
    static STORE_BUDGET_WAITERS: Arc<AtomicUsize>;
}

thread_local! {
    static BLOCKING_DEPTH: Cell<u32> = const { Cell::new(0) };
    static HELD_STORE_PERMIT: RefCell<Option<OwnedSemaphorePermit>> = const { RefCell::new(None) };
}

/// Guard: this OS thread may take std locks / store IO (blocking pool or tests).
pub struct BlockingRegion(());

impl BlockingRegion {
    pub fn enter() -> Self {
        BLOCKING_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
        Self(())
    }
}

impl Drop for BlockingRegion {
    fn drop(&mut self) {
        BLOCKING_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

pub fn on_tokio_worker() -> bool {
    thread::current()
        .name()
        .is_some_and(|n| n.starts_with("tokio-rt-worker"))
}

pub fn in_blocking_region() -> bool {
    BLOCKING_DEPTH.with(|d| d.get() > 0)
}

const OFF_POOL_THREAD: &str = "rbtc-off-pool";

thread_local! {
    static ON_OFF_POOL: Cell<bool> = const { Cell::new(false) };
}

fn on_off_pool_thread() -> bool {
    ON_OFF_POOL.with(|c| c.get())
}

fn reject_off_pool_reentry() {
    if on_off_pool_thread() {
        panic!("off_pool re-entered from rbtc-off-pool");
    }
}

fn off_pool_tx() -> mpsc::Sender<Box<dyn FnOnce() + Send>> {
    static TX: OnceLock<mpsc::Sender<Box<dyn FnOnce() + Send>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        thread::Builder::new()
            .name(OFF_POOL_THREAD.into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .expect("rbtc-off-pool");
        tx
    })
    .clone()
}

/// Run `f` on the one `rbtc-off-pool` thread and await it.
///
/// Not the Tokio blocking pool: a parked pool must still finish a connect or
/// a mempool admit. The thread starts on the first call and runs one job at
/// a time. Callers await between jobs, so a job must not call [`off_pool`]
/// again. Dropping the awaiter does not cancel `f`. Script verify stays on
/// the caller via `run_cpu`, outside `f`.
pub async fn off_pool<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    reject_off_pool_reentry();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job = Box::new(move || {
        // The flag lives on this thread. The next job sets it again, and
        // nothing here reads it between jobs.
        ON_OFF_POOL.with(|c| c.set(true));
        let result = catch_unwind(AssertUnwindSafe(f));
        let _ = tx.send(result);
    });
    off_pool_tx().send(job).expect("rbtc-off-pool");
    match rx.await.expect("rbtc-off-pool dropped") {
        Ok(value) => value,
        Err(payload) => resume_unwind(payload),
    }
}

/// Run `fut` with the API store-read budget armed. Prepare takes one permit
/// and drops it before script verify. The commit coin re-read takes one
/// again. A task that never calls this does not take the budget (P2P decode,
/// and admits that are not an API read).
pub async fn with_store_budget<R>(budget: Arc<Semaphore>, fut: impl Future<Output = R>) -> R {
    API_STORE_BUDGET.scope(budget, fut).await
}

/// Same task as [`with_store_budget`]. [`acquire_armed_store_budget`] counts a
/// caller that found the budget empty.
pub async fn with_store_budget_waiters<R>(
    waiters: Arc<AtomicUsize>,
    fut: impl Future<Output = R>,
) -> R {
    STORE_BUDGET_WAITERS.scope(waiters, fut).await
}

struct StoreBudgetWait(Arc<AtomicUsize>);

impl StoreBudgetWait {
    fn enter(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for StoreBudgetWait {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether `e` carries a [`tokio::task::JoinError`]: a task this side spawned
/// panicked or was cancelled. Socket errors a client can cause are not.
pub fn is_join_error(e: &std::io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<tokio::task::JoinError>())
}

/// The child's `Ok` value. A panicked child resumes that payload. Cancel
/// still aborts the caller.
pub fn finish_join<T>(joined: Result<T, tokio::task::JoinError>, cancel: &'static str) -> T {
    match joined {
        Ok(value) => value,
        Err(err) => match err.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(err) => panic!("{cancel}: {err}"),
        },
    }
}

/// `tokio::spawn` does not keep the task-local budget. Copy it into the
/// child so a detached admit still waits for a store permit.
pub fn spawn_with_store_budget<T: Send + 'static>(
    fut: impl Future<Output = T> + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let budget = API_STORE_BUDGET.try_with(Arc::clone).ok();
    tokio::spawn(async move {
        if let Some(budget) = budget {
            with_store_budget(budget, fut).await
        } else {
            fut.await
        }
    })
}

/// One permit of the armed budget, or `None` when this task has no budget.
/// The wait is async. The caller moves the permit into the read so a dropped
/// request does not release it while the read still runs.
pub async fn acquire_armed_store_budget() -> Option<OwnedSemaphorePermit> {
    let Ok(sem) = API_STORE_BUDGET.try_with(Arc::clone) else {
        return None;
    };
    match Arc::clone(&sem).try_acquire_owned() {
        Ok(permit) => Some(permit),
        Err(_) => {
            let _waiting = STORE_BUDGET_WAITERS
                .try_with(Arc::clone)
                .ok()
                .map(StoreBudgetWait::enter);
            Some(sem.acquire_owned().await.expect("store budget"))
        }
    }
}

/// Park `permit` on this thread. [`release_store_permit`] drops it before
/// later CPU on the same call. The guard drops whatever is still held.
#[doc(hidden)]
pub struct StorePermitGuard;

#[doc(hidden)]
pub fn hold_store_permit(permit: Option<OwnedSemaphorePermit>) -> StorePermitGuard {
    HELD_STORE_PERMIT.with(|slot| {
        *slot.borrow_mut() = permit;
    });
    StorePermitGuard
}

/// Drop this thread's store-read permit. A thread that never held one is unchanged.
#[doc(hidden)]
pub fn release_store_permit() {
    HELD_STORE_PERMIT.with(|slot| {
        slot.borrow_mut().take();
    });
}

impl Drop for StorePermitGuard {
    fn drop(&mut self) {
        release_store_permit();
    }
}

/// [`off_pool`] that holds one armed store-read permit for `f` only.
pub async fn off_pool_budgeted<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let permit = acquire_armed_store_budget().await;
    off_pool(move || {
        let _permit = permit;
        f()
    })
    .await
}

pub fn assert_not_reactor(what: &'static str) {
    assert!(
        !on_tokio_worker() || in_blocking_region(),
        "{what} on tokio-rt-worker"
    );
}

pub use rbitcoin_primitives::assert_not_script_worker;

#[cfg(test)]
mod off_pool_reentry {
    use super::off_pool;
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::task::Context;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_cannot_call_off_pool() {
        off_pool(|| {
            let mut fut = Box::pin(off_pool(|| ()));
            let waker = std::task::Waker::noop();
            let mut cx = Context::from_waker(waker);
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                let _ = fut.as_mut().poll(&mut cx);
            }));
            assert!(panicked.is_err(), "nested off_pool must panic");
        })
        .await;
    }
}

#[cfg(test)]
mod store_permit_drop {
    use super::{acquire_armed_store_budget, hold_store_permit, with_store_budget};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn dropping_the_guard_returns_the_permit() {
        let sem = Arc::new(Semaphore::new(1));
        with_store_budget(sem, async {
            let first = acquire_armed_store_budget().await;
            let guard = hold_store_permit(first);
            drop(guard);
            let again =
                tokio::time::timeout(Duration::from_millis(200), acquire_armed_store_budget())
                    .await;
            assert!(
                again.is_ok(),
                "the dropped guard must release the store permit"
            );
        })
        .await;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod off_pool_lane {
    use super::off_pool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    fn off_pool_threads() -> usize {
        let mut n = 0usize;
        for entry in std::fs::read_dir("/proc/self/task").expect("task dir") {
            let path = entry.expect("task").path().join("comm");
            let Ok(comm) = std::fs::read_to_string(&path) else {
                continue;
            };
            if comm.trim() == "rbtc-off-pool" {
                n += 1;
            }
        }
        n
    }

    /// Eight callers blocked inside `off_pool` still share one OS thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_thread_runs_every_off_pool_job() {
        off_pool(|| ()).await;
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let extra = std::thread::Builder::new()
            .name("rbtc-off-pool".into())
            .spawn(move || {
                let _ = stop_rx.recv();
            })
            .expect("extra off-pool thread");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut named = off_pool_threads();
        while named < 2 {
            assert!(
                Instant::now() < deadline,
                "named rbtc-off-pool threads stayed at {named}"
            );
            tokio::task::yield_now().await;
            named = off_pool_threads();
        }
        drop(stop_tx);
        extra.join().expect("extra off-pool thread");
        let started = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let started = Arc::clone(&started);
            let arrived = Arc::clone(&arrived);
            let gate = Arc::clone(&gate);
            tasks.push(tokio::spawn(async move {
                arrived.fetch_add(1, Ordering::Release);
                off_pool(move || {
                    started.fetch_add(1, Ordering::Release);
                    let (lock, cv) = &*gate;
                    let mut go = lock.lock().expect("gate");
                    while !*go {
                        go = cv.wait(go).expect("gate");
                    }
                })
                .await;
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while arrived.load(Ordering::Acquire) < 8 {
            assert!(Instant::now() < deadline, "off-pool callers did not start");
            tokio::task::yield_now().await;
        }
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        let threads = off_pool_threads();
        let running = started.load(Ordering::Acquire);
        {
            let (lock, cv) = &*gate;
            *lock.lock().expect("gate") = true;
            cv.notify_all();
        }
        for task in tasks {
            task.await.expect("off_pool task");
        }
        assert_eq!(
            threads, 1,
            "rbtc-off-pool threads: {threads}, jobs inside: {running}"
        );
        assert!(
            running < 8,
            "each job took its own thread ({running} inside at once)"
        );
    }
}
