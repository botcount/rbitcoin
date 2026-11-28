//! Peer handshake, serve, tip follow, and announce (BIP324 v2 transport).

use crate::cache::BlockCache;
use crate::chain::{
    accept_block_header_nodos_log, accept_prev_not_found_log, ignoring_low_work_chain_log,
    received_getdata_wtx_log, received_tx_log, synchronizing_blockheaders_log, AcceptOutcome,
    BlockAsk, ChainHub,
};
use crate::codec::{
    FramedMessage, MAX_HEADERS_RESULTS, MAX_INV_SIZE, MAX_LOCATOR_SZ, TX_INV_BATCH,
};
use crate::error::NetError;
use crate::msg_decode::decode_framed_offload;
use crate::peer_dos::{PeerRateLimiter, OVERSIZE_BAN_SCORE, RATE_LIMIT_BAN_SCORE};
use crate::peers::{CappedSet, PeerOut};
use crate::send_queue::{PeerOutRx, PeerOutTx, Queued};
use crate::v1::{open_inbound_with_wire, FrameRead, FrameWrite, PeerReader, PeerWriter};
use crate::v2::{
    open_v2, open_v2_with_wire, read_v2_contents, read_v2_frame, write_v2_contents, write_v2_msg,
    V2Reader, V2Writer,
};
use bitcoin::bip152::{BlockTransactions, BlockTransactionsRequest, HeaderAndShortIds};
use bitcoin::hashes::Hash;
use bitcoin::p2p::address::{AddrV2Message, Address};
use bitcoin::p2p::message::{NetworkMessage, RawNetworkMessage};
use bitcoin::p2p::message_blockdata::{GetBlocksMessage, GetHeadersMessage, Inventory};
use bitcoin::p2p::message_compact_blocks::{BlockTxn, CmpctBlock, GetBlockTxn, SendCmpct};
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::{Magic, ServiceFlags, PROTOCOL_VERSION};
use bitcoin::{Block, BlockHash, Transaction};
use rbitcoin_primitives::Height;
use rbitcoin_query::Query;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::sync::watch;

/// Protocol version we advertise (BIP339 wtxidrelay needs ≥70016; rust-bitcoin's
/// `PROTOCOL_VERSION` is still 70001).
#[doc(hidden)]
pub const OUR_PROTOCOL_VERSION: u32 = 70016;

/// Core `WTXID_RELAY_VERSION`: BIP339 `wtxidrelay` goes both ways only from
/// this peer version up.
#[doc(hidden)]
pub const WTXID_RELAY_VERSION: u32 = 70016;

/// How often an established session re-issues `getheaders` so a quiet peer or
/// a gap opened while we were offline still gets filled (signet ~10m blocks).
#[doc(hidden)]
pub const HEADERS_POLL_SECS: u64 = 120;

#[doc(hidden)]
pub const SESSION_HEARTBEAT: Duration = Duration::from_millis(50);

async fn session_heartbeat_due(rx: &mut Option<watch::Receiver<u64>>, wait: Duration) {
    tokio::select! {
        biased;
        _ = async {
            match rx.as_mut() {
                Some(rx) => {
                    let _ = rx.changed().await;
                }
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = tokio::time::sleep(wait) => {}
    }
}

/// Addr-fetch sessions expire after this many seconds.
#[doc(hidden)]
pub const ADDRFETCH_TIMEOUT_SECS: u64 = 300;

/// On a reorg larger than this, announce via inv instead of a header list.
#[doc(hidden)]
pub const MAX_BLOCKS_TO_ANNOUNCE: u32 = 8;

/// Quiet period before announcing a reorg tip. A direct child of the header we
/// already sent goes out immediately. `p2p_sendheaders` part 3 needs the whole
/// reorg in one inv (nine blocks) or one headers list (eight).
#[doc(hidden)]
pub const TIP_ANNOUNCE_BATCH: Duration = Duration::from_millis(200);

/// True when a session error is a missing store row (not peer malice / corrupt IO).
///
/// These must not tear down the TCP session: re-request or skip and keep the peer.
pub fn net_error_is_store_not_found(e: &NetError) -> bool {
    match e {
        NetError::Consensus(s) | NetError::Store(s) => {
            let l = s.to_ascii_lowercase();
            l.contains("record not found")
                || l.contains("not found")
                || l.contains("storeerror::notfound")
        }
        _ => false,
    }
}

/// Per-session misbehavior score that triggers disconnect.
pub const BAN_SCORE_THRESHOLD: u32 = 100;

pub fn misbehavior_disconnect_log(peer: &str, score: u32) -> String {
    format!("p2p: {peer} misbehavior {score} ≥ {BAN_SCORE_THRESHOLD} — disconnect")
}

/// `-blocksonly` (relay off, no whitelist `relay`) or a block-relay-only
/// session must not receive txs / tx invs (`p2p_blocksonly`).
#[doc(hidden)]
pub fn reject_unsolicited_tx(hub: &ChainHub, session: Option<&crate::peers::LivePeer>) -> bool {
    if hub.in_ibd() {
        return false;
    }
    if session.is_some_and(|s| s.conn_type == crate::peers::PeerConnType::BlockRelay) {
        return true;
    }
    let node_relay = hub.mempool().is_none_or(|m| m.relay_enabled());
    if node_relay {
        return false;
    }
    !session.is_some_and(|s| s.session_relay_perm())
}

/// Reconstruct compact blocks when this node is not `-blocksonly` and
/// either it has no mempool or it is accepting transactions.
///
/// High-bandwidth selection and compact `getdata` use this. A tip
/// announcement does not: this node already has the block, and the peer
/// fills short ids. Coinbase is the prefill when there is nothing to
/// guess from.
#[doc(hidden)]
pub fn compact_blocks_ok(hub: &ChainHub) -> bool {
    if hub.blocks_only.load(Ordering::Acquire) {
        return false;
    }
    match hub.mempool() {
        None => true,
        Some(mp) => mp.relay_enabled(),
    }
}

/// A connect that leaves IBD after the scripthash pack starts peer tx relay
/// before compact selection. The tip-follow loop may not have run yet.
/// Direct mode is the pack: relay stays off. A purge fault leaves the bit
/// clear and sends no fee filter.
fn start_relay_after_tip_connect(hub: &ChainHub, peers: Option<&crate::peers::PeerHub>) {
    let Some(peers) = peers else {
        return;
    };
    let Some(mp) = hub.mempool() else {
        return;
    };
    if !hub.query.index_mode().is_ok_and(|mode| mode.is_tip()) {
        return;
    }
    let want =
        hub.meets_minimum_chain_work() && !hub.blocks_only.load(Ordering::Acquire) && !hub.in_ibd();
    if !want || mp.relay_enabled() {
        return;
    }
    mp.set_relay_enabled(true);
    if !mp.relay_enabled() {
        return;
    }
    peers.queue_feefilter_all(hub.feefilter_sat_kvb() as i64);
    rbitcoin_log::info!("ibd: leaving IBD — enabling tx relay");
}

/// BIP152 HB is only for peers we will take compact blocks from.
#[doc(hidden)]
pub fn maybe_select_hb_if_relay(hub: &ChainHub, session: Option<&crate::peers::LivePeer>) {
    if !compact_blocks_ok(hub) {
        return;
    }
    let Some(s) = session else {
        return;
    };
    // Compact-block reconstruction stays. The trusted session is not asked
    // for high-bandwidth announcements.
    if s.peer_hub().is_some_and(|ph| ph.session_is_trusted_peer(s)) {
        return;
    }
    s.maybe_select_as_hb();
}

#[doc(hidden)]
pub fn note_accepted_hb(accepted: bool, hb: &Option<(std::sync::Arc<crate::peers::PeerHub>, u64)>) {
    if accepted {
        if let Some((ph, id)) = hb {
            if ph.get(*id).is_some_and(|s| ph.session_is_trusted_peer(&s)) {
                return;
            }
            ph.maybe_select_hb(*id);
        }
    }
}

pub async fn accept_received_from_peer(
    hub: &ChainHub,
    block: Block,
    session: Option<&crate::peers::LivePeer>,
) -> Result<AcceptOutcome, NetError> {
    let announced = block.block_hash();
    let waive =
        session.is_some_and(|s| s.peer_hub().is_some_and(|ph| ph.session_is_trusted_peer(s)));
    // Relay may flip inside this connect once the index is in tip mode.
    // Compact selection reads that bit afterwards.
    let peers = session.and_then(|s| s.peer_hub());
    let hb = session.and_then(|s| s.peer_hub().map(|ph| (ph, s.id)));
    // The tip is visible before this await returns. Stamp the clock from
    // before the wait so a caller that moves time once it sees the tip does
    // not extend the post-block ping grace.
    let received_at = session.map(|s| s.clock_now());
    // Park the body before the lane job so an equal-work sibling that runs
    // first can race it (T1). A connect removes it; a loser stays held.
    let was_held = hub.held_body(&announced).is_some();
    let was_connected = hub.is_connected(&announced);
    let block = std::sync::Arc::new(block);
    hub.park_body(std::sync::Arc::clone(&block), waive);
    let r = if let Some(hub) = hub.shared_arc() {
        crate::tip_accept::run_on_tip_accept_async(move || {
            let r = hub.accept_received_arc_on_lane(block, waive);
            start_relay_after_tip_connect(&hub, peers.as_deref());
            let chosen = if compact_blocks_ok(&hub) { hb } else { None };
            let credited = match &r {
                Ok(AcceptOutcome::Accepted { .. }) => true,
                // The pair can connect this hash before its own job runs.
                // Credit the delivery that got it connected, not a later
                // duplicate of an already-connected block.
                Ok(AcceptOutcome::AlreadyHave) => !was_connected && hub.is_connected(&announced),
                _ => false,
            };
            note_accepted_hb(credited, &chosen);
            r
        })
        .await
    } else {
        // A stack hub (unit tests) has no `Arc`. Join before return so the
        // borrow outlives the job.
        crate::tip_accept::run_on_tip_accept(|| {
            let r = hub.accept_received_arc_on_lane(block.clone(), waive);
            start_relay_after_tip_connect(hub, peers.as_deref());
            let chosen = if compact_blocks_ok(hub) { hb } else { None };
            let credited = match &r {
                Ok(AcceptOutcome::Accepted { .. }) => true,
                // The pair can connect this hash before its own job runs.
                // Credit the delivery that got it connected, not a later
                // duplicate of an already-connected block.
                Ok(AcceptOutcome::AlreadyHave) => !was_connected && hub.is_connected(&announced),
                _ => false,
            };
            note_accepted_hb(credited, &chosen);
            r
        })
    };
    if matches!(r, Err(NetError::Store(_))) && !was_held {
        hub.drop_held(announced);
    }
    if drop_servable_after_accept(hub, &announced, &r) {
        hub.cache.drop_servable(&announced);
    }
    let credited = match &r {
        Ok(AcceptOutcome::Accepted { .. }) => true,
        Ok(AcceptOutcome::AlreadyHave) => !was_connected && hub.is_connected(&announced),
        _ => false,
    };
    if let (Some(s), Some(at), true) = (session, received_at, credited) {
        s.note_last_block_at(at);
    }
    r
}

/// Drop the staged body only once this hash is in the store, or the block
/// was rejected for good. `Accepted` can be a different held branch.
/// `UnknownParent` and a lost tip race come back as
/// [`AcceptOutcome::IgnoredWeaker`]. A not-yet-valid timestamp is not a
/// permanent reject, and the store still has no body.
fn drop_servable_after_accept(
    hub: &ChainHub,
    announced: &bitcoin::BlockHash,
    r: &Result<AcceptOutcome, NetError>,
) -> bool {
    match r {
        Ok(AcceptOutcome::AlreadyHave) => true,
        Ok(AcceptOutcome::Accepted { .. }) => store_has_announced_body(hub, announced),
        Ok(AcceptOutcome::IgnoredWeaker) => false,
        Err(e) if keep_staged_body_after_accept_err(hub, e) => false,
        Err(_) => true,
    }
}

/// Not a verdict on the staged bytes. `Mutated` stays dropped: `servable`
/// is read before the store, so those transactions would answer every later
/// `getdata` for this hash.
fn keep_staged_body_after_accept_err(hub: &ChainHub, e: &NetError) -> bool {
    if e.is_mutated() {
        return false;
    }
    net_error_needs_parent(e)
        || matches!(e, NetError::Store(_) | NetError::Cancelled)
        || net_error_is_store_not_found(e)
        || crate::chain::accept_err_is_temporary_time(e)
        || matches!(e, NetError::Consensus(m) if m == "bad-prevblk")
        || (hub.query.trust_peer() && crate::chain::accept_err_is_warn_and_apply(e))
}

fn store_has_announced_body(hub: &ChainHub, hash: &bitcoin::BlockHash) -> bool {
    if hub.has_block(hash) {
        return true;
    }
    hub.query
        .reconstruct_block_by_hash(&hash.to_byte_array())
        .ok()
        .flatten()
        .is_some()
}

/// The hub and key a misbehavior exit may record. Noban and manual peers
/// are not recorded. A loopback or `0.0.0.0` identity is every local
/// connection, so Core disconnects that peer and does not discourage it.
/// A marked onion listener can likewise see one TCP source for many peers.
/// Onion and I2P use [`crate::peers::LivePeer::net`], not that socket.
#[doc(hidden)]
pub fn refusable_addr(
    session: Option<&crate::peers::LivePeer>,
) -> Option<(Arc<crate::peers::PeerHub>, crate::peers::DiscourageKey)> {
    let s = session.filter(|s| !misbehavior_exempt(s))?;
    let key = crate::peers::DiscourageKey::from_net(s.net)?;
    let hub = s.peer_hub()?;
    if s.inbound
        && matches!(key, crate::peers::DiscourageKey::Ip(_))
        && hub.onion_bind() == Some(s.addrbind)
    {
        return None;
    }
    Some((hub, key))
}

/// One-day in-memory refusal.
#[doc(hidden)]
pub fn note_threshold_refusal(session: Option<&crate::peers::LivePeer>) {
    if let Some((hub, key)) = refusable_addr(session) {
        hub.note_misbehavior_key(key);
    }
}

/// Disconnect error for a score that is already at the misbehavior line.
/// Records the address, including when the caller is not [`punish_disconnect`].
#[doc(hidden)]
pub fn threshold_disconnect(session: Option<&crate::peers::LivePeer>) -> NetError {
    note_threshold_refusal(session);
    NetError::Protocol("peer misbehavior threshold")
}

/// The first rate-limit exit from a recordable address leaves a strike. A
/// second one within [`crate::peers::PeerHub::RATE_STRIKE_SECS`] is a
/// threshold exit, as two overflows in one session were.
#[doc(hidden)]
pub fn rate_limit_disconnect(session: Option<&crate::peers::LivePeer>) -> NetError {
    if refusable_addr(session).is_some_and(|(hub, key)| hub.note_rate_strike(key)) {
        return threshold_disconnect(session);
    }
    NetError::Protocol("peer rate limit")
}

/// Count one tip-follow frame in the session's rate window. The first
/// `block` frame for a hash this session asked for stays off it, as in IBD:
/// the getdata window already bounds those bytes, and a full window of large
/// bodies served at link speed is more than a second of budget. One ask, one
/// unmetered frame. This runs before decode, so a resend while the ask is
/// still open counts.
#[doc(hidden)]
pub fn note_frame_rate(
    rate: &mut PeerRateLimiter,
    follow: &mut PeerFollowState,
    frame: &FramedMessage,
) -> bool {
    let requested = &follow.requested_blocks;
    follow.unmetered_bodies.retain(|h| requested.contains(h));
    let first_body = frame
        .block_hash_from_header()
        .is_some_and(|h| requested.contains(&h) && follow.unmetered_bodies.insert(h));
    first_body || rate.note(frame.payload_len())
}

/// Heavy frames (`block`, `headers`, `notfound`) already read while a decode
/// permit is held. Tip-follow asks for [`MAX_SERVE_BLOCKS`] bodies, and a
/// Core versionbits test sends a period of unsolicited bodies (144), so the
/// cap leaves room for both without treating that burst as a flood. Cheap
/// frames (`tx`, `inv`, ping) use [`DECODE_Q_MAX_CHEAP_FRAMES`], so a burst
/// behind one block is not a disconnect. Heavy bytes also share a process
/// cap of the blocking-pool size times [`DECODE_Q_MAX_BYTES`].
pub const DECODE_Q_MAX_FRAMES: usize = 256;
pub const DECODE_Q_MAX_CHEAP_FRAMES: usize = 1024;
pub const DECODE_Q_MAX_BYTES: usize =
    (MAX_SERVE_BLOCKS + 1) * crate::codec::MAX_PROTOCOL_MESSAGE_LENGTH;

/// `true` when this queue, after accepting the frame, is over a cap.
/// Comparison is `>`, so a queue sitting on the cap still fits.
fn decode_queue_over_cap(
    heavy_frames: usize,
    heavy_bytes: usize,
    cheap_frames: usize,
    global_heavy: usize,
    global_cap: usize,
) -> bool {
    heavy_frames > DECODE_Q_MAX_FRAMES
        || cheap_frames > DECODE_Q_MAX_CHEAP_FRAMES
        || heavy_bytes > DECODE_Q_MAX_BYTES
        || global_heavy > global_cap
}

fn decode_q_global_byte_cap() -> usize {
    crate::msg_decode::blocking_pool_size().saturating_mul(DECODE_Q_MAX_BYTES)
}

/// Heavy bytes queued for decode in this process. Pop and drop subtract.
static DECODE_Q_HEAVY_BYTES: AtomicUsize = AtomicUsize::new(0);

fn add_decode_q_heavy(n: usize) {
    if n == 0 {
        return;
    }
    DECODE_Q_HEAVY_BYTES.fetch_add(n, Ordering::Relaxed);
}

fn sub_decode_q_heavy(n: usize) {
    if n == 0 {
        return;
    }
    let _ = DECODE_Q_HEAVY_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        Some(cur.saturating_sub(n))
    });
}

/// Disconnect a peer that queued more heavy frames than
/// [`DECODE_Q_MAX_FRAMES`], more cheap frames than
/// [`DECODE_Q_MAX_CHEAP_FRAMES`], or more heavy bytes than the per-peer
/// window or the process cap.
#[doc(hidden)]
pub fn decode_queue_disconnect(session: Option<&crate::peers::LivePeer>) -> NetError {
    if let Some(s) = session {
        s.request_disconnect();
    }
    NetError::Protocol("decode queue full")
}

type DecodeRx = tokio::sync::oneshot::Receiver<Result<RawNetworkMessage, NetError>>;

struct QueuedDecode {
    rx: DecodeRx,
    n: usize,
    heavy: bool,
}

struct DecodeQueue {
    q: VecDeque<QueuedDecode>,
    heavy_frames: usize,
    cheap_frames: usize,
    heavy_bytes: usize,
}

impl DecodeQueue {
    fn new() -> Self {
        Self {
            q: VecDeque::new(),
            heavy_frames: 0,
            cheap_frames: 0,
            heavy_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    fn front_mut(&mut self) -> Option<&mut DecodeRx> {
        self.q.front_mut().map(|item| &mut item.rx)
    }

    fn front_len(&self) -> usize {
        self.q.front().map(|item| item.n).unwrap_or(0)
    }

    fn front_heavy(&self) -> bool {
        self.q.front().is_some_and(|item| item.heavy)
    }

    fn account(&mut self, n: usize, heavy: bool) {
        if heavy {
            self.heavy_frames = self.heavy_frames.saturating_add(1);
            self.heavy_bytes = self.heavy_bytes.saturating_add(n);
            add_decode_q_heavy(n);
            return;
        }
        self.cheap_frames = self.cheap_frames.saturating_add(1);
    }

    fn release(&mut self, n: usize, heavy: bool) {
        if heavy {
            self.heavy_frames = self.heavy_frames.saturating_sub(1);
            self.heavy_bytes = self.heavy_bytes.saturating_sub(n);
            sub_decode_q_heavy(n);
            return;
        }
        self.cheap_frames = self.cheap_frames.saturating_sub(1);
    }

    fn pop_front(&mut self) -> Option<DecodeRx> {
        let item = self.q.pop_front()?;
        self.release(item.n, item.heavy);
        Some(item.rx)
    }

    /// Put a decoded frame back at the front. The caller already counted
    /// these bytes, so this does not re-check the cap.
    fn push_front_ready(
        &mut self,
        msg: Result<RawNetworkMessage, NetError>,
        n: usize,
        heavy: bool,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(msg);
        self.q.push_front(QueuedDecode { rx, n, heavy });
        self.account(n, heavy);
    }

    /// Queue one frame already taken off the socket. `heavy` matches the
    /// spawn decision (`block`, `headers`, `notfound`). `true` when the peer
    /// has now passed a cap and must be disconnected after a drain.
    fn push(
        &mut self,
        rx: DecodeRx,
        n: usize,
        heavy: bool,
        session: Option<&crate::peers::LivePeer>,
    ) -> bool {
        self.q.push_back(QueuedDecode { rx, n, heavy });
        self.account(n, heavy);
        #[cfg(debug_assertions)]
        if let Some(session) = session {
            session.note_decode_enqueued();
        }
        #[cfg(not(debug_assertions))]
        let _ = session;
        decode_queue_over_cap(
            self.heavy_frames,
            self.heavy_bytes,
            self.cheap_frames,
            DECODE_Q_HEAVY_BYTES.load(Ordering::Relaxed),
            decode_q_global_byte_cap(),
        )
    }
}

impl Drop for DecodeQueue {
    fn drop(&mut self) {
        sub_decode_q_heavy(self.heavy_bytes);
        self.heavy_bytes = 0;
    }
}

/// Block, compact block, and blocktxn still apply on the way out. Anything
/// else (ping, tx, getheaders) must not run after the peer is leaving or
/// while the send budget is spent.
fn queued_msg_is_block_body(msg: &RawNetworkMessage) -> bool {
    matches!(
        msg.payload(),
        NetworkMessage::Block(_) | NetworkMessage::CmpctBlock(_) | NetworkMessage::BlockTxn(_)
    )
}

/// Core `MaybeDiscourageAndDisconnect`: misbehavior never disconnects or
/// discourages a noban peer or a manual one (`--connect`, `addnode`).
#[doc(hidden)]
pub fn misbehavior_exempt(s: &crate::peers::LivePeer) -> bool {
    s.session_noban() || s.conn_type == crate::peers::PeerConnType::Manual
}

/// Core `Misbehaving`. Unlike [`punish_disconnect`], it also keeps a manual
/// peer connected.
#[doc(hidden)]
pub fn misbehaving(ban_score: &mut u32, session: Option<&crate::peers::LivePeer>) {
    if let Some(s) =
        session.filter(|s| s.conn_type == crate::peers::PeerConnType::Manual && !s.session_noban())
    {
        rbitcoin_log::info!("Warning: not punishing manually connected peer {}!", s.id);
        return;
    }
    punish_disconnect(ban_score, session);
}

/// Disconnects and refuses the address; a noban peer is kept. Used where Core
/// sets `fDisconnect`, which drops a manual peer too. Core `Misbehaving`
/// sites call [`misbehaving`].
#[doc(hidden)]
pub fn punish_disconnect(ban_score: &mut u32, session: Option<&crate::peers::LivePeer>) {
    if let Some(s) = session.filter(|s| s.session_noban()) {
        rbitcoin_log::info!("Warning: not punishing noban peer {}!", s.id);
        return;
    }
    *ban_score = ban_score.saturating_add(BAN_SCORE_THRESHOLD);
    if let Some(s) = session {
        s.request_disconnect();
        note_threshold_refusal(Some(s));
    }
}

/// Core `MaybeDiscourageAndDisconnect`: misbehavior never drops a noban or
/// manual peer.
#[doc(hidden)]
pub fn misbehavior_disconnects(ban_score: u32, session: Option<&crate::peers::LivePeer>) -> bool {
    ban_score >= BAN_SCORE_THRESHOLD && !session.is_some_and(misbehavior_exempt)
}
/// Cap on incomplete compact blocks awaiting `blocktxn` (DoS).
#[doc(hidden)]
pub const MAX_PENDING_CMPCT: usize = 1;
/// Cap on headers held while assembling tip/reorg work (DoS / process RAM).
#[doc(hidden)]
pub const MAX_PENDING_HEADERS: usize = 8_000;
/// One more step than a full pending map plus one `headers` batch, so the
/// walk can still see genesis or our tip after the last header.
#[doc(hidden)]
pub const PENDING_WALK_LIMIT: usize = MAX_PENDING_HEADERS + MAX_HEADERS_RESULTS + 1;

/// Unknown parent, and how many pending headers lie between a hash and it.
/// A marked hash does not reach a header this node has.
type NoBaseMap = HashMap<BlockHash, (BlockHash, u32)>;

fn empty_no_base() -> &'static NoBaseMap {
    static EMPTY: std::sync::OnceLock<NoBaseMap> = std::sync::OnceLock::new();
    EMPTY.get_or_init(NoBaseMap::new)
}

/// Roots [`no_base_depth`] follows through their own marks. A root parked
/// after the headers marked at it gets a mark of its own, and those marks are
/// not rewritten, so the next root is looked up when a walk stops. Past this
/// many, the mark counts as unknown, which keeps a long chain of out-of-order
/// one-header pages to a bounded lookup per stop.
#[doc(hidden)]
pub const MAX_MARK_HOPS: usize = 16;

/// Steps from `hash` to the unknown parent its mark leads to, following up
/// to [`MAX_MARK_HOPS`] roots that are marked themselves. `None` when `hash`
/// is unmarked or the chain of roots reaches genesis or a header we have.
fn no_base_depth(hub: &ChainHub, no_base: &NoBaseMap, hash: &BlockHash) -> Option<u32> {
    let (mut root, mut depth) = *no_base.get(hash)?;
    for _ in 0..MAX_MARK_HOPS {
        if is_genesis_hash(&root) || hub.knows_header(&root) {
            return None;
        }
        let Some(&(next, more)) = no_base.get(&root) else {
            return Some(depth);
        };
        root = next;
        depth = depth.saturating_add(more);
    }
    Some(depth)
}

/// A new hash at the cap is refused. The headers already held stay.
#[doc(hidden)]
pub fn admit_pending_header(
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    hash: BlockHash,
    header: bitcoin::block::Header,
) -> Option<Option<bitcoin::block::Header>> {
    if pending.len() >= MAX_PENDING_HEADERS && !pending.contains_key(&hash) {
        return None;
    }
    Some(pending.insert(hash, header))
}

/// Changes made to a peer's pending map while one message is classified,
/// so the map can be put back as it was without a copy of it.
#[derive(Default)]
struct PendingUndo(Vec<(BlockHash, Option<bitcoin::block::Header>)>);

impl PendingUndo {
    fn insert(
        &mut self,
        pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
        hash: BlockHash,
        header: bitcoin::block::Header,
    ) {
        let prior = pending.insert(hash, header);
        self.0.push((hash, prior));
    }

    fn admit(
        &mut self,
        pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
        hash: BlockHash,
        header: bitcoin::block::Header,
    ) {
        if let Some(prior) = admit_pending_header(pending, hash, header) {
            self.0.push((hash, prior));
        }
    }

    fn removed(&mut self, gone: &[(BlockHash, bitcoin::block::Header)]) {
        self.0
            .extend(gone.iter().map(|(hash, hdr)| (*hash, Some(*hdr))));
    }

    /// Undo every change, newest first.
    fn revert(self, pending: &mut HashMap<BlockHash, bitcoin::block::Header>) {
        for (hash, prior) in self.0.into_iter().rev() {
            match prior {
                Some(hdr) => {
                    pending.insert(hash, hdr);
                }
                None => {
                    pending.remove(&hash);
                }
            }
        }
    }
}

/// `f` on `pending` with `headers` added, then the map as it was.
fn with_batch<T>(
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    headers: &[bitcoin::block::Header],
    f: impl FnOnce(&HashMap<BlockHash, bitcoin::block::Header>) -> T,
) -> T {
    let mut undo = PendingUndo::default();
    for hdr in headers {
        undo.insert(pending, hdr.block_hash(), *hdr);
    }
    let out = f(pending);
    undo.revert(pending);
    out
}
/// Cap on decoded bodies stashed per session (DoS / process RAM). Must be
/// ≥99 so tip-follow can assemble a 99-block competing branch; apply is
/// `ChainHub::accept_received_block` (see `docs/architecture.md`).
/// Inflight `getdata` is [`MAX_SERVE_BLOCKS`] (peer reconstruct cap).
#[doc(hidden)]
pub const MAX_PENDING_BLOCKS: usize = 128;
/// Honest maximum block serialization. One larger body is not parked.
#[doc(hidden)]
pub const MAX_BLOCK_SERIALIZED: usize = 4_000_000;
/// Max reconstructed full bodies queued on one session writer, and the
/// matching catch-up `getdata` window (extra hashes stick in `requested`).
pub const MAX_SERVE_BLOCKS: usize = 16;
/// Compact getdata/serve only near the validated tip. Deeper catch-up uses
/// `MSG_WITNESS_BLOCK`.
#[doc(hidden)]
pub const MAX_CMPCTBLOCK_DEPTH: u32 = 5;
/// Drop inflight `getdata` hashes that the peer never sent so catch-up can
/// ask again. `sync_blocks` is 60s; 120s headers poll is too late.
pub const BLOCK_GETDATA_TIMEOUT: Duration = Duration::from_secs(10);

/// INV-origin txids this session sent us. Cap matches the announced-wtxid set (FIFO roll).
pub const FROM_THIS_PEER_CAP: usize = 50_000;

/// Test/assert surface for the tip-follow pending-body cap (equals production).
#[cfg(debug_assertions)]
pub const MAX_PENDING_BLOCKS_FOR_TEST: usize = MAX_PENDING_BLOCKS;

/// Tip-follow decoded bodies waiting for a connectable parent. Cap 128;
/// insert evicts the oldest hash (FIFO), not `HashMap::keys().next()`. A body
/// parked while it was still asked keeps its [`BlockAsk`] here, so evicting
/// or dropping the body releases the ask.
#[derive(Default)]
pub struct PendingBlocks {
    pub map: HashMap<BlockHash, bitcoin::Block>,
    pub fifo: VecDeque<BlockHash>,
    asks: HashMap<BlockHash, BlockAsk>,
}

impl PendingBlocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains_key(&self, hash: &BlockHash) -> bool {
        self.map.contains_key(hash)
    }

    pub fn values(&self) -> std::collections::hash_map::Values<'_, BlockHash, bitcoin::Block> {
        self.map.values()
    }

    pub fn keys(&self) -> std::collections::hash_map::Keys<'_, BlockHash, bitcoin::Block> {
        self.map.keys()
    }

    pub fn insert(&mut self, hash: BlockHash, block: bitcoin::Block, ask: Option<BlockAsk>) {
        if stash_pending_block(self, hash, block) {
            if let Some(ask) = ask {
                self.asks.insert(hash, ask);
            }
        }
    }

    pub fn remove(&mut self, hash: &BlockHash) -> Option<bitcoin::Block> {
        self.take(hash).map(|(block, _)| block)
    }

    /// The body and the ask it was parked with.
    pub fn take(&mut self, hash: &BlockHash) -> Option<(bitcoin::Block, Option<BlockAsk>)> {
        let b = self.map.remove(hash)?;
        if let Some(i) = self.fifo.iter().position(|h| h == hash) {
            self.fifo.remove(i);
        }
        Some((b, self.asks.remove(hash)))
    }
}

#[doc(hidden)]
pub fn block_wire_len(block: &bitcoin::Block) -> usize {
    block.total_size()
}

/// Park `block`. False when it is too large to park or the cap is full.
#[doc(hidden)]
pub fn stash_pending_block(
    pending: &mut PendingBlocks,
    hash: BlockHash,
    block: bitcoin::Block,
) -> bool {
    let nbytes = block_wire_len(&block);
    if nbytes > MAX_BLOCK_SERIALIZED {
        return false;
    }
    if let std::collections::hash_map::Entry::Occupied(mut e) = pending.map.entry(hash) {
        e.insert(block);
        return true;
    }
    while pending.map.len() >= MAX_PENDING_BLOCKS {
        let Some(k) = pending.fifo.pop_front() else {
            break;
        };
        pending.map.remove(&k);
        pending.asks.remove(&k);
    }
    if pending.map.len() >= MAX_PENDING_BLOCKS {
        return false;
    }
    pending.fifo.push_back(hash);
    pending.map.insert(hash, block);
    true
}

/// Bodies this session asked its peer for by `getdata` (Core
/// `vBlocksInFlight`, without compact partials). An entry holds its hub
/// [`BlockAsk`] until the body is settled: removing the entry, or dropping
/// the table when the session ends or its task is aborted, releases it.
#[doc(hidden)]
#[derive(Default)]
pub struct RequestedBlocks {
    hashes: HashSet<BlockHash>,
    asks: HashMap<BlockHash, BlockAsk>,
}

impl RequestedBlocks {
    pub fn hashes(&self) -> &HashSet<BlockHash> {
        &self.hashes
    }

    pub fn contains(&self, hash: &BlockHash) -> bool {
        self.hashes.contains(hash)
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Ask for `hash` unless another session already has: the header path's
    /// one claim per body.
    pub fn claim(&mut self, hub: &ChainHub, hash: BlockHash) -> bool {
        if self.hashes.contains(&hash) {
            return false;
        }
        let Some(ask) = hub.claim_block_ask(&hash) else {
            return false;
        };
        self.hashes.insert(hash);
        self.asks.insert(hash, ask);
        true
    }

    /// Ask for `hash` even when another session already has. The new ask
    /// is held before an earlier one for the hash drops, so the hash stays
    /// asked even if the set cleared since that earlier ask.
    pub fn ask(&mut self, hub: &ChainHub, hash: BlockHash) {
        self.hashes.insert(hash);
        self.asks.insert(hash, hub.ask_block(hash));
    }

    /// Put back an ask taken out by [`Self::remove`].
    pub fn restore(&mut self, ask: BlockAsk) {
        self.hashes.insert(ask.hash());
        self.asks.insert(ask.hash(), ask);
    }

    /// The body is settled here. The caller owns the returned ask; dropping
    /// it releases the body to other sessions.
    pub fn remove(&mut self, hash: &BlockHash) -> Option<BlockAsk> {
        self.hashes.remove(hash);
        self.asks.remove(hash)
    }

    /// Release the hub ask while the getdata stays in flight on this
    /// session, so other sessions may fetch the same body.
    pub fn release_ask(&mut self, hash: &BlockHash) {
        self.asks.remove(hash);
    }
}

/// Service bits for one node.
///
/// BIP159: a pruned node offers `NETWORK_LIMITED`, not `NETWORK`.
/// `NODE_COMPACT_FILTERS` is that hub's bit. No hub means this node has
/// not caught filters up.
pub fn local_service_flags(pruned: bool, peers: Option<&crate::peers::PeerHub>) -> ServiceFlags {
    let mut base = if pruned {
        // NETWORK_LIMITED, WITNESS, and P2P_V2 are distinct bits.
        let mut flags = ServiceFlags::NETWORK_LIMITED;
        flags.add(ServiceFlags::WITNESS);
        flags.add(ServiceFlags::P2P_V2);
        flags
    } else {
        crate::seeds::required_seed_services()
    };
    if peers.is_some_and(|p| p.compact_filters_advertised()) {
        base.add(ServiceFlags::COMPACT_FILTERS);
    }
    base
}

/// NETWORK_LIMITED is enough when the tip is shallower than this (~24h at 10m).
pub const NODE_NETWORK_LIMITED_ALLOW_CONN_BLOCKS: i64 = 144;

/// Outbound-full, block-relay, and addr-fetch sessions must offer NETWORK.
pub fn expect_services_from_conn(typ: crate::peers::PeerConnType) -> bool {
    matches!(
        typ,
        crate::peers::PeerConnType::OutboundFullRelay
            | crate::peers::PeerConnType::BlockRelay
            | crate::peers::PeerConnType::AddrFetch
    )
}

/// Service bits we want from a peer given what they offered and how deep the tip is.
pub fn desirable_service_flags(offered: ServiceFlags, tip_depth_blocks: i64) -> ServiceFlags {
    if offered.has(ServiceFlags::NETWORK_LIMITED)
        && tip_depth_blocks < NODE_NETWORK_LIMITED_ALLOW_CONN_BLOCKS
    {
        let mut flags = ServiceFlags::NETWORK_LIMITED;
        flags.add(ServiceFlags::WITNESS);
        flags
    } else {
        let mut flags = ServiceFlags::NETWORK;
        flags.add(ServiceFlags::WITNESS);
        flags
    }
}

/// Whether offered flags already include what we want for this tip depth.
pub fn has_all_desirable_service_flags(offered: ServiceFlags, tip_depth_blocks: i64) -> bool {
    let want = desirable_service_flags(offered, tip_depth_blocks);
    offered.has(want)
}

pub fn expected_services_disconnect_log(offered: u64, expected: u64) -> String {
    format!(
        "p2p: does not offer the expected services ({offered:08x} offered, {expected:08x} expected)"
    )
}

// Feeler completion sentence. The disconnect path does not read it.
#[cfg_attr(false, mutants::skip)]
pub fn feeler_connection_completed_log() -> &'static str {
    "p2p: feeler connection completed"
}

pub fn connected_to_self_log(addr: impl std::fmt::Display) -> String {
    format!("p2p: connected to self at {addr}, disconnecting")
}

pub fn log_connected_to_self(addr: impl std::fmt::Display) {
    rbitcoin_log::info_repeated(
        rbitcoin_log::RepeatedInfo::ConnectedToSelf,
        format_args!("{}", connected_to_self_log(addr)),
    );
}

pub fn version_handshake_timeout_log(peer: u64) -> String {
    format!("version handshake timeout, disconnecting peer={peer}")
}

/// Max addresses in one ADDR / addrv2. Interop size — do not change without
/// a named reason to diverge (see COMPAT.md).
pub const MAX_ADDR_TO_SEND: usize = 1000;
/// Core `MAX_ADDR_RATE_PER_SECOND`: a peer earns one address token per 10 s.
pub const MAX_ADDR_RATE_PER_SECOND: f64 = 0.1;
/// Core `MAX_ADDR_PROCESSING_TOKEN_BUCKET`: refill stops at this many tokens.
pub const MAX_ADDR_PROCESSING_TOKEN_BUCKET: f64 = MAX_ADDR_TO_SEND as f64;

/// Core `Peer::m_addr_token_bucket`. It starts at one token. Each address a
/// peer sends costs one, and an address with no token left is dropped before
/// AddrMan and relay.
#[derive(Debug, Clone, Copy)]
pub struct AddrTokenBucket {
    pub tokens: f64,
    pub at_ms: u64,
}

impl AddrTokenBucket {
    pub fn new(now_ms: u64) -> Self {
        Self {
            tokens: 1.0,
            at_ms: now_ms,
        }
    }

    /// Refill for the time since the last call (only below the cap), then
    /// take up to `want` tokens. Returns how many addresses may be processed.
    pub fn take(&mut self, want: usize, now_ms: u64) -> usize {
        // At the cap, time adds nothing.
        let at_cap = self.tokens >= MAX_ADDR_PROCESSING_TOKEN_BUCKET;
        if !at_cap {
            let dt = now_ms.saturating_sub(self.at_ms) as f64 / 1000.0;
            self.tokens =
                (self.tokens + dt * MAX_ADDR_RATE_PER_SECOND).min(MAX_ADDR_PROCESSING_TOKEN_BUCKET);
        }
        self.at_ms = self.at_ms.max(now_ms);
        let take = (self.tokens.floor() as usize).min(want);
        self.tokens -= take as f64;
        take
    }

