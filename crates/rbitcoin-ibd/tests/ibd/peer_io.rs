use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_blockdata::Inventory;
use bitcoin::p2p::{Magic, ServiceFlags};
use bitcoin::BlockHash;
use rbitcoin_ibd::ibd::peer_io::*;
use rbitcoin_ibd::ibd::rate::PeerRate;
use rbitcoin_net::codec::FramedMessage;
use rbitcoin_net::error::NetError;
use rbitcoin_net::peer_dos::{PeerRateLimiter, RATE_LIMIT_BAN_SCORE};
use rbitcoin_net::peers::PongSlot;
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Semaphore};

fn dummy_slot(id: usize) -> PeerSlot {
    let (cmd_tx, _rx) = mpsc::unbounded_channel();
    let task = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .spawn(async {});
    PeerSlot {
        id,
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 18444),
        net: rbitcoin_net::NetAddr::from_socket(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            18444,
        )),
        cmd_tx,
        in_flight: Default::default(),
        requested: Arc::new(Mutex::new(Default::default())),
        headers_asks: Default::default(),
        solicited_bytes: Arc::new(AtomicU64::new(0)),
        solicited_ms: Arc::new(AtomicU64::new(0)),
        open_block_bytes: Arc::new(AtomicU64::new(0)),
        any_rx_ms: Arc::new(AtomicU64::new(0)),
        getdata_wire: GetdataWire::new(),
        peer_height: 100,
        connected_ms: 1,
        first_data_ms: 0,
        bytes_rx_total: Arc::new(AtomicU64::new(0)),
        rate: Default::default(),
        chain: Default::default(),
        conn_type: rbitcoin_net::peers::PeerConnType::OutboundFullRelay,
        block_download_paused_until: None,
        late_answers: Default::default(),
        alive: true,
        task,
    }
}

/// `sample_peer_rates` copies the on-wire getdata time into the rate.
#[test]
fn pio_pin_write_time_enters_the_rate() {
    let mut s = dummy_slot(4);
    s.getdata_wire.wrote(1, 55);
    s.rate.work_started_ms = 0;
    sample_peer_rates(std::slice::from_mut(&mut s), 55);
    assert_eq!(s.rate.work_started_ms, 55);
}

/// Cancelling the parent aborts both halves. A detached handle would leave
/// the oneshot open.
#[test]
fn pio_pin_dropping_io_tasks_aborts_both_halves() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (read_tx, mut read_rx) = tokio::sync::oneshot::channel::<()>();
        let (write_tx, mut write_rx) = tokio::sync::oneshot::channel::<()>();
        let tasks = PeerIoTasks {
            reader: tokio::spawn(async move {
                let _hold = read_tx;
                std::future::pending::<()>().await;
            }),
            writer: Some(tokio::spawn(async move {
                let _hold = write_tx;
                std::future::pending::<()>().await;
            })),
        };
        drop(tasks);
        tokio::task::yield_now().await;
        assert!(matches!(
            read_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            write_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    });
}

/// Drop tells the peer task to stop.
#[test]
fn pio_pin_dropping_a_slot_shuts_the_peer() {
    let (cmd_tx, mut rx) = mpsc::unbounded_channel();
    let mut s = dummy_slot(1);
    s.cmd_tx = cmd_tx;
    drop(s);
    assert!(matches!(rx.try_recv(), Ok(PeerCmd::Shutdown)));
}

/// A decode that is still running stays beside the one just pushed.
#[test]
fn pio_pin_a_live_decode_is_kept() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut light = LightDecodes { tasks: Vec::new() };
        light.push(tokio::spawn(std::future::pending::<()>()));
        light.push(tokio::spawn(std::future::pending::<()>()));
        assert_eq!(light.tasks.len(), 2);
    });
}

/// Dropping the reader aborts a decode that is still waiting.
#[test]
fn pio_pin_dropping_decodes_aborts_them() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let light = LightDecodes {
            tasks: vec![tokio::spawn(async move {
                let _hold = tx;
                std::future::pending::<()>().await;
            })],
        };
        drop(light);
        tokio::task::yield_now().await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    });
}

/// An IO error that is not EOF or reset is not labeled as one.
#[test]
fn pio_pin_a_refused_connection_is_not_an_eof() {
    let (mut ctx, _, mut body_rx, _) = test_ctx(1);
    let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "no");
    assert!(ctx.dispatch_err(NetError::Io(err)));
    match body_rx.try_recv() {
        Ok(PeerEvent::Dead { reason, .. }) => {
            assert!(!reason.starts_with("eof:"), "{reason}");
        }
        other => {
            let _ = other;
            panic!("expected a dead event");
        }
    }
}

/// A repeated progress stamp does not move the byte cursor. A later stamp does.
#[test]
fn unchanged_progress_keeps_the_byte_mark() {
    let mut s = dummy_slot(3);
    s.rate.progress_ms = 40;
    s.rate.bytes_at_progress = 7;
    s.solicited_ms.store(40, Ordering::Relaxed);
    s.solicited_bytes.store(90, Ordering::Relaxed);
    sample_peer_rates(std::slice::from_mut(&mut s), 40);
    assert_eq!(s.rate.progress_ms, 40);
    assert_eq!(s.rate.bytes_at_progress, 7);

    s.solicited_ms.store(41, Ordering::Relaxed);
    sample_peer_rates(std::slice::from_mut(&mut s), 41);
    assert_eq!(s.rate.progress_ms, 41);
    assert_eq!(s.rate.bytes_at_progress, 90);
}

#[test]
fn usable_dial_and_services_filters() {
    assert!(!services_useful_for_ibd(ServiceFlags::NETWORK));
    assert!(!services_useful_for_ibd(ServiceFlags::NETWORK_LIMITED));
    assert!(!services_useful_for_ibd(ServiceFlags::P2P_V2));
    assert!(!services_useful_for_ibd(ServiceFlags::NONE));
    assert!(services_useful_for_ibd(
        ServiceFlags::NETWORK | ServiceFlags::P2P_V2
    ));
    assert!(services_useful_for_ibd(
        ServiceFlags::NETWORK_LIMITED | ServiceFlags::P2P_V2
    ));

    let good = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    assert!(usable_dial_addr(&good));
    let zero_port = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 0);
    assert!(!usable_dial_addr(&zero_port));
    let unspec = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8333);
    assert!(!usable_dial_addr(&unspec));
    let bcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 8333);
    assert!(!usable_dial_addr(&bcast));
    let multi = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)), 8333);
    assert!(!usable_dial_addr(&multi));
    let v6_good = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8333);
    assert!(usable_dial_addr(&v6_good));
    let v6_unspec = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 8333);
    assert!(!usable_dial_addr(&v6_unspec));
}

