use rbitcoin_consensus::error::ConsensusError;
use rbitcoin_consensus::script_pool::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use std::sync::MutexGuard;
use std::time::{Duration, Instant};

static OCCUPY: Mutex<()> = Mutex::new(());

struct OccupyGate {
    gate: Arc<(Mutex<bool>, Condvar)>,
    _occupy: MutexGuard<'static, ()>,
}

impl OccupyGate {
    fn occupy_all() -> Self {
        Self::occupy_n(worker_spawn_count())
    }

    fn occupy_n(k: usize) -> Self {
        let occupy = OCCUPY.lock().unwrap_or_else(|p| p.into_inner());
        let n = worker_spawn_count();
        assert!(n >= 1);
        let k = k.min(n);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let me = Self {
            gate: Arc::clone(&gate),
            _occupy: occupy,
        };
        let entered = Arc::new(AtomicUsize::new(0));
        for _ in 0..k {
            let entered = Arc::clone(&entered);
            let gate = Arc::clone(&gate);
            spawn_detached(move || {
                let (lock, cv) = &*gate;
                let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
                entered.fetch_add(1, Ordering::SeqCst);
                cv.notify_all();
                while !*g {
                    g = cv.wait(g).unwrap_or_else(|p| p.into_inner());
                }
            });
        }
        let (lock, cv) = &*gate;
        let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
        let start = Instant::now();
        while entered.load(Ordering::SeqCst) < k {
            let left = Duration::from_secs(2).saturating_sub(start.elapsed());
            assert!(!left.is_zero(), "failed to occupy steal workers");
            let (g2, _) = cv.wait_timeout(g, left).unwrap_or_else(|p| p.into_inner());
            g = g2;
        }
        drop(g);
        me
    }

    fn release(&self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }
}

impl Drop for OccupyGate {
    fn drop(&mut self) {
        self.release();
    }
}

/// Publish `items` as a foreground wave and wait for it.
fn run_owned(
    items: Vec<u32>,
    f: fn(&u32) -> Result<(), ConsensusError>,
) -> Result<(), ConsensusError> {
    match start_for_each_owned(items, f)? {
        Some(w) => w.finish(),
        None => Ok(()),
    }
}

static ALL_HITS: AtomicUsize = AtomicUsize::new(0);
fn count_all(_: &u32) -> Result<(), ConsensusError> {
    ALL_HITS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[test]
fn parallel_all_ok_and_counts() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    ALL_HITS.store(0, Ordering::Relaxed);
    run_owned((0..64).collect(), count_all).unwrap();
    assert_eq!(ALL_HITS.load(Ordering::Relaxed), 64);
}

fn boom_at_seven(i: &u32) -> Result<(), ConsensusError> {
    if *i == 7 {
        Err(ConsensusError::BadBlock("boom"))
    } else {
        Ok(())
    }
}

#[test]
fn owned_wave_first_error_surfaces() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let err = match start_for_each_owned((0..32u32).collect(), boom_at_seven) {
        Ok(Some(w)) => w.finish().expect_err("owned wave must fail"),
        Ok(None) => panic!("expected a published owned wave"),
        Err(e) => e,
    };
    assert!(format!("{err}").contains("boom"));
}

#[test]
fn empty_and_single_run_inline() {
    assert!(start_for_each_owned(Vec::new(), ok_u32).unwrap().is_none());
    assert!(start_for_each_owned(vec![1u32], ok_u32).unwrap().is_none());
}

#[test]
fn spawn_detached_runs_work() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    use std::sync::mpsc;
    let (tx, rx) = mpsc::sync_channel(1);
    spawn_detached(move || {
        let _ = tx.send(42u32);
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        42
    );
}

#[test]
fn join_many_does_not_spawn_per_job() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let before = worker_spawn_count();
    assert!(before >= 1);
    for i in 0..32u32 {
        let v = run_detached_join(move || i).expect("join");
        assert_eq!(v, i);
    }
    assert_eq!(
        worker_spawn_count(),
        before,
        "pool must not spawn a thread per mempool-style join"
    );
}

/// All `rbtc-scripts-*` workers must be able to sit in [`thread::park`] at
/// once (no jobs-mutex held across the idle wait).
#[test]
fn pool_waiters_run_concurrently() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    assert!(n >= 1);
    wait_until("workers idle before occupy", || idle_waiter_count() >= n);
    let occupy = OccupyGate::occupy_all();
    occupy.release();
    wait_until("workers idle after release", || idle_waiter_count() >= n);
}

struct DelayParkArm;

