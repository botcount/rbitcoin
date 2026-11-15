//! Admit join. `Ok` is that value. A panicked child resumes that payload.
use std::panic::AssertUnwindSafe;

use rbitcoin_net::finish_join;

#[test]
fn ok_join_returns_the_value_and_a_panic_resumes_that_payload() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let handle = rt.spawn(async { 7u8 });
    let value = rt.block_on(async { finish_join(handle.await, "tx admit") });
    assert_eq!(value, 7);

    let handle = rt.spawn(async { panic!("admit payload") });
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(async { finish_join(handle.await, "tx admit") });
    }));
    let payload = caught.expect_err("panicked admit must resume");
    let msg = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
    assert_eq!(msg, Some("admit payload"));

    let handle = rt.spawn(std::future::pending::<()>());
    handle.abort();
    let cancelled = std::panic::catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(async { finish_join(handle.await, "tx admit") });
    }));
    assert!(cancelled.is_err(), "cancelled admit must abort the caller");
}
