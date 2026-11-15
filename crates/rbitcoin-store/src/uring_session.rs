//! Completion-driven IO session — **the** ring abstraction for this crate.
//!
//! All production bulk / machine IO goes through [`UringSession`] via
//! [`with_thread_local`]:
//! - plan head-resolve (confirm stamp / denserels)
//! - spend annotate abs-meta RMW / pure pwrite
//! - [`crate::bulk_io`] pread/pwrite batches and page RMW
//!
//! Backends: Linux `io_uring`, portable [`crate::io_session_pool`] (Darwin
//! default + `RBITCOIN_IO=pool`), Windows IOCP.
//!
//! **Lifetime:** one long-lived session per OS thread (TLS). **Nested**
//! [`with_thread_local`] on the same OS thread is a **hard error** (panic) —
//! never open a temporary mid-wave ring (that regressed plan `head_fk`).
//! Callers that need bulk probe IO must run it **before** taking the TLS ring
//! (e.g. plan probe outside the identity/idx machine). Do **not** call
//! [`UringSession::new`] on hot paths — that setup/teardowns every batch.

use crate::error::StoreError;
use crate::io_handle::IoHandle;
use crate::io_session_pool::PoolEngine;
use std::cell::Cell;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Default SQ/CQ depth for all store io_uring sessions (bulk, plan head-resolve,
/// spend annotate). TLS rings open at this size; [`with_thread_local`] may grow
/// if a caller requests more (none currently do).
pub const DEFAULT_ENTRIES: u32 = 128;

/// One `drain_all` wait window. Progress in a window resets the zero-CQE clock.
#[doc(hidden)]
pub const DRAIN_WINDOW: Duration = Duration::from_millis(100);
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub const DRAIN_WAIT_NS: u32 = 100_000_000;
#[doc(hidden)]
pub const DRAIN_SLOW_AFTER: Duration = Duration::from_secs(5);
#[doc(hidden)]
pub const DRAIN_HARD_DEFAULT_SECS: u64 = 120;

/// Drain wait policy: keep waiting while CQEs arrive; abort only after
/// [`DrainBudget::hard`] of zero completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainVerdict {
    Continue,
    LogSlow,
    HardCap,
}

#[derive(Debug)]
#[doc(hidden)]
pub enum WaitOneWindow {
    Ready,
    TimedOut,
    Idle,
    #[cfg(target_os = "linux")]
    Enter(StoreError),
}

pub struct DrainBudget {
    pub hard: Duration,
    pub zero: Duration,
    pub elapsed: Duration,
    pub since_slow_log: Duration,
}

impl DrainBudget {
    pub fn new(hard: Duration) -> Self {
        Self {
            hard,
            zero: Duration::ZERO,
            elapsed: Duration::ZERO,
            since_slow_log: Duration::ZERO,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(drain_hard_cap())
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub fn note(&mut self, dt: Duration, completed: usize) -> DrainVerdict {
        self.elapsed = self.elapsed.saturating_add(dt);
        self.since_slow_log = self.since_slow_log.saturating_add(dt);
        if completed > 0 {
            self.zero = Duration::ZERO;
        } else {
            self.zero = self.zero.saturating_add(dt);
            if self.zero >= self.hard {
                return DrainVerdict::HardCap;
            }
        }
        if self.elapsed >= DRAIN_SLOW_AFTER && self.since_slow_log >= DRAIN_SLOW_AFTER {
            self.since_slow_log = Duration::ZERO;
            return DrainVerdict::LogSlow;
        }
        DrainVerdict::Continue
    }
}

#[doc(hidden)]
pub fn drain_hard_cap() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(d) = TEST_DRAIN_HARD.with(|c| c.get()) {
        return d;
    }
    static CAP: OnceLock<Duration> = OnceLock::new();
    *CAP.get_or_init(|| {
        let secs = std::env::var("RBITCOIN_URING_DRAIN_HARD_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DRAIN_HARD_DEFAULT_SECS);
        Duration::from_secs(secs)
    })
}

#[cfg(debug_assertions)]
thread_local! {
    static TEST_DRAIN_HARD: Cell<Option<Duration>> = const { Cell::new(None) };
    static DROP_DRAIN_ERROR: Cell<Option<&'static str>> = const { Cell::new(None) };
}

#[cfg(debug_assertions)]
pub fn take_drop_drain_error() -> Option<&'static str> {
    DROP_DRAIN_ERROR.with(Cell::take)
}

#[cfg(debug_assertions)]
pub fn with_drain_hard_cap<R>(hard: Duration, f: impl FnOnce() -> R) -> R {
    TEST_DRAIN_HARD.with(|c| {
        let prev = c.replace(Some(hard));
        let out = f();
        c.set(prev);
        out
    })
}

/// FATAL then abort. Operator may restart with `RBITCOIN_IO=pread`.
pub fn abort_uring_unusable(reason: &str) -> ! {
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    rbitcoin_log::error!(
        "store: completion session unusable thread={thread} {reason} — aborting; restart with RBITCOIN_IO=pread if the filesystem or IO scheduling class cannot complete bulk IO"
    );
    std::process::abort();
}

/// Enter failed. Pending SQEs still own caller buffers, so this matches the
/// hard cap (abort, including a debug node) instead of returning into a free.
///
/// Linux is the only production caller. Other targets keep the function for
/// the unit test; a non-Linux lib build must not see it as dead code.
#[cfg(any(debug_assertions, target_os = "linux"))]
pub fn inflight_enter_failure(pending: usize, err: StoreError) -> Result<(), StoreError> {
    if pending == 0 {
        return Err(err);
    }
    finish_drain_hard_cap(pending)
}

#[doc(hidden)]
pub fn finish_drain_hard_cap(pending: usize) -> Result<(), StoreError> {
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    let reason = format!("drain hard cap pending={pending} thread={thread}");
    // `with_drain_hard_cap` is the only opt-in. A debug node does not set it,
    // so a leftover SQE still aborts instead of freeing caller buffers.
    #[cfg(debug_assertions)]
    if TEST_DRAIN_HARD.with(|c| c.get()).is_some() {
        let _ = reason;
        return Err(StoreError::Corrupt("invariant: io_uring undrained"));
    }
    abort_uring_unusable(&reason);
}

/// Which completion backend [`UringSession`] opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Linux io_uring. Asked for elsewhere, Windows builds IOCP (or the pool
    /// when IOCP will not open) and other targets build the pool.
    Uring,
    /// Worker-pool completion ring (`RBITCOIN_IO=pool` on Darwin or Linux).
    Pool,
    /// Windows IOCP.
    Iocp,
}

