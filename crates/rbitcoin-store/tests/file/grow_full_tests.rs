//! A grow on a full disk fails at the allocation, not later on a page write.
//!
//! The injected tests need the debug-only `fallocate` hooks and run under a
//! plain `cargo test`. The tmpfs mount test needs root and is `#[ignore]`.

#![cfg(target_os = "linux")]

use rbitcoin_primitives::TableKind;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::*;
use rbitcoin_store::testutil::TempDir;
use std::ffi::CString;
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;

fn disk_len(f: &TableFile) -> u64 {
    std::fs::metadata(f.path()).unwrap().len()
}

fn expect_enospc(r: Result<(), StoreError>) {
    match r.expect_err("the grow must fail") {
        StoreError::Io { source, .. } => {
            assert_eq!(source.raw_os_error(), Some(libc::ENOSPC), "{source}")
        }
        other => panic!("expected an ENOSPC io error, got {other:?}"),
    }
}

/// A fresh table grown to 1 MiB, so the next slab (double) exceeds `need`.
#[cfg(debug_assertions)]
fn table_at_1mib(dir: &TempDir) -> TableFile {
    let f = TableFile::create(dir.path.join("t"), TableKind::TxOut).unwrap();
    f.ensure_capacity_grow(MIB).unwrap();
    assert_eq!(f.file_cap.load(std::sync::atomic::Ordering::Acquire), MIB);
    f
}

/// Disarms the injected `fallocate` faults on drop, also when a test panics.
#[cfg(debug_assertions)]
struct Armed(rbitcoin_store::io_handle::IoHandle);

#[cfg(debug_assertions)]
impl Drop for Armed {
    fn drop(&mut self) {
        rbitcoin_store::io_handle::disarm_pwrite(self.0);
    }
}

/// ENOSPC from `fallocate` fails the grow and moves neither the on-disk
/// length nor `file_cap`.
#[cfg(debug_assertions)]
#[test]
fn injected_enospc_leaves_length_and_cap_unchanged() {
    let dir = TempDir::new().unwrap();
    let f = table_at_1mib(&dir);
    let h = f.write_handle();
    rbitcoin_store::io_handle::arm_fallocate_limit(h, MIB, false);
    let _armed = Armed(h);
    let cap = f.file_cap.load(std::sync::atomic::Ordering::Acquire);
    let len = disk_len(&f);
    expect_enospc(f.ensure_capacity_grow(MIB + 4096));
    assert_eq!(f.file_cap.load(std::sync::atomic::Ordering::Acquire), cap);
    assert_eq!(disk_len(&f), len, "no sparse extend after the failure");
}

/// A `fallocate` that grows the file and then runs out of room is undone.
#[cfg(debug_assertions)]
#[test]
fn partial_fallocate_is_rolled_back_when_need_does_not_fit() {
    let dir = TempDir::new().unwrap();
    let f = table_at_1mib(&dir);
    let h = f.write_handle();
    // Room for 8 KiB more, then ENOSPC; the failed call keeps those blocks.
    rbitcoin_store::io_handle::arm_fallocate_limit(h, MIB + 8192, true);
    let _armed = Armed(h);
    let len = disk_len(&f);
    expect_enospc(f.ensure_capacity_grow(MIB + 16384));
    assert_eq!(disk_len(&f), len, "the partial extend must be truncated");
    assert_eq!(
        f.file_cap.load(std::sync::atomic::Ordering::Acquire),
        MIB,
        "file_cap must not move"
    );
}

/// The slab does not fit but `need` does: the grow succeeds at `need` and
/// `file_cap` matches the length the file really has.
#[cfg(debug_assertions)]
#[test]
fn slab_enospc_retries_at_need() {
    let dir = TempDir::new().unwrap();
    let f = table_at_1mib(&dir);
    let h = f.write_handle();
    rbitcoin_store::io_handle::arm_fallocate_limit(h, MIB + 8192, true);
    let _armed = Armed(h);
    f.ensure_capacity_grow(MIB + 4096).unwrap();
    let cap = f.file_cap.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(cap, MIB + 4096, "grew to need, not to the 2 MiB slab");
    assert_eq!(disk_len(&f), cap);
}