#[test]
fn stream_bytes_sample_and_first_data() {
    let mut s = dummy_slot(7);
    note_stream_bytes(&s.bytes_rx_total, 0);
    assert_eq!(s.bytes_rx_total.load(Ordering::Relaxed), 0);
    note_stream_bytes(&s.bytes_rx_total, 100);
    note_stream_bytes(&s.bytes_rx_total, 50);
    assert_eq!(s.bytes_rx_total.load(Ordering::Relaxed), 150);

    s.in_flight.insert(BlockHash::from_byte_array([1u8; 32]));
    sample_peer_rates(std::slice::from_mut(&mut s), 0);
    sample_peer_rates(std::slice::from_mut(&mut s), 5_000);
    assert!(s.rate.bps().is_some());

    wait_past_mono_sentinel();
    note_block_rx(std::slice::from_mut(&mut s), 7, 0);
    assert_eq!(s.first_data_ms, 0);
    note_block_progress(std::slice::from_mut(&mut s), 7);
    note_block_rx(std::slice::from_mut(&mut s), 7, 1000);
    assert!(s.first_data_ms > 0);
    note_block_progress(std::slice::from_mut(&mut s), 99);
    note_block_rx(std::slice::from_mut(&mut s), 99, 1);
    assert!(ibd_mono_ms() > 0);
}

#[test]
fn track_retain_judges_each_hash_once() {
    let mut slot = dummy_slot(3);
    let keep_hash = BlockHash::from_byte_array([1u8; 32]);
    let drop_hash = BlockHash::from_byte_array([2u8; 32]);
    slot.track_insert(keep_hash);
    slot.track_insert(drop_hash);
    let mut calls = 0usize;
    slot.track_retain(|hash| {
        calls += 1;
        *hash != drop_hash
    });
    assert_eq!(calls, 2, "one judgment per in-flight hash");
    assert!(slot.in_flight.contains(&keep_hash));
    assert!(!slot.in_flight.contains(&drop_hash));
    assert!(
        slot.late_answers.contains_key(&drop_hash),
        "a withdrawn getdata is still owed an answer"
    );
    let requested = slot.requested.lock().unwrap().clone();
    assert_eq!(
        requested,
        rbitcoin_ibd::ibd::state::BlockHashSet::from_iter([keep_hash, drop_hash])
    );
}