impl DelayParkArm {
    fn arm() -> Self {
        DELAY_PARK_ENTERED.store(0, Ordering::SeqCst);
        DELAY_PARK_GO.store(false, Ordering::SeqCst);
        DELAY_PARK.store(true, Ordering::SeqCst);
        Self
    }

    fn go(&self) {
        let _g = DELAY_PARK_MU.lock().unwrap_or_else(|p| p.into_inner());
        DELAY_PARK_GO.store(true, Ordering::SeqCst);
        DELAY_PARK_CV.notify_all();
    }
}

impl Drop for DelayParkArm {
    fn drop(&mut self) {
        DELAY_PARK.store(false, Ordering::SeqCst);
        let _g = DELAY_PARK_MU.lock().unwrap_or_else(|p| p.into_inner());
        DELAY_PARK_GO.store(true, Ordering::SeqCst);
        DELAY_PARK_CV.notify_all();
    }
}

/// Publish a wave after the last free worker has missed steal and before
/// it parks. Condvar notify-without-mutex lost that wake; park permits
/// must still run the wave.
#[test]
fn wave_published_before_park_is_not_missed() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    let occupy = OccupyGate::occupy_n(n.saturating_sub(1));
    wait_until("free worker did not park", || idle_waiter_count() != 0);
    let arm = DelayParkArm::arm();
    // One empty job pulls the parked worker into the park delay. The wave
    // is published only after that, so the worker has already missed steal.
    spawn_detached(|| {});
    wait_until("free worker did not reach park gate", || {
        DELAY_PARK_ENTERED.load(Ordering::SeqCst) != 0
    });
    ALL_HITS.store(0, Ordering::Relaxed);
    let wave = thread::spawn(|| run_owned((0..64).collect(), count_all));
    wait_until("wave was not published", || !waves_snap().load().is_empty());
    arm.go();
    wave.join().expect("wave thread").expect("wave ok");
    assert_eq!(ALL_HITS.load(Ordering::Relaxed), 64);
    occupy.release();
}

static HOLD_ENTERED: AtomicUsize = AtomicUsize::new(0);
static HOLD_GO: AtomicBool = AtomicBool::new(false);
static HOLD_MU: Mutex<()> = Mutex::new(());
static HOLD_CV: Condvar = Condvar::new();

fn reset_hold() {
    HOLD_ENTERED.store(0, Ordering::SeqCst);
    HOLD_GO.store(false, Ordering::SeqCst);
}

fn hold_chunk(_: &u32) -> Result<(), ConsensusError> {
    HOLD_ENTERED.fetch_add(1, Ordering::SeqCst);
    note_script_progress();
    let mut g = HOLD_MU.lock().unwrap_or_else(|p| p.into_inner());
    while !HOLD_GO.load(Ordering::SeqCst) {
        g = HOLD_CV.wait(g).unwrap_or_else(|p| p.into_inner());
    }
    Ok(())
}

fn release_hold() {
    let _g = HOLD_MU.lock().unwrap_or_else(|p| p.into_inner());
    HOLD_GO.store(true, Ordering::SeqCst);
    HOLD_CV.notify_all();
}

/// Releases the chunk hold before join so a failed assert cannot leave the
/// workers parked inside `hold_chunk` while `OwnedWave` waits for them.
struct HeldWave {
    wave: Option<OwnedWave<u32>>,
}

impl HeldWave {
    fn publish(items: Vec<u32>) -> Self {
        Self {
            wave: Some(
                start_for_each_pooled(items, hold_chunk, 1)
                    .unwrap()
                    .expect("chunks are published"),
            ),
        }
    }

    fn wave(&self) -> &OwnedWave<u32> {
        self.wave.as_ref().expect("wave")
    }

    fn finish(mut self) -> Result<(), ConsensusError> {
        let wave = self.wave.take().expect("wave");
        release_hold();
        wave.finish()
    }
}

impl Drop for HeldWave {
    fn drop(&mut self) {
        release_hold();
        drop(self.wave.take());
    }
}

fn wait_until(label: &str, pred: impl Fn() -> bool) {
    let start = Instant::now();
    let mut g = SCRIPT_PROGRESS_MU.lock().unwrap_or_else(|p| p.into_inner());
    while !pred() {
        let left = Duration::from_secs(2).saturating_sub(start.elapsed());
        assert!(
            !left.is_zero(),
            "{label} idle={} entered={} unparks={}",
            idle_waiter_count(),
            HOLD_ENTERED.load(Ordering::SeqCst),
            STEAL_UNPARKS.load(Ordering::Relaxed)
        );
        let (g2, _) = SCRIPT_PROGRESS_CV
            .wait_timeout(g, left)
            .unwrap_or_else(|p| p.into_inner());
        g = g2;
    }
}

