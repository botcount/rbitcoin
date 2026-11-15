use rbitcoin_ibd::ibd::progress::{
    format_duration_short, format_progress_line, format_rate, ibd_pct, tip_hole_from_claim_ready,
    ProgressLineInput, TipRateTracker,
};
use rbitcoin_query::{
    soft_assign_restricted, soft_confirm_window_n, soft_densify_band_hi, BQ_SOFT_FREE_BYTES,
};
use std::time::{Duration, Instant};

/// Tokens `docs/operator/operations.md` names on `ibd: progress`.
#[test]
fn format_progress_line_names_hole_and_queue() {
    let line = format_progress_line(&ProgressLineInput {
        pct: 42,
        tip: 100_000,
        tip_rate: 12.5,
        tip_hole: 3,
        peers: 8,
        conf_q: "ready=1 scriptq=1/2 writeq=0/2".into(),
        txs: 50_000_000,
        horizon: 900_000,
        eta: "eta=18h".into(),
        bq_bytes: 256 * 1024 * 1024,
        bq_count: 17,
        bq_soft_stop: 180,
    });
    assert!(line.starts_with("ibd: progress "), "{line}");
    assert!(line.contains(" hole=3 "), "{line}");
    assert!(line.contains(" bq soft=17/180 RAM=256MiB"), "{line}");
}

#[test]
fn soft_confirm_window_and_band_at_rate() {
    assert_eq!(soft_confirm_window_n(Some(2.0)), 120, "2 blk/s × 60s");
    assert_eq!(soft_confirm_window_n(None), 0, "cold rate");
    assert_eq!(soft_confirm_window_n(Some(5.0)), 300, "5 blk/s × 60s");
    assert_eq!(soft_confirm_window_n(Some(10.0)), 600);

    let free = BQ_SOFT_FREE_BYTES;
    let over = free + 1;
    assert_eq!(
        soft_densify_band_hi(10, 5000, free, Some(5.0), u64::MAX, None),
        5000
    );
    assert_eq!(
        soft_densify_band_hi(10, 5000, over, Some(5.0), u64::MAX, None),
        309
    );
    assert_eq!(
        soft_densify_band_hi(10, 5000, over, None, u64::MAX, None),
        10
    );
    assert!(!soft_assign_restricted(free));
    assert!(soft_assign_restricted(over));
}

#[allow(clippy::cognitive_complexity)] // one fixture, many progress arms
#[test]
fn pct_tip_hole_and_format_surface() {
    assert_eq!(ibd_pct(0, 100), 0);
    assert_eq!(ibd_pct(50, 100), 50);
    assert_eq!(ibd_pct(100, 100), 100);
    assert_eq!(ibd_pct(200, 100), 100);
    assert_eq!(ibd_pct(0, 0), 0);
    assert_eq!(ibd_pct(5, 0), 100);

    // claim_ready flags from tip+1: false,false,true → hole=2
    assert_eq!(tip_hole_from_claim_ready(&[]), 0);
    assert_eq!(tip_hole_from_claim_ready(&[true, false, false]), 0);
    assert_eq!(tip_hole_from_claim_ready(&[false, false, true, false]), 2);
    assert_eq!(tip_hole_from_claim_ready(&[false, false, false]), 3);

    assert_eq!(format_duration_short(45), "45s");
    assert_eq!(format_duration_short(59), "59s");
    assert_eq!(format_duration_short(60), "1m");
    assert_eq!(format_duration_short(120), "2m");
    assert_eq!(format_duration_short(3_600), "1.0h");
    assert_eq!(format_duration_short(9 * 3600), "9.0h");
    assert_eq!(format_duration_short(36_000), "10h");
    assert_eq!(format_duration_short(86_400), "1d");
    assert_eq!(format_duration_short(2 * 86_400), "2d");
    assert_eq!(format_duration_short(90_000), "1d1h");
    assert_eq!(format_rate(0.0), "0.0");
    assert_eq!(format_rate(2.4), "2.4");
    assert_eq!(format_rate(9.9), "9.9");
    assert_eq!(format_rate(10.0), "10");
    assert_eq!(format_rate(31.2), "31");
    assert_eq!(format_rate(f64::NAN), "0");
    assert_eq!(format_rate(f64::INFINITY), "0");
    assert_eq!(format_rate(-1.0), "0");

    let mut done = TipRateTracker::new();
    let t0 = Instant::now();
    done.push(t0, 100);
    assert_eq!(
        done.eta_string(t0 + Duration::from_secs(30), 100, 100),
        "done"
    );
}

