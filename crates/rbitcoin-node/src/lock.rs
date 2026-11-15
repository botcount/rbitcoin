//! Exclusive datadir flock (Core `LockDirectory`).

use crate::config::NodeConfig;
use crate::error::NodeError;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Held exclusive `.lock` files. Dropping releases the flock.
///
/// A duplicated fd shares the flock. Closing the original would leave the
/// datadir busy until that dup closed too. `LOCK_UN` releases it now.
#[derive(Debug)]
pub struct DirLocks {
    pub _files: Vec<File>,
}

impl Drop for DirLocks {
    fn drop(&mut self) {
        for file in &self._files {
            unlock(file);
        }
    }
}

/// Core-shaped lock refusal (operator name is rbitcoin; harness maps CLIENT_NAME).
pub fn lock_busy_msg(dir: &Path) -> String {
    format!(
        "Cannot obtain a lock on directory {}. rbitcoin is probably already running.",
        dir.display()
    )
}

/// Exclusive-lock the process datadir.
pub fn lock_node_dirs(config: &NodeConfig) -> Result<DirLocks, NodeError> {
    Ok(DirLocks {
        _files: vec![lock_dir(config.datadir.path())?],
    })
}

#[doc(hidden)]
pub fn lock_dir(dir: &Path) -> Result<File, NodeError> {
    let path = dir.join(".lock");
    match open_lockfile(&path) {
        Ok(file) => match try_exclusive(&file) {
            Ok(()) => Ok(file),
            Err(LockBusy) => Err(NodeError::Locked(dir.to_path_buf())),
        },
        Err(e) if is_lock_busy_io(&e) => Err(NodeError::Locked(dir.to_path_buf())),
        Err(source) => Err(NodeError::Datadir {
            path: path.clone(),
            source,
        }),
    }
}

#[doc(hidden)]
pub fn is_lock_busy_io(e: &io::Error) -> bool {
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION
        e.raw_os_error() == Some(32)
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        false
    }
}

#[doc(hidden)]
pub struct LockBusy;

#[doc(hidden)]
pub fn open_lockfile(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    no_follow(opts.create(true).read(true).write(true));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    opts.open(path)
}

/// A symlinked datadir file fails closed: the open neither reads nor creates
/// the link's target.
#[doc(hidden)]
pub fn no_follow(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts
}

#[doc(hidden)]
pub fn try_exclusive(file: &File) -> Result<(), LockBusy> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // EINTR is a caught signal, not another node. A short EAGAIN retry
        // covers a lock that was released in the same millisecond. A live
        // node still loses.
        for attempt in 0..8 {
            // SAFETY: `file` is open. `LOCK_NB` returns instead of blocking.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let again = err.raw_os_error() == Some(libc::EAGAIN)
                || err.raw_os_error() == Some(libc::EWOULDBLOCK);
            if !again || attempt + 1 == 8 {
                return Err(LockBusy);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(LockBusy)
    }
    #[cfg(windows)]
    {
        let _ = file;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(())
    }
}

fn unlock(file: &File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        for _ in 0..64 {
            // SAFETY: `file` is open. `LOCK_UN` releases this description's flock.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}