fn wait_all_idle(n: usize) {
    wait_until("workers idle", || idle_waiter_count() >= n);
}

/// Publish while every worker is parked, and require that publish to unpark
/// `k` of them. A worker that left `park` in the gap claims one or more
/// chunks before this unpark, so the count is short of `k` (zero when it
/// claimed them all). Finish that attempt and try again. A publish that
/// wakes nobody stays at `entered == 0` and the wait fails.
fn publish_waking(n: usize, k: usize, items: Vec<u32>) -> HeldWave {
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "clean wake of {k} not observed: idle={} entered={} unparks={}",
            idle_waiter_count(),
            HOLD_ENTERED.load(Ordering::SeqCst),
            STEAL_UNPARKS.load(Ordering::Relaxed)
        );
        wait_all_idle(n);
        reset_hold();
        STEAL_UNPARKS.store(0, Ordering::Relaxed);
        if idle_waiter_count() < n {
            continue;
        }
        let wave = HeldWave::publish(items.clone());
        let spin = Instant::now();
        let mut g = SCRIPT_PROGRESS_MU.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let entered = HOLD_ENTERED.load(Ordering::SeqCst);
            let unparks = STEAL_UNPARKS.load(Ordering::Relaxed);
            if entered >= k {
                drop(g);
                if unparks == k {
                    return wave;
                }
                wave.finish().expect("retry wave");
                break;
            }
            let left = Duration::from_secs(2).saturating_sub(spin.elapsed());
            assert!(
                !left.is_zero(),
                "claim of {k} stalled: entered={entered} unparks={unparks}"
            );
            let (g2, _) = SCRIPT_PROGRESS_CV
                .wait_timeout(g, left)
                .unwrap_or_else(|p| p.into_inner());
            g = g2;
        }
    }
}

/// One published chunk wakes one idle worker. K chunks wake K workers, capped
/// at the pool. A larger wave still drains, and a parked worker is not left
/// beside a queued chunk.
#[test]
fn steal_publish_wakes_idle_workers() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    assert!(n >= 1);
    wait_all_idle(n);

    let one = publish_waking(n, 1, vec![1u32]);
    // `publish_waking` returns when the unpark count matches. A worker that
    // was already leaving `park` is still awake, so the idle count is short
    // until that worker parks again. Passing here would flake the suite.
    wait_until("the other workers stay parked", || {
        idle_waiter_count() == n - 1
    });
    one.finish().unwrap();
    wait_all_idle(n);

    let k = n.min(2);
    let many = publish_waking(n, k, (0..k as u32).collect());
    wait_until("k workers stay up", || idle_waiter_count() == n - k);
    many.finish().unwrap();
    wait_all_idle(n);

    let chunks = n + 2;
    let wide = publish_waking(n, n, (0..chunks as u32).collect());
    wait_until("no parked worker beside a queued chunk", || {
        idle_waiter_count() == 0
    });
    assert!(
        wide.wave().has_unclaimed(),
        "chunks remain after the pool is busy"
    );
    wide.finish().unwrap();
    assert_eq!(HOLD_ENTERED.load(Ordering::SeqCst), chunks);
    wait_all_idle(n);
}

static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn record_name(_: &u32) -> Result<(), ConsensusError> {
    NAMES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(thread::current().name().unwrap_or("").to_string());
    Ok(())
}

#[test]
fn owned_wave_runs_on_script_workers() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let before = worker_spawn_count();
    NAMES.lock().unwrap_or_else(|p| p.into_inner()).clear();
    run_owned((0..32).collect(), record_name).unwrap();
    assert_eq!(worker_spawn_count(), before);
    let names = NAMES.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(names.len(), 32);
    for n in names.iter() {
        assert!(n.starts_with("rbtc-scripts-"), "item ran on {n:?}");
    }
}

