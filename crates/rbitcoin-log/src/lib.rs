//! Minimal process logger: leveled lines with UTC timestamps to stderr.
//!
//! No external crates. Configure with [`init`], [`init_from_env`], or
//! `RBITCOIN_LOG` / `--log-level` (wired by the node CLI).
//!
//! Format: `2026-07-15T19:21:03.456Z  INFO message…`
//!
//! When `RBITCOIN_DEBUG_LOG` is set, the plain line is appended to that file
//! and flushed before stderr, and before [`log_at`] returns. The functional
//! harness points it at `debug.log` so a test that reads the file as the
//! socket closes sees a line this call just wrote. A missing env, an empty
//! path, or a path that cannot be opened stays stderr-only until
//! `reset_debug_log`.
//!
//! [`progress`] keeps live done/total for long stages, readable at any level.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub mod api_log;
pub mod progress;

#[doc(hidden)]
pub use api_log::PARAMS_MAX;
pub use api_log::{api_call, close_api_log, init_api_log};

use std::cell::RefCell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static DEBUG_LOG: Mutex<Option<File>> = Mutex::new(None);
/// Set once the env is missing, empty, or the file cannot be opened.
/// Later lines do not lock the environment or call `open` again.
static DEBUG_LOG_OFF: AtomicBool = AtomicBool::new(false);

/// Drop the cached `RBITCOIN_DEBUG_LOG` file and read the env again.
/// Tests delete the path.
#[doc(hidden)]
pub fn reset_debug_log() {
    let mut slot = DEBUG_LOG.lock().unwrap_or_else(|e| e.into_inner());
    DEBUG_LOG_OFF.store(false, Ordering::Relaxed);
    *slot = None;
}

/// Open the debug-log file once. `false` means later lines stay on stderr.
fn debug_log_sink_ready() -> bool {
    if DEBUG_LOG_OFF.load(Ordering::Relaxed) {
        return false;
    }
    let mut slot = DEBUG_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if DEBUG_LOG_OFF.load(Ordering::Relaxed) {
        return false;
    }
    if slot.is_some() {
        return true;
    }
    let Ok(path) = std::env::var("RBITCOIN_DEBUG_LOG") else {
        DEBUG_LOG_OFF.store(true, Ordering::Relaxed);
        return false;
    };
    if path.is_empty() {
        DEBUG_LOG_OFF.store(true, Ordering::Relaxed);
        return false;
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            *slot = Some(file);
            true
        }
        Err(_) => {
            DEBUG_LOG_OFF.store(true, Ordering::Relaxed);
            false
        }
    }
}

fn append_debug_log(line: &str) {
    if DEBUG_LOG_OFF.load(Ordering::Relaxed) {
        return;
    }
    let mut slot = DEBUG_LOG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(file) = slot.as_mut() else {
        return;
    };
    let locked = file.lock().is_ok();
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
    if locked {
        let _ = file.unlock();
    }
}

fn write_stderr(level: Level, style: Style, ts: &str, message: &EscapedMessage<'_>) {
    let mut stderr = io::stderr().lock();
    let bold = matches!(style, Style::Bold) && io::IsTerminal::is_terminal(&stderr);
    if bold {
        let _ = writeln!(stderr, "{ts} {level:<5} \x1b[1m{message}\x1b[0m");
    } else {
        let _ = writeln!(stderr, "{ts} {level:<5} {message}");
    }
    let _ = stderr.flush();
}

/// Log severity. Higher numeric values are more verbose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// Parse `error|warn|info|debug|trace` (case-insensitive). Also accepts
    /// `warning` and single-letter forms `e|w|i|d|t`. `off`/`none`/`0` is
    /// not a level (`None`) — callers that want silence use [`init_off`].
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "e" => Some(Level::Error),
            "warn" | "warning" | "w" => Some(Level::Warn),
            "info" | "i" => Some(Level::Info),
            "debug" | "d" => Some(Level::Debug),
            "trace" | "t" => Some(Level::Trace),
            "off" | "none" | "0" => None,
            _ => None,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Global max enabled level. 0 = off; default [`Level::Info`].
#[doc(hidden)]
pub static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

