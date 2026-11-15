//! Poisoned confirm-feed wait. A session cannot schedule the panic that
//! poisons the mutex, so this is the pure result of the shipped 20ms wait.
use std::sync::atomic::{AtomicUsize, Ordering};

use rbitcoin_ibd::ibd::confirm::ConfirmFeed;

#[test]
fn poisoned_feed_wait_returns_the_guard() {
    let feed = ConfirmFeed::new();
    feed.inner.lock().unwrap().inflight.insert(1);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = feed.inner.lock().unwrap();
        panic!("poison the confirm feed");
    }));
    let calls = AtomicUsize::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        feed.wait_claims_below(2, || calls.fetch_add(1, Ordering::SeqCst) > 0);
    }));
    assert!(result.is_ok(), "poisoned feed wait must return the guard");
}

#[test]
fn notify_wakes_a_feed_waiter() {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let feed = Arc::new(ConfirmFeed::new());
    let armed = Arc::new(Mutex::new(false));
    let feed_w = Arc::clone(&feed);
    let armed_w = Arc::clone(&armed);
    let waiter = std::thread::spawn(move || {
        let guard = feed_w.inner.lock().unwrap();
        *armed_w.lock().unwrap() = true;
        let (guard, timeout) = feed_w
            .cv
            .wait_timeout(guard, Duration::from_millis(400))
            .unwrap();
        drop(guard);
        timeout.timed_out()
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !*armed.lock().unwrap() {
        assert!(Instant::now() < deadline, "waiter did not arm");
        std::thread::yield_now();
    }
    drop(feed.inner.lock().unwrap());
    feed.notify();
    let timed_out = waiter.join().expect("waiter exits");
    assert!(!timed_out, "notify must wake the feed waiter");
}