#[test]
fn unsolicited_block_does_not_refresh_progress() {
    let hash = BlockHash::from_byte_array([7u8; 32]);
    let requested = Mutex::new(Default::default());
    let bytes = AtomicU64::new(0);
    let ms = AtomicU64::new(0);
    assert!(
        !note_solicited_block(&requested, &bytes, &ms, &hash, 80),
        "an unsolicited block does not count"
    );
    assert_eq!(bytes.load(Ordering::Relaxed), 0);
    assert_eq!(ms.load(Ordering::Relaxed), 0);

    wait_past_mono_sentinel();
    requested.lock().unwrap().insert(hash);
    let len = rbitcoin_ibd::ibd::rate::PROGRESS_STEP as usize;
    assert!(
        note_solicited_block(&requested, &bytes, &ms, &hash, len),
        "a requested block records its bytes"
    );
    assert_eq!(bytes.load(Ordering::Relaxed), len as u64);
    let marked = ms.load(Ordering::Relaxed);
    assert!(marked > 0, "a requested block stamps the read time");

    let mut quiet = PeerRate::default();
    quiet.sample(0, 0, true);
    quiet.sample(10_000, 0, true);
    assert_eq!(
        quiet.progress_ms, 0,
        "zero solicited bytes over 10s leave the stall clock"
    );
    let mut moved = PeerRate::default();
    moved.sample(0, 0, true);
    moved.sample(1_000, bytes.load(Ordering::Relaxed), true);
    assert_eq!(moved.progress_ms, 1_000);

    let mut slot = dummy_slot(4);
    slot.in_flight.insert(hash);
    assert_eq!(slot.solicited_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(slot.solicited_ms.load(Ordering::Relaxed), 0);
    sample_peer_rates(std::slice::from_mut(&mut slot), 0);
    note_stream_bytes(&slot.bytes_rx_total, 8_000_000);
    sample_peer_rates(std::slice::from_mut(&mut slot), 10_000);
    assert_eq!(
        slot.rate.progress_ms, 0,
        "unsolicited stream bytes do not refresh progress"
    );

    slot.solicited_ms.store(marked, Ordering::Relaxed);
    sample_peer_rates(std::slice::from_mut(&mut slot), 0);
    assert_eq!(slot.rate.progress_ms, marked);
}

/// The progress baseline is the solicited total. Socket bytes that are not
/// a requested block stay off that stamp.
#[test]
fn block_progress_stamp_records_solicited_bytes() {
    let step = rbitcoin_ibd::ibd::rate::PROGRESS_STEP;
    let mut slot = dummy_slot(8);
    slot.in_flight.insert(BlockHash::from_byte_array([8u8; 32]));
    slot.bytes_rx_total.store(1, Ordering::Relaxed);
    slot.solicited_bytes.store(step, Ordering::Relaxed);
    slot.solicited_ms.store(5_000, Ordering::Relaxed);
    sample_peer_rates(std::slice::from_mut(&mut slot), 5_000);
    assert_eq!(slot.rate.progress_ms, 5_000);
    assert_eq!(
        slot.rate.bytes_at_progress, step,
        "a moved solicited clock stamps solicited bytes, not the socket total"
    );

    slot.bytes_rx_total.store(2, Ordering::Relaxed);
    slot.solicited_bytes
        .store(step.saturating_mul(2), Ordering::Relaxed);
    note_block_progress(std::slice::from_mut(&mut slot), 8);
    assert_eq!(slot.rate.bytes_at_progress, step.saturating_mul(2));

    slot.bytes_rx_total.store(3, Ordering::Relaxed);
    slot.solicited_bytes
        .store(step.saturating_mul(3), Ordering::Relaxed);
    note_block_rx(std::slice::from_mut(&mut slot), 8, 1_000);
    assert_eq!(slot.rate.bytes_at_progress, step.saturating_mul(3));
}

#[test]
fn open_block_bytes_count_a_frame_past_a_headers_message() {
    let hash = BlockHash::from_byte_array([9u8; 32]);
    let requested = Mutex::new(rbitcoin_ibd::ibd::state::BlockHashSet::from_iter([hash]));
    let open = AtomicU64::new(0);
    let step = rbitcoin_ibd::ibd::rate::PROGRESS_STEP as usize;
    let headers = rbitcoin_net::codec::MAX_HEADERS_CONTENTS;
    note_open_block(&open, &requested, headers, step);
    assert_eq!(open.load(Ordering::Relaxed), 0, "a headers message is chat");
    note_open_block(&open, &requested, headers + 1, step);
    assert_eq!(
        open.load(Ordering::Relaxed),
        step as u64,
        "a longer frame while a block is owed is that frame"
    );
    requested.lock().unwrap().clear();
    note_open_block(&open, &requested, headers + 1, step);
    assert_eq!(
        open.load(Ordering::Relaxed),
        0,
        "a long frame with nothing owed is not a block"
    );
}

#[test]
fn socket_bytes_refresh_the_quiet_clock_only() {
    let mut slot = dummy_slot(4);
    let hash = BlockHash::from_byte_array([4u8; 32]);
    slot.in_flight.insert(hash);
    slot.rate.note_connected(0);
    wait_past_mono_sentinel();
    let mut mark = 0usize;
    note_read_progress(&slot.bytes_rx_total, &slot.any_rx_ms, &mut mark, 32);
    sample_peer_rates(std::slice::from_mut(&mut slot), 5_000);
    assert!(slot.rate.any_rx_ms > 0);
    assert_eq!(slot.rate.progress_ms, 0);
    assert_eq!(slot.solicited_ms.load(Ordering::Relaxed), 0);
}

#[test]
fn on_wire_window_owns_stall_and_quiet() {
    let mut idle = dummy_slot(1);
    idle.rate.note_rx(0);
    idle.rate.note_any_rx(0);
    assert!(!getdata_on_wire_stalled(&idle, 100_000, 30_000));
    assert!(!getdata_on_wire_quiet(&idle, 100_000, 30_000));

    let mut asked = dummy_slot(2);
    asked
        .in_flight
        .insert(BlockHash::from_byte_array([9u8; 32]));
    asked.getdata_wire.queue(1);
    asked.getdata_wire.wrote(1, 0);
    asked.rate.note_rx(0);
    asked.rate.note_any_rx(0);
    assert!(!getdata_on_wire_stalled(&asked, 30_000, 30_000));
    assert!(!getdata_on_wire_quiet(&asked, 30_000, 30_000));
    assert!(getdata_on_wire_stalled(&asked, 30_001, 30_000));
    assert!(getdata_on_wire_quiet(&asked, 30_001, 30_000));
}

#[test]
fn light_decode_permit_does_not_wait() {
    let sem = Arc::new(Semaphore::new(1));
    let first = try_light_decode_permit(&sem);
    assert!(first.is_some(), "one permit is available immediately");
    assert!(
        try_light_decode_permit(&sem).is_none(),
        "a second permit does not wait"
    );
    drop(first);
    assert!(try_light_decode_permit(&sem).is_some());
}

#[test]
fn solicited_replay_does_not_refresh_progress() {
    let (req, bytes, ms) = solicit_track();
    let hash = BlockHash::from_byte_array([9u8; 32]);
    req.lock().unwrap().insert(hash);
    assert!(note_solicited_block(&req, &bytes, &ms, &hash, 100));
    let stamped = ms.load(Ordering::Relaxed);
    assert_eq!(bytes.load(Ordering::Relaxed), 100);
    assert!(!req.lock().unwrap().contains(&hash));
    assert!(
        !note_solicited_block(&req, &bytes, &ms, &hash, 50),
        "a resend of a solicited block is not another request"
    );
    assert_eq!(bytes.load(Ordering::Relaxed), 100);
    assert_eq!(ms.load(Ordering::Relaxed), stamped);
}

fn framed(command: [u8; 12], payload: Vec<u8>) -> FramedMessage {
    FramedMessage {
        magic: Magic::from(bitcoin::Network::Regtest),
        command,
        payload,
    }
}

fn headers_frame(headers: &[Header]) -> FramedMessage {
    use bitcoin::consensus::Encodable;
    let mut payload = Vec::new();
    bitcoin::consensus::encode::VarInt(headers.len() as u64)
        .consensus_encode(&mut payload)
        .unwrap();
    for header in headers {
        header.consensus_encode(&mut payload).unwrap();
        payload.push(0);
    }
    framed(*b"headers\0\0\0\0\0", payload)
}

fn notfound_frame(hashes: &[BlockHash]) -> FramedMessage {
    use bitcoin::consensus::Encodable;
    let mut payload = Vec::new();
    bitcoin::consensus::encode::VarInt(hashes.len() as u64)
        .consensus_encode(&mut payload)
        .unwrap();
    for hash in hashes {
        Inventory::WitnessBlock(*hash)
            .consensus_encode(&mut payload)
            .unwrap();
    }
    framed(*b"notfound\0\0\0\0", payload)
}

/// `headers` and `notfound` are heavy decodes like any other session frame:
/// they take the message and byte window before the decode is spawned.
#[tokio::test]
async fn headers_and_notfound_charge_the_session_window() {
    use rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC;

    let (mut ctx, _out, _body, _ctrl) = test_ctx(1);
    ctx.rate = PeerRateLimiter::new(2, DEFAULT_MAX_BYTES_PER_SEC);
    assert!(!ctx.handle(Ok(headers_frame(&[one_header()]))));
    assert_eq!(ctx.rate.cur_msgs, 1, "a headers frame is one message");
    let hash = BlockHash::from_byte_array([11u8; 32]);
    assert!(!ctx.handle(Ok(notfound_frame(&[hash]))));
    assert_eq!(ctx.rate.cur_msgs, 2, "a notfound frame is one message");
    assert_eq!(
        ctx.rate.cur_bytes,
        (headers_frame(&[one_header()]).payload_len() + notfound_frame(&[hash]).payload_len())
            as u64,
        "the framed payload bytes are charged"
    );

    assert!(
        !ctx.handle(Ok(headers_frame(&[one_header()]))),
        "the first overflow stays connected"
    );
    assert_eq!(ctx.ban_score, RATE_LIMIT_BAN_SCORE);
    assert!(ctx.handle(Ok(notfound_frame(&[hash]))));
    assert_eq!(ctx.ban_score, RATE_LIMIT_BAN_SCORE * 2);
}

/// One `getheaders` exempts exactly one `headers` reply. A second reply is
/// unsolicited spam and takes the window like any heavy frame.
#[tokio::test]
async fn one_solicited_headers_reply_stays_off_the_session_window() {
    use rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC;

    let (mut ctx, _out, mut body_rx, mut ctrl_rx) = test_ctx(1);
    ctx.rate = PeerRateLimiter::new(1, DEFAULT_MAX_BYTES_PER_SEC);
    ctx.headers_asks.note_ask(ibd_mono_ms());
    assert!(!ctx.handle(Ok(headers_frame(&[one_header()]))));
    assert_eq!(ctx.rate.cur_msgs, 0, "the asked reply is exempt");
    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), ctrl_rx.recv())
        .await
        .expect("an exempt reply is still decoded")
        .expect("ctrl open");
    match ev {
        PeerEvent::Headers { headers, .. } => assert_eq!(headers.len(), 1),
        _ => panic!("the exempt reply must reach ctrl"),
    }

    assert!(!ctx.handle(Ok(headers_frame(&[one_header()]))));
    assert_eq!(ctx.rate.cur_msgs, 1, "a second reply is charged");
    assert!(!ctx.handle(Ok(headers_frame(&[one_header()]))));
    assert_eq!(ctx.ban_score, RATE_LIMIT_BAN_SCORE);
    assert!(body_rx.try_recv().is_err(), "the overflow stays connected");
}