thread_local! {
    static CAPTURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CAPTURED: std::cell::RefCell<Vec<(Level, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record [`log_at`] lines on this thread until [`take_logs`]. Off in production.
pub fn capture_logs(on: bool) {
    CAPTURE.with(|c| c.set(on));
    if !on {
        let _ = take_logs();
    }
}

/// Drain lines recorded after [`capture_logs`]`(true)`.
pub fn take_logs() -> Vec<(Level, String)> {
    CAPTURED.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Set the maximum log level (inclusive).
pub fn init(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Disable all logging.
pub fn init_off() {
    MAX_LEVEL.store(0, Ordering::Relaxed);
}

/// Whether `level` would be emitted under the current max.
pub fn enabled(level: Level) -> bool {
    (level as u8) <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// Read `RBITCOIN_LOG` (or optional `RUST_LOG` fallback for the first token).
///
/// Accepts a bare level (`info`) or `rbitcoin=debug` / `*=warn` style; the last
/// recognizable level token wins. Missing/invalid → leave current max unchanged
/// and return `false`.
pub fn init_from_env() -> bool {
    let raw = std::env::var("RBITCOIN_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok();
    let Some(raw) = raw else {
        return false;
    };
    if let Some(level) = parse_level_spec(&raw) {
        init(level);
        return true;
    }
    if raw.trim().eq_ignore_ascii_case("off")
        || raw.trim().eq_ignore_ascii_case("none")
        || raw.trim() == "0"
    {
        init_off();
        return true;
    }
    false
}

/// Extract a [`Level`] from env-style specs (`debug`, `info,rbitcoin=trace`, …).
#[doc(hidden)]
pub fn parse_level_spec(spec: &str) -> Option<Level> {
    let mut found = None;
    for part in spec.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let token = part.rsplit('=').next().unwrap_or(part).trim();
        if let Some(l) = Level::parse(token) {
            found = Some(l);
        }
    }
    found
}

/// Format UTC timestamp `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[doc(hidden)]
pub fn format_timestamp(now: SystemTime) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (y, mo, d, h, mi, s) = civil_utc(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Civil UTC date/time from Unix seconds (proleptic Gregorian, no leap seconds).
#[doc(hidden)]
pub fn civil_utc(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mins = secs / 60;
    let mi = (mins % 60) as u32;
    let hours = mins / 60;
    let h = (hours % 24) as u32;
    let days = hours / 24;

    // Howard Hinnant civil-from-days (not the naive 365.25 approximation).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as u32;
    (y, m, d, h, mi, s)
}

/// Optional line emphasis (ANSI on a TTY stderr only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Plain,
    /// Bold message body (`\x1b[1m…\x1b[0m`) when stderr is a terminal.
    Bold,
}

/// Peer- and client-driven info lines that must not append once per event.
///
/// The slot is the event class. A distinct header hash, peer address, or
/// client address does not open a new slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatedInfo {
    ConnectedToSelf = 0,
    HeaderReject = 1,
    RpcAtMaxConnections = 2,
}

const REPEATED_INFO_GAP_MS: u64 = 1_000;

thread_local! {
    static REPEATED_INFO_AT: RefCell<[Option<Instant>; 3]> = const { RefCell::new([None; 3]) };
}

/// Forget this thread's last emit time. Tests start a burst from an empty window.
#[doc(hidden)]
pub fn reset_repeated_info() {
    REPEATED_INFO_AT.with(|slot| *slot.borrow_mut() = [None; 3]);
}

/// [`log_at`] at info, at most once per second for `which` on this thread.
///
/// A repeat inside the gap is dropped before capture and before stderr.
/// The slot is the event class, so a new header hash does not open a new line.
/// The clock is monotonic, so a wall-clock step does not silence the line.
pub fn info_repeated(which: RepeatedInfo, args: fmt::Arguments<'_>) {
    let now = Instant::now();
    let emit = REPEATED_INFO_AT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let prev = slot[which as usize];
        if prev.is_some_and(|t| {
            now.saturating_duration_since(t) < Duration::from_millis(REPEATED_INFO_GAP_MS)
        }) {
            return false;
        }
        slot[which as usize] = Some(now);
        true
    });
    if emit {
        log_at(Level::Info, args);
    }
}

/// Write one log line if `level` is enabled.
pub fn log_at(level: Level, args: fmt::Arguments<'_>) {
    log_at_style(level, Style::Plain, args);
}

struct EscapedMessage<'a>(&'a str);

impl fmt::Display for EscapedMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut start = 0;
        for (i, byte) in self.0.bytes().enumerate() {
            if !byte.is_ascii_control() {
                continue;
            }
            f.write_str(&self.0[start..i])?;
            write!(f, "\\x{byte:02x}")?;
            start = i + 1;
        }
        f.write_str(&self.0[start..])
    }
}

/// Write one log line with optional style. Bold is applied only when stderr is
/// an interactive terminal so redirected logs stay clean ASCII.
pub fn log_at_style(level: Level, style: Style, args: fmt::Arguments<'_>) {
    if CAPTURE.with(|c| c.get()) {
        CAPTURED.with(|c| c.borrow_mut().push((level, args.to_string())));
    }
    if !enabled(level) {
        return;
    }
    let message = args.to_string();
    let message = EscapedMessage(&message);
    let ts = format_timestamp(SystemTime::now());
    // File first. A full stderr pipe must not delay the line a test reads
    // from debug.log before this call returns and the peer is dropped.
    if debug_log_sink_ready() {
        let plain = format!("{ts} {level:<5} {message}");
        append_debug_log(&plain);
    }
    write_stderr(level, style, &ts, &message);
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log_at($crate::Level::Error, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log_at($crate::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log_at($crate::Level::Info, format_args!($($arg)*))
    };
}

/// Log an info-level line with bold emphasis (TTY only; plain when redirected).
#[macro_export]
macro_rules! info_bold {
    ($($arg:tt)*) => {
        $crate::log_at_style(
            $crate::Level::Info,
            $crate::Style::Bold,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log_at($crate::Level::Debug, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::log_at($crate::Level::Trace, format_args!($($arg)*))
    };
}