    pub(crate) fn credit(&mut self, n: usize) {
        self.tokens += n as f64;
    }
}

#[doc(hidden)]
pub fn addr_relay_key(msg: &bitcoin::p2p::address::AddrV2Message) -> u64 {
    let raw = bitcoin::consensus::encode::serialize(msg);
    let mut h = 0xcbf29ce484222325u64;
    for b in raw {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// GetAddr returns at most this percent of AddrMan (then `MAX_ADDR_TO_SEND`).
/// Interop size — do not change without a named reason to diverge
/// (see COMPAT.md).
pub const MAX_PCT_ADDR_TO_SEND: usize = 23;

pub fn sendaddrv2_after_verack_log(peer: u64) -> String {
    format!("sendaddrv2 received after verack, disconnecting peer={peer}")
}

pub fn addrv2_message_size_log(n: usize) -> String {
    format!("addrv2 message size = {n}")
}

pub fn received_addrv2_log(nbytes: usize, peer: u64) -> String {
    format!("received: addrv2 ({nbytes} bytes) peer={peer}")
}

pub fn sending_addrv2_log(nbytes: usize, peer: u64) -> String {
    format!("sending addrv2 ({nbytes} bytes) peer={peer}")
}

pub fn block_download_timeout_log(hash: BlockHash, peer: u64) -> String {
    format!("p2p: Timeout downloading block {hash}, disconnecting peer={peer}")
}

pub fn ping_timeout_log(elapsed_secs: f64) -> String {
    format!("p2p: ping timeout: {elapsed_secs:.6}s")
}

pub fn ping_prior_to_verack_log(peer: u64) -> String {
    unsupported_before_verack_log("ping", peer)
}

pub fn unsupported_before_verack_log(cmd: &str, peer: u64) -> String {
    let cmd = peer_log_text(cmd);
    format!("Unsupported message \"{cmd}\" prior to verack from peer={peer}")
}

/// One log line: peer-controlled bytes cannot insert a raw newline.
#[doc(hidden)]
pub fn peer_log_text(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Disconnect peers advertising protocol version below this.
pub const MIN_PEER_PROTO_VERSION: i32 = 31800;

pub fn obsolete_version_log(version: i32, peer: u64) -> String {
    format!("using obsolete version {version}, disconnecting peer={peer}")
}

pub fn advertising_address_log(addr_port: impl std::fmt::Display, peer: u64) -> String {
    format!("Advertising address {addr_port} to peer={peer}")
}

#[doc(hidden)]
pub fn queue_addr_list(
    session: Option<&crate::peers::LivePeer>,
    out: &PeerOutTx,
    addrs: Vec<(u32, crate::NetAddr)>,
    v2: bool,
) -> Result<(), NetError> {
    let services = session
        .and_then(|s| s.peer_hub())
        .map(|h| local_service_flags(h.is_pruned(), Some(&h)))
        .unwrap_or_else(|| local_service_flags(false, None));
    if v2 {
        let list: Vec<AddrV2Message> = addrs
            .into_iter()
            .map(|(t, a)| AddrV2Message {
                time: t,
                services,
                addr: a.to_addrv2(),
                port: a.port(),
            })
            .collect();
        out.send(NetworkMessage::AddrV2(list))
    } else {
        let list: Vec<(u32, Address)> = addrs
            .into_iter()
            .filter_map(|(t, a)| match a {
                crate::NetAddr::Ip(s) => Some((t, Address::new(&s, services))),
                crate::NetAddr::Onion { .. }
                | crate::NetAddr::I2p { .. }
                | crate::NetAddr::Cjdns { .. }
                | crate::NetAddr::Name { .. } => None,
            })
            .collect();
        out.send(NetworkMessage::Addr(list))
    }
}

#[doc(hidden)]
pub fn maybe_queue_local_addr(
    _hub: &ChainHub,
    session: &crate::peers::LivePeer,
    out: &PeerOutTx,
) -> Result<(), NetError> {
    if let Some(msg) = session.take_self_announce_msg() {
        out.send(msg)?;
    }
    Ok(())
}

pub fn hidden_addr_from() -> Address {
    Address::new(
        &SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
        ServiceFlags::NONE,
    )
}

pub fn non_version_before_handshake_log(cmd: &str, peer: u64) -> String {
    let cmd = peer_log_text(cmd);
    format!("non-version message before version handshake. Message \"{cmd}\" from peer={peer}")
}

/// Tip age in blocks: `(now - tip_time) / pow_target_spacing`.
pub fn approximate_best_block_depth(hub: &ChainHub) -> i64 {
    let Some(h) = hub.tip_header() else {
        return i64::MAX;
    };
    let spacing = hub.params.btc.pow_target_spacing.max(1) as i64;
    let age = hub.clock.now_secs().saturating_sub(u64::from(h.time)) as i64;
    age / spacing
}

/// Optional bookkeeping for outbound tip-follow sessions.
#[derive(Clone, Default)]
pub struct FollowSessionMeta {
    /// Peer address (logging).
    pub peer: Option<SocketAddr>,
    /// Live outbound follow count (inc on start, dec on exit).
    pub live: Option<Arc<AtomicUsize>>,
    /// RPC session row (bytes + disconnect).
    pub session: Option<Arc<crate::peers::LivePeer>>,
    /// Tip when the handshake started. Catch-up announces only a tip that
    /// moved after this. `None` means there was no tip yet.
    pub tip_at_handshake: Option<BlockHash>,
}

/// Decrements the live follow counter when a session task exits.
/// Increment happens in [`crate::service::P2PNode::follow_from`] so the count
/// is visible as soon as handshake succeeds (before the task is scheduled).
#[doc(hidden)]
pub struct LiveFollowDec(pub Option<Arc<AtomicUsize>>);

impl Drop for LiveFollowDec {
    fn drop(&mut self) {
        if let Some(ref c) = self.0 {
            c.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Optional hub/peers policy for VERSION checks (services + self-connect nonce).
#[derive(Clone, Copy)]
pub struct HandshakePolicy<'a> {
    pub hub: Option<&'a ChainHub>,
    pub peers: Option<&'a crate::peers::PeerHub>,
    pub session: Option<&'a crate::peers::LivePeer>,
    pub conn_type: crate::peers::PeerConnType,
}

impl HandshakePolicy<'static> {
    pub fn plain() -> Self {
        Self {
            hub: None,
            peers: None,
            session: None,
            conn_type: crate::peers::PeerConnType::OutboundFullRelay,
        }
    }

    /// Core's private broadcast handshake: one constant VERSION
    /// (`private_broadcast_version`), no WTXIDRELAY or SENDADDRV2, and a peer
    /// whose own VERSION has relay off is dropped before VERACK.
    pub(crate) fn private_broadcast() -> Self {
        Self {
            conn_type: crate::peers::PeerConnType::PrivateBroadcast,
            ..Self::plain()
        }
    }
}

/// Core's user agent on a private broadcast connection (#29415).
pub(crate) const PRIVATE_BROADCAST_USER_AGENT: &str = "/pynode:0.0.1/";

/// Core `PushNodeVersion` on a private broadcast connection. Every such
/// connection sends these bytes, the nonce aside, so they say nothing about us.
/// Both addresses are Core's `CService{}` (16 zero bytes, port 0).
fn private_broadcast_version(nonce: u64) -> VersionMessage {
    let unset = Address {
        services: ServiceFlags::NONE,
        address: [0; 8],
        port: 0,
    };
    VersionMessage {
        version: OUR_PROTOCOL_VERSION.max(PROTOCOL_VERSION),
        services: ServiceFlags::NONE,
        timestamp: 0,
        receiver: unset.clone(),
        sender: unset,
        nonce,
        user_agent: PRIVATE_BROADCAST_USER_AGENT.to_string(),
        start_height: 0,
        relay: false,
    }
}

/// Outbound BIP324 session. [`Self::outbound_regtest`] completes VERSION/VERACK;
/// [`Self::outbound_bip324`] stops after transport so the caller can send a
/// custom first application message.
pub struct V2PlainSession {
    pub reader: V2Reader,
    pub writer: V2Writer,
    pub tcp_shutdown: std::net::TcpStream,
}

impl V2PlainSession {
    /// Dial-side BIP324 only (no VERSION). Caller sends the first application message.
    pub async fn outbound_bip324(stream: TcpStream) -> Result<Self, NetError> {
        let magic = Magic::REGTEST;
        let (reader, writer, _wire, tcp_shutdown) = open_v2(stream, magic, false).await?;
        Ok(Self {
            reader,
            writer,
            tcp_shutdown,
        })
    }

    /// Dial-side handshake on `stream`; `limit` bounds VERSION/VERACK.
    pub async fn outbound_regtest(
        stream: TcpStream,
        user_agent: &str,
        limit: Duration,
    ) -> Result<Self, NetError> {
        let our_addr = stream.local_addr()?;
        let their_addr = stream.peer_addr()?;
        let magic = Magic::REGTEST;
        let (_ver, reader, writer, _wire, tcp_shutdown) = connect_and_handshake_timed(
            limit,
            stream,
            magic,
            our_addr,
            their_addr,
            0,
            false,
            user_agent,
            HandshakePolicy::plain(),
        )
        .await?;
        Ok(Self {
            reader,
            writer,
            tcp_shutdown,
        })
    }

    pub async fn write_contents(&mut self, contents: &[u8]) -> Result<(), NetError> {
        write_v2_contents(&mut self.writer, contents.to_vec()).await
    }

    pub async fn read_contents(&mut self) -> Result<Vec<u8>, NetError> {
        read_v2_contents(&mut self.reader).await
    }

    pub async fn read_frame(&mut self) -> Result<(), NetError> {
        self.read_contents().await.map(|_| ())
    }

    pub fn close(&mut self) {
        let _ = self.tcp_shutdown.shutdown(std::net::Shutdown::Both);
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
/// Open BIP324 v2 transport + perform the version/verack exchange.
///
/// Returns the peer's version and the encrypted read/write halves. All further
/// messages must use those halves. Only an inbound `v1@` listener speaks v1
/// ([`inbound_connect_and_handshake`]).
pub async fn connect_and_handshake(
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    inbound: bool,
    user_agent: &str,
    policy: HandshakePolicy<'_>,
) -> Result<
    (
        VersionMessage,
        V2Reader,
        V2Writer,
        crate::v2::WireBytes,
        std::net::TcpStream,
    ),
    NetError,
> {
    let wire = policy.session.map(|s| s.new_wire()).unwrap_or_default();
    let (mut reader, mut writer, wire, tcp_shutdown) =
        open_v2_with_wire(stream, magic, inbound, wire).await?;
    let their_version = application_handshake(
        &mut reader,
        &mut writer,
        magic,
        our_addr,
        their_addr,
        start_height,
        inbound,
        user_agent,
        policy,
    )
    .await?;
    Ok((their_version, reader, writer, wire, tcp_shutdown))
}

/// Core VERSION/VERACK bound: 60s from TCP connect/accept. Timeout drops the stream.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Inbound handshake bound. `--peer-timeout` can shorten it. It cannot
/// lengthen it past [`HANDSHAKE_TIMEOUT`].
pub fn inbound_handshake_limit(peer_timeout_secs: u64) -> Duration {
    Duration::from_secs(peer_timeout_secs).min(HANDSHAKE_TIMEOUT)
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn inbound_connect_and_handshake(
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    user_agent: &str,
    peers: &std::sync::Arc<crate::peers::PeerHub>,
    bind: SocketAddr,
) -> Result<
    (
        VersionMessage,
        PeerReader,
        PeerWriter,
        std::net::TcpStream,
        std::sync::Arc<crate::peers::LivePeer>,
    ),
    NetError,
> {
    let _ = stream.set_nodelay(true);
    let std = stream.into_std().map_err(NetError::Io)?;
    std.set_nonblocking(true).map_err(NetError::Io)?;
    let tcp_pre = std.try_clone().map_err(NetError::Io)?;
    let stream = TcpStream::from_std(std).map_err(NetError::Io)?;
    let sess =
        peers.register_connecting(their_addr, bind, true, crate::peers::PeerConnType::Inbound);
    let wire = sess.new_wire();
    sess.attach_tcp_shutdown(tcp_pre);
    // One deadline from accept through key exchange and VERSION/VERACK.
    // A per-read deadline would reset on each drip byte.
    let limit = inbound_handshake_limit(peers.peer_timeout_secs());
    let opened = tokio::time::timeout(limit, async {
        let perms = peers.permission_flags(their_addr, true, bind);
        let (mut reader, mut writer, wire, tcp_shutdown) =
            match open_inbound_with_wire(stream, magic, wire, perms).await {
                Ok(x) => x,
                Err(NetError::Protocol("magic-prefixed ellswift")) => {
                    while !sess.stop.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    return Err(NetError::Timeout);
                }
                Err(e) => return Err(e),
            };
        match &reader {
            PeerReader::V1(_) => sess.mark_v1_transport(),
            PeerReader::V2(_) => sess.mark_v2_transport_ready(),
        }
        if let Ok(clone) = tcp_shutdown.try_clone() {
            sess.attach_tcp_shutdown(clone);
        }
        let policy = HandshakePolicy {
            hub: None,
            peers: Some(peers),
            session: Some(sess.as_ref()),
            conn_type: crate::peers::PeerConnType::Inbound,
        };
        let their_version = application_handshake(
            &mut reader,
            &mut writer,
            magic,
            our_addr,
            their_addr,
            start_height,
            true,
            user_agent,
            policy,
        )
        .await?;
        Ok((their_version, reader, writer, wire, tcp_shutdown))
    })
    .await;
    let (their_version, reader, writer, wire, tcp_shutdown) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => {
            let _ = peers.disconnect_id(sess.id);
            return Err(e);
        }
        Err(_) => {
            if sess.v2_transport_ready() {
                rbitcoin_log::debug!(P2p, "{}", crate::v2::v2_handshake_timeout_log(sess.id));
            } else {
                rbitcoin_log::debug!(P2p, "{}", version_handshake_timeout_log(sess.id));
            }
            let _ = peers.disconnect_id(sess.id);
            return Err(NetError::Timeout);
        }
    };
    // Publication refuses an endpoint a manual ban names, including a
    // ban added while this handshake ran.
    let Some(sess) = peers.register_handshaken_net(
        &sess,
        crate::peers::PeerEndpoint {
            addr: their_addr,
            net: crate::NetAddr::from_socket(their_addr),
            addrbind: bind,
        },
        &their_version,
        crate::peers::PeerConnType::Inbound,
        wire,
    ) else {
        let _ = peers.disconnect_id(sess.id);
        return Err(NetError::Protocol("banned"));
    };
    Ok((their_version, reader, writer, tcp_shutdown, sess))
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn connect_and_handshake_timed(
    limit: Duration,
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    inbound: bool,
    user_agent: &str,
    policy: HandshakePolicy<'_>,
) -> Result<
    (
        VersionMessage,
        V2Reader,
        V2Writer,
        crate::v2::WireBytes,
        std::net::TcpStream,
    ),
    NetError,
> {
    tokio::time::timeout(
        limit,
        connect_and_handshake(
            stream,
            magic,
            our_addr,
            their_addr,
            start_height,
            inbound,
            user_agent,
            policy,
        ),
    )
    .await
    .map_err(|_| NetError::Timeout)?
}

/// Feeler: send version (relay=0), read their version, close. No verack, no session.
pub async fn run_feeler(
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    user_agent: &str,
    peers: &crate::peers::PeerHub,
) -> Result<(), NetError> {
    run_feeler_timed(
        HANDSHAKE_TIMEOUT,
        stream,
        magic,
        our_addr,
        their_addr,
        start_height,
        user_agent,
        peers,
    )
    .await
}

/// Feeler handshake with an explicit timeout (production uses [`HANDSHAKE_TIMEOUT`]).
#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn run_feeler_timed(
    limit: Duration,
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    user_agent: &str,
    peers: &crate::peers::PeerHub,
) -> Result<(), NetError> {
    tokio::time::timeout(
        limit,
        run_feeler_inner(
            stream,
            magic,
            our_addr,
            their_addr,
            start_height,
            user_agent,
            peers,
        ),
    )
    .await
    .map_err(|_| NetError::Timeout)?
}

pub async fn run_feeler_inner(
    stream: TcpStream,
    magic: Magic,
    our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    user_agent: &str,
    peers: &crate::peers::PeerHub,
) -> Result<(), NetError> {
    let (mut reader, mut writer, _wire, _tcp_shutdown) = open_v2(stream, magic, false).await?;
    let services = local_service_flags(peers.is_pruned(), Some(peers));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let version = VersionMessage {
        version: OUR_PROTOCOL_VERSION.max(PROTOCOL_VERSION),
        services,
        timestamp: now,
        receiver: Address::new(&their_addr, ServiceFlags::NONE),
        sender: Address::new(&our_addr, services),
        nonce: rand_nonce(),
        user_agent: user_agent.to_string(),
        start_height,
        relay: false,
    };
    write_v2_msg(&mut writer, NetworkMessage::Version(version)).await?;
    loop {
        let frame = read_v2_frame(&mut reader, magic).await?;
        if command_str(&frame.command) != "version" {
            continue;
        }
        let msg = frame.try_decode()?;
        if matches!(msg.payload(), NetworkMessage::Version(_)) {
            break;
        }
    }
    rbitcoin_log::info!("{}", feeler_connection_completed_log());
    Ok(())
}

#[doc(hidden)]
pub fn command_label(cmd: &[u8; 12]) -> String {
    let n = cmd.iter().position(|&b| b == 0).unwrap_or(12);
    String::from_utf8_lossy(&cmd[..n]).into_owned()
}

pub fn fail_if_handshake_timed_out(policy: &HandshakePolicy<'_>) -> Result<(), NetError> {
    let Some(s) = policy.session else {
        return Ok(());
    };
    let Some(peers) = policy.peers else {
        return Ok(());
    };
    if !peers.handshake_timed_out(s, peers.now_secs()) {
        return Ok(());
    }
    let line = if s.v2_transport_ready() {
        version_handshake_timeout_log(s.id)
    } else {
        crate::v2::v2_handshake_timeout_log(s.id)
    };
    rbitcoin_log::debug!(P2p, "{}", line);
    let _ = peers.disconnect_id(s.id);
    Err(NetError::Timeout)
}

pub async fn read_handshake_frame<R: FrameRead>(
    reader: &mut R,
    magic: Magic,
    policy: &HandshakePolicy<'_>,
) -> Result<FramedMessage, NetError> {
    loop {
        fail_if_handshake_timed_out(policy)?;
        if let Some(s) = policy.session {
            if s.stop.load(Ordering::Relaxed) {
                return Err(NetError::Disconnected);
            }
        }
        match tokio::time::timeout(Duration::from_millis(50), reader.read_frame(magic)).await {
            Ok(Ok(frame)) => return Ok(frame),
            Ok(Err(e)) => {
                fail_if_handshake_timed_out(policy)?;
                return Err(e);
            }
            Err(_) => continue,
        }
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
/// Perform the version/verack exchange over an established BIP324 session.
pub async fn application_handshake<R: FrameRead, W: FrameWrite>(
    reader: &mut R,
    writer: &mut W,
    magic: Magic,
    _our_addr: SocketAddr,
    their_addr: SocketAddr,
    start_height: i32,
    inbound: bool,
    user_agent: &str,
    policy: HandshakePolicy<'_>,
) -> Result<VersionMessage, NetError> {
    let private = policy.conn_type == crate::peers::PeerConnType::PrivateBroadcast;
    let our_nonce = rand_nonce();
    let version = if private {
        private_broadcast_version(our_nonce)
    } else {
        let pruned = policy.peers.map(|p| p.is_pruned()).unwrap_or(false);
        let services = local_service_flags(pruned, policy.peers);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        VersionMessage {
            version: OUR_PROTOCOL_VERSION.max(PROTOCOL_VERSION),
            services,
            timestamp: now,
            receiver: Address::new(&their_addr, ServiceFlags::NONE),
            sender: hidden_addr_from(),
            nonce: our_nonce,
            user_agent: user_agent.to_string(),
            start_height,
            relay: true,
        }
    };

    struct OutboundNonceGuard<'a> {
        peers: Option<&'a crate::peers::PeerHub>,
        nonce: u64,
        clear: bool,
    }
    impl Drop for OutboundNonceGuard<'_> {
        fn drop(&mut self) {
            if self.clear {
                if let Some(p) = self.peers {
                    p.clear_outbound_nonce(self.nonce);
                }
            }
        }
    }
    let mut nonce_guard = OutboundNonceGuard {
        peers: None,
        nonce: our_nonce,
        clear: false,
    };
    if !inbound {
        if let Some(peers) = policy.peers {
            peers.note_outbound_nonce(our_nonce);
            nonce_guard.peers = Some(peers);
            nonce_guard.clear = true;
        }
        writer
            .write_msg(NetworkMessage::Version(version.clone()))
            .await?;
    }

    let their_version = read_peer_version(reader, magic, &policy).await?;

    if inbound {
        if let Some(peers) = policy.peers {
            if !peers.check_incoming_nonce(their_version.nonce) {
                log_connected_to_self(their_addr);
                return Err(NetError::Protocol("connected to self"));
            }
        }
        writer.write_msg(NetworkMessage::Version(version)).await?;
    } else if private && !their_version.relay {
        return Err(NetError::Protocol("peer does not relay transactions"));
    } else if expect_services_from_conn(policy.conn_type) {
        if let Some(hub) = policy.hub {
            let depth = approximate_best_block_depth(hub);
            if !has_all_desirable_service_flags(their_version.services, depth) {
                let expected = desirable_service_flags(their_version.services, depth);
                rbitcoin_log::info!(
                    "{}",
                    expected_services_disconnect_log(
                        their_version.services.to_u64(),
                        expected.to_u64()
                    )
                );
                return Err(NetError::Protocol("peer missing desirable services"));
            }
        }
    }

    // BIP339 / BIP155 feature negotiation starts at 70016 (`p2p_leak` pre-wtxid).
    if !private && their_version.version >= WTXID_RELAY_VERSION {
        writer.write_msg(NetworkMessage::WtxidRelay).await?;
        writer.write_msg(NetworkMessage::SendAddrV2).await?;
    }
    writer.write_msg(NetworkMessage::Verack).await?;
    wait_peer_verack(reader, magic, &policy, their_version.version).await?;

    nonce_guard.clear = false;
    if let Some(peers) = policy.peers {
        if !inbound {
            peers.clear_outbound_nonce(our_nonce);
        }
    }

    Ok(their_version)
}

fn is_pre_verack_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "version" | "verack" | "wtxidrelay" | "sendaddrv2" | "sendtxrcncl" | "ping"
    )
}

fn log_non_version_before_handshake(
    policy: &HandshakePolicy<'_>,
    cmd12: &[u8; 12],
) -> Result<(), NetError> {
    if let Some(s) = policy.session {
        rbitcoin_log::debug!(
            P2p,
            "{}",
            non_version_before_handshake_log(command_str(cmd12), s.id)
        );
    }
    fail_if_handshake_timed_out(policy)
}

pub async fn read_peer_version<R: FrameRead>(
    reader: &mut R,
    magic: Magic,
    policy: &HandshakePolicy<'_>,
) -> Result<VersionMessage, NetError> {
    loop {
        let frame = read_handshake_frame(reader, magic, policy).await?;
        let cmd12 = frame.command;
        if !matches!(command_str(&cmd12), "version" | "verack") {
            log_non_version_before_handshake(policy, &cmd12)?;
            continue;
        }
        let payload_len = frame.payload_len();
        let msg = frame.try_decode()?;
        match msg.payload() {
            NetworkMessage::Version(v) => {
                if (v.version as i32) < MIN_PEER_PROTO_VERSION {
                    if let Some(s) = policy.session {
                        rbitcoin_log::debug!(
                            P2p,
                            "{}",
                            obsolete_version_log(v.version as i32, s.id)
                        );
                        if let Some(peers) = policy.peers {
                            let _ = peers.disconnect_id(s.id);
                        }
                    }
                    return Err(NetError::Protocol("obsolete version"));
                }
                if let Some(s) = policy.session {
                    s.set_version_payload_len(payload_len);
                }
                return Ok(v.clone());
            }
            NetworkMessage::Verack => return Err(NetError::Protocol("verack before version")),
            _ => log_non_version_before_handshake(policy, &cmd12)?,
        }
    }
}

/// `peer_version` is the version the peer's VERSION claimed.
pub async fn wait_peer_verack<R: FrameRead>(
    reader: &mut R,
    magic: Magic,
    policy: &HandshakePolicy<'_>,
    peer_version: u32,
) -> Result<(), NetError> {
    loop {
        let frame = read_handshake_frame(reader, magic, policy).await?;
        let cmd12 = frame.command;
        let cmd = command_str(&cmd12);
        if !is_pre_verack_command(cmd) {
            if let Some(s) = policy.session {
                rbitcoin_log::debug!(P2p, "{}", unsupported_before_verack_log(cmd, s.id));
            }
            continue;
        }
        let msg = frame.try_decode()?;
        if apply_pre_verack(policy.session, peer_version, msg.payload(), cmd) {
            return Ok(());
        }
        // The read already applied the handshake deadline. Repeating it
        // here cannot fail, including for a wtxid or addr preface.
    }
}

/// Remember BIP339 / BIP155 features sent before VERACK. `true` = VERACK seen.
#[doc(hidden)]
pub fn apply_pre_verack(
    session: Option<&crate::peers::LivePeer>,
    peer_version: u32,
    payload: &NetworkMessage,
    cmd: &str,
) -> bool {
    match payload {
        NetworkMessage::Verack => true,
        NetworkMessage::SendAddrV2 => {
            if let Some(s) = session {
                s.set_wants_addrv2();
            }
            false
        }
        // Core ignores it below the version we send our own `wtxidrelay`
        // at, so a tx is never named by wtxid to a peer we did not send it.
        NetworkMessage::WtxidRelay => {
            if let Some(s) = session.filter(|_| peer_version >= WTXID_RELAY_VERSION) {
                s.set_wtxid_relay();
            }
            false
        }
        // A ping before verack only changes the log line. Both arms return
        // false, so the suite cannot tell this arm from the fallback.
        #[cfg_attr(false, mutants::skip)]
        NetworkMessage::Ping(_) => {
            if let Some(s) = session {
                rbitcoin_log::debug!(P2p, "{}", ping_prior_to_verack_log(s.id));
            }
            false
        }
        _ => {
            if let Some(s) = session {
                rbitcoin_log::debug!(P2p, "{}", unsupported_before_verack_log(cmd, s.id));
            }
            false
        }
    }
}

#[doc(hidden)]
pub fn framed_cmd(frame: &FramedMessage) -> String {
    let end = frame.command.iter().position(|&b| b == 0).unwrap_or(12);
    String::from_utf8_lossy(&frame.command[..end]).into_owned()
}

/// A command [`crate::v2::v2_command`] already checked, without the NUL
/// padding. Borrowed, so the writer does not allocate per frame.
fn command_str(cmd: &[u8; 12]) -> &str {
    let end = cmd.iter().position(|&b| b == 0).unwrap_or(12);
    std::str::from_utf8(&cmd[..end]).unwrap_or("*other*")
}

#[doc(hidden)]
pub fn rand_nonce() -> u64 {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("CSPRNG for version nonce");
    u64::from_le_bytes(buf)
}

/// Bidirectional peer session: serve history, tip follow, announce our tip.
///
/// After handshake preferences (`sendheaders` / `sendcmpct`), the session
/// **actively** `getheaders` from our tip locator so blocks mined while we were
/// offline or mid–SH materialize are pulled — not only unsolicited announces.
/// Long history catch-up remains `rbitcoin-ibd`. Tip-follow is
/// [`crate::service::P2PNode::follow_from`].
///
/// A dedicated writer drains outbound messages while the reader keeps draining
/// the encrypted channel. `meta` labels the peer for logs and optionally tracks
/// live outbound follow count.
/// Ping and `sendcmpct` first, then bodies, then headers and inv.
/// A pong is last in the batch. It must not pass a reply already queued
/// for an earlier message from this peer.
#[doc(hidden)]
pub fn outbound_write_rank(out: &PeerOut) -> u8 {
    match out {
        PeerOut::Msg(NetworkMessage::Ping(_) | NetworkMessage::SendCmpct(_)) => 0,
        PeerOut::Msg(NetworkMessage::Pong(_)) => 3,
        PeerOut::Encoded(_)
        | PeerOut::Served(_)
        | PeerOut::Msg(NetworkMessage::Block(_))
        | PeerOut::Msg(NetworkMessage::NotFound(_))
        | PeerOut::Msg(NetworkMessage::CmpctBlock(_)) => 1,
        _ => 2,
    }
}

#[doc(hidden)]
pub fn take_outbound_write_batch(first: Queued, rx: &mut PeerOutRx) -> Vec<Queued> {
    let mut batch = Vec::with_capacity(32);
    batch.push(first);
    while batch.len() < 64 {
        match rx.try_recv() {
            Ok(x) => batch.push(x),
            Err(_) => break,
        }
    }
    batch.sort_by_key(|q| outbound_write_rank(q.out()));
    batch
}

/// Queue one pong when a nonce newer than `written` is stored.
///
/// The message goes on the session channel, behind a reply already waiting.
/// It is not written inside the batch that just finished.
#[doc(hidden)]
pub fn requeue_later_pong(session: &crate::peers::LivePeer, written: u64) {
    let Some(extra) = session.finish_pong_write(written) else {
        return;
    };
    if !session.queue_msg(NetworkMessage::Pong(extra)) {
        // The claim inside `finish_pong_write` would otherwise stick with
        // no message on the channel, and the next ping would not queue.
        let _ = session.finish_pong_write(extra);
    }
}

/// Write v2 `contents` and, once the write succeeded, count them in
/// `bytessent_per_msg`: contents plus the cipher expansion on a v2 writer,
/// payload plus the v1 header on a v1 writer. Every frame the session sends
/// goes through here.
async fn write_counted(
    writer: &mut PeerWriter,
    session: Option<&crate::peers::LivePeer>,
    contents: Vec<u8>,
) -> Result<(), NetError> {
    let sent = crate::v2::v2_command(&contents)
        .ok()
        .map(|(cmd, prefix)| (cmd, contents.len(), prefix));
    writer.write_contents(contents).await?;
    let (Some(s), Some((cmd, contents_len, prefix))) = (session, sent) else {
        return Ok(());
    };
    let cmd = command_str(&cmd);
    match writer {
        PeerWriter::V2(_) => s.note_sent_v2(cmd, contents_len),
        PeerWriter::V1(_) => s.note_sent(cmd, contents_len.saturating_sub(prefix) as u64),
    }
    Ok(())
}

/// [`write_counted`] for a message the caller has not encoded.
async fn write_msg_counted(
    writer: &mut PeerWriter,
    session: Option<&crate::peers::LivePeer>,
    msg: NetworkMessage,
) -> Result<(), NetError> {
    let contents = crate::v2::encode_v2_contents(msg)?;
    write_counted(writer, session, contents).await
}

pub async fn run_writer_task(
    mut writer: PeerWriter,
    mut out_rx: PeerOutRx,
    writer_session: Option<Arc<crate::peers::LivePeer>>,
) {
    while let Some(first) = out_rx.recv().await {
        // Write the nonce that was queued. A newer ping is queued again
        // after this write, so it stays behind replies still on the channel.
        for queued in take_outbound_write_batch(first, &mut out_rx) {
            let (out, charge) = queued.into_parts();
            let pong_written = match &out {
                PeerOut::Msg(NetworkMessage::Pong(queued)) => Some(*queued),
                _ => None,
            };
            // A heavy payload encodes on `rbtc-scripts-*`.
            let contents = match out {
                PeerOut::Msg(msg) | PeerOut::Served(msg) => crate::v2::encode_v2_payload(msg).await,
                PeerOut::Encoded(bytes) => Ok(bytes),
            };
            let err = match contents {
                Ok(contents) => write_counted(&mut writer, writer_session.as_deref(), contents)
                    .await
                    .is_err(),
                Err(_) => true,
            };
            // Core `SocketSendData`: the bytes leave `nSendSize` once the
            // socket took them, not when the writer picked them up.
            drop(charge);
            if err {
                return;
            }
            if let (Some(s), Some(written)) = (&writer_session, pong_written) {
                requeue_later_pong(s, written);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn on_heartbeat(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    requested_since: &mut Option<std::time::Instant>,
    tx_announce_rx: &mut Option<broadcast::Receiver<crate::tx_relay::MempoolAnnounce>>,
    inv_flush_rx: &mut Option<broadcast::Receiver<()>>,
    decode_waiting: bool,
    decode_queued: bool,
) -> Result<(), NetError> {
    if tx_announce_rx.is_none() {
        *tx_announce_rx = hub.mempool().map(|m| m.subscribe_announces());
    }
    if inv_flush_rx.is_none() {
        *inv_flush_rx = hub.mempool().map(|m| m.subscribe_inv_flush());
    }
    let Some(s) = session else {
        return Ok(());
    };
    maybe_queue_feefilter(hub, out_tx, s);
    if addrfetch_timed_out(s) {
        rbitcoin_log::debug!(P2p, "addrfetch connection timeout");
        s.request_disconnect();
    }
    let now = s.clock_now();
    s.sync_block_download(now, |h| follow.block_in_flight(h));
    // Blocks in flight, or a frame already read and still waiting to decode,
    // suspend the ping timeout. A due ping still sends. The next TCP read
    // stays armed, so this is the heartbeat side of that same wait. A frame
    // held for the send budget is not waiting on a decode permit: the writer
    // may never drain, so its hold must not suppress the timeout.
    let ping_elapsed = if decode_waiting || follow.has_blocks_in_flight() {
        None
    } else {
        s.ping_timeout(now)
    };
    // A block or compact frame still waiting to decode is the oldest block
    // in flight, so timing that window out would race its decode. A frame
    // held for the send budget does not suspend this window: the disconnect
    // drain still applies block bodies already off the socket.
    let download_timed_out = if decode_waiting {
        None
    } else {
        s.block_download_timed_out(now, hub.params.btc.pow_target_spacing)
    };
    if let Some(hash) = download_timed_out {
        rbitcoin_log::info!("{}", block_download_timeout_log(hash, s.id));
        s.request_disconnect();
    } else if let Some(elapsed_secs) = ping_elapsed {
        rbitcoin_log::info!("{}", ping_timeout_log(elapsed_secs));
        s.request_disconnect();
    } else if let Some(nonce) = s.take_ping_send(now) {
        let _ = out_tx.send(NetworkMessage::Ping(nonce));
    }
    if let Some(ph) = s.peer_hub() {
        #[cfg(debug_assertions)]
        ph.note_heartbeat_lap();
        ph.on_session_heartbeat();
    }
    // A block or compact frame already sits in `decode_q`, even when it is
    // behind a front held for the send budget. Expiring the ask here forgets
    // it before that frame is applied.
    if !decode_queued {
        if maybe_expire_block_requests(
            &mut follow.requested_blocks,
            requested_since,
            std::time::Instant::now(),
            session,
        ) {
            drain_pending(
                hub,
                out_tx,
                &mut follow.pending_blocks,
                &mut follow.pending_headers,
                &mut follow.requested_blocks,
                getdata_use_compact(hub, follow.cmpct_version),
                session,
            )
            .await?;
            if !follow.requested_blocks.is_empty() {
                *requested_since = Some(std::time::Instant::now());
            }
        }
        maybe_expire_pending_cmpct(hub, follow, session, out_tx, std::time::Instant::now())?;
    }
    queue_due_tx_invs(hub, s, &follow.from_this_peer, out_tx);
    queue_due_parent_getdata(hub, s, out_tx);
    let _ = maybe_queue_local_addr(hub, s, out_tx);
    let _ = maybe_queue_initial_getheaders(out_tx, hub, s);
    match crate::peers::PendingSendCmpct::from_u8(s.pending_sendcmpct.swap(0, Ordering::Relaxed)) {
        crate::peers::PendingSendCmpct::Lb => {
            let _ = out_tx.send(NetworkMessage::SendCmpct(SendCmpct {
                send_compact: false,
                version: 2,
            }));
        }
        crate::peers::PendingSendCmpct::Hb => {
            let _ = out_tx.send(NetworkMessage::SendCmpct(SendCmpct {
                send_compact: true,
                version: 2,
            }));
        }
        crate::peers::PendingSendCmpct::None => {}
    }
    Ok(())
}

/// Announce a tip that moved during this session's handshake.
///
/// The outbound task subscribes after the handshake returns, so a block
/// mined in that window is not in `tip_rx`. A tip that was already in place
/// when the handshake started is not offered: the peer asks with getheaders.
/// `tip_at_handshake` is that earlier hash (`None` when there was no tip).
/// `Ok(true)` means the broadcast is closed and the session should exit.
pub async fn announce_unsent_tip(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    tip_at_handshake: Option<BlockHash>,
) -> Result<bool, NetError> {
    let Some(tip) = hub.tip_hash() else {
        return Ok(false);
    };
    if Some(tip) == tip_at_handshake {
        return Ok(false);
    }
    let sent = session.and_then(|s| s.header_marks().0);
    if sent == Some(tip) || session.is_some_and(|s| s.last_tip_offered() == Some(tip)) {
        return Ok(false);
    }
    match current_tip_announce(hub) {
        TipRecvAnnounce::Announce(ev) => {
            let offered_before = session.and_then(|s| s.last_tip_offered());
            let closed = on_tip_event(hub, out_tx, follow, session, Ok(ev)).await?;
            // A skipped offer records this tip. A send already recorded the
            // hash it queued, including a tip that moved during the await.
            if let Some(s) = session {
                if s.last_tip_offered() == offered_before {
                    s.note_last_tip_offered(hub.tip_hash().unwrap_or(tip));
                }
            }
            Ok(closed)
        }
        TipRecvAnnounce::Skip | TipRecvAnnounce::Closed => Ok(false),
    }
}

/// `true` means the tip broadcast closed; the session should exit.
pub async fn on_tip_event(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    tip: Result<crate::chain::TipEvent, broadcast::error::RecvError>,
) -> Result<bool, NetError> {
    let ev = match tip_event_for_announce(tip, hub) {
        TipRecvAnnounce::Closed => return Ok(true),
        TipRecvAnnounce::Skip => return Ok(false),
        TipRecvAnnounce::Announce(ev) => ev,
    };
    if !hub.meets_minimum_chain_work() {
        return Ok(false);
    }
    let from_peer = session.is_some_and(|s| s.take_block_from_peer(&ev.hash));
    let (sent, known) = session.map(|s| s.header_marks()).unwrap_or((None, None));
    // Core NewPoWValidBlock / SendMessages: compact is one-block tip-relay.
    // A lagged TipEvent (generate burst) or a peer who lacks pprev gets headers.
    if follow.send_cmpct
        && !from_peer
        && sent != Some(ev.hash)
        && hub.tip_hash() == Some(ev.hash)
        && (session.is_none()
            || (peer_has_header(hub, sent, known, ev.header.prev_blockhash)
                && !peer_has_header(hub, sent, known, ev.hash)))
    {
        if let Some(msg) = cmpct_tip_announce(hub, &ev.hash, follow.cmpct_version).await? {
            out_tx.send(msg)?;
            if let Some(s) = session {
                s.note_best_header_sent(ev.hash);
                s.note_last_tip_offered(ev.hash);
            }
            if !follow.wants_headers {
                return Ok(false);
            }
        }
    }
    match tip_announce_decision(hub, &ev, follow.wants_headers, sent, known, from_peer) {
        TipAnnounce::Skip => {}
        TipAnnounce::Inv(h) => {
            // An inv is not a header the peer has. `pindexBestHeaderSent`
            // stays unset so the next block after a long reorg is still an inv.
            // The hash is still the tip we offered, so a burst of other tips waits.
            if let Some(s) = session {
                s.note_last_tip_offered(h);
            }
            out_tx.send(NetworkMessage::Inv(vec![Inventory::Block(h)]))?;
        }
        TipAnnounce::Headers(hs) => {
            if let Some(last) = hs.last() {
                if let Some(s) = session {
                    let hash = last.block_hash();
                    s.note_best_header_sent(hash);
                    s.note_last_tip_offered(hash);
                }
            }
            out_tx.send(NetworkMessage::Headers(hs))?;
        }
    }
    Ok(false)
}

#[doc(hidden)]
pub fn on_headers_poll(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
) {
    let skip = session.is_some_and(|s| {
        s.conn_type == crate::peers::PeerConnType::AddrFetch
            || !should_poll_peer_headers(hub, s.best_known())
    });
    if !skip {
        let _ = queue_getheaders(out_tx, hub, session, false, None);
    }
    if let Some(mp) = hub.mempool() {
        mp.expire_stale_from_session();
    }
}

#[doc(hidden)]
pub fn on_tx_announce(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    ann: Option<Result<crate::tx_relay::MempoolAnnounce, broadcast::error::RecvError>>,
) -> Result<(), NetError> {
    // The announce only wakes this session. The global buckets decide which
    // wtxids are released, and `queue_due_tx_invs` queues that slice.
    if ann.is_some() {
        if let Some(s) = session {
            queue_due_tx_invs(hub, s, &follow.from_this_peer, out_tx);
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn on_tx_announce_ok(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    ann: crate::tx_relay::MempoolAnnounce,
) -> Result<(), NetError> {
    match announce_ready_txid(hub, &follow.from_this_peer, session, ann.txid, false) {
        ReadyInv::Queued(inv) => out_tx.send(NetworkMessage::Inv(vec![inv])),
        ReadyInv::Skipped | ReadyInv::Retry => Ok(()),
    }
}

/// One release's result. `Retry` means the mempool read was busy and the
/// session cursor must stay in front of this row so a later tick tries it
/// again; `Skipped` is a permanent per-peer decision.
enum ReadyInv {
    Queued(Inventory),
    Skipped,
    Retry,
}

/// Filter one released txid for `session`. Both the bucket release path and a
/// direct announce test use this one filter body.
fn announce_ready_txid(
    hub: &ChainHub,
    from_this_peer: &CappedSet<bitcoin::Txid>,
    session: Option<&crate::peers::LivePeer>,
    txid: bitcoin::Txid,
    released: bool,
) -> ReadyInv {
    if from_this_peer.contains_key(&txid) {
        return ReadyInv::Skipped;
    }
    let Some(mp) = hub.mempool() else {
        return ReadyInv::Skipped;
    };
    let Some(view) = mp.try_relay_inv_view(&txid) else {
        return ReadyInv::Retry;
    };
    let Some(view) = view else {
        return ReadyInv::Skipped;
    };
    if mp.skip_standing_inv(&txid)
        || !tx_announce_peer_ok_at(session, mp, &txid, released)
        || !(mp.relay_enabled() || mp.is_unbroadcast(&txid))
    {
        return ReadyInv::Skipped;
    }
    if let Some(s) = session {
        if tx_announce_inbound_gated(s, mp, &txid) && !s.session_noban() {
            s.count_tx_inv_to_send();
        }
    }
    // Core erases a below-filter tx from this peer's to-send set. A later
    // lower feefilter does not announce it (`p2p_feefilter.py`).
    let peer_min = session.map(|s| s.minfeefilter_sat_kvb()).unwrap_or(0);
    if peer_min > 0 {
        let modified = i128::from(view.fee_sat)
            .saturating_add(i128::from(mp.fee_delta(&txid)))
            .max(0) as u64;
        let rate = rbitcoin_consensus::policy::fee_rate_sat_per_kvb(modified, view.weight);
        if rate < peer_min {
            if let Some(s) = session {
                s.note_announced_tx(txid, view.wtxid);
            }
            return ReadyInv::Skipped;
        }
    }
    // A later rebroadcast still reaches a peer that has not heard this tx.
    // The claim is shared with flush, so the two cannot both queue. With no
    // session row, the peer never sent `wtxidrelay`.
    let inv = match session {
        Some(s) => s.claim_announced_tx(txid, view.wtxid),
        None => Some(Inventory::Transaction(txid)),
    };
    let Some(inv) = inv else {
        return ReadyInv::Skipped;
    };
    tx_announce_note_seq(session, mp, &view.wtxid);
    ReadyInv::Queued(inv)
}

#[doc(hidden)]
pub fn tx_announce_inbound_gated(
    s: &crate::peers::LivePeer,
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
) -> bool {
    s.inbound && tx_inv_peer_ok(s, mp, txid) && mp.relay_enabled()
}

/// A tx inv may go to `s`: not block-relay-only, and it asked for txs in
/// VERSION unless the tx is our own unbroadcast one.
fn tx_inv_peer_ok(
    s: &crate::peers::LivePeer,
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
) -> bool {
    s.conn_type != crate::peers::PeerConnType::BlockRelay && (s.relay || mp.is_unbroadcast(txid))
}

fn tx_announce_note_seq(
    session: Option<&crate::peers::LivePeer>,
    mp: &crate::tx_relay::MempoolHub,
    wtxid: &bitcoin::Wtxid,
) {
    let Some(s) = session else {
        return;
    };
    if let Some(seq) = mp.relay_seq_of(wtxid) {
        s.note_tx_inv_seq_at_least(seq.saturating_add(1));
    }
}

/// `None` when the tx left the pool after the ready check. Core skips such a
/// tx.
#[doc(hidden)]
pub fn tx_announce_wtxid(
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
) -> Option<bitcoin::Wtxid> {
    mp.try_get_tx(txid).map(|tx| tx.compute_wtxid())
}

#[doc(hidden)]
pub fn announce_fee_rate_sat_kvb(
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
) -> Option<u64> {
    let (fee, weight) = mp.try_get_live_meta(txid)?;
    // `prioritisetransaction` changes what we relay, not the base fee.
    let modified = i128::from(fee)
        .saturating_add(i128::from(mp.fee_delta(txid)))
        .max(0) as u64;
    Some(rbitcoin_consensus::policy::fee_rate_sat_per_kvb(
        modified, weight,
    ))
}

/// `released` is a wtxid the global bucket let out. An inbound relay peer
/// has already passed the 30 s gate by the time the release is read, so the
/// direct-announce inbound hold does not apply.
fn tx_announce_peer_ok_at(
    session: Option<&crate::peers::LivePeer>,
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
    released: bool,
) -> bool {
    session.is_none_or(|s| {
        tx_inv_peer_ok(s, mp, txid)
            && (released || !s.inbound || s.session_noban() || !mp.relay_enabled())
    })
}

#[doc(hidden)]
pub fn on_inv_flush(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    flush: Option<Result<(), broadcast::error::RecvError>>,
) {
    if matches!(flush, Some(Ok(()))) {
        if let Some(s) = session {
            queue_due_tx_invs(hub, s, &follow.from_this_peer, out_tx);
            queue_due_parent_getdata(hub, s, out_tx);
            let _ = maybe_queue_initial_getheaders(out_tx, hub, s);
        }
    }
}

struct OpenedPeerWriter {
    peer_s: String,
    out_tx: PeerOutTx,
    writer_task: tokio::task::JoinHandle<()>,
}

/// Handshake bytes and the writer task. The read loop starts only after this
/// returns, so a pong can land before the first inbound frame is applied.
async fn open_peer_writer(
    mut writer: PeerWriter,
    hub: &ChainHub,
    meta: &FollowSessionMeta,
) -> OpenedPeerWriter {
    let peer_s = meta
        .peer
        .map(|p| p.to_string())
        .unwrap_or_else(|| "peer".into());

    let session = meta.session.as_deref();
    let _ = write_msg_counted(&mut writer, session, NetworkMessage::SendHeaders).await;
    // BIP152: compact v2 low-bandwidth. HB is selected later (max 3, prefer outbound).
    let _ = write_msg_counted(
        &mut writer,
        session,
        NetworkMessage::SendCmpct(SendCmpct {
            send_compact: false,
            version: 2,
        }),
    )
    .await;
    // Handshake-writer ping, same nonce as LivePeer: connect_nodes needs pong
    // bytes before the writer task; a second ping makes the first pong mismatch.
    let keepalive = if let Some(s) = meta.session.as_ref() {
        s.take_ping_send(s.clock_now())
    } else {
        Some(rand_nonce())
    };
    if let Some(n) = keepalive {
        let _ = write_msg_counted(&mut writer, session, NetworkMessage::Ping(n)).await;
    }
    if let Some(fee_sat) = outbound_feefilter_sats(hub, session) {
        if write_msg_counted(&mut writer, session, NetworkMessage::FeeFilter(fee_sat))
            .await
            .is_ok()
        {
            if let Some(s) = session {
                s.note_sent_feefilter(fee_sat);
            }
        }
    }

    let (out_tx, out_rx) = match meta.session.as_ref() {
        Some(s) => s.open_out(),
        None => crate::send_queue::peer_out_channel(),
    };
    if let Some(s) = meta.session.as_ref() {
        let _ = maybe_queue_local_addr(hub, s, &out_tx);
    }

    let writer_session = meta.session.clone();
    let writer_task = tokio::spawn(run_writer_task(writer, out_rx, writer_session));
    if let Some(s) = meta.session.as_ref() {
        s.set_writer_abort(writer_task.abort_handle());
    }

    if let Some(s) = meta.session.as_ref() {
        let _ = maybe_queue_addrfetch_getaddr(&out_tx, s);
        let _ = maybe_queue_initial_getheaders(&out_tx, hub, s);
    } else if let Err(e) = queue_getheaders(&out_tx, hub, None, true, None) {
        rbitcoin_log::warn!("p2p: {peer_s} initial getheaders queue failed: {e}");
    }

    OpenedPeerWriter {
        peer_s,
        out_tx,
        writer_task,
    }
}

pub async fn peer_session_with(
    mut reader: PeerReader,
    writer: PeerWriter,
    magic: Magic,
    hub: Arc<ChainHub>,
    mut tip_rx: broadcast::Receiver<crate::chain::TipEvent>,
    meta: FollowSessionMeta,
) -> Result<(), NetError> {
    let _live_dec = LiveFollowDec(meta.live.clone());
    let opened = open_peer_writer(writer, hub.as_ref(), &meta).await;
    let peer_s = opened.peer_s;
    let out_tx = opened.out_tx;
    let mut writer_task = opened.writer_task;

    let mut owned_follow = PeerFollowState::new();
    let follow = &mut owned_follow;
    if let Some(s) = meta.session.as_ref() {
        follow.wtxid_relay = s.wtxid_relay();
    }
    let mut requested_since: Option<std::time::Instant> = None;
    // Heavy decodes wait for a pool-sized permit on the decode task. This
    // loop keeps reading: a ping or pong already on the socket must not sit
    // in the kernel buffer until a permit frees. Later frames stay behind
    // earlier ones in `decode_q`. The queue is capped; a peer that fills it
    // is disconnected. Timeouts stay suspended only while that capped queue
    // still holds a frame, so a pong already read is not dropped.
    let mut decode_q = DecodeQueue::new();
    let mut decode_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut rate = PeerRateLimiter::default_limits();
    let decoy_score = AtomicU32::new(0);
    let mut logged_invalid_v2 = false;
    let mut frame_contents_len: Option<usize> = None;
    let mut tx_announce_rx = hub.mempool().map(|m| m.subscribe_announces());
    let mut inv_flush_rx = hub.mempool().map(|m| m.subscribe_inv_flush());
    let mut headers_poll = tokio::time::interval(Duration::from_secs(HEADERS_POLL_SECS));
    headers_poll.tick().await;

    let session = meta.session.clone();
    let mut last_hb = std::time::Instant::now();
    let mut heartbeat_rx = session
        .as_ref()
        .and_then(|s| s.peer_hub())
        .map(|peers| peers.subscribe_session_heartbeat());
    let mut pending_tip: Option<Result<crate::chain::TipEvent, broadcast::error::RecvError>> = None;
    let mut tip_deadline: Option<tokio::time::Instant> = None;
    // The decode arm consumes a ready oneshot. A non-block frame at the
    // front waits here until the send budget clears, so a later frame
    // cannot pass it and the request is not dropped. It is not waiting on
    // a decode permit, so the heartbeat still times out a writer that never
    // drains.
    let mut hold_front = false;
    let result = async {
        // Drop tips already sitting in the receiver so catch-up and the
        // loop do not both announce them. A later block still arrives here.
        drain_queued_tips(&mut tip_rx);
        if hub.tip_hash() != meta.tip_at_handshake
            && announce_unsent_tip(
                hub.as_ref(),
                &out_tx,
                follow,
                session.as_deref(),
                meta.tip_at_handshake,
            )
            .await?
        {
            return Ok(());
        }
        loop {
            if session
                .as_ref()
                .is_some_and(|s| s.stop.load(Ordering::Relaxed))
            {
                apply_queued_decodes(
                    &mut decode_q,
                    hub.as_ref(),
                    &out_tx,
                    follow,
                    session.as_deref(),
                    &mut requested_since,
                    &peer_s,
                )
                .await?;
                return Ok(());
            }
            // Core `fPauseSend`: no inbound message is read while the writer
            // is past the send budget or a getdata / getblocktxn / cfilter tail waits
            // for serve room. Heartbeat (ping timeout, block-request expiry)
            // still runs.
            let over_budget = session.as_ref().is_some_and(|s| s.send_over_budget());
            // A decoded non-block frame is parked only when the send budget
            // flips after that frame is queued and before this loop polls it.
            // The read arm does not start while the budget is already blown,
            // so the suite cannot place that frame.
            #[cfg_attr(false, mutants::skip)]
            if !over_budget {
                hold_front = false;
            }
            // The decode arm and the heartbeat must agree on whether the
            // front is waiting to decode (suspend timeouts) or held for the
            // send budget (do not). Any queued frame still blocks request
            // expiry, including one sitting behind a held front.
            let decode_queued = !decode_q.is_empty();
            let front_waiting = decode_queued && !hold_front;
            #[cfg(debug_assertions)]
            if hold_front && decode_queued {
                if let Some(s) = session.as_ref() {
                    s.note_held_front_queued();
                }
            }
            let tail_pending = serve_tail_pending(follow);
            let reading = !over_budget && !tail_pending;
            let hb_wait = SESSION_HEARTBEAT.saturating_sub(last_hb.elapsed());
            decoy_score.store(follow.ban_score, Ordering::Relaxed);
            tokio::select! {
                biased;
                // Peer half-close / write failure: tear down so getpeerinfo
                // clears without waiting on a stuck read/decode arm.
                decoded = std::future::poll_fn(|cx| {
                    let Some(rx) = decode_q.front_mut() else {
                        return Poll::Pending;
                    };
                    std::pin::Pin::new(rx).poll(cx)
                }), if front_waiting => {
                    let n = decode_q.front_len();
                    let heavy = decode_q.front_heavy();
                    let _ = decode_q.pop_front();
                    if over_budget
                        && matches!(&decoded, Ok(Ok(msg)) if !queued_msg_is_block_body(msg))
                    {
                        if let Ok(msg) = decoded {
                            decode_q.push_front_ready(msg, n, heavy);
                            hold_front = true;
                            continue;
                        }
                    }
                    let mut slot = ReadyDecode {
                        queue: &mut decode_q,
                        requested_since: &mut requested_since,
                        peer: &peer_s,
                    };
                    apply_ready_decode(
                        decoded,
                        &mut slot,
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                    )
                    .await?;
                }
                writer_done = &mut writer_task => {
                    let _ = writer_done;
                    apply_queued_decodes(
                        &mut decode_q,
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                        &mut requested_since,
                        &peer_s,
                    )
                    .await?;
                    return Ok(());
                }
                // Inbound before local tip announce so GetData during a
                // generate burst is not queued behind hundreds of cmpctblocks.
                frame = reader.read_frame_with_progress(magic, |_, len| frame_contents_len = Some(len), |n| {
                    let mut score = decoy_score.load(Ordering::Relaxed);
                    let keep = crate::peer_dos::decoy_stays(
                        &mut rate,
                        &mut score,
                        n,
                        BAN_SCORE_THRESHOLD,
                    );
                    decoy_score.store(score, Ordering::Relaxed);
                    if keep {
                        Ok(())
                    } else {
                        Err(NetError::Protocol("peer misbehavior threshold"))
                    }
                }), if reading => {
                    follow.ban_score = decoy_score.load(Ordering::Relaxed);
                    let frame = match frame {
                        Ok(f) => f,
                        // Any socket Io means the peer is gone — exit cleanly so
                        // unregister runs inside the Core disconnect_nodes 5s wait.
                        Err(NetError::Io(_)) => {
                            apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await?;
                            return Ok(());
                        }
                        Err(NetError::Protocol("peer misbehavior threshold")) => {
                            apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await?;
                            return Err(threshold_disconnect(session.as_deref()));
                        }
                        Err(NetError::MessageTooLarge(n)) => {
                            follow.ban_score = follow.ban_score.saturating_add(OVERSIZE_BAN_SCORE);
                            rbitcoin_log::warn!(
                                "p2p: {peer_s} oversize frame ({n}) misbehavior={}",
                                follow.ban_score
                            );
                            apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await?;
                            if follow.ban_score >= BAN_SCORE_THRESHOLD {
                                return Err(threshold_disconnect(session.as_deref()));
                            }
                            return Err(NetError::MessageTooLarge(n));
                        }
                        Err(NetError::InvalidV2Type { contents_len }) => {
                            // Core stays connected; counts raw v2 size as `*other*`.
                            if !logged_invalid_v2 {
                                logged_invalid_v2 = true;
                                rbitcoin_log::debug!(P2p, "{}", crate::v2::v2_invalid_message_type_log());
                            }
                            if let Some(ref sess) = session {
                                sess.note_recv_v2("*other*", contents_len);
                            }
                            if !rate.note(contents_len) {
                                follow.ban_score =
                                    follow.ban_score.saturating_add(RATE_LIMIT_BAN_SCORE);
                                rbitcoin_log::warn!(
                                    "p2p: {peer_s} rate limit exceeded misbehavior={}",
                                    follow.ban_score
                                );
                                if follow.ban_score >= BAN_SCORE_THRESHOLD {
                                    apply_queued_decodes(
                                        &mut decode_q,
                                        hub.as_ref(),
                                        &out_tx,
                                        follow,
                                        session.as_deref(),
                                        &mut requested_since,
                                        &peer_s,
                                    )
                                    .await?;
                                    return Err(threshold_disconnect(session.as_deref()));
                                }
                            }
                            continue;
                        }
                        Err(e) => {
                            apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await?;
                            return Err(e);
                        }
                    };
                    if let Some(ref sess) = session {
                        let cmd = framed_cmd(&frame);
                        match frame_contents_len.take() {
                            Some(contents_len) => sess.note_recv_v2(&cmd, contents_len),
                            None => sess.note_recv(&cmd, frame.payload_len() as u64),
                        }
                    }
                    if !note_frame_rate(&mut rate, follow, &frame) {
                        follow.ban_score = follow.ban_score.saturating_add(RATE_LIMIT_BAN_SCORE);
                        rbitcoin_log::warn!(
                            "p2p: {peer_s} rate limit exceeded misbehavior={}",
                            follow.ban_score
                        );
                        if follow.ban_score >= BAN_SCORE_THRESHOLD {
                            apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await?;
                            return Err(threshold_disconnect(session.as_deref()));
                        }
                        apply_queued_decodes(
                            &mut decode_q,
                            hub.as_ref(),
                            &out_tx,
                            follow,
                            session.as_deref(),
                            &mut requested_since,
                            &peer_s,
                        )
                        .await?;
                        // Close to free this session's asks. A second strike refuses the address.
                        return Err(rate_limit_disconnect(session.as_deref()));
                    }
                    // A ping or pong already in `decode_q` is applied with those
                    // frames. A later pong must not move the clock ahead of an
                    // earlier block. `on_ping` holds its pong while a getdata,
                    // getblocktxn or cfilter tail is still unsent.
                    if (frame.is_ping() || frame.is_pong()) && !decode_q.is_empty() {
                        let n = frame.payload_len();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = tx.send(frame.try_decode());
                        push_decode_frame(
                            &mut decode_q,
                            rx,
                            n,
                            false,
                            DecodeDrain {
                                hub: hub.as_ref(),
                                out_tx: &out_tx,
                                follow,
                                session: session.as_deref(),
                                requested_since: &mut requested_since,
                                peer: &peer_s,
                            },
                        )
                        .await?;
                        continue;
                    }
                    if frame.is_pong() {
                        if let Some(s) = session.as_ref() {
                            if let Some(line) = s.on_pong(&frame.payload, s.clock_now()) {
                                rbitcoin_log::info!("{line}");
                            }
                        }
                        continue;
                    }
                    let n_req = follow.requested_blocks.len();
                    if frame.is_block() || frame.decode_is_cpu_heavy() {
                        let n = frame.payload_len();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let task = tokio::spawn(async move {
                            let _ = tx.send(decode_framed_offload(frame).await);
                        });
                        decode_tasks.retain(|t| !t.is_finished());
                        decode_tasks.push(task);
                        push_decode_frame(
                            &mut decode_q,
                            rx,
                            n,
                            true,
                            DecodeDrain {
                                hub: hub.as_ref(),
                                out_tx: &out_tx,
                                follow,
                                session: session.as_deref(),
                                requested_since: &mut requested_since,
                                peer: &peer_s,
                            },
                        )
                        .await?;
                        continue;
                    }
                    if !decode_q.is_empty() {
                        // A heavy decode is still in flight. This frame is
                        // cheap (no permit). Hold it behind the frames already
                        // queued so a block is not overtaken by the tx or inv
                        // that followed it on the wire.
                        let n = frame.payload_len();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = tx.send(frame.try_decode());
                        push_decode_frame(
                            &mut decode_q,
                            rx,
                            n,
                            false,
                            DecodeDrain {
                                hub: hub.as_ref(),
                                out_tx: &out_tx,
                                follow,
                                session: session.as_deref(),
                                requested_since: &mut requested_since,
                                peer: &peer_s,
                            },
                        )
                        .await?;
                        continue;
                    }
                    handle_peer_frame(
                        frame,
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                    )
                    .await?;
                    let n_after = follow.requested_blocks.len();
                    if n_after == 0 {
                        requested_since = None;
                    } else if n_after < n_req || n_req == 0 {
                        requested_since = Some(std::time::Instant::now());
                    }
                    if misbehavior_disconnects(follow.ban_score, session.as_deref()) {
                        rbitcoin_log::warn!(
                            "{}",
                            misbehavior_disconnect_log(&peer_s, follow.ban_score)
                        );
                        return Err(threshold_disconnect(session.as_deref()));
                    }
                }
                tip = tip_rx.recv(), if !over_budget => {
                    let mut tip = tip;
                    loop {
                        match tip_rx.try_recv() {
                            Ok(ev) => tip = Ok(ev),
                            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                                tip = Err(broadcast::error::RecvError::Lagged(0));
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    // A reorg is not a child of the header this peer was sent.
                    // Hold it so a burst becomes one announcement. A normal
                    // extend, and a closed channel, go out immediately.
                    let batch = pending_tip.is_some()
                        || tip_is_reorg_vs_sent(hub.as_ref(), session.as_deref(), &tip);
                    if batch {
                        pending_tip = Some(tip);
                        tip_deadline = Some(tokio::time::Instant::now() + TIP_ANNOUNCE_BATCH);
                    } else if on_tip_event(
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                        tip,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
                _ = async {
                    match tip_deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if pending_tip.is_some() && !over_budget => {
                    tip_deadline = None;
                    if let Some(tip) = pending_tip.take() {
                        if on_tip_event(
                            hub.as_ref(),
                            &out_tx,
                            follow,
                            session.as_deref(),
                            tip,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                    }
                }
                _ = headers_poll.tick(), if !over_budget => {
                    on_headers_poll(hub.as_ref(), &out_tx, session.as_deref());
                }
                ann = async {
                    if let Some(rx) = tx_announce_rx.as_mut() {
                        Some(rx.recv().await)
                    } else {
                        std::future::pending::<()>().await;
                        None
                    }
                }, if !over_budget => {
                    on_tx_announce(
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                        ann,
                    )?;
                }
                flush = async {
                    if let Some(rx) = inv_flush_rx.as_mut() {
                        Some(rx.recv().await)
                    } else {
                        std::future::pending::<()>().await;
                        None
                    }
                }, if !over_budget => {
                    on_inv_flush(
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                        flush,
                    );
                }
                // Inbound is biased first so a queued pong beats ping-timeout.
                _ = session_heartbeat_due(&mut heartbeat_rx, hb_wait), if session.is_some() => {
                    on_heartbeat(
                        hub.as_ref(),
                        &out_tx,
                        follow,
                        session.as_deref(),
                        &mut requested_since,
                        &mut tx_announce_rx,
                        &mut inv_flush_rx,
                        front_waiting,
                        decode_queued,
                    )
                    .await?;
                    last_hb = std::time::Instant::now();
                    continue;
                }
                // After the heartbeat: a tail that keeps finding room must
                // not starve the ping timeout.
                () = async {
                    if let Some(s) = session.as_deref() {
                        s.wait_send_budget().await;
                    }
                }, if over_budget && !tail_pending => {}
                room = async {
                    match session.as_deref() {
                        Some(s) => s.wait_serve_room(&out_tx).await,
                        None => true,
                    }
                }, if tail_pending => {
                    if !room {
                        apply_queued_decodes(
                            &mut decode_q,
                            hub.as_ref(),
                            &out_tx,
                            follow,
                            session.as_deref(),
                            &mut requested_since,
                            &peer_s,
                        )
                        .await?;
                        return Ok(());
                    }
                    if !follow.getdata_tail.is_empty() {
                        if let Err(e) = serve_getdata_tail(
                            hub.as_ref(),
                            &out_tx,
                            follow,
                            session.as_deref(),
                        )
                        .await
                        {
                            let _ = apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await;
                            return Err(e);
                        }
                    }
                    if !follow.getblocktxn_tail.is_empty() {
                        if let Err(e) = serve_getblocktxn_tail(
                            hub.as_ref(),
                            &out_tx,
                            follow,
                            session.as_deref(),
                        )
                        .await
                        {
                            let _ = apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await;
                            return Err(e);
                        }
                    }
                    if !follow.cfilter_tail.is_empty() || !follow.cfilter_requests.is_empty() {
                        if let Err(e) =
                            resume_cfilters(hub.as_ref(), &out_tx, follow, session.as_deref()).await
                        {
                            let _ = apply_queued_decodes(
                                &mut decode_q,
                                hub.as_ref(),
                                &out_tx,
                                follow,
                                session.as_deref(),
                                &mut requested_since,
                                &peer_s,
                            )
                            .await;
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
    .await;

    // A `?` in the tip or tx-announce arm leaves frames that were already
    // read. Apply them before dropping the follow state releases their asks.
    let drained = apply_queued_decodes(
        &mut decode_q,
        hub.as_ref(),
        &out_tx,
        follow,
        session.as_deref(),
        &mut requested_since,
        &peer_s,
    )
    .await;
    let result = match (result, drained) {
        (Err(e), _) => Err(e),
        (Ok(()), drained) => drained,
    };

    drop(out_tx);
    // If select! already joined the writer, do not await again (would pend).
    if writer_task.is_finished() {
        drop(writer_task);
    } else {
        writer_task.abort();
        let _ = writer_task.await;
    }
    match &result {
        Ok(()) => rbitcoin_log::debug!(P2p, "session {peer_s} closed"),
        Err(e) => rbitcoin_log::warn!("p2p: session {peer_s} ended: {e}"),
    }
    result
}

/// Tip locator for post-handshake `getheaders` (store chain; genesis fallback).
pub fn tip_follow_locator(hub: &ChainHub) -> Vec<BlockHash> {
    match hub.query.locator_hashes() {
        // No tip is the zero hash, so a success is never empty. `truncate`
        // past the length does nothing, and a longer locator keeps the front.
        Ok(mut v) => {
            v.truncate(MAX_LOCATOR_SZ);
            v
        }
        _ => {
            let mut v = Vec::new();
            if let Some(t) = hub.tip_hash() {
                v.push(t);
            }
            v.push(BlockHash::from_byte_array([0u8; 32]));
            v
        }
    }
}

/// Locator for `getheaders`. `from` is the last header of a full 2000-header
/// reply so the next batch starts after it (Core continues from that hash).
pub fn headers_sync_locator(hub: &ChainHub, from: Option<BlockHash>) -> Vec<BlockHash> {
    match from {
        None => tip_follow_locator(hub),
        Some(start) => locator_from_start(hub, start),
    }
}

#[doc(hidden)]
pub fn locator_from_start(hub: &ChainHub, start: BlockHash) -> Vec<BlockHash> {
    if let Some(h) = hub.query.published_height_of_hash(&start.to_byte_array()) {
        return locator_from_height(hub, h.0);
    }
    let mut out = vec![start];
    push_genesis_locator(hub, &mut out);
    out
}

#[doc(hidden)]
pub fn locator_from_height(hub: &ChainHub, start: u32) -> Vec<BlockHash> {
    let mut out = Vec::new();
    let mut h = start as i64;
    let mut step = 1i64;
    while h >= 0 {
        match hub.query.header_at_height(Height(h as u32)) {
            Ok(Some((_, rec))) => out.push(BlockHash::from_byte_array(rec.hash)),
            _ => break,
        }
        if out.len() >= 10 {
            // `saturating_mul` stays positive. A dividing step sticks at zero.
            step = step.saturating_mul(2);
        }
        h -= step;
        if out.len() >= MAX_LOCATOR_SZ {
            break;
        }
    }
    push_genesis_locator(hub, &mut out);
    out
}

#[doc(hidden)]
pub fn push_genesis_locator(hub: &ChainHub, out: &mut Vec<BlockHash>) {
    let g = hub.params.genesis_hash;
    if out.last() != Some(&g) {
        out.push(g);
    }
}

/// Periodic getheaders is for discovering more work. Skip peers whose best
/// known header is already on our chain behind tip, or a connecting fork that
/// cannot beat us.
pub fn should_poll_peer_headers(hub: &ChainHub, best_known: Option<BlockHash>) -> bool {
    let Some(best) = best_known else {
        return true;
    };
    let our_tip = hub.tip_height().unwrap_or(0);
    if let Some(h) = hub.query.published_height_of_hash(&best.to_byte_array()) {
        return h.0 >= our_tip;
    }
    let empty = HashMap::new();
    !matches!(
        header_branch_vs_tip(hub, &empty, best),
        Some(std::cmp::Ordering::Less)
    )
}

/// Start Core's single initial headers sync on this session.
///
/// The header walk does not see this session. Skipping the ask while the
/// tip is older than 24 hours left regtest peers with an empty block queue.
/// [`crate::peers::PeerHub::try_start_headers_sync`] still allows one peer
/// until the tip is caught up.
#[doc(hidden)]
pub fn maybe_queue_initial_getheaders(
    out: &PeerOutTx,
    hub: &ChainHub,
    session: &crate::peers::LivePeer,
) -> bool {
    if session.conn_type == crate::peers::PeerConnType::AddrFetch {
        return false;
    }
    if session.is_sync_started() {
        return false;
    }
    let now = session.clock_now();
    let best_t = hub.tip_header().map(|h| u64::from(h.time)).unwrap_or(0);
    let started = session
        .peer_hub()
        .is_some_and(|ph| ph.try_start_headers_sync(session, now, best_t));
    if started {
        let h = hub.tip_height().unwrap_or(0);
        rbitcoin_log::info!("{}", crate::chain::initial_getheaders_log(h, session.id));
        let _ = queue_getheaders(out, hub, Some(session), true, None);
    }
    started
}

#[doc(hidden)]
pub fn maybe_queue_addrfetch_getaddr(out: &PeerOutTx, session: &crate::peers::LivePeer) -> bool {
    if session.conn_type != crate::peers::PeerConnType::AddrFetch {
        return false;
    }
    session.credit_getaddr_reply();
    let _ = out.send(NetworkMessage::GetAddr);
    true
}

#[doc(hidden)]
pub fn addrfetch_timed_out(session: &crate::peers::LivePeer) -> bool {
    session.conn_type == crate::peers::PeerConnType::AddrFetch
        && session.clock_now().saturating_sub(session.connected_at()) > ADDRFETCH_TIMEOUT_SECS
}

#[doc(hidden)]
pub fn queue_getheaders(
    out: &PeerOutTx,
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    mark_awaiting: bool,
    from: Option<BlockHash>,
) -> Result<(), NetError> {
    if mark_awaiting {
        if let Some(s) = session {
            // Core `MaybeSendGetHeaders`: one in-flight getheaders at a time
            // (or after HEADERS_RESPONSE_TIME = 2 min).
            if s.is_awaiting_headers() {
                return Ok(());
            }
            s.note_awaiting_headers();
        }
    }
    let locator = headers_sync_locator(hub, from);
    let gh = GetHeadersMessage::new(locator, BlockHash::from_byte_array([0u8; 32]));
    out.send(NetworkMessage::GetHeaders(gh))
}

/// BIP152: request `MSG_CMPCT_BLOCK` when the peer speaks compact v2 and
/// [`compact_blocks_ok`] is true. Blocks-only, and a mempool that is not
/// receiving transactions, keep `MSG_WITNESS_BLOCK`.
#[doc(hidden)]
pub fn getdata_use_compact(hub: &ChainHub, peer_cmpct_version: u32) -> bool {
    peer_cmpct_version == 2 && compact_blocks_ok(hub)
}

/// Drop inflight body asks the peer never answered so `asked_blocks` cannot
/// permanently skip the same hashes (`rpc_createmultisig` generate-149
/// `sync_blocks` 60s). A hash the download window still lists stays, so the
/// ping suspend holds until that window times the peer out.
pub fn maybe_expire_block_requests(
    requested: &mut RequestedBlocks,
    since: &mut Option<std::time::Instant>,
    now: std::time::Instant,
    session: Option<&crate::peers::LivePeer>,
) -> bool {
    if requested.is_empty() {
        *since = None;
        return false;
    }
    let start = *since.get_or_insert(now);
    if now.saturating_duration_since(start) < BLOCK_GETDATA_TIMEOUT {
        return false;
    }
    let stale: Vec<BlockHash> = requested
        .hashes()
        .iter()
        .copied()
        .filter(|h| session.is_none_or(|s| !s.block_download_owns(h)))
        .collect();
    if stale.is_empty() {
        return false;
    }
    for h in stale {
        requested.remove(&h);
        if let Some(s) = session {
            s.release_cmpct_taken(h);
        }
    }
    if requested.is_empty() {
        *since = None;
    }
    true
}

#[doc(hidden)]
pub fn maybe_expire_pending_cmpct(
    hub: &ChainHub,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    out: &PeerOutTx,
    now: std::time::Instant,
) -> Result<bool, NetError> {
    let stale: Vec<BlockHash> = follow
        .pending_cmpct
        .iter()
        .filter(|(_, pc)| now.saturating_duration_since(pc.since) >= BLOCK_GETDATA_TIMEOUT)
        .map(|(h, _)| *h)
        .collect();
    if stale.is_empty() {
        return Ok(false);
    }
    for hash in stale {
        drop_pending_cmpct(follow, session, hash);
        queue_block_getdata(
            hub,
            out,
            session,
            &mut follow.requested_blocks,
            &[hash],
            false,
        )?;
    }
    Ok(true)
}

#[doc(hidden)]
pub fn queue_block_getdata(
    hub: &ChainHub,
    out: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    requested_blocks: &mut RequestedBlocks,
    want: &[BlockHash],
    compact: bool,
) -> Result<(), NetError> {
    if want.is_empty() {
        return Ok(());
    }
    let inv: Vec<Inventory> = want
        .iter()
        .map(|h| {
            if compact && compact_getdata_near_tip(hub, h) {
                Inventory::CompactBlock(*h)
            } else {
                Inventory::WitnessBlock(*h)
            }
        })
        .collect();
    for h in want {
        requested_blocks.ask(hub, *h);
    }
    if let Some(s) = session {
        s.note_blocks_requested(want);
    }
    for chunk in inv.chunks(MAX_INV_SIZE.min(500)) {
        out.send(NetworkMessage::GetData(chunk.to_vec()))?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn net_error_needs_parent(e: &NetError) -> bool {
    matches!(e, NetError::UnknownParent)
}

/// Incomplete compact block waiting for `blocktxn`.
#[doc(hidden)]
pub struct PendingCmpct {
    pub hsi: HeaderAndShortIds,
    pub partial: crate::compact::CmpctPartial,
    pub fill: Option<crate::compact::CmpctFillSets>,
    pub since: std::time::Instant,
}

fn shortid_avail(
    mp: Option<&crate::tx_relay::MempoolHub>,
    hsi: &HeaderAndShortIds,
    version: u32,
) -> (
    HashMap<bitcoin::bip152::ShortId, Vec<Transaction>>,
    Option<crate::compact::CmpctFillSets>,
) {
    let pref: Vec<bitcoin::Wtxid> = hsi
        .prefilled_txs
        .iter()
        .map(|p| p.tx.compute_wtxid())
        .collect();
    match mp
        .and_then(|mp| mp.try_cmpct_avail(&hsi.header, hsi.nonce, version, &hsi.short_ids, &pref))
    {
        Some((txs, fill)) => (txs, Some(fill)),
        None => (HashMap::new(), None),
    }
}

#[derive(Debug)]
#[doc(hidden)]
pub enum CmpctReconstruct {
    Block(Block, Option<Box<crate::compact::CmpctFillSets>>),
    NeedTxn(
        crate::compact::CmpctPartial,
        Option<Box<crate::compact::CmpctFillSets>>,
    ),
    GetData,
}

/// One reconstruct: full block, getblocktxn indexes, or no mempool (`None` → getdata).
#[doc(hidden)]
pub fn try_reconstruct_cmpct(
    hub: &ChainHub,
    hsi: &HeaderAndShortIds,
    version: u32,
) -> Option<CmpctReconstruct> {
    reconstruct_cmpct_from_mempool(hub.mempool().map(Arc::as_ref), hsi, version)
}

fn reconstruct_cmpct_from_mempool(
    mp: Option<&crate::tx_relay::MempoolHub>,
    hsi: &HeaderAndShortIds,
    version: u32,
) -> Option<CmpctReconstruct> {
    crate::reactor::assert_not_reactor("compact reconstruct");
    let (owned, fill) = shortid_avail(mp, hsi, version);
    match crate::compact::reconstruct(hsi, &owned) {
        crate::compact::Reconstruct::Block(block) => {
            Some(CmpctReconstruct::Block(block, fill.map(Box::new)))
        }
        crate::compact::Reconstruct::Fail => Some(CmpctReconstruct::GetData),
        crate::compact::Reconstruct::Partial(_) if mp.is_none() => None,
        crate::compact::Reconstruct::Partial(p) => {
            Some(CmpctReconstruct::NeedTxn(p, fill.map(Box::new)))
        }
    }
}

#[doc(hidden)]
pub fn log_cmpct_filled(
    hub: &ChainHub,
    hsi: &HeaderAndShortIds,
    block: &Block,
    fetched: &[u64],
    fill: Option<&crate::compact::CmpctFillSets>,
) {
    let wtxids = crate::compact::block_wtxids(block);
    let stats = crate::compact::reconstruct_stats_from_wtxids(
        hsi,
        block,
        fill.unwrap_or(&crate::compact::CmpctFillSets::default()),
        fetched,
        &wtxids,
    );
    rbitcoin_log::info!("{stats}");
    if let Some(fill) = fill {
        let indexes = crate::compact::prefill_indexes_from_wtxids(block, fill, &wtxids);
        hub.remember_cmpct_prefill(block.block_hash(), block.header.prev_blockhash, indexes);
    }
}

#[doc(hidden)]
pub fn log_cmpct_getdata(hash: BlockHash, missing_n: usize) {
    rbitcoin_log::info!(
        "{}",
        crate::compact::reconstruct_getdata_stats(hash, missing_n)
    );
}

/// Flush due / unbroadcast tx INVs onto every live session writer.
/// Used by RPC sendraw and whitelist-relay accept (`p2p_blocksonly`).
pub fn flush_tx_invs(hub: &ChainHub, peers: &crate::peers::PeerHub) {
    let rows = peers.live_peers();
    for s in rows {
        if let Some(out) = s.writer() {
            queue_due_tx_invs(hub, s.as_ref(), &CappedSet::new(), &out);
            queue_due_parent_getdata(hub, s.as_ref(), &out);
        }
    }
}

/// INV one live mempool tx to every peer that has not seen it, ignoring the
/// inbound age gate and `inv_gen_floor` (Core same-nonwitness rebroadcast).
pub fn force_announce_txid(hub: &ChainHub, peers: &crate::peers::PeerHub, txid: bitcoin::Txid) {
    let Some(mp) = hub.mempool() else {
        return;
    };
    if mp.skip_standing_inv(&txid) {
        return;
    }
    let Some(tx) = mp.try_get_tx(&txid) else {
        return;
    };
    let w = tx.compute_wtxid();
    for s in peers.live_peers() {
        if !tx_inv_peer_ok(&s, mp, &txid) {
            continue;
        }
        let peer_min = s.minfeefilter_sat_kvb();
        if peer_min > 0 && announce_fee_rate_sat_kvb(mp, &txid).is_some_and(|rate| rate < peer_min)
        {
            continue;
        }
        let Some(out) = s.writer() else {
            continue;
        };
        let Some(inv) = s.claim_announced_tx(txid, w) else {
            continue;
        };
        queue_tx_inv_batches(&out, vec![inv]);
        if let Some(seq) = mp.relay_seq_of(&w) {
            s.note_tx_inv_seq_at_least(seq.saturating_add(1));
        }
    }
}

#[doc(hidden)]
pub fn maybe_force_relay_recent_reject(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    mp: &crate::tx_relay::MempoolHub,
    txid: bitcoin::Txid,
    wtxid: bitcoin::Wtxid,
) {
    if !session.is_some_and(|s| s.session_forcerelay()) {
        return;
    }
    let id = session.map(|s| s.id).unwrap_or(0);
    if mp.try_contains(&txid) {
        rbitcoin_log::info!("p2p: Force relaying tx {txid} (wtxid={wtxid}) from peer={id}");
        if let Some(ph) = session.and_then(|s| s.peer_hub()) {
            force_announce_txid(hub, ph.as_ref(), txid);
        }
    } else {
        rbitcoin_log::info!(
            "p2p: Not relaying non-mempool transaction {txid} (wtxid={wtxid}) from forcerelay peer={id}"
        );
    }
}

#[doc(hidden)]
pub fn maybe_force_relay_duplicate(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    tx: &Transaction,
    tid: bitcoin::Txid,
) {
    if !session.is_some_and(|s| s.session_forcerelay()) {
        return;
    }
    let id = session.map(|s| s.id).unwrap_or(0);
    rbitcoin_log::info!(
        "p2p: Force relaying tx {tid} (wtxid={}) from peer={id}",
        tx.compute_wtxid()
    );
    if let Some(ph) = session.and_then(|s| s.peer_hub()) {
        force_announce_txid(hub, ph.as_ref(), tid);
    }
}

#[doc(hidden)]
pub fn queue_due_tx_invs(
    hub: &ChainHub,
    session: &crate::peers::LivePeer,
    from_this_peer: &CappedSet<bitcoin::Txid>,
    out_tx: &PeerOutTx,
) {
    let Some(mp) = hub.mempool() else {
        return;
    };
    if session.conn_type == crate::peers::PeerConnType::BlockRelay
        || (!session.relay && mp.unbroadcast_count() == 0)
    {
        return;
    }
    // The buckets are global: whichever session ticks first releases the
    // slice this instant's tokens cover. Every session then reads its own
    // cursor over the released log. Bucket time is the mempool's mock/wall
    // clock, so `setmocktime` refills the same buckets every session reads.
    let now = mp.relay_now_secs();
    mp.drain_inv_backlog(now.saturating_mul(1000));

    // `-blocksonly` (relay off) still INV locally submitted (unbroadcast)
    // txs immediately (`p2p_blocksonly.py:48`). When relay is on, inbound
    // keeps the 30s age gate (`mempool_reorg.py:71`).
    let inbound_age_gate = session.inbound && mp.relay_enabled() && !session.session_noban();
    let dir = crate::inv_bucket::InvDirection::for_inbound(session.inbound);
    let (last, released) = mp.take_released_invs(
        dir,
        session.inv_released_seen(),
        crate::tx_relay::RELEASED_INV_SCAN_MAX,
    );
    let had_released = !released.is_empty();
    // Do not move the cursor past a row whose mempool read was busy.
    let mut cursor = session.inv_released_seen();
    if released.is_empty() {
        cursor = last;
    }
    let mut due = Vec::new();
    let total = released.len();
    let mut waiting = 0usize;
    for (i, (seq, txid, w)) in released.into_iter().enumerate() {
        // The cursor stays on a live row whose 30 s age has not elapsed.
        // A dead row was already skipped by `take_released_invs`.
        if inbound_age_gate && !mp.tx_inv_due(&w) {
            waiting = total - i;
            break;
        }
        // A tx accepted before this session's handshake is not leaked to it
        // (`p2p_tx_privacy`). A direct announce already ran this gate at
        // accept time; a released log can be read by a later session.
        if mp
            .accept_gen(&w)
            .is_some_and(|g| g < session.inv_gen_floor())
        {
            cursor = seq;
            continue;
        }
        match announce_ready_txid(hub, from_this_peer, Some(session), txid, true) {
            ReadyInv::Queued(inv) => {
                due.push(inv);
                cursor = seq;
            }
            ReadyInv::Skipped => cursor = seq,
            ReadyInv::Retry => break,
        }
    }
    session.note_inv_released_seen(cursor);
    if !due.is_empty() {
        queue_tx_inv_batches(out_tx, due);
    }
    if had_released {
        // The releases are queued or skipped by now. The per-tx note raised
        // `last_inv_sequence`; this clears the queued count in the same lock.
        session.note_tx_inv_batch_sent(session.last_inv_sequence());
    }
    for _ in 0..waiting {
        session.count_tx_inv_to_send();
    }
}

/// Finish a pending compact block with a `blocktxn` payload.
#[doc(hidden)]
#[allow(clippy::result_unit_err)] // a short blocktxn is (); the caller scores the peer
pub fn apply_cmpct_blocktxn(
    pc: &mut PendingCmpct,
    bt: &BlockTransactions,
) -> Result<(Block, Option<crate::compact::CmpctFillSets>), ()> {
    let block =
        crate::compact::apply_block_transactions(&pc.hsi, &mut pc.partial, bt).map_err(|_| ())?;
    Ok((block, pc.fill.take()))
}

#[doc(hidden)]
pub struct PeerFollowState {
    pub wants_headers: bool,
    pub wtxid_relay: bool,
    pub send_cmpct: bool,
    pub cmpct_version: u32,
    pub pending_headers: HashMap<BlockHash, bitcoin::block::Header>,
    pub pending_blocks: PendingBlocks,
    pub pending_cmpct: HashMap<BlockHash, PendingCmpct>,
    pub from_this_peer: CappedSet<bitcoin::Txid>,
    pub requested_blocks: RequestedBlocks,
    /// Asks in `requested_blocks` whose one rate-exempt `block` frame has
    /// arrived ([`note_frame_rate`]). Pruned to `requested_blocks` on every
    /// frame, so it never outgrows the open asks, and a later ask for the
    /// same hash is exempt again.
    pub unmetered_bodies: HashSet<BlockHash>,
    pub ban_score: u32,
    /// Unserved rest of an inbound `getdata` paused on the writer (Core
    /// `m_getdata_requests`). The session reads no frame while it is
    /// non-empty, so it holds at most one message: `MAX_INV_SIZE` items,
    /// about 2 MB per session. That RAM keeps every hash served in order
    /// instead of dropping the rest or holding the reader across items.
    pub getdata_tail: VecDeque<Inventory>,
    /// One parked `getblocktxn`. The session reads no frame while this is
    /// non-empty, so a peer that never reads cannot stack reconstructs.
    pub getblocktxn_tail: VecDeque<BlockTransactionsRequest>,
    /// Unsent replies of one compact-filter request paused on the writer
    /// (#2078): up to 1,000 filters, already read in full before the first
    /// is queued. Only one request's replies are held at a time.
    pub cfilter_tail: VecDeque<NetworkMessage>,
    /// Compact-filter requests applied while a range was parked, waiting
    /// for it (a frame decoded before the pause). Their replies are read
    /// only once the tail is empty, so they cost their request bytes, and
    /// the decode queue's frame cap bounds them.
    pub cfilter_requests: VecDeque<NetworkMessage>,
    /// Ping nonce held until every serve tail is empty. Queuing the pong
    /// earlier lets it leave before the rest of that reply. A later ping
    /// replaces this nonce.
    pub parked_pong: Option<u64>,
    /// Last hash of a full 500-entry `getblocks` page (Core
    /// `m_continuation_block`). Serving that block queues one `MSG_BLOCK` inv
    /// of the tip, which makes the peer send the next `getblocks`.
    pub continuation_block: Option<BlockHash>,
    /// Parked headers that do not reach a header this node has. The value is
    /// the unknown parent and the pending-walk length from this hash to that
    /// parent. A path walk stops at a marked hash.
    pending_no_base: Arc<NoBaseMap>,
    /// Marks whose recorded parent is this hash. Admitting the hash rewrites
    /// or drops them. The header job does not read this index.
    pending_no_base_kids: HashMap<BlockHash, HashSet<BlockHash>>,
}

impl PeerFollowState {
    /// Core `vBlocksInFlight`: a body asked by `getdata`, or a compact
    /// block waiting on `blocktxn`.
    pub fn has_blocks_in_flight(&self) -> bool {
        !self.requested_blocks.is_empty() || !self.pending_cmpct.is_empty()
    }

    pub fn block_in_flight(&self, hash: &BlockHash) -> bool {
        self.requested_blocks.contains(hash) || self.pending_cmpct.contains_key(hash)
    }

    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            wants_headers: false,
            wtxid_relay: false,
            send_cmpct: false,
            cmpct_version: 0,
            pending_headers: HashMap::new(),
            pending_blocks: PendingBlocks::new(),
            pending_cmpct: HashMap::new(),
            from_this_peer: CappedSet::new(),
            requested_blocks: RequestedBlocks::default(),
            unmetered_bodies: HashSet::new(),
            ban_score: 0,
            getdata_tail: VecDeque::new(),
            getblocktxn_tail: VecDeque::new(),
            cfilter_tail: VecDeque::new(),
            cfilter_requests: VecDeque::new(),
            parked_pong: None,
            continuation_block: None,
            pending_no_base: Arc::new(NoBaseMap::new()),
            pending_no_base_kids: HashMap::new(),
        }
    }
}

#[doc(hidden)]
pub fn serve_mempool_getdata(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    item: &Inventory,
    tx: Option<bitcoin::Transaction>,
) -> Result<bool, NetError> {
    let Some(mp) = hub.mempool() else {
        return Ok(false);
    };
    let Some(tx) = tx else {
        return Ok(false);
    };
    let txid = tx.compute_txid();
    let wtxid = tx.compute_wtxid();
    let announced = session.is_some_and(|s| s.has_announced(&s.tx_announcement(txid, wtxid)));
    let last_inv = session.map(|s| s.last_inv_sequence()).unwrap_or(1);
    if announced || mp.is_relay_servable(&wtxid, last_inv) {
        mp.mark_broadcast(&txid);
        out_tx.send(NetworkMessage::Tx(getdata_tx(item, tx)))?;
        return Ok(true);
    }
    Ok(false)
}

fn note_block_request_window(
    follow: &PeerFollowState,
    n_req: usize,
    requested_since: &mut Option<std::time::Instant>,
) {
    let n_after = follow.requested_blocks.len();
    if n_after == 0 {
        *requested_since = None;
    } else if n_after < n_req || n_req == 0 {
        *requested_since = Some(std::time::Instant::now());
    }
}

struct ReadyDecode<'a> {
    queue: &'a mut DecodeQueue,
    requested_since: &'a mut Option<std::time::Instant>,
    peer: &'a str,
}

struct DecodeDrain<'a> {
    hub: &'a ChainHub,
    out_tx: &'a PeerOutTx,
    follow: &'a mut PeerFollowState,
    session: Option<&'a crate::peers::LivePeer>,
    requested_since: &'a mut Option<std::time::Instant>,
    peer: &'a str,
}

async fn push_decode_frame(
    decode_q: &mut DecodeQueue,
    rx: DecodeRx,
    n: usize,
    heavy: bool,
    drain: DecodeDrain<'_>,
) -> Result<(), NetError> {
    if !decode_q.push(rx, n, heavy, drain.session) {
        return Ok(());
    }
    finish_decode_queue_full(
        decode_q,
        drain.hub,
        drain.out_tx,
        drain.follow,
        drain.session,
        drain.requested_since,
        drain.peer,
    )
    .await
}

async fn finish_decode_queue_full(
    decode_q: &mut DecodeQueue,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    requested_since: &mut Option<std::time::Instant>,
    peer_s: &str,
) -> Result<(), NetError> {
    rbitcoin_log::warn!("p2p: {peer_s} decode queue full");
    apply_queued_decodes(
        decode_q,
        hub,
        out_tx,
        follow,
        session,
        requested_since,
        peer_s,
    )
    .await?;
    Err(decode_queue_disconnect(session))
}

/// Apply one frame that already left the socket. A dropped decode task or a
/// failed apply still drains frames behind it, so dropping the follow state
/// does not release their asks.
async fn apply_ready_decode(
    decoded: Result<Result<RawNetworkMessage, NetError>, tokio::sync::oneshot::error::RecvError>,
    slot: &mut ReadyDecode<'_>,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    let decoded = match decoded {
        Ok(r) => r,
        Err(_) => {
            let _ = apply_queued_decodes(
                slot.queue,
                hub,
                out_tx,
                follow,
                session,
                slot.requested_since,
                slot.peer,
            )
            .await;
            return Err(NetError::Protocol("decode task dropped"));
        }
    };
    let n_req = follow.requested_blocks.len();
    let applied = apply_decoded_peer_msg(decoded, hub, out_tx, follow, session).await;
    note_block_request_window(follow, n_req, slot.requested_since);
    if let Err(e) = applied {
        let _ = apply_queued_decodes(
            slot.queue,
            hub,
            out_tx,
            follow,
            session,
            slot.requested_since,
            slot.peer,
        )
        .await;
        return Err(e);
    }
    if misbehavior_disconnects(follow.ban_score, session) {
        rbitcoin_log::warn!(
            "{}",
            misbehavior_disconnect_log(slot.peer, follow.ban_score)
        );
        let _ = apply_queued_decodes(
            slot.queue,
            hub,
            out_tx,
            follow,
            session,
            slot.requested_since,
            slot.peer,
        )
        .await;
        return Err(threshold_disconnect(session));
    }
    Ok(())
}

/// Apply frames already taken off the socket, in order, before a later read
/// error tears the session down. Dropping the follow state would otherwise
/// release the ask for a block that is still sitting in `decode_q`.
async fn apply_queued_decodes(
    decode_q: &mut DecodeQueue,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    requested_since: &mut Option<std::time::Instant>,
    peer_s: &str,
) -> Result<(), NetError> {
    // Blocks already off the socket still apply. Ping, tx, and getheaders
    // do not: they would start headers sync or a download window on a peer
    // that is about to unregister.
    if let Some(s) = session {
        s.request_disconnect();
    }
    let mut first_err: Option<NetError> = None;
    while let Some(rx) = decode_q.pop_front() {
        let decoded = match rx.await {
            Ok(r) => r,
            Err(_) => {
                if first_err.is_none() {
                    first_err = Some(NetError::Protocol("decode task dropped"));
                }
                continue;
            }
        };
        if matches!(&decoded, Ok(msg) if !queued_msg_is_block_body(msg)) {
            continue;
        }
        let n_req = follow.requested_blocks.len();
        if let Err(e) = apply_decoded_peer_msg(decoded, hub, out_tx, follow, session).await {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
        note_block_request_window(follow, n_req, requested_since);
        if misbehavior_disconnects(follow.ban_score, session) && first_err.is_none() {
            rbitcoin_log::warn!("{}", misbehavior_disconnect_log(peer_s, follow.ban_score));
            first_err = Some(threshold_disconnect(session));
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn apply_decoded_peer_msg(
    decoded: Result<RawNetworkMessage, NetError>,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    let msg = match decoded {
        Ok(m) => m,
        Err(NetError::MessageTooLarge(n)) => {
            follow.ban_score = follow.ban_score.saturating_add(OVERSIZE_BAN_SCORE);
            return Err(NetError::MessageTooLarge(n));
        }
        Err(e) => return Err(e),
    };
    handle_decoded_peer_msg(msg, hub, out_tx, follow, session).await
}

pub async fn handle_peer_frame(
    frame: FramedMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    let msg = match decode_framed_offload(frame).await {
        Ok(m) => m,
        Err(NetError::MessageTooLarge(n)) => {
            follow.ban_score = follow.ban_score.saturating_add(OVERSIZE_BAN_SCORE);
            return Err(NetError::MessageTooLarge(n));
        }
        Err(e) => return Err(e),
    };
    handle_decoded_peer_msg(msg, hub, out_tx, follow, session).await
}

pub async fn handle_decoded_peer_msg(
    msg: RawNetworkMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    match msg.into_payload() {
        NetworkMessage::GetData(inv) => serve_getdata(hub, out_tx, follow, session, inv).await?,
        NetworkMessage::GetHeaders(gh) => {
            if session.is_some_and(|s| s.send_over_budget()) {
                return Ok(());
            }
            on_getheaders(hub, out_tx, follow, session, &gh).await?
        }
        NetworkMessage::GetBlocks(gb) => {
            if session.is_some_and(|s| s.send_over_budget()) {
                return Ok(());
            }
            on_getblocks(hub, out_tx, follow, session, &gb).await?
        }
        NetworkMessage::Headers(headers) => {
            // The frame is already off the socket. Store it and clear the
            // locator latch. Getdata and the next getheaders wait in
            // `apply_headers` until the writer drains.
            on_headers(hub, out_tx, follow, session, &headers).await?
        }
        NetworkMessage::Block(block) => on_block(hub, out_tx, follow, session, &block).await?,
        NetworkMessage::CmpctBlock(cb) => on_cmpctblock(hub, out_tx, follow, session, &cb).await?,
        NetworkMessage::BlockTxn(BlockTxn { transactions: bt }) => {
            on_blocktxn(hub, out_tx, follow, session, &bt).await?
        }
        NetworkMessage::Tx(tx) => on_tx(hub, out_tx, follow, session, &tx).await?,
        msg @ (NetworkMessage::GetCFilters(_)
        | NetworkMessage::GetCFHeaders(_)
        | NetworkMessage::GetCFCheckpt(_)) => {
            on_compact_filters(msg, hub, out_tx, follow, session).await?
        }
        NetworkMessage::GetBlockTxn(GetBlockTxn { txs_request }) => {
            serve_getblocktxn(hub, out_tx, follow, session, txs_request).await?
        }
        other => handle_peer_sync_msg(&other, hub, out_tx, follow, session)?,
    }
    Ok(())
}

#[doc(hidden)]
pub fn handle_peer_sync_msg(
    payload: &NetworkMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    if handle_peer_control_msg(payload, hub, out_tx, follow, session)? {
        return Ok(());
    }
    handle_peer_inventory_msg(payload, hub, out_tx, follow, session)
}

#[doc(hidden)]
pub fn handle_peer_control_msg(
    payload: &NetworkMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<bool, NetError> {
    match payload {
        // A second version only logs. The inventory path then ignores it, so
        // the suite cannot tell this arm from the fallback.
        #[cfg_attr(false, mutants::skip)]
        NetworkMessage::Version(_) => on_redundant_version(session),
        NetworkMessage::Verack => on_redundant_verack(session),
        NetworkMessage::Ping(n) => on_ping(hub, out_tx, follow, session, *n)?,
        // Extra bytes after the nonce decode as Unknown. The pong still
        // uses the first eight, the same as a ping with an exact payload.
        NetworkMessage::Unknown { command, payload }
            if command.as_ref() == "ping" && payload.len() >= 8 =>
        {
            let mut nonce = [0u8; 8];
            nonce.copy_from_slice(&payload[..8]);
            on_ping(hub, out_tx, follow, session, u64::from_le_bytes(nonce))?;
        }
        // A pong that waited behind `decode_q` is applied here. The read arm
        // only handles a pong when that queue is empty. Extra bytes, or fewer
        // than eight, decode as Unknown. The ping state still uses the raw
        // payload, the same as that read arm.
        NetworkMessage::Pong(n) => {
            if let Some(s) = session {
                let payload = n.to_le_bytes();
                if let Some(line) = s.on_pong(&payload, s.clock_now()) {
                    rbitcoin_log::info!("{line}");
                }
            }
        }
        NetworkMessage::Unknown { command, payload } if command.as_ref() == "pong" => {
            if let Some(s) = session {
                if let Some(line) = s.on_pong(payload, s.clock_now()) {
                    rbitcoin_log::info!("{line}");
                }
            }
        }
        NetworkMessage::FeeFilter(amt) => on_feefilter(session, *amt),
        NetworkMessage::SendHeaders => on_sendheaders(follow),
        NetworkMessage::SendCmpct(sc) => on_sendcmpct(follow, session, sc),
        // rust-bitcoin rejects announce bytes other than 0 or 1 at decode,
        // so a bad sendcmpct arrives as Unknown. BIP152 disconnects that.
        NetworkMessage::Unknown { command, .. } if command.as_ref() == "sendcmpct" => {
            rbitcoin_log::info!("invalid sendcmpct announce field");
            punish_disconnect(&mut follow.ban_score, session);
        }
        NetworkMessage::WtxidRelay => on_wtxid_relay(session),
        NetworkMessage::SendAddrV2 => on_sendaddrv2(follow, session),
        _ => return Ok(false),
    }
    Ok(true)
}

#[doc(hidden)]
pub fn filter_q<T>(r: Result<T, rbitcoin_store::StoreError>) -> Result<T, NetError> {
    r.map_err(|e| NetError::Consensus(e.to_string()))
}

#[doc(hidden)]
pub async fn on_compact_filters(
    payload: NetworkMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    // Queued behind any parked range, so a request that arrives over the
    // budget is answered once the writer drains, not dropped. Up to 1,000
    // filters or 2,000 filter headers are read on the blocking pool under a
    // header permit, not on the session's worker.
    follow.cfilter_requests.push_back(payload);
    resume_cfilters(hub, out_tx, follow, session).await
}

/// Send the parked compact-filter range, then read and send each queued
/// request in order, until a reply crosses the send budget. A request's
/// replies are read only once the range before it is sent, so one range at
/// most is held.
#[doc(hidden)]
pub async fn resume_cfilters(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    loop {
        serve_cfilter_tail(out_tx, follow, session)?;
        if !follow.cfilter_tail.is_empty() {
            return Ok(());
        }
        let Some(payload) = follow.cfilter_requests.pop_front() else {
            return Ok(());
        };
        let replies =
            run_header_job(hub, move |hub| compact_filter_replies(hub, &payload)).await??;
        follow.cfilter_tail.extend(replies);
    }
}

/// Queue compact-filter replies in order until a reply pushes the writer
/// past the send budget (`fPauseSend`). The rest stays in
/// `follow.cfilter_tail` for the session loop to resume once the writer
/// drains, so a range that crosses the budget is sent whole (#2078).
#[doc(hidden)]
pub fn serve_cfilter_tail(
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    while session.is_none_or(|s| !s.send_over_budget()) {
        let Some(reply) = follow.cfilter_tail.pop_front() else {
            break;
        };
        out_tx.send(reply)?;
    }
    flush_parked_pong(out_tx, follow, session)
}

/// The replies to a compact-filter request, read from the store. Runs off
/// the reactor. An `Err` disconnects the peer.
#[doc(hidden)]
pub fn compact_filter_replies(
    hub: &ChainHub,
    payload: &NetworkMessage,
) -> Result<Vec<NetworkMessage>, NetError> {
    crate::reactor::assert_not_reactor("compact filter read");
    match payload {
        NetworkMessage::GetCFilters(m) => getcfilters_replies(hub, m),
        NetworkMessage::GetCFHeaders(m) => Ok(getcfheaders_reply(hub, m)?.into_iter().collect()),
        NetworkMessage::GetCFCheckpt(m) => Ok(getcfcheckpt_reply(hub, m)?.into_iter().collect()),
        _ => Ok(Vec::new()),
    }
}

/// Core `MAX_GETCFILTERS_SIZE` / `MAX_GETCFHEADERS_SIZE`.
#[doc(hidden)]
pub const MAX_GETCFILTERS: u32 = 1000;
#[doc(hidden)]
pub const MAX_GETCFHEADERS: u32 = 2000;

/// Core `PrepareBlockFilterRequest` range rule: start past stop, or `max`
/// or more heights, disconnects the peer.
#[doc(hidden)]
pub fn compact_filter_range(start: u32, stop: u32, max: u32) -> Result<(), NetError> {
    if start > stop {
        return Err(NetError::Protocol("compact filter request start past stop"));
    }
    if stop - start >= max {
        return Err(NetError::Protocol("compact filter request range too large"));
    }
    Ok(())
}

/// Stop height of a basic-filter request. `None` (silence: not a short
/// batch, not an empty filter) when the index is off, the stop hash is not
/// on the best chain, or the stop is past the filter watermark.
#[doc(hidden)]
pub fn compact_filter_stop(
    hub: &ChainHub,
    filter_type: u8,
    stop_hash: &[u8; 32],
) -> Result<Option<u32>, NetError> {
    if filter_type != 0 || !hub.query.block_filter_enabled() {
        return Ok(None);
    }
    Ok(filter_q(hub.query.height_of_hash(stop_hash))?.map(|h| h.0))
}

#[doc(hidden)]
pub fn within_filter_watermark(hub: &ChainHub, stop: u32) -> Result<bool, NetError> {
    Ok(filter_q(hub.query.basic_filter_hwm())?.is_some_and(|hwm| stop <= hwm))
}

fn getcfilters_replies(
    hub: &ChainHub,
    m: &bitcoin::p2p::message_filter::GetCFilters,
) -> Result<Vec<NetworkMessage>, NetError> {
    use bitcoin::hashes::Hash;
    let Some(stop) = compact_filter_stop(hub, m.filter_type, m.stop_hash.as_byte_array())? else {
        return Ok(Vec::new());
    };
    compact_filter_range(m.start_height, stop, MAX_GETCFILTERS)?;
    if !within_filter_watermark(hub, stop)? {
        return Ok(Vec::new());
    }
    let Some(filters) = filter_q(hub.query.basic_filters(m.start_height, stop))? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(filters.len());
    for (h, body) in (m.start_height..).zip(filters) {
        let Some((_, rec)) = filter_q(hub.query.header_at_height(rbitcoin_primitives::Height(h)))?
        else {
            break;
        };
        out.push(NetworkMessage::CFilter(
            bitcoin::p2p::message_filter::CFilter {
                filter_type: 0,
                block_hash: bitcoin::BlockHash::from_byte_array(rec.hash),
                filter: body,
            },
        ));
    }
    Ok(out)
}

fn getcfheaders_reply(
    hub: &ChainHub,
    m: &bitcoin::p2p::message_filter::GetCFHeaders,
) -> Result<Option<NetworkMessage>, NetError> {
    use bitcoin::bip158::FilterHeader;
    use bitcoin::hashes::Hash;
    let Some(stop) = compact_filter_stop(hub, m.filter_type, m.stop_hash.as_byte_array())? else {
        return Ok(None);
    };
    compact_filter_range(m.start_height, stop, MAX_GETCFHEADERS)?;
    if !within_filter_watermark(hub, stop)? {
        return Ok(None);
    }
    // One idx read covers the previous header and every hash in range.
    let first = m.start_height.saturating_sub(1);
    let Some(rows) = filter_q(hub.query.basic_filter_hashes_and_headers(first, stop))? else {
        return Ok(None);
    };
    let (previous, hashes) = if m.start_height == 0 {
        (FilterHeader::from_byte_array([0u8; 32]), &rows[..])
    } else {
        (rows[0].1, &rows[1..])
    };
    Ok(Some(NetworkMessage::CFHeaders(
        bitcoin::p2p::message_filter::CFHeaders {
            filter_type: 0,
            stop_hash: m.stop_hash,
            previous_filter_header: previous,
            filter_hashes: hashes.iter().map(|(hash, _)| *hash).collect(),
        },
    )))
}

fn getcfcheckpt_reply(
    hub: &ChainHub,
    m: &bitcoin::p2p::message_filter::GetCFCheckpt,
) -> Result<Option<NetworkMessage>, NetError> {
    use bitcoin::hashes::Hash;
    let Some(stop) = compact_filter_stop(hub, m.filter_type, m.stop_hash.as_byte_array())? else {
        return Ok(None);
    };
    if !within_filter_watermark(hub, stop)? {
        return Ok(None);
    }
    let mut filter_headers = Vec::new();
    if stop >= 1000 {
        let Some(headers) = filter_q(hub.query.basic_filter_checkpoint_headers(stop))? else {
            return Ok(None);
        };
        filter_headers = headers;
    }
    Ok(Some(NetworkMessage::CFCheckpt(
        bitcoin::p2p::message_filter::CFCheckpt {
            filter_type: 0,
            stop_hash: m.stop_hash,
            filter_headers,
        },
    )))
}

#[doc(hidden)]
pub fn handle_peer_inventory_msg(
    payload: &NetworkMessage,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    // The reply that crossed the budget is already queued. Do not serve
    // another request until the writer drains.
    if session.is_some_and(|s| s.send_over_budget()) {
        return Ok(());
    }
    match payload {
        NetworkMessage::Addr(list) => on_addr_list(follow, session, list)?,
        NetworkMessage::AddrV2(list) => on_addrv2(follow, session, list)?,
        NetworkMessage::Inv(items) => on_inv(hub, out_tx, follow, session, items)?,
        NetworkMessage::NotFound(items) => on_notfound(hub, session, items),
        NetworkMessage::MemPool
        | NetworkMessage::FilterLoad(_)
        | NetworkMessage::FilterAdd(_)
        | NetworkMessage::FilterClear => on_bloom_forbidden(follow, session)?,
        NetworkMessage::GetAddr => on_getaddr(hub, out_tx, session)?,
        NetworkMessage::Headers(_)
        | NetworkMessage::GetHeaders(_)
        | NetworkMessage::GetBlocks(_) => {
            return Err(NetError::Protocol(
                "header work is not on the sync inventory path",
            ));
        }
        NetworkMessage::GetCFilters(_)
        | NetworkMessage::GetCFHeaders(_)
        | NetworkMessage::GetCFCheckpt(_) => {
            return Err(NetError::Protocol(
                "compact filter reads are not on the sync inventory path",
            ));
        }
        _ => {}
    }
    Ok(())
}

#[doc(hidden)]
/// Core `LogDebug(BCLog::NET, ...)`: a peer can repeat this at the message
/// rate, so it is not an INFO line.
pub fn on_redundant_version(session: Option<&crate::peers::LivePeer>) {
    if let Some(s) = session {
        rbitcoin_log::debug!(P2p, "redundant version message from peer={}", s.id);
    } else {
        rbitcoin_log::debug!(P2p, "redundant version message from peer");
    }
}

#[doc(hidden)]
/// Core `LogDebug(BCLog::NET, ...)`, as [`on_redundant_version`].
pub fn on_redundant_verack(session: Option<&crate::peers::LivePeer>) {
    if let Some(s) = session {
        rbitcoin_log::debug!(P2p, "ignoring redundant verack message from peer={}", s.id);
    } else {
        rbitcoin_log::debug!(P2p, "ignoring redundant verack message");
    }
}

#[doc(hidden)]
pub fn on_ping(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    n: u64,
) -> Result<(), NetError> {
    if let Some(s) = session {
        queue_due_tx_invs(hub, s, &follow.from_this_peer, out_tx);
        queue_due_parent_getdata(hub, s, out_tx);
        let _ = maybe_queue_local_addr(hub, s, out_tx);
        // Noban headers-timeout reset: Core re-issues getheaders in the
        // same SendMessages turn; hook the ping so the official test
        // sees it before sync_with_ping returns.
        let _ = maybe_queue_initial_getheaders(out_tx, hub, s);
    }
    if serve_tail_pending(follow) {
        follow.parked_pong = Some(n);
        return Ok(());
    }
    follow.parked_pong = None;
    queue_pong(session, out_tx, n)?;
    Ok(())
}

fn serve_tail_pending(follow: &PeerFollowState) -> bool {
    !follow.getdata_tail.is_empty()
        || !follow.getblocktxn_tail.is_empty()
        || !follow.cfilter_tail.is_empty()
        || !follow.cfilter_requests.is_empty()
}

/// Queue a pong parked by [`on_ping`] once every serve tail and queued
/// compact-filter request is empty.
fn flush_parked_pong(
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    if serve_tail_pending(follow) {
        return Ok(());
    }
    let Some(n) = follow.parked_pong.take() else {
        return Ok(());
    };
    queue_pong(session, out_tx, n)
}

#[doc(hidden)]
pub fn on_feefilter(session: Option<&crate::peers::LivePeer>, amt: i64) {
    if let Some(s) = session {
        s.note_minfeefilter_sat_kvb(amt.max(0) as u64);
    }
}

#[doc(hidden)]
pub fn on_sendheaders(follow: &mut PeerFollowState) {
    follow.wants_headers = true;
}

#[doc(hidden)]
pub fn on_sendcmpct(
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    sc: &SendCmpct,
) {
    // Segwit networks: only version 2 (wtxid short-ids) enables HB.
    // Version 1 and version > 2 are ignored.
    if sc.version == 2 {
        follow.send_cmpct = sc.send_compact;
        follow.cmpct_version = 2;
        if let Some(sess) = session {
            sess.set_hb_from(sc.send_compact);
        }
    }
}

#[doc(hidden)]
pub fn on_wtxid_relay(session: Option<&crate::peers::LivePeer>) {
    // BIP339 negotiates before verack. Core disconnects a peer that switches
    // after, with no misbehavior score: the tx INVs already sent name the
    // tx by the hash of the mode they were sent in.
    if let Some(s) = session {
        rbitcoin_log::debug!(
            P2p,
            "wtxidrelay received after verack, disconnecting peer={}",
            s.id
        );
        s.request_disconnect();
    }
}

#[doc(hidden)]
pub fn on_sendaddrv2(follow: &mut PeerFollowState, session: Option<&crate::peers::LivePeer>) {
    let id = session.map(|s| s.id).unwrap_or(0);
    rbitcoin_log::debug!(P2p, "{}", sendaddrv2_after_verack_log(id));
    punish_disconnect(&mut follow.ban_score, session);
}

/// v1 `addr` is learned only as an addr-fetch session's `getaddr` reply.
#[doc(hidden)]
pub fn on_addr_list(
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    list: &[(u32, bitcoin::p2p::address::Address)],
) -> Result<(), NetError> {
    let Some(s) = session.filter(|s| s.conn_type == crate::peers::PeerConnType::AddrFetch) else {
        return Ok(());
    };
    if list.len() > MAX_ADDR_TO_SEND {
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    let v2: Vec<_> = list
        .iter()
        .filter_map(|(time, a)| {
            let sock = a.socket_addr().ok()?;
            Some(bitcoin::p2p::address::AddrV2Message {
                time: *time,
                services: a.services,
                addr: crate::NetAddr::from_socket(sock).to_addrv2(),
                port: sock.port(),
            })
        })
        .collect();
    learn_addrfetch_reply(s, &v2, list.len());
    Ok(())
}

/// Core `ProcessMessage` ADDR/ADDRV2 on an addr-fetch session: the reply to
/// the `getaddr` it sent is learned and not relayed (`m_getaddr_sent`).
/// More than one address ends the session, without misbehavior.
fn learn_addrfetch_reply(
    s: &crate::peers::LivePeer,
    list: &[bitcoin::p2p::address::AddrV2Message],
    n: usize,
) {
    if let Some(ph) = s.peer_hub() {
        // Node time, as the bucket's start (`connected_at`) is.
        let now = s.clock_now();
        let allow = s.take_addr_tokens(list.len(), now.saturating_mul(1000));
        ph.learn_addrv2(s.net, &list[..allow], now);
        if n > 1 {
            ph.note_addrfetch_reply();
        }
    }
    if n > 1 {
        s.request_disconnect();
    }
}

#[doc(hidden)]
pub fn on_addrv2(
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    list: &[bitcoin::p2p::address::AddrV2Message],
) -> Result<(), NetError> {
    let n = list.len();
    // The count first: an oversized list is not serialized to be logged.
    if n > MAX_ADDR_TO_SEND {
        rbitcoin_log::debug!(P2p, "{}", addrv2_message_size_log(n));
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    let nbytes =
        bitcoin::consensus::encode::serialize(&NetworkMessage::AddrV2(list.to_vec())).len();
    let id = session.map(|s| s.id).unwrap_or(0);
    // Core `LogDebug(BCLog::NET, "received: %s (%u bytes)")`.
    rbitcoin_log::debug!(P2p, "{}", received_addrv2_log(nbytes, id));
    if let Some(s) = session.filter(|s| s.conn_type == crate::peers::PeerConnType::AddrFetch) {
        learn_addrfetch_reply(s, list, n);
        return Ok(());
    }
    if let Some(s) = session {
        if let Some(ph) = s.peer_hub() {
            // Node time, as the bucket's start (`connected_at`) is.
            let now = s.clock_now();
            let allow = s.take_addr_tokens(list.len(), now.saturating_mul(1000));
            // One sanitized copy for the book and the relay, as Core
            // `ProcessAddrs` rewrites an insane time before `RelayAddress`.
            let rows: Vec<bitcoin::p2p::address::AddrV2Message> = list[..allow]
                .iter()
                .map(|a| {
                    let mut a = a.clone();
                    a.time = crate::seeds::learned_addr_time(a.time, now);
                    a
                })
                .collect();
            ph.learn_addrv2(s.net, &rows, now);
            let mut neighbors: Vec<_> = ph
                .live_peers()
                .into_iter()
                .filter(|other| {
                    other.id != s.id && other.wants_addrv2() && other.writer().is_some()
                })
                .collect();
            neighbors.sort_by_key(|other| other.id);
            let own = ph.own_addrs();
            let mut batches: Vec<Vec<bitcoin::p2p::address::AddrV2Message>> =
                vec![Vec::new(); neighbors.len()];
            for addr in &rows {
                if crate::netaddr::addrv2_skip_relay(addr) {
                    continue;
                }
                if crate::NetAddr::from_addrv2(addr).is_some_and(|a| own.contains(a)) {
                    continue;
                }
                let key = addr_relay_key(addr);
                let n_dest = if neighbors.len() <= 1 {
                    neighbors.len()
                } else if key & 1 == 0 {
                    1
                } else {
                    2
                };
                if n_dest == 0 {
                    continue;
                }
                let start = (key as usize) % neighbors.len();
                for step in 0..n_dest {
                    let idx = match step {
                        0 => start,
                        _ => {
                            if start + 1 == neighbors.len() {
                                0
                            } else {
                                start + 1
                            }
                        }
                    };
                    batches[idx].push(addr.clone());
                }
            }
            for (other, batch) in neighbors.iter().zip(batches) {
                if batch.is_empty() {
                    continue;
                }
                if let Some(tx) = other.writer() {
                    let msg = NetworkMessage::AddrV2(batch);
                    let sent = bitcoin::consensus::encode::serialize(&msg).len();
                    rbitcoin_log::debug!(P2p, "{}", sending_addrv2_log(sent, other.id));
                    tx.send(msg)?;
                }
            }
        }
    }
    Ok(())
}

#[doc(hidden)]
async fn header_pool_permit() -> Result<tokio::sync::OwnedSemaphorePermit, NetError> {
    crate::msg_decode::ctrl_decode_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| NetError::Protocol("decode semaphore closed"))
}

/// Store work for headers. A live node holds `hub` in an `Arc` and runs `job`
/// on the blocking pool. A stack hub (unit tests) runs `job` on the caller.
async fn run_header_job<T, F>(hub: &ChainHub, job: F) -> Result<T, NetError>
where
    T: Send + 'static,
    F: FnOnce(&ChainHub) -> T + Send + 'static,
{
    let Some(owned) = hub.shared_arc() else {
        return Ok(job(hub));
    };
    let permit = header_pool_permit().await?;
    tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        job(owned.as_ref())
    })
    .await
    .map_err(|_| NetError::Protocol("header job join failed"))
}

pub async fn on_getheaders(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    gh: &GetHeadersMessage,
) -> Result<(), NetError> {
    if gh.locator_hashes.len() > MAX_LOCATOR_SZ {
        punish_disconnect(&mut follow.ban_score, session);
        return Ok(());
    }
    let gh = gh.clone();
    let (headers, withhold_stale, met_at) =
        run_header_job(hub, move |hub| getheaders_apply(hub, &gh)).await??;
    if !withhold_stale {
        if let Some(s) = session {
            if let Some(last) = headers.last() {
                let hash = last.block_hash();
                s.note_best_header_sent(hash);
                // The reply is our tip, or a block on the way to it (one more
                // block may have connected while the header job ran). A stop
                // on another branch does not. A newer offer stays only while
                // that block is still on the way to the tip.
                let tip = hub.tip_hash();
                let newer_offer = s.last_tip_offered().is_some_and(|prev| {
                    prev != hash
                        && hub.is_header_ancestor(hash, prev)
                        && tip.is_some_and(|tip| hub.is_header_ancestor(prev, tip))
                });
                if !newer_offer && tip.is_some_and(|tip| hub.is_header_ancestor(hash, tip)) {
                    s.note_last_tip_offered(hash);
                }
            } else if let Some(hash) = met_at {
                s.note_best_header_sent(hash);
            }
        }
        out_tx.send(NetworkMessage::Headers(headers))?;
    }
    Ok(())
}

fn getheaders_apply(
    hub: &ChainHub,
    gh: &GetHeadersMessage,
) -> Result<(Vec<bitcoin::block::Header>, bool, Option<BlockHash>), NetError> {
    let headers = headers_reply_for_getheaders(hub, gh)?;
    let withhold_stale = headers.is_empty()
        && gh.locator_hashes.is_empty()
        && gh.stop_hash.to_byte_array() != [0u8; 32]
        && hub.header_of(&gh.stop_hash).is_some()
        && !hub.stale_relay_allowed(&gh.stop_hash);
    // An empty reply marks the block the locator met our chain at. The peer
    // named it, so it has it. Our tip can be a block that connected after
    // the reply was built, and marking it would skip that block's announce.
    // Below minimum chain work every reply is empty and, as in Core, marks
    // nothing.
    let met_at = if headers.is_empty() && hub.meets_minimum_chain_work() {
        gh.locator_hashes
            .iter()
            .copied()
            .find(|h| hub.is_connected(h))
    } else {
        None
    };
    Ok((headers, withhold_stale, met_at))
}

/// Core's `getblocks` page (`nLimit`).
const GETBLOCKS_PAGE: usize = 500;

#[doc(hidden)]
pub async fn on_getblocks(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    gb: &GetBlocksMessage,
) -> Result<(), NetError> {
    if gb.locator_hashes.len() > MAX_LOCATOR_SZ {
        punish_disconnect(&mut follow.ban_score, session);
        return Ok(());
    }
    let gh = GetHeadersMessage {
        version: gb.version,
        locator_hashes: gb.locator_hashes.clone(),
        stop_hash: gb.stop_hash,
    };
    let headers = run_header_job(hub, move |hub| {
        headers_for_peer(hub.cache.as_ref(), hub.query.as_ref(), &gh)
    })
    .await??;
    // The header walk is `getheaders`', which includes `hashStop`. Core's
    // `getblocks` loop breaks before it.
    let inv: Vec<Inventory> = headers
        .into_iter()
        .map(|h| h.block_hash())
        .take_while(|h| *h != gb.stop_hash)
        .take(GETBLOCKS_PAGE)
        .map(Inventory::Block)
        .collect();
    if inv.len() == GETBLOCKS_PAGE {
        if let Some(Inventory::Block(last)) = inv.last() {
            follow.continuation_block = Some(*last);
        }
    }
    if !inv.is_empty() {
        out_tx.send(NetworkMessage::Inv(inv))?;
    }
    Ok(())
}

pub async fn serve_getdata(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    inv: Vec<Inventory>,
) -> Result<(), NetError> {
    if inv.len() > MAX_INV_SIZE {
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    follow.getdata_tail.extend(inv);
    serve_getdata_tail(hub, out_tx, follow, session).await
}

/// Core `ProcessGetData`: serve queued getdata items in order until the
/// writer has no serve room (`fPauseSend`). The rest stays in
/// `follow.getdata_tail` for the session loop to resume. The `notfound`
/// collected so far is sent either way.
pub async fn serve_getdata_tail(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    let mut notfound: Vec<Inventory> = Vec::new();
    while session.is_none_or(|s| s.has_serve_room()) {
        let Some(item) = follow.getdata_tail.pop_front() else {
            break;
        };
        match &item {
            // A block-relay-only session does not serve transactions and
            // does not answer notfound for them (`p2p_blocksonly`).
            Inventory::Transaction(_) | Inventory::WitnessTransaction(_) | Inventory::WTx(_)
                if session
                    .is_some_and(|s| s.conn_type == crate::peers::PeerConnType::BlockRelay) => {}
            // MSG_FILTERED_BLOCK. NODE_BLOOM is off, so this is a disconnect.
            Inventory::Unknown { inv_type, .. } if *inv_type & 0x3fff_ffff == 3 => {
                rbitcoin_log::info!(
                    "filtered block request received when NODE_BLOOM service disabled"
                );
                punish_disconnect(&mut follow.ban_score, session);
                return flush_parked_pong(out_tx, follow, session);
            }
            // Unknown hash: Core ProcessGetData answers notfound. Silence
            // holds the requester's getdata until the stall floor.
            // The header check and the body reconstruct both run on the
            // blocking pool. A missing header or body is `NotFound`.
            Inventory::Block(h) | Inventory::WitnessBlock(h) => {
                let witness = matches!(item, Inventory::WitnessBlock(_));
                match serve_getdata_full_block(hub, out_tx, h, witness).await? {
                    FullBlockReply::NotFound => notfound.push(item),
                    // Core returns before the continuation when it sends nothing.
                    FullBlockReply::Withheld => {}
                    FullBlockReply::Queued if follow.continuation_block == Some(*h) => {
                        // Core sends this right after the block, even when redundant.
                        follow.continuation_block = None;
                        if let Some(tip) = hub.tip_hash() {
                            out_tx.send(NetworkMessage::Inv(vec![Inventory::Block(tip)]))?;
                        }
                    }
                    FullBlockReply::Queued => {}
                }
            }
            Inventory::CompactBlock(h) => {
                serve_getdata_compact(hub, out_tx, follow, h).await?;
            }
            Inventory::Transaction(txid) | Inventory::WitnessTransaction(txid) => {
                let tx = hub.mempool().and_then(|mp| mp.try_get_tx(txid));
                let served = serve_mempool_getdata(hub, out_tx, session, &item, tx)?
                    || serve_announced_tip_tx(hub, out_tx, session, &item).await?;
                if !served && hub.mempool().is_some() {
                    notfound.push(item);
                }
            }
            Inventory::WTx(wtxid) => {
                serve_getdata_wtx(hub, out_tx, session, item, wtxid, &mut notfound).await?;
            }
            _ => {}
        }
    }
    if !notfound.is_empty() {
        out_tx.send(NetworkMessage::NotFound(notfound))?;
    }
    flush_parked_pong(out_tx, follow, session)
}

pub async fn serve_getdata_full_block(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    h: &bitcoin::BlockHash,
    witness: bool,
) -> Result<FullBlockReply, NetError> {
    let query = Arc::clone(&hub.query);
    let cache = Arc::clone(&hub.cache);
    let hash = *h;
    let served = tokio::task::spawn_blocking(move || -> Result<FullBlockServe, NetError> {
        let _g = crate::reactor::BlockingRegion::enter();
        // A known stale body is not read. A header with no body is notfound.
        if header_outside_relay_window(query.as_ref(), &hash) {
            let have = cache.has_body(&hash)
                || cache.has_servable(&hash)
                || query
                    .is_block_archived(&hash.to_byte_array())
                    .unwrap_or(false);
            return Ok(if have {
                FullBlockServe::Withheld
            } else {
                FullBlockServe::Missing
            });
        }
        match encode_served_block(cache.as_ref(), query.as_ref(), &hash, witness)? {
            Some(bytes) => Ok(FullBlockServe::Bytes(bytes)),
            None => Ok(FullBlockServe::Missing),
        }
    })
    .await
    .map_err(|_| NetError::Protocol("serve reconstruct join failed"))??;
    match served {
        FullBlockServe::Missing => Ok(FullBlockReply::NotFound),
        // A body we have but will not relay (month-old side block) stays silent.
        FullBlockServe::Withheld => Ok(FullBlockReply::Withheld),
        FullBlockServe::Bytes(bytes) => {
            if !hub.stale_relay_allowed(h) {
                return Ok(FullBlockReply::Withheld);
            }
            out_tx.send(PeerOut::Encoded(bytes))?;
            Ok(FullBlockReply::Queued)
        }
    }
}

/// What a full-block `getdata` did.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullBlockReply {
    /// Unknown, header-only, or pruned: the caller answers `notfound`.
    NotFound,
    /// A body we have but will not relay (month-old side block): silent.
    Withheld,
    /// The block bytes are queued.
    Queued,
}

enum FullBlockServe {
    Missing,
    Withheld,
    Bytes(Vec<u8>),
}

#[doc(hidden)]
pub enum CompactServe {
    Missing,
    Full(Block),
    Cmpct(HeaderAndShortIds),
}

/// Reconstruct and, when the block is near the tip, build the compact
/// announcement. Store reads and short ids stay off the session task.
#[doc(hidden)]
pub fn compact_serve_outcome(
    cache: &BlockCache,
    query: &Query,
    hash: &BlockHash,
    tip_h: u32,
    ver: u32,
    pref: Vec<usize>,
) -> Result<CompactServe, NetError> {
    if header_outside_relay_window(query, hash) {
        return Ok(CompactServe::Missing);
    }
    let Some(block) = block_for_peer(cache, query, hash)? else {
        return Ok(CompactServe::Missing);
    };
    if !within_relay_window(query, &block) {
        return Ok(CompactServe::Missing);
    }
    let block_h = served_block_height(query, &block);
    if block_h.is_some_and(|h| tip_h.saturating_sub(h) > MAX_CMPCTBLOCK_DEPTH) {
        return Ok(CompactServe::Full(block));
    }
    let ids = crate::compact::short_id_hashes(&block, ver);
    match crate::compact::header_and_short_ids(&block, &ids, rand_nonce(), ver, &pref) {
        Some(hsi) => {
            rbitcoin_log::info!(
                "{}",
                crate::compact::cmpct_send_line(block.block_hash(), block.txdata.len(), &hsi)
            );
            Ok(CompactServe::Cmpct(hsi))
        }
        None => Ok(CompactServe::Full(block)),
    }
}

async fn block_reconstruct_permit() -> Result<tokio::sync::OwnedSemaphorePermit, NetError> {
    crate::msg_decode::block_decode_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| NetError::Protocol("decode semaphore closed"))
}

#[doc(hidden)]
pub async fn serve_getdata_compact(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    h: &bitcoin::BlockHash,
) -> Result<(), NetError> {
    let permit = block_reconstruct_permit().await?;
    let cache = Arc::clone(&hub.cache);
    let query = Arc::clone(&hub.query);
    let hash = *h;
    let tip_h = hub.tip_height().unwrap_or(0);
    let ver = follow.cmpct_version.clamp(1, 2);
    let pref = hub.cmpct_prefill_indexes(h).unwrap_or_else(|| vec![0]);
    let outcome = tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        compact_serve_outcome(cache.as_ref(), query.as_ref(), &hash, tip_h, ver, pref)
    })
    .await
    .map_err(|_| NetError::Protocol("compact serve join failed"))??;
    match outcome {
        CompactServe::Missing => Ok(()),
        CompactServe::Full(block) => out_tx.send(PeerOut::Served(NetworkMessage::Block(block))),
        CompactServe::Cmpct(hsi) => {
            out_tx.send(PeerOut::Served(NetworkMessage::CmpctBlock(CmpctBlock {
                compact_block: hsi,
            })))
        }
    }
}

#[doc(hidden)]
pub async fn serve_getdata_wtx(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    item: Inventory,
    wtxid: &bitcoin::Wtxid,
    notfound: &mut Vec<Inventory>,
) -> Result<(), NetError> {
    if let Some(s) = session {
        rbitcoin_log::trace!(Txrelay, "{}", received_getdata_wtx_log(wtxid, s.id));
    }
    let tx = hub.mempool().and_then(|mp| mp.try_get_tx_by_wtxid(wtxid));
    if serve_mempool_getdata(hub, out_tx, session, &item, tx)?
        || serve_announced_tip_tx(hub, out_tx, session, &item).await?
    {
        return Ok(());
    }
    notfound.push(item);
    Ok(())
}

/// Core `FindTxForGetData`: a tx announced to this peer that left the pool
/// for the tip block is served from that block, by txid or wtxid as it was
/// announced. `true` = queued.
async fn serve_announced_tip_tx(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    item: &Inventory,
) -> Result<bool, NetError> {
    if !session.is_some_and(|s| s.has_announced(item)) {
        return Ok(false);
    }
    let Some(hash) = hub.tip_hash() else {
        return Ok(false);
    };
    let Some(tx) = tx_from_tip_index(hub, hash, item).await? else {
        return Ok(false);
    };
    out_tx.send(NetworkMessage::Tx(getdata_tx(item, tx)))?;
    Ok(true)
}

/// Core: only `MSG_WITNESS_TX` and `MSG_WTX` carry the witness.
fn getdata_tx(item: &Inventory, mut tx: Transaction) -> Transaction {
    if matches!(item, Inventory::Transaction(_)) {
        tx.input.iter_mut().for_each(|i| i.witness.clear());
    }
    tx
}

/// One tip index for every announced tx in this message. A warm index
/// does not take a reconstruct permit and does not read the body again.
async fn tx_from_tip_index(
    hub: &ChainHub,
    hash: BlockHash,
    item: &Inventory,
) -> Result<Option<Transaction>, NetError> {
    if let Some(found) = hub.cached_tip_wtx(&hash, item) {
        return Ok(found);
    }
    let permit = block_reconstruct_permit().await?;
    if let Some(found) = hub.cached_tip_wtx(&hash, item) {
        return Ok(found);
    }
    let cache = Arc::clone(&hub.cache);
    let query = Arc::clone(&hub.query);
    let built = tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        block_for_peer(cache.as_ref(), query.as_ref(), &hash)
            .map(|block| crate::chain::TipWtxIndex::from_block(hash, block))
    })
    .await
    .map_err(|_| NetError::Protocol("tip wtxid serve join failed"))?;
    let index = match built {
        Ok(index) => index,
        Err(_) => return Ok(None),
    };
    #[cfg(debug_assertions)]
    hub.note_tip_wtx_scan();
    let tx = index.tx(item);
    hub.install_tip_wtx(Arc::new(index));
    Ok(tx)
}

/// Core: past this depth a `getblocktxn` is answered with the full block.
#[doc(hidden)]
pub const MAX_GETBLOCKTXN_DEPTH: u32 = 10;

/// Best-chain height, or the same count walked back to a confirmed
/// ancestor. A tip child is `tip + 1`, so it stays inside the compact
/// window. A loser still ages out after its parent leaves the best chain.
///
/// The walk is [`crate::chain::ancestor_walk`]: at most
/// [`MAX_GETBLOCKTXN_DEPTH`] headers, and `None` when that cap is hit.
/// `None` is an unknown height, not genesis. Height 0 would already be
/// the full block once the tip is past the window. A height-index error
/// is the same unknown height: the body stays the transactions.
fn served_block_height(query: &Query, block: &Block) -> Option<u32> {
    match query.height_of_hash(&block.block_hash().to_byte_array()) {
        Ok(Some(h)) => return Some(h.0),
        Err(_) => return None,
        Ok(None) => {}
    }
    let hash = block.block_hash();
    crate::chain::ancestor_walk(
        &hash,
        MAX_GETBLOCKTXN_DEPTH,
        |cur| {
            if cur == &hash {
                return Some(block.header);
            }
            let Ok(Some((_, rec))) = query.get_header_by_hash(&cur.to_byte_array()) else {
                return None;
            };
            query.wire_header_from_record(&rec).ok()
        },
        |prev| match query.height_of_hash(&prev.to_byte_array()) {
            Ok(Some(h)) => Some(h.0),
            Ok(None) | Err(_) => None,
        },
    )
}

/// Active-chain blocks are always served. A side body is silent once the
/// best header is a month ahead of it, same as full-block `getdata`.
/// A failed height read is not proof the block is off the active chain.
fn within_relay_window(query: &Query, block: &Block) -> bool {
    match query.height_of_hash(&block.block_hash().to_byte_array()) {
        Ok(Some(_)) | Err(_) => return true,
        Ok(None) => {}
    }
    header_time_in_relay_window(query, block.header.time)
}

/// The header is stored and the body must not be relayed. An unknown header
/// is `false`: the caller still looks up the body, which is how a missing
/// block becomes notfound.
fn header_outside_relay_window(query: &Query, hash: &BlockHash) -> bool {
    let Ok(Some((_, rec))) = query.get_header_by_hash(&hash.to_byte_array()) else {
        return false;
    };
    match query.height_of_hash(&hash.to_byte_array()) {
        Ok(Some(_)) | Err(_) => false,
        Ok(None) => {
            let Ok(wire) = query.wire_header_from_record(&rec) else {
                return true;
            };
            !header_time_in_relay_window(query, wire.time)
        }
    }
}

fn header_time_in_relay_window(query: &Query, block_time: u32) -> bool {
    let Some(tip_h) = query.tip_height() else {
        return false;
    };
    let Ok(best) = query.wire_header_at_height(tip_h) else {
        return false;
    };
    u64::from(best.time).saturating_sub(u64::from(block_time))
        < crate::chain::STALE_RELAY_AGE_LIMIT_SECS
}

#[doc(hidden)]
pub enum GetBlockTxnServe {
    BadIndex,
    Block(Block),
    Txns(BlockTransactions),
}

/// Park one `getblocktxn` and serve it while the writer has room. A full
/// queue keeps the request on [`PeerFollowState::getblocktxn_tail`]. An
/// empty request disconnects before the block is read, as in Core.
pub async fn serve_getblocktxn(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    txs_request: BlockTransactionsRequest,
) -> Result<(), NetError> {
    if txs_request.indexes.is_empty() {
        let id = session.map(|s| s.id).unwrap_or(0);
        rbitcoin_log::info!(
            "p2p: getblocktxn received with no transaction indexes, disconnecting peer={id}"
        );
        punish_disconnect(&mut follow.ban_score, session);
        return Ok(());
    }
    follow.getblocktxn_tail.push_back(txs_request);
    serve_getblocktxn_tail(hub, out_tx, follow, session).await
}

pub async fn serve_getblocktxn_tail(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    while session.is_none_or(|s| s.has_serve_room()) {
        let Some(req) = follow.getblocktxn_tail.pop_front() else {
            break;
        };
        serve_one_getblocktxn(hub, out_tx, follow, session, req).await?;
    }
    flush_parked_pong(out_tx, follow, session)
}

pub async fn serve_one_getblocktxn(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    txs_request: BlockTransactionsRequest,
) -> Result<(), NetError> {
    let query = Arc::clone(&hub.query);
    let cache = Arc::clone(&hub.cache);
    let reply = tokio::task::spawn_blocking(move || {
        let _g = crate::reactor::BlockingRegion::enter();
        let tip_h = query
            .tip_height()
            .map(|h| h.0)
            .or_else(|| cache.tip_height())
            .unwrap_or(0);
        getblocktxn_reply(cache.as_ref(), query.as_ref(), txs_request, tip_h)
    })
    .await
    .map_err(|_| NetError::Protocol("getblocktxn reconstruct join failed"))??;
    match reply {
        None => Ok(()),
        Some(GetBlockTxnServe::BadIndex) => {
            rbitcoin_log::info!("p2p: getblocktxn with out-of-bounds tx indices");
            // Core `Misbehaving`: noban and manual peers stay connected.
            misbehaving(&mut follow.ban_score, session);
            Ok(())
        }
        Some(GetBlockTxnServe::Block(block)) => {
            out_tx.send(PeerOut::Served(NetworkMessage::Block(block)))
        }
        Some(GetBlockTxnServe::Txns(transactions)) => {
            out_tx.send(PeerOut::Served(NetworkMessage::BlockTxn(BlockTxn {
                transactions,
            })))
        }
    }
}

/// Serve missing txs for a compact block we hold (BIP152). Archive
/// reconstruct stays off the connection task.
#[doc(hidden)]
pub fn getblocktxn_reply(
    cache: &BlockCache,
    query: &Query,
    txs_request: BlockTransactionsRequest,
    tip_h: u32,
) -> Result<Option<GetBlockTxnServe>, NetError> {
    let hash = txs_request.block_hash;
    if header_outside_relay_window(query, &hash) {
        return Ok(None);
    }
    let block = match block_for_peer(cache, query, &hash) {
        Ok(b) => b,
        Err(e) => {
            rbitcoin_log::warn!("p2p: getblocktxn reconstruct {hash}: {e}");
            return Ok(None);
        }
    };
    let Some(block) = block else {
        return Ok(None);
    };
    if !within_relay_window(query, &block) {
        return Ok(None);
    }
    let mut transactions = Vec::with_capacity(txs_request.indexes.len());
    for idx in &txs_request.indexes {
        match block.txdata.get(*idx as usize) {
            Some(tx) => transactions.push(tx.clone()),
            None => return Ok(Some(GetBlockTxnServe::BadIndex)),
        }
    }
    let block_h = served_block_height(query, &block);
    if block_h.is_some_and(|h| tip_h.saturating_sub(h) > MAX_GETBLOCKTXN_DEPTH) {
        return Ok(Some(GetBlockTxnServe::Block(block)));
    }
    Ok(Some(GetBlockTxnServe::Txns(BlockTransactions {
        block_hash: hash,
        transactions,
    })))
}

#[doc(hidden)]
pub fn on_inv(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    items: &[Inventory],
) -> Result<(), NetError> {
    if items.len() > MAX_INV_SIZE {
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    let mut want = Vec::new();
    let mut inv_tx_n = 0u64;
    let mut parent_capped = false;
    let mut need_headers = false;
    let mut tx_inv_hex: Option<String> = None;
    let mut seen_txids: HashSet<bitcoin::Txid> = HashSet::new();
    let mut seen_wtxids: HashSet<bitcoin::Wtxid> = HashSet::new();
    let relay = !hub.in_ibd()
        && (hub.mempool().map(|m| m.relay_enabled()).unwrap_or(false)
            || session.is_some_and(|s| s.session_relay_perm()));
    let mut path_work = PathWorkMemo::default();
    let wtxid_peer_scan = std::cell::OnceCell::new();
    for item in items {
        match item {
            Inventory::Block(h) | Inventory::WitnessBlock(h) => {
                if let Some(s) = session {
                    s.note_block_from_peer(*h);
                }
                note_advanced_best_known(
                    hub,
                    &mut path_work,
                    &follow.pending_headers,
                    session,
                    *h,
                    &follow.pending_no_base,
                );
                if on_inv_block_needs_headers(hub, follow, session, h) {
                    need_headers = true;
                }
            }
            Inventory::Transaction(txid) | Inventory::WitnessTransaction(txid) => {
                if tx_inv_hex.is_none() {
                    tx_inv_hex = Some(txid.to_string());
                }
                // A wtxid-relay peer announces by wtxid. Ignoring only a txid
                // we already have, and fetching one we lack, is the same
                // answer as a node that already has the tx. The inv is still
                // noted so a block-relay peer is disconnected for it.
                if session.is_some_and(|s| s.wtxid_relay()) {
                    continue;
                }
                if !seen_txids.insert(*txid) {
                    continue;
                }
                if let Some(inv) = on_inv_txid(
                    hub,
                    session,
                    relay,
                    txid,
                    &mut parent_capped,
                    &wtxid_peer_scan,
                ) {
                    want.push(inv);
                    inv_tx_n = inv_tx_n.saturating_add(1);
                }
            }
            Inventory::WTx(wtxid) => {
                if tx_inv_hex.is_none() {
                    tx_inv_hex = Some(wtxid.to_string());
                }
                if !seen_wtxids.insert(*wtxid) {
                    continue;
                }
                if let Some(inv) = on_inv_wtxid(hub, session, relay, wtxid, &mut parent_capped) {
                    want.push(inv);
                    inv_tx_n = inv_tx_n.saturating_add(1);
                }
            }
            _ => {}
        }
    }
    if let Some(mp) = hub.mempool() {
        mp.note_inv_tx(inv_tx_n);
        let gd_tx = want
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    Inventory::Transaction(_)
                        | Inventory::WitnessTransaction(_)
                        | Inventory::WTx(_)
                )
            })
            .count() as u64;
        mp.note_getdata_tx(gd_tx);
    }
    if parent_capped {
        punish_disconnect(&mut follow.ban_score, session);
        return Ok(());
    }
    if let Some(hx) = tx_inv_hex {
        if reject_unsolicited_tx(hub, session) {
            rbitcoin_log::info!(
                "p2p: transaction ({hx}) inv sent in violation of protocol, disconnecting peer"
            );
            punish_disconnect(&mut follow.ban_score, session);
            return Ok(());
        }
    }
    if need_headers {
        let _ = queue_getheaders(out_tx, hub, session, true, None);
    }
    if !want.is_empty() {
        out_tx.send(NetworkMessage::GetData(want))?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn on_inv_block_needs_headers(
    hub: &ChainHub,
    follow: &PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    h: &bitcoin::BlockHash,
) -> bool {
    if hub.is_connected(h) {
        return false;
    }
    if hub.knows_header(h) || follow.pending_headers.contains_key(h) {
        return false;
    }
    if hub.in_ibd() {
        hub.note_header_inv_challenge();
    }
    session.is_none_or(|s| {
        s.peer_hub()
            .is_some_and(|ph| ph.should_getheaders_for_inv(s, *h))
    })
}

#[doc(hidden)]
pub fn on_notfound(hub: &ChainHub, session: Option<&crate::peers::LivePeer>, items: &[Inventory]) {
    let Some(s) = session else {
        return;
    };
    let Some(mp) = hub.mempool() else {
        return;
    };
    for item in items {
        let hash = match item {
            Inventory::WTx(w) => w.to_byte_array(),
            Inventory::Transaction(txid) | Inventory::WitnessTransaction(txid) => {
                txid.to_byte_array()
            }
            _ => continue,
        };
        mp.note_tx_not_found(s.id, hash);
    }
}

#[doc(hidden)]
pub fn on_inv_txid(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    relay: bool,
    txid: &bitcoin::Txid,
    parent_capped: &mut bool,
    wtxid_peer_scan: &std::cell::OnceCell<bool>,
) -> Option<Inventory> {
    if !relay {
        return None;
    }
    let mp = hub.mempool()?;
    // Core `AlreadyHaveTx` (without the reconsiderable set): a recent reject
    // is not asked for again, from this peer or another. The child of a
    // rejected parent is in the set by txid.
    if mp.try_recent_reject_txid(txid) {
        return None;
    }
    // Core `AlreadyHaveTx` does not ask the orphanage by txid: a parked
    // witness may not be the one announced.
    if mp.try_contains(txid) && !asks_back_local_tx(mp, session, txid) {
        return None;
    }
    if let Some(s) = session {
        let wtxid_peers =
            *wtxid_peer_scan.get_or_init(|| s.peer_hub().is_some_and(|h| h.has_wtxid_relay_peer()));
        match mp.note_inv_tx_requested(
            tx_inv_source(s, wtxid_peers),
            txid.to_byte_array(),
            s.clock_now(),
            false,
        ) {
            crate::tx_relay::ParentNote::RequestNow => {}
            crate::tx_relay::ParentNote::Deferred => return None,
            crate::tx_relay::ParentNote::PeerCapped => {
                *parent_capped = true;
                return None;
            }
        }
    }
    Some(Inventory::WitnessTransaction(*txid))
}

/// Core `fPreferredDownload`: an outbound peer or an inbound one with
/// `noban`, that is not an addr-fetch connection and can serve blocks.
fn tx_download_preferred(s: &crate::peers::LivePeer) -> bool {
    (!s.inbound || s.session_noban())
        && s.conn_type != crate::peers::PeerConnType::AddrFetch
        && s.can_serve_blocks()
}

/// `wtxid_peers` matters only to a txid announcement, so a wtxid one passes
/// `false` and skips the scan of the live peers.
fn tx_inv_source(s: &crate::peers::LivePeer, wtxid_peers: bool) -> crate::tx_relay::InvSource {
    crate::tx_relay::InvSource {
        peer: s.id,
        preferred: tx_download_preferred(s),
        relay_perm: s.session_relay_perm(),
        wtxid_peers,
    }
}

fn asks_back_local_tx(
    mp: &crate::tx_relay::MempoolHub,
    session: Option<&crate::peers::LivePeer>,
    txid: &bitcoin::Txid,
) -> bool {
    session.is_some_and(|s| s.conn_type != crate::peers::PeerConnType::BlockRelay)
        && mp.skip_standing_inv(txid)
}

#[doc(hidden)]
pub fn on_inv_wtxid(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    relay: bool,
    wtxid: &bitcoin::Wtxid,
    parent_capped: &mut bool,
) -> Option<Inventory> {
    if !relay {
        return None;
    }
    let mp = hub.mempool()?;
    if mp.try_recent_reject(wtxid) {
        return None;
    }
    let ask_back = mp
        .try_txid_for_wtxid(wtxid)
        .is_some_and(|txid| asks_back_local_tx(mp, session, &txid));
    if ask_back || !mp.try_contains_wtxid(wtxid) {
        if let Some(s) = session {
            match mp.note_inv_tx_requested(
                tx_inv_source(s, false),
                wtxid.to_byte_array(),
                s.clock_now(),
                true,
            ) {
                crate::tx_relay::ParentNote::RequestNow => {}
                crate::tx_relay::ParentNote::Deferred => return None,
                crate::tx_relay::ParentNote::PeerCapped => {
                    *parent_capped = true;
                    return None;
                }
            }
        }
        return Some(Inventory::WTx(*wtxid));
    }
    let s = session?;
    let tx = mp.try_orphan_tx_wtxid(wtxid)?;
    note_parked_announcer(mp, s, &tx);
    None
}

/// Core `AddTxAnnouncement` for a parked orphan: `s` becomes an announcer
/// and is asked for the missing parents. An orphan with no parent left to
/// fetch, such as one whose txid is already pooled, takes no announcer.
fn note_parked_announcer(
    mp: &crate::tx_relay::MempoolHub,
    s: &crate::peers::LivePeer,
    parked: &Transaction,
) {
    let missing = mp.orphan_getdata_parents(parked);
    if missing.is_empty() {
        return;
    }
    let _ = mp.add_orphan_announcer(&parked.compute_wtxid(), s.id);
    mp.schedule_orphan_parents(&missing, s.id, tx_download_preferred(s), s.clock_now());
}

struct HeadersApply {
    from_peer: Vec<BlockHash>,
    best_known: Vec<BlockHash>,
    admits: Vec<(BlockHash, bitcoin::block::Header)>,
    evicts: Vec<BlockHash>,
    misbehave: bool,
    /// `(announced height, our tip)` when the peer is a stale fork.
    disconnect: Option<(u32, u32)>,
    sync_height: Option<u32>,
    ignore_height: Option<u32>,
    /// In-flight asks off the stored walk. Applying the batch releases them.
    drop_asks: Vec<BlockHash>,
    want_bodies: Vec<BlockHash>,
    /// Locator for one follow-up `getheaders`, if the batch still wants a page.
    getheaders: Option<Vec<BlockHash>>,
    headers_len: usize,
    /// Best-known header-chain work is under `-minimumchainwork`.
    peer_below_floor: bool,
    /// The last header was new, accepted, connected, and beats our tip work.
    stamp_block_announcement: bool,
    /// Empty or connecting: a reply to our getheaders. Core never counts an
    /// unconnecting message, so its rate limit holds.
    answers_getheaders: bool,
}
/// Drop pending headers until `slots` new headers fit under [`MAX_PENDING_HEADERS`].
///
/// `slots` is how many headers this call would newly store. A header that
/// fails claimed proof of work goes first, then one whose path does not reach
/// a stored header, then a connected fork with less work than `headers`, then
/// one with the same work. Stop once those slots fit. Ancestors of `prev`
/// stay. A fork with more work stays. A fork whose work cannot be scored
/// stays, and so does every connected fork when `headers` cannot.
fn evict_pending_for_room(
    hub: &ChainHub,
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    prev: BlockHash,
    headers: &[bitcoin::block::Header],
    slots: usize,
    gone: &mut Vec<(BlockHash, bitcoin::block::Header)>,
) {
    if !pending_over_cap(pending, slots) {
        return;
    }
    let protected = pending_ancestors(pending, prev);
    let failed_pow: Vec<BlockHash> = pending
        .iter()
        .filter(|(hash, hdr)| !protected.contains(*hash) && !hub.header_claimed_pow_ok(hdr))
        .map(|(hash, _)| *hash)
        .collect();
    drop_hashes_until_fit(pending, slots, failed_pow, gone);
    if !pending_over_cap(pending, slots) {
        return;
    }
    let mut memo = PathWorkMemo::default();
    let failed_forks: Vec<BlockHash> = pending_tips(pending)
        .into_iter()
        .filter(|tip| memo.path_fails_claimed_pow(hub, pending, *tip))
        .collect();
    drop_tips_until_fit(pending, &protected, slots, &failed_forks, gone);
    if !pending_over_cap(pending, slots) {
        return;
    }
    let disconnected = disconnected_pending(hub, pending, &protected);
    drop_hashes_until_fit(pending, slots, disconnected, gone);
    let Some(incoming_work) = incoming_header_work(hub, pending, headers) else {
        return;
    };
    evict_forks_with_work(
        hub,
        pending,
        &protected,
        (slots, incoming_work),
        std::cmp::Ordering::Less,
        gone,
    );
    evict_forks_with_work(
        hub,
        pending,
        &protected,
        (slots, incoming_work),
        std::cmp::Ordering::Equal,
        gone,
    );
}

fn pending_over_cap(pending: &HashMap<BlockHash, bitcoin::block::Header>, incoming: usize) -> bool {
    pending.len().saturating_add(incoming) > MAX_PENDING_HEADERS
}

fn drop_hashes_until_fit(
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    incoming: usize,
    hashes: impl IntoIterator<Item = BlockHash>,
    gone: &mut Vec<(BlockHash, bitcoin::block::Header)>,
) {
    for hash in hashes {
        if !pending_over_cap(pending, incoming) {
            return;
        }
        if let Some(hdr) = pending.remove(&hash) {
            gone.push((hash, hdr));
        }
    }
}

fn drop_tips_until_fit(
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    protected: &HashSet<BlockHash>,
    incoming: usize,
    tips: &[BlockHash],
    gone: &mut Vec<(BlockHash, bitcoin::block::Header)>,
) {
    for &tip in tips {
        let mut hash = tip;
        loop {
            if !pending_over_cap(pending, incoming) {
                return;
            }
            if protected.contains(&hash) {
                break;
            }
            let Some(hdr) = pending.remove(&hash) else {
                break;
            };
            gone.push((hash, hdr));
            hash = hdr.prev_blockhash;
        }
    }
}

fn pending_tips(pending: &HashMap<BlockHash, bitcoin::block::Header>) -> Vec<BlockHash> {
    let has_child: HashSet<BlockHash> = pending.values().map(|hdr| hdr.prev_blockhash).collect();
    pending
        .keys()
        .copied()
        .filter(|hash| !has_child.contains(hash))
        .collect()
}

fn disconnected_pending(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    protected: &HashSet<BlockHash>,
) -> Vec<BlockHash> {
    let no_pending = HashMap::new();
    let mut root_of: HashMap<BlockHash, BlockHash> = HashMap::new();
    let mut root_connects: HashMap<BlockHash, bool> = HashMap::new();
    let hashes: Vec<BlockHash> = pending.keys().copied().collect();
    for &hash in &hashes {
        let root = pending_root(pending, hash, &mut root_of);
        root_connects.entry(root).or_insert_with(|| {
            let parent = pending[&root].prev_blockhash;
            parent != BlockHash::all_zeros()
                && header_announcement_connects(hub, &no_pending, parent)
        });
    }
    hashes
        .into_iter()
        .filter(|hash| !protected.contains(hash) && !root_connects[&root_of[hash]])
        .collect()
}

/// Pending slots this message needs. A header this call would write, and
/// that is not already in `pending`, takes one. A repeat takes one. A header
/// the store already has, or that is not on the walk from the last hash,
/// takes none.
fn newly_stored_slot_count(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    headers: &[bitcoin::block::Header],
) -> usize {
    let Some(last) = headers.last() else {
        return 0;
    };
    let mut batch = HashMap::with_capacity(headers.len());
    for hdr in headers {
        batch.insert(hdr.block_hash(), *hdr);
    }
    // The message overwrites a hash already in `pending`, matching the admit
    // that runs before the store walk.
    unstored_header_path(hub, last.block_hash(), |hash| {
        batch
            .get(&hash)
            .copied()
            .or_else(|| pending.get(&hash).copied())
    })
    .into_iter()
    .filter(|hdr| !pending.contains_key(&hdr.block_hash()))
    .count()
}

fn queue_batch_admits(apply: &mut HeadersApply, headers: &[bitcoin::block::Header]) {
    let mut seen = HashSet::new();
    for hdr in headers {
        let hash = hdr.block_hash();
        if seen.insert(hash) {
            apply.admits.push((hash, *hdr));
        }
    }
}

/// Headers in this message that `persist_pending_header_path` actually wrote.
/// A sibling that is not on the walk from the last header stays out.
fn queue_stored_header_admits(
    apply: &mut HeadersApply,
    hub: &ChainHub,
    headers: &[bitcoin::block::Header],
) {
    let mut seen = HashSet::new();
    for hdr in headers {
        let hash = hdr.block_hash();
        if seen.insert(hash) && header_is_stored(hub, &hash) {
            apply.admits.push((hash, *hdr));
        }
    }
}

/// A header the store refused scores its sender unless the fault is ours or
/// its time is only too far ahead (Core `BLOCK_TIME_FUTURE`). Callers only
/// ask for a header path that connects to a known chain.
fn header_reject_scores(e: &NetError) -> bool {
    !e.is_local_fault() && !crate::chain::accept_err_is_temporary_time(e)
}

/// Persist the walk from `last`, then admit only the headers the store kept.
fn note_stored_header_path(
    hub: &ChainHub,
    apply: &mut HeadersApply,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    headers: &[bitcoin::block::Header],
    last: BlockHash,
    work_cmp: Option<std::cmp::Ordering>,
    announced_h: u32,
) -> bool {
    let should_stamp =
        work_cmp == Some(std::cmp::Ordering::Greater) && !header_is_stored(hub, &last);
    let persisted = persist_pending_header_path(hub, pending, last);
    // Core `BLOCK_INVALID_HEADER`: a header the store refuses (bad
    // proof of work, bits, or time at or below the median) scores the peer.
    // A local fault or a stamp still in the future is not the peer's.
    if persisted.as_ref().is_err_and(header_reject_scores) {
        apply.misbehave = true;
    }
    let stored = persisted.is_ok();
    apply.stamp_block_announcement = should_stamp && header_is_stored(hub, &last);
    queue_stored_header_admits(apply, hub, headers);
    apply.sync_height = Some(announced_h);
    stored
}

/// Drop in-flight asks that left the stored walk, and ask for bodies on it.
fn retarget_stored_header_asks(
    hub: &ChainHub,
    apply: &mut HeadersApply,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    last: BlockHash,
    requested_before: &HashSet<BlockHash>,
    have_bodies: &HashSet<BlockHash>,
) {
    let keep = connecting_header_path(hub, pending, last);
    apply.drop_asks = asks_off_path(requested_before, &keep);
    let mut requested = requested_before.clone();
    for h in &apply.drop_asks {
        requested.remove(h);
    }
    let mut want =
        fetchable_header_path_bodies(hub, pending, last, &PendingBlocks::new(), &requested);
    want.retain(|h| !have_bodies.contains(h) && !requested.contains(h));
    want.truncate(MAX_SERVE_BLOCKS.saturating_sub(requested.len()));
    apply.want_bodies = want;
}

fn incoming_header_work(
    hub: &ChainHub,
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    headers: &[bitcoin::block::Header],
) -> Option<bitcoin::Work> {
    let last = headers.last()?.block_hash();
    with_batch(pending, headers, |p| work_of_header_path(hub, p, last))
}

fn evict_forks_with_work(
    hub: &ChainHub,
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    protected: &HashSet<BlockHash>,
    (incoming, incoming_work): (usize, bitcoin::Work),
    want: std::cmp::Ordering,
    gone: &mut Vec<(BlockHash, bitcoin::block::Header)>,
) {
    if !pending_over_cap(pending, incoming) {
        return;
    }
    let mut keep = protected.clone();
    let mut victims = Vec::new();
    let mut memo = PathWorkMemo::default();
    for tip in pending_tips(pending) {
        if memo.path_fails_claimed_pow(hub, pending, tip) {
            continue;
        }
        match memo.work_of_header_path(hub, pending, tip) {
            Some(work) if work.cmp(&incoming_work) == want => victims.push(tip),
            _ => add_pending_ancestors(&mut keep, pending, tip),
        }
    }
    drop_tips_until_fit(pending, &keep, incoming, &victims, gone);
}

/// `start` and its ancestors that are in `pending`, walking at most
/// `PENDING_WALK_LIMIT` steps. Reads only the pending map.
fn pending_ancestors(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
) -> HashSet<BlockHash> {
    let mut out = HashSet::new();
    add_pending_ancestors(&mut out, pending, start);
    out
}

/// Add `start` and its pending ancestors to `out`. A hash already in `out`
/// ends the walk: `out` holds whole ancestor chains, so the rest is there.
fn add_pending_ancestors(
    out: &mut HashSet<BlockHash>,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
) {
    let mut h = start;
    for _ in 0..PENDING_WALK_LIMIT {
        let Some(hdr) = pending.get(&h) else {
            break;
        };
        if !out.insert(h) {
            break;
        }
        h = hdr.prev_blockhash;
    }
}

/// The oldest pending ancestor of `hash` (whose parent is not pending),
/// memoised in `root_of`. Bounded by `PENDING_WALK_LIMIT`.
fn pending_root(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    hash: BlockHash,
    root_of: &mut HashMap<BlockHash, BlockHash>,
) -> BlockHash {
    let mut path = Vec::new();
    let mut h = hash;
    let root = loop {
        if let Some(r) = root_of.get(&h) {
            break *r;
        }
        path.push(h);
        let parent = pending[&h].prev_blockhash;
        if !pending.contains_key(&parent) || path.len() >= PENDING_WALK_LIMIT {
            break h;
        }
        h = parent;
    };
    for p in path {
        root_of.insert(p, root);
    }
    root
}

/// Classify `headers` against the peer's `pending` map. The map comes back
/// as it was: the staging below changes it in place and is undone here,
/// so a message costs its own headers, not a copy of the map.
fn classify_headers(
    hub: &ChainHub,
    headers: &[bitcoin::block::Header],
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    requested_before: &HashSet<BlockHash>,
    have_bodies: &HashSet<BlockHash>,
    noban: bool,
    no_base: &NoBaseMap,
) -> HeadersApply {
    let mut undo = PendingUndo::default();
    let apply = classify_staged(
        hub,
        headers,
        pending,
        &mut undo,
        requested_before,
        have_bodies,
        noban,
        no_base,
    );
    undo.revert(pending);
    apply
}

/// [`classify_headers`] with each change to `pending` logged in `undo`.
#[allow(clippy::too_many_arguments)] // no-base marks ride with the staged map
fn classify_staged(
    hub: &ChainHub,
    headers: &[bitcoin::block::Header],
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    undo: &mut PendingUndo,
    requested_before: &HashSet<BlockHash>,
    have_bodies: &HashSet<BlockHash>,
    noban: bool,
    no_base: &NoBaseMap,
) -> HeadersApply {
    let n = headers.len();
    let mut apply = HeadersApply {
        from_peer: Vec::new(),
        best_known: Vec::new(),
        admits: Vec::new(),
        evicts: Vec::new(),
        misbehave: false,
        disconnect: None,
        sync_height: None,
        ignore_height: None,
        drop_asks: Vec::new(),
        want_bodies: Vec::new(),
        getheaders: None,
        headers_len: n,
        peer_below_floor: false,
        stamp_block_announcement: false,
        answers_getheaders: n == 0,
    };
    let mut ignored_below_floor = false;
    let mut ask_from_tip = false;
    if n == 0 {
        // Empty headers is a failed getheaders response, not an announcement.
    } else if !noban && headers_below_floor(hub, pending, headers, no_base) {
        // Work stays under the floor from the pending headers alone. Do not
        // read parent headers, do not move this peer's best-known tip, and
        // do not ask for the next page of a batch we refused.
        // No header reads: the batch answers our getheaders when its pending
        // path joins a best-chain block (Core: parent in the block index).
        let (join, _) = pending_walk_in(pending, headers[0].prev_blockhash, |h| {
            no_base_depth(hub, no_base, h)
        });
        for hdr in headers.iter().take(n) {
            let hash = hdr.block_hash();
            apply.from_peer.push(hash);
            undo.admit(pending, hash, *hdr);
            apply.admits.push((hash, *hdr));
        }
        let last = headers[n - 1].block_hash();
        apply.ignore_height = Some(pending_announced_height_in(hub, pending, last, no_base));
        ignored_below_floor = true;
        apply.answers_getheaders = is_genesis_hash(&join) || hub.is_connected(&join);
    } else if let Some(first) = headers.first() {
        let prev = first.prev_blockhash;
        if hub.is_block_invalid(&prev)
            || headers
                .iter()
                .take(n)
                .any(|h| hub.is_block_invalid(&h.block_hash()))
        {
            // Headers on a cached-invalid chain: disconnect
            // (`p2p_unrequested_blocks` step 8 follow-up header).
            apply.misbehave = true;
            return apply;
        }
        // The all-zero hash is only genesis's parent. A header claiming it is
        // not a continuation of any chain we hold.
        let connecting =
            prev != BlockHash::all_zeros() && header_connects(hub, pending, prev, no_base);
        apply.answers_getheaders = connecting;
        // Only a connecting batch is stored, so only it needs room.
        let slots = if connecting {
            newly_stored_slot_count(hub, pending, headers)
        } else {
            0
        };
        let mut staged_evicts = Vec::new();
        if connecting && pending.len().saturating_add(slots) > MAX_PENDING_HEADERS {
            // Claimed proof of work includes the chain pow limit. Evictions
            // stay staged until the store accepts the batch.
            if !headers
                .iter()
                .take(n)
                .all(|hdr| hub.header_claimed_pow_ok(hdr))
            {
                return apply;
            }
            let mut gone = Vec::new();
            evict_pending_for_room(hub, pending, prev, headers, slots, &mut gone);
            undo.removed(&gone);
            if pending.len().saturating_add(slots) > MAX_PENDING_HEADERS {
                return apply;
            }
            staged_evicts = gone.into_iter().map(|(hash, _)| hash).collect();
        }
        for hdr in headers.iter().take(n) {
            let hash = hdr.block_hash();
            apply.from_peer.push(hash);
            apply.best_known.push(hash);
            // The real map caps in `apply_headers`. The staged map has to
            // hold the whole message so the store walk can see the last header.
            undo.insert(pending, hash, *hdr);
        }
        if !connecting {
            // These headers are not sent to the store. A later parent can
            // still join them. A short batch asks from our tip. A full page
            // continues from its last hash, below.
            queue_batch_admits(&mut apply, headers);
            ask_from_tip = true;
        } else {
            let last = headers[n - 1].block_hash();
            // Core `chain_start.nHeight + headers.size()`. One-header
            // tip announces still accumulate via `follow.pending_headers`
            // (`p2p_headers_sync_with_minchainwork` height=14).
            let announced_h = announced_headers_height(hub, pending, last);
            let work_cmp = announced_work_cmp(hub, pending, last);
            let our_tip = hub.tip_height().unwrap_or(0);
            if announced_tip_is_hopeless(our_tip, announced_h, work_cmp) && !noban {
                queue_batch_admits(&mut apply, headers);
                apply.disconnect = Some((announced_h, our_tip));
                return apply;
            }
            let mut stored = false;
            if !header_path_meets_minwork(hub, pending, last) {
                if noban {
                    stored = note_stored_header_path(
                        hub,
                        &mut apply,
                        pending,
                        headers,
                        last,
                        work_cmp,
                        announced_h,
                    );
                } else {
                    // Below the floor this path does not touch the store.
                    queue_batch_admits(&mut apply, headers);
                    apply.ignore_height = Some(announced_h);
                    ignored_below_floor = true;
                }
                // Core: do not download bodies until the chain meets
                // `-minimumchainwork` (`p2p_headers_sync_with_minchainwork`).
            } else {
                // This check is deliberately after the below-floor early path:
                // that path must not touch the header store or walk parents.
                stored = note_stored_header_path(
                    hub,
                    &mut apply,
                    pending,
                    headers,
                    last,
                    work_cmp,
                    announced_h,
                );
                // Asks follow the stored walk only. A rejected header must
                // not drop an in-flight body or start a new one.
                if stored {
                    retarget_stored_header_asks(
                        hub,
                        &mut apply,
                        pending,
                        last,
                        requested_before,
                        have_bodies,
                    );
                }
            }
            if stored {
                apply.evicts = staged_evicts;
            }
        }
    }
    if n >= MAX_HEADERS_RESULTS && !ignored_below_floor {
        let from = headers.get(n - 1).map(|h| h.block_hash());
        apply.getheaders = Some(headers_sync_locator(hub, from));
    } else if ask_from_tip {
        apply.getheaders = Some(headers_sync_locator(hub, None));
    }
    if n > 0 {
        if let Some(last) = headers.last() {
            apply.peer_below_floor =
                peer_headers_below_floor(hub, pending, last.block_hash(), no_base);
        }
    }
    apply
}

/// This headers message ended the peer's chain, and that chain's work is
/// under `-minimumchainwork`, while we are still in IBD.
///
/// Core drops outbound full-relay and block-relay peers there. A full page
/// can continue, so it is not their tip yet. Inbound and manual peers stay.
/// `prior_best_meets_floor` is the peer's stored best-known header from
/// before this message. Core keeps that when the new header has less work.
#[doc(hidden)]
pub fn outbound_insufficient_work_disconnect(
    in_ibd: bool,
    header_count: usize,
    conn: crate::peers::PeerConnType,
    peer_below_floor: bool,
    prior_best_meets_floor: bool,
) -> bool {
    in_ibd
        && header_count > 0
        && header_count < MAX_HEADERS_RESULTS
        && peer_below_floor
        && !prior_best_meets_floor
        && matches!(
            conn,
            crate::peers::PeerConnType::OutboundFullRelay | crate::peers::PeerConnType::BlockRelay
        )
}

fn peer_headers_below_floor(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    no_base: &NoBaseMap,
) -> bool {
    let Some(min) = hub.min_chain_work_floor() else {
        return false;
    };
    // Pending work only. `work_of_header_path` reads parent headers, and a
    // below-floor batch must not.
    match pending_path_work_in(hub, pending, tip, no_base) {
        Some(work) => work.to_be_bytes() < min,
        None => false,
    }
}

fn hash_meets_work_floor(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    hash: BlockHash,
    remembered: Option<bitcoin::Work>,
    no_base: &NoBaseMap,
) -> bool {
    let Some(min) = hub.min_chain_work_floor() else {
        return false;
    };
    // The pending map and the tip index can both forget a side header.
    // The work measured when we stored the hash still counts.
    let work = pending_path_work_in(hub, pending, hash, no_base).or(remembered);
    match work {
        Some(work) => work.to_be_bytes() >= min,
        None => false,
    }
}

/// Core moves `pindexBestKnownBlock` only when the new header's work is
/// greater or equal. Unknown work does not replace a stored header.
fn best_known_moves_forward(
    hub: &ChainHub,
    memo: &mut PathWorkMemo,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    current: Option<BlockHash>,
    next: BlockHash,
    remembered_old: Option<bitcoin::Work>,
    no_base: &NoBaseMap,
) -> Option<bitcoin::Work> {
    let next_work = memo.pending_path_work_stopping(hub, pending, next, no_base)?;
    let Some(old) = current else {
        return Some(next_work);
    };
    let old_work = memo
        .pending_path_work_stopping(hub, pending, old, no_base)
        .or(remembered_old);
    match old_work {
        Some(old_work) if next_work >= old_work => Some(next_work),
        Some(_) => None,
        None => Some(next_work),
    }
}

/// `memo` may be shared by the items of one message. The pending map must
/// not change while it is.
fn note_advanced_best_known(
    hub: &ChainHub,
    memo: &mut PathWorkMemo,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
    no_base: &NoBaseMap,
) {
    let Some(s) = session else {
        return;
    };
    if let Some(work) = best_known_moves_forward(
        hub,
        memo,
        pending,
        s.best_known(),
        hash,
        s.best_known_work(),
        no_base,
    ) {
        s.note_best_known_with_work(hash, work);
    }
}

/// Record `hdr` after it is admitted. A header whose parent is already marked
/// keeps that unknown parent and one more step. A header that reaches a
/// header we have, including through a parked parent that already does,
/// drops every mark waiting on it. While the parent is still unknown, marks
/// already parked on this hash stay as they are. A new orphan root is one step.
fn remember_admitted(
    hub: &ChainHub,
    follow: &mut PeerFollowState,
    hash: BlockHash,
    hdr: bitcoin::block::Header,
) {
    if admit_pending_header(&mut follow.pending_headers, hash, hdr).is_none() {
        return;
    }
    let prev = hdr.prev_blockhash;
    let parent = follow.pending_no_base.get(&prev).copied();
    let parent_parked = follow.pending_headers.contains_key(&prev);
    let own = if let Some((root, depth)) = parent {
        Some((root, depth.saturating_add(1)))
    } else if is_genesis_hash(&prev) || hub.knows_header(&prev) || parent_parked {
        None
    } else {
        Some((prev, 1))
    };
    let no_base = Arc::make_mut(&mut follow.pending_no_base);
    retarget_no_base(no_base, &mut follow.pending_no_base_kids, hash, own);
}

/// Point `hash` at `root`, and move it off the previous root's child list.
fn set_no_base(
    no_base: &mut NoBaseMap,
    kids: &mut HashMap<BlockHash, HashSet<BlockHash>>,
    hash: BlockHash,
    root: BlockHash,
    depth: u32,
) {
    if let Some((old, _)) = no_base.insert(hash, (root, depth)) {
        if old != root {
            if let Some(set) = kids.get_mut(&old) {
                set.remove(&hash);
                if set.is_empty() {
                    kids.remove(&old);
                }
            }
        }
    }
    kids.entry(root).or_default().insert(hash);
}

/// Drop `hash`'s mark. Child marks that name `hash` stay, so a later admit
/// of this same header can still find them.
fn unlink_no_base(
    no_base: &mut NoBaseMap,
    kids: &mut HashMap<BlockHash, HashSet<BlockHash>>,
    hash: &BlockHash,
) {
    let Some((root, _)) = no_base.remove(hash) else {
        return;
    };
    if let Some(set) = kids.get_mut(&root) {
        set.remove(hash);
        if set.is_empty() {
            kids.remove(&root);
        }
    }
}

/// `own` is where `hash` sits after this admit. `None` means it reaches a
/// header this node has, so `hash` and every mark parked on it go away.
/// `Some` records only this header. Rewriting descendants while the parent
/// is still unknown copies the parked set on every newer-first admit.
fn retarget_no_base(
    no_base: &mut NoBaseMap,
    kids: &mut HashMap<BlockHash, HashSet<BlockHash>>,
    hash: BlockHash,
    own: Option<(BlockHash, u32)>,
) {
    let Some((root, base)) = own else {
        let mut batch = Vec::new();
        let mut stack = vec![hash];
        let mut seen = HashSet::new();
        seen.insert(hash);
        while let Some(node) = stack.pop() {
            if batch.len() >= MAX_PENDING_HEADERS {
                break;
            }
            let Some(childs) = kids.get(&node) else {
                continue;
            };
            let childs: Vec<BlockHash> = childs.iter().copied().collect();
            for kid in childs {
                if !seen.insert(kid) {
                    continue;
                }
                if no_base.contains_key(&kid) {
                    batch.push(kid);
                    stack.push(kid);
                }
            }
        }
        for kid in &batch {
            unlink_no_base(no_base, kids, kid);
            kids.remove(kid);
        }
        unlink_no_base(no_base, kids, &hash);
        kids.remove(&hash);
        return;
    };
    set_no_base(no_base, kids, hash, root, base);
}

fn forget_no_base(follow: &mut PeerFollowState, hash: &BlockHash) {
    if !follow.pending_no_base.contains_key(hash) {
        return;
    }
    let no_base = Arc::make_mut(&mut follow.pending_no_base);
    unlink_no_base(no_base, &mut follow.pending_no_base_kids, hash);
}

fn apply_headers(
    apply: HeadersApply,
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    if apply.answers_getheaders {
        let _ = session.is_some_and(|s| s.take_awaiting_headers());
    }
    if apply.misbehave {
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    // Read before this message. The disconnect compares that hash and the
    // work saved with it, which outlives the pending map.
    let prior_best = session.and_then(|s| s.best_known());
    let prior_work = session.and_then(|s| s.best_known_work());
    for hash in apply.evicts {
        follow.pending_headers.remove(&hash);
        forget_no_base(follow, &hash);
    }
    for hash in apply.from_peer {
        if let Some(s) = session {
            s.note_block_from_peer(hash);
        }
    }
    for (hash, hdr) in apply.admits {
        remember_admitted(hub, follow, hash, hdr);
    }
    let mut path_work = PathWorkMemo::default();
    for hash in apply.best_known {
        note_advanced_best_known(
            hub,
            &mut path_work,
            &follow.pending_headers,
            session,
            hash,
            &follow.pending_no_base,
        );
    }
    if let Some((announced_h, our_tip)) = apply.disconnect {
        rbitcoin_log::info!("p2p: disconnect stale fork tip announced={announced_h} our={our_tip}");
        if let Some(s) = session {
            s.request_disconnect();
        }
        return Ok(());
    }
    if let Some(announced_h) = apply.sync_height {
        rbitcoin_log::info!("{}", synchronizing_blockheaders_log(announced_h));
    }
    if let Some(announced_h) = apply.ignore_height {
        rbitcoin_log::info!("{}", ignoring_low_work_chain_log(announced_h));
    }
    if let Some(s) = session.filter(|s| {
        let prior_meets = apply.peer_below_floor
            && prior_best.is_some_and(|h| {
                hash_meets_work_floor(
                    hub,
                    &follow.pending_headers,
                    h,
                    prior_work,
                    &follow.pending_no_base,
                )
            });
        outbound_insufficient_work_disconnect(
            hub.in_ibd(),
            apply.headers_len,
            s.conn_type,
            apply.peer_below_floor,
            prior_meets,
        )
    }) {
        rbitcoin_log::info!(
            "outbound peer headers chain has insufficient work, peer={}",
            s.id
        );
        s.request_disconnect();
        return Ok(());
    }
    for hash in &apply.drop_asks {
        follow.requested_blocks.remove(hash);
    }
    // The batch is already persisted. A spent send budget does not claim
    // bodies or queue another locator. The 120s poll can ask again.
    if session.is_some_and(|s| s.send_over_budget()) {
        return Ok(());
    }
    // Claim on this session, next to the queue. The header job only
    // filters. An aborted join never publishes an ask that was not sent.
    queue_claimed_block_getdata(
        hub,
        out_tx,
        session,
        &mut follow.requested_blocks,
        &apply.want_bodies,
        getdata_use_compact(hub, follow.cmpct_version),
    )?;
    if let Some(locator) = apply.getheaders {
        queue_headers_locator(out_tx, session, locator)?;
    }
    Ok(())
}

/// Claim each body on the hub, then queue one `getdata` on this session.
/// A body another session already claimed is skipped. A failed send
/// releases every claim it took, so the next announcement can ask.
fn queue_claimed_block_getdata(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    requested_blocks: &mut RequestedBlocks,
    want: &[BlockHash],
    compact: bool,
) -> Result<(), NetError> {
    let claimed: Vec<BlockHash> = want
        .iter()
        .copied()
        .filter(|hash| requested_blocks.claim(hub, *hash))
        .collect();
    let sent = queue_block_getdata(hub, out_tx, session, requested_blocks, &claimed, compact);
    if sent.is_err() {
        for hash in &claimed {
            requested_blocks.remove(hash);
        }
        return sent;
    }
    if compact {
        for hash in &claimed {
            if compact_getdata_near_tip(hub, hash) {
                ask_other_hb_for_block(session, *hash);
            }
        }
    }
    sent
}

/// While one peer owes a body, ask other high-bandwidth compact peers too.
/// At most two more, and an outbound when the announcer is inbound and one
/// exists. The hub claim stays with the first ask.
fn ask_other_hb_for_block(session: Option<&crate::peers::LivePeer>, hash: BlockHash) {
    let Some(announcer) = session else {
        return;
    };
    let Some(peers) = announcer.peer_hub() else {
        return;
    };
    let mut rest: Vec<std::sync::Arc<crate::peers::LivePeer>> = peers
        .live_peers()
        .into_iter()
        .filter(|p| {
            p.id != announcer.id
                && p.hb_to.load(Ordering::Relaxed)
                && !p.stop.load(Ordering::Relaxed)
                && p.writer().is_some()
        })
        .collect();
    let mut picked = Vec::new();
    if announcer.inbound {
        if let Some(i) = rest.iter().position(|p| !p.inbound) {
            picked.push(rest.remove(i));
        }
    }
    for peer in rest {
        if picked.len() >= 2 {
            break;
        }
        picked.push(peer);
    }
    if picked.is_empty() || !announcer.claim_hb_body_fanout(hash) {
        return;
    }
    for peer in picked {
        let Some(out) = peer.writer() else {
            continue;
        };
        let _ = out.send(NetworkMessage::GetData(vec![Inventory::WitnessBlock(hash)]));
    }
}

/// Send a locator the header job already built. Same in-flight gate as
/// [`queue_getheaders`].
fn queue_headers_locator(
    out: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    locator: Vec<BlockHash>,
) -> Result<(), NetError> {
    if let Some(s) = session {
        if s.is_awaiting_headers() {
            return Ok(());
        }
        s.note_awaiting_headers();
    }
    let gh = GetHeadersMessage::new(locator, BlockHash::from_byte_array([0u8; 32]));
    out.send(NetworkMessage::GetHeaders(gh))
}

#[doc(hidden)]
pub async fn on_headers(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    headers: &[bitcoin::block::Header],
) -> Result<(), NetError> {
    let noban = session.is_some_and(|s| s.session_noban());
    let apply = if headers.is_empty() {
        // An empty reply reads neither the store nor the peer's maps, so it
        // takes no header permit and touches no pending headers.
        let none = HashSet::new();
        classify_headers(
            hub,
            &[],
            &mut HashMap::new(),
            &none,
            &none,
            noban,
            &follow.pending_no_base,
        )
    } else {
        // The job borrows the map by moving it and hands it back: a copy per
        // message cost the whole map (up to `MAX_PENDING_HEADERS`) each time.
        // A job that fails drops the map; that error ends the session anyway.
        // The no-base marks are shared, not copied: the job only reads them.
        let no_base = Arc::clone(&follow.pending_no_base);
        let mut pending = std::mem::take(&mut follow.pending_headers);
        let requested = follow.requested_blocks.hashes().clone();
        let have_bodies: HashSet<BlockHash> = follow.pending_blocks.keys().copied().collect();
        let headers = headers.to_vec();
        let (apply, pending) = run_header_job(hub, move |hub| {
            let apply = classify_headers(
                hub,
                &headers,
                &mut pending,
                &requested,
                &have_bodies,
                noban,
                &no_base,
            );
            (apply, pending)
        })
        .await?;
        follow.pending_headers = pending;
        apply
    };
    let accepted = !apply.misbehave;
    let stamp_block_announcement = apply.stamp_block_announcement;
    apply_headers(apply, hub, out_tx, follow, session)?;
    if accepted && stamp_block_announcement {
        if let Some(s) = session {
            s.note_last_block_announcement();
        }
    }
    Ok(())
}

/// Core `BLOCK_MUTATED`: the copy is dropped, the block is not marked
/// invalid, and the sender is punished. Core's next `SendMessages` asks for
/// the block again while it is on the best header chain. A session that
/// [`misbehaving`] keeps (manual or noban) asks at once; one it drops leaves
/// the block to the other sessions, as the released ask allows.
async fn on_mutated_block(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    ask: Option<BlockAsk>,
    reason: &str,
) -> Result<(), NetError> {
    rbitcoin_log::info!("Block mutated: {reason}");
    // `drain_pending` skips a body that is still asked.
    drop(ask);
    misbehaving(&mut follow.ban_score, session);
    if session.is_some_and(misbehavior_exempt) {
        drain_pending(
            hub,
            out_tx,
            &mut follow.pending_blocks,
            &mut follow.pending_headers,
            &mut follow.requested_blocks,
            getdata_use_compact(hub, follow.cmpct_version),
            session,
        )
        .await?;
    }
    Ok(())
}

pub async fn on_block(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    block: &Block,
) -> Result<(), NetError> {
    let hash = block.block_hash();
    let merkle = crate::compact::block_txid_merkle(&block.txdata);
    let mutated = if merkle.is_none_or(|(root, _)| root != block.header.merkle_root.to_byte_array())
    {
        Some("bad-txnmrklroot, hashMerkleRoot mismatch")
    } else if merkle.is_some_and(|(_, mutated)| mutated) {
        Some("bad-txns-duplicate")
    } else if rbitcoin_consensus::block_mutated_without_coinbase(block) {
        Some("64-byte transaction without a coinbase")
    } else {
        None
    };
    if let Some(reason) = mutated {
        let ask = follow.requested_blocks.remove(&hash);
        return on_mutated_block(hub, out_tx, follow, session, ask, reason).await;
    }
    if let Some(s) = session {
        s.note_block_from_peer(hash);
    }
    note_advanced_best_known(
        hub,
        &mut PathWorkMemo::default(),
        &follow.pending_headers,
        session,
        hash,
        &follow.pending_no_base,
    );
    // One frame at a time: the ping behind this body is not read until we
    // return. A duplicate must not take the process-wide tip-accept lane.
    if hub.already_have(hash) {
        drop_pending_cmpct(follow, session, hash);
        let ask = follow.requested_blocks.remove(&hash);
        hub.drop_held(hash);
        return drain_after_accept(hub, out_tx, follow, session, hash, ask, false).await;
    }
    let prev = block.header.prev_blockhash;
    if prev.to_byte_array() != [0u8; 32]
        && !hub.knows_header(&prev)
        && !follow.pending_headers.contains_key(&prev)
        && follow.requested_blocks.contains(&hash)
    {
        rbitcoin_log::info!("{}", accept_prev_not_found_log(hash));
        if let Some(s) = session {
            let _ = s.take_awaiting_headers();
        }
        let _ = queue_getheaders(out_tx, hub, session, true, None);
        remember_admitted(hub, follow, hash, block.header);
        follow.pending_blocks.insert(hash, block.clone(), None);
        follow.requested_blocks.release_ask(&hash);
        return Ok(());
    }
    if on_block_unrequested_skip(hub, follow, session, block, hash)? {
        return Ok(());
    }
    if let Err(e) = ensure_peer_header(hub, &block.header) {
        if e.is_local_fault() || crate::chain::accept_err_is_temporary_time(&e) {
            follow.requested_blocks.remove(&hash);
            return Ok(());
        }
        misbehaving(&mut follow.ban_score, session);
        if e.consensus_rules().is_some() {
            hub.remember_failed_accept(hash, &e);
        }
        return Ok(());
    }
    drop_pending_cmpct(follow, session, hash);
    let ask = follow.requested_blocks.remove(&hash);
    remember_admitted(hub, follow, hash, block.header);
    if !any_header_path_meets_minwork_in(
        hub,
        &follow.pending_headers,
        hash,
        &follow.pending_no_base,
    ) {
        follow.pending_blocks.insert(hash, block.clone(), ask);
        return Ok(());
    }
    // T2: a body already marked invalid costs no lane slot and is not
    // re-announced. Placed after the header/PoW checks so a bad-proof-of-work
    // header still scores the peer. No ban here: the sender may not know.
    if hub.is_block_invalid(&hash) {
        hub.drop_held(hash);
        return Ok(());
    }
    relay_new_pow_valid_block(hub, block, session).await;
    on_block_accept(hub, out_tx, follow, session, block, hash, ask).await
}

#[allow(clippy::unnecessary_get_then_check)]
#[doc(hidden)]
pub fn on_block_unrequested_skip(
    hub: &ChainHub,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    block: &Block,
    hash: BlockHash,
) -> Result<bool, NetError> {
    if follow.requested_blocks.contains(&hash) {
        return Ok(false);
    }
    let prev = block.header.prev_blockhash;
    if prev.to_byte_array() != [0u8; 32]
        && !hub.knows_header(&prev)
        && follow.pending_headers.get(&prev).is_none()
    {
        rbitcoin_log::info!("{}", accept_prev_not_found_log(hash));
        misbehaving(&mut follow.ban_score, session);
        return Ok(true);
    }
    if let Err(e) = ensure_peer_header(hub, &block.header) {
        if e.is_local_fault() || crate::chain::accept_err_is_temporary_time(&e) {
            return Ok(true);
        }
        misbehaving(&mut follow.ban_score, session);
        if e.consensus_rules().is_some() {
            hub.remember_failed_accept(hash, &e);
        }
        return Ok(true);
    }
    if hub.header_below_minwork(&block.header) {
        rbitcoin_log::info!("{}", accept_block_header_nodos_log(hash));
        return Ok(true);
    }
    if hub.header_below_anti_dos(&block.header) && !follow.pending_headers.contains_key(&hash) {
        return Ok(true);
    }
    if hub.unrequested_too_far_ahead(&block.header) {
        return Ok(true);
    }
    Ok(false)
}

/// Local IO fault: keep the session. The block is not a verdict, so its ask
/// is dropped and a later getdata can claim the same hash.
fn note_store_fault_keep_session(hash: BlockHash, e: &NetError) {
    rbitcoin_log::warn!("p2p: accept dropped {hash} (local fault — keep session): {e}");
}

pub async fn on_block_accept(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    block: &Block,
    hash: BlockHash,
    ask: Option<BlockAsk>,
) -> Result<(), NetError> {
    let outcome = accept_received_from_peer(hub, block.clone(), session).await;
    finish_block_accept(hub, out_tx, follow, session, block, hash, ask, outcome).await
}

fn accept_mutated_log_reason(e: &NetError) -> &'static str {
    let reason = match e {
        NetError::Mutated(s) | NetError::Consensus(s) => s.clone(),
        NetError::Rules(err) => rbitcoin_consensus::block_reject_reason(err),
        NetError::ConnectFailed {
            rules: Some(err), ..
        } => rbitcoin_consensus::block_reject_reason(err),
        NetError::ConnectFailed { msg, .. } => msg.clone(),
        _ => String::new(),
    };
    crate::chain::mutated_copy_log_reason(&reason)
}

/// What a peer-accept result does to the session and to the body's ask. A
/// reject keeps the ask only on the hub's verdict
/// ([`ChainHub::settle_rejected_ask`]); every other outcome releases it.
/// The neighborhood is the match in this function.
#[allow(clippy::too_many_arguments)] // one accept result; call site stays unbundled
async fn finish_block_accept(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    block: &Block,
    hash: BlockHash,
    ask: Option<BlockAsk>,
    outcome: Result<AcceptOutcome, NetError>,
) -> Result<(), NetError> {
    match outcome {
        Ok(AcceptOutcome::Accepted { .. }) => {
            drain_after_accept(hub, out_tx, follow, session, hash, ask, false).await
        }
        Ok(AcceptOutcome::AlreadyHave) | Ok(AcceptOutcome::IgnoredWeaker) => {
            drain_after_accept(hub, out_tx, follow, session, hash, ask, false).await
        }
        Err(e) if net_error_is_store_not_found(&e) => {
            rbitcoin_log::warn!("p2p: accept dropped {hash} (store not found — keep session): {e}");
            hub.settle_rejected_ask(ask, hash, &e);
            Ok(())
        }
        Err(e) if net_error_needs_parent(&e) => {
            drop(ask);
            follow.pending_blocks.insert(hash, block.clone(), None);
            drain_pending(
                hub,
                out_tx,
                &mut follow.pending_blocks,
                &mut follow.pending_headers,
                &mut follow.requested_blocks,
                getdata_use_compact(hub, follow.cmpct_version),
                session,
            )
            .await
        }
        Err(e @ NetError::Store(_)) => {
            note_store_fault_keep_session(hash, &e);
            Ok(())
        }
        // Witness bytes the block does not commit to: Core `IsBlockMutated`
        // drops these before validation, as `on_block` drops the others.
        Err(e) if e.is_mutated() => {
            let reason = accept_mutated_log_reason(&e);
            on_mutated_block(hub, out_tx, follow, session, ask, reason).await
        }
        Err(e) => {
            rbitcoin_log::warn!("p2p: accept dropped {hash} (invalid — keep session): {e}");
            hub.settle_rejected_ask(ask, hash, &e);
            Ok(())
        }
    }
}

#[doc(hidden)]
pub fn drop_pending_cmpct(
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
) {
    follow.pending_cmpct.remove(&hash);
    if let Some(s) = session {
        s.release_cmpct_taken(hash);
    }
}

/// Core `MarkBlockAsReceived` clears every peer's in-flight entry for a
/// received block. Ours are per session, so a partial whose block connected
/// through another peer is dropped here before it counts against the cap.
#[doc(hidden)]
pub fn drop_connected_pending_cmpct(
    hub: &ChainHub,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) {
    let done: Vec<BlockHash> = follow
        .pending_cmpct
        .keys()
        .filter(|h| hub.is_connected(h))
        .copied()
        .collect();
    for hash in done {
        if let (Some(s), Some(h)) = (session, hub.header_height(&hash)) {
            s.clear_block_inflight(h);
        }
        drop_pending_cmpct(follow, session, hash);
    }
}

pub async fn on_cmpctblock(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    cb: &CmpctBlock,
) -> Result<(), NetError> {
    let hsi = cb.compact_block.clone();
    let hash = hsi.header.block_hash();
    if on_cmpctblock_reject_early(hub, follow, session, &hsi, hash)? {
        return Ok(());
    }
    if hsi.header.prev_blockhash.to_byte_array() != [0u8; 32]
        && !hub.knows_header(&hsi.header.prev_blockhash)
    {
        // Keep in-session; persist only once prev is stored.
        if let Some(s) = session {
            let _ = s.take_awaiting_headers();
        }
        let _ = queue_getheaders(out_tx, hub, session, true, None);
    }
    remember_admitted(hub, follow, hash, hsi.header);
    if !any_header_path_meets_minwork_in(
        hub,
        &follow.pending_headers,
        hash,
        &follow.pending_no_base,
    ) {
        return Ok(());
    }
    let keep =
        keep_pending_connecting_paths_in(hub, &follow.pending_headers, &follow.pending_no_base);
    release_asks_off_path(&mut follow.requested_blocks, &keep);
    on_cmpctblock_queue_ancestors(hub, out_tx, follow, session, hash)?;
    if hub.header_below_anti_dos(&hsi.header) && !follow.requested_blocks.contains(&hash) {
        let id = session.map(|s| s.id).unwrap_or(0);
        rbitcoin_log::info!("p2p: ignore low-work compact block from peer {id}");
        return Ok(());
    }
    // Process the compact block's header before trying reconstruction, as Core
    // does. This also makes the announcement timestamp contingent on the
    // header actually entering the block index.
    let announcement_unknown = !header_is_stored(hub, &hash);
    let should_stamp = announcement_unknown
        && header_connects(
            hub,
            &follow.pending_headers,
            hsi.header.prev_blockhash,
            &follow.pending_no_base,
        )
        && work_of_header_path(hub, &follow.pending_headers, hash)
            .zip(hub.chain_work().ok())
            .is_some_and(|(announced, tip)| announced > tip);
    let connects = header_connects(
        hub,
        &follow.pending_headers,
        hsi.header.prev_blockhash,
        &follow.pending_no_base,
    );
    let no_base = Arc::clone(&follow.pending_no_base);
    let pending = std::mem::take(&mut follow.pending_headers);
    let (persisted, pending) = run_header_job(hub, move |hub| {
        let persisted = persist_pending_header_path_in(hub, &pending, hash, &no_base);
        (persisted, pending)
    })
    .await?;
    follow.pending_headers = pending;
    // Core `invalid header via cmpctblock`: scored like a `headers` message.
    // A path that does not connect yet only waits for its parent.
    if connects && persisted.as_ref().is_err_and(header_reject_scores) {
        follow.pending_headers.remove(&hash);
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    if should_stamp && header_is_stored(hub, &hash) {
        if let Some(s) = session {
            s.note_last_block_announcement();
        }
    }
    if hub.has_block(&hash) {
        follow.requested_blocks.remove(&hash);
        return Ok(());
    }
    if !follow.requested_blocks.contains(&hash)
        && !compact_unsolicited_reconstruct(hub, session, &hsi.header)
    {
        return on_cmpctblock_header_announce(hub, out_tx, follow, session, hash).await;
    }
    on_cmpctblock_reconstruct(hub, out_tx, follow, session, &hsi, hash).await
}

/// An unsolicited compact block we do not reconstruct is a header
/// announcement. The header path is stored and the bodies
/// [`fetchable_header_path_bodies`] returns are a full-block `getdata` to
/// this peer, as Core's block download asks once the announcement moved
/// the peer's best block. The ask follows the headers path: a spent send
/// budget asks nothing, and each body is one hub claim.
async fn on_cmpctblock_header_announce(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
) -> Result<(), NetError> {
    if session.is_some_and(|s| s.send_over_budget()) {
        return Ok(());
    }
    let mut want = fetchable_header_path_bodies_in(
        hub,
        &follow.pending_headers,
        hash,
        &follow.pending_blocks,
        follow.requested_blocks.hashes(),
        &follow.pending_no_base,
    );
    want.truncate(MAX_SERVE_BLOCKS.saturating_sub(follow.requested_blocks.len()));
    queue_claimed_block_getdata(
        hub,
        out_tx,
        session,
        &mut follow.requested_blocks,
        &want,
        false,
    )
}

#[doc(hidden)]
pub fn on_cmpctblock_reject_early(
    hub: &ChainHub,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hsi: &HeaderAndShortIds,
    hash: BlockHash,
) -> Result<bool, NetError> {
    // `-blocksonly` does not reconstruct compact blocks, solicited or not.
    // The peer still gets a full-block getdata from the headers path.
    if hub.blocks_only.load(Ordering::Acquire) {
        return Ok(true);
    }
    if follow.cmpct_version != 2 {
        rbitcoin_log::debug!(
            P2p,
            "cmpctblock from a peer that never sent sendcmpct version 2"
        );
        return Ok(true);
    }
    if session.is_some_and(|s| s.has_failed_cmpct(&hash)) {
        rbitcoin_log::info!("p2p: previous compact block reconstruction attempt failed");
        misbehaving(&mut follow.ban_score, session);
        return Ok(true);
    }
    if !crate::compact::cmpct_init_data_ok(hsi) {
        rbitcoin_log::info!("p2p: invalid compact block");
        misbehaving(&mut follow.ban_score, session);
        return Ok(true);
    }
    // Child of a cached-invalid block: disconnect. Same-hash cached invalid
    // via compact stays connected.
    if hub.is_block_invalid(&hsi.header.prev_blockhash) {
        misbehaving(&mut follow.ban_score, session);
        return Ok(true);
    }
    if hub.is_block_invalid(&hash) {
        return Ok(true);
    }
    // Claimed proof of work before any peer or header-sync state. Once the
    // tip meets minimum chain work the work-path walk does not check this header.
    if !hub.header_claimed_pow_ok(&hsi.header) {
        rbitcoin_log::info!("{}", crate::chain::accept_block_header_nodos_log(hash));
        misbehaving(&mut follow.ban_score, session);
        follow.requested_blocks.remove(&hash);
        return Ok(true);
    }
    if let Some(s) = session {
        s.note_block_from_peer(hash);
    }
    note_advanced_best_known(
        hub,
        &mut PathWorkMemo::default(),
        &follow.pending_headers,
        session,
        hash,
        &follow.pending_no_base,
    );
    Ok(false)
}

#[doc(hidden)]
pub fn on_cmpctblock_queue_ancestors(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
) -> Result<(), NetError> {
    let mut ancestors: Vec<BlockHash> = fetchable_header_path_bodies_in(
        hub,
        &follow.pending_headers,
        hash,
        &follow.pending_blocks,
        follow.requested_blocks.hashes(),
        &follow.pending_no_base,
    )
    .into_iter()
    .filter(|h| *h != hash)
    .collect();
    ancestors.truncate(MAX_SERVE_BLOCKS.saturating_sub(follow.requested_blocks.len()));
    queue_block_getdata(
        hub,
        out_tx,
        session,
        &mut follow.requested_blocks,
        &ancestors,
        getdata_use_compact(hub, follow.cmpct_version),
    )
}

pub async fn on_cmpctblock_reconstruct(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hsi: &HeaderAndShortIds,
    hash: BlockHash,
) -> Result<(), NetError> {
    // Reconstruct after the message is in hand. The permit is the same
    // budget as block decode and compact serve. Session state stays here.
    let permit = block_reconstruct_permit().await?;
    let mp = hub.mempool().cloned();
    let owned_hsi = hsi.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        reconstruct_cmpct_from_mempool(mp.as_deref(), &owned_hsi, 2)
    })
    .await
    .map_err(|_| NetError::Protocol("compact reconstruct join failed"))?;
    match outcome {
        Some(CmpctReconstruct::Block(block, fill)) => {
            on_cmpct_got_block(hub, out_tx, follow, session, hsi, hash, block, fill).await
        }
        Some(CmpctReconstruct::GetData) => on_cmpct_need_getdata(out_tx, session, hash),
        Some(CmpctReconstruct::NeedTxn(partial, fill)) => {
            on_cmpct_need_txn(hub, out_tx, follow, session, hsi, hash, partial, fill)
        }
        None => {
            log_cmpct_getdata(hash, 0);
            out_tx.send(NetworkMessage::GetData(vec![Inventory::WitnessBlock(hash)]))
        }
    }
}

#[allow(clippy::too_many_arguments)] // one compact-block fill; call site stays unbundled
pub async fn on_cmpct_got_block(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hsi: &HeaderAndShortIds,
    hash: BlockHash,
    block: Block,
    fill: Option<Box<crate::compact::CmpctFillSets>>,
) -> Result<(), NetError> {
    log_cmpct_filled(hub, hsi, &block, &[], fill.as_deref());
    let ask = follow.requested_blocks.remove(&hash);
    drop_pending_cmpct(follow, session, hash);
    relay_new_pow_valid_block(hub, &block, session).await;
    let outcome = accept_received_from_peer(hub, block.clone(), session).await;
    apply_cmpct_accept(hub, out_tx, follow, session, hsi, hash, ask, block, outcome);
    drain_pending(
        hub,
        out_tx,
        &mut follow.pending_blocks,
        &mut follow.pending_headers,
        &mut follow.requested_blocks,
        getdata_use_compact(hub, follow.cmpct_version),
        session,
    )
    .await
}

/// Compact accept outcomes for the body and its ask. A reject
/// keeps the ask only on the hub's verdict
/// ([`ChainHub::settle_rejected_ask`]); a mutated fill asks the same peer
/// again; every other outcome releases it. An unknown previous header asks
/// for headers.
#[allow(clippy::too_many_arguments)] // one compact fill; the match stays one function
fn apply_cmpct_accept(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hsi: &HeaderAndShortIds,
    hash: BlockHash,
    ask: Option<BlockAsk>,
    block: Block,
    outcome: Result<AcceptOutcome, NetError>,
) {
    match outcome {
        Err(e) if net_error_needs_parent(&e) => {
            follow.pending_blocks.insert(hash, block, None);
            maybe_select_hb_if_relay(hub, session);
        }
        Ok(_) => {}
        Err(e @ NetError::Store(_)) if !net_error_is_store_not_found(&e) => {
            note_store_fault_keep_session(hash, &e);
        }
        // Core `FillBlock` returns `READ_STATUS_FAILED` and asks the same
        // peer for the full block, without `Misbehaving`: a short-id
        // collision is not the sender's fault. A mutated full block is
        // punished in `on_block`.
        Err(e) if e.is_mutated() => {
            log_cmpct_getdata(hash, 0);
            if let Some(s) = session {
                s.note_failed_cmpct(hash);
                s.release_cmpct_taken(hash);
            }
            if let Some(ask) = ask {
                follow.requested_blocks.restore(ask);
            }
            let _ = queue_block_getdata(
                hub,
                out_tx,
                session,
                &mut follow.requested_blocks,
                &[hash],
                false,
            );
            ask_other_hb_for_block(session, hash);
        }
        Err(e) => {
            hub.settle_rejected_ask(ask, hash, &e);
            if !hub.knows_header(&hsi.header.prev_blockhash) {
                let _ = queue_getheaders(out_tx, hub, session, true, None);
            }
        }
    }
}

#[doc(hidden)]
pub fn on_cmpct_need_getdata(
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
) -> Result<(), NetError> {
    log_cmpct_getdata(hash, 0);
    if let Some(s) = session {
        s.note_failed_cmpct(hash);
    }
    out_tx.send(NetworkMessage::GetData(vec![Inventory::WitnessBlock(hash)]))
}

#[allow(clippy::too_many_arguments)] // one getblocktxn ask; call site stays unbundled
#[doc(hidden)]
pub fn on_cmpct_need_txn(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hsi: &HeaderAndShortIds,
    hash: BlockHash,
    partial: crate::compact::CmpctPartial,
    fill: Option<Box<crate::compact::CmpctFillSets>>,
) -> Result<(), NetError> {
    if follow.pending_cmpct.contains_key(&hash) {
        return Ok(());
    }
    let missing_n = partial.missing().len();
    drop_connected_pending_cmpct(hub, follow, session);
    if follow.pending_cmpct.len() >= MAX_PENDING_CMPCT {
        log_cmpct_getdata(hash, missing_n);
        return out_tx.send(NetworkMessage::GetData(vec![Inventory::WitnessBlock(hash)]));
    }
    let may_fill = session.is_none_or(|s| s.try_cmpct_fill(hash));
    if !may_fill {
        return Ok(());
    }
    let missing = partial.missing().to_vec();
    follow.pending_cmpct.insert(
        hash,
        PendingCmpct {
            hsi: hsi.clone(),
            partial,
            fill: fill.map(|b| *b),
            since: std::time::Instant::now(),
        },
    );
    if let Some(s) = session {
        let h = hub
            .query
            .height_of_hash(&hsi.header.prev_blockhash.to_byte_array())
            .ok()
            .flatten()
            .map(|ht| ht.0.saturating_add(1))
            .unwrap_or_else(|| hub.tip_height().unwrap_or(0).saturating_add(1));
        s.note_block_inflight(h);
        s.note_blocks_requested(&[hash]);
    }
    out_tx.send(NetworkMessage::GetBlockTxn(GetBlockTxn {
        txs_request: crate::compact::missing_request(hash, &missing),
    }))?;
    ask_other_hb_for_block(session, hash);
    rbitcoin_log::debug!(
        P2p,
        "cmpct reconstruct {hash} missing={missing_n} awaiting blocktxn"
    );
    Ok(())
}

pub async fn on_blocktxn(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    bt: &BlockTransactions,
) -> Result<(), NetError> {
    let hash = bt.block_hash;
    if let Some(mp) = hub.mempool() {
        mp.try_note_extra_compact_txs(bt.transactions.iter());
    }
    if session.is_some_and(|s| s.has_failed_cmpct(&hash)) {
        rbitcoin_log::info!("p2p: previous compact block reconstruction attempt failed");
        misbehaving(&mut follow.ban_score, session);
        return Ok(());
    }
    let Some(mut pc) = follow.pending_cmpct.remove(&hash) else {
        follow.ban_score = follow.ban_score.saturating_add(5);
        return Ok(());
    };
    match apply_cmpct_blocktxn(&mut pc, bt) {
        Ok((block, fill)) => {
            on_blocktxn_got_block(hub, out_tx, follow, session, &pc, hash, block, fill).await
        }
        Err(()) => on_blocktxn_apply_fail(hub, out_tx, follow, session, &pc, hash),
    }
}

#[allow(clippy::too_many_arguments)] // one blocktxn fill; call site stays unbundled
pub async fn on_blocktxn_got_block(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    pc: &PendingCmpct,
    hash: BlockHash,
    block: Block,
    fill: Option<crate::compact::CmpctFillSets>,
) -> Result<(), NetError> {
    log_cmpct_filled(hub, &pc.hsi, &block, pc.partial.missing(), fill.as_ref());
    relay_new_pow_valid_block(hub, &block, session).await;
    match accept_received_from_peer(hub, block.clone(), session).await {
        Ok(AcceptOutcome::Accepted { .. }) => {
            follow.requested_blocks.remove(&hash);
            if let Some(s) = session {
                if let Some(h) = hub.tip_height() {
                    s.clear_block_inflight(h);
                }
                if let Some(ph) = s.peer_hub() {
                    ph.clear_cmpct_fill(hash);
                }
            }
            drain_pending(
                hub,
                out_tx,
                &mut follow.pending_blocks,
                &mut follow.pending_headers,
                &mut follow.requested_blocks,
                getdata_use_compact(hub, follow.cmpct_version),
                session,
            )
            .await
        }
        Ok(_) => {
            follow.requested_blocks.remove(&hash);
            if let Some(s) = session {
                s.release_cmpct_taken(hash);
            }
            drain_pending(
                hub,
                out_tx,
                &mut follow.pending_blocks,
                &mut follow.pending_headers,
                &mut follow.requested_blocks,
                getdata_use_compact(hub, follow.cmpct_version),
                session,
            )
            .await
        }
        Err(e) if net_error_needs_parent(&e) => {
            follow.requested_blocks.remove(&hash);
            follow.pending_blocks.insert(hash, block, None);
            maybe_select_hb_if_relay(hub, session);
            if let Some(s) = session {
                s.release_cmpct_taken(hash);
            }
            drain_pending(
                hub,
                out_tx,
                &mut follow.pending_blocks,
                &mut follow.pending_headers,
                &mut follow.requested_blocks,
                getdata_use_compact(hub, follow.cmpct_version),
                session,
            )
            .await
        }
        Err(e) => {
            // Core asks no peer again for a body it holds invalid.
            if hub.settle_rejected_ask(follow.requested_blocks.remove(&hash), hash, &e) {
                if let Some(s) = session {
                    s.release_cmpct_taken(hash);
                }
                return Ok(());
            }
            on_blocktxn_unconnectable(hub, out_tx, follow, session, hash)
        }
    }
}

#[doc(hidden)]
pub fn on_blocktxn_apply_fail(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    pc: &PendingCmpct,
    hash: BlockHash,
) -> Result<(), NetError> {
    rbitcoin_log::info!("p2p: previous compact block reconstruction attempt failed");
    log_cmpct_getdata(hash, pc.partial.missing().len());
    if let Some(s) = session {
        s.note_failed_cmpct(hash);
        s.release_cmpct_taken(hash);
    }
    follow.ban_score = follow.ban_score.saturating_add(10);
    ask_other_hb_for_block(session, hash);
    queue_block_getdata(
        hub,
        out_tx,
        session,
        &mut follow.requested_blocks,
        &[hash],
        false,
    )
}

#[doc(hidden)]
pub fn on_blocktxn_unconnectable(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
) -> Result<(), NetError> {
    rbitcoin_log::info!("p2p: previous compact block reconstruction attempt failed");
    if let Some(s) = session {
        s.note_failed_cmpct(hash);
        s.release_cmpct_taken(hash);
    }
    follow.ban_score = follow.ban_score.saturating_add(10);
    ask_other_hb_for_block(session, hash);
    queue_block_getdata(
        hub,
        out_tx,
        session,
        &mut follow.requested_blocks,
        &[hash],
        false,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum TxAcceptLog<'a> {
    Silent,
    Park(&'a BTreeSet<bitcoin::Txid>),
    ParentFetch(&'a BTreeSet<bitcoin::Txid>),
    Reject,
}

/// Invalid-script announcements are misbehavior. Policy rejects are not.
pub fn tx_reject_ban_score(e: &rbitcoin_mempool::AcceptError) -> u32 {
    match e {
        rbitcoin_mempool::AcceptError::Script(_) => 10,
        _ => 0,
    }
}

#[doc(hidden)]
pub fn tx_accept_log(e: &rbitcoin_mempool::AcceptError) -> TxAcceptLog<'_> {
    match e {
        rbitcoin_mempool::AcceptError::Duplicate(_) => TxAcceptLog::Silent,
        rbitcoin_mempool::AcceptError::Orphaned {
            missing,
            fresh: true,
            ..
        } => TxAcceptLog::Park(missing),
        rbitcoin_mempool::AcceptError::Orphaned {
            missing,
            fresh: false,
            ..
        } => TxAcceptLog::ParentFetch(missing),
        _ => TxAcceptLog::Reject,
    }
}

#[doc(hidden)]
pub fn schedule_orphan_parent_getdata(
    mp: &crate::tx_relay::MempoolHub,
    tx: &Transaction,
    session: Option<&crate::peers::LivePeer>,
    extra_from: &[u64],
) {
    let Some(s) = session else {
        return;
    };
    let missing = mp.orphan_getdata_parents(tx);
    let now = s.clock_now();
    mp.schedule_orphan_parents(&missing, s.id, tx_download_preferred(s), now);
    let Some(ph) = s.peer_hub() else {
        return;
    };
    for id in extra_from {
        if *id == s.id {
            continue;
        }
        if let Some(p) = ph.live_peers().into_iter().find(|p| p.id == *id) {
            mp.schedule_announcer_parents(&missing, p.id, tx_download_preferred(&p), now);
        }
    }
}

#[doc(hidden)]
pub fn queue_due_parent_getdata(
    hub: &ChainHub,
    session: &crate::peers::LivePeer,
    out_tx: &PeerOutTx,
) {
    let Some(mp) = hub.mempool() else {
        return;
    };
    let want = mp.take_due_parent_getdata(session.id, session.clock_now());
    if want.is_empty() {
        return;
    }
    mp.note_getdata_tx(want.len() as u64);
    let inv = want
        .into_iter()
        .map(|due| {
            if due.wtxid {
                Inventory::WTx(bitcoin::Wtxid::from_byte_array(due.hash))
            } else {
                Inventory::WitnessTransaction(bitcoin::Txid::from_byte_array(due.hash))
            }
        })
        .collect();
    let _ = out_tx.send(NetworkMessage::GetData(inv));
}

/// Flush walks every writer with an empty from-this-peer set. The sender
/// already has the tx, so that INV must not come back.
fn note_blocksonly_sender(
    mp: &crate::tx_relay::MempoolHub,
    session: Option<&crate::peers::LivePeer>,
    txid: bitcoin::Txid,
    wtxid: bitcoin::Wtxid,
) {
    if mp.relay_enabled() {
        return;
    }
    if let Some(s) = session {
        s.note_announced_tx(txid, wtxid);
    }
}

/// The other announcers of a parked `tx` become its announcers. With a
/// `session`, its missing parents are asked for from it and from the
/// announcers still live.
fn fetch_parked_parents(
    mp: &crate::tx_relay::MempoolHub,
    tx: &Transaction,
    wtxid: &bitcoin::Wtxid,
    session: Option<&crate::peers::LivePeer>,
    extra_from: &[u64],
) {
    for p in extra_from {
        let _ = mp.add_orphan_announcer(wtxid, *p);
    }
    if mp.try_orphan_missing(wtxid).is_some() {
        schedule_orphan_parent_getdata(mp, tx, session, extra_from);
    }
}

/// Core `AlreadyHaveTx` for a received tx, which is dropped unvalidated: a
/// recent reject, or a parked wtxid whose sender is no new announcer. Unlike
/// Core, a mempool hit is not had here; it goes on to the duplicate handling
/// in [`on_tx`].
fn already_had_unvalidated(
    mp: &crate::tx_relay::MempoolHub,
    txid: &bitcoin::Txid,
    wtxid: &bitcoin::Wtxid,
) -> bool {
    mp.try_recent_reject(wtxid)
        || (mp.try_orphan_missing(wtxid).is_some() && !mp.try_contains(txid))
}

pub async fn on_tx(
    hub: &ChainHub,
    _out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    tx: &Transaction,
) -> Result<(), NetError> {
    rbitcoin_log::trace!(Txrelay, "{}", received_tx_log());
    if hub.in_ibd() {
        return Ok(());
    }
    if reject_unsolicited_tx(hub, session) {
        let id = session.map(|s| s.id).unwrap_or(0);
        rbitcoin_log::info!(
            "p2p: transaction sent in violation of protocol, disconnecting peer={id}"
        );
        punish_disconnect(&mut follow.ban_score, session);
        return Ok(());
    }
    if let Some(mp) = hub.mempool() {
        if mp.relay_enabled() || session.is_some_and(|s| s.session_relay_perm()) {
            let txid = tx.compute_txid();
            let wtxid = tx.compute_wtxid();
            follow.from_this_peer.insert(txid, FROM_THIS_PEER_CAP);
            let from = session.map(|s| s.id);
            let trusted = session
                .is_some_and(|s| s.peer_hub().is_some_and(|ph| ph.session_is_trusted_peer(s)));
            // The trusted peer already accepted the tx, so judge it again.
            if !trusted && already_had_unvalidated(mp, &txid, &wtxid) {
                mp.resolve_tx_request(&txid, &wtxid, false);
                maybe_force_relay_recent_reject(hub, session, mp, txid, wtxid);
                return Ok(());
            }
            if trusted {
                mp.set_trusted_session(from);
            }
            note_blocksonly_sender(mp, session, txid, wtxid);
            let mp_task = Arc::clone(mp);
            let tx_task = tx.clone();
            let peers = session.and_then(|s| s.peer_hub());
            let hub_arc = hub.shared_arc();
            // The blocksonly note shares the admit task. Aborting this
            // session during script verify must not skip the rebroadcast.
            let admitted = crate::reactor::finish_join(
                crate::reactor::spawn_with_store_budget(async move {
                    let result = mp_task
                        .accept_tx_from_work(tx_task, crate::tx_relay::TxOrigin::Relay(from), None)
                        .await;
                    if let Ok(r) = &result {
                        if !mp_task.relay_enabled() {
                            mp_task.note_unbroadcast(r.txid);
                            mp_task.notify_inv_flush();
                            if let (Some(h), Some(ph)) = (hub_arc.as_ref(), peers.as_ref()) {
                                flush_tx_invs(h, ph.as_ref());
                            }
                        }
                    }
                    result
                })
                .await,
                "tx admit",
            );
            match admitted {
                Ok(_r) => {
                    mp.resolve_tx_request(&txid, &wtxid, true);
                    if let Some(s) = session {
                        s.note_last_transaction();
                    }
                    // A hub that is not `into_arc` has no detached handle.
                    // Flush here so the other inbound still sees the INV.
                    // An `into_arc` hub already flushed inside the admit task.
                    if !mp.relay_enabled() && hub.shared_arc().is_none() {
                        if let Some(s) = session {
                            if let Some(ph) = s.peer_hub() {
                                flush_tx_invs(hub, ph.as_ref());
                            }
                        }
                    }
                }
                Err(e) => {
                    let extra_from = matches!(
                        tx_accept_log(&e),
                        TxAcceptLog::Park(_) | TxAcceptLog::ParentFetch(_)
                    )
                    .then(|| mp.announcer_peers_for(&txid, &wtxid))
                    .unwrap_or_default();
                    mp.resolve_tx_request(&txid, &wtxid, false);
                    match tx_accept_log(&e) {
                        TxAcceptLog::Silent => {
                            if let rbitcoin_mempool::AcceptError::Duplicate(tid) = &e {
                                if mp.try_contains_wtxid(&wtxid) {
                                    mp.note_returned(tid);
                                }
                                maybe_force_relay_duplicate(hub, session, tx, *tid);
                            }
                        }
                        TxAcceptLog::Park(_missing) => {
                            rbitcoin_log::debug!(Txrelay, "park {txid} missingorspent");
                            fetch_parked_parents(mp, tx, &wtxid, session, &extra_from);
                        }
                        TxAcceptLog::ParentFetch(_missing) => {
                            fetch_parked_parents(mp, tx, &wtxid, session, &extra_from);
                        }
                        TxAcceptLog::Reject => {
                            follow.ban_score =
                                follow.ban_score.saturating_add(tx_reject_ban_score(&e));
                            let id = session.map(|s| s.id).unwrap_or(0);
                            rbitcoin_log::info!(
                                "txrelay: reject {txid} (wtxid={wtxid}) from peer={id} was not accepted: {}",
                                e.mempool_reject_reason()
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn on_bloom_forbidden(
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    punish_disconnect(&mut follow.ban_score, session);
    Ok(())
}

#[doc(hidden)]
pub fn on_getaddr(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    if let Some(s) = session {
        if !s.take_getaddr() {
            return Ok(());
        }
        let _ = maybe_queue_local_addr(hub, s, out_tx);
    }
    let bind = session
        .map(|s| s.addrbind)
        .unwrap_or_else(|| std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
    let v2 = session.is_some_and(|s| s.wants_addrv2());
    let addrs = match session.and_then(|s| s.peer_hub()) {
        Some(ph) => ph.addr_response_net(bind, v2),
        None => Vec::new(),
    };
    queue_addr_list(session, out_tx, addrs, v2)?;
    Ok(())
}

pub async fn drain_after_accept(
    hub: &ChainHub,
    out_tx: &PeerOutTx,
    follow: &mut PeerFollowState,
    session: Option<&crate::peers::LivePeer>,
    hash: BlockHash,
    ask: Option<BlockAsk>,
    select_hb: bool,
) -> Result<(), NetError> {
    follow.pending_blocks.remove(&hash);
    follow.pending_headers.remove(&hash);
    forget_no_base(follow, &hash);
    drop(ask);
    if select_hb {
        maybe_select_hb_if_relay(hub, session);
    }
    drain_pending(
        hub,
        out_tx,
        &mut follow.pending_blocks,
        &mut follow.pending_headers,
        &mut follow.requested_blocks,
        getdata_use_compact(hub, follow.cmpct_version),
        session,
    )
    .await
}

#[derive(Debug)]
#[doc(hidden)]
pub enum TipAnnounce {
    Headers(Vec<bitcoin::block::Header>),
    Inv(BlockHash),
    Skip,
}

#[derive(Debug)]
pub enum TipRecvAnnounce {
    Announce(crate::chain::TipEvent),
    Skip,
    Closed,
}

#[doc(hidden)]
pub fn current_tip_announce(hub: &ChainHub) -> TipRecvAnnounce {
    match (hub.tip_height(), hub.tip_hash(), hub.tip_header()) {
        (Some(height), Some(hash), Some(header)) => {
            TipRecvAnnounce::Announce(crate::chain::TipEvent {
                height,
                hash,
                header,
                reorg_branch_len: 0,
            })
        }
        _ => TipRecvAnnounce::Skip,
    }
}

/// True when `tip` is not the tip we last offered this peer, and not a
/// descendant of it. An inv counts. The header cursor counts only when no
/// tip has been offered yet. Those announcements wait [`TIP_ANNOUNCE_BATCH`]
/// so a reorg is one message.
#[doc(hidden)]
pub fn tip_is_reorg_vs_sent(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    tip: &Result<crate::chain::TipEvent, broadcast::error::RecvError>,
) -> bool {
    if matches!(tip, Err(broadcast::error::RecvError::Closed)) {
        return false;
    }
    let Some(mark) = session.and_then(|s| s.last_tip_offered().or(s.header_marks().0)) else {
        return false;
    };
    let hash = match tip {
        Ok(ev) => ev.hash,
        Err(_) => match hub.tip_hash() {
            Some(h) => h,
            None => return false,
        },
    };
    hash != mark && !hub.is_header_ancestor(mark, hash)
}

/// Forget tip events already queued. Catch-up offers the current tip once.
fn drain_queued_tips(tip_rx: &mut broadcast::Receiver<crate::chain::TipEvent>) {
    while let Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) = tip_rx.try_recv() {}
}

/// Map a `tip_rx.recv()` result to the tip we should announce.
///
/// `Lagged` and a queued `TipEvent` whose hash is no longer `hub.tip` both
/// announce the **current** hub tip so a generate burst does not enqueue one
/// headers/cmpct per height ahead of GetData bodies.
pub fn tip_event_for_announce(
    recv: Result<crate::chain::TipEvent, broadcast::error::RecvError>,
    hub: &ChainHub,
) -> TipRecvAnnounce {
    match recv {
        Ok(ev) if hub.tip_hash() == Some(ev.hash) => TipRecvAnnounce::Announce(ev),
        Ok(_) => current_tip_announce(hub),
        Err(broadcast::error::RecvError::Closed) => TipRecvAnnounce::Closed,
        Err(broadcast::error::RecvError::Lagged(_)) => current_tip_announce(hub),
    }
}

/// `hash` is a strict ancestor of a header this peer already has.
/// An equal mark still needs a block inv.
fn peer_already_walked_past(
    hub: &ChainHub,
    sent: Option<BlockHash>,
    known: Option<BlockHash>,
    hash: BlockHash,
) -> bool {
    [sent, known]
        .into_iter()
        .flatten()
        .any(|mark| mark != hash && hub.is_header_ancestor(hash, mark))
}

#[doc(hidden)]
pub fn peer_has_header(
    hub: &ChainHub,
    sent: Option<BlockHash>,
    known: Option<BlockHash>,
    hash: BlockHash,
) -> bool {
    if hash.to_byte_array() == [0u8; 32] {
        return true;
    }
    for mark in [sent, known].into_iter().flatten() {
        if mark == hash || hub.is_header_ancestor(hash, mark) {
            return true;
        }
    }
    false
}

/// BIP152 compact tip announcement from an in-RAM body.
#[doc(hidden)]
pub fn cmpct_announce_from_block(
    hub: &ChainHub,
    block: &Block,
    cmpct_version: u32,
) -> Option<NetworkMessage> {
    crate::reactor::assert_not_reactor("cmpct announce");
    let ver = cmpct_version.clamp(1, 2);
    let pref = hub
        .cmpct_prefill_indexes(&block.block_hash())
        .unwrap_or_else(|| vec![0]);
    let ids = crate::compact::short_id_hashes(block, ver);
    cmpct_from_parts(block, &ids, rand_nonce(), ver, &pref)
}

/// Short ids for a block already in hand. No store read. `ids` is
/// [`crate::compact::short_id_hashes`] for `ver`.
fn cmpct_from_parts(
    block: &Block,
    ids: &[[u8; 32]],
    nonce: u64,
    ver: u32,
    pref: &[usize],
) -> Option<NetworkMessage> {
    crate::reactor::assert_not_reactor("cmpct short ids");
    let hsi = crate::compact::header_and_short_ids(block, ids, nonce, ver, pref)
        .or_else(|| crate::compact::header_and_short_ids(block, ids, nonce, ver, &[0]))?;
    rbitcoin_log::info!(
        "{}",
        crate::compact::cmpct_send_line(block.block_hash(), block.txdata.len(), &hsi)
    );
    Some(NetworkMessage::CmpctBlock(CmpctBlock {
        compact_block: hsi,
    }))
}

async fn cmpct_from_parts_cpu(
    block: Block,
    nonce: u64,
    ver: u32,
    pref: Vec<usize>,
) -> Result<Option<NetworkMessage>, NetError> {
    match rbitcoin_consensus::script_pool::run_cpu(move || {
        let ids = crate::compact::short_id_hashes(&block, ver);
        cmpct_from_parts(&block, &ids, nonce, ver, &pref)
    })
    .await
    {
        Some(msg) => Ok(msg),
        None => Err(NetError::Protocol("cmpct relay join failed")),
    }
}

/// BIP152 compact tip announcement (coinbase prefilled). `None` if the body
/// is not in cache/store yet.
#[doc(hidden)]
pub fn cmpct_announce_msg(
    hub: &ChainHub,
    hash: &BlockHash,
    cmpct_version: u32,
) -> Option<NetworkMessage> {
    let block = block_for_peer(hub.cache.as_ref(), hub.query.as_ref(), hash).ok()??;
    cmpct_announce_from_block(hub, &block, cmpct_version)
}

/// A header from the current peer. While the mode is on that peer is the trusted one.
/// Flag-off does not write the waive cell. Runs on the same thread as the header store.
fn ensure_peer_header(hub: &ChainHub, header: &bitcoin::block::Header) -> Result<(), NetError> {
    rbitcoin_query::confirm_run::waive_batch(hub.query.trust_peer(), || hub.ensure_header(header))
}

/// Tip `cmpctblock`. The body load stays on the blocking pool. Short ids
/// run on `rbtc-scripts-*` once that body is in hand. A missing body is
/// `None` and the caller sends headers.
async fn cmpct_tip_announce(
    hub: &ChainHub,
    hash: &BlockHash,
    cmpct_version: u32,
) -> Result<Option<NetworkMessage>, NetError> {
    let Some(owned) = hub.shared_arc() else {
        return Ok(cmpct_announce_msg(hub, hash, cmpct_version));
    };
    let permit = block_reconstruct_permit().await?;
    let hash = *hash;
    let ver = cmpct_version.clamp(1, 2);
    let loaded = tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        let block = block_for_peer(owned.cache.as_ref(), owned.query.as_ref(), &hash).ok()??;
        let pref = owned
            .cmpct_prefill_indexes(&block.block_hash())
            .unwrap_or_else(|| vec![0]);
        Some((block, pref, rand_nonce()))
    })
    .await
    .map_err(|_| NetError::Protocol("cmpct announce join failed"))?;
    let Some((block, pref, nonce)) = loaded else {
        return Ok(None);
    };
    cmpct_from_parts_cpu(block, nonce, ver, pref).await
}

/// Header persist and prefill for a block already in hand, then the body
/// check and short ids.
///
/// An `Arc` hub persists the header on the blocking pool and hashes the
/// body on `rbtc-scripts-*`. A stack hub (tests) does both inline.
/// `Ok(None)` is a header the store refused, a mutated body, or a body that
/// did not become a compact message. A closed permit or a failed join is
/// `Err`, the same class as [`cmpct_tip_announce`].
async fn cmpct_announce_off_session(
    hub: &ChainHub,
    block: Block,
    height: u32,
) -> Result<Option<NetworkMessage>, NetError> {
    let Some(owned) = hub.shared_arc() else {
        return Ok(hb_relay_message(hub, &block, height));
    };
    let cpu_hub = Arc::clone(&owned);
    let permit = block_reconstruct_permit().await?;
    let joined = tokio::task::spawn_blocking(move || {
        let _region = crate::reactor::BlockingRegion::enter();
        let _permit = permit;
        hb_relay_prepare(owned.as_ref(), block)
    })
    .await;
    let Some((block, pref, nonce)) = map_cmpct_relay_join(joined)? else {
        return Ok(None);
    };
    rbitcoin_consensus::script_pool::run_cpu(move || {
        hb_relay_cmpct(cpu_hub.as_ref(), &block, height, nonce, &pref)
    })
    .await
    .ok_or(NetError::Protocol("cmpct relay join failed"))
}

fn hb_relay_prepare(hub: &ChainHub, block: Block) -> Option<(Block, Vec<usize>, u64)> {
    if ensure_peer_header(hub, &block.header).is_err() {
        return None;
    }
    hub.remember_cmpct_prefill_from_block(&block);
    let pref = hub
        .cmpct_prefill_indexes(&block.block_hash())
        .unwrap_or_else(|| vec![0]);
    Some((block, pref, rand_nonce()))
}

fn hb_relay_message(hub: &ChainHub, block: &Block, height: u32) -> Option<NetworkMessage> {
    let (block, pref, nonce) = hb_relay_prepare(hub, block.clone())?;
    hb_relay_cmpct(hub, &block, height, nonce, &pref)
}

/// Core sends this from `NewPoWValidBlock`, after `CheckBlock` and
/// `ContextualCheckBlock`. Callers check the txid merkle root and the
/// 64-byte mutation where the body arrives. The witness commitment is not
/// in the block hash, so a peer can change a witness or the coinbase nonce
/// and keep the header. That check (Core `IsBlockMutated` with the witness
/// root, which reads the coinbase) runs here on the wtxids the short ids
/// use, so the check does not hash the transactions again. With compact
/// prefill on, `hb_relay_prepare` still hashes them for the prefill plan.
fn hb_relay_cmpct(
    hub: &ChainHub,
    block: &Block,
    height: u32,
    nonce: u64,
    pref: &[usize],
) -> Option<NetworkMessage> {
    if !block.txdata.first().is_some_and(Transaction::is_coinbase) {
        return None;
    }
    let wtxids = crate::compact::short_id_hashes(block, 2);
    let ctx = rbitcoin_consensus::ValidationContext::at(&hub.params, Height(height), hub.milestone);
    if let Err(e) = rbitcoin_consensus::block::reject_witness_malleation_wtxids(
        block,
        &ctx,
        rbitcoin_consensus::block::block_has_witness(block),
        wtxids.iter().skip(1).copied(),
    ) {
        rbitcoin_log::debug!(
            P2p,
            "no compact relay of mutated block {}: {}",
            block.block_hash(),
            rbitcoin_consensus::block_reject_reason(&e)
        );
        return None;
    }
    cmpct_from_parts(block, &wtxids, nonce, 2, pref)
}

/// A panicked or cancelled blocking job is not "no compact message".
#[doc(hidden)]
pub fn map_cmpct_relay_join<T>(joined: Result<T, tokio::task::JoinError>) -> Result<T, NetError> {
    joined.map_err(|_| NetError::Protocol("cmpct relay join failed"))
}

fn hb_peer_offered_block(
    hub: &ChainHub,
    peer: &crate::peers::LivePeer,
    from_id: Option<u64>,
    hash: BlockHash,
    prev: BlockHash,
) -> bool {
    if from_id == Some(peer.id) {
        return false;
    }
    if !peer.hb_from.load(Ordering::Relaxed) {
        return false;
    }
    if peer.conn_type == crate::peers::PeerConnType::BlockRelay {
        return false;
    }
    let (sent, known) = peer.header_marks();
    !peer_has_header(hub, sent, known, hash) && peer_has_header(hub, sent, known, prev)
}

/// Send `cmpctblock` to peers who asked for high-bandwidth announcements
/// (`hb_from`: they sent `sendcmpct` announce=1) as soon as a reconstructed
/// or received body has a PoW-valid header that extends our tip and the
/// body matches its witness commitment, **before** `tip-accept` connect.
/// The body is staged for `getblocktxn` before that message is queued.
/// Does not mark the block connected.
///
/// The compact message is built only when one of those peers would be
/// offered the block. No such peer returns before the clone and the
/// blocking job.
///
/// Core `NewPoWValidBlock`: the peer already has the parent header and not
/// this one. `hb_to` is the opposite direction (we asked them to announce
/// to us). Only the current tip-child (not a reorg branch), and only once
/// segwit is active at its height. Sender is skipped.
#[doc(hidden)]
pub async fn relay_new_pow_valid_block(
    hub: &ChainHub,
    block: &Block,
    from: Option<&crate::peers::LivePeer>,
) {
    if !hub.meets_minimum_chain_work() {
        return;
    }
    if !compact_blocks_ok(hub) {
        return;
    }
    let hash = block.block_hash();
    if hub.has_block(&hash) {
        return;
    }
    let Some(tip) = hub.tip_hash() else {
        return;
    };
    if block.header.prev_blockhash != tip {
        return;
    }
    let Some(ph) = from.and_then(|s| s.peer_hub()) else {
        return;
    };
    let from_id = from.map(|s| s.id);
    let prev = block.header.prev_blockhash;
    let targets: Vec<_> = ph
        .live_peers()
        .into_iter()
        .filter(|s| hb_peer_offered_block(hub, s, from_id, hash, prev))
        .collect();
    if targets.is_empty() {
        return;
    }
    let Some(height) = hub.header_height(&prev).map(|h| h.saturating_add(1)) else {
        return;
    };
    if !hub.params.segwit_active_at(height) {
        return;
    }
    let msg = match cmpct_announce_off_session(hub, block.clone(), height).await {
        Ok(Some(msg)) => msg,
        Ok(None) => return,
        Err(e) => {
            rbitcoin_log::warn!("p2p: high-bandwidth compact relay skipped: {e}");
            return;
        }
    };
    // The peer can ask for the missing transactions as soon as this message
    // leaves. Connect has not written the body yet.
    hub.cache.stage_servable(block.clone());
    for s in targets {
        let Some(out) = s.writer() else {
            continue;
        };
        if out.send(msg.clone()).is_ok() {
            s.note_best_header_sent(hash);
            s.note_last_tip_offered(hash);
        }
    }
}

#[doc(hidden)]
pub fn tip_announce_decision(
    hub: &ChainHub,
    ev: &crate::chain::TipEvent,
    wants_headers: bool,
    best_header_sent: Option<BlockHash>,
    best_known: Option<BlockHash>,
    from_this_peer: bool,
) -> TipAnnounce {
    if from_this_peer {
        return TipAnnounce::Skip;
    }
    if ev.reorg_branch_len > MAX_BLOCKS_TO_ANNOUNCE {
        if hub.tip_hash() == Some(ev.hash)
            && !peer_already_walked_past(hub, best_header_sent, best_known, ev.hash)
        {
            return TipAnnounce::Inv(ev.hash);
        }
        return TipAnnounce::Skip;
    }
    if !wants_headers {
        if peer_already_walked_past(hub, best_header_sent, best_known, ev.hash) {
            return TipAnnounce::Skip;
        }
        return TipAnnounce::Inv(ev.hash);
    }
    if peer_has_header(hub, best_header_sent, best_known, ev.hash) {
        return TipAnnounce::Skip;
    }
    let mut out = vec![ev.header];
    let mut prev = ev.header.prev_blockhash;
    if peer_has_header(hub, best_header_sent, best_known, prev) {
        return TipAnnounce::Headers(out);
    }
    for _ in 1..MAX_BLOCKS_TO_ANNOUNCE {
        let Some(hdr) = hub.header_of(&prev) else {
            return TipAnnounce::Inv(ev.hash);
        };
        out.push(hdr);
        prev = hdr.prev_blockhash;
        if peer_has_header(hub, best_header_sent, best_known, prev) {
            out.reverse();
            return TipAnnounce::Headers(out);
        }
    }
    TipAnnounce::Inv(ev.hash)
}

#[doc(hidden)]
pub fn is_genesis_hash(h: &BlockHash) -> bool {
    h.to_byte_array() == [0u8; 32]
}

/// `true` when this batch's pending work is under `-minimumchainwork`.
///
/// The sum uses the pending headers and, when they sit on our tip or another
/// indexed block, that block's cached chain work. It does not walk parent
/// headers in the store. `None` means the walk never reached an indexed
/// block. While our chain is under the floor, that is still the ignore path.
/// A marked hash whose parent is still unknown ends the walk.
fn headers_below_floor(
    hub: &ChainHub,
    pending: &mut HashMap<BlockHash, bitcoin::block::Header>,
    headers: &[bitcoin::block::Header],
    no_base: &NoBaseMap,
) -> bool {
    let Some(min) = hub.min_chain_work_floor() else {
        return false;
    };
    if hub.meets_minimum_chain_work() {
        return false;
    }
    let Some(last) = headers.last().map(|h| h.block_hash()) else {
        return false;
    };
    match with_batch(pending, headers, |p| {
        pending_path_work_in(hub, p, last, no_base)
    }) {
        Some(work) => work.to_be_bytes() < min,
        None => true,
    }
}

fn pending_path_work_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    no_base: &NoBaseMap,
) -> Option<bitcoin::Work> {
    PathWorkMemo::default().pending_path_work_stopping(hub, pending, tip, no_base)
}

/// One hash on a work walk toward our chain.
enum WorkStep {
    /// The walk ends here with this much chain work.
    Base(bitcoin::Work),
    Header(bitcoin::block::Header),
    Dead,
}

/// Work of header paths, memoised for one message or one pass over the
/// pending map. Each hash on a shared path is scored once, so a message pays
/// for its items plus the parked headers, not their product. It does not
/// see a pending-map change made after a walk: build a new one after the
/// map changes.
///
/// Each map holds at most one entry per parked header, which the pending
/// cap bounds. A stored header is outside that cap. `header_path` and
/// `fails_pow` each keep at most `stored_cap` of them and walk the rest
/// again. They do not share that count: the failure walk must not spend
/// the slots the work walk caches.
#[doc(hidden)]
pub struct PathWorkMemo {
    pending_path: HashMap<BlockHash, Option<(bitcoin::Work, usize)>>,
    header_path: HashMap<BlockHash, Option<(bitcoin::Work, usize)>>,
    fails_pow: HashMap<BlockHash, bool>,
    header_stored: usize,
    fails_stored: usize,
    stored_cap: usize,
}

impl Default for PathWorkMemo {
    fn default() -> Self {
        Self::with_stored_cap(PENDING_WALK_LIMIT)
    }
}

/// Whether a walked hash is memoised: a parked header always, a stored one
/// while the memo is under its cap.
fn memo_keeps(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    stored_kept: &mut usize,
    stored_cap: usize,
    hash: &BlockHash,
) -> bool {
    if pending.contains_key(hash) {
        return true;
    }
    if *stored_kept >= stored_cap {
        return false;
    }
    *stored_kept += 1;
    true
}

impl PathWorkMemo {
    pub fn with_stored_cap(stored_cap: usize) -> Self {
        Self {
            pending_path: HashMap::new(),
            header_path: HashMap::new(),
            fails_pow: HashMap::new(),
            header_stored: 0,
            fails_stored: 0,
            stored_cap,
        }
    }

    /// Entries held over the three maps.
    pub fn entries(&self) -> usize {
        self.pending_path.len() + self.header_path.len() + self.fails_pow.len()
    }

    /// Pending headers plus our tip or another indexed block's cached chain
    /// work. It does not walk parent headers in the store. `None` means the
    /// walk never reached an indexed block. A hash in `no_base` whose unknown
    /// parent is still unknown ends the walk.
    fn pending_path_work_stopping(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
        no_base: &NoBaseMap,
    ) -> Option<bitcoin::Work> {
        let our_tip = hub.tip_hash();
        memo_path_work(
            &mut self.pending_path,
            hub,
            tip,
            |h| {
                if no_base_depth(hub, no_base, &h).is_some() {
                    return WorkStep::Dead;
                }
                if is_genesis_hash(&h) {
                    return WorkStep::Base(bitcoin::Work::from_be_bytes([0u8; 32]));
                }
                if our_tip == Some(h) {
                    return hub
                        .cached_chain_work()
                        .map_or(WorkStep::Dead, WorkStep::Base);
                }
                if let Some(hdr) = pending.get(&h) {
                    return WorkStep::Header(*hdr);
                }
                hub.query
                    .published_height_of_hash(&h.to_byte_array())
                    .and_then(|height| hub.work_through_height(height.0).ok())
                    .map_or(WorkStep::Dead, WorkStep::Base)
            },
            |_| true,
        )
    }

    /// Pending or stored headers down to a connected block.
    pub fn work_of_header_path(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
    ) -> Option<bitcoin::Work> {
        self.work_of_header_path_in(hub, pending, tip, empty_no_base())
    }

    /// [`Self::work_of_header_path`]. A live no-base mark ends the walk.
    fn work_of_header_path_in(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
        no_base: &NoBaseMap,
    ) -> Option<bitcoin::Work> {
        let Self {
            header_path,
            header_stored,
            stored_cap,
            ..
        } = self;
        let step = |h: BlockHash| {
            if no_base_depth(hub, no_base, &h).is_some() {
                return WorkStep::Dead;
            }
            if hub.is_connected(&h) {
                return hub
                    .query
                    .published_height_of_hash(&h.to_byte_array())
                    .and_then(|height| hub.work_through_height(height.0).ok())
                    .map_or(WorkStep::Dead, WorkStep::Base);
            }
            let Some(hdr) = pending.get(&h).copied().or_else(|| hub.header_of(&h)) else {
                return WorkStep::Dead;
            };
            if hdr.prev_blockhash != BlockHash::all_zeros() {
                return WorkStep::Header(hdr);
            }
            if hub.header_claimed_pow_ok(&hdr) {
                WorkStep::Base(hdr.work())
            } else {
                WorkStep::Dead
            }
        };
        memo_path_work(header_path, hub, tip, step, |h| {
            memo_keeps(pending, header_stored, *stored_cap, h)
        })
    }

    /// Core: do not download/connect a peer's chain until its best-known work
    /// meets `-minimumchainwork` (`feature_minchainwork.py`).
    pub fn header_path_meets_minwork(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
    ) -> bool {
        self.header_path_meets_minwork_in(hub, pending, tip, empty_no_base())
    }

    fn header_path_meets_minwork_in(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
        no_base: &NoBaseMap,
    ) -> bool {
        let Some(min) = hub.min_chain_work_floor() else {
            return true;
        };
        if hub.meets_minimum_chain_work() {
            return true;
        }
        let Some(work) = self.work_of_header_path_in(hub, pending, tip, no_base) else {
            return false;
        };
        work.to_be_bytes() >= min
    }

    /// `tip` joins our chain and meets `-minimumchainwork`.
    fn path_connects(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
    ) -> bool {
        self.path_connects_in(hub, pending, tip, empty_no_base())
    }

    fn path_connects_in(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
        no_base: &NoBaseMap,
    ) -> bool {
        self.header_path_meets_minwork_in(hub, pending, tip, no_base)
            && self
                .work_of_header_path_in(hub, pending, tip, no_base)
                .is_some()
    }

    /// A header on `tip`'s path fails its claimed proof of work before the
    /// path reaches a connected block or a header we do not have.
    #[doc(hidden)]
    pub fn path_fails_claimed_pow(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
    ) -> bool {
        let mut path = Vec::new();
        let mut hash = tip;
        let fails = loop {
            if let Some(known) = self.fails_pow.get(&hash) {
                break *known;
            }
            if path.len() >= PENDING_WALK_LIMIT {
                break false;
            }
            #[cfg(debug_assertions)]
            hub.note_path_work_step();
            if hub.is_connected(&hash) {
                break false;
            }
            let Some(hdr) = pending.get(&hash).copied().or_else(|| hub.header_of(&hash)) else {
                break false;
            };
            if !hub.header_claimed_pow_ok(&hdr) {
                break true;
            }
            path.push(hash);
            hash = hdr.prev_blockhash;
            if hash == BlockHash::all_zeros() {
                break false;
            }
        };
        // Stored headers passed proof of work, so a failing header is a
        // pending one and lies within the walk limit of every hash above it.
        for h in path {
            if memo_keeps(pending, &mut self.fails_stored, self.stored_cap, &h) {
                self.fails_pow.insert(h, fails);
            }
        }
        fails
    }

    /// [`Self::path_connects`] and not weaker than our tip.
    fn better_path_connects(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
    ) -> bool {
        self.better_path_connects_in(hub, pending, tip, empty_no_base())
    }

    fn better_path_connects_in(
        &mut self,
        hub: &ChainHub,
        pending: &HashMap<BlockHash, bitcoin::block::Header>,
        tip: BlockHash,
        no_base: &NoBaseMap,
    ) -> bool {
        let weaker = self
            .work_of_header_path_in(hub, pending, tip, no_base)
            .zip(hub.chain_work().ok())
            .is_some_and(|(announced, ours)| announced < ours);
        !weaker && self.path_connects_in(hub, pending, tip, no_base)
    }
}

/// Walk from `tip` until `step` ends the path or a memoised hash, then score
/// every new hash on the way back up and memoise the ones `keep` takes.
/// Claimed proof of work is checked only on a path that reaches a base. A
/// path of more than `PENDING_WALK_LIMIT - 1` headers has no work.
fn memo_path_work(
    known: &mut HashMap<BlockHash, Option<(bitcoin::Work, usize)>>,
    hub: &ChainHub,
    tip: BlockHash,
    mut step: impl FnMut(BlockHash) -> WorkStep,
    mut keep: impl FnMut(&BlockHash) -> bool,
) -> Option<bitcoin::Work> {
    let mut path: Vec<(BlockHash, bitcoin::block::Header)> = Vec::new();
    let mut h = tip;
    let mut below = loop {
        if let Some(scored) = known.get(&h) {
            break *scored;
        }
        #[cfg(debug_assertions)]
        hub.note_path_work_step();
        match step(h) {
            WorkStep::Base(work) => break Some((work, 0)),
            WorkStep::Dead => break None,
            WorkStep::Header(_) if path.len() + 1 >= PENDING_WALK_LIMIT => {
                // Only `tip` is known to be too deep. A lower hash on this
                // path may still reach a base within its own limit.
                if keep(&tip) {
                    known.insert(tip, None);
                }
                return None;
            }
            WorkStep::Header(hdr) => {
                path.push((h, hdr));
                h = hdr.prev_blockhash;
            }
        }
    };
    for (hash, hdr) in path.into_iter().rev() {
        below = below.and_then(|(work, depth)| {
            if depth + 1 >= PENDING_WALK_LIMIT || !hub.header_claimed_pow_ok(&hdr) {
                return None;
            }
            let work = crate::most_work::checked_add_work(work, hdr.work()).ok()?;
            Some((work, depth + 1))
        });
        if keep(&hash) {
            known.insert(hash, below);
        }
    }
    below.map(|(work, _)| work)
}

/// Height of a pending path that reaches genesis. No header-store read.
/// A marked hash contributes the steps recorded for it.
fn pending_announced_height_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    no_base: &NoBaseMap,
) -> u32 {
    if is_genesis_hash(&tip) {
        return 0;
    }
    pending_walk_in(pending, tip, |h| no_base_depth(hub, no_base, h)).1
}

#[doc(hidden)]
pub fn pending_walk(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
) -> (BlockHash, u32) {
    pending_walk_in(pending, start, |_| None)
}

/// [`pending_walk`]. `stop` returns the remaining steps when `hash` does not
/// reach a base, and the walk ends there.
fn pending_walk_in(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
    mut stop: impl FnMut(&BlockHash) -> Option<u32>,
) -> (BlockHash, u32) {
    let mut h = start;
    let mut steps = 0u32;
    while steps < PENDING_WALK_LIMIT as u32 {
        if let Some(depth) = stop(&h) {
            return (h, steps.saturating_add(depth));
        }
        if is_genesis_hash(&h) {
            return (h, steps);
        }
        let Some(hdr) = pending.get(&h) else {
            return (h, steps);
        };
        steps = steps.saturating_add(1);
        h = hdr.prev_blockhash;
    }
    (h, steps)
}

/// Height of `tip` from stored headers or a walk of this peer's pending path.
///
/// Core logs `chain_start.nHeight + headers.size()` on the *batch*. Node-to-node
/// generate announces one header per tip; ignored headers are not stored, so
/// height must come from the pending walk (14 one-header announces → 14).
#[doc(hidden)]
pub fn announced_headers_height(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
) -> u32 {
    if is_genesis_hash(&tip) {
        return 0;
    }
    if let Some(h) = hub.header_height(&tip) {
        return h;
    }
    let (join, steps) = pending_walk(pending, tip);
    if is_genesis_hash(&join) {
        return steps;
    }
    hub.header_height(&join).unwrap_or(0).saturating_add(steps)
}

/// Headers on the walk from `tip` that are not in the store yet, oldest first.
/// The walk stops at a stored header. That header is not included.
fn unstored_header_path(
    hub: &ChainHub,
    tip: BlockHash,
    mut lookup: impl FnMut(BlockHash) -> Option<bitcoin::block::Header>,
) -> Vec<bitcoin::block::Header> {
    let mut path = Vec::new();
    let mut h = tip;
    for _ in 0..PENDING_WALK_LIMIT {
        let Some(hdr) = lookup(h) else {
            break;
        };
        if header_is_stored(hub, &h) {
            break;
        }
        path.push(hdr);
        h = hdr.prev_blockhash;
        if is_genesis_hash(&h) {
            break;
        }
    }
    path.reverse();
    path
}

/// Persist `tip`'s pending path oldest-first so `ensure_header` has parents.
/// Store the pending headers from `tip` back to the last stored one. Headers
/// already stored were checked when they were written.
///
/// The store's error when a chunk is rejected. Chunks already written in this
/// call stay.
#[doc(hidden)]
pub fn persist_pending_header_path(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
) -> Result<(), NetError> {
    persist_pending_header_path_in(hub, pending, tip, empty_no_base())
}

fn persist_pending_header_path_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    no_base: &NoBaseMap,
) -> Result<(), NetError> {
    let path = unstored_header_path(hub, tip, |hash| {
        if no_base_depth(hub, no_base, &hash).is_some() {
            return None;
        }
        pending.get(&hash).copied()
    });
    // One store batch. Per-header ensure walks parent heights to genesis, so a
    // full below-floor chain (thousands of headers) is quadratic.
    for chunk in path.chunks(MAX_HEADERS_RESULTS) {
        hub.ensure_headers_batch(chunk)?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn header_is_stored(hub: &ChainHub, hash: &BlockHash) -> bool {
    #[cfg(debug_assertions)]
    hub.note_stored_header_lookup();
    hub.query
        .get_header_by_hash(&hash.to_byte_array())
        .ok()
        .flatten()
        .is_some()
}

#[doc(hidden)]
pub fn header_announcement_connects(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    prev: BlockHash,
) -> bool {
    header_connects(hub, pending, prev, empty_no_base())
}

fn header_connects(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    prev: BlockHash,
    no_base: &NoBaseMap,
) -> bool {
    if is_genesis_hash(&prev) || hub.knows_header(&prev) {
        return true;
    }
    if no_base_depth(hub, no_base, &prev).is_some() {
        return false;
    }
    let (join, _) = pending_walk_in(pending, prev, |h| no_base_depth(hub, no_base, h));
    is_genesis_hash(&join) || hub.knows_header(&join)
}

/// Headers more than this many blocks behind our tip are not useful for
/// tip-follow (Core `NODE_NETWORK_LIMITED` window, ~2 days).
pub const ANCIENT_TIP_BLOCKS: u32 = 288;

/// Connecting header path whose **work** cannot beat our tip and whose announced
/// height is more than [`ANCIENT_TIP_BLOCKS`] behind — BIP-110-class minority fork.
pub fn announced_tip_is_hopeless(
    our_tip: u32,
    announced_h: u32,
    work_cmp: Option<std::cmp::Ordering>,
) -> bool {
    matches!(work_cmp, Some(std::cmp::Ordering::Less))
        && announced_h.saturating_add(ANCIENT_TIP_BLOCKS) < our_tip
}

/// Announced path work vs our tip work. `None` if the walk cannot sum work.
#[doc(hidden)]
pub fn announced_work_cmp(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
) -> Option<std::cmp::Ordering> {
    let announced = work_of_header_path(hub, pending, start)?;
    let ours = hub.chain_work().ok()?;
    Some(announced.cmp(&ours))
}

/// Compare announced header-chain length (equal-bits ≈ work) to our path
/// from the same ancestor. `None` if the header walk does not reach our chain.
#[doc(hidden)]
pub fn header_branch_vs_tip(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    start: BlockHash,
) -> Option<std::cmp::Ordering> {
    if hub.is_connected(&start) {
        let ancestor = hub
            .query
            .published_height_of_hash(&start.to_byte_array())?
            .0;
        let tip = hub.tip_height()?;
        return Some(0u32.cmp(&tip.saturating_sub(ancestor)));
    }
    let (mut h, mut n_new) = pending_walk(pending, start);
    if is_genesis_hash(&h) {
        return Some(std::cmp::Ordering::Greater);
    }
    for _ in 0..PENDING_WALK_LIMIT {
        if hub.is_connected(&h) {
            let ancestor = hub.query.published_height_of_hash(&h.to_byte_array())?.0;
            let tip = hub.tip_height()?;
            return Some(n_new.cmp(&tip.saturating_sub(ancestor)));
        }
        let prev = hub.prev_of(&h)?;
        n_new = n_new.saturating_add(1);
        h = prev;
        if is_genesis_hash(&h) {
            return Some(std::cmp::Ordering::Greater);
        }
    }
    None
}

/// See [`PathWorkMemo::header_path_meets_minwork`].
#[doc(hidden)]
pub fn header_path_meets_minwork(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
) -> bool {
    PathWorkMemo::default().header_path_meets_minwork(hub, pending, tip)
}

fn any_header_path_meets_minwork_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    extra_tip: BlockHash,
    no_base: &NoBaseMap,
) -> bool {
    let mut memo = PathWorkMemo::default();
    let meets = |memo: &mut PathWorkMemo, hash: BlockHash| {
        if no_base_depth(hub, no_base, &hash).is_some() {
            return false;
        }
        memo.header_path_meets_minwork_in(hub, pending, hash, no_base)
    };
    if meets(&mut memo, extra_tip) {
        return true;
    }
    pending
        .keys()
        .any(|h| *h != extra_tip && meets(&mut memo, *h))
}

#[doc(hidden)]
pub fn work_of_header_path(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
) -> Option<bitcoin::Work> {
    PathWorkMemo::default().work_of_header_path(hub, pending, tip)
}

/// Connecting header path from `tip` back to our chain, oldest first.
/// Empty below `-minimumchainwork` or when the walk cannot join our chain.
/// Includes a still-weaker competitor so stale-fork getdata can be dropped.
#[doc(hidden)]
pub fn connecting_header_path(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
) -> Vec<BlockHash> {
    if !PathWorkMemo::default().path_connects(hub, pending, tip) {
        return Vec::new();
    }
    header_path_above(hub, pending, tip, &HashSet::new())
}

/// Hashes from `tip` down to our chain, oldest first, cut above the first
/// hash in `given`. Callers put whole connecting paths in `given`, so every
/// hash under a given one is given too and the cut drops only those.
fn header_path_above(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    given: &HashSet<BlockHash>,
) -> Vec<BlockHash> {
    let mut path = Vec::new();
    let mut h = tip;
    for _ in 0..PENDING_WALK_LIMIT {
        if hub.is_connected(&h) || given.contains(&h) {
            break;
        }
        path.push(h);
        let prev = pending
            .get(&h)
            .map(|hdr| hdr.prev_blockhash)
            .or_else(|| hub.prev_of(&h));
        let Some(prev) = prev else {
            break;
        };
        h = prev;
        if h.to_byte_array() == [0u8; 32] {
            break;
        }
    }
    path.reverse();
    path
}

/// Bodies on `tip`'s connecting header path that we may `getdata`.
/// Weaker-than-tip and below `-minimumchainwork` stay header-only.
#[doc(hidden)]
pub fn fetchable_header_path_bodies(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    pending_blocks: &PendingBlocks,
    requested: &HashSet<BlockHash>,
) -> Vec<BlockHash> {
    fetchable_header_path_bodies_in(
        hub,
        pending,
        tip,
        pending_blocks,
        requested,
        empty_no_base(),
    )
}

fn fetchable_header_path_bodies_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    tip: BlockHash,
    pending_blocks: &PendingBlocks,
    requested: &HashSet<BlockHash>,
    no_base: &NoBaseMap,
) -> Vec<BlockHash> {
    if !PathWorkMemo::default().better_path_connects_in(hub, pending, tip, no_base) {
        return Vec::new();
    }
    let path = header_path_above(hub, pending, tip, &HashSet::new());
    fetchable_bodies(hub, path, pending_blocks, requested)
}

/// Connecting paths of the pending leaves that `wanted` accepts, oldest
/// first per leaf, each hash once. One memo scores every leaf.
fn pending_leaf_paths(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    wanted: impl Fn(
        &mut PathWorkMemo,
        &ChainHub,
        &HashMap<BlockHash, bitcoin::block::Header>,
        BlockHash,
    ) -> bool,
) -> Vec<BlockHash> {
    let mut memo = PathWorkMemo::default();
    let mut given = HashSet::new();
    let mut out = Vec::new();
    for last in pending_header_leaves(pending) {
        if !wanted(&mut memo, hub, pending, last) {
            continue;
        }
        let path = header_path_above(hub, pending, last, &given);
        given.extend(path.iter().copied());
        out.extend(path);
    }
    out
}

fn fetchable_bodies(
    hub: &ChainHub,
    path: Vec<BlockHash>,
    pending_blocks: &PendingBlocks,
    requested: &HashSet<BlockHash>,
) -> Vec<BlockHash> {
    path.into_iter()
        .filter(|h| {
            !pending_blocks.contains_key(h)
                && !requested.contains(h)
                && !hub.already_have_or_asked_block(h)
        })
        .collect()
}

/// Compact getdata is the next block (prev is our tip). Catch-up and competing
/// forks use `MSG_WITNESS_BLOCK`.
#[doc(hidden)]
pub fn compact_getdata_near_tip(hub: &ChainHub, hash: &BlockHash) -> bool {
    if hub.tip_hash() == Some(*hash) {
        return true;
    }
    hub.header_of(hash)
        .is_some_and(|hdr| hub.tip_hash() == Some(hdr.prev_blockhash))
}

/// Core CMPCTBLOCK: reconstruct unsolicited compact only from a peer we sent
/// `sendcmpct(1)` (BIP152 high-bandwidth, Core #32606), when claimed work
/// beats the tip and `nHeight <= tipHeight + 2`. The next block (prev is
/// tip) is always near-tip even if the header is not yet in the work prefix.
/// In-tree callers with no session are not a peer and skip the
/// high-bandwidth rule.
#[doc(hidden)]
pub fn compact_unsolicited_reconstruct(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
    header: &bitcoin::block::Header,
) -> bool {
    if !session.is_none_or(|s| s.hb_to.load(Ordering::Relaxed)) {
        return false;
    }
    if hub.tip_hash() == Some(header.prev_blockhash) || hub.tip_hash() == Some(header.block_hash())
    {
        return true;
    }
    let Ok(tip_w) = hub.chain_work() else {
        return true;
    };
    if hub.work_with_header(header) <= tip_w {
        return false;
    }
    let Some(tip_h) = hub.tip_height() else {
        return true;
    };
    let Some(h) = compact_claimed_height(hub, header) else {
        return false;
    };
    h <= tip_h.saturating_add(2)
}

#[doc(hidden)]
pub fn compact_claimed_height(hub: &ChainHub, header: &bitcoin::block::Header) -> Option<u32> {
    let hash = header.block_hash();
    if let Some(h) = hub.header_height(&hash) {
        return Some(h);
    }
    hub.header_height(&header.prev_blockhash)
        .map(|p| p.saturating_add(1))
}

/// Resend BIP133 feefilter when the published floor moved since the last send.
#[doc(hidden)]
pub fn maybe_queue_feefilter(hub: &ChainHub, out_tx: &PeerOutTx, session: &crate::peers::LivePeer) {
    let Some(fee) = outbound_feefilter_sats(hub, Some(session)) else {
        return;
    };
    if session.sent_feefilter_sat_kvb() == fee {
        return;
    }
    if out_tx.send(NetworkMessage::FeeFilter(fee)).is_ok() {
        session.note_sent_feefilter(fee);
    }
}

/// BIP133 feefilter to send after handshake. None = do not send (blocksonly,
/// forcerelay, block-relay-only). IBD still sends rounded MAX_MONEY.
pub fn outbound_feefilter_sats(
    hub: &ChainHub,
    session: Option<&crate::peers::LivePeer>,
) -> Option<i64> {
    if session.is_some_and(|s| {
        s.conn_type == crate::peers::PeerConnType::BlockRelay || s.session_forcerelay()
    }) {
        return None;
    }
    if hub.in_ibd() {
        return Some(hub.feefilter_sat_kvb() as i64);
    }
    if hub.mempool().is_none_or(|m| !m.relay_enabled()) {
        return None;
    }
    Some(hub.feefilter_sat_kvb() as i64)
}

/// One pong per session on the queue. A later ping replaces the nonce and
/// does not queue another message. No session queues every pong.
#[doc(hidden)]
pub fn queue_pong(
    session: Option<&crate::peers::LivePeer>,
    out: &PeerOutTx,
    nonce: u64,
) -> Result<(), NetError> {
    let Some(session) = session else {
        return out.send(NetworkMessage::Pong(nonce));
    };
    session.store_pong_nonce(nonce);
    if !session.claim_pong_send() {
        return Ok(());
    }
    out.send(NetworkMessage::Pong(nonce))
}

/// Send filtered tx announcements in [`TX_INV_BATCH`] chunks.
///
/// The caller already owns the filtered inventories. Chunking does not clone
/// the live wtxid set and does not hold the mempool lock across the send.
#[doc(hidden)]
pub fn queue_tx_inv_batches(out_tx: &PeerOutTx, inv: Vec<Inventory>) {
    for chunk in inv.chunks(TX_INV_BATCH) {
        let _ = out_tx.send(NetworkMessage::Inv(chunk.to_vec()));
    }
}

#[doc(hidden)]
pub fn pending_header_leaves(
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
) -> Vec<BlockHash> {
    let prevs: HashSet<BlockHash> = pending.values().map(|h| h.prev_blockhash).collect();
    pending
        .keys()
        .copied()
        .filter(|h| !prevs.contains(h))
        .collect()
}

#[doc(hidden)]
pub fn keep_pending_connecting_paths(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
) -> Vec<BlockHash> {
    keep_pending_connecting_paths_in(hub, pending, empty_no_base())
}

fn keep_pending_connecting_paths_in(
    hub: &ChainHub,
    pending: &HashMap<BlockHash, bitcoin::block::Header>,
    no_base: &NoBaseMap,
) -> Vec<BlockHash> {
    pending_leaf_paths(hub, pending, |memo, hub, pending, tip| {
        memo.path_connects_in(hub, pending, tip, no_base)
    })
}

/// In-flight asks off `path`. An empty path keeps them all.
fn asks_off_path(requested: &HashSet<BlockHash>, path: &[BlockHash]) -> Vec<BlockHash> {
    if path.is_empty() {
        return Vec::new();
    }
    requested
        .iter()
        .copied()
        .filter(|h| !path.contains(h))
        .collect()
}

#[doc(hidden)]
pub fn release_asks_off_path(requested: &mut RequestedBlocks, path: &[BlockHash]) {
    for h in asks_off_path(requested.hashes(), path) {
        requested.remove(&h);
    }
}

/// Try to accept pending blocks that connect to tip or form a better branch.
pub async fn drain_pending(
    hub: &ChainHub,
    out: &PeerOutTx,
    pending_blocks: &mut PendingBlocks,
    pending_headers: &mut HashMap<BlockHash, bitcoin::block::Header>,
    requested_blocks: &mut RequestedBlocks,
    compact: bool,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    // A reorg can make a held block the child of the *new* tip after the
    // greedy pass already ran. Repeat until the tip is stable.
    loop {
        let tip_before = hub.tip_hash();
        drain_pending_once(hub, pending_blocks, pending_headers, session).await?;
        if hub.tip_hash() == tip_before {
            break;
        }
    }

    let keep = keep_pending_connecting_paths(hub, pending_headers);
    let better = pending_leaf_paths(hub, pending_headers, PathWorkMemo::better_path_connects);
    let mut missing = fetchable_bodies(hub, better, pending_blocks, requested_blocks.hashes());
    release_asks_off_path(requested_blocks, &keep);
    missing.retain(|h| !requested_blocks.contains(h));
    for h in hub.held_missing_parents() {
        if !missing.contains(&h) {
            missing.push(h);
        }
    }
    for b in pending_blocks.values() {
        let prev = b.header.prev_blockhash;
        if prev.to_byte_array() != [0u8; 32]
            && !hub.is_connected(&prev)
            && !pending_blocks.contains_key(&prev)
            && hub.held_body(&prev).is_none()
            && !missing.contains(&prev)
        {
            missing.push(prev);
        }
    }
    missing.retain(|h| !requested_blocks.contains(h));
    let room = MAX_SERVE_BLOCKS.saturating_sub(requested_blocks.len());
    missing.truncate(room);
    queue_block_getdata(hub, out, session, requested_blocks, &missing, compact)?;
    Ok(())
}

pub fn drain_pending_now(
    hub: &ChainHub,
    out: &PeerOutTx,
    pending_blocks: &mut PendingBlocks,
    pending_headers: &mut HashMap<BlockHash, bitcoin::block::Header>,
    requested_blocks: &mut RequestedBlocks,
    compact: bool,
) -> Result<(), NetError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("drain_pending test runtime")
        .block_on(drain_pending(
            hub,
            out,
            pending_blocks,
            pending_headers,
            requested_blocks,
            compact,
            None,
        ))
}

/// Feed complete pending bodies into the hub receive path. Pending is a
/// download window, not a second most-work assembler.
pub async fn drain_pending_once(
    hub: &ChainHub,
    pending_blocks: &mut PendingBlocks,
    pending_headers: &mut HashMap<BlockHash, bitcoin::block::Header>,
    session: Option<&crate::peers::LivePeer>,
) -> Result<(), NetError> {
    let mut progress = true;
    while progress {
        progress = false;
        let candidates: Vec<BlockHash> = pending_blocks.keys().copied().collect();
        for h in candidates {
            let Some((block, ask)) = pending_blocks.take(&h) else {
                continue;
            };
            pending_headers.remove(&h);
            relay_new_pow_valid_block(hub, &block, session).await;
            match accept_received_from_peer(hub, block.clone(), session).await {
                Ok(AcceptOutcome::Accepted { .. })
                | Ok(AcceptOutcome::AlreadyHave)
                | Ok(AcceptOutcome::IgnoredWeaker) => {
                    progress = true;
                }
                Err(e) if net_error_needs_parent(&e) => {
                    pending_blocks.insert(h, block, ask);
                }
                // Invalid body: reject the block, keep the peer. BIP-152
                // high-bandwidth can deliver PoW-valid-but-invalid blocks
                // from honest Core peers that have not validated yet
                // (docs/external_findings/001-disconnect-on-invalid-block.md).
                Err(e) if net_error_is_store_not_found(&e) => {
                    rbitcoin_log::warn!(
                        "p2p: accept dropped {} (store not found — keep session): {e}",
                        h
                    );
                    hub.settle_rejected_ask(ask, h, &e);
                }
                Err(e @ NetError::Store(_)) => {
                    note_store_fault_keep_session(h, &e);
                }
                Err(e) => {
                    rbitcoin_log::warn!(
                        "p2p: accept dropped {} (invalid/unconnectable — keep session): {e}",
                        h
                    );
                    hub.settle_rejected_ask(ask, h, &e);
                }
            }
        }
    }
    Ok(())
}

/// Inbound `getheaders` reply. Empty while tip work is below `-minimumchainwork`.
pub fn headers_reply_for_getheaders(
    hub: &ChainHub,
    gh: &bitcoin::p2p::message_blockdata::GetHeadersMessage,
) -> Result<Vec<bitcoin::block::Header>, NetError> {
    if !hub.meets_minimum_chain_work() {
        return Ok(Vec::new());
    }
    if gh.locator_hashes.is_empty() {
        let stop = gh.stop_hash;
        if stop.to_byte_array() != [0u8; 32] {
            if hub.is_connected(&stop) {
                if let Some(h) = hub.header_of(&stop) {
                    return Ok(vec![h]);
                }
            } else if hub.stale_relay_allowed(&stop) {
                let have_body = hub.cache.has_body(&stop)
                    || hub
                        .query
                        .is_block_archived(&stop.to_byte_array())
                        .unwrap_or(false);
                if have_body {
                    if let Some(h) = hub.header_of(&stop) {
                        return Ok(vec![h]);
                    }
                }
            }
            return Ok(Vec::new());
        }
    }
    headers_for_peer(hub.cache.as_ref(), hub.query.as_ref(), gh)
}

#[doc(hidden)]
pub fn headers_for_peer(
    cache: &BlockCache,
    query: &Query,
    gh: &bitcoin::p2p::message_blockdata::GetHeadersMessage,
) -> Result<Vec<bitcoin::block::Header>, NetError> {
    crate::reactor::assert_not_reactor("header reply");
    match query.headers_after_locator(&gh.locator_hashes, gh.stop_hash, MAX_HEADERS_RESULTS) {
        Ok(h) if !h.is_empty() || query.tip_height().is_some() => Ok(h),
        Ok(_) => Ok(cache.headers_after_locator(&gh.locator_hashes, gh.stop_hash)),
        Err(e) => Err(NetError::Consensus(e.to_string())),
    }
}

/// A store error while reading an announced tip wtxid is `notfound`.
/// The caller still turns a join failure into [`NetError`].
#[doc(hidden)]
pub fn announced_wtx_from_reconstruct(
    found: Result<Option<Transaction>, NetError>,
) -> Option<Transaction> {
    found.ok().flatten()
}

#[doc(hidden)]
pub fn tx_from_stored_block(
    cache: &BlockCache,
    query: &Query,
    hash: &BlockHash,
    wtxid: &bitcoin::Wtxid,
) -> Result<Option<Transaction>, NetError> {
    let Some(block) = block_for_peer(cache, query, hash)? else {
        return Ok(None);
    };
    Ok(block
        .txdata
        .into_iter()
        .find(|tx| tx.compute_wtxid() == *wtxid))
}

#[doc(hidden)]
pub fn block_for_peer(
    cache: &BlockCache,
    query: &Query,
    hash: &BlockHash,
) -> Result<Option<bitcoin::Block>, NetError> {
    #[cfg(debug_assertions)]
    cache.note_body_load();
    crate::reactor::assert_not_reactor("block reconstruct");
    if let Some(block) = cache.get_block(hash) {
        return Ok(Some(block));
    }
    if let Some(block) = cache.servable(hash) {
        return Ok(Some(block));
    }
    match query.reconstruct_block_by_hash(&hash.to_byte_array()) {
        Ok(b) => Ok(b),
        Err(rbitcoin_store::StoreError::Pruned { .. }) => Ok(None),
        Err(e) => Err(NetError::Consensus(e.to_string())),
    }
}

#[doc(hidden)]
pub fn payload_tx_count(payload: &[u8]) -> u32 {
    use bitcoin::consensus::Decodable;
    if payload.len() < 81 {
        return 0;
    }
    bitcoin::consensus::encode::VarInt::consensus_decode(&mut &payload[80..])
        .map(|v| v.0 as u32)
        .unwrap_or(0)
}

/// A getdata `block` body as v2 contents. Core serves `MSG_WITNESS_BLOCK`
/// with witness data and `MSG_BLOCK` without (`TX_NO_WITNESS`). Both start
/// from the witness serialization; `MSG_BLOCK` then drops the marker, flag
/// and witness stacks with a wire walk, so no `Block` is decoded for it.
#[doc(hidden)]
pub fn encode_served_block(
    cache: &BlockCache,
    query: &Query,
    hash: &BlockHash,
    witness: bool,
) -> Result<Option<Vec<u8>>, NetError> {
    #[cfg(debug_assertions)]
    cache.note_body_load();
    crate::reactor::assert_not_reactor("getdata reconstruct");
    let t0 = std::time::Instant::now();
    let (tx_count, contents) =
        if let Some(block) = cache.get_block(hash).or_else(|| cache.servable(hash)) {
            let tx_count = block.txdata.len() as u32;
            (
                tx_count,
                crate::v2::encode_v2_contents(NetworkMessage::Block(block))?,
            )
        } else {
            match query.witness_block_bytes_by_hash(&hash.to_byte_array()) {
                Ok(Some(payload)) => {
                    let mut contents = Vec::with_capacity(1 + payload.len());
                    contents.push(2);
                    contents.extend_from_slice(&payload);
                    (payload_tx_count(&payload), contents)
                }
                Ok(None) => return Ok(None),
                Err(rbitcoin_store::StoreError::Pruned { .. }) => return Ok(None),
                Err(e) => return Err(NetError::Consensus(e.to_string())),
            }
        };
    let contents = if witness {
        contents
    } else {
        let stripped = rbitcoin_store::block_wire::block_wire_strip_witness(&contents[1..])
            .ok_or_else(|| NetError::Consensus(format!("served block {hash} does not walk")))?;
        let mut plain = Vec::with_capacity(1 + stripped.len());
        plain.push(2);
        plain.extend_from_slice(&stripped);
        plain
    };
    let payload_len = contents.len().saturating_sub(1);
    crate::serve_perf::note_serve(tx_count, payload_len, t0.elapsed().as_nanos());
    Ok(Some(contents))
}
