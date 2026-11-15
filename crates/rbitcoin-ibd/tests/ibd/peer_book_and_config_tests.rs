use rbitcoin_ibd::ibd::PeerBookSession;
use rbitcoin_net::seeds::AddrMan;
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::Mutex;

fn sa(o: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, o)), 18444)
}

#[test]
fn peer_book_session_injects_seeds_and_flushes_on_drop() {
    let shared = Arc::new(Mutex::new(AddrMan::new()));
    {
        let mut sess = PeerBookSession::new(
            Some(Arc::clone(&shared)),
            &[
                rbitcoin_net::NetAddr::Ip(sa(1)),
                rbitcoin_net::NetAddr::Ip(sa(2)),
            ],
        );
        assert!(sess
            .book()
            .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(1)))
            .is_some());
        assert!(sess
            .book()
            .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(2)))
            .is_some());
        sess.book_mut().add(sa(3));
        assert!(sess
            .book()
            .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(3)))
            .is_some());
        // Shared not yet flushed until drop/flush.
        assert!(shared
            .lock()
            .unwrap()
            .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(3)))
            .is_none());
        sess.flush();
        assert!(shared
            .lock()
            .unwrap()
            .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(3)))
            .is_some());
    }
    // Drop flushes again (idempotent).
    assert!(shared
        .lock()
        .unwrap()
        .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(1)))
        .is_some());

    // No shared book — seeds only, flush is a no-op.
    let sess2 = PeerBookSession::new(None, &[rbitcoin_net::NetAddr::Ip(sa(9))]);
    assert!(sess2
        .book()
        .entry_of(&rbitcoin_net::NetAddr::from_socket(sa(9)))
        .is_some());
    sess2.flush();
}

/// Rows another writer adds to the shared book during catch-up (the
/// deferred DNS seeds) reach this session and survive its flush. A row
/// the session dropped is not pulled back in.
#[test]
fn peer_book_session_pulls_late_shared_rows() {
    let net = |o| rbitcoin_net::NetAddr::from_socket(sa(o));
    let shared = Arc::new(Mutex::new(AddrMan::new()));
    shared.lock().unwrap().add(sa(1));
    shared.lock().unwrap().add(sa(2));
    let mut sess = PeerBookSession::new(Some(Arc::clone(&shared)), &[]);
    sess.book_mut().evict_one(net(2));
    shared.lock().unwrap().add(sa(3));
    assert!(sess.book().entry_of(&net(3)).is_none());
    sess.pull_shared();
    assert!(sess.book().entry_of(&net(3)).is_some(), "late row pulled");
    assert!(
        sess.book().entry_of(&net(2)).is_none(),
        "evicted row stays out"
    );
    shared.lock().unwrap().add(sa(4));
    drop(sess);
    let book = shared.lock().unwrap();
    for o in [1, 3, 4] {
        assert!(book.entry_of(&net(o)).is_some(), "row {o} after flush");
    }
    assert!(book.entry_of(&net(2)).is_none());
}
