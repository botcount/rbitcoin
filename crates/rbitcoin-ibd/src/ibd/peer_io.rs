//! IBD download peer: split read/write over BIP324 v2.
//!
//! Socket tasks stay **I/O-only**:
//! - reader: decrypt frame + cheap ping handling; heavy decode off-thread
//! - writer: encode offloaded for heavy payloads; then encrypt + write

use super::rate::PeerRate;
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_blockdata::{GetHeadersMessage, Inventory};
use bitcoin::p2p::{Magic, ServiceFlags};
use bitcoin::BlockHash;
use rbitcoin_net::codec::{FramedMessage, MAX_INV_SIZE};
use rbitcoin_net::error::NetError;
use rbitcoin_net::msg_decode::spawn_decode_then_with_err;
use rbitcoin_net::peer::{
    connect_and_handshake_timed, HandshakePolicy, BAN_SCORE_THRESHOLD, HANDSHAKE_TIMEOUT,
};
use rbitcoin_net::peer_dos::{PeerRateLimiter, RATE_LIMIT_BAN_SCORE};
use rbitcoin_net::peers::PongSlot;
use rbitcoin_net::v2::{read_v2_frame_with_progress, write_v2_msg_offload, V2Reader, V2Writer};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

/// Block and witness-block entries from an `inv` or `notfound`.
#[doc(hidden)]
pub fn block_inventory_hashes(inv: &[Inventory]) -> Vec<BlockHash> {
    inv.iter()
        .filter_map(|i| match i {
            Inventory::Block(h) | Inventory::WitnessBlock(h) => Some(*h),
            _ => None,
        })
        .collect()
}

pub enum PeerCmd {
    GetHeaders { locator: Vec<BlockHash> },
    GetData { hashes: Vec<BlockHash> },
    Shutdown,
}

pub enum PeerEvent {
    Headers {
        peer: usize,
        headers: Vec<Header>,
    },
    /// Full `block` frame on the wire: hash from header bytes + **raw payload**.
    ///
    /// No full `Block` deserialize on the peer path — body queue stores these
    /// bytes; confirm pack decodes. Free getdata slots immediately.
    BlockFramed {
        peer: usize,
        hash: BlockHash,
        /// Consensus-serialized block (frame payload).
        payload: Vec<u8>,
    },
    /// Framed as `block` but unusable (e.g. truncated header) — re-request.
    BlockDecodeFailed {
        peer: usize,
        hash: BlockHash,
    },
    /// Peer answered `notfound` for these block hashes (does not have them).
    NotFound {
        peer: usize,
        hashes: Vec<BlockHash>,
    },
    /// Block `inv` during IBD. A peer retired from the header walk can rejoin.
    BlocksInv {
        peer: usize,
        hashes: Vec<BlockHash>,
    },
    /// Addresses learned from `addr` / `addrv2` (for IBD redial pool growth).
    /// `from` is the peer's endpoint: its slot may not be installed yet.
    Addrs {
        peer: usize,
        from: rbitcoin_net::NetAddr,
        /// `(claimed time, address)` as gossiped; the time is untrusted.
        addrs: Vec<(u32, rbitcoin_net::NetAddr)>,
    },
    /// Peer failed or closed.
    Dead {
        peer: usize,
        reason: String,
    },
}

/// Dual fan-out so body delivery is never stuck behind header floods on one FIFO.
///
/// - **body**: `BlockFramed` / fail / `NotFound` / `Dead` — drain first
/// - **ctrl**: `Headers` — budgeted so multi-peer header spam cannot livelock apply
#[derive(Clone)]
pub struct PeerEventSinks {
    pub body: mpsc::UnboundedSender<PeerEvent>,
    pub ctrl: mpsc::UnboundedSender<PeerEvent>,
}

impl PeerEventSinks {
    pub fn send_body(&self, ev: PeerEvent) {
        let _ = self.body.send(ev);
    }
    pub fn send_ctrl(&self, ev: PeerEvent) {
        let _ = self.ctrl.send(ev);
    }
}

pub struct PeerSlot {
    pub id: usize,
    pub addr: SocketAddr,
    pub net: rbitcoin_net::NetAddr,
    pub cmd_tx: mpsc::UnboundedSender<PeerCmd>,
    /// Hashes currently requested from this peer.
    pub in_flight: super::state::BlockHashSet,
    /// Same hashes, shared with the reader. One extra set per live peer so a
    /// block check does not take the IBD work state.
    pub requested: Arc<Mutex<super::state::BlockHashSet>>,
    /// `getheaders` asks the reader may exempt one `headers` reply for.
    pub headers_asks: Arc<HeaderAsks>,
    /// Bytes of blocks this peer was asked for. Stall-clock input.
    pub solicited_bytes: Arc<AtomicU64>,
    /// Bytes of the block frame still on the socket. Zero for pings and headers.
    pub open_block_bytes: Arc<AtomicU64>,
    /// Mono ms of the last solicited block the reader accepted.
    pub solicited_ms: Arc<AtomicU64>,
    /// Mono ms of the last socket read, including pings. Quiet stall only.
    pub any_rx_ms: Arc<AtomicU64>,
    /// Getdata queued on this peer and not yet written, plus the last write time.
    pub getdata_wire: Arc<GetdataWire>,
    /// Peer's `version.start_height` (best-effort network tip signal).
    pub peer_height: u32,
    /// Mono ms when the slot became live (post-handshake).
    pub connected_ms: u64,
    /// First block-payload mono ms (0 = none yet). IBD main thread only.
    pub first_data_ms: u64,
    /// All streamed wire bytes. Reader-only `fetch_add`. Not the stall clock.
    pub bytes_rx_total: Arc<AtomicU64>,
    pub rate: PeerRate,
    /// What this peer has shown of its chain. IBD main thread only.
    pub chain: super::peer_chain::PeerChain,
    /// `Manual` for a `--connect` target, as Core dials `-connect`.
    pub conn_type: rbitcoin_net::peers::PeerConnType,
    /// Node-clock seconds until which this peer is sent no block getdata.
    pub block_download_paused_until: Option<u64>,
    /// Getdata this peer was sent and has not answered, withdrawn from
    /// `in_flight` at the mono ms stored. It stays solicited on the reader
    /// for [`LATE_ANSWER_MS`].
    pub late_answers: super::state::BlockHashMap<u64>,
    pub alive: bool,
    pub task: JoinHandle<()>,
}

