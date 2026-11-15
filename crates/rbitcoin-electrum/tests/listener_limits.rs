//! One accept error must not kill the Electrum listener.
//! Own binary so a tight `RLIMIT_NOFILE` stays off the suite.

use rbitcoin_consensus::ChainParams;
use rbitcoin_electrum::{run_electrum, ElectrumConfig};
use rbitcoin_query::Query;
use std::net::SocketAddr;
use std::time::Duration;

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

/// Open descriptors, or `None` when the table is too full to count them.
#[cfg(unix)]
fn open_fds() -> Option<u64> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let n = std::fs::read_dir(dir).ok()?.count() as u64;
    Some(n.saturating_sub(1))
}

#[cfg(unix)]
fn fill_nulls(stop_free: u64, soft: u64) -> Vec<std::fs::File> {
    let mut files = Vec::new();
    while let Some(open) = open_fds() {
        if soft.saturating_sub(open) <= stop_free {
            break;
        }
        match std::fs::File::open("/dev/null") {
            Ok(f) => files.push(f),
            Err(_) => break,
        }
    }
    files
}

/// A full descriptor table makes `accept` fail. The listener logs that and
/// keeps serving once a descriptor is free again.
///
/// `current_thread` is required: another worker would accept the queued
/// connection the moment a descriptor closed, and the old loop would never
/// see `EMFILE`.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn listener_cap_electrum_retries_accept_error() {
    let (soft, hard) = nofile();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rbitcoin-electrum-accept-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let q = Query::open_or_create_tiny(&dir).unwrap();
    let (tip_tx, _) = tokio::sync::broadcast::channel(4);
    let mut cfg =
        ElectrumConfig::for_params("127.0.0.1:0".parse().unwrap(), &ChainParams::regtest());
    cfg.limits.max_connections = 64;
    let handle = run_electrum(
        cfg,
        std::sync::Arc::new(q),
        ChainParams::regtest(),
        tip_tx,
        None,
    )
    .await
    .expect("electrum");
    let addr = handle.local_addr;
    // Park `accept` before the table is full.
    tokio::task::yield_now().await;

    let open = open_fds().expect("fd count");
    let tight = open.saturating_add(48).min(soft);
    assert!(tight > open + 20, "soft {soft} open {open}");
    // Spawn before the squeeze so the child keeps a soft limit that can connect.
    let queued = queue_connections(addr);
    set_nofile_soft(tight, hard);
    struct Restore {
        soft: u64,
        hard: u64,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            set_nofile_soft(self.soft, self.hard);
        }
    }
    let _restore = Restore { soft, hard };

    // Freeze the runtime so `accept` cannot run until the queue is full
    // and every spare descriptor is taken.
    let filler = fill_nulls(0, tight);
    std::thread::sleep(Duration::from_millis(500));
    let filler_more = fill_nulls(0, tight);
    let queued_out = queued.wait_with_output().expect("connector");
    // Reaping the child closes its pipes. Take those descriptors too.
    let filler_last = fill_nulls(0, tight);
    let queued_n: u32 = String::from_utf8_lossy(&queued_out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(
        queued_n > 0,
        "no connection reached the listen queue: {}",
        String::from_utf8_lossy(&queued_out.stderr)
    );

    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let accept_ended = handle.tasks[0].is_finished();
    drop(filler);
    drop(filler_more);
    drop(filler_last);
    assert!(
        !accept_ended,
        "electrum listener exited on the first accept error"
    );

    let again =
        tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await;
    assert!(
        matches!(again, Ok(Ok(_))),
        "electrum listener stayed down after an accept error: {again:?}"
    );
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn queue_connections(addr: SocketAddr) -> std::process::Child {
    // A thread shares this process's descriptor table and could not connect
    // once it is full.
    let script = "import socket,sys,time\ntime.sleep(0.25)\nn=0\nfor _ in range(8):\n s=socket.socket()\n s.settimeout(1)\n try:\n  s.connect((sys.argv[1], int(sys.argv[2])))\n  n+=1\n except OSError:\n  pass\nprint(n)\ntime.sleep(0.5)\n";
    std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(addr.ip().to_string())
        .arg(addr.port().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3")
}

#[cfg(not(unix))]
#[test]
fn listener_cap_electrum_retries_accept_error() {}