static A_HITS: AtomicUsize = AtomicUsize::new(0);
static B_HITS: AtomicUsize = AtomicUsize::new(0);
fn count_a(_: &u32) -> Result<(), ConsensusError> {
    A_HITS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
fn count_b(_: &u32) -> Result<(), ConsensusError> {
    B_HITS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[test]
fn overlapping_owned_waves_both_complete() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    A_HITS.store(0, Ordering::Relaxed);
    B_HITS.store(0, Ordering::Relaxed);
    let a = thread::spawn(|| run_owned((0..16).collect(), count_a));
    let b = thread::spawn(|| run_owned((0..16).collect(), count_b));
    a.join().expect("a").expect("a ok");
    b.join().expect("b").expect("b ok");
    assert_eq!(A_HITS.load(Ordering::Relaxed), 16);
    assert_eq!(B_HITS.load(Ordering::Relaxed), 16);
}

static PER_ITEM: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];
fn count_item(&i: &u32) -> Result<(), ConsensusError> {
    PER_ITEM[i as usize].fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[test]
fn steal_chunk_amortizes_claims() {
    // 256 items → 8 chunks of 32. Not 256 fetch_adds on `next`.
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    workers();
    STEAL_CLAIMS.store(0, Ordering::Relaxed);
    STEAL_CLAIMS_ON.store(true, Ordering::Relaxed);
    for h in &PER_ITEM {
        h.store(0, Ordering::Relaxed);
    }
    run_owned((0..256).collect(), count_item).unwrap();
    STEAL_CLAIMS_ON.store(false, Ordering::Relaxed);
    for (i, h) in PER_ITEM.iter().enumerate() {
        assert_eq!(h.load(Ordering::Relaxed), 1, "index {i} not run once");
    }
    let claims = STEAL_CLAIMS.load(Ordering::Relaxed);
    assert!(
        (8..32).contains(&claims),
        "expected ~8 chunks of 32 for 256 jobs, got {claims}"
    );
}

fn run_owned_chunk(items: Vec<u32>, chunk: usize) -> Result<(), ConsensusError> {
    match start_for_each_owned_chunk(items, ok_u32, chunk)? {
        Some(w) => w.finish(),
        None => Ok(()),
    }
}

/// Index windows are at most 64 heights and often under 32, so a claim of
/// 32 assigns the whole window to one worker. Eight jobs at size 1 are
/// eight claims; the script-verify default still covers those eight in one.
#[test]
fn index_wave_claims_one_job() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    workers();
    STEAL_CLAIMS.store(0, Ordering::Relaxed);
    STEAL_CLAIMS_ON.store(true, Ordering::Relaxed);
    run_owned_chunk((0..8).collect(), 1).unwrap();
    let one = STEAL_CLAIMS.load(Ordering::Relaxed);
    STEAL_CLAIMS.store(0, Ordering::Relaxed);
    run_owned((0..8).collect(), ok_u32).unwrap();
    let wide = STEAL_CLAIMS.load(Ordering::Relaxed);
    STEAL_CLAIMS_ON.store(false, Ordering::Relaxed);
    assert_eq!(one, 8, "claim size 1 is one claim per height");
    assert_eq!(
        wide, 1,
        "script-verify chunk of 32 covers 8 jobs in one claim"
    );
}

fn boom_immediate(_: &u32) -> Result<(), ConsensusError> {
    Err(ConsensusError::BadBlock("boom"))
}

/// Drop the occupy gate before the wave. `OwnedWave`'s drop waits until
/// the wave is complete, and that wait cannot run while every worker is
/// parked in the gate.
struct ReleaseOccupy(Option<OccupyGate>);
impl Drop for ReleaseOccupy {
    fn drop(&mut self) {
        self.0.take();
    }
}

/// A caller that is not a script worker can still claim a chunk. A failed
/// wave is not still claimable, and `next == n` is finished.
#[test]
fn steal_chunk_runs_while_workers_are_busy() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    ALL_HITS.store(0, Ordering::Relaxed);
    let occupy = OccupyGate::occupy_all();
    let wave = start_for_each_owned_chunk((0..4u32).collect(), count_all, 1)
        .unwrap()
        .expect("wave");
    let _release = ReleaseOccupy(Some(occupy));
    assert!(wave.has_unclaimed(), "next < n while workers are busy");
    for _ in 0..4 {
        let (range, hold) = steal_chunk().expect("one index claim");
        run_held(range, hold);
    }
    assert_eq!(ALL_HITS.load(Ordering::Relaxed), 4);
    assert!(!wave.has_unclaimed(), "next == n is not still unclaimed");
    assert!(steal_chunk().is_none(), "an empty wave is not a steal");
    drop(_release);
    wave.finish().unwrap();

    let occupy = OccupyGate::occupy_all();
    let failed = start_for_each_owned_chunk((0..8u32).collect(), boom_immediate, 1)
        .unwrap()
        .expect("failed wave");
    let _release = ReleaseOccupy(Some(occupy));
    assert!(failed.has_unclaimed());
    let (range, hold) = steal_chunk().expect("the failing job");
    run_held(range, hold);
    assert!(
        !failed.has_unclaimed(),
        "a failed wave is not stealable while jobs remain"
    );
    drop(_release);
    let err = failed.finish().expect_err("boom");
    assert!(format!("{err}").contains("boom"));
}

