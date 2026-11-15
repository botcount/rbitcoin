use rbitcoin_electrum::server::*;
use serde_json::json;

#[test]
fn electrum_perf_window_resets_and_totals_keep_the_note() {
    const NOTE: u64 = 1_000_000_000;
    let _ = sample_reset_perf();
    let before = perf_totals();
    meter_dispatch_wall(NOTE);
    let window = sample_reset_perf();
    let after = perf_totals();
    assert!(window.0 >= 1 && window.1 >= NOTE && window.2 >= NOTE);
    assert!(after.0 > before.0);
    assert!(after.1 >= before.1.saturating_add(NOTE));
}

#[test]
fn silent_payment_log_drops_the_scan_secret() {
    let secret = "ab".repeat(32);
    let line = redact_sp_params(&json!([secret, "02ff", 1]));
    assert!(!line.contains(&secret), "{line}");
    assert!(line.contains("<redacted>"), "{line}");
}