/// How long a withdrawn getdata stays solicited. Core processes a block a
/// peer sends after its request was removed, and it is not misbehavior; an
/// answer inside this window is a body, not unsolicited bytes. Two minutes,
/// the manual-peer cooldown, so a paused peer's released blocks stay
/// answerable through its pause.
pub const LATE_ANSWER_MS: u64 = 120_000;

/// Empty side-set and solicited counters for a new [`PeerSlot`].
/// New counters at zero. `Default` builds the same triple.
#[cfg_attr(false, mutants::skip)]
pub fn solicit_track() -> (
    Arc<Mutex<super::state::BlockHashSet>>,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
) {
    (
        Arc::new(Mutex::new(Default::default())),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    )
}

/// Asks sent on this peer that a `headers` reply may still answer.
///
/// A solicited reply is a normal part of header sync, so it stays off the
/// session window once per ask. The count is capped, and an ask older than
/// [`HEADERS_ASK_TTL_MS`] is forgotten when the next ask arrives or a reply
/// is taken, so a peer that never answers cannot bank exemptions for an
/// unsolicited burst.
#[derive(Debug, Default)]
pub struct HeaderAsks {
    state: Mutex<HeaderAskState>,
}

#[derive(Debug, Default)]
struct HeaderAskState {
    asks: VecDeque<u64>,
}

/// Most `getheaders` asks held for one peer.
#[doc(hidden)]
pub const HEADERS_ASKS_MAX: usize = 2;
/// How long a `getheaders` ask can exempt a reply.
#[doc(hidden)]
pub const HEADERS_ASK_TTL_MS: u64 = 60_000;

impl HeaderAsks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HeaderAskState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one `getheaders` sent at `now_ms`. Each ask carries its own
    /// expiry; a full record gives up its oldest.
    pub fn note_ask(&self, now_ms: u64) {
        let mut state = self.state();
        state
            .asks
            .retain(|at| now_ms.saturating_sub(*at) <= HEADERS_ASK_TTL_MS);
        if state.asks.len() >= HEADERS_ASKS_MAX {
            state.asks.pop_front();
        }
        state.asks.push_back(now_ms);
    }

    /// Take one exemption for a `headers` frame read at `now_ms`. Expired asks
    /// leave from the front; the oldest live ask answers this reply.
    pub fn take_reply(&self, now_ms: u64) -> bool {
        let mut state = self.state();
        while let Some(at) = state.asks.front().copied() {
            if now_ms.saturating_sub(at) <= HEADERS_ASK_TTL_MS {
                break;
            }
            state.asks.pop_front();
        }
        state.asks.pop_front().is_some()
    }
}

/// Outstanding getdata that has not left the process, and when this stall
/// window opened.
///
/// The window opens at the first [`Self::wrote`] after it was disarmed, not
/// when the main loop queues the command. A later write while the window is
/// open does not move it. [`Self::disarm_window`] runs when the peer owes
/// nothing, so the next ask starts a new 30s.
pub struct GetdataWire {
    pub unwritten: AtomicUsize,
    pub written_ms: AtomicU64,
    /// Mono ms the current on-wire window opened. [`WINDOW_DISARMED`] when
    /// no getdata of this window is on the socket yet.
    pub window_ms: AtomicI64,
}

/// `window_ms` while the peer owes no on-wire block.
#[doc(hidden)]
pub const WINDOW_DISARMED: i64 = -1;