impl SessionKind {
    /// Engine name, as the startup log prints it.
    pub fn name(self) -> &'static str {
        match self {
            SessionKind::Uring => "io_uring",
            SessionKind::Pool => "pool",
            SessionKind::Iocp => "iocp",
        }
    }
}

/// Held completion session, or standalone (TLS / libc).
///
/// `held` is for machines that already own the thread-local ring. `none` is
/// the standalone path. Do **not** nest [`with_thread_local`] while a `held`
/// ctx is live.
pub struct IoCtx<'a> {
    pub session: Option<&'a mut UringSession>,
}

impl<'a> IoCtx<'a> {
    #[inline]
    pub fn held(session: &'a mut UringSession) -> Self {
        Self {
            session: Some(session),
        }
    }

    #[inline]
    pub fn none() -> IoCtx<'static> {
        IoCtx { session: None }
    }

    #[inline]
    pub fn from_opt(session: Option<&'a mut UringSession>) -> Self {
        Self { session }
    }

    #[inline]
    pub fn session(&mut self) -> Option<&mut UringSession> {
        self.session.as_deref_mut()
    }
}

thread_local! {
    static SESSION: std::cell::RefCell<Option<UringSession>> =
        const { std::cell::RefCell::new(None) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[allow(clippy::large_enum_variant)] // uring vs pool vs iocp backends
#[doc(hidden)]
pub enum SessionBackend {
    #[cfg(target_os = "linux")]
    Uring(io_uring::IoUring),
    Pool(PoolEngine),
    #[cfg(windows)]
    Iocp(crate::io_session_iocp::IocpEngine),
}

/// Owned completion session for multi-stage submit/complete loops.
pub struct UringSession {
    pub backend: SessionBackend,
    pub entries: u32,
    pub pending: UringPending,
    pub epoch: u16,
    /// Set on undrained leftover, unexpected CQE, CQ overflow, or wait timeout.
    /// Push and [`Self::begin_batch`] fail closed; [`with_thread_local`] drops the TLS ring.
    pub poisoned: bool,
}

impl UringSession {
    /// Open a private ring. Returns `Err` if io_uring is disabled or setup fails.
    ///
    /// Prefer [`with_thread_local`] on production paths (avoids setup/teardown).
    /// Unit tests / one-shot probes only.
    #[cfg(debug_assertions)]
    pub fn new(entries: u32) -> Result<Self, StoreError> {
        if !crate::bulk_io::io_uring_enabled() {
            return Err(StoreError::Unavailable);
        }
        Self::try_open(entries)
    }

    /// Create a session without consulting [`crate::bulk_io::io_uring_enabled`].
    ///
    /// Used for capability probe and by TLS open after the gate has already
    /// passed (avoids recursion through `io_uring_enabled` → probe → here).
    pub fn try_open(entries: u32) -> Result<Self, StoreError> {
        Self::try_open_kind(crate::bulk_io::resolved_session_kind(), entries)
    }

    /// Open an explicit backend (tests + probe).
    pub fn try_open_kind(kind: SessionKind, entries: u32) -> Result<Self, StoreError> {
        let entries = entries.clamp(32, 4096);
        let backend = match kind {
            SessionKind::Uring => {
                #[cfg(target_os = "linux")]
                {
                    let ring = io_uring::IoUring::new(entries).map_err(|e| {
                        StoreError::io(
                            Path::new("io_uring"),
                            std::io::Error::other(format!("io_uring: {e}")),
                        )
                    })?;
                    SessionBackend::Uring(ring)
                }
                #[cfg(windows)]
                {
                    match crate::io_session_iocp::IocpEngine::open(entries) {
                        Ok(eng) => SessionBackend::Iocp(eng),
                        Err(_) => SessionBackend::Pool(PoolEngine::open(
                            crate::bulk_io::bulk_io_workers(),
                        )),
                    }
                }
                #[cfg(not(any(target_os = "linux", windows)))]
                {
                    SessionBackend::Pool(PoolEngine::open(crate::bulk_io::bulk_io_workers()))
                }
            }
            SessionKind::Pool => {
                SessionBackend::Pool(PoolEngine::open(crate::bulk_io::bulk_io_workers()))
            }
            SessionKind::Iocp => {
                #[cfg(windows)]
                {
                    SessionBackend::Iocp(crate::io_session_iocp::IocpEngine::open(entries)?)
                }
                #[cfg(not(windows))]
                {
                    let _ = entries;
                    return Err(StoreError::Corrupt("iocp is Windows-only"));
                }
            }
        };
        Ok(Self {
            backend,
            entries,
            pending: UringPending::new(),
            epoch: 0,
            poisoned: false,
        })
    }

    /// Engine this session built, which can differ from the kind asked for.
    pub fn kind(&self) -> SessionKind {
        match self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(_) => SessionKind::Uring,
            SessionBackend::Pool(_) => SessionKind::Pool,
            #[cfg(windows)]
            SessionBackend::Iocp(_) => SessionKind::Iocp,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    pub fn entries(&self) -> u32 {
        self.entries
    }

    pub fn free_sq(&self) -> usize {
        (self.entries as usize).saturating_sub(self.pending.len())
    }

    /// Harvest epoch baked into [`pack_ud`]. Increment at the start of each
    /// machine wave so leftover CQEs from the previous wave cannot match.
    pub fn epoch(&self) -> u16 {
        self.epoch
    }

    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    #[doc(hidden)]
    pub fn rollback_pending(&mut self, user_data: u64) {
        if self.pending.expect_cqe(user_data).is_err() {
            self.poison();
        }
    }

    #[doc(hidden)]
    pub fn check_live(&self) -> Result<(), StoreError> {
        if self.poisoned {
            Err(StoreError::Corrupt("invariant: io_uring session poisoned"))
        } else {
            Ok(())
        }
    }

    /// Bump the harvest epoch. Leftover in-flight SQEs are fatal.
    ///
    /// Held TLS rings run probe → idx → BDZ g-pages → rel preads on one
    /// session. A previous machine that swallowed `drain_all` leaves CQEs in
    /// `pending`, but its buffers are already gone. There is no safe drain
    /// here, so fail closed instead of writing completions into freed memory.
    /// Machines drain (or use a guard declared after their buffers) before
    /// returning. Poisoned or leftover is fatal — callers must not push.
    pub fn begin_batch(&mut self) -> Result<(), StoreError> {
        if !self.pending.is_empty() {
            return self.fail_closed_boundary();
        }
        self.check_live()?;
        self.epoch = self.epoch.wrapping_add(1);
        Ok(())
    }

    /// Push a pread SQE.
    ///
    /// # Safety
    ///
    /// `buf` must stay allocated and not be written by anyone else until the
    /// CQE for `user_data` is harvested or [`Self::drain_all`] has returned.
    pub unsafe fn push_pread(
        &mut self,
        fd: impl Into<IoHandle>,
        offset: u64,
        buf: &mut [u8],
        user_data: u64,
    ) -> Result<(), StoreError> {
        if buf.is_empty() {
            return Ok(());
        }
        self.check_live()?;
        if self.pending.len() >= self.entries as usize {
            return Err(StoreError::BudgetFull("io_session SQ (in_flight cap)"));
        }
        self.pending.insert(user_data)?;
        let handle = fd.into();
        let r = match &mut self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(ring) => {
                use io_uring::{opcode, types};
                let sqe = opcode::Read::new(
                    types::Fd(handle.as_raw_fd()),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                )
                .offset(offset)
                .build()
                .user_data(user_data);
                // SAFETY: caller keeps `buf` alive until matching CQE is harvested.
                unsafe {
                    if ring.submission().push(&sqe).is_err() {
                        if self.pending.expect_cqe(user_data).is_err() {
                            self.poisoned = true;
                        }
                        return Err(StoreError::BudgetFull("io_uring SQ"));
                    }
                }
                Ok(())
            }
            SessionBackend::Pool(pool) => pool.push_pread(handle, offset, buf, user_data),
            #[cfg(windows)]
            SessionBackend::Iocp(eng) => eng.push_pread(handle, offset, buf, user_data),
        };
        if r.is_err() {
            self.rollback_pending(user_data);
        }
        r
    }

    /// Push a pwrite SQE.
    ///
    /// # Safety
    ///
    /// `buf` must stay allocated and unchanged until the CQE for `user_data`
    /// is harvested or [`Self::drain_all`] has returned.
    pub unsafe fn push_pwrite(
        &mut self,
        fd: impl Into<IoHandle>,
        offset: u64,
        buf: &[u8],
        user_data: u64,
    ) -> Result<(), StoreError> {
        if buf.is_empty() {
            return Ok(());
        }
        self.check_live()?;
        if self.pending.len() >= self.entries as usize {
            return Err(StoreError::BudgetFull("io_session SQ (in_flight cap)"));
        }
        self.pending.insert(user_data)?;
        let handle = fd.into();
        let r = match &mut self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(ring) => {
                use io_uring::{opcode, types};
                let sqe = opcode::Write::new(
                    types::Fd(handle.as_raw_fd()),
                    buf.as_ptr(),
                    buf.len() as u32,
                )
                .offset(offset)
                .build()
                .user_data(user_data);
                // SAFETY: caller keeps `buf` alive until matching CQE is harvested.
                unsafe {
                    if ring.submission().push(&sqe).is_err() {
                        if self.pending.expect_cqe(user_data).is_err() {
                            self.poisoned = true;
                        }
                        return Err(StoreError::BudgetFull("io_uring SQ"));
                    }
                }
                Ok(())
            }
            SessionBackend::Pool(pool) => pool.push_pwrite(handle, offset, buf, user_data),
            #[cfg(windows)]
            SessionBackend::Iocp(eng) => eng.push_pwrite(handle, offset, buf, user_data),
        };
        if r.is_err() {
            self.rollback_pending(user_data);
        }
        r
    }

    pub fn sync_submission(&mut self) {
        #[cfg(target_os = "linux")]
        if let SessionBackend::Uring(ring) = &mut self.backend {
            ring.submission().sync();
        }
    }

    /// Submit pending SQEs and wait for at least one CQE. Does not harvest.
    ///
    /// Slow-log after 5 s of zero completions; poison at the drain hard cap.
    /// A lost CQE / phantom pending must not park lookup forever.
    pub fn submit_and_wait_one(&mut self) -> Result<(), StoreError> {
        rbitcoin_primitives::assert_not_script_worker("store read");
        self.check_live()?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut budget = DrainBudget::with_defaults();
        loop {
            let t0 = Instant::now();
            match self.wait_one_window() {
                WaitOneWindow::Ready => return Ok(()),
                WaitOneWindow::Idle => {
                    self.poisoned = true;
                    return Err(StoreError::Corrupt("invariant: io_uring wait timeout"));
                }
                #[cfg(target_os = "linux")]
                WaitOneWindow::Enter(err) => {
                    self.poisoned = true;
                    return Err(err);
                }
                WaitOneWindow::TimedOut => match budget.note(t0.elapsed(), 0) {
                    DrainVerdict::Continue => {}
                    DrainVerdict::LogSlow => self.note_slow_drain(budget.elapsed()),
                    DrainVerdict::HardCap => {
                        self.poisoned = true;
                        return Err(StoreError::Corrupt("invariant: io_uring wait timeout"));
                    }
                },
            }
        }
    }

    #[doc(hidden)]
    pub fn wait_one_window(&mut self) -> WaitOneWindow {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(ring) => {
                ring.submission().sync();
                let ts = io_uring::types::Timespec::new().nsec(DRAIN_WAIT_NS);
                let args = io_uring::types::SubmitArgs::new().timespec(&ts);
                let enter = ring.submitter().submit_with_args(1, &args);
                let cq_ready = {
                    let mut cq = ring.completion();
                    cq.sync();
                    cq.len()
                };
                wait_window_after_enter(enter, cq_ready)
            }
            SessionBackend::Pool(pool) => {
                if pool.wait_one_cqe_timeout(DRAIN_WINDOW) {
                    WaitOneWindow::Ready
                } else if pool.inflight() == 0 {
                    WaitOneWindow::Idle
                } else {
                    WaitOneWindow::TimedOut
                }
            }
            #[cfg(windows)]
            SessionBackend::Iocp(eng) => {
                if eng.wait_one_cqe_timeout(100) {
                    WaitOneWindow::Ready
                } else {
                    WaitOneWindow::TimedOut
                }
            }
        }
    }

    /// Kick submission without waiting (non-blocking).
    pub fn submit(&mut self) -> Result<(), StoreError> {
        rbitcoin_primitives::assert_not_script_worker("store read");
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(ring) => {
                ring.submit()
                    .map_err(|_| StoreError::Corrupt("io_uring submit failed"))?;
                Ok(())
            }
            SessionBackend::Pool(_) => Ok(()),
            #[cfg(windows)]
            SessionBackend::Iocp(_) => Ok(()),
        }
    }

    /// Harvest all currently ready CQEs (non-blocking). Each item is
    /// `(user_data, result)` where `result` is bytes transferred or negated errno.
    ///
    /// A CQE whose `user_data` is not pending is **not** a completion —
    /// `Corrupt("invariant: io_uring unexpected cqe")`. Expected CQEs in the
    /// same harvest are still returned after the error is noted; callers must
    /// treat `Err` as fatal and drain.
    pub fn harvest_ready(&mut self) -> Result<Vec<(u64, i32)>, StoreError> {
        #[cfg(target_os = "linux")]
        let mut overflow = 0u32;
        let raw = match &mut self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(ring) => {
                overflow = {
                    let cq = ring.completion();
                    cq.overflow()
                };
                ring.completion().sync();
                ring.completion()
                    .map(|cqe| (cqe.user_data(), cqe.result()))
                    .collect::<Vec<_>>()
            }
            SessionBackend::Pool(pool) => pool.harvest_ready(),
            #[cfg(windows)]
            SessionBackend::Iocp(eng) => eng.harvest_ready(),
        };
        #[cfg(target_os = "linux")]
        if overflow != 0 {
            self.poison();
            cq_overflow_result(overflow)?;
        }
        let mut out = Vec::new();
        let mut unexpected = false;
        for (ud, res) in raw {
            match self.pending.expect_cqe(ud) {
                Ok(()) => out.push((ud, res)),
                Err(_) => unexpected = true,
            }
        }
        if unexpected {
            self.poison();
            Err(StoreError::Corrupt("invariant: io_uring unexpected cqe"))
        } else {
            Ok(out)
        }
    }

    /// Wait for and discard **all** in-flight CQEs.
    ///
    /// Callers that hold SQE buffer pointers **must** call this *before* those
    /// buffers are dropped (error unwind, end of probe batch). A shallow
    /// harvest of only-ready CQEs is not enough — the kernel may still write
    /// into buffers for unfinished SQEs (use-after-free → SIGSEGV).
    pub fn drain_all(&mut self) -> Result<(), StoreError> {
        self.drain_all_inner(true)
    }

    #[doc(hidden)]
    pub fn drain_all_inner(&mut self, fail_closed: bool) -> Result<(), StoreError> {
        if let Err(e) = self.drain_all_harvest(fail_closed) {
            if fail_closed && !self.pending.is_empty() {
                // Returning would let the caller free buffers these SQEs own.
                self.poisoned = true;
                return finish_drain_hard_cap(self.pending.len());
            }
            return Err(e);
        }
        if let Err(e) = self.pending.assert_drained() {
            let already = self.poisoned;
            self.poisoned = true;
            if !already {
                self.note_invariant(UringInvariant::Undrained);
            }
            if fail_closed && !self.pending.is_empty() {
                return finish_drain_hard_cap(self.pending.len());
            }
            return Err(e);
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn drain_all_harvest(&mut self, fail_closed: bool) -> Result<(), StoreError> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(_) => self.drain_all_uring(fail_closed),
            SessionBackend::Pool(_) => self.drain_all_pool(fail_closed),
            #[cfg(windows)]
            SessionBackend::Iocp(_) => self.drain_all_iocp(fail_closed),
        }
    }

    #[doc(hidden)]
    pub fn note_slow_drain(&self, waited: Duration) {
        URING_METERS
            .slow_drain
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        rbitcoin_log::warn!(
            "store: io_uring drain slow pending={} waited={:?} thread={thread} kind={:?}",
            self.pending.len(),
            waited,
            self.kind(),
        );
    }

    #[doc(hidden)]
    pub fn apply_drain_window(
        &mut self,
        budget: &mut DrainBudget,
        completed: usize,
        dt: Duration,
        fail_closed: bool,
    ) -> Result<(), StoreError> {
        match budget.note(dt, completed) {
            DrainVerdict::Continue => Ok(()),
            DrainVerdict::LogSlow => {
                self.note_slow_drain(budget.elapsed());
                Ok(())
            }
            DrainVerdict::HardCap => {
                self.poisoned = true;
                if fail_closed {
                    finish_drain_hard_cap(self.pending.len())
                } else {
                    let thread = std::thread::current();
                    let thread = thread.name().unwrap_or("unnamed");
                    rbitcoin_log::warn!(
                        "store: io_uring drain hard cap on drop pending={} thread={thread}",
                        self.pending.len()
                    );
                    Err(StoreError::Corrupt("invariant: io_uring undrained"))
                }
            }
        }
    }

    #[doc(hidden)]
    pub fn pool_inflight(&self) -> usize {
        match &self.backend {
            SessionBackend::Pool(pool) => pool.inflight(),
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(_) => 0,
            #[cfg(windows)]
            SessionBackend::Iocp(_) => 0,
        }
    }

    #[doc(hidden)]
    pub fn pool_harvest_uds(&mut self) -> Vec<u64> {
        match &mut self.backend {
            SessionBackend::Pool(pool) => {
                pool.harvest_ready().into_iter().map(|(ud, _)| ud).collect()
            }
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(_) => Vec::new(),
            #[cfg(windows)]
            SessionBackend::Iocp(_) => Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn pool_wait_harvest_uds(&mut self) -> Vec<u64> {
        match &mut self.backend {
            SessionBackend::Pool(pool) => {
                let _ = pool.wait_one_cqe_timeout(DRAIN_WINDOW);
                pool.harvest_ready().into_iter().map(|(ud, _)| ud).collect()
            }
            #[cfg(target_os = "linux")]
            SessionBackend::Uring(_) => Vec::new(),
            #[cfg(windows)]
            SessionBackend::Iocp(_) => Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn drain_all_pool(&mut self, fail_closed: bool) -> Result<(), StoreError> {
        let mut budget = DrainBudget::with_defaults();
        loop {
            if self.pool_inflight() == 0 {
                let uds = self.pool_harvest_uds();
                return self.apply_drain_cqes(uds);
            }
            let before = self.pending.len();
            let t0 = Instant::now();
            let uds = self.pool_wait_harvest_uds();
            self.apply_drain_cqes(uds)?;
            let completed = before.saturating_sub(self.pending.len());
            self.apply_drain_window(&mut budget, completed, t0.elapsed(), fail_closed)?;
        }
    }

    #[cfg(windows)]
    #[doc(hidden)]
    pub fn drain_all_iocp(&mut self, fail_closed: bool) -> Result<(), StoreError> {
        let mut budget = DrainBudget::with_defaults();
        loop {
            let before = self.pending.len();
            let t0 = Instant::now();
            let wait_res = match &mut self.backend {
                SessionBackend::Iocp(eng) => eng.wait_idle(),
                _ => Ok(()),
            };
            wait_res?;
            let uds = match &mut self.backend {
                SessionBackend::Iocp(eng) => {
                    eng.harvest_ready().into_iter().map(|(ud, _)| ud).collect()
                }
                _ => Vec::new(),
            };
            self.apply_drain_cqes(uds)?;
            if self.pending.is_empty() {
                return Ok(());
            }
            let completed = before.saturating_sub(self.pending.len());
            self.apply_drain_window(&mut budget, completed, t0.elapsed(), fail_closed)?;
        }
    }

    #[doc(hidden)]
    pub fn apply_drain_cqes(&mut self, uds: Vec<u64>) -> Result<(), StoreError> {
        let mut unexpected = false;
        for ud in uds {
            if self.pending.expect_cqe(ud).is_err() {
                unexpected = true;
            }
        }
        if unexpected {
            self.poison();
            Err(StoreError::Corrupt("invariant: io_uring unexpected cqe"))
        } else {
            Ok(())
        }
    }

    #[doc(hidden)]
    pub fn note_invariant(&self, kind: UringInvariant) {
        kind.counter()
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        rbitcoin_log::warn!(
            "store: io_uring invariant {} pending={} epoch={} thread={thread} (Corrupt; not a TipOnly miss)",
            kind.label(),
            self.pending.len(),
            self.epoch,
        );
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn drain_all_uring(&mut self, fail_closed: bool) -> Result<(), StoreError> {
        let mut unexpected = false;
        let mut overflow = 0u32;
        let mut enter_err = None;
        let mut budget = DrainBudget::with_defaults();
        self.uring_sync_sq();
        while !self.pending.is_empty() {
            let before = self.pending.len();
            let t0 = Instant::now();
            self.uring_drain_window(&mut unexpected, &mut overflow, &mut enter_err);
            if enter_err.is_some() || unexpected || overflow != 0 {
                break;
            }
            let completed = before.saturating_sub(self.pending.len());
            if let Err(e) =
                self.apply_drain_window(&mut budget, completed, t0.elapsed(), fail_closed)
            {
                enter_err = Some(e);
                break;
            }
        }
        if enter_err.is_none() && !unexpected && overflow == 0 {
            self.uring_final_harvest(&mut unexpected);
        }
        if let Some(err) = enter_err {
            self.poisoned = true;
            return inflight_enter_failure(self.pending.len(), err);
        }
        if unexpected {
            self.poison();
            return Err(StoreError::Corrupt("invariant: io_uring unexpected cqe"));
        }
        if overflow != 0 {
            self.poisoned = true;
            cq_overflow_result(overflow)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn uring_sync_sq(&mut self) {
        if let SessionBackend::Uring(ring) = &mut self.backend {
            ring.submission().sync();
        }
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn uring_drain_window(
        &mut self,
        unexpected: &mut bool,
        overflow: &mut u32,
        enter_err: &mut Option<StoreError>,
    ) {
        let SessionBackend::Uring(ring) = &mut self.backend else {
            return;
        };
        let ts = io_uring::types::Timespec::new().nsec(DRAIN_WAIT_NS);
        let args = io_uring::types::SubmitArgs::new().timespec(&ts);
        match ring.submitter().submit_with_args(1, &args) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => {}
            Err(e) => {
                if let Some(err) = map_enter_err(&e) {
                    *enter_err = Some(err);
                    return;
                }
            }
        }
        *overflow = {
            let cq = ring.completion();
            cq.overflow()
        };
        ring.completion().sync();
        for cqe in ring.completion() {
            if self.pending.expect_cqe(cqe.user_data()).is_err() {
                *unexpected = true;
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn uring_final_harvest(&mut self, unexpected: &mut bool) {
        rbitcoin_primitives::assert_not_script_worker("store read");
        let SessionBackend::Uring(ring) = &mut self.backend else {
            return;
        };
        let _ = ring.submit();
        ring.completion().sync();
        for cqe in ring.completion() {
            if self.pending.expect_cqe(cqe.user_data()).is_err() {
                *unexpected = true;
            }
        }
    }
}

impl UringSession {
    /// A frame boundary has no buffer owner. Never drain pending SQEs here.
    /// Aborts unless the `with_drain_hard_cap` test seam is active.
    fn fail_closed_boundary(&mut self) -> Result<(), StoreError> {
        self.poisoned = true;
        finish_drain_hard_cap(self.pending.len())
    }

    /// Drain while the guard's SQE buffers are still live. Declare the guard
    /// after those buffers. On unwind, abort instead: a caller-frame guard
    /// cannot know whether a callee already freed them.
    ///
    /// `fail_closed` aborts unless this thread is inside [`with_drain_hard_cap`].
    /// Leftover ids stay in `pending`.
    #[doc(hidden)]
    pub fn drop_drain(&mut self) {
        if std::thread::panicking() && self.in_flight() != 0 {
            abort_uring_unusable("panic with in-flight SQEs during drop");
        }
        if let Err(err) = self.drain_all_inner(true) {
            note_drop_drain_error(&err);
        }
        if !self.pending.is_empty() {
            // A failed drain left SQEs that may still own caller buffers.
            let _ = finish_drain_hard_cap(self.pending.len());
        }
    }
}

#[doc(hidden)]
pub fn note_drop_drain_error(err: &StoreError) {
    #[cfg(debug_assertions)]
    if let StoreError::Corrupt(msg) = err {
        DROP_DRAIN_ERROR.with(|c| c.set(Some(*msg)));
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = err;
    }
}

impl Drop for UringSession {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            // No buffer owner is known at session drop. Draining here could
            // complete into memory a previous frame already freed.
            let _ = finish_drain_hard_cap(self.pending.len());
        }
    }
}

/// Drain the session when dropped, while the guard's SQE buffers are live.
///
/// Declare **after** SQE buffers owned by the same frame so this drops first
/// and drains while they are still live. A guard in a caller frame cannot
/// outlive buffers owned by a callee: use [`AbortOnDrop`] there instead.
pub struct DrainOnDrop<'a> {
    pub session: &'a mut UringSession,
}

impl UringSession {
    pub fn drain_guard(&mut self) -> DrainOnDrop<'_> {
        DrainOnDrop { session: self }
    }
}

impl Drop for DrainOnDrop<'_> {
    fn drop(&mut self) {
        self.session.drop_drain();
    }
}

impl std::ops::Deref for DrainOnDrop<'_> {
    type Target = UringSession;
    fn deref(&self) -> &UringSession {
        self.session
    }
}

impl std::ops::DerefMut for DrainOnDrop<'_> {
    fn deref_mut(&mut self) -> &mut UringSession {
        self.session
    }
}

/// Abort if any SQE is still pending when dropped. Never drains.
///
/// Use in a caller frame or as a backstop where the SQE buffers belong to a
/// callee or a previous call and may already be gone. The buffer-owning frame
/// must drain explicitly (or use [`DrainOnDrop`] declared after its buffers).
pub struct AbortOnDrop<'a> {
    pub session: &'a mut UringSession,
}

impl UringSession {
    pub fn abort_guard(&mut self) -> AbortOnDrop<'_> {
        AbortOnDrop { session: self }
    }
}

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        if !self.session.pending.is_empty() {
            let _ = self.session.fail_closed_boundary();
        }
    }
}

