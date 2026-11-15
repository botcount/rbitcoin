use rbitcoin_log::*;
use std::io::{self, Write};
use std::time::UNIX_EPOCH;

use std::sync::{Mutex, OnceLock};

/// Global log level is process-wide; serialize tests that call `init*`.
fn lock_log_init() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn level_parse_and_order() {
    assert_eq!(Level::parse("INFO"), Some(Level::Info));
    assert_eq!(Level::parse("warning"), Some(Level::Warn));
    assert_eq!(Level::parse("t"), Some(Level::Trace));
    assert!(Level::Error < Level::Warn);
    assert!(Level::Warn < Level::Info);
    assert!(Level::Info < Level::Debug);
    assert_eq!(Level::Info.as_str(), "INFO");
    assert_eq!(format!("{}", Level::Warn), "WARN");
}

#[test]
fn level_spec_env_style() {
    assert_eq!(parse_level_spec("debug"), Some(Level::Debug));
    assert_eq!(parse_level_spec("rbitcoin=trace"), Some(Level::Trace));
    assert_eq!(parse_level_spec("warn,net=debug"), Some(Level::Debug));
    assert_eq!(parse_level_spec("   "), None);
    assert_eq!(parse_level_spec("garbage"), None);
}

#[test]
fn init_and_enabled() {
    let _g = lock_log_init();
    init(Level::Warn);
    assert!(enabled(Level::Error));
    assert!(enabled(Level::Warn));
    assert!(!enabled(Level::Info));
    init_off();
    assert!(!enabled(Level::Error));
    init(Level::Info); // restore default for other tests in process
}

#[test]
fn timestamp_epoch() {
    let s = format_timestamp(UNIX_EPOCH);
    assert_eq!(s, "1970-01-01T00:00:00.000Z");
    let s = format_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200));
    assert_eq!(s, "2024-01-01T00:00:00.000Z");
    // February and December are the Hinnant month arms. A log line's
    // timestamp is the operator surface; these two dates are the arms.
    let s = format_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(31 * 86_400));
    assert_eq!(s, "1970-02-01T00:00:00.000Z");
    let s = format_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(364 * 86_400));
    assert_eq!(s, "1970-12-31T00:00:00.000Z");
}

#[test]
fn debug_log_file_holds_the_line_when_log_at_returns() {
    let _g = lock_log_init();
    let path =
        std::env::temp_dir().join(format!("rbtc-debug-log-{}-{}", std::process::id(), "holds"));
    let _ = std::fs::remove_file(&path);
    reset_debug_log();
    std::env::set_var("RBITCOIN_DEBUG_LOG", &path);
    init(Level::Info);
    log_at(Level::Info, format_args!("ping timeout marker {}", 1201));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    reset_debug_log();
    std::env::remove_var("RBITCOIN_DEBUG_LOG");
    let _ = std::fs::remove_file(&path);
    assert!(
        text.contains("ping timeout marker 1201"),
        "debug log file missing the line: {text:?}"
    );
}

#[test]
fn debug_log_open_failure_stays_off_until_reset() {
    let _g = lock_log_init();
    let missing = std::env::temp_dir().join(format!(
        "rbtc-debug-missing-{}-no-such-dir/debug.log",
        std::process::id()
    ));
    let good =
        std::env::temp_dir().join(format!("rbtc-debug-log-{}-{}", std::process::id(), "retry"));
    let _ = std::fs::remove_file(&good);
    reset_debug_log();
    std::env::set_var("RBITCOIN_DEBUG_LOG", &missing);
    init(Level::Info);
    log_at(Level::Info, format_args!("open failed"));
    std::env::set_var("RBITCOIN_DEBUG_LOG", &good);
    log_at(Level::Info, format_args!("must stay off"));
    let before_reset = std::fs::read_to_string(&good).unwrap_or_default();
    reset_debug_log();
    log_at(Level::Info, format_args!("after reset"));
    let after_reset = std::fs::read_to_string(&good).unwrap_or_default();
    reset_debug_log();
    std::env::remove_var("RBITCOIN_DEBUG_LOG");
    let _ = std::fs::remove_file(&good);
    assert!(
        !before_reset.contains("must stay off"),
        "failed open retried on the next line: {before_reset:?}"
    );
    assert!(
        after_reset.contains("after reset"),
        "reset did not open the new path: {after_reset:?}"
    );
}

#[test]
fn log_at_respects_level() {
    let _g = lock_log_init();
    init(Level::Error);
    assert!(!enabled(Level::Info));
    assert!(enabled(Level::Error));
    capture_logs(true);
    log_at(Level::Info, format_args!("hidden"));
    log_at(Level::Error, format_args!("shown {}", 1));
    log_at_style(Level::Error, Style::Bold, format_args!("bold shown"));
    let logs = take_logs();
    capture_logs(false);
    init(Level::Info);
    assert!(
        logs.iter()
            .any(|(l, m)| *l == Level::Error && m.contains("shown 1")),
        "{logs:?}"
    );
    assert!(
        logs.iter()
            .any(|(l, m)| *l == Level::Error && m.contains("bold shown")),
        "{logs:?}"
    );
    // Capture is independent of enabled(); Info is recorded but not emitted.
    assert!(
        logs.iter()
            .any(|(l, m)| *l == Level::Info && m.contains("hidden")),
        "{logs:?}"
    );
}

#[test]
fn style_default_is_plain() {
    assert_eq!(Style::default(), Style::Plain);
}