#[test]
fn owned_wave_from_script_worker_is_refused() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let got =
        run_detached_join(|| start_for_each_owned(vec![1u32, 2], ok_u32).map(|w| w.is_some()))
            .expect("join");
    let err = got.expect_err("must refuse nested wait");
    assert!(
        format!("{err}").contains("try_for_each from a script worker"),
        "{err}"
    );
}

#[test]
fn panic_while_workers_occupied_does_not_deadlock_pool() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _gate = OccupyGate::occupy_all();
        panic!("occupy test boom");
    }));
    std::panic::set_hook(prev);
    let err = panicked.expect_err("occupy must panic");
    let text = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(text.contains("occupy test boom"), "panic payload {text:?}");
    run_owned(vec![1, 2, 3, 4], ok_u32).unwrap();
}

static HOLD_JOBS: AtomicBool = AtomicBool::new(false);
static HOLD_IN: AtomicUsize = AtomicUsize::new(0);

static HOLD_JOB_THREADS: Mutex<Vec<thread::Thread>> = Mutex::new(Vec::new());

fn hold_job(_: &u32) -> Result<(), ConsensusError> {
    HOLD_IN.fetch_add(1, Ordering::SeqCst);
    HOLD_JOB_THREADS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(thread::current());
    note_script_progress();
    while HOLD_JOBS.load(Ordering::SeqCst) {
        thread::park();
    }
    Ok(())
}

struct HoldJobs;
impl HoldJobs {
    fn arm() -> Self {
        HOLD_IN.store(0, Ordering::SeqCst);
        HOLD_JOBS.store(true, Ordering::SeqCst);
        Self
    }
}
impl Drop for HoldJobs {
    fn drop(&mut self) {
        HOLD_JOBS.store(false, Ordering::SeqCst);
        let threads =
            std::mem::take(&mut *HOLD_JOB_THREADS.lock().unwrap_or_else(|p| p.into_inner()));
        for t in threads {
            t.unpark();
        }
    }
}

#[test]
fn second_wave_publishes_when_first_is_claimed() {
    let _steal = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let _occupy = OCCUPY.lock().unwrap_or_else(|p| p.into_inner());
    let hold = HoldJobs::arm();
    let a = start_for_each_owned((0..64u32).collect(), hold_job)
        .unwrap()
        .expect("wave a");
    wait_until("first wave not fully claimed", || !a.has_unclaimed());
    assert!(!fg_has_unclaimed());
    let b = start_for_each_owned((0..64u32).collect(), hold_job)
        .unwrap()
        .expect("wave b");
    assert!(
        !a.is_complete(),
        "first wave still in_wave when second is published"
    );
    assert!(b.has_unclaimed() || fg_has_unclaimed() || !a.is_complete());
    drop(hold);
    a.finish().unwrap();
    b.finish().unwrap();
    assert!(!fg_has_unclaimed());
}

fn ok_u32(_: &u32) -> Result<(), ConsensusError> {
    Ok(())
}

struct DelayClaimArm;
impl DelayClaimArm {
    fn arm() -> Self {
        DELAY_CLAIM_ENTERED.store(0, Ordering::SeqCst);
        DELAY_CLAIM_GO.store(false, Ordering::SeqCst);
        DELAY_CLAIM.store(true, Ordering::SeqCst);
        Self
    }
    fn go(&self) {
        let _g = DELAY_CLAIM_MU.lock().unwrap_or_else(|p| p.into_inner());
        DELAY_CLAIM_GO.store(true, Ordering::SeqCst);
        DELAY_CLAIM_CV.notify_all();
    }
}
impl Drop for DelayClaimArm {
    fn drop(&mut self) {
        DELAY_CLAIM.store(false, Ordering::SeqCst);
        let _g = DELAY_CLAIM_MU.lock().unwrap_or_else(|p| p.into_inner());
        DELAY_CLAIM_GO.store(true, Ordering::SeqCst);
        DELAY_CLAIM_CV.notify_all();
    }
}

/// A panicking chunk must not leave `in_wave` elevated. `wait_done` returns
/// inside the test's own bound.
#[test]
fn chunk_panic_releases_the_wave() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    fn boom(_: &u32) -> Result<(), ConsensusError> {
        panic!("chunk panic");
    }
    let wave = start_for_each_owned((0..64u32).collect(), boom)
        .unwrap()
        .expect("wave");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let failed = wave.finish().is_err();
        let _ = tx.send(failed);
    });
    let failed = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("wait_done after a chunk panic");
    assert!(failed, "chunk panic is an error");
}

