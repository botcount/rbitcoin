//! Listeners must leave file descriptors for the store and RPC.
//! This binary is alone so `setrlimit` does not trip the rest of the suite.

use rbitcoin_consensus::{ChainParams, Milestone};
#[cfg(unix)]
use rbitcoin_net::listen::listener_fd_reserve;
#[cfg(unix)]
use rbitcoin_net::peers::MAX_INCOMPLETE_INBOUND_PER_ADDR;
use rbitcoin_net::service::P2PNode;
use rbitcoin_query::Query;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `setrlimit` is process-wide. One test holds this for its whole body.
struct LimitGuard;

impl Drop for LimitGuard {
    fn drop(&mut self) {
        FD_LIMIT_BUSY.store(false, Ordering::SeqCst);
    }
}

static FD_LIMIT_BUSY: AtomicBool = AtomicBool::new(false);

fn hold_fd_limit() -> LimitGuard {
    while FD_LIMIT_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    LimitGuard
}

#[cfg(unix)]
fn nofile() -> (u64, u64) {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a local the call writes.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    assert_eq!(rc, 0);
    (rlim.rlim_cur, rlim.rlim_max)
}

#[cfg(unix)]
fn set_nofile_soft(soft: u64, hard: u64) {
    let rlim = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: `rlim` outlives the call.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
}

#[cfg(unix)]
fn open_fds() -> u64 {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let n = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("fd count failed: {e}"))
        .count() as u64;
    n.saturating_sub(1)
}

struct Restore {
    soft: u64,
    hard: u64,
}

#[cfg(unix)]
impl Drop for Restore {
    fn drop(&mut self) {
        set_nofile_soft(self.soft, self.hard);
    }
}

/// Filling the P2P listener must stop while descriptors remain.
///
/// A plain test that takes the limit before it builds the runtime: building a
/// runtime opens descriptors, so under `#[tokio::test]` the other test could
/// already hold a full table by then and the build failed with EMFILE.
#[cfg(unix)]
#[test]
fn listener_cap_fd_reserve_leaves_room() {
    let _limit = hold_fd_limit();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(listener_cap_fd_reserve_room());
}

#[cfg(unix)]
async fn listener_cap_fd_reserve_room() {
    let (soft, hard) = nofile();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-fd-reserve-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let node = P2PNode::start_with_agent(
        "127.0.0.1:0".parse().unwrap(),
        Query::open_or_create_tiny(&dir).unwrap(),
        ChainParams::regtest(),
        Milestone::NONE,
        "/rbitcoin:0.1.0(fd)/".into(),
        200,
        rbitcoin_net::REDIAL_INTERVAL_TEST,
    )
    .await
    .unwrap();
    // Every client connects from 127.0.0.1, the one loopback address macOS
    // configures. Leave room over the reserve for fewer connections than one
    // address may hold, so the reserve and not the address cap refuses.
    let open = open_fds();
    let mut tight = open + 48;
    while tight - open < listener_fd_reserve(tight) + 16 {
        tight += 1;
    }
    let reserve = listener_fd_reserve(tight);
    assert!(
        tight <= soft,
        "soft {soft} open {open} leaves no room to tighten"
    );
    set_nofile_soft(tight, hard);
    let _restore = Restore { soft, hard };

    let mut held = Vec::new();
    let mut refused = 0u32;
    for _ in 0..MAX_INCOMPLETE_INBOUND_PER_ADDR * 2 {
        let connect = tokio::net::TcpStream::connect(node.local_addr);
        let mut sock = match tokio::time::timeout(Duration::from_millis(400), connect).await {
            Ok(Ok(sock)) => sock,
            _ => {
                refused += 1;
                if refused >= 3 {
                    break;
                }
                continue;
            }
        };
        let mut buf = [0u8; 1];
        let open_sock = match tokio::time::timeout(
            Duration::from_millis(80),
            tokio::io::AsyncReadExt::read(&mut sock, &mut buf),
        )
        .await
        {
            Err(_) => true,
            Ok(Ok(0)) | Ok(Err(_)) => false,
            Ok(Ok(_)) => true,
        };
        if open_sock {
            held.push(sock);
        } else {
            refused += 1;
            if refused >= 3 && !held.is_empty() {
                break;
            }
        }
    }
    let left = tight.saturating_sub(open_fds());
    assert!(
        !held.is_empty() && held.len() < MAX_INCOMPLETE_INBOUND_PER_ADDR && left >= reserve,
        "listener filled the table: {left} fds left of reserve {reserve}, held {}, refused {refused}",
        held.len()
    );
    drop(held);
    node.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Counting fds needs a descriptor. A full table must refuse the socket.
#[cfg(unix)]
#[test]
fn listener_fd_room_refuses_when_the_count_cannot_open() {
    let _limit = hold_fd_limit();
    let (soft, hard) = nofile();
    let _restore = Restore { soft, hard };
    let open = open_fds();
    let tight = open.saturating_add(48).min(hard);
    assert!(
        tight > open + 8,
        "soft {soft} hard {hard} open {open} cannot fill the table"
    );
    set_nofile_soft(tight, hard);
    let mut held = Vec::new();
    let mut full = None;
    for _ in 0..(tight as usize + 8) {
        match std::fs::File::open("/dev/null") {
            Ok(f) => held.push(f),
            Err(e) => {
                full = Some(e);
                break;
            }
        }
    }
    let err = full.expect("the descriptor table never filled");
    assert!(
        matches!(err.raw_os_error(), Some(23 | 24)),
        "filling the table returned {err}"
    );
    assert!(
        !rbitcoin_net::listen::listener_fd_room(),
        "a full descriptor table still had listener room"
    );
    drop(held);
}

#[cfg(not(unix))]
#[test]
fn listener_cap_fd_reserve_leaves_room() {}