/// A `notfound` is not a headers reply: it neither spends the ask nor skips
/// the window.
#[tokio::test]
async fn notfound_does_not_spend_a_headers_exemption() {
    use rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC;

    let (mut ctx, _out, _body, _ctrl) = test_ctx(1);
    ctx.rate = PeerRateLimiter::new(1, DEFAULT_MAX_BYTES_PER_SEC);
    ctx.headers_asks.note_ask(ibd_mono_ms());
    let hash = BlockHash::from_byte_array([12u8; 32]);
    assert!(!ctx.handle(Ok(notfound_frame(&[hash]))));
    assert_eq!(ctx.rate.cur_msgs, 1, "notfound is charged");

    assert!(!ctx.handle(Ok(headers_frame(&[one_header()]))));
    assert_eq!(
        ctx.rate.cur_msgs, 1,
        "the headers reply still uses the outstanding ask"
    );
}

#[test]
fn a_fresh_headers_ask_is_not_stale_against_process_start() {
    let asks = HeaderAsks::new();
    let now = HEADERS_ASK_TTL_MS + 1;
    asks.note_ask(now);
    assert!(
        asks.take_reply(now),
        "the ask's own time is the freshness mark"
    );
    assert!(!asks.take_reply(now), "one ask exempts one reply");
}

#[test]
fn a_stale_headers_ask_does_not_bank_credit_for_the_next_one() {
    let asks = HeaderAsks::new();
    asks.note_ask(0);
    asks.note_ask(HEADERS_ASK_TTL_MS + 1);
    assert!(
        asks.take_reply(HEADERS_ASK_TTL_MS + 2),
        "the fresh ask exempts its reply"
    );
    assert!(
        !asks.take_reply(HEADERS_ASK_TTL_MS + 2),
        "the stale ask is forgotten when the next one arrives"
    );
}

#[test]
fn a_newer_ask_does_not_keep_an_expired_one() {
    let asks = HeaderAsks::new();
    asks.note_ask(0);
    asks.note_ask(50_000);
    assert!(
        asks.take_reply(100_000),
        "the ask 50 s old is still inside the window"
    );
    assert!(
        !asks.take_reply(100_000),
        "the ask 100 s old did not bank a second exemption"
    );
}

#[test]
fn a_stale_or_stacked_headers_ask_does_not_bank_exemptions() {
    let asks = HeaderAsks::new();
    asks.note_ask(1_000);
    assert!(
        !asks.take_reply(1_000 + HEADERS_ASK_TTL_MS + 1),
        "a reply after the ask window is charged"
    );
    assert!(!asks.take_reply(1_000), "the stale ask is forgotten");

    let newest = 1_000 + HEADERS_ASKS_MAX as u64 + 3;
    for now in 1_000..newest {
        asks.note_ask(now);
    }
    for _ in 0..HEADERS_ASKS_MAX {
        assert!(asks.take_reply(newest), "a stacked ask exempts one reply");
    }
    assert!(!asks.take_reply(newest), "the ask count stays at its cap");
}

fn test_ctx(
    permits: usize,
) -> (
    IbdReadCtx,
    mpsc::UnboundedReceiver<NetworkMessage>,
    mpsc::UnboundedReceiver<PeerEvent>,
    mpsc::UnboundedReceiver<PeerEvent>,
) {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (body_tx, body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    let ctx = IbdReadCtx {
        id: 3,
        net: FROM,
        out_tx,
        sinks: PeerEventSinks {
            body: body_tx,
            ctrl: ctrl_tx,
        },
        requested: Arc::new(Mutex::new(Default::default())),
        headers_asks: Default::default(),
        solicited_bytes: Arc::new(AtomicU64::new(0)),
        solicited_ms: Arc::new(AtomicU64::new(0)),
        light_decode: Arc::new(Semaphore::new(permits)),
        light: LightDecodes { tasks: Vec::new() },
        rate: PeerRateLimiter::default_limits(),
        logged_invalid_v2: false,
        ban_score: 0,
        pong: Arc::new(PongSlot::new()),
    };
    (ctx, out_rx, body_rx, ctrl_rx)
}

fn oversized_block() -> FramedMessage {
    let n = (rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC as usize) + 1;
    framed(*b"block\0\0\0\0\0\0\0", vec![0u8; n])
}

fn one_header() -> Header {
    use bitcoin::block::Version;
    use bitcoin::CompactTarget;
    Header {
        version: Version::from_consensus(4),
        prev_blockhash: BlockHash::from_byte_array([0u8; 32]),
        merkle_root: bitcoin::TxMerkleNode::from_byte_array([0u8; 32]),
        time: 1,
        bits: CompactTarget::from_consensus(0x207f_ffff),
        nonce: 1,
    }
}

fn recv_body(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) -> PeerEvent {
    rx.try_recv().expect("body event")
}

#[test]
fn read_progress_counts_the_delta_from_the_mark() {
    let bytes = AtomicU64::new(0);
    let mut mark = 0usize;
    let rx_ms = AtomicU64::new(0);
    // The process clock starts at 0. A stamp of 0 is "no read yet".
    wait_past_mono_sentinel();
    note_read_progress(&bytes, &rx_ms, &mut mark, 10);
    note_read_progress(&bytes, &rx_ms, &mut mark, 25);
    assert_eq!(bytes.load(Ordering::Relaxed), 25);
    assert_eq!(mark, 25);
    assert!(rx_ms.load(Ordering::Relaxed) > 0);
    let stamped = rx_ms.load(Ordering::Relaxed);
    // The reader stores the latest buffered size, even when it shrinks.
    note_read_progress(&bytes, &rx_ms, &mut mark, 4);
    assert_eq!(bytes.load(Ordering::Relaxed), 25);
    assert_eq!(mark, 4);
    assert_eq!(rx_ms.load(Ordering::Relaxed), stamped);
}

#[test]
fn second_decoy_past_the_window_is_the_threshold() {
    let mut rate = PeerRateLimiter::default_limits();
    let mut score = 0u32;
    let n = (rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC as usize) + 1;
    assert!(decoy_hook(&mut rate, &mut score, n).is_ok());
    assert_eq!(score, RATE_LIMIT_BAN_SCORE);
    let err = decoy_hook(&mut rate, &mut score, n).unwrap_err();
    assert!(matches!(
        err,
        NetError::Protocol("peer misbehavior threshold")
    ));
}

fn ping_frame(nonce: u64) -> FramedMessage {
    framed(*b"ping\0\0\0\0\0\0\0\0", nonce.to_le_bytes().to_vec())
}

/// Two valid pings leave one pong, and the writer emits the later nonce.
/// Pings past the message window take the misbehavior disconnect. A block
/// this node requested still skips that window.
#[test]
fn ibd_pings_coalesce_and_overflow_the_message_window() {
    use rbitcoin_net::peer_dos::DEFAULT_MAX_MSGS_PER_SEC;

    let (mut ctx, mut out_rx, _body_rx, _ctrl) = test_ctx(1);
    assert!(!ctx.handle(Ok(ping_frame(1))));
    assert!(!ctx.handle(Ok(ping_frame(0))));
    assert!(matches!(out_rx.try_recv(), Ok(NetworkMessage::Pong(_))));
    assert!(
        out_rx.try_recv().is_err(),
        "a second ping does not queue a second pong"
    );
    assert_eq!(
        ctx.pong.claimed_nonce_or(1),
        0,
        "nonce 0 is the pong the writer sends"
    );
    assert!(ctx.pong.finish(0).is_none());

    let (mut ctx, mut out_rx, mut body_rx, _ctrl) = test_ctx(1);
    for n in 0..DEFAULT_MAX_MSGS_PER_SEC {
        assert!(
            !ctx.handle(Ok(ping_frame(u64::from(n) + 1))),
            "ping {n} fits the window"
        );
    }
    let mut queued = 0usize;
    while out_rx.try_recv().is_ok() {
        queued += 1;
    }
    assert_eq!(queued, 1, "the window of pings is one pong");
    assert!(!ctx.handle(Ok(ping_frame(9))));
    assert_eq!(ctx.ban_score, RATE_LIMIT_BAN_SCORE);
    assert!(body_rx.try_recv().is_err(), "the first overflow stays");
    assert!(ctx.handle(Ok(ping_frame(10))));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { peer, reason } => {
            assert_eq!(peer, 3);
            assert_eq!(reason, "peer misbehavior threshold");
        }
        _ => panic!("the next ping disconnects"),
    }

    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    for n in 0..DEFAULT_MAX_MSGS_PER_SEC {
        assert!(!ctx.handle(Ok(ping_frame(u64::from(n) + 1))));
    }
    let block = framed(*b"block\0\0\0\0\0\0\0", vec![7u8; 80]);
    let hash = block.block_hash_from_header().expect("80-byte header");
    ctx.requested.lock().unwrap().insert(hash);
    assert!(
        !ctx.handle(Ok(block)),
        "a requested block skips the ping window"
    );
    assert!(
        matches!(recv_body(&mut body_rx), PeerEvent::BlockFramed { .. }),
        "the requested block is delivered"
    );
}