impl std::ops::Deref for AbortOnDrop<'_> {
    type Target = UringSession;
    fn deref(&self) -> &UringSession {
        self.session
    }
}

impl std::ops::DerefMut for AbortOnDrop<'_> {
    fn deref_mut(&mut self) -> &mut UringSession {
        self.session
    }
}

/// Full-length CQE or `StoreError::io`. Short success is not a soft miss.
pub fn require_full_cqe(res: i32, want: usize, path: &Path) -> Result<(), StoreError> {
    if res < 0 {
        return Err(StoreError::io(
            path,
            std::io::Error::from_raw_os_error(-res),
        ));
    }
    if res as usize != want {
        return Err(StoreError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "io_uring short cqe"),
        ));
    }
    Ok(())
}

pub fn note_uring_recover() {
    URING_METERS
        .recover_n
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn uring_slow_drain_count() -> u64 {
    URING_METERS
        .slow_drain
        .load(std::sync::atomic::Ordering::Relaxed)
}

pub fn uring_recover_count() -> u64 {
    URING_METERS
        .recover_n
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Run `f` with this **OS thread's** long-lived io_uring session.
///
/// - Opens once on first use; reopens only if `min_entries` exceeds the current
///   ring size (grows in place, never shrinks).
/// - **Nested** calls **panic** (hard error). Do not call bulk_io / another
///   `with_thread_local` while holding the session — fold IO into the outer
///   machine instead (e.g. plan `STAGE_IDX` for body_range).
/// - Leftover in-flight CQEs from a previous call are fatal: that frame's
///   buffers are gone, so this aborts instead of draining them.
/// - `f` must drain its own buffers (or use a guard declared after them)
///   before returning. Leftover SQEs after `f` are fatal, as is a panic after
///   SQEs were queued: neither a drain nor session reuse is safe.
///
/// Returns `Err` if io_uring is unavailable or setup fails. Callers that prefer
/// a silent fallback can map `Err` themselves (bulk_io does).
pub fn with_thread_local<R>(
    min_entries: u32,
    f: impl FnOnce(&mut UringSession) -> R,
) -> Result<R, StoreError> {
    let min_entries = min_entries.clamp(32, 4096);

    {
        // Gate once; TLS open uses try_open to avoid recursive enabled() probe.
        if !crate::bulk_io::io_uring_enabled() {
            return Err(StoreError::Unavailable);
        }

        DEPTH.with(|depth| {
            let d = depth.get();
            if d != 0 {
                panic!(
                    "nested thread-local io_uring (depth={d}); \
                     fold IO into the outer with_thread_local machine \
                     (do not call bulk_io / with_thread_local while holding the ring)"
                );
            }
            depth.set(1);
            struct DepthGuard;
            impl Drop for DepthGuard {
                fn drop(&mut self) {
                    DEPTH.with(|depth| depth.set(0));
                }
            }
            let _guard = DepthGuard;
            let out = SESSION.with(|cell| -> Result<R, StoreError> {
                let mut slot = cell.borrow_mut();
                let need_open = match slot.as_ref() {
                    None => true,
                    Some(s) => s.entries() < min_entries,
                };
                if need_open {
                    if let Some(mut old) = slot.take() {
                        if old.in_flight() != 0 {
                            let _ = old.fail_closed_boundary();
                            return Err(StoreError::Corrupt(
                                "invariant: io_uring leftover at session growth",
                            ));
                        }
                        drop(old);
                    }
                    let open_n = min_entries.max(DEFAULT_ENTRIES);
                    *slot = Some(UringSession::try_open(open_n)?);
                }
                let session = slot.as_mut().expect("session just ensured");
                if session.is_poisoned() {
                    let _ = slot.take();
                    *slot = Some(UringSession::try_open(min_entries.max(DEFAULT_ENTRIES))?);
                }
                let session = slot.as_mut().expect("session just ensured");
                if session.in_flight() != 0 {
                    let _ = session.fail_closed_boundary();
                    let _ = slot.take();
                    return Err(StoreError::Corrupt(
                        "invariant: io_uring leftover at session entry",
                    ));
                }
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(session)));
                match out {
                    Ok(out) => {
                        if session.in_flight() != 0 {
                            // `f`'s buffers are already gone. Never drain them.
                            let _ = session.fail_closed_boundary();
                            let _ = slot.take();
                            return Err(StoreError::Corrupt(
                                "invariant: io_uring leftover after session use",
                            ));
                        }
                        if session.is_poisoned() {
                            let _ = slot.take();
                        }
                        Ok(out)
                    }
                    Err(payload) => {
                        // Unwinding may already have freed buffers the SQEs
                        // point at; callers without a drain guard (bulk_io)
                        // still own theirs, but we cannot tell them apart here.
                        // Abort rather than drain into memory an inner frame
                        // may have freed, and never reuse this session.
                        if session.in_flight() != 0 {
                            abort_uring_unusable("panic with in-flight SQEs");
                        }
                        if session.is_poisoned() {
                            let _ = slot.take();
                        }
                        std::panic::resume_unwind(payload)
                    }
                }
            });
            out
        })
    }
}

