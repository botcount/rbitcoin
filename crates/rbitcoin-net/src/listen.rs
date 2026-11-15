//! TCP listen sockets for every server (P2P, RPC, Electrum, Esplora, health, SV2).

use std::io;
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpSocket};

/// `listen(2)` backlog: what `tokio::net::TcpListener::bind` passed.
const BACKLOG: u32 = 128;

/// Bind and listen on `addr`. Must run inside a tokio runtime.
pub fn bind_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
    bind_tcp_step(addr).map_err(|(_, e)| e)
}

/// The [`bind_tcp`] call that failed. Core names a failed `bind` apart from
/// the socket, option, and `listen` calls around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenStep {
    Socket,
    Bind,
    Listen,
}

/// [`bind_tcp`], with the call that failed.
pub fn bind_tcp_step(addr: SocketAddr) -> Result<TcpListener, (ListenStep, io::Error)> {
    let at = |step| move |e| (step, e);
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .map_err(at(ListenStep::Socket))?;
    // Windows SO_REUSEADDR lets another process bind the port and take its
    // connections; claim it outright (Core #36169, #36305).
    #[cfg(windows)]
    set_exclusive_addr_use(&socket).map_err(at(ListenStep::Socket))?;
    // Rebind a port still in TIME_WAIT after a restart.
    #[cfg(not(windows))]
    socket.set_reuseaddr(true).map_err(at(ListenStep::Socket))?;
    socket.bind(addr).map_err(at(ListenStep::Bind))?;
    socket.listen(BACKLOG).map_err(at(ListenStep::Listen))
}

#[cfg(windows)]
fn set_exclusive_addr_use(socket: &TcpSocket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, SOCKET_ERROR, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
    };

    let on: i32 = 1;
    // SAFETY: `socket` owns a live SOCKET for this call, and `optval` points
    // at an `i32` that outlives it, with `optlen` its size.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        // SAFETY: reads the calling thread's WinSock error; no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

/// Descriptors kept back from listeners so the store and RPC can still open
/// files. Above 1280 this is 1024. At or below that it is one eighth of the
/// soft limit, and at least 32, so a 1024 limit does not refuse every accept.
#[cfg(unix)]
pub fn listener_fd_reserve(soft: u64) -> u64 {
    if soft > 1024 + 256 {
        1024
    } else {
        (soft / 8).max(32)
    }
}

/// `true` when one more accepted socket still leaves the listener reserve.
///
/// The new fd is already in the process count. Counting fds needs a
/// descriptor, so a failed count refuses the socket. A failed `getrlimit`,
/// an unlimited soft limit, and every non-unix target fail open.
pub fn listener_fd_room() -> bool {
    #[cfg(unix)]
    {
        fd_room_unix()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn fd_room_unix() -> bool {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a local the call writes.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } != 0 {
        return true;
    }
    if rlim.rlim_cur == libc::RLIM_INFINITY {
        return true;
    }
    let Some(open) = open_fd_count() else {
        return false;
    };
    let soft = rlim.rlim_cur;
    soft.saturating_sub(open) >= listener_fd_reserve(soft)
}

#[cfg(unix)]
fn open_fd_count() -> Option<u64> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let n = std::fs::read_dir(dir).ok()?.count() as u64;
    Some(n.saturating_sub(1))
}