/// A ping during the extra pong is written, and a later ping still queues.
#[tokio::test]
async fn an_extra_ibd_pong_still_answers_the_next_ping() {
    let pong = PongSlot::new();
    assert!(pong.note(7));
    let mut sent = Vec::new();
    struct Emit<'a> {
        sent: &'a mut Vec<u64>,
        pong: &'a PongSlot,
    }
    impl IbdPongEmit for Emit<'_> {
        fn emit(
            &mut self,
            nonce: u64,
        ) -> impl std::future::Future<Output = Result<(), NetError>> + Send {
            self.sent.push(nonce);
            if nonce == 7 {
                assert!(
                    !self.pong.note(8),
                    "the slot stays held during the first write"
                );
            }
            if nonce == 8 {
                assert!(
                    !self.pong.note(9),
                    "a ping during the extra write is stored"
                );
            }
            std::future::ready(Ok(()))
        }
    }
    write_ibd_pong(
        &pong,
        1,
        &mut Emit {
            sent: &mut sent,
            pong: &pong,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        sent,
        vec![7, 8, 9],
        "the writer emits every nonce that arrived during a write"
    );
    assert!(
        pong.note(10),
        "the slot is free after the latest nonce is written"
    );
}

#[test]
fn ping_and_solicited_block_stay_on_their_channels() {
    let (mut ctx, mut out_rx, mut body_rx, mut ctrl_rx) = test_ctx(1);
    let nonce = 0x0102_0304_0506_0708u64;
    assert!(!ctx.handle(Ok(framed(
        *b"ping\0\0\0\0\0\0\0\0",
        nonce.to_le_bytes().to_vec()
    ))));
    assert!(matches!(out_rx.try_recv(), Ok(NetworkMessage::Pong(n)) if n == nonce));
    assert!(!ctx.handle(Ok(framed(*b"ping\0\0\0\0\0\0\0\0", vec![1, 2, 3]))));
    assert!(out_rx.try_recv().is_err());

    let block = framed(*b"block\0\0\0\0\0\0\0", vec![7u8; 80]);
    let hash = block.block_hash_from_header().expect("80-byte header");
    ctx.requested.lock().unwrap().insert(hash);
    assert!(!ctx.handle(Ok(block)));
    match recv_body(&mut body_rx) {
        PeerEvent::BlockFramed {
            peer,
            hash: got,
            payload,
        } => {
            assert_eq!(peer, 3);
            assert_eq!(got, hash);
            assert_eq!(payload.len(), 80);
        }
        _ => panic!("solicited block must be framed"),
    }
    assert_eq!(ctx.solicited_bytes.load(Ordering::Relaxed), 80);
    assert!(ctrl_rx.try_recv().is_err());

    let again = framed(*b"block\0\0\0\0\0\0\0", vec![7u8; 80]);
    assert!(!ctx.handle(Ok(again)), "one resend still fits the window");
    assert!(body_rx.try_recv().is_err(), "a resend is not a second body");
    assert_eq!(ctx.solicited_bytes.load(Ordering::Relaxed), 80);

    assert!(!ctx.handle(Ok(framed(*b"block\0\0\0\0\0\0\0", vec![1, 2, 3]))));
    assert!(body_rx.try_recv().is_err(), "a short block has no hash");
}

#[test]
fn second_unsolicited_block_past_the_window_stops_the_reader() {
    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    assert!(!ctx.handle(Ok(oversized_block())));
    assert!(body_rx.try_recv().is_err(), "the first overflow stays");
    assert!(ctx.handle(Ok(oversized_block())));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { peer, reason } => {
            assert_eq!(peer, 3);
            assert_eq!(reason, "peer misbehavior threshold");
        }
        _ => panic!("second overflow must stop"),
    }
}

#[test]
fn short_block_frames_score_like_other_frames() {
    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    // One message per window: three frames reach the threshold without a wait.
    ctx.rate = PeerRateLimiter::new(1, rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC);
    let short = || framed(*b"block\0\0\0\0\0\0\0", vec![1, 2, 3]);
    assert!(
        !ctx.handle(Ok(short())),
        "the first short block fits the window"
    );
    assert_eq!(ctx.ban_score, 0, "a frame inside the window scores nothing");
    assert!(
        !ctx.handle(Ok(short())),
        "the first overflow stays connected"
    );
    assert_eq!(
        ctx.ban_score, RATE_LIMIT_BAN_SCORE,
        "a short block past the window scores like any other frame"
    );
    assert!(body_rx.try_recv().is_err(), "the first overflow stays");
    assert!(
        ctx.handle(Ok(short())),
        "the next short block reaches the threshold"
    );
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { peer, reason } => {
            assert_eq!(peer, 3);
            assert_eq!(reason, "peer misbehavior threshold");
        }
        _ => panic!("short block frames must stop the reader"),
    }
}