/// Kind byte for [`pack_ud`]. Distinct per machine so a leftover CQE cannot
/// complete a different stage's slot (pwrite slot `5` ≠ ID op `5`).
pub const KIND_BULK_PREAD: u8 = 1;
pub const KIND_BULK_PWRITE: u8 = 4;
pub const KIND_SPEND_PAGE_READ: u8 = 7;
pub const KIND_SPEND_PAGE_WRITE: u8 = 8;
/// SP-tweak machine: `txout.body` pread.
pub const KIND_SP_TXOUT: u8 = 12;
/// SP-tweak machine: `seqsigwit.body` pread (P2TR only).
pub const KIND_SP_SEQSIGWIT: u8 = 13;
/// SP-tweak machine: parent `txout.body` pread.
pub const KIND_SP_PARENT: u8 = 14;
pub const KIND_MPHF_G: u8 = 15;
/// Block index build machine (filters + tweaks): every stage's reads.
pub const KIND_INDEX_BUILD: u8 = 16;

/// Pack `(kind, epoch, slot)` into `user_data`.
///
/// Layout: kind `u8` @ 56, epoch `u16` @ 40, slot `u32` @ 0.
/// Used by multi-stage io_uring machines (head resolve, spend annotate, bulk).
#[inline]
pub fn pack_ud(kind: u8, epoch: u16, slot: u32) -> u64 {
    ((kind as u64) << 56) | ((epoch as u64) << 40) | (slot as u64)
}