/// After `in_wave++` and before `next += chunk`, `is_complete` must be
/// false (publisher must not free ctx under the claimer).
#[test]
fn last_claim_holds_in_wave_before_next() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    let occupy = OccupyGate::occupy_n(n.saturating_sub(1));
    let arm = DelayClaimArm::arm();
    let wave = thread::spawn(|| {
        start_for_each_owned((0..2u32).collect(), ok_u32)
            .unwrap()
            .expect("wave")
            .finish()
    });
    wait_until("claimer did not enter in_wave delay", || {
        DELAY_CLAIM_ENTERED.load(Ordering::SeqCst) != 0
    });
    let snap = waves_snap().load();
    assert!(!snap.is_empty(), "wave not published");
    assert!(
        !snap[0].is_complete(),
        "is_complete during in_wave-before-next window"
    );
    arm.go();
    wave.join().expect("join").expect("wave ok");
    occupy.release();
}

static STAGE_PARKED: AtomicBool = AtomicBool::new(false);

/// Two script stages park in one process. The second registration must
/// not steal wakes from the first, and clearing the second must leave
/// the first registered.
#[test]
fn second_stage_keeps_the_first_publisher_wake() {
    let phase = Arc::new(AtomicUsize::new(0));
    let a_id = Arc::new(Mutex::new(None));
    let b_id = Arc::new(Mutex::new(None));
    let a_registered = Arc::new(AtomicBool::new(false));
    let b_registered = Arc::new(AtomicBool::new(false));
    let release_b = Arc::new(AtomicBool::new(false));
    let b_cleared = Arc::new(AtomicBool::new(false));
    let wakes = Arc::new(AtomicUsize::new(0));

    let phase_a = Arc::clone(&phase);
    let a_id_slot = Arc::clone(&a_id);
    let a_flag = Arc::clone(&a_registered);
    let b_flag_for_a = Arc::clone(&b_registered);
    let wakes_a = Arc::clone(&wakes);
    let a = thread::spawn(move || {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                set_script_publisher(None);
            }
        }
        set_script_publisher(Some(thread::current()));
        *a_id_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread::current().id());
        let _clear = Clear;
        a_flag.store(true, Ordering::Release);
        note_script_progress();
        wait_until("second stage did not register", || {
            b_flag_for_a.load(Ordering::Acquire)
        });
        let take_wake = || {
            phase_a.store(0, Ordering::Release);
            STAGE_PARKED.store(false, Ordering::Release);
            wakes_a.fetch_add(1, Ordering::Release);
            note_script_progress();
        };
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                // phase 2 is this round's unpark. Storing 1 over it, or
                // park_timeout(0), drops a wake the driver does not repeat.
                if phase_a.load(Ordering::Acquire) == 2 {
                    take_wake();
                    break;
                }
                let raced = phase_a
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err();
                if raced && phase_a.load(Ordering::Acquire) == 2 {
                    continue;
                }
                STAGE_PARKED.store(true, Ordering::Release);
                note_script_progress();
                if phase_a.load(Ordering::Acquire) == 2 {
                    continue;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "script stage was not woken");
                thread::park_timeout(left);
                STAGE_PARKED.store(false, Ordering::Release);
            }
        }
    });

    let b_flag = Arc::clone(&b_registered);
    let b_id_slot = Arc::clone(&b_id);
    let start_b = Arc::clone(&a_registered);
    let release = Arc::clone(&release_b);
    let cleared = Arc::clone(&b_cleared);
    let b = thread::spawn(move || {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                set_script_publisher(None);
            }
        }
        wait_until("first stage did not register", || {
            start_b.load(Ordering::Acquire)
        });
        set_script_publisher(Some(thread::current()));
        *b_id_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread::current().id());
        let _clear = Clear;
        b_flag.store(true, Ordering::Release);
        note_script_progress();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !release.load(Ordering::Acquire) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "second stage was not released");
            thread::park_timeout(left);
        }
        set_script_publisher(None);
        cleared.store(true, Ordering::Release);
        note_script_progress();
    });

    let drive = |want: usize| {
        wait_until("stage did not park", || {
            phase.load(Ordering::Acquire) == 1 && STAGE_PARKED.load(Ordering::Acquire)
        });
        phase.store(2, Ordering::Release);
        unpark_script_publisher();
        wait_until("wake did not land", || {
            wakes.load(Ordering::Acquire) >= want
        });
    };

    drive(1);
    release_b.store(true, Ordering::Release);
    unpark_script_publisher();
    wait_until("second stage did not clear", || {
        b_cleared.load(Ordering::Acquire)
    });
    let publishers: Vec<_> = PUBLISHERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|t| t.id())
        .collect();
    assert!(
        publishers.contains(&a_id.lock().unwrap().expect("stage a id")),
        "clearing the second stage dropped the first"
    );
    assert!(
        !publishers.contains(&b_id.lock().unwrap().expect("stage b id")),
        "clearing the second stage kept it"
    );
    drive(2);
    a.join().expect("stage a");
    b.join().expect("stage b");
}