#[test]
fn light_permit_exhaustion_scores_like_other_frames() {
    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(0);
    let n = (rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC as usize) + 1;
    let frame = framed(*b"inv\0\0\0\0\0\0\0\0\0", vec![0u8; n]);
    assert!(!ctx.handle(Ok(frame)));
    assert!(body_rx.try_recv().is_err());
    let frame = framed(*b"inv\0\0\0\0\0\0\0\0\0", vec![0u8; n]);
    assert!(ctx.handle(Ok(frame)));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { reason, .. } => assert_eq!(reason, "peer misbehavior threshold"),
        _ => panic!("a second permit miss past the window must stop"),
    }
}

#[test]
fn invalid_v2_and_io_errors_stop_on_the_shipped_reasons() {
    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    let n = (rbitcoin_net::peer_dos::DEFAULT_MAX_BYTES_PER_SEC as usize) + 1;
    assert!(!ctx.handle(Err(NetError::InvalidV2Type { contents_len: n })));
    assert!(ctx.handle(Err(NetError::InvalidV2Type { contents_len: n })));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { reason, .. } => assert_eq!(reason, "peer misbehavior threshold"),
        _ => panic!("second invalid type must stop"),
    }

    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    let eof = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed");
    assert!(ctx.handle(Err(NetError::Io(eof))));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { reason, .. } => assert!(reason.starts_with("eof:"), "{reason}"),
        _ => panic!("eof must be a dead peer"),
    }

    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
    assert!(ctx.handle(Err(NetError::Io(reset))));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { reason, .. } => assert!(reason.starts_with("eof:"), "{reason}"),
        _ => panic!("reset must be a dead peer"),
    }

    let (mut ctx, _out, mut body_rx, _ctrl) = test_ctx(1);
    assert!(ctx.handle(Err(NetError::Protocol("bye"))));
    match recv_body(&mut body_rx) {
        PeerEvent::Dead { reason, .. } => assert!(reason.contains("bye"), "{reason}"),
        _ => panic!("protocol error must be a dead peer"),
    }
}

#[test]
fn light_and_heavy_frames_deliver_or_die_without_stopping_the_read() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use bitcoin::consensus::encode::Encodable;
        let (mut ctx, _out, mut body_rx, mut ctrl_rx) = test_ctx(2);
        let hash = BlockHash::from_byte_array([4u8; 32]);
        let inv = bitcoin::consensus::encode::serialize(&vec![Inventory::Block(hash)]);
        assert!(!ctx.handle(Ok(framed(*b"inv\0\0\0\0\0\0\0\0\0", inv))));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), ctrl_rx.recv())
            .await
            .expect("light inv")
            .expect("ctrl open");
        match ev {
            PeerEvent::BlocksInv { peer, hashes } => {
                assert_eq!(peer, 3);
                assert_eq!(hashes, vec![hash]);
            }
            _ => panic!("block inv must reach ctrl"),
        }

        let mut over = Vec::new();
        bitcoin::consensus::encode::VarInt((rbitcoin_net::codec::MAX_INV_SIZE as u64) + 1)
            .consensus_encode(&mut over)
            .unwrap();
        assert!(!ctx.handle(Ok(framed(*b"inv\0\0\0\0\0\0\0\0\0", over))));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), body_rx.recv())
            .await
            .expect("oversize inv")
            .expect("body open");
        match ev {
            PeerEvent::Dead { reason, .. } => {
                assert!(reason.contains("message too large"), "{reason}")
            }
            _ => panic!("oversize inv must die in the decoder"),
        }

        // Core's headers payload is count, then each 80-byte header and a 0 tx count.
        let mut headers = Vec::new();
        bitcoin::consensus::encode::VarInt(1)
            .consensus_encode(&mut headers)
            .unwrap();
        one_header().consensus_encode(&mut headers).unwrap();
        headers.push(0);
        let headers_frame = framed(*b"headers\0\0\0\0\0", headers);
        match headers_frame.clone().try_decode() {
            Ok(msg) => {
                assert!(matches!(msg.payload(), NetworkMessage::Headers(h) if h.len() == 1))
            }
            Err(e) => panic!("headers fixture must decode, got {e}"),
        }
        assert!(!ctx.handle(Ok(headers_frame)));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), ctrl_rx.recv())
            .await
            .expect("headers")
            .expect("ctrl open");
        match ev {
            PeerEvent::Headers { headers, .. } => assert_eq!(headers.len(), 1),
            _ => panic!("headers must reach ctrl"),
        }

        let mut too_many = Vec::new();
        bitcoin::consensus::encode::VarInt((rbitcoin_net::codec::MAX_HEADERS_RESULTS as u64) + 1)
            .consensus_encode(&mut too_many)
            .unwrap();
        assert!(!ctx.handle(Ok(framed(*b"headers\0\0\0\0\0", too_many))));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), body_rx.recv())
            .await
            .expect("oversize headers")
            .expect("body open");
        match ev {
            PeerEvent::Dead { reason, .. } => {
                assert!(reason.contains("message too large"), "{reason}")
            }
            _ => panic!("oversize headers must die in the decoder"),
        }
    });
}

fn sinks() -> (
    PeerEventSinks,
    mpsc::UnboundedReceiver<PeerEvent>,
    mpsc::UnboundedReceiver<PeerEvent>,
) {
    let (body_tx, body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    (
        PeerEventSinks {
            body: body_tx,
            ctrl: ctrl_tx,
        },
        body_rx,
        ctrl_rx,
    )
}

/// The endpoint of test peer 3.
const FROM: rbitcoin_net::NetAddr =
    rbitcoin_net::NetAddr::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 8333));

fn apply(msg: NetworkMessage, sinks: &PeerEventSinks) {
    apply_decoded_message(
        3,
        FROM,
        sinks,
        bitcoin::p2p::message::RawNetworkMessage::new(Magic::from(bitcoin::Network::Regtest), msg),
    );
}