/// Unpack [`pack_ud`].
#[inline]
pub fn unpack_ud(ud: u64) -> (u8, u16, u32) {
    let kind = (ud >> 56) as u8;
    let epoch = (ud >> 40) as u16;
    let slot = ud as u32;
    (kind, epoch, slot)
}

/// Process counters for harvest invariants (tests + operator logs).
#[derive(Default)]
pub struct UringMeters {
    pub unexpected_cqe: std::sync::atomic::AtomicU64,
    pub undrained: std::sync::atomic::AtomicU64,
    #[cfg(any(debug_assertions, target_os = "linux"))]
    pub cq_overflow: std::sync::atomic::AtomicU64,
    pub idx_range_missing: std::sync::atomic::AtomicU64,
    pub slow_drain: std::sync::atomic::AtomicU64,
    pub recover_n: std::sync::atomic::AtomicU64,
}

#[doc(hidden)]
pub static URING_METERS: UringMeters = UringMeters {
    unexpected_cqe: std::sync::atomic::AtomicU64::new(0),
    undrained: std::sync::atomic::AtomicU64::new(0),
    #[cfg(any(debug_assertions, target_os = "linux"))]
    cq_overflow: std::sync::atomic::AtomicU64::new(0),
    idx_range_missing: std::sync::atomic::AtomicU64::new(0),
    slow_drain: std::sync::atomic::AtomicU64::new(0),
    recover_n: std::sync::atomic::AtomicU64::new(0),
};