#[test]
fn fg_has_unclaimed_while_a_chunk_remains() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    let occupy = OccupyGate::occupy_n(n.saturating_sub(1));
    let arm = DelayClaimArm::arm();
    let wave = thread::spawn(|| {
        start_for_each_owned_chunk((0..64u32).collect(), ok_u32, 1)
            .unwrap()
            .expect("wave")
            .finish()
    });
    wait_until("claimer did not enter the claim delay", || {
        DELAY_CLAIM_ENTERED.load(Ordering::SeqCst) != 0
    });
    assert!(fg_has_unclaimed());
    arm.go();
    wave.join().expect("join").expect("wave ok");
    occupy.release();
}

#[test]
fn warn_fault_without_keep_fails_the_wave() {
    fn warn(_: &u32) -> Result<(), ConsensusError> {
        Err(ConsensusError::BadTx("not final"))
    }
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let err = start_for_each_owned((0..2u32).collect(), warn)
        .unwrap()
        .expect("wave")
        .finish();
    assert!(
        err.is_err(),
        "a warn fault is a failure when keep_faults is off"
    );
}

#[test]
fn park_delay_waits_until_go() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let arm = DelayParkArm::arm();
    let entered = thread::spawn(maybe_delay_park);
    wait_until("park delay was not entered", || {
        DELAY_PARK_ENTERED.load(Ordering::SeqCst) != 0
    });
    thread::sleep(Duration::from_millis(50));
    assert!(!entered.is_finished(), "park delay returned before go");
    arm.go();
    entered.join().expect("park delay");
}

static CPU_ORDER: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn note_cpu_wave(_: &u32) -> Result<(), ConsensusError> {
    CPU_ORDER.lock().unwrap_or_else(|p| p.into_inner()).push(1);
    Ok(())
}

fn block_on_cpu<T>(job: CpuJob<T>) -> Option<T> {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut job = Pin::new(Box::new(job));
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match job.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => thread::park(),
        }
    }
}

/// `None` when `limit` passes with the job still pending. The waiter thread
/// is left behind on timeout so a dead script worker cannot hang the suite.
fn block_on_cpu_timeout<T: Send + 'static>(job: CpuJob<T>, limit: Duration) -> Option<Option<T>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(block_on_cpu(job));
    });
    rx.recv_timeout(limit).ok()
}

/// `run_cpu` finishes on `rbtc-scripts-*`. The caller parks; it does not
/// take a blocking-pool thread. A published steal chunk runs before the guest.
#[test]
fn run_cpu_uses_one_script_worker_after_the_wave() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    CPU_ORDER.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let n = worker_spawn_count();
    let occupy = OccupyGate::occupy_n(n.saturating_sub(1));
    let wave = start_for_each_pooled(vec![1u32], note_cpu_wave, 1)
        .unwrap()
        .expect("published wave");
    let name = block_on_cpu(run_cpu(|| {
        CPU_ORDER.lock().unwrap_or_else(|p| p.into_inner()).push(2);
        thread::current().name().unwrap_or("").to_string()
    }))
    .expect("script worker");
    wave.finish().unwrap();
    occupy.release();
    assert!(name.starts_with("rbtc-scripts-"), "guest ran on {name}");
    assert_eq!(
        CPU_ORDER
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_slice(),
        &[1, 2],
        "the steal chunk finishes before the guest"
    );
}

/// A panicking guest drops the reply and leaves that worker in the pool.
/// The other workers are held, so the next job can run only on the one
/// that just panicked.
#[test]
fn run_cpu_panic_keeps_the_worker() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let n = worker_spawn_count();
    let occupy = OccupyGate::occupy_n(n.saturating_sub(1));
    let lost = block_on_cpu_timeout(
        run_cpu(|| -> u8 { panic!("run_cpu guest") }),
        Duration::from_secs(2),
    );
    assert_eq!(lost, Some(None), "a panicking guest must drop the reply");
    let next = block_on_cpu_timeout(run_cpu(|| 7u8), Duration::from_secs(2));
    occupy.release();
    assert_eq!(
        next,
        Some(Some(7)),
        "the script worker must take the next job"
    );
}

/// A guest that enters store IO trips the script-worker guard.
#[test]
fn run_cpu_guest_cannot_touch_store_io() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let hit = block_on_cpu(run_cpu(|| {
        std::panic::catch_unwind(|| {
            rbitcoin_primitives::assert_not_script_worker("store read");
        })
        .is_err()
    }))
    .expect("script worker");
    assert!(hit, "store read on a script worker must panic");
}

