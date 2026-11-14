//! A grow on a full disk fails at the allocation, not later on a page write.

#![cfg(target_os = "linux")]

use rbitcoin_primitives::TableKind;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::file::*;
use std::ffi::CString;
use std::path::{Path, PathBuf};

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

#[test]
fn grow_on_a_full_disk_errors_without_a_sparse_fallback() {
    let Some(fs) = SmallFs::mount() else {
        eprintln!("skipped: cannot mount a tmpfs (needs root)");
        return;
    };
    let f = TableFile::create(fs.path().join("t"), TableKind::TxOut).unwrap();
    let before = std::fs::metadata(fs.path().join("t")).unwrap().len();
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
    // Space that does fit still grows.
    f.ensure_capacity(before + 4096).unwrap();
}