#[derive(Clone, Copy)]
pub enum UringInvariant {
    UnexpectedCqe,
    Undrained,
    #[cfg(any(debug_assertions, target_os = "linux"))]
    CqOverflow,
    IdxRangeMissing,
}

impl UringInvariant {
    #[doc(hidden)]
    pub fn counter(self) -> &'static std::sync::atomic::AtomicU64 {
        match self {
            Self::UnexpectedCqe => &URING_METERS.unexpected_cqe,
            Self::Undrained => &URING_METERS.undrained,
            #[cfg(any(debug_assertions, target_os = "linux"))]
            Self::CqOverflow => &URING_METERS.cq_overflow,
            Self::IdxRangeMissing => &URING_METERS.idx_range_missing,
        }
    }

    #[doc(hidden)]
    pub fn label(self) -> &'static str {
        match self {
            Self::UnexpectedCqe => "unexpected_cqe",
            Self::Undrained => "undrained",
            #[cfg(any(debug_assertions, target_os = "linux"))]
            Self::CqOverflow => "cq_overflow",
            Self::IdxRangeMissing => "idx_range_missing",
        }
    }
}

pub fn note_uring_invariant(kind: UringInvariant) {
    kind.counter()
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    rbitcoin_log::warn!(
        "store: io_uring invariant {} thread={thread} (Corrupt; not a TipOnly miss)",
        kind.label()
    );
}