/// Lost-wakeup close: work pending or a newer epoch returns before park.
/// A parked mutant is unparked so this process still exits.
fn expect_idle_miss(pool: Arc<ScriptWorkers>, epoch: usize) -> bool {
    let idle = Arc::new(rbitcoin_consensus::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let pool_t = Arc::clone(&pool);
    let idle_t = Arc::clone(&idle);
    let handle = thread::spawn(move || {
        let v = park_if_still_idle(&pool_t, epoch, &idle_t);
        let _ = tx.send(v);
    });
    match rx.recv_timeout(Duration::from_millis(400)) {
        Ok(v) => {
            handle.join().expect("idle turn");
            v
        }
        Err(_) => {
            handle.thread().unpark();
            let v = rx.recv_timeout(Duration::from_secs(2)).unwrap_or(true);
            handle.join().expect("idle turn");
            v
        }
    }
}

fn local_pool(epoch: usize) -> Arc<ScriptWorkers> {
    Arc::new(ScriptWorkers {
        jobs: rbitcoin_consensus::sync::Mutex::new(Default::default()),
        epoch: AtomicUsize::new(epoch),
    })
}

#[test]
fn park_returns_before_sleep_when_work_or_epoch_moved() {
    let _gate = STEAL_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pool = local_pool(7);
    {
        let mut jobs = rbitcoin_consensus::sync::lock(&pool.jobs);
        jobs.push_back(Box::new(|| {}));
    }
    // The recheck repeats this predicate. Queued work must return before that gate.
    // Pool workers share `DELAY_PARK`. Wait until they are parked so one of
    // them cannot enter the gate for this arm.
    wait_all_idle(worker_spawn_count());
    let arm = DelayParkArm::arm();
    let idle = Arc::new(rbitcoin_consensus::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let pool_t = Arc::clone(&pool);
    let idle_t = Arc::clone(&idle);
    let handle = thread::spawn(move || {
        let v = park_if_still_idle(&pool_t, 7, &idle_t);
        let _ = tx.send(v);
    });
    let reached_gate = match rx.recv_timeout(Duration::from_millis(400)) {
        Ok(false) => false,
        Ok(true) => true,
        Err(_) => {
            arm.go();
            let _ = rx.recv_timeout(Duration::from_secs(2));
            true
        }
    };
    handle.join().expect("queued work");
    assert!(
        !reached_gate,
        "queued work must return before the park gate"
    );
    drop(arm);
    let fresh = local_pool(7);
    assert!(
        !expect_idle_miss(Arc::clone(&fresh), 3),
        "a newer epoch must not park"
    );

    let second = local_pool(4);
    wait_all_idle(worker_spawn_count());
    let arm = DelayParkArm::arm();
    let idle = Arc::new(rbitcoin_consensus::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let pool_t = Arc::clone(&second);
    let idle_t = Arc::clone(&idle);
    let handle = thread::spawn(move || {
        let v = park_if_still_idle(&pool_t, 4, &idle_t);
        let _ = tx.send(v);
    });
    wait_until("park recheck", || {
        DELAY_PARK_ENTERED.load(Ordering::SeqCst) != 0
    });
    second.epoch.fetch_add(1, Ordering::Release);
    arm.go();
    let parked = match rx.recv_timeout(Duration::from_millis(400)) {
        Ok(v) => v,
        Err(_) => {
            handle.thread().unpark();
            rx.recv_timeout(Duration::from_secs(2)).unwrap_or(true)
        }
    };
    handle.join().expect("second check");
    assert!(!parked, "an epoch bump during the recheck must not park");

    let third = local_pool(4);
    wait_all_idle(worker_spawn_count());
    let arm = DelayParkArm::arm();
    let idle = Arc::new(rbitcoin_consensus::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let pool_t = Arc::clone(&third);
    let idle_t = Arc::clone(&idle);
    let handle = thread::spawn(move || {
        let v = park_if_still_idle(&pool_t, 4, &idle_t);
        let _ = tx.send(v);
    });
    wait_until("park recheck work", || {
        DELAY_PARK_ENTERED.load(Ordering::SeqCst) != 0
    });
    rbitcoin_consensus::sync::lock(&third.jobs).push_back(Box::new(|| {}));
    arm.go();
    let parked = match rx.recv_timeout(Duration::from_millis(400)) {
        Ok(v) => v,
        Err(_) => {
            handle.thread().unpark();
            rx.recv_timeout(Duration::from_secs(2)).unwrap_or(true)
        }
    };
    handle.join().expect("second check work");
    assert!(!parked, "work queued during the recheck must not park");
}
