//! `std` synchronization behind one import. `--cfg loom` and `--cfg shuttle`
//! swap in that model's primitives. Production is `std`. The two cfgs are
//! exclusive. The OS script pool still spawns `std::thread`s; the idle turn
//! and the claim atomics go through this module so a model can run them.

#[cfg(all(loom, shuttle))]
compile_error!("--cfg loom and --cfg shuttle are exclusive");

#[cfg(loom)]
pub use loom::{
    sync::{atomic, Mutex},
    thread,
};

#[cfg(shuttle)]
pub use shuttle::{
    sync::{atomic, Mutex},
    thread,
};

#[cfg(not(any(loom, shuttle)))]
pub use std::{
    sync::{atomic, Mutex},
    thread,
};

/// `lock` returns the guard. A poisoned mutex yields the guard.
#[cfg(not(any(loom, shuttle)))]
pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

// The package suite does not pass `--cfg loom`, so this body is not in that binary.
#[cfg(loom)]
#[cfg_attr(feature = "mutants", mutants::skip)]
pub fn lock<T>(mutex: &Mutex<T>) -> loom::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

// The package suite does not pass `--cfg shuttle`, so this body is not in that binary.
#[cfg(shuttle)]
#[cfg_attr(feature = "mutants", mutants::skip)]
pub fn lock<T>(mutex: &Mutex<T>) -> shuttle::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