/// Expected in-flight `user_data` values for one harvest wave.
///
/// Unmatched or duplicate CQEs are **not** completions — they are
/// `Corrupt("invariant: io_uring unexpected cqe")`.
#[derive(Default)]
pub struct UringPending {
    pub set: std::collections::HashSet<u64>,
}

impl UringPending {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Record a submitted SQE. Duplicate `ud` is an invariant fail.
    pub fn insert(&mut self, ud: u64) -> Result<(), StoreError> {
        if !self.set.insert(ud) {
            note_uring_invariant(UringInvariant::UnexpectedCqe);
            return Err(StoreError::Corrupt("invariant: io_uring unexpected cqe"));
        }
        Ok(())
    }

    /// Accept one CQE. Unmatched or already-completed `ud` is an invariant fail.
    /// On `Err`, `len` is unchanged (the CQE does not count as a completion).
    pub fn expect_cqe(&mut self, ud: u64) -> Result<(), StoreError> {
        if !self.set.remove(&ud) {
            note_uring_invariant(UringInvariant::UnexpectedCqe);
            return Err(StoreError::Corrupt("invariant: io_uring unexpected cqe"));
        }
        Ok(())
    }

    /// After drain: leftover pending SQEs are an invariant, not a silent OK.
    pub fn assert_drained(&self) -> Result<(), StoreError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Corrupt("invariant: io_uring undrained"))
        }
    }
}