#[test]
fn decoded_messages_fan_out_blocks_and_addrs_only() {
    use bitcoin::p2p::address::{AddrV2, AddrV2Message, Address};

    let (sinks, mut body_rx, mut ctrl_rx) = sinks();
    apply(NetworkMessage::Headers(vec![one_header()]), &sinks);
    assert!(matches!(
        ctrl_rx.try_recv(),
        Ok(PeerEvent::Headers { headers, .. }) if headers.len() == 1
    ));
    apply(NetworkMessage::Headers(vec![]), &sinks);
    assert!(matches!(ctrl_rx.try_recv(), Ok(PeerEvent::Headers { .. })));

    apply(NetworkMessage::NotFound(vec![]), &sinks);
    apply(
        NetworkMessage::NotFound(vec![Inventory::Transaction(
            bitcoin::Txid::from_byte_array([3u8; 32]),
        )]),
        &sinks,
    );
    assert!(body_rx.try_recv().is_err());
    let block = BlockHash::from_byte_array([1u8; 32]);
    apply(
        NetworkMessage::NotFound(vec![Inventory::Block(block)]),
        &sinks,
    );
    assert!(matches!(
        body_rx.try_recv(),
        Ok(PeerEvent::NotFound { hashes, .. }) if hashes == vec![block]
    ));

    apply(NetworkMessage::Inv(vec![]), &sinks);
    assert!(ctrl_rx.try_recv().is_err());
    apply(NetworkMessage::Inv(vec![Inventory::Block(block)]), &sinks);
    assert!(matches!(
        ctrl_rx.try_recv(),
        Ok(PeerEvent::BlocksInv { hashes, .. }) if hashes == vec![block]
    ));

    let good = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let flags = ServiceFlags::NETWORK | ServiceFlags::P2P_V2;
    apply(NetworkMessage::Addr(vec![]), &sinks);
    assert!(ctrl_rx.try_recv().is_err());
    apply(
        NetworkMessage::Addr(vec![(1, Address::new(&good, flags))]),
        &sinks,
    );
    assert!(matches!(ctrl_rx.try_recv(), Ok(PeerEvent::Addrs { from, .. }) if from == FROM));

    apply(NetworkMessage::AddrV2(vec![]), &sinks);
    assert!(ctrl_rx.try_recv().is_err());
    apply(
        NetworkMessage::AddrV2(vec![AddrV2Message {
            time: 1,
            services: flags,
            addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 9)),
            port: 8333,
        }]),
        &sinks,
    );
    assert!(matches!(ctrl_rx.try_recv(), Ok(PeerEvent::Addrs { from, .. }) if from == FROM));

    apply(NetworkMessage::SendAddrV2, &sinks);
    apply(
        NetworkMessage::Block(bitcoin::Block {
            header: one_header(),
            txdata: vec![],
        }),
        &sinks,
    );
    apply(NetworkMessage::Verack, &sinks);
    assert!(body_rx.try_recv().is_err());
    assert!(ctrl_rx.try_recv().is_err());
}

#[test]
fn block_inventory_hashes_keeps_blocks_only() {
    let block = BlockHash::from_byte_array([1u8; 32]);
    let witness = BlockHash::from_byte_array([2u8; 32]);
    let tx = bitcoin::Txid::from_byte_array([3u8; 32]);
    let inv = vec![
        Inventory::Transaction(tx),
        Inventory::Block(block),
        Inventory::WitnessBlock(witness),
    ];
    assert_eq!(block_inventory_hashes(&inv), vec![block, witness]);
}

#[test]
fn event_sinks_send_body_and_ctrl() {
    let (body_tx, mut body_rx) = mpsc::unbounded_channel();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    let sinks = PeerEventSinks {
        body: body_tx,
        ctrl: ctrl_tx,
    };
    sinks.send_body(PeerEvent::Dead {
        peer: 1,
        reason: "x".into(),
    });
    sinks.send_ctrl(PeerEvent::Headers {
        peer: 1,
        headers: vec![],
    });
    assert!(matches!(
        body_rx.try_recv().unwrap(),
        PeerEvent::Dead { .. }
    ));
    assert!(matches!(
        ctrl_rx.try_recv().unwrap(),
        PeerEvent::Headers { .. }
    ));
}

#[test]
fn socket_addrs_from_addr_and_addrv2_filter() {
    use bitcoin::p2p::address::{AddrV2, AddrV2Message, Address};
    use bitcoin::p2p::ServiceFlags;

    let v2_net = ServiceFlags::NETWORK | ServiceFlags::P2P_V2;
    let v2_limited = ServiceFlags::NETWORK_LIMITED | ServiceFlags::P2P_V2;
    let good = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let limited_sa = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 8333);
    let no_svc = Address::new(&good, ServiceFlags::NONE);
    let net_only = Address::new(&good, ServiceFlags::NETWORK);
    let net_v2 = Address::new(&good, v2_net);
    let limited_only = Address::new(&limited_sa, ServiceFlags::NETWORK_LIMITED);
    let limited_v2 = Address::new(&limited_sa, v2_limited);
    let unusable = Address::new(
        &SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8333),
        v2_net,
    );
    let out = socket_addrs_from_addr(&[
        (1, no_svc),
        (2, net_only),
        (3, net_v2),
        (4, limited_only),
        (5, limited_v2),
        (6, unusable),
    ]);
    assert_eq!(out, vec![(3, good), (5, limited_sa)]);

    let v2_good = AddrV2Message {
        time: 1,
        services: v2_net,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 9)),
        port: 18444,
    };
    let v2_net_only = AddrV2Message {
        time: 1,
        services: ServiceFlags::NETWORK,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 8)),
        port: 18444,
    };
    let v2_bad_svc = AddrV2Message {
        time: 1,
        services: ServiceFlags::NONE,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 10)),
        port: 18444,
    };
    let v2_zero_port = AddrV2Message {
        time: 1,
        services: v2_net,
        addr: AddrV2::Ipv4(Ipv4Addr::new(9, 9, 9, 11)),
        port: 0,
    };
    let out2 = net_addrs_from_addrv2(&[v2_good, v2_net_only, v2_bad_svc, v2_zero_port]);
    assert_eq!(
        out2,
        vec![(
            1,
            rbitcoin_net::NetAddr::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
                18444
            ))
        )]
    );

    let v6_multi = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1)), 8333);
    assert!(!usable_dial_addr(&v6_multi));
    let v6_net = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8333);
    let v2_v6 = AddrV2Message {
        time: 1,
        services: v2_limited,
        addr: AddrV2::Ipv6(Ipv6Addr::LOCALHOST),
        port: 8333,
    };
    let out3 = net_addrs_from_addrv2(&[v2_v6]);
    assert_eq!(out3, vec![(1, rbitcoin_net::NetAddr::Ip(v6_net))]);

    let onion: rbitcoin_net::NetAddr =
        "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion:8333"
            .parse()
            .unwrap();
    let rbitcoin_net::NetAddr::Onion { pk, port } = onion else {
        panic!("fixture");
    };
    let v2_onion = AddrV2Message {
        time: 1,
        services: v2_net,
        addr: AddrV2::TorV3(pk),
        port,
    };
    let nets = net_addrs_from_addrv2(&[v2_onion]);
    assert_eq!(nets, vec![(1, onion)]);
}

