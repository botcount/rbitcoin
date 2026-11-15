//! Drop-cleaning Tiny fixtures for tests. Not an operator API.
//!
//! Production constructors stay Mainnet (`StoreLayout::single`). Tests that
//! need an on-disk store use [`tiny_store`] / [`TempDir`] instead of rolling
//! `std::env::temp_dir()` + `create_dir_all`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Store;

#[doc(hidden)]
pub static SEQ: AtomicU64 = AtomicU64::new(0);

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn clock_tick() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Path no other call in this process shares, including two calls in one clock tick.
fn unique_path(label: &str, tick: u128) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let safe = sanitize_label(label);
    std::env::temp_dir().join(format!("rbitcoin-{safe}-{}-{tick}-{n}", std::process::id(),))
}

/// Unique path. The file or directory is not created.
pub fn fresh_path(label: &str) -> PathBuf {
    unique_path(label, clock_tick())
}

/// Unique directory removed on drop.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new() -> std::io::Result<Self> {
        Self::labeled("tmp")
    }

    pub fn labeled(label: &str) -> std::io::Result<Self> {
        Self::labeled_at(label, clock_tick())
    }

    /// `tick` is the clock sample. Two calls with the same sample still differ.
    #[doc(hidden)]
    pub fn labeled_at(label: &str, tick: u128) -> std::io::Result<Self> {
        let path = unique_path(label, tick);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Unique directory short enough to bind a Unix socket in. macOS
    /// `sun_path` holds 103 bytes and its default `TMPDIR` already takes 49,
    /// so the name has no label or timestamp. `create_dir` refusing a
    /// leftover from an earlier process with the same PID keeps it unique.
    pub fn for_socket() -> std::io::Result<Self> {
        loop {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rbitcoin-sock-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                created => return created.map(|()| Self { path }),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Recursive copy of a store directory. Creates `dst` first.
pub fn copy_store_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create store copy");
    for ent in std::fs::read_dir(src).expect("read store") {
        let ent = ent.expect("store entry");
        let to = dst.join(ent.file_name());
        let ty = ent.file_type().expect("file type");
        if ty.is_dir() {
            copy_store_tree(&ent.path(), &to);
        } else {
            std::fs::copy(ent.path(), &to).expect("copy store file");
        }
    }
}

/// Tiny-head store in a drop-cleaning directory.
pub fn tiny_store() -> (TempDir, Store) {
    tiny_store_labeled("store")
}

pub fn tiny_store_labeled(label: &str) -> (TempDir, Store) {
    let dir = TempDir::labeled(label).expect("create temp dir");
    let store = Store::create_tiny(dir.path()).expect("create tiny store");
    (dir, store)
}

/// Tiny store at `dir` whose `tx.head` segments have `2^bits` slots, so a
/// few hundred creates fill one and roll it.
pub fn create_tiny_with_head_bits(dir: &Path, bits: u32) -> Store {
    let head = crate::address_head::HeadLayout::new(bits).expect("head bits in range");
    Store::create_layout_with_head(crate::StoreLayout::tiny(dir.to_path_buf()), head)
        .expect("create tiny store")
}

/// Zero the slot pages of every unsealed `tx.head` segment under `root`,
/// keeping each file's length, its trailing layout footer, and the synced
/// `meta`. A power loss leaves this when `meta` was synced after an insert
/// and the page writes were not.
pub fn drop_unsynced_tx_head_pages(root: &Path) {
    fn find_head(dir: &Path) -> Option<PathBuf> {
        std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
            let p = e.path();
            if !p.is_dir() {
                return None;
            }
            if p.file_name()? == "tx.head" {
                Some(p)
            } else {
                find_head(&p)
            }
        })
    }
    let head = find_head(root).expect("tx.head under root");
    for e in std::fs::read_dir(&head).expect("read tx.head").flatten() {
        let p = e.path();
        if p.extension().is_some() || p.file_name().is_some_and(|n| n == "meta") {
            continue;
        }
        let len = std::fs::metadata(&p).expect("segment len").len();
        let slots = len.saturating_sub(crate::file::TRAILING_FOOTER_LEN as u64);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .expect("open segment");
        std::io::Write::write_all(&mut f, &vec![0u8; slots as usize]).expect("zero slot pages");
    }
}

/// Checkpoint an explicit height, running `between` after `sync_data`.
#[cfg(debug_assertions)]
pub fn checkpoint_spend_through_between(
    store: &Store,
    height: u32,
    between: impl FnOnce(&Store),
) -> Result<(), crate::StoreError> {
    store.checkpoint_spend_through_between(height, between)
}

/// Disconnect-generation sample, then snapshot height, with a hook between
/// them and a hook during `sync_data`.
#[cfg(debug_assertions)]
pub fn checkpoint_observed_spend_gap(
    store: &Store,
    after_first_read: impl FnOnce(&Store),
    during_sync: impl FnOnce(&Store),
) -> Result<(), crate::StoreError> {
    store.checkpoint_observed_spend_gap(after_first_read, during_sync)
}