#[cfg(any(debug_assertions, target_os = "linux"))]
pub fn cq_overflow_result(overflow: u32) -> Result<(), StoreError> {
    if overflow == 0 {
        Ok(())
    } else {
        note_uring_invariant(UringInvariant::CqOverflow);
        Err(StoreError::Corrupt("invariant: io_uring cq overflow"))
    }
}

/// Enter success is SQE count. Ready iff the CQ already has an event.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn wait_window_after_enter(
    enter: Result<usize, std::io::Error>,
    cq_ready: usize,
) -> WaitOneWindow {
    if cq_ready > 0 {
        return WaitOneWindow::Ready;
    }
    match enter {
        Ok(_) => WaitOneWindow::TimedOut,
        Err(e) if e.raw_os_error() == Some(libc::ETIME) => WaitOneWindow::TimedOut,
        Err(e) => {
            if let Some(err) = map_enter_err(&e) {
                WaitOneWindow::Enter(err)
            } else {
                WaitOneWindow::TimedOut
            }
        }
    }
}

/// `None` → retry the enter (EINTR). `Some` → hard fail.
#[cfg(any(debug_assertions, target_os = "linux"))]
pub fn map_enter_err(err: &std::io::Error) -> Option<StoreError> {
    if err.raw_os_error() == Some(libc::EINTR) {
        None
    } else {
        Some(StoreError::Corrupt("io_uring submit_and_wait failed"))
    }
}