#[test]
fn level_as_str_all_variants() {
    assert_eq!(Level::Error.as_str(), "ERROR");
    assert_eq!(Level::Debug.as_str(), "DEBUG");
    assert_eq!(Level::Trace.as_str(), "TRACE");
    assert_eq!(Level::parse("off"), None);
    assert_eq!(Level::parse("none"), None);
    assert_eq!(Level::parse("0"), None);
    assert_eq!(Level::parse("e"), Some(Level::Error));
    assert_eq!(Level::parse("w"), Some(Level::Warn));
    assert_eq!(Level::parse("i"), Some(Level::Info));
    assert_eq!(Level::parse("d"), Some(Level::Debug));
}

#[test]
fn enabled_maps_all_stored_values() {
    let _g = lock_log_init();
    init(Level::Error);
    assert!(enabled(Level::Error));
    assert!(!enabled(Level::Warn));
    init(Level::Debug);
    assert!(enabled(Level::Debug));
    assert!(!enabled(Level::Trace));
    init(Level::Trace);
    assert!(enabled(Level::Trace));
    init(Level::Info);
}

#[test]
fn init_from_env_off_and_level() {
    let _g = lock_log_init();
    // Save/restore around env mutation for process safety.
    let prev_rb = std::env::var_os("RBITCOIN_LOG");
    let prev_rust = std::env::var_os("RUST_LOG");
    std::env::remove_var("RUST_LOG");
    std::env::set_var("RBITCOIN_LOG", "debug");
    assert!(init_from_env());
    assert!(enabled(Level::Debug));
    assert!(!enabled(Level::Trace));
    std::env::set_var("RBITCOIN_LOG", "off");
    assert!(init_from_env());
    assert!(!enabled(Level::Error));
    std::env::set_var("RBITCOIN_LOG", "none");
    assert!(init_from_env());
    std::env::set_var("RBITCOIN_LOG", "0");
    assert!(init_from_env());
    std::env::set_var("RBITCOIN_LOG", "not-a-level");
    assert!(!init_from_env());
    std::env::remove_var("RBITCOIN_LOG");
    // RUST_LOG fallback.
    std::env::set_var("RUST_LOG", "warn");
    assert!(init_from_env());
    assert!(enabled(Level::Warn));
    assert!(!enabled(Level::Info));
    std::env::remove_var("RBITCOIN_LOG");
    std::env::remove_var("RUST_LOG");
    assert!(!init_from_env());
    match prev_rb {
        Some(v) => std::env::set_var("RBITCOIN_LOG", v),
        None => std::env::remove_var("RBITCOIN_LOG"),
    }
    match prev_rust {
        Some(v) => std::env::set_var("RUST_LOG", v),
        None => std::env::remove_var("RUST_LOG"),
    }
    init(Level::Warn);
    assert!(enabled(Level::Warn));
    assert!(!enabled(Level::Info));
    // Bold style path (non-TTY → plain write arm still executed).
    init(Level::Error);
    log_at_style(Level::Error, Style::Bold, format_args!("bold-err"));
    init(Level::Info);
}

// Operator stderr, not the capture buffer. A redirected fd is the log file
// an operator tails. Unix-only: the logger writes fd 2.
#[cfg(unix)]
unsafe extern "C" {
    safe fn dup(fd: i32) -> i32;
    safe fn dup2(oldfd: i32, newfd: i32) -> i32;
    // `close` is a runtime symbol. rustc 1.99 rejects a safe declaration.
    unsafe fn close(fd: i32) -> i32;
}

#[cfg(unix)]
#[test]
fn stderr_line_is_plain_when_not_a_terminal() {
    let _g = lock_log_init();
    init(Level::Error);
    let path = std::env::temp_dir().join(format!(
        "rbitcoin-log-stderr-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let file = std::fs::File::create(&path).unwrap();
    use std::os::fd::AsRawFd;
    let saved = dup(2);
    assert!(saved >= 0);
    assert_eq!(dup2(file.as_raw_fd(), 2), 2);
    log_at_style(Level::Error, Style::Bold, format_args!("operator-line"));
    let controls: String = (0u8..=31).chain([127]).map(char::from).collect();
    log_at(
        Level::Error,
        format_args!("peer={controls} 貓 € 😀 \\\" printable~\n"),
    );
    log_at_style(
        Level::Error,
        Style::Bold,
        format_args!("peer={}\n", "remote\nERROR forged\r\x1b[31m"),
    );
    log_at_style(Level::Info, Style::Plain, format_args!("hidden-info"));
    let _ = io::stderr().flush();
    assert_eq!(dup2(saved, 2), 2);
    // SAFETY: `saved` is the fd from `dup` above, and stderr is already restored.
    unsafe { close(saved) };
    let body = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    init(Level::Info);
    assert!(body.contains("operator-line"), "{body}");
    assert!(!body.contains('\u{1b}'), "{body}");
    assert!(!body.contains("hidden-info"), "{body}");
    let messages: Vec<_> = body
        .lines()
        .map(|line| line.split_once(" ERROR ").unwrap().1)
        .collect();
    assert_eq!(
        messages,
        [
            "operator-line",
            concat!(
                r"peer=\x00\x01\x02\x03\x04\x05\x06\x07",
                r"\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
                r"\x10\x11\x12\x13\x14\x15\x16\x17",
                r"\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\x7f",
                " 貓 € 😀 \\\" printable~\\x0a",
            ),
            r"peer=remote\x0aERROR forged\x0d\x1b[31m\x0a",
        ]
    );
    assert!(body.ends_with('\n'));
}