#[test]
fn work_chain_progress_fetch_hole_requires_body_queue() {
    use bitcoin::hashes::Hash;
    use bitcoin::BlockHash;
    use rbitcoin_ibd::ibd::body::BodyPresence;
    use rbitcoin_ibd::ibd::progress::{claim_ready, tip_fetch_hole, work_chain_progress};

    use std::collections::HashMap;

    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("wcp");
    hub.ensure_genesis().unwrap();

    // tip=0 (genesis) → path_lo=1. Heights 1..3 missing from BQ.
    let mut h2h = HashMap::new();
    let mut body = BodyPresence::new();
    let h1 = BlockHash::from_byte_array([1u8; 32]);
    let h2 = BlockHash::from_byte_array([2u8; 32]);
    let h3 = BlockHash::from_byte_array([3u8; 32]);
    h2h.insert(1, h1);
    h2h.insert(2, h2);
    h2h.insert(3, h3);
    body.mark_missing(h1);
    body.mark_missing(h2);
    // Zombie pending without BQ must **not** look claim-ready (mainnet stall).
    body.mark_pending(h3);
    assert!(
        !claim_ready(&hub, &mut body, 3, &h3),
        "pending alone without body queue is not claim-ready"
    );

    let p = work_chain_progress(&hub, &h2h, &mut body, 50, 10);
    assert_eq!(p.tip, 0);
    assert_eq!(
        p.tip_hole, 3,
        "zombie pending at h=3 still counts as fetch hole"
    );

    // Zombie pending at tip+1 → still hole (must re-getdata).
    body.mark_pending(h1);
    let p2 = work_chain_progress(&hub, &h2h, &mut body, 50, 10);
    assert_eq!(p2.tip_hole, 3);

    body.mark_rejected(h1);
    assert!(!claim_ready(&hub, &mut body, 1, &h1));
    let p3 = work_chain_progress(&hub, &h2h, &mut body, 50, 10);
    assert_eq!(p3.tip_hole, 0, "rejected tip+1 is not a download hole");

    // tip_fetch_hole with empty height map (no headers past tip).
    let hole0 = tip_fetch_hole(&hub, &HashMap::new(), &mut body);
    assert_eq!(hole0, 0);

    // Gap in height map stops walk (missing header on path).
    let mut sparse = HashMap::new();
    sparse.insert(1, h1);
    // no height 2 → break after counting hole for h1 if not ready
    let mut body_gap = BodyPresence::new();
    body_gap.mark_missing(h1);
    let hole_gap = tip_fetch_hole(&hub, &sparse, &mut body_gap);
    assert_eq!(hole_gap, 1);

    // BQ claim-ready at tip+1 stops hole after reset body.
    let mut body2 = BodyPresence::new();
    body2.mark_missing(h1);
    hub.query
        .block_queue_enqueue(1, h1.to_byte_array(), 1, b"x")
        .unwrap();
    assert!(claim_ready(&hub, &mut body2, 1, &h1));
    let mut span = rbitcoin_ibd::ibd::progress::QueuedHashSpan::load(&hub, 1, 2);
    assert_eq!(span.hash_at(&hub, 1), Some(h1.to_byte_array()));
    assert_eq!(span.hash_at(&hub, 2), None, "an empty height stays empty");
    let mut inverted = rbitcoin_ibd::ibd::progress::QueuedHashSpan::load(&hub, 2, 1);
    assert_eq!(
        inverted.hash_at(&hub, 1),
        Some(h1.to_byte_array()),
        "an inverted span still reads the live queue"
    );
    hub.query.block_queue_dequeue_height(1).unwrap();
    assert_eq!(
        span.hash_at(&hub, 1),
        None,
        "a listed height is read again after confirm dequeues it"
    );
    hub.query
        .block_queue_enqueue(1, h1.to_byte_array(), 1, b"x")
        .unwrap();
    let hole_ready = tip_fetch_hole(&hub, &h2h, &mut body2);
    assert_eq!(hole_ready, 0, "BQ-ready tip+1 → hole=0");

    // Later gap at height 5 is not operator hole= (prefix is in hand).
    let h5 = BlockHash::from_byte_array([5u8; 32]);
    h2h.insert(5, h5);
    body2.mark_missing(h5);
    let hole_later = tip_fetch_hole(&hub, &h2h, &mut body2);
    assert_eq!(hole_later, 0, "only +5 missing after ready tip+1 → hole=0");

    // Confirmed tip block is claim-ready (hub.has_block).
    let ghash = hub.tip_hash().expect("genesis tip");
    assert!(claim_ready(&hub, &mut body2, 0, &ghash));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn claim_ready_taken_prefix_is_not_a_fetch_hole() {
    use bitcoin::hashes::Hash;
    use bitcoin::BlockHash;
    use rbitcoin_ibd::ibd::body::BodyPresence;
    use rbitcoin_ibd::ibd::progress::{claim_ready, tip_fetch_hole};

    use std::collections::HashMap;

    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("taken-hole");
    hub.ensure_genesis().unwrap();

    let mut h2h = HashMap::new();
    let mut body = BodyPresence::new();
    for i in 1u8..=4 {
        let h = BlockHash::from_byte_array([i; 32]);
        h2h.insert(i as u32, h);
        body.mark_missing(h);
    }
    hub.query.set_lookup_taken_hi(Some(2));
    assert!(claim_ready(&hub, &mut body, 1, &h2h[&1]));
    assert!(claim_ready(&hub, &mut body, 2, &h2h[&2]));
    assert!(!claim_ready(&hub, &mut body, 3, &h2h[&3]));
    assert_eq!(tip_fetch_hole(&hub, &h2h, &mut body), 0);

    hub.query.set_lookup_taken_hi(None);
    assert!(!claim_ready(&hub, &mut body, 1, &h2h[&1]));
    assert_eq!(tip_fetch_hole(&hub, &h2h, &mut body), 4);

    let _ = std::fs::remove_dir_all(dir);
}

/// EWMA tip-rate: warmup gate, steady ETA.
#[test]
fn tip_rate_tracker_eta_surface() {
    let t0 = Instant::now();

    let mut cold = TipRateTracker::new();
    cold.push(t0, 0);
    cold.push(t0 + Duration::from_secs(5), 50);
    assert!(cold.eta_rate(t0 + Duration::from_secs(5)).is_none());
    assert_eq!(
        cold.eta_string(t0 + Duration::from_secs(5), 50, 1_000_000),
        "eta=?"
    );

    let mut steady = TipRateTracker::new();
    for i in 0u32..=60 {
        steady.push(t0 + Duration::from_secs(u64::from(i) * 5), 100 + i * 5);
    }
    let now = t0 + Duration::from_secs(300);
    let rate = steady.eta_rate(now).expect("warmed");
    assert!((rate - 1.0).abs() < 0.15, "rate={rate}");
    // ~7200 remain at ~1 blk/s → ~2h
    let eta = steady.eta_string(now, 400, 7_600);
    assert!(eta.starts_with("eta="), "{eta}");
    assert!(eta.contains('h') || eta.contains('m'), "{eta}");

    // Sub-0.5s dt does not update EMA; cold tracker still eta=?
    let mut tiny = TipRateTracker::new();
    tiny.push(t0, 10);
    tiny.push(t0 + Duration::from_millis(100), 11);
    assert!(tiny.eta_rate(t0 + Duration::from_millis(100)).is_none());
    let mut warm = TipRateTracker::new();
    for i in 0u32..=40 {
        warm.push(t0 + Duration::from_secs(u64::from(i) * 2), i * 2);
    }
    assert_eq!(
        warm.eta_string(t0 + Duration::from_secs(100), 10_000, 100),
        "done"
    );
    assert_eq!(
        warm.eta_string(t0 + Duration::from_secs(100), 100, 100),
        "done"
    );
}

#[test]
fn progress_tick_reads_published_counters() {
    use bitcoin::hashes::Hash;

    let (dir, hub) = rbitcoin_net::chain::tiny_regtest_hub_labeled("prog-pub");
    hub.ensure_genesis().unwrap();
    let gen = hub.tip_hash().unwrap();
    hub.query.publish_confirmed_tip(0, gen.to_byte_array());
    rbitcoin_query::reset_progress_snapshot_counts();
    hub.query.publish_progress_snapshots();
    let (sh_calls, owned_calls) = rbitcoin_query::progress_snapshot_counts();
    assert!(sh_calls >= 1 && owned_calls >= 1);
    let sh = hub.query.published_scripthash_run_count();
    let h2h = hub.query.published_owned_sizes().h2h_keys;
    rbitcoin_query::reset_progress_snapshot_counts();
    let view = rbitcoin_ibd::ibd::progress::read_progress_published(&hub.query);
    assert_eq!(view.tip, Some(0));
    assert_eq!(view.txs, hub.query.tx_body_count());
    assert_eq!(view.sh_runs, sh);
    assert_eq!(view.owned.h2h_keys, h2h);
    assert_eq!(rbitcoin_query::progress_snapshot_counts(), (0, 0));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn progress_tick_logs_a_walk_and_a_quiet_header_line() {
    let t0 = Instant::now();
    let mut clock = rbitcoin_ibd::ibd::progress::StatusClock::new(0, t0);
    let published = rbitcoin_ibd::ibd::progress::ProgressPublished {
        tip: Some(10),
        txs: 4,
        sh_runs: 0,
        owned: rbitcoin_query::ProcessOwnedSizes::default(),
    };
    let mut snap = rbitcoin_ibd::ibd::progress::StatusSnap {
        peers: 1,
        horizon: 100,
        hole: 2,
        header_origin: true,
        header_height: 40,
        header_stored: 30,
        header_queue: 3,
        header_work_ok: false,
        header_walk: true,
        header_quiet: false,
    };
    rbitcoin_ibd::ibd::progress::log_progress_tick(
        &mut clock,
        &snap,
        &published,
        (0, 0, 0),
        1,
        2,
        t0 + Duration::from_secs(5),
    );
    assert!(!clock.saw_quiet, "a walking header line is not caught up");
    snap.header_work_ok = true;
    snap.header_walk = false;
    snap.header_quiet = true;
    rbitcoin_ibd::ibd::progress::log_progress_tick(
        &mut clock,
        &snap,
        &published,
        (0, 0, 0),
        1,
        2,
        t0 + Duration::from_secs(10),
    );
    assert!(clock.saw_quiet, "the caught-up line fires once");
    rbitcoin_ibd::ibd::progress::log_progress_tick(
        &mut clock,
        &snap,
        &published,
        (0, 0, 0),
        1,
        2,
        t0 + Duration::from_secs(15),
    );
    assert!(
        clock.saw_quiet,
        "a second quiet tick does not clear the latch"
    );
    snap.header_quiet = false;
    snap.header_origin = false;
    let unpublished = rbitcoin_ibd::ibd::progress::ProgressPublished {
        tip: None,
        txs: 0,
        sh_runs: 0,
        owned: rbitcoin_query::ProcessOwnedSizes::default(),
    };
    rbitcoin_ibd::ibd::progress::log_progress_tick(
        &mut clock,
        &snap,
        &unpublished,
        (0, 0, 0),
        0,
        0,
        t0 + Duration::from_secs(20),
    );
    assert!(
        !clock.saw_quiet,
        "leaving the walk clears the caught-up latch"
    );
}