/// A signal-interrupted `fallocate` is retried, not turned into a sparse
/// `set_len`.
#[cfg(debug_assertions)]
#[test]
fn interrupted_fallocate_is_retried_not_sparse() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDir::new().unwrap();
    let f = table_at_1mib(&dir);
    let h = f.write_handle();
    rbitcoin_store::io_handle::arm_fallocate_eintr(h, 3);
    let _armed = Armed(h);
    f.ensure_capacity_grow(MIB + 4096).unwrap();
    let cap = f.file_cap.load(std::sync::atomic::Ordering::Acquire);
    assert!(cap >= MIB + 4096);
    let md = std::fs::metadata(f.path()).unwrap();
    assert_eq!(md.len(), cap);
    assert!(
        md.blocks() * 512 >= cap,
        "blocks are reserved, the file is not sparse: {} < {cap}",
        md.blocks() * 512
    );
}

/// Only an "unsupported" answer may fall back to `set_len`.
#[test]
fn only_unsupported_errors_fall_back_to_set_len() {
    for errno in [libc::EOPNOTSUPP, libc::ENOSYS, libc::EINVAL] {
        assert!(fallocate_unsupported(&std::io::Error::from_raw_os_error(
            errno
        )));
    }
    assert!(fallocate_unsupported(&std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "fallocate unavailable"
    )));
    for errno in [
        libc::ENOSPC,
        libc::EDQUOT,
        libc::EINTR,
        libc::EIO,
        libc::EFBIG,
    ] {
        assert!(!fallocate_unsupported(&std::io::Error::from_raw_os_error(
            errno
        )));
    }
}

/// A 1 MiB tmpfs mounted on a fresh directory, unmounted on drop.
struct SmallFs(PathBuf);

impl SmallFs {
    /// `None` when this process may not mount (not root, or no tmpfs).
    fn mount() -> Option<Self> {
        let dir = std::env::temp_dir().join(format!("rbitcoin-grow-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let src = CString::new("tmpfs").unwrap();
        let tgt = CString::new(dir.to_str()?).unwrap();
        let fstype = CString::new("tmpfs").unwrap();
        let opts = CString::new("size=1m").unwrap();
        // SAFETY: every pointer is a live NUL-terminated string for the call.
        let rc = unsafe {
            libc::mount(
                src.as_ptr(),
                tgt.as_ptr(),
                fstype.as_ptr(),
                0,
                opts.as_ptr().cast(),
            )
        };
        if rc != 0 {
            let _ = std::fs::remove_dir(&dir);
            return None;
        }
        Some(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SmallFs {
    fn drop(&mut self) {
        if let Ok(tgt) = CString::new(self.0.to_str().unwrap_or_default()) {
            // SAFETY: `tgt` is a live NUL-terminated string for the call.
            unsafe { libc::umount2(tgt.as_ptr(), libc::MNT_DETACH) };
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// Needs root to mount a tmpfs, so CI does not run it. Run it with
/// `cargo test -p rbitcoin-store --test suite -- --ignored grow_on_a_full_disk` as root.
#[test]
#[ignore = "mounts a tmpfs: needs root"]
fn grow_on_a_full_disk_errors_without_a_sparse_fallback() {
    let fs = SmallFs::mount().expect("cannot mount a tmpfs: run as root");
    let f = TableFile::create(fs.path().join("t"), TableKind::TxOut).unwrap();
    let before = std::fs::metadata(fs.path().join("t")).unwrap().len();
    let cap = f.file_cap.load(std::sync::atomic::Ordering::Acquire);
    let err = f
        .ensure_capacity(64 * 1024 * 1024)
        .expect_err("64 MiB cannot fit in a 1 MiB filesystem");
    match err {
        StoreError::Io { source, .. } => {
            assert_eq!(source.raw_os_error(), Some(libc::ENOSPC), "{source}")
        }
        other => panic!("expected an ENOSPC io error, got {other:?}"),
    }
    // A sparse `set_len` would have reported success and left a 64 MiB file.
    let after = std::fs::metadata(fs.path().join("t")).unwrap().len();
    assert_eq!(before, after, "failed grow must not extend the file");
    assert_eq!(f.file_cap.load(std::sync::atomic::Ordering::Acquire), cap);
    // Space that does fit still grows.
    f.ensure_capacity(before + 4096).unwrap();
}