impl GetdataWire {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            unwritten: AtomicUsize::new(0),
            written_ms: AtomicU64::new(0),
            window_ms: AtomicI64::new(WINDOW_DISARMED),
        })
    }

    pub fn queue(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.unwritten.fetch_add(n, Ordering::Relaxed);
    }

    /// `n` hashes from one getdata chunk just went out on the socket.
    ///
    /// The first write of a disarmed window opens the stall clock at
    /// `now_ms`. A write into an open window does not.
    pub fn wrote(&self, n: usize, now_ms: u64) {
        if n == 0 {
            return;
        }
        self.written_ms.store(now_ms, Ordering::Relaxed);
        let _ = self
            .unwritten
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(n))
            });
        self.arm_window_if_closed(now_ms);
    }

    pub fn unwritten(&self) -> usize {
        self.unwritten.load(Ordering::Relaxed)
    }

    pub fn written_ms(&self) -> u64 {
        self.written_ms.load(Ordering::Relaxed)
    }

    /// Open the stall window at `now_ms` if it is disarmed.
    pub fn arm_window_if_closed(&self, now_ms: u64) {
        let opened = i64::try_from(now_ms).unwrap_or(i64::MAX);
        let _ = self.window_ms.compare_exchange(
            WINDOW_DISARMED,
            opened,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Open a new stall window at `now_ms` for what is already on the wire.
    pub fn restart_window(&self, now_ms: u64) {
        let opened = i64::try_from(now_ms).unwrap_or(i64::MAX);
        self.window_ms.store(opened, Ordering::Relaxed);
    }

    /// Drop the stall deadline. The next [`Self::wrote`] starts a new one.
    pub fn disarm_window(&self) {
        self.window_ms.store(WINDOW_DISARMED, Ordering::Relaxed);
    }

    pub fn window_ms(&self) -> Option<u64> {
        let v = self.window_ms.load(Ordering::Relaxed);
        if v < 0 {
            None
        } else {
            Some(u64::try_from(v).unwrap_or(0))
        }
    }
}

/// Getdata is already on the socket. An empty window, or a window whose
/// hashes are still in the writer, has not asked the peer yet.
#[doc(hidden)]
pub fn asked_on_wire(slot: &PeerSlot) -> bool {
    let outstanding = slot.in_flight.len();
    outstanding > 0 && slot.getdata_wire.unwritten() < outstanding
}

/// Stall deadline for hashes already on the socket.
///
/// Nothing on the wire disarms the window, including a full queue that has
/// not been written. An open window stays at its first write. A window that
/// is on the wire but was never armed (tests, or a write the clock missed)
/// opens at [`GetdataWire::written_ms`].
#[doc(hidden)]
pub fn prepare_stall_window(slot: &PeerSlot) -> Option<u64> {
    if !asked_on_wire(slot) {
        slot.getdata_wire.disarm_window();
        return None;
    }
    if slot.getdata_wire.window_ms().is_none() {
        slot.getdata_wire
            .arm_window_if_closed(slot.getdata_wire.written_ms());
    }
    slot.getdata_wire.window_ms()
}

/// Copy the rate and lift one clock up to the window open.
///
/// Idle time before the ask falls behind `open` and drops out. A block or
/// socket byte inside the window stays later than `open` and still counts.
#[doc(hidden)]
pub fn rate_in_window(slot: &PeerSlot, open: u64, block_progress: bool) -> PeerRate {
    let mut rate = slot.rate;
    if block_progress {
        rate.progress_ms = rate.progress_ms.max(open);
    } else {
        rate.any_rx_ms = rate.any_rx_ms.max(open);
    }
    rate
}

/// True when this peer owes a block that is already on the wire and no
/// solicited block has arrived inside `stall_ms` of this window.
///
/// Socket bytes that are not a requested block do not move this clock.
pub fn getdata_on_wire_stalled(slot: &PeerSlot, now_ms: u64, stall_ms: u64) -> bool {
    let Some(open) = prepare_stall_window(slot) else {
        return false;
    };
    rate_in_window(slot, open, true).stalled(now_ms, stall_ms)
}

/// True when getdata is on the wire and this peer has sent no bytes at all
/// inside `stall_ms` of this window. A peer with nothing assigned is not quiet.
pub fn getdata_on_wire_quiet(slot: &PeerSlot, now_ms: u64, stall_ms: u64) -> bool {
    let Some(open) = prepare_stall_window(slot) else {
        return false;
    };
    rate_in_window(slot, open, false).quiet(now_ms, stall_ms)
}

/// In-flight light decodes (inv, addr, tx, cmpct) per IBD peer. Not a knob.
#[doc(hidden)]
pub const LIGHT_DECODE_PERMITS: usize = 32;

impl PeerSlot {
    /// Alive and not paused: assign may send this peer block getdata.
    pub fn takes_block_requests(&self) -> bool {
        self.alive && self.block_download_paused_until.is_none()
    }

    /// Who this peer is for mutated-copy records: its network address without
    /// the port, so a reconnect from a new port is the same sender. An onion,
    /// I2P or `--proxy` peer has no socket address (`addr` is `0.0.0.0`), so
    /// its own address keeps it apart from every other overlay peer.
    pub fn copy_sender(&self) -> rbitcoin_net::NetAddr {
        use rbitcoin_net::NetAddr;
        match self.net {
            NetAddr::Ip(s) => NetAddr::Ip(SocketAddr::new(s.ip().to_canonical(), 0)),
            NetAddr::Onion { pk, .. } => NetAddr::Onion { pk, port: 0 },
            NetAddr::I2p { dest, .. } => NetAddr::I2p { dest, port: 0 },
            NetAddr::Cjdns { ip, .. } => NetAddr::Cjdns { ip, port: 0 },
            NetAddr::Name { id, .. } => NetAddr::Name { id, port: 0 },
        }
    }

    /// Assign may ask this peer for `hash`: it takes block requests and did
    /// not answer `notfound` for it recently.
    pub fn may_take_block(&self, hash: &BlockHash, now_ms: u64) -> bool {
        self.takes_block_requests() && !self.chain.lacks(hash, now_ms)
    }

    /// Finished solicited bytes plus the open block frame, since the stamp.
    /// Pings, headers, and other socket bytes stay on `bytes_rx_total`.
    pub fn solicited_since_progress(&self) -> u64 {
        self.solicited_bytes
            .load(Ordering::Relaxed)
            .saturating_add(self.open_block_bytes.load(Ordering::Relaxed))
            .saturating_sub(self.rate.bytes_at_progress)
    }

    /// Pause block download until `until` and withdraw what this peer owed.
    pub fn pause_block_download(&mut self, until: u64) -> Vec<BlockHash> {
        let now = ibd_mono_ms();
        let released: Vec<BlockHash> = self.in_flight.drain().collect();
        for h in &released {
            self.late_answers.insert(*h, now);
        }
        self.disarm_stall_if_idle();
        self.block_download_paused_until = Some(until);
        released
    }

    /// Lift the pause once the node clock reaches it.
    pub fn resume_block_download_if_due(&mut self, now_secs: u64) {
        if self
            .block_download_paused_until
            .is_some_and(|until| now_secs >= until)
        {
            self.block_download_paused_until = None;
        }
    }

    /// This peer answered `hash` with a block or `notfound`. `true` when the
    /// getdata had been withdrawn and the answer was still owed.
    pub fn note_answer(&mut self, hash: &BlockHash) -> bool {
        self.in_flight.remove(hash);
        self.requested_set().remove(hash);
        self.disarm_stall_if_idle();
        self.late_answers.remove(hash).is_some()
    }

    /// Forget withdrawn getdata older than [`LATE_ANSWER_MS`]. The next
    /// [`Self::track_retain`] drops them from the reader.
    pub fn expire_late_answers(&mut self, now_ms: u64) {
        self.late_answers
            .retain(|_, at| now_ms.saturating_sub(*at) <= LATE_ANSWER_MS);
    }

    #[doc(hidden)]
    pub fn requested_set(&self) -> std::sync::MutexGuard<'_, super::state::BlockHashSet> {
        self.requested.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn track_insert(&mut self, hash: BlockHash) {
        self.in_flight.insert(hash);
        self.requested_set().insert(hash);
    }

    /// Withdraw `hash`. A getdata this peer still owes stays answerable.
    pub fn track_remove(&mut self, hash: &BlockHash) -> bool {
        let removed = self.in_flight.remove(hash);
        if removed {
            self.late_answers.insert(*hash, ibd_mono_ms());
        } else if !self.late_answers.contains_key(hash) {
            self.requested_set().remove(hash);
        }
        self.disarm_stall_if_idle();
        removed
    }

    pub fn track_clear(&mut self) {
        self.in_flight.clear();
        self.late_answers.clear();
        self.requested_set().clear();
        self.disarm_stall_if_idle();
    }

    pub fn track_retain(&mut self, mut keep: impl FnMut(&BlockHash) -> bool) {
        // Judge each hash once, before taking the reader mutex. The shared
        // set then matches that decision. `has_block` does not run while the
        // IBD reader is blocked in `note_solicited_block`.
        let now = ibd_mono_ms();
        let late = &mut self.late_answers;
        self.in_flight.retain(|h| {
            let kept = keep(h);
            if !kept {
                late.insert(*h, now);
            }
            kept
        });
        let requested = Arc::clone(&self.requested);
        let mut set = requested.lock().unwrap_or_else(|e| e.into_inner());
        set.clear();
        set.extend(self.in_flight.iter().copied());
        set.extend(self.late_answers.keys().copied());
        drop(set);
        self.disarm_stall_if_idle();
    }

    pub fn track_drain(&mut self) -> Vec<BlockHash> {
        let drained: Vec<BlockHash> = self.in_flight.drain().collect();
        self.late_answers.clear();
        self.requested_set().clear();
        self.disarm_stall_if_idle();
        drained
    }

    #[doc(hidden)]
    pub fn disarm_stall_if_idle(&self) {
        if self.in_flight.is_empty() {
            self.getdata_wire.disarm_window();
        }
    }
}

/// `true` when `hash` was requested: record `len` and the read time.
/// An absent hash leaves both counters unchanged.
pub fn note_solicited_block(
    requested: &Mutex<super::state::BlockHashSet>,
    bytes: &AtomicU64,
    ms: &AtomicU64,
    hash: &BlockHash,
    len: usize,
) -> bool {
    let mut set = requested.lock().unwrap_or_else(|e| e.into_inner());
    // One request, one unmetered body. A resend is not new progress.
    if !set.remove(hash) {
        return false;
    }
    bytes.fetch_add(len as u64, Ordering::Relaxed);
    ms.store(ibd_mono_ms(), Ordering::Relaxed);
    true
}

/// One light-decode slot, or `None` without waiting.
pub fn try_light_decode_permit(sem: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(sem).try_acquire_owned().ok()
}

#[doc(hidden)]
pub fn misbehavior_disconnect(rate: &mut PeerRateLimiter, ban_score: &mut u32, n: usize) -> bool {
    if rate.note(n) {
        return false;
    }
    *ban_score = ban_score.saturating_add(RATE_LIMIT_BAN_SCORE);
    *ban_score >= BAN_SCORE_THRESHOLD
}

#[doc(hidden)]
pub fn relay_headers(id: usize, sinks: &PeerEventSinks, headers: Vec<Header>) {
    sinks.send_ctrl(PeerEvent::Headers { peer: id, headers });
}

#[doc(hidden)]
pub fn relay_notfound(id: usize, sinks: &PeerEventSinks, inv: &[Inventory]) {
    let hashes = block_inventory_hashes(inv);
    if !hashes.is_empty() {
        sinks.send_body(PeerEvent::NotFound { peer: id, hashes });
    }
}

#[doc(hidden)]
pub fn relay_addr(
    id: usize,
    from: rbitcoin_net::NetAddr,
    sinks: &PeerEventSinks,
    list: &[(u32, bitcoin::p2p::address::Address)],
) {
    let addrs = net_addrs_from_addr(list);
    if !addrs.is_empty() {
        sinks.send_ctrl(PeerEvent::Addrs {
            peer: id,
            from,
            addrs,
        });
    }
}

#[doc(hidden)]
pub fn relay_addrv2(
    id: usize,
    from: rbitcoin_net::NetAddr,
    sinks: &PeerEventSinks,
    list: &[bitcoin::p2p::address::AddrV2Message],
) {
    let addrs = net_addrs_from_addrv2(list);
    if !addrs.is_empty() {
        sinks.send_ctrl(PeerEvent::Addrs {
            peer: id,
            from,
            addrs,
        });
    }
}

#[doc(hidden)]
pub fn relay_blocks_inv(id: usize, sinks: &PeerEventSinks, inv: &[Inventory]) {
    let hashes = block_inventory_hashes(inv);
    if !hashes.is_empty() {
        sinks.send_ctrl(PeerEvent::BlocksInv { peer: id, hashes });
    }
}

#[doc(hidden)]
pub fn apply_decoded_message(
    id: usize,
    from: rbitcoin_net::NetAddr,
    sinks: &PeerEventSinks,
    msg: bitcoin::p2p::message::RawNetworkMessage,
) {
    match msg.into_payload() {
        NetworkMessage::Headers(h) => relay_headers(id, sinks, h),
        NetworkMessage::NotFound(inv) => relay_notfound(id, sinks, &inv),
        NetworkMessage::Addr(list) => relay_addr(id, from, sinks, &list),
        NetworkMessage::AddrV2(list) => relay_addrv2(id, from, sinks, &list),
        NetworkMessage::SendAddrV2 => {}
        NetworkMessage::Block(_) => {}
        NetworkMessage::Inv(inv) => relay_blocks_inv(id, sinks, &inv),
        _other => {}
    }
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PeerCmd::Shutdown);
        self.task.abort();
    }
}