/// Legacy `addr` has no BIP155 network id. `fc00::/8` is CJDNS, not a
/// clearnet socket the dial book can fill.
#[test]
fn v1_addr_fc00_is_cjdns_not_clearnet() {
    use bitcoin::p2p::address::Address;
    use bitcoin::p2p::ServiceFlags;

    let fc_ip = Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1);
    let fc = SocketAddr::new(IpAddr::V6(fc_ip), 8333);
    let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);
    let flags = ServiceFlags::NETWORK | ServiceFlags::P2P_V2;
    let rows = net_addrs_from_addr(&[(1, Address::new(&fc, flags)), (2, Address::new(&v4, flags))]);
    assert!(
        rows.iter().any(
            |(_, a)| matches!(a, rbitcoin_net::NetAddr::Cjdns { ip, port: 8333 } if *ip == fc_ip)
        ),
        "fc00 must be CJDNS, got {rows:?}"
    );
    assert!(rows
        .iter()
        .any(|(_, a)| matches!(a, rbitcoin_net::NetAddr::Ip(s) if *s == v4)));
    assert!(
        !rows
            .iter()
            .any(|(_, a)| matches!(a, rbitcoin_net::NetAddr::Ip(s) if s.ip() == IpAddr::V6(fc_ip))),
        "fc00 must not stay a clearnet Ip"
    );

    let local = SocketAddr::from(([127, 0, 0, 1], 18444));
    let mut off = rbitcoin_net::seeds::AddrMan::new();
    rbitcoin_ibd::ibd::events::inject_learned_addrs(
        &mut off,
        &rows,
        local,
        rbitcoin_net::NetAddr::Ip(local),
        1,
        1_700_000_000,
    );
    assert!(
        off.entries()
            .iter()
            .all(|e| !matches!(e.addr, rbitcoin_net::NetAddr::Cjdns { .. })),
        "CJDNS stays out of the book until the overlay is reachable: {:?}",
        off.entries()
    );
    assert!(off
        .entries()
        .iter()
        .any(|e| e.addr == rbitcoin_net::NetAddr::Ip(v4)));

    let mut on = rbitcoin_net::seeds::AddrMan::new();
    on.set_cjdns_reachable(true);
    rbitcoin_ibd::ibd::events::inject_learned_addrs(
        &mut on,
        &rows,
        local,
        rbitcoin_net::NetAddr::Ip(local),
        1,
        1_700_000_000,
    );
    assert!(
        on.entries().iter().any(|e| matches!(
            e.addr,
            rbitcoin_net::NetAddr::Cjdns { ip, port: 8333 } if ip == fc_ip
        )),
        "reachable CJDNS is learned as CJDNS: {:?}",
        on.entries()
    );
}

/// The IBD dial book keeps the time a catch-up peer claimed, with Core's
/// two-hour gossip discount, so the rows it flushes into the served book are
/// not all pinned five days old.
#[test]
fn ibd_gossip_keeps_the_claimed_time() {
    let local = SocketAddr::from(([127, 0, 0, 1], 18444));
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)), 8333);
    let mut book = rbitcoin_net::seeds::AddrMan::new();
    rbitcoin_ibd::ibd::events::inject_learned_addrs(
        &mut book,
        &[(1_699_996_400, rbitcoin_net::NetAddr::Ip(peer))],
        local,
        rbitcoin_net::NetAddr::Ip(local),
        1,
        1_700_000_000,
    );
    assert_eq!(
        book.last_seen_of(&rbitcoin_net::NetAddr::Ip(peer)),
        1_699_996_400 - 2 * 60 * 60
    );
}

/// Waits until the other thread posts `round`. Spins briefly so the two
/// threads stay close together, then yields so a waiter on a small runner
/// does not hold the core the other thread needs. Fails the round if the
/// other thread never arrives.
fn wait_for_round(counter: &AtomicUsize, round: usize, other: &str) {
    const SPINS: u32 = 256;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut spins = 0u32;
    while counter.load(Ordering::Acquire) != round {
        if spins < SPINS {
            spins += 1;
            std::hint::spin_loop();
        } else {
            assert!(
                std::time::Instant::now() < deadline,
                "{other} never reached round {round}"
            );
            std::thread::yield_now();
        }
    }
}

/// A ping that lands while the pong is written is answered. The reader
/// stores the nonce then claims; the writer clears the claim then reads the
/// nonce. With release/acquire each side can miss the other's store (x86
/// lets the clear sit in the store buffer past the load), and neither queues
/// the new pong. Many tight rounds give that window a chance.
#[test]
fn pong_written_while_a_ping_lands_is_not_lost() {
    use std::sync::atomic::AtomicBool;
    const ROUNDS: usize = 200_000;
    const IDLE: u64 = u64::MAX;
    let slot = Arc::new(PongSlot::new());
    let go = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let claimed = Arc::new(AtomicBool::new(false));
    let reader = {
        let (slot, go, done, claimed) = (
            Arc::clone(&slot),
            Arc::clone(&go),
            Arc::clone(&done),
            Arc::clone(&claimed),
        );
        std::thread::spawn(move || {
            for round in 1..=ROUNDS {
                wait_for_round(&go, round, "writer");
                claimed.store(slot.note(2 * round as u64 + 1), Ordering::Release);
                done.store(round, Ordering::Release);
            }
        })
    };
    let mut lost = 0usize;
    for round in 1..=ROUNDS {
        // The writer holds the claim for the previous ping's pong.
        let written = 2 * round as u64;
        assert!(slot.note(written), "round {round} starts unclaimed");
        go.store(round, Ordering::Release);
        let again = slot.finish(written);
        wait_for_round(&done, round, "reader");
        if !claimed.load(Ordering::Acquire) && again.is_none() {
            lost += 1;
        }
        // Write out whatever is still claimed.
        let mut n = slot.claimed_nonce_or(IDLE);
        while n != IDLE {
            n = slot.finish(n).unwrap_or(IDLE);
        }
    }
    reader.join().unwrap();
    assert_eq!(lost, 0, "{lost} of {ROUNDS} pings were never answered");
}

/// Aborting the parent while it is suspended after the reader spawn
/// but before the writer exists still aborts the reader: the guard is
/// created with the reader, before the yield. A detached reader would
/// leave its oneshot open.
#[test]
fn pio_pin_abort_before_writer_spawn_aborts_reader() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (read_tx, mut read_rx) = tokio::sync::oneshot::channel::<()>();
        let parent = tokio::spawn(async move {
            let mut guard = PeerIoTasks {
                reader: tokio::spawn(async move {
                    let _hold = read_tx;
                    std::future::pending::<()>().await;
                }),
                writer: None,
            };
            tokio::task::yield_now().await;
            // The abort lands here, before the writer is spawned.
            std::future::pending::<()>().await;
            guard.writer = Some(tokio::spawn(async {}));
            let _ = &mut guard;
        });
        tokio::task::yield_now().await;
        parent.abort();
        tokio::task::yield_now().await;
        assert!(matches!(
            read_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    });
}