/// Monotonic milliseconds for IBD stall clocks (process-relative).
pub fn ibd_mono_ms() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// `0` means no stamp yet. A running test is already past that millisecond.
#[doc(hidden)]
#[cfg_attr(false, mutants::skip)]
pub fn wait_past_mono_sentinel() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
    while ibd_mono_ms() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "ibd_mono_ms stayed at the zero sentinel"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

pub fn note_stream_bytes(counter: &AtomicU64, n: u64) {
    if n == 0 {
        return;
    }
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn sample_peer_rates(slots: &mut [PeerSlot], now_ms: u64) {
    for s in slots {
        if !s.alive {
            continue;
        }
        let any = s.any_rx_ms.load(Ordering::Relaxed);
        s.rate.any_rx_ms = s.rate.any_rx_ms.max(any);
        let progress_before = s.rate.progress_ms;
        // Solicited block accept time. Pings stay on `any_rx_ms`.
        let mark = s.solicited_ms.load(Ordering::Relaxed);
        s.rate.progress_ms = s.rate.progress_ms.max(mark);
        let asked = s.getdata_wire.written_ms();
        s.rate.work_started_ms = s.rate.work_started_ms.max(asked);
        let solicited = s.solicited_bytes.load(Ordering::Relaxed);
        s.rate.sample(now_ms, solicited, !s.in_flight.is_empty());
        if s.rate.progress_ms > progress_before {
            s.rate.note_bytes_at_progress(solicited);
        }
    }
}

pub fn note_block_progress(slots: &mut [PeerSlot], peer: usize) {
    if let Some(s) = slots.iter_mut().find(|s| s.id == peer) {
        s.rate.note_rx(ibd_mono_ms());
        s.rate
            .note_bytes_at_progress(s.solicited_bytes.load(Ordering::Relaxed));
    }
}

pub fn note_block_rx(slots: &mut [PeerSlot], peer: usize, wire_bytes: usize) {
    if let Some(s) = slots.iter_mut().find(|s| s.id == peer) {
        let now = ibd_mono_ms();
        s.rate.note_rx(now);
        s.rate
            .note_bytes_at_progress(s.solicited_bytes.load(Ordering::Relaxed));
        if wire_bytes > 0 && s.first_data_ms == 0 {
            s.first_data_ms = now;
        }
    }
}

/// Light-decode tasks die with the reader. A dropped `JoinHandle` would detach.
#[doc(hidden)]
pub struct LightDecodes {
    pub tasks: Vec<JoinHandle<()>>,
}

impl Drop for LightDecodes {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl LightDecodes {
    #[doc(hidden)]
    pub fn push(&mut self, task: JoinHandle<()>) {
        self.tasks.retain(|t| !t.is_finished());
        self.tasks.push(task);
    }
}

/// Count `buffered` toward the open block when this frame cannot be a
/// `headers` message and this peer still owes a block. Otherwise clear it.
#[doc(hidden)]
pub fn note_open_block(
    open: &AtomicU64,
    requested: &Mutex<super::state::BlockHashSet>,
    contents_len: usize,
    buffered: usize,
) {
    if contents_len <= rbitcoin_net::codec::MAX_HEADERS_CONTENTS {
        open.store(0, Ordering::Relaxed);
        return;
    }
    let owed = !requested
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if !owed {
        open.store(0, Ordering::Relaxed);
        return;
    }
    open.store(buffered as u64, Ordering::Relaxed);
}

#[doc(hidden)]
pub fn note_read_progress(
    bytes_io: &AtomicU64,
    rx_ms: &AtomicU64,
    prog_mark: &mut usize,
    buffered: usize,
) {
    let delta = buffered.saturating_sub(*prog_mark);
    if delta > 0 {
        note_stream_bytes(bytes_io, delta as u64);
        rx_ms.store(ibd_mono_ms(), Ordering::Relaxed);
    }
    *prog_mark = buffered;
}

#[doc(hidden)]
pub fn decoy_hook(
    rate: &mut PeerRateLimiter,
    ban_score: &mut u32,
    n: usize,
) -> Result<(), NetError> {
    if rbitcoin_net::peer_dos::decoy_stays(rate, ban_score, n, BAN_SCORE_THRESHOLD) {
        Ok(())
    } else {
        Err(NetError::Protocol("peer misbehavior threshold"))
    }
}

/// Reader state for one IBD peer. The socket loop only pulls frames.
#[doc(hidden)]
pub struct IbdReadCtx {
    pub id: usize,
    /// The peer's endpoint, carried by its address events.
    pub net: rbitcoin_net::NetAddr,
    pub out_tx: mpsc::UnboundedSender<NetworkMessage>,
    pub sinks: PeerEventSinks,
    pub requested: Arc<Mutex<super::state::BlockHashSet>>,
    pub headers_asks: Arc<HeaderAsks>,
    pub solicited_bytes: Arc<AtomicU64>,
    pub solicited_ms: Arc<AtomicU64>,
    pub light_decode: Arc<Semaphore>,
    pub light: LightDecodes,
    /// Decoys, unknown types, short or unsolicited bodies, and heavy frames.
    /// Requested block bodies and the reply to each `getheaders` stay off
    /// this window so a fast peer is not clipped.
    pub rate: PeerRateLimiter,
    pub logged_invalid_v2: bool,
    pub ban_score: u32,
    pub pong: Arc<PongSlot>,
}

#[doc(hidden)]
pub fn send_threshold_dead(sinks: &PeerEventSinks, id: usize) {
    sinks.send_body(PeerEvent::Dead {
        peer: id,
        reason: "peer misbehavior threshold".to_string(),
    });
}

/// One debug line. The misbehavior score is applied either way.
#[cfg_attr(false, mutants::skip)]
fn first_invalid_v2(logged: &mut bool) -> bool {
    if *logged {
        return false;
    }
    *logged = true;
    true
}

#[doc(hidden)]
pub fn is_eof_or_reset(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::UnexpectedEof
        || err.kind() == std::io::ErrorKind::ConnectionReset
}

pub async fn decode_light_frame(
    id: usize,
    from: rbitcoin_net::NetAddr,
    sinks: PeerEventSinks,
    frame: FramedMessage,
    permit: OwnedSemaphorePermit,
) {
    let _permit = permit;
    match frame.try_decode() {
        Ok(msg) => apply_decoded_message(id, from, &sinks, msg),
        Err(e) => {
            sinks.send_body(PeerEvent::Dead {
                peer: id,
                reason: e.to_string(),
            });
        }
    }
}

#[doc(hidden)]
pub fn on_heavy_decoded(
    id: usize,
    from: rbitcoin_net::NetAddr,
    sinks: &PeerEventSinks,
    msg: bitcoin::p2p::message::RawNetworkMessage,
) {
    apply_decoded_message(id, from, sinks, msg);
}

#[doc(hidden)]
pub fn on_heavy_err(id: usize, sinks: &PeerEventSinks, err: NetError) {
    sinks.send_body(PeerEvent::Dead {
        peer: id,
        reason: err.to_string(),
    });
}

impl IbdReadCtx {
    /// `true` stops the reader.
    #[doc(hidden)]
    pub fn handle(&mut self, frame: Result<FramedMessage, NetError>) -> bool {
        match frame {
            Ok(frame) => self.dispatch_frame(frame),
            Err(err) => self.dispatch_err(err),
        }
    }

    #[doc(hidden)]
    pub fn dispatch_frame(&mut self, frame: FramedMessage) -> bool {
        if frame.is_ping() {
            return self.on_ping(&frame);
        }
        if frame.is_block() {
            return self.on_block(frame);
        }
        // Headers and notfound stay on the heavy decode pool.
        // Light frames take a permit without waiting.
        if !frame.decode_is_cpu_heavy() {
            return self.on_light(frame);
        }
        self.on_heavy(frame)
    }

    #[doc(hidden)]
    pub fn on_ping(&mut self, frame: &FramedMessage) -> bool {
        if self.score_overflow(frame.payload.len()) {
            return true;
        }
        if let Some(n) = frame.ping_nonce() {
            if self.pong.note(n) {
                let _ = self.out_tx.send(NetworkMessage::Pong(n));
            }
        }
        false
    }

    #[doc(hidden)]
    pub fn on_block(&mut self, frame: FramedMessage) -> bool {
        match frame.block_hash_from_header() {
            Some(hash) => {
                // Unsolicited bodies stay off the channel.
                // Requested bodies are not rate-capped.
                // A hash is only present once the payload holds a header.
                let n = frame.payload.len();
                if note_solicited_block(
                    &self.requested,
                    &self.solicited_bytes,
                    &self.solicited_ms,
                    &hash,
                    n,
                ) {
                    self.sinks.send_body(PeerEvent::BlockFramed {
                        peer: self.id,
                        hash,
                        payload: frame.payload,
                    });
                    false
                } else {
                    self.score_overflow(n)
                }
            }
            None => {
                rbitcoin_log::debug!(
                    Ibd,
                    "peer[{}] block frame without usable header hash",
                    self.id
                );
                // No header to hash, but the frame still costs the window.
                self.score_overflow(frame.payload.len())
            }
        }
    }

    #[doc(hidden)]
    pub fn on_light(&mut self, frame: FramedMessage) -> bool {
        let n = frame.payload.len();
        match try_light_decode_permit(&self.light_decode) {
            Some(permit) => {
                let sinks = self.sinks.clone();
                let id = self.id;
                let task = tokio::spawn(decode_light_frame(id, self.net, sinks, frame, permit));
                self.light.push(task);
                false
            }
            None => self.score_overflow(n),
        }
    }

    /// Never await a decode permit on the reader (stalls TCP). A heavy frame
    /// takes the session window before its decode is spawned; one `headers`
    /// reply per outstanding ask is exempt.
    #[doc(hidden)]
    pub fn on_heavy(&mut self, frame: FramedMessage) -> bool {
        let solicited_headers = frame.is_headers() && self.headers_asks.take_reply(ibd_mono_ms());
        if !solicited_headers && self.score_overflow(frame.payload.len()) {
            return true;
        }
        let id = self.id;
        let net = self.net;
        let sinks_ok = self.sinks.clone();
        let sinks_err = self.sinks.clone();
        spawn_decode_then_with_err(
            frame,
            move |msg| on_heavy_decoded(id, net, &sinks_ok, msg),
            move |err| on_heavy_err(id, &sinks_err, err),
        );
        false
    }

    #[doc(hidden)]
    pub fn dispatch_err(&mut self, err: NetError) -> bool {
        match err {
            NetError::InvalidV2Type { contents_len } => self.on_invalid_v2(contents_len),
            NetError::Io(err) if is_eof_or_reset(&err) => {
                self.sinks.send_body(PeerEvent::Dead {
                    peer: self.id,
                    reason: format!("eof: {err}"),
                });
                true
            }
            other => {
                self.sinks.send_body(PeerEvent::Dead {
                    peer: self.id,
                    reason: other.to_string(),
                });
                true
            }
        }
    }

    #[doc(hidden)]
    pub fn on_invalid_v2(&mut self, contents_len: usize) -> bool {
        if first_invalid_v2(&mut self.logged_invalid_v2) {
            rbitcoin_log::debug!(P2p, "{}", rbitcoin_net::v2::v2_invalid_message_type_log());
        }
        self.score_overflow(contents_len)
    }

    #[doc(hidden)]
    pub fn score_overflow(&mut self, n: usize) -> bool {
        if misbehavior_disconnect(&mut self.rate, &mut self.ban_score, n) {
            send_threshold_dead(&self.sinks, self.id);
            true
        } else {
            false
        }
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn read_ibd_peer(
    id: usize,
    net: rbitcoin_net::NetAddr,
    magic: Magic,
    mut reader: V2Reader,
    out_tx: mpsc::UnboundedSender<NetworkMessage>,
    sinks_r: PeerEventSinks,
    bytes_io: Arc<AtomicU64>,
    requested: Arc<Mutex<super::state::BlockHashSet>>,
    headers_asks: Arc<HeaderAsks>,
    solicited_bytes: Arc<AtomicU64>,
    open_block_bytes: Arc<AtomicU64>,
    any_rx_ms: Arc<AtomicU64>,
    solicited_ms: Arc<AtomicU64>,
    light_decode: Arc<Semaphore>,
    pong: Arc<PongSlot>,
) {
    let mut prog_mark = 0usize;
    let requested_progress = Arc::clone(&requested);
    let mut ctx = IbdReadCtx {
        id,
        net,
        out_tx,
        sinks: sinks_r,
        requested,
        headers_asks,
        solicited_bytes,
        solicited_ms,
        light_decode,
        light: LightDecodes { tasks: Vec::new() },
        rate: PeerRateLimiter::default_limits(),
        logged_invalid_v2: false,
        ban_score: 0,
        pong: Arc::clone(&pong),
    };
    reader.enable_ibd_ciphertext_buf();
    loop {
        let frame = read_v2_frame_with_progress(
            &mut reader,
            magic,
            |buffered, contents_len| {
                note_read_progress(&bytes_io, &any_rx_ms, &mut prog_mark, buffered);
                note_open_block(
                    &open_block_bytes,
                    &requested_progress,
                    contents_len,
                    buffered,
                );
            },
            |n| {
                open_block_bytes.store(0, Ordering::Relaxed);
                decoy_hook(&mut ctx.rate, &mut ctx.ban_score, n)
            },
        )
        .await;
        // The progress mark is per read. The next frame starts at zero.
        prog_mark = 0;
        let stop = ctx.handle(frame);
        open_block_bytes.store(0, Ordering::Relaxed);
        if stop {
            break;
        }
    }
}

/// Writes one IBD pong nonce. The future may borrow `self` until it is awaited.
#[doc(hidden)]
pub trait IbdPongEmit {
    /// `Send` so the spawned IBD writer can await the write.
    #[allow(clippy::manual_async_fn)]
    fn emit(
        &mut self,
        nonce: u64,
    ) -> impl std::future::Future<Output = Result<(), NetError>> + Send + '_;
}

/// Write the queued pong and every nonce that arrives during a write.
///
/// The pending slot is clear when this returns `Ok`.
#[doc(hidden)]
pub async fn write_ibd_pong<E: IbdPongEmit>(
    pong: &PongSlot,
    queued: u64,
    emit: &mut E,
) -> Result<(), NetError> {
    let mut nonce = pong.claimed_nonce_or(queued);
    loop {
        emit.emit(nonce).await?;
        match pong.finish(nonce) {
            Some(extra) => nonce = extra,
            None => return Ok(()),
        }
    }
}

struct IbdPongWriter<'a> {
    writer: &'a mut V2Writer,
}

impl IbdPongEmit for IbdPongWriter<'_> {
    #[allow(clippy::manual_async_fn)]
    fn emit(
        &mut self,
        nonce: u64,
    ) -> impl std::future::Future<Output = Result<(), NetError>> + Send + '_ {
        async move { write_v2_msg_offload(self.writer, NetworkMessage::Pong(nonce)).await }
    }
}

pub async fn write_ibd_peer(
    id: usize,
    mut cmd_rx: mpsc::UnboundedReceiver<PeerCmd>,
    mut out_rx: mpsc::UnboundedReceiver<NetworkMessage>,
    mut writer: V2Writer,
    sinks_w: PeerEventSinks,
    wire: Arc<GetdataWire>,
    pong: Arc<PongSlot>,
) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(PeerCmd::GetHeaders { locator }) => {
                        let locator = if locator.len() > rbitcoin_net::codec::MAX_LOCATOR_SZ {
                            locator[..rbitcoin_net::codec::MAX_LOCATOR_SZ].to_vec()
                        } else {
                            locator
                        };
                        let gh = GetHeadersMessage::new(
                            locator,
                            BlockHash::from_byte_array([0u8; 32]),
                        );
                        if write_v2_msg_offload(
                            &mut writer,
                            NetworkMessage::GetHeaders(gh),
                        )
                        .await
                        .is_err()
                        {
                            sinks_w.send_body(PeerEvent::Dead {
                                peer: id,
                                reason: "write getheaders failed".into(),
                            });
                            break;
                        }
                    }
                    Some(PeerCmd::GetData { hashes }) => {
                        for chunk in hashes.chunks(MAX_INV_SIZE) {
                            let inv: Vec<_> = chunk
                                .iter()
                                .copied()
                                .map(Inventory::WitnessBlock)
                                .collect();
                            if inv.is_empty() {
                                continue;
                            }
                            if write_v2_msg_offload(
                                &mut writer,
                                NetworkMessage::GetData(inv),
                            )
                            .await
                            .is_err()
                            {
                                sinks_w.send_body(PeerEvent::Dead {
                                    peer: id,
                                    reason: "write getdata failed".into(),
                                });
                                return;
                            }
                            wire.wrote(chunk.len(), ibd_mono_ms());
                        }
                    }
                    Some(PeerCmd::Shutdown) | None => break,
                }
            }
            msg = out_rx.recv() => {
                match msg {
                    Some(payload) => {
                        let failed = match payload {
                            NetworkMessage::Pong(queued) => {
                                let mut emit = IbdPongWriter {
                                    writer: &mut writer,
                                };
                                write_ibd_pong(&pong, queued, &mut emit).await.is_err()
                            }
                            other => write_v2_msg_offload(&mut writer, other).await.is_err(),
                        };
                        if failed {
                            sinks_w.send_body(PeerEvent::Dead {
                                peer: id,
                                reason: "write outbound failed".into(),
                            });
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

/// Aborts both halves when the parent task is cancelled. A dropped
/// `JoinHandle` only detaches, so the read half would keep the socket.
#[doc(hidden)]
pub struct PeerIoTasks {
    pub reader: tokio::task::JoinHandle<()>,
    /// Set once the writer spawns; the guard is created with the
    /// reader alone so an abort before that still aborts the reader.
    pub writer: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PeerIoTasks {
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(w) = self.writer.as_ref() {
            w.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn spawn_peer(
    id: usize,
    addr: rbitcoin_net::NetAddr,
    magic: Magic,
    local: SocketAddr,
    tip_h: Option<u32>,
    sinks: PeerEventSinks,
    dialer: rbitcoin_net::socks::Dialer,
    perms: Option<Arc<rbitcoin_net::peers::PeerHub>>,
    conn_type: rbitcoin_net::peers::PeerConnType,
) -> Result<PeerSlot, NetError> {
    if perms.as_ref().is_some_and(|p| p.net_discouraged(addr)) {
        return Err(NetError::Protocol("discouraged"));
    }
    if perms.as_ref().is_some_and(|p| p.net_banned(addr)) {
        return Err(NetError::Protocol("banned"));
    }
    if let Some(p) = perms.as_ref() {
        if let Err(msg) = p.trust_allows_net(addr) {
            return Err(NetError::Consensus(msg));
        }
    }
    let stream = dialer.connect_net(addr).await?;
    let version_socket = addr
        .socket_addr()
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], addr.port())));
    let ua = rbitcoin_primitives::rbitcoin_subversion(env!("CARGO_PKG_VERSION"), &[] as &[&str])
        .unwrap_or_else(|_| format!("/rbitcoin:{}/", env!("CARGO_PKG_VERSION")));
    let (ver, reader, writer, _wire, _tcp_shutdown) = connect_and_handshake_timed(
        HANDSHAKE_TIMEOUT,
        stream,
        magic,
        local,
        version_socket,
        tip_h.map(|h| h as i32).unwrap_or(0),
        false,
        &ua,
        HandshakePolicy::plain(),
    )
    .await?;
    let peer_height = u32::try_from(ver.start_height).unwrap_or(0);
    let connected_ms = ibd_mono_ms();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<PeerCmd>();
    // Reader → writer for pongs (must not write on the read task — that would
    // stall the receive half and look like a peer stall).
    let (out_tx, out_rx) = mpsc::unbounded_channel::<NetworkMessage>();
    let pong = Arc::new(PongSlot::new());
    let pong_r = Arc::clone(&pong);
    let bytes_rx_total = Arc::new(AtomicU64::new(0));
    let bytes_io = Arc::clone(&bytes_rx_total);
    let (requested, solicited_bytes, solicited_ms) = solicit_track();
    let headers_asks = HeaderAsks::new();
    let open_block_bytes = Arc::new(AtomicU64::new(0));
    let any_rx_ms = Arc::new(AtomicU64::new(0));
    let getdata_wire = GetdataWire::new();
    let wire_w = Arc::clone(&getdata_wire);
    let req_r = Arc::clone(&requested);
    let headers_asks_r = Arc::clone(&headers_asks);
    let sol_bytes_r = Arc::clone(&solicited_bytes);
    let open_block_r = Arc::clone(&open_block_bytes);
    let any_rx_r = Arc::clone(&any_rx_ms);
    let sol_ms_r = Arc::clone(&solicited_ms);
    let light_decode = Arc::new(Semaphore::new(LIGHT_DECODE_PERMITS));

    // Parent owns concurrent read + write tasks. Aborting the parent (PeerSlot
    // Drop / stall disconnect) must abort both children — plain JoinHandle drop
    // only detaches.
    let task = tokio::spawn(async move {
        // Reader first: Core pipelines sendheaders/sendcmpct/… right after verack.
        // July 18 cold-start worked with **no** post-handshake getaddr/sendaddrv2
        // before getheaders; those writes raced Core's pipeline and peers closed
        // (ordered=0 / inflight=0 / never archive).
        let sinks_r = sinks.clone();
        let reader_task = tokio::spawn(read_ibd_peer(
            id,
            addr,
            magic,
            reader,
            out_tx,
            sinks_r,
            bytes_io,
            req_r,
            headers_asks_r,
            sol_bytes_r,
            open_block_r,
            any_rx_r,
            sol_ms_r,
            light_decode,
            pong_r,
        ));

        // Guard the reader before the yield: aborting this task while
        // it is suspended there must abort the reader too, not detach
        // it with the socket still open.
        let mut guard = PeerIoTasks {
            reader: reader_task,
            writer: None,
        };

        // Let the reader poll once before we accept write work (getheaders).
        tokio::task::yield_now().await;

        let sinks_w = sinks;
        let writer_task = tokio::spawn(write_ibd_peer(
            id, cmd_rx, out_rx, writer, sinks_w, wire_w, pong,
        ));
        guard.writer = Some(writer_task);

        let writer = guard.writer.as_mut().expect("writer spawned above");
        tokio::select! {
            _ = &mut guard.reader => {}
            _ = writer => {}
        }
    });

    Ok(PeerSlot {
        id,
        addr: version_socket,
        net: addr,
        cmd_tx,
        in_flight: Default::default(),
        requested,
        headers_asks,
        solicited_bytes,
        open_block_bytes,
        solicited_ms,
        any_rx_ms,
        getdata_wire,
        peer_height,
        connected_ms,
        first_data_ms: 0,
        bytes_rx_total,
        rate: {
            let mut rate = PeerRate::default();
            rate.note_connected(connected_ms);
            rate
        },
        chain: Default::default(),
        conn_type,
        block_download_paused_until: None,
        late_answers: Default::default(),
        alive: true,
        task,
    })
}

/// IPv4/IPv6 rows that advertise full/limited network **and** `P2P_V2`.
#[doc(hidden)]
pub fn net_addrs_from_addr(
    list: &[(u32, bitcoin::p2p::address::Address)],
) -> Vec<(u32, rbitcoin_net::NetAddr)> {
    socket_addrs_from_addr(list)
        .into_iter()
        .map(|(time, sa)| (time, rbitcoin_net::NetAddr::from_socket(sa)))
        .collect()
}

/// BIP155 rows that advertise full/limited network **and** `P2P_V2`, including onion.
#[doc(hidden)]
pub fn net_addrs_from_addrv2(
    list: &[bitcoin::p2p::address::AddrV2Message],
) -> Vec<(u32, rbitcoin_net::NetAddr)> {
    let mut out = Vec::with_capacity(list.len().min(32));
    for a in list {
        if !services_useful_for_ibd(a.services) {
            continue;
        }
        let Some(addr) = rbitcoin_net::NetAddr::from_addrv2(a) else {
            continue;
        };
        match addr {
            rbitcoin_net::NetAddr::Ip(sa) if !usable_dial_addr(&sa) => {}
            ok => out.push((a.time, ok)),
        }
    }
    out
}

/// IPv4/IPv6 sockets that advertise full/limited network **and** `P2P_V2`,
/// with the time each row claimed.
#[doc(hidden)]
pub fn socket_addrs_from_addr(
    list: &[(u32, bitcoin::p2p::address::Address)],
) -> Vec<(u32, SocketAddr)> {
    let mut out = Vec::with_capacity(list.len().min(32));
    for (ts, a) in list {
        if !services_useful_for_ibd(a.services) {
            continue;
        }
        if let Ok(sa) = a.socket_addr() {
            if usable_dial_addr(&sa) {
                out.push((*ts, sa));
            }
        }
    }
    out
}

#[doc(hidden)]
pub fn services_useful_for_ibd(flags: ServiceFlags) -> bool {
    (flags.has(ServiceFlags::NETWORK) || flags.has(ServiceFlags::NETWORK_LIMITED))
        && flags.has(ServiceFlags::P2P_V2)
}

#[doc(hidden)]
pub fn usable_dial_addr(sa: &SocketAddr) -> bool {
    if sa.port() == 0 {
        return false;
    }
    match sa {
        SocketAddr::V4(v4) => {
            let ip = *v4.ip();
            !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()
        }
        SocketAddr::V6(v6) => {
            let ip = *v6.ip();
            !ip.is_unspecified() && !ip.is_multicast()
        }
    }
}
