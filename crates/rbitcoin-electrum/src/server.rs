//! Line-delimited JSON-RPC Electrum server (TCP).
//!
//! Confirmed history from the store; unconfirmed + broadcast via optional
//! [`MempoolHub`] (plan P6, libre-relay-class).

use bitcoin::consensus::Encodable;
use bitcoin::hashes::Hash;
use rbitcoin_consensus::ChainParams;
use rbitcoin_net::RequestMeter;
use rbitcoin_net::{BlockingRegion, MempoolHub};
use rbitcoin_primitives::{Fk, Height};

use rbitcoin_query::{ChainView, ChainViewKind, HistoryFilter, Query, ShJoinSlot};
use rbitcoin_store::StoreError;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, Notify, Semaphore};
use tokio::task::JoinHandle;

#[doc(hidden)]
pub const PROTOCOL_MIN: &str = "1.4";
#[doc(hidden)]
pub const PROTOCOL_MAX: &str = "1.6";
// Older as-of clients negotiate this; a trailing `asof:<blockhash>` needs no
// dialect. Not a dotted-int; `protocol_max` stays 1.6.
#[doc(hidden)]
pub const PROTOCOL_ASOF: &str = "1.4.2-asof";
/// First `server.version` element, read by wallets that branch on it.
/// Sparrow sends JSON-RPC batches when the lowercased string contains
/// `electrumx`. BlueWallet stops batching only when it starts with
/// `electrs`, so it keeps batching. Cake Wallet `getNodeIsElectrs()` probes
/// `blockchain.tweaks.subscribe [0, 1, false]` when it contains `electrs`.
#[doc(hidden)]
pub const SERVER_VERSION: &str =
    concat!("rbitcoin-electrs ", env!("CARGO_PKG_VERSION"), " electrumx");

/// One JSON-RPC request line. Junk is `None`; must not panic.
pub fn parse_electrum_request_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

/// Electrum JSON-RPC requests: the `tip: perf` window and the `/metrics` totals.
#[doc(hidden)]
pub static METER: RequestMeter = RequestMeter::new();

/// Electrum request meters since the previous sample: `(count, sum_us, max_us)`.
/// Running totals are untouched ([`perf_totals`]).
pub fn sample_reset_perf() -> (u64, u64, u64) {
    METER.take_window()
}

/// Running Electrum totals for `/metrics`: `(requests, sum_us)`.
pub fn perf_totals() -> (u64, u64) {
    METER.totals()
}

#[doc(hidden)]
pub fn meter_dispatch_wall(us: u64) {
    METER.note(us);
}

/// Max simultaneous query-surface clients (Electrum / future Esplora).
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;
/// Max request payload bytes (Electrum: one JSON-RPC line incl. `\n`; Esplora: body).
pub const DEFAULT_MAX_LINE_BYTES: usize = 1_048_576;
/// Max requests in one JSON-RPC batch line: the work one line can queue on
/// a connection. Reply RAM is bounded separately by writing the reply one
/// slot at a time. Fulcrum's default `max_batch`; Sparrow pages at 100.
#[doc(hidden)]
pub const MAX_BATCH_ITEMS: usize = 345;
/// Alias for shared docs / Esplora body cap ([`DEFAULT_MAX_LINE_BYTES`]).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = DEFAULT_MAX_LINE_BYTES;
/// Max scripthash subscriptions per Electrum connection (notify fan-out).
///
/// A desktop wallet subscribes every receive and change address up to its gap
/// limit, so 1,000 cut off ordinary Sparrow wallets mid-sync. Each sub costs a
/// key plus a last-sent status (~170 B; ~0.45 GB worst case at 256 connections
/// x 10,000) and, per new block, one posting-list check against that block's
/// shared touch set. A reorg or a tick gap > 32 restatuses every sub in full.
/// Operators can raise it (`--electrum-max-subs`).
pub const DEFAULT_MAX_SCRIPTHASH_SUBS: usize = 10_000;
/// Default cap on `blockchain.silentpayments.subscribe` addresses per
/// connection. Each address scans every new block with its own key, and
/// each subscribe may scan the whole chain, so this is far below the
/// scripthash cap. Frigate's default is 100. Operators can raise it
/// (`--electrum-max-sp-subs`).
pub const DEFAULT_MAX_SP_SUBS: usize = 16;
/// Silent-payment history scans the process runs at once, across every
/// connection. The per-connection cap alone leaves connections x addresses.
/// A subscribe past this is refused until a scan ends. Not a knob.
pub const DEFAULT_MAX_SP_SCANS: usize = 32;
/// Idle read timeout — disconnect quiet clients (DoS of FD/tasks).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;
/// Max raw tx hex chars for `transaction.broadcast` (~4 MiB wire → 8 MiB hex).
pub const DEFAULT_MAX_BROADCAST_HEX: usize = 8_388_608;

/// Shared application DoS bounds for internet-facing query surfaces.
///
/// Defaults are sized for a **public bind behind a TLS reverse proxy** (or a
/// private LAN bind). The node always enforces these limits — binding only on
/// localhost is **not** required for safety. TLS, rate limiting at the edge,
/// and auth remain operator / proxy concerns.
///
/// Electrum uses this today; Esplora (HTTP) reuses the same type for connection
/// / body / idle caps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeLimits {
    /// Max concurrent clients (TCP accept or HTTP).
    pub max_connections: usize,
    /// Max request payload (Electrum line or HTTP body), bytes.
    pub max_request_bytes: usize,
    /// Disconnect if no complete request within this duration.
    pub idle_timeout: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self::for_public_proxy()
    }
}

impl ServeLimits {
    /// Defaults suitable when the listen address is reachable from untrusted
    /// clients **behind** TLS termination / a reverse proxy.
    pub fn for_public_proxy() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ElectrumConfig {
    pub listen: SocketAddr,
    pub banner: String,
    pub donation_address: String,
    /// Genesis hash (display order hex) for features.
    pub genesis_hash_hex: String,
    /// Shared connection / request / idle bounds (also used by future Esplora).
    pub limits: ServeLimits,
    /// Max `blockchain.scripthash.subscribe` entries per connection.
    pub max_scripthash_subs: usize,
    /// Silent-payment addresses one connection may subscribe (>= 1).
    pub max_sp_subs: usize,
    /// One permit per running silent-payment history scan
    /// ([`DEFAULT_MAX_SP_SCANS`]). Clones of the config share it, so it is
    /// one bound for every listener.
    pub sp_scan_slots: Arc<Semaphore>,
    /// Heights one history scan step reads. Tests shorten it.
    #[doc(hidden)]
    pub sp_scan_chunk: u32,
    /// Max hex length accepted by `blockchain.transaction.broadcast`.
    pub max_broadcast_hex: usize,
    /// Omit served P2TR outs with `value <=` this (sats). `0` serves all.
    /// Default [`crate::tweaks::DEFAULT_TWEAKS_MIN_DUST`].
    pub tweaks_min_dust: u64,
    /// Tor v3 hostname + TCP port for `server.features.hosts` (empty until set).
    pub onion_tcp: Arc<OnceLock<(String, u16)>>,
    /// Read-only batch items one connection may have in flight
    /// ([`batch_window`]). The node sets it from its blocking pool.
    pub batch_window: usize,
    /// `Some` is [`LISTEN_OFF`], [`LISTEN_UP`], or [`LISTEN_DOWN`]. The accept
    /// task writes it. `None` leaves the caller's flag alone.
    pub listen_state: Option<Arc<AtomicU8>>,
}

/// Electrum accept task has not been started.
pub const LISTEN_OFF: u8 = 0;
/// Electrum accept loop is running.
pub const LISTEN_UP: u8 = 1;
/// Electrum accept loop stopped without a shutdown request.
pub const LISTEN_DOWN: u8 = 2;

impl ElectrumConfig {
    pub fn for_params(listen: SocketAddr, params: &ChainParams) -> Self {
        let genesis = params.genesis_hash.to_byte_array();
        Self {
            listen,
            banner: "rbitcoin electrum — libre-relay-class (0.1 sat/vB, no dust ban, full RBF)"
                .into(),
            donation_address: String::new(),
            genesis_hash_hex: rbitcoin_primitives::display_hash_hex(genesis),
            limits: ServeLimits::for_public_proxy(),
            max_scripthash_subs: DEFAULT_MAX_SCRIPTHASH_SUBS,
            max_sp_subs: DEFAULT_MAX_SP_SUBS,
            sp_scan_slots: Arc::new(Semaphore::new(DEFAULT_MAX_SP_SCANS)),
            sp_scan_chunk: crate::silent_scan::SP_SCAN_CHUNK,
            max_broadcast_hex: DEFAULT_MAX_BROADCAST_HEX,
            tweaks_min_dust: crate::tweaks::DEFAULT_TWEAKS_MIN_DUST,
            onion_tcp: Arc::new(OnceLock::new()),
            batch_window: batch_window(MIN_BLOCKING_POOL),
            listen_state: None,
        }
    }

    /// Max concurrent Electrum clients ([`ServeLimits::max_connections`]).
    pub fn max_connections(&self) -> usize {
        self.limits.max_connections
    }

    /// Max JSON-RPC request line bytes ([`ServeLimits::max_request_bytes`]).
    pub fn max_line_bytes(&self) -> usize {
        self.limits.max_request_bytes
    }

    /// Idle read timeout ([`ServeLimits::idle_timeout`]).
    pub fn idle_timeout(&self) -> Duration {
        self.limits.idle_timeout
    }
}

/// Stop bit for the accept loop and each client. `store(true)` wakes waiters.
pub struct ElectrumStop {
    flag: AtomicBool,
    wake: Notify,
}

impl ElectrumStop {
    pub fn new(initial: bool) -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(initial),
            wake: Notify::new(),
        })
    }

    pub fn load(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn store(&self, value: bool) {
        self.flag.store(value, Ordering::SeqCst);
        if value {
            self.wake.notify_waiters();
        }
    }

    fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.wake.notified()
    }
}

pub struct ElectrumHandle {
    pub local_addr: SocketAddr,
    pub shutdown: Arc<ElectrumStop>,
    pub tasks: Vec<JoinHandle<()>>,
    pub clients: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ElectrumHandle {
    pub async fn shutdown(mut self) {
        self.shutdown.store(true);
        // The accept task owns the listener. Await it so the port is closed
        // when this returns, not when a worker next runs the abort.
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
        // Client tasks do not observe the accept-loop flag; abort so SIGINT
        // is not blocked behind a scripthash history / mempool restatus.
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        for t in clients.drain(..) {
            t.abort();
        }
    }
}

/// Log the end of one connection. A client causes any socket error by closing
/// or resetting, so those log at DEBUG. A task this side spawned that panicked
/// or was cancelled logs at WARN.
#[doc(hidden)]
pub fn log_disconnect(peer: SocketAddr, how: &std::io::Result<()>) {
    match how {
        Ok(()) => rbitcoin_log::debug!("electrum: disconnect {peer}"),
        Err(e) if rbitcoin_net::is_join_error(e) => {
            rbitcoin_log::warn!("electrum: disconnect {peer} (dispatch join: {e})")
        }
        Err(e) => rbitcoin_log::debug!("electrum: disconnect {peer} ({e})"),
    }
}

/// Tip notification for header subscriptions.
#[derive(Clone, Debug)]
pub struct TipNotify {
    pub height: u32,
    pub header_hex: String,
    /// Set when this tip replaced a previous prefix (reorg / same-height).
    /// Subscribe restatuses every watched scripthash, not only the new block.
    pub reorg_from_height: Option<u32>,
}

/// Start Electrum **plain TCP** listener.
///
/// `mempool` enables broadcast, unconfirmed history/balance, fee estimates, and
/// `transaction.get` fallback. Without it, confirmed-only behaviour remains.
///
/// TLS is intentionally not built in — terminate TLS at nginx/caddy/haproxy
/// (or similar) and proxy to this TCP port. Safe for internet-facing deployment
/// **only with app [`ServeLimits`] always on** plus edge TLS/limits; do not treat
/// “localhost-only bind” as the sole safety model.
///
/// **DoS limits:** [`ElectrumConfig::limits`] ([`ServeLimits`]) plus
/// scripthash-sub and broadcast-hex caps on [`ElectrumConfig`].
pub async fn run_electrum(
    config: ElectrumConfig,
    query: Arc<Query>,
    params: ChainParams,
    tip_tx: broadcast::Sender<TipNotify>,
    mempool: Option<Arc<MempoolHub>>,
) -> Result<ElectrumHandle, std::io::Error> {
    let listener = rbitcoin_net::listen::bind_tcp(config.listen)?;
    let local_addr = listener.local_addr()?;
    let shutdown = ElectrumStop::new(false);
    let shutdown_c = Arc::clone(&shutdown);
    let max_conn = config.max_connections().max(1);
    let conn_sem = Arc::new(Semaphore::new(max_conn));
    let config = Arc::new(config);
    let params = Arc::new(params);
    let clients: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let clients_c = clients.clone();

    let listen_state = config.listen_state.clone();
    let task = tokio::spawn(async move {
        struct ListenMark {
            state: Option<Arc<AtomicU8>>,
            stop: Arc<ElectrumStop>,
        }
        impl Drop for ListenMark {
            fn drop(&mut self) {
                let Some(state) = &self.state else {
                    return;
                };
                let next = if self.stop.load() {
                    LISTEN_OFF
                } else {
                    LISTEN_DOWN
                };
                state.store(next, Ordering::SeqCst);
            }
        }
        if let Some(state) = &listen_state {
            state.store(LISTEN_UP, Ordering::SeqCst);
        }
        let _mark = ListenMark {
            state: listen_state,
            stop: Arc::clone(&shutdown_c),
        };
        loop {
            let wake = shutdown_c.notified();
            if shutdown_c.load() {
                break;
            }
            tokio::select! {
                biased;
                _ = wake => break,
                accept = listener.accept() => {
            match accept {
                Ok((stream, peer)) => {
                    if !rbitcoin_net::listen::listener_fd_room() {
                        rbitcoin_log::warn!(
                            "electrum: reject {peer} (descriptor reserve)"
                        );
                        drop(stream);
                        continue;
                    }
                    let Ok(permit) = conn_sem.clone().try_acquire_owned() else {
                        rbitcoin_log::warn!(
                            "electrum: reject {peer} (at max_connections={max_conn})"
                        );
                        drop(stream);
                        continue;
                    };
                    rbitcoin_log::debug!("electrum: connect {peer}");
                    // Replies, batch slots included, are small writes the
                    // client waits on whole; Nagle would hold them for an ACK.
                    if let Err(e) = stream.set_nodelay(true) {
                        rbitcoin_log::warn!("electrum: {peer} set_nodelay: {e}");
                    }
                    let q = query.clone();
                    let cfg = config.clone();
                    let p = params.clone();
                    let tip_rx = tip_tx.subscribe();
                    let mp = mempool.clone();
                    let stop = shutdown_c.clone();
                    let h = tokio::spawn(async move {
                        let _connection_slot = permit;
                        let how = handle_client(stream, peer, q, cfg, p, tip_rx, mp, stop).await;
                        log_disconnect(peer, &how);
                    });
                    let mut g = clients_c.lock().unwrap_or_else(|e| e.into_inner());
                    g.retain(|t| !t.is_finished());
                    g.push(h);
                }
                Err(e) => {
                    if rbitcoin_net::service::accept_error_ends_listen(&e) {
                        rbitcoin_log::warn!("electrum: accept ended: {e}");
                        break;
                    }
                    rbitcoin_log::warn!("electrum: accept retry: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
                }
            }
        }
    });

    Ok(ElectrumHandle {
        local_addr,
        shutdown,
        tasks: vec![task],
        clients,
    })
}

/// Read one `\n`-terminated line with a hard byte cap (prevents OOM without newline).
pub async fn read_line_capped<R>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, std::io::Error>
where
    R: AsyncBufReadExt + Unpin,
{
    let max_bytes = max_bytes.max(1);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                if buf.len() > max_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "electrum request line too long",
                    ));
                }
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if buf.len().saturating_add(take) > max_bytes {
                reader.consume(take);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "electrum request line too long",
                ));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        if buf.len().saturating_add(available.len()) > max_bytes {
            let n = available.len();
            reader.consume(n);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "electrum request line too long",
            ));
        }
        buf.extend_from_slice(available);
        let n = available.len();
        reader.consume(n);
    }
}

#[doc(hidden)]
pub struct ElectrumConn {
    pub protocol: String,
    pub header_sub: bool,
    pub sh_subs: HashSet<[u8; 32]>,
    pub sh_join: Option<Arc<ShJoinSlot>>,
    pub outpoint_subs: HashSet<([u8; 32], u32)>,
    /// Silent-payment subscriptions whose history has been sent.
    pub sp_subs: Vec<SpLive>,
    /// History scans of subscribes that have not reached the tip. An address
    /// is in one of the two lists, once.
    pub sp_scans: Vec<SpScanRun>,
}

impl ElectrumConn {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            protocol: String::new(),
            header_sub: false,
            sh_subs: HashSet::new(),
            sh_join: None,
            outpoint_subs: HashSet::new(),
            sp_subs: Vec::new(),
            sp_scans: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
#[allow(clippy::cognitive_complexity)] // Electrum session dispatch
pub async fn handle_client<S>(
    stream: S,
    peer: SocketAddr,
    query: Arc<Query>,
    config: Arc<ElectrumConfig>,
    params: Arc<ChainParams>,
    mut tip_rx: broadcast::Receiver<TipNotify>,
    mempool: Option<Arc<MempoolHub>>,
    stop: Arc<ElectrumStop>,
) -> Result<(), std::io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut conn = ElectrumConn::new();
    let mut last_sent_status: HashMap<[u8; 32], String> = HashMap::new();
    let notify = Arc::new(Notify::new());
    let mut mempool_rx = mempool.as_ref().map(|m| m.subscribe_announces());
    let mut writer = ConnWrite::new(writer, config.idle_timeout());
    let max_line = config.max_line_bytes();
    let mut sh_seen = query.sh_indexed_through_height();
    // A status read overlapped a held pool, so the tick reads every sub again.
    let mut sh_rescan = false;
    let mut sh_tick = tokio::time::interval(Duration::from_millis(50));
    sh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let stop_wait = stop.notified();
        if stop.load() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_wait => break,
            tip = tip_rx.recv() => {
                match tip {
                    Ok(t) => {
                        if conn.header_sub {
                            let msg = json!({
                                "jsonrpc": "2.0",
                                "method": "blockchain.headers.subscribe",
                                "params": [{ "hex": t.header_hex, "height": t.height }]
                            });
                            writer.line(msg).await?;
                        }
                        if !conn.outpoint_subs.is_empty() {
                            emit_outpoint_notes(
                                &mut writer,
                                &query,
                                mempool.clone(),
                                conn.outpoint_subs.iter().copied().collect(),
                            )
                            .await?;
                        }
                        restart_sp_scans_after_reorg(&mut conn.sp_scans, &query, &params, &t);
                        emit_sp_tips(&mut writer, &query, &params, &mut conn, &t).await?;
                        if !conn.sh_subs.is_empty() {
                            let heights = if t.reorg_from_height.is_some() {
                                None
                            } else {
                                Some(vec![t.height])
                            };
                            sh_rescan |= !emit_sh_notes(
                                &mut writer,
                                &query,
                                mempool.clone(),
                                &conn.sh_subs,
                                &mut last_sent_status,
                                heights,
                            )
                            .await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !conn.sh_subs.is_empty() {
                            sh_rescan |= !emit_sh_notes(
                                &mut writer,
                                &query,
                                mempool.clone(),
                                &conn.sh_subs,
                                &mut last_sent_status,
                                None,
                            )
                            .await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = async {
                if conn.sh_subs.is_empty() {
                    std::future::pending::<()>().await;
                } else {
                    sh_tick.tick().await;
                }
            } => {
                let now = query.sh_indexed_through_height();
                let scan = match tick_scan(sh_seen, now) {
                    _ if sh_rescan => None,
                    Some(scan) => scan,
                    None => continue,
                };
                if conn.sh_subs.is_empty() {
                    sh_seen = now;
                    continue;
                }
                // A reorg walks the watermark back before its txs are back in
                // the pool, and may walk it forward again before that. A read
                // inside the hold is dropped and every sub is read again.
                sh_rescan = !emit_sh_notes(
                    &mut writer,
                    &query,
                    mempool.clone(),
                    &conn.sh_subs,
                    &mut last_sent_status,
                    scan,
                )
                .await?;
                if !sh_rescan {
                    sh_seen = now;
                }
            }
            ann = async {
                if let Some(rx) = mempool_rx.as_mut() {
                    Some(rx.recv().await)
                } else {
                    std::future::pending::<()>().await;
                    None
                }
            } => {
                // Only restatus outpoints and hashes this tx (or its RBF victims)
                // actually touch. Re-walking every subscribe on every mempool
                // accept pegs CPUs (Cake: tens of gap-limit subs × full history +
                // full-mempool scan).
                if let Some(Ok(ann)) = ann {
                    if let Some(mp) = &mempool {
                        let touched_ops: Vec<([u8; 32], u32)> = ann
                            .outpoints
                            .iter()
                            .map(|op| (op.txid.to_byte_array(), op.vout))
                            .filter(|op| conn.outpoint_subs.contains(op))
                            .collect();
                        if !touched_ops.is_empty() {
                            emit_outpoint_notes(&mut writer, &query, mempool.clone(), touched_ops)
                                .await?;
                        }
                        // Walk the tx's few scripthashes and probe the sub set,
                        // so the cost per accept does not grow with sub count.
                        let mut touched: Vec<[u8; 32]> = ann
                            .scripthashes
                            .iter()
                            .chain(ann.replaced_scripthashes.iter())
                            .filter(|sh| conn.sh_subs.contains(*sh))
                            .copied()
                            .collect();
                        touched.sort_unstable();
                        touched.dedup();
                        if touched.is_empty() {
                            continue;
                        }
                        // History and the mempool read take std locks. The
                        // blocking pool is named tokio-rt-worker too, so the
                        // region has to be entered there.
                        let q = Arc::clone(&query);
                        let hub = Arc::clone(mp);
                        let q_read = Arc::clone(&q);
                        let statuses = budgeted_blocking(&q, move || {
                            settled_restatus_notes(&q_read, Some(&hub), &touched, None)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            rbitcoin_log::warn!(
                                "electrum: mempool scripthash status join failed: {e}"
                            );
                            Some(Vec::new())
                        });
                        let Some(statuses) = statuses else {
                            sh_rescan = true;
                            continue;
                        };
                        push_sh_notes(&mut writer, &conn.sh_subs, &mut last_sent_status, statuses)
                            .await?;
                    }
                }
            }
            line = tokio::time::timeout(writer.idle(), read_line_capped(&mut reader, max_line)) => {
                let line = match line {
                    Ok(Ok(Some(l))) => l,
                    Ok(Ok(None)) => {
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        if e.kind() == std::io::ErrorKind::InvalidData {
                            let resp = json!({
                                "jsonrpc":"2.0","id": null,
                                "error": {"code": -32600, "message": "request line too long"}
                            });
                            let _ = writer.line(resp).await;
                            return Err(e);
                        }
                        return Err(e);
                    }
                    Err(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "idle timeout",
                        ));
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let req: Value = match parse_electrum_request_line(&line) {
                    Some(v) => v,
                    None => {
                        writer.line(json!({
                            "jsonrpc":"2.0","id":null,
                            "error":{"code":-32700,"message":"parse error"}
                        })).await?;
                        continue;
                    }
                };
                let ctx = RequestCtx {
                    peer,
                    query: &query,
                    config: &config,
                    params: &params,
                    mempool: &mempool,
                };
                let resp = match req {
                    Value::Array(items) => {
                        answer_batch(&ctx, &mut conn, &mut last_sent_status, &mut writer, items)
                        .await?;
                        continue;
                    }
                    req => match request_stream(&req) {
                        Some(stream) => {
                            let id = req.get("id").cloned().unwrap_or(Value::Null);
                            let params_v = req.get("params").cloned().unwrap_or(json!([]));
                            match stream {
                                ElectrumStream::SilentPayments => {
                                    serve_sp_subscribe(
                                        &mut writer,
                                        &query,
                                        &params,
                                        &peer,
                                        id,
                                        &params_v,
                                        &mut conn,
                                        &config,
                                    )
                                    .await?
                                }
                                ElectrumStream::Tweaks => {
                                    serve_tweaks_subscribe(
                                        &mut reader,
                                        &mut writer,
                                        &query,
                                        &params,
                                        &peer,
                                        id,
                                        &params_v,
                                        max_line,
                                        config.tweaks_min_dust,
                                    )
                                    .await?
                                }
                            }
                            continue;
                        }
                        None => answer_one(&ctx, &mut conn, &mut last_sent_status, &req).await?,
                    },
                };
                writer.line(resp).await?;
                let _ = &notify;
            }
            // Last: a scan with steps ready does not hold back a request.
            (address, step) = next_sp_scan_step(&mut conn.sp_scans) => {
                write_sp_scan_step(&mut writer, &query, &params, &mut conn, &address, step)
                    .await?;
            }
        }
    }
    Ok(())
}

/// Smallest blocking pool the node builds (`max(cores, 4)` threads).
pub use rbitcoin_net::MIN_BLOCKING_POOL;

/// Read-only batch items one connection may have in flight for a blocking
/// pool of `blocking_pool` threads: half of it, at least one. Each item holds
/// a thread while it runs and its reply until written. Block decode, compact
/// reconstruct, header jobs and mempool accept share that pool
/// (`docs/concurrency.md`), so one batch leaves them the other half.
pub fn batch_window(blocking_pool: usize) -> usize {
    (blocking_pool / 2).max(1)
}

/// One scheduling step of a batch, by item index. Recorded only for tests.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchStep {
    Spawn(usize),
    Serial(usize),
    Write(usize),
    /// Item and the wall time it reports to the API log and the meter.
    Timed(usize, Duration),
}

#[cfg(debug_assertions)]
thread_local! {
    static BATCH_TRACE_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BATCH_TRACE: std::cell::RefCell<Vec<BatchStep>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn batch_mark(_step: BatchStep) {
    #[cfg(debug_assertions)]
    if BATCH_TRACE_ON.with(|on| on.get()) {
        BATCH_TRACE.with(|trace| trace.borrow_mut().push(_step));
    }
}

/// Start recording batch steps run on this thread, from an empty trace. A
/// debug node schedules every batch through `batch_mark` and nothing reads
/// the trace, so it records only after this call.
#[cfg(debug_assertions)]
pub fn batch_trace_arm() {
    BATCH_TRACE.with(|trace| trace.borrow_mut().clear());
    BATCH_TRACE_ON.with(|on| on.set(true));
}

/// Steps recorded on this thread, then stop. A debug build keeps nothing
/// unless a test arms the trace again.
#[cfg(debug_assertions)]
pub fn batch_trace_snapshot() -> Vec<BatchStep> {
    BATCH_TRACE_ON.with(|on| on.set(false));
    BATCH_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

/// The connection's fixed inputs to one request.
struct RequestCtx<'a> {
    peer: SocketAddr,
    query: &'a Arc<Query>,
    config: &'a Arc<ElectrumConfig>,
    params: &'a Arc<ChainParams>,
    mempool: &'a Option<Arc<MempoolHub>>,
}

/// Methods that answer with more than one line. They own the connection
/// until the stream ends, so they cannot sit in a batch.
enum ElectrumStream {
    SilentPayments,
    Tweaks,
}

fn request_stream(req: &Value) -> Option<ElectrumStream> {
    match req.get("method").and_then(|m| m.as_str())? {
        "blockchain.silentpayments.subscribe" => Some(ElectrumStream::SilentPayments),
        "blockchain.tweaks.subscribe" => Some(ElectrumStream::Tweaks),
        _ => None,
    }
}

fn invalid_request(id: Value, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id": id, "error": {"code": -32600, "message": message}})
}

/// Methods a batch may answer out of turn. They read the store, the
/// mempool, or the negotiated protocol and never change connection state.
/// `sh_join` is only a cache of the last scripthash join, so an item that
/// starts from an older copy recomputes; it does not answer differently.
#[doc(hidden)]
pub fn method_reads_only(method: &str) -> bool {
    matches!(
        method,
        "blockchain.scripthash.get_history"
            | "blockchain.scripthash.get_balance"
            | "blockchain.scripthash.listunspent"
            | "blockchain.scripthash.get_mempool"
            | "blockchain.transaction.get"
            | "blockchain.transaction.get_merkle"
            | "blockchain.transaction.id_from_pos"
            | "blockchain.block.header"
            | "blockchain.block.headers"
            | "blockchain.estimatefee"
            | "blockchain.outpoint.get_status"
            | "mempool.get_fee_histogram"
            | "mempool.get_info"
    )
}

/// One request as asked.
struct Asked {
    id: Value,
    method: String,
    params: Value,
}

impl Asked {
    fn of(req: &Value) -> Self {
        Self {
            id: req.get("id").cloned().unwrap_or(Value::Null),
            method: req
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            params: req.get("params").cloned().unwrap_or(json!([])),
        }
    }
}

/// How a batch takes up one item.
enum BatchItem {
    /// A notification (no `id` member, unlike `"id": null`). It is not run:
    /// no reply means no slot write to hold back a client that stops
    /// reading, and Electrum clients never send notifications.
    Skip,
    /// Answered without running anything.
    Reply(Value),
    /// [`method_reads_only`]: may run beside other reads.
    Read(Asked),
    /// Touches connection state: runs alone, after every earlier slot.
    Serial,
}

fn batch_item(item: &Value) -> BatchItem {
    let Some(obj) = item.as_object() else {
        return BatchItem::Reply(invalid_request(Value::Null, "Invalid Request"));
    };
    let Some(id) = obj.get("id") else {
        return BatchItem::Skip;
    };
    if request_stream(item).is_some() {
        return BatchItem::Reply(invalid_request(
            id.clone(),
            "streaming method cannot be sent in a batch",
        ));
    }
    let asked = Asked::of(item);
    if method_reads_only(&asked.method) {
        BatchItem::Read(asked)
    } else {
        BatchItem::Serial
    }
}

/// The reply array, written slot by slot as one line.
struct BatchOut<'w, W> {
    writer: &'w mut ConnWrite<W>,
    opened: bool,
}

impl<W: AsyncWrite + Unpin> BatchOut<'_, W> {
    async fn slot(&mut self, index: usize, answer: Value) -> Result<(), std::io::Error> {
        batch_mark(BatchStep::Write(index));
        let mut slot = vec![if self.opened { b',' } else { b'[' }];
        slot.extend(
            crate::json_cpu::encode_json_body(Arc::new(answer))
                .await
                .map_err(|_| std::io::Error::other("json encode join failed"))?,
        );
        self.opened = true;
        self.writer.bytes(&slot).await
    }

    async fn close(self) -> Result<(), std::io::Error> {
        if !self.opened {
            return Ok(());
        }
        self.writer.bytes(b"]\n").await
    }
}

/// A dispatch result, the worker state it ran with, its chain view, and how
/// long the dispatch itself took on the blocking thread.
type Dispatched = (
    Result<Value, String>,
    ElectrumConn,
    Option<ChainView>,
    Duration,
);

/// A read running on the blocking pool, not yet written.
struct InFlight {
    index: usize,
    asked: Asked,
    task: JoinHandle<Dispatched>,
}

/// JSON-RPC 2.0 batch, one array line on the wire, slots in request order.
/// Read-only items run up to [`ElectrumConfig::batch_window`] at once; slot
/// `i` is written before read `i + window` starts, so a client that
/// stops reading stops the work and the connection holds at most that many
/// unwritten replies. Any other item waits until every earlier slot is
/// written, then runs alone, so later items see what it did. A batch of
/// only notifications writes nothing.
async fn answer_batch<W: AsyncWrite + Unpin>(
    ctx: &RequestCtx<'_>,
    conn: &mut ElectrumConn,
    last_sent: &mut HashMap<[u8; 32], String>,
    writer: &mut ConnWrite<W>,
    items: Vec<Value>,
) -> Result<(), std::io::Error> {
    if items.is_empty() {
        return writer
            .line(invalid_request(Value::Null, "empty batch"))
            .await;
    }
    if items.len() > MAX_BATCH_ITEMS {
        return writer
            .line(invalid_request(Value::Null, "batch too large"))
            .await;
    }
    let mut out = BatchOut {
        writer,
        opened: false,
    };
    let limit = ctx.config.batch_window.max(1);
    let mut window: VecDeque<InFlight> = VecDeque::with_capacity(limit);
    for (index, item) in items.iter().enumerate() {
        let answer = match batch_item(item) {
            BatchItem::Skip => continue,
            BatchItem::Read(asked) => {
                if window.len() >= limit {
                    if let Some(oldest) = window.pop_front() {
                        land(ctx, conn, last_sent, &mut out, oldest).await?;
                    }
                }
                batch_mark(BatchStep::Spawn(index));
                let task = spawn_dispatch(ctx, read_conn(conn, &asked.method), &asked);
                window.push_back(InFlight { index, asked, task });
                continue;
            }
            BatchItem::Reply(answer) => answer,
            BatchItem::Serial => {
                while let Some(oldest) = window.pop_front() {
                    land(ctx, conn, last_sent, &mut out, oldest).await?;
                }
                batch_mark(BatchStep::Serial(index));
                answer_one(ctx, conn, last_sent, item).await?
            }
        };
        while let Some(oldest) = window.pop_front() {
            land(ctx, conn, last_sent, &mut out, oldest).await?;
        }
        out.slot(index, answer).await?;
    }
    while let Some(oldest) = window.pop_front() {
        land(ctx, conn, last_sent, &mut out, oldest).await?;
    }
    out.close().await
}

/// Methods that read and fill the connection's scripthash join cache.
fn method_uses_sh_join(method: &str) -> bool {
    matches!(
        method,
        "blockchain.scripthash.get_history"
            | "blockchain.scripthash.get_balance"
            | "blockchain.scripthash.listunspent"
    )
}

/// The connection state a read needs: the negotiated protocol, and for a
/// scripthash read the join cache itself, moved out so the read can fill
/// it in place. A scripthash read that starts while another holds the
/// cache gets none and joins afresh. Subscription sets stay with the
/// connection.
fn read_conn(conn: &mut ElectrumConn, method: &str) -> ElectrumConn {
    ElectrumConn {
        protocol: conn.protocol.clone(),
        header_sub: conn.header_sub,
        sh_subs: HashSet::new(),
        sh_join: if method_uses_sh_join(method) {
            conn.sh_join.take()
        } else {
            None
        },
        outpoint_subs: HashSet::new(),
        sp_subs: Vec::new(),
        sp_scans: Vec::new(),
    }
}

/// Wait for the oldest read and write its slot. A join cache the read hands
/// back (the one it took, or one it built) becomes the connection's, so in
/// request order the last scripthash read's join is kept, as when reads run
/// one after another. A read that never held one leaves the cache alone.
async fn land<W: AsyncWrite + Unpin>(
    ctx: &RequestCtx<'_>,
    conn: &mut ElectrumConn,
    last_sent: &mut HashMap<[u8; 32], String>,
    out: &mut BatchOut<'_, W>,
    flight: InFlight,
) -> Result<(), std::io::Error> {
    let (result, work, stamped, ran) = flight.task.await.map_err(std::io::Error::other)?;
    if work.sh_join.is_some() {
        conn.sh_join = work.sh_join;
    }
    batch_mark(BatchStep::Timed(flight.index, ran));
    let answer = finish_reply(
        ctx,
        conn,
        last_sent,
        flight.asked,
        result,
        stamped,
        Took::Ran(ran),
    );
    out.slot(flight.index, answer).await
}

/// The wall time a reply reports. A single request counts from when it was
/// taken up; a batch read counts only its own dispatch, not the window wait
/// or the writes of earlier slots.
enum Took {
    Since(Instant),
    Ran(Duration),
}

impl Took {
    fn elapsed(&self) -> Duration {
        match self {
            Took::Since(t0) => t0.elapsed(),
            Took::Ran(d) => *d,
        }
    }
}

/// Store reads that also change connection state. They stay serial in a
/// batch, and they still take the shared disk budget.
fn method_reads_store(method: &str) -> bool {
    method_reads_only(method)
        || matches!(
            method,
            "blockchain.headers.subscribe"
                | "blockchain.scripthash.subscribe"
                | "blockchain.outpoint.subscribe"
        )
}

fn spawn_dispatch(
    ctx: &RequestCtx<'_>,
    work: ElectrumConn,
    asked: &Asked,
) -> JoinHandle<Dispatched> {
    let q = Arc::clone(ctx.query);
    let cfg = Arc::clone(ctx.config);
    let p = Arc::clone(ctx.params);
    let mp = ctx.mempool.clone();
    let method = asked.method.clone();
    let params = asked.params.clone();
    let reads = method_reads_store(&method);
    // The permit wait stays on this task. The blocking pool is taken only
    // after a slot is free, so a budget of 1 serializes the reads.
    tokio::spawn(async move {
        let permit = if reads {
            Some(
                q.api_read_budget()
                    .acquire_owned()
                    .await
                    .expect("store budget"),
            )
        } else {
            None
        };
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _g = BlockingRegion::enter();
            let t0 = Instant::now();
            let (r, work, view) =
                dispatch_off_worker(&q, &cfg, &p, mp.as_deref(), &method, &params, work);
            (r, work, view, t0.elapsed())
        })
        .await;
        rbitcoin_net::finish_join(joined, "electrum dispatch join")
    })
}

/// Wait for one store-read permit on this task, then run `f` on the blocking
/// pool. The permit stays inside `f` until the walk returns, so a waiter does
/// not occupy a pool thread.
async fn budgeted_blocking<T: Send + 'static>(
    query: &Query,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let permit = query
        .api_read_budget()
        .acquire_owned()
        .await
        .expect("store budget");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _g = BlockingRegion::enter();
        f()
    })
    .await
}

/// The blocking half of a request, run with `work` moved in and handed back.
fn dispatch_off_worker(
    q: &Query,
    cfg: &ElectrumConfig,
    p: &ChainParams,
    mp: Option<&MempoolHub>,
    method: &str,
    params: &Value,
    mut work: ElectrumConn,
) -> (Result<Value, String>, ElectrumConn, Option<ChainView>) {
    if !method_stamps_chain_tip(method) {
        let r = dispatch_pinned(method, params, q, cfg, p, mp, &mut work, None, false);
        return (r, work, None);
    }
    let (r, view) = electrum_at_chain_view(q, method, params, |q, params, view, is_asof| {
        dispatch_pinned(method, params, q, cfg, p, mp, &mut work, view, is_asof)
    });
    (r, work, view)
}

/// One request/response method. `Err` is a dispatch task that died, which
/// ends the connection.
async fn answer_one(
    ctx: &RequestCtx<'_>,
    conn: &mut ElectrumConn,
    last_sent: &mut HashMap<[u8; 32], String>,
    req: &Value,
) -> Result<Value, std::io::Error> {
    let asked = Asked::of(req);
    let t0 = Instant::now();
    if method_awaits_scripts(&asked.method) {
        let result = match asked.method.as_str() {
            "blockchain.transaction.broadcast" => {
                broadcast_raw_tx_async(&asked.params, ctx.config, ctx.mempool.as_ref()).await
            }
            "blockchain.transaction.broadcast_package" => {
                broadcast_package_async(&asked.params, ctx.config, ctx.mempool.as_ref()).await
            }
            _ => Err("unknown broadcast".into()),
        };
        return Ok(finish_reply(
            ctx,
            conn,
            last_sent,
            asked,
            result,
            None,
            Took::Since(t0),
        ));
    }
    if method_stays_on_worker(&asked.method) {
        let result = dispatch_with_join(
            &asked.method,
            &asked.params,
            ctx.query,
            ctx.config,
            ctx.params,
            ctx.mempool.as_deref(),
            conn,
        );
        return Ok(finish_reply(
            ctx,
            conn,
            last_sent,
            asked,
            result,
            None,
            Took::Since(t0),
        ));
    }
    // Move the sub sets in and back (`*conn = work` below): a clone per
    // request made a wallet's N-address sync O(N²). A JoinError ends the
    // connection, so nothing reuses them.
    let work = ElectrumConn {
        protocol: conn.protocol.clone(),
        header_sub: conn.header_sub,
        sh_subs: std::mem::take(&mut conn.sh_subs),
        sh_join: conn.sh_join.take(),
        outpoint_subs: std::mem::take(&mut conn.outpoint_subs),
        sp_subs: std::mem::take(&mut conn.sp_subs),
        sp_scans: std::mem::take(&mut conn.sp_scans),
    };
    let (result, work, stamped, _ran) = spawn_dispatch(ctx, work, &asked)
        .await
        .map_err(std::io::Error::other)?;
    *conn = work;
    Ok(finish_reply(
        ctx,
        conn,
        last_sent,
        asked,
        result,
        stamped,
        Took::Since(t0),
    ))
}

/// Status bookkeeping, metering, the API log line, and the reply object.
#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
fn finish_reply(
    ctx: &RequestCtx<'_>,
    conn: &ElectrumConn,
    last_sent: &mut HashMap<[u8; 32], String>,
    asked: Asked,
    result: Result<Value, String>,
    stamped: Option<ChainView>,
    took: Took,
) -> Value {
    let Asked {
        id,
        method,
        params: params_v,
    } = asked;
    let method = method.as_str();
    drop_unsubscribed_status(last_sent, &conn.sh_subs);
    let wall_ms = took.elapsed().as_millis() as u64;
    meter_dispatch_wall(took.elapsed().as_micros() as u64);
    let params_s = serde_json::to_string(&params_v).unwrap_or_else(|_| "[]".into());
    match result {
        Ok(v) => {
            if method == "blockchain.scripthash.subscribe" {
                if let Ok(sh) = param_scripthash(&params_v, 0) {
                    record_sent_status(last_sent, sh, &v);
                }
            } else if method == "blockchain.scripthash.unsubscribe" {
                // Exact and O(1): a resubscribe must not be
                // deduplicated against a status sent before.
                if let Ok(sh) = param_scripthash(&params_v, 0) {
                    last_sent.remove(&sh);
                }
            }
            rbitcoin_log::api_call(
                "electrum",
                &ctx.peer.to_string(),
                method,
                &params_s,
                wall_ms,
                None,
            );
            rpc_result(&id, &v, stamped.as_ref())
        }
        Err(e) => {
            rbitcoin_log::api_call(
                "electrum",
                &ctx.peer.to_string(),
                method,
                &params_s,
                wall_ms,
                Some(&e),
            );
            json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": e}})
        }
    }
}

/// Frigate subscribe: the result line, then the history scan started beside
/// the session. Its notes follow from [`write_sp_scan_step`]. A subscribe
/// to an address the connection already has replaces that subscription and
/// scans again. `config` caps the addresses of the connection and the scans
/// of the process.
#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub async fn serve_sp_subscribe<W>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    peer: &SocketAddr,
    id: Value,
    params_v: &Value,
    conn: &mut ElectrumConn,
    config: &ElectrumConfig,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    let max_subs = config.max_sp_subs;
    let tip = query.tip_height().map(|h| h.0);
    let mut sub = match parse_sp_sub(params_v, query, chain).await {
        Ok(s) => s,
        Err(e) => {
            writer
                .line(json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": e}}))
                .await?;
            return Ok(());
        }
    };
    // Frigate: a subscribe must not narrow the coverage the address has.
    let held_range = sp_held_range(conn, &sub.address);
    if let Some((start, end)) = held_range {
        sub.start = sub.start.min(start);
        // An open subscription stays open. Two ranges keep the later end.
        sub.end = sub.end.zip(end).map(|(new, held)| new.max(held));
    }
    let held = conn.sp_subs.len() + conn.sp_scans.len();
    if held_range.is_none() && held >= max_subs {
        let message = format!("subscription limit reached ({max_subs}) for this connection");
        writer
            .line(json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": message}}))
            .await?;
        return Ok(());
    }
    // The reply is the one place an error reaches the client: a note has
    // no error member. A start that cannot be read is refused here.
    let (q, c, start) = (Arc::clone(query), Arc::clone(chain), sub.start);
    let readable = budgeted_blocking(query, move || {
        rbitcoin_query::tweaks_at_height(&q, &c, Height(start))
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    if let Err(message) = readable {
        writer
            .line(json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": message}}))
            .await?;
        return Ok(());
    }
    // One slot per running scan, process-wide. A scan this subscribe
    // replaces hands its slot on.
    let replaced = conn
        .sp_scans
        .iter()
        .position(|run| run.sub.address == sub.address)
        .map(|i| conn.sp_scans.remove(i));
    let slot = match replaced {
        Some(run) => run.slot,
        None => {
            match Arc::clone(&config.sp_scan_slots).try_acquire_owned() {
                Ok(slot) => slot,
                Err(_) => {
                    let message = "silent payment scan limit reached, try again later";
                    writer
                    .line(json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": message}}))
                    .await?;
                    return Ok(());
                }
            }
        }
    };
    let result = crate::silent_scan::subscribe_result(&sub);
    rbitcoin_log::api_call(
        "electrum",
        &peer.to_string(),
        "blockchain.silentpayments.subscribe",
        &redact_sp_params(params_v),
        0,
        None,
    );
    writer.line(rpc_result(&id, &result, None)).await?;
    let last = sub.end.unwrap_or(tip.unwrap_or(sub.start).max(sub.start));
    drop_sp_address(conn, &sub.address);
    let from = sub.start;
    conn.sp_scans.push(SpScanRun::start(
        query,
        chain,
        sub,
        from,
        last,
        config.sp_scan_chunk,
        slot,
    ));
    Ok(())
}

/// The start, and the end of a closed range, of the connection's
/// subscription to `address`.
fn sp_held_range(conn: &ElectrumConn, address: &str) -> Option<(u32, Option<u32>)> {
    let live = conn.sp_subs.iter().map(|live| &live.sub);
    let scanning = conn.sp_scans.iter().map(|run| &run.sub);
    live.chain(scanning)
        .find(|sub| sub.address == address)
        .map(|sub| (sub.start, sub.end))
}

/// End the subscription to `address`, live or still scanning.
fn drop_sp_address(conn: &mut ElectrumConn, address: &str) {
    conn.sp_subs.retain(|live| live.sub.address != address);
    conn.sp_scans.retain(|run| run.sub.address != address);
}

/// Parse against the confirmed chain: its tip, and its header times for a
/// timestamp start.
fn parse_sp_sub_on_chain(
    params: &Value,
    query: &Query,
    chain: &ChainParams,
) -> Result<crate::silent_scan::SpSub, String> {
    let tip = query.tip_height().map(|h| h.0);
    let block_time = |height: u32| match query.header_at_height(Height(height)) {
        Ok(Some((_fk, header))) => Ok(header.timestamp),
        Ok(None) => Err(format!("no header at height {height}")),
        Err(e) => Err(e.to_string()),
    };
    crate::silent_scan::parse_sub(params, chain.network, tip, &block_time)
}

/// [`parse_sp_sub_on_chain`] off the connection task: a timestamp start
/// reads headers.
async fn parse_sp_sub(
    params: &Value,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
) -> Result<crate::silent_scan::SpSub, String> {
    let (p, q, c) = (params.clone(), Arc::clone(query), Arc::clone(chain));
    budgeted_blocking(query, move || parse_sp_sub_on_chain(&p, &q, &c))
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

#[doc(hidden)]
pub fn silentpayments_unsubscribe(
    conn: &mut ElectrumConn,
    query: &Query,
    chain: &ChainParams,
    params: &Value,
) -> Result<Value, String> {
    let sub = parse_sp_sub_on_chain(params, query, chain)?;
    drop_sp_address(conn, &sub.address);
    Ok(json!(sub.address))
}

/// A tip event for every live silent-payment subscription: each scans the
/// heights it has not seen. One whose heights cannot be read gets the error
/// note and ends. Left live, it would count those heights as scanned.
#[doc(hidden)]
pub async fn emit_sp_tips<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &ChainParams,
    conn: &mut ElectrumConn,
    tip: &TipNotify,
) -> Result<(), std::io::Error> {
    let mut kept = Vec::with_capacity(conn.sp_subs.len());
    for mut live in std::mem::take(&mut conn.sp_subs) {
        let from = live.next_unscanned(tip);
        if from <= tip.height
            && !emit_sp_tip(writer, query, chain, &live.sub, from, tip.height).await?
        {
            continue;
        }
        // A reorg replaced what was above its tip, so the tip is the last
        // height scanned. A late event for a lower height rewinds nothing.
        live.through = match tip.reorg_from_height {
            Some(_) => tip.height,
            None => live.through.max(tip.height),
        };
        kept.push(live);
    }
    conn.sp_subs = kept;
    Ok(())
}

/// Heights `from..=to` connected after the history: one note when any of
/// them pays the subscription. `false` when a height could not be read; the
/// error note is written.
pub async fn emit_sp_tip<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &ChainParams,
    sub: &crate::silent_scan::SpSub,
    from: u32,
    to: u32,
) -> Result<bool, std::io::Error> {
    let q = Arc::clone(query);
    let c = chain.clone();
    let piece = sub.clone();
    let outcome = budgeted_blocking(query, move || {
        crate::silent_scan::scan_hits(&q, &c, &piece, from, to)
    })
    .await;
    let hits = match outcome {
        Ok(Ok(hits)) => hits,
        Ok(Err(message)) => {
            writer.line(sp_scan_error_note(sub, message)).await?;
            return Ok(false);
        }
        Err(e) => {
            writer.line(sp_scan_error_note(sub, e.to_string())).await?;
            return Ok(false);
        }
    };
    if !hits.is_empty() {
        writer.line(sp_history_note(sub, 1.0, hits)).await?;
    }
    Ok(true)
}

/// Push the current status of each of `outpoints`.
pub async fn emit_outpoint_notes<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    mempool: Option<Arc<MempoolHub>>,
    outpoints: Vec<([u8; 32], u32)>,
) -> Result<(), std::io::Error> {
    let q = Arc::clone(query);
    let looked = budgeted_blocking(query, move || {
        let mut out = Vec::with_capacity(outpoints.len());
        for (txid, vout) in outpoints {
            let params = json!([txid_hex(&txid), vout]);
            if let Ok(status) = outpoint_status(&q, mempool.as_deref(), &params) {
                out.push((txid, vout, status));
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    for (txid, vout, status) in looked {
        let note = json!({
            "jsonrpc": "2.0",
            "method": "blockchain.outpoint.subscribe",
            "params": [txid_hex(&txid), vout, status],
        });
        writer.line(note).await?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn redact_sp_params(params: &Value) -> String {
    let mut v = params.clone();
    if let Some(arr) = v.as_array_mut() {
        if !arr.is_empty() {
            arr[0] = json!("<redacted>");
        }
    }
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())
}

/// Last height of the scan chunk of `chunk` heights that starts at `from`,
/// at most `last`.
#[doc(hidden)]
pub fn sp_chunk_end(from: u32, last: u32, chunk: u32) -> u32 {
    from.saturating_add(chunk.saturating_sub(1)).min(last)
}

/// A subscription whose history is sent, and the last height scanned for it.
pub struct SpLive {
    pub sub: crate::silent_scan::SpSub,
    pub through: u32,
}

impl SpLive {
    /// First height a tip event still has to scan. Events can be missed or
    /// arrive late, so this is the height after the last scan, not the
    /// event's own. A reorg starts again above its fork.
    fn next_unscanned(&self, tip: &TipNotify) -> u32 {
        let next = self.through.saturating_add(1);
        match tip.reorg_from_height {
            Some(fork) => next.min(fork.saturating_add(1)),
            None => next,
        }
    }
}

/// One scanned chunk of a history scan.
#[doc(hidden)]
pub struct SpScanStep {
    pub hits: Vec<Value>,
    /// Last height scanned so far.
    pub through: u32,
    /// Share of the heights scanned. 1.0 only once `through` is the tip.
    pub progress: f64,
}

/// A scan task that is aborted when its owner drops it.
struct SpScanTask(JoinHandle<usize>);

impl Drop for SpScanTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A history scan running beside the session. Dropping it stops the scan
/// before its next chunk and frees its slot.
pub struct SpScanRun {
    pub sub: crate::silent_scan::SpSub,
    steps: tokio::sync::mpsc::Receiver<Result<SpScanStep, String>>,
    /// When the client last got a note of this scan.
    noted: Instant,
    /// First height whose step the session has not taken.
    #[doc(hidden)]
    pub next: u32,
    chunk: u32,
    task: SpScanTask,
    /// This scan's share of [`ElectrumConfig::sp_scan_slots`].
    slot: tokio::sync::OwnedSemaphorePermit,
}

impl SpScanRun {
    /// Scan `from..=last` for `sub`, `chunk` heights a step.
    #[allow(clippy::too_many_arguments)] // call-site args stay unbundled
    fn start(
        query: &Arc<Query>,
        chain: &Arc<ChainParams>,
        sub: crate::silent_scan::SpSub,
        from: u32,
        last: u32,
        chunk: u32,
        slot: tokio::sync::OwnedSemaphorePermit,
    ) -> Self {
        let (steps, task) = Self::spawn(query, chain, &sub, from, last, chunk);
        Self {
            sub,
            steps,
            noted: Instant::now(),
            next: from,
            chunk,
            task,
            slot,
        }
    }

    /// Scan again from `from`, in the same slot. The old task is aborted
    /// and a step it had queued is dropped.
    fn restart(&mut self, query: &Arc<Query>, chain: &Arc<ChainParams>, from: u32, last: u32) {
        let (steps, task) = Self::spawn(query, chain, &self.sub, from, last, self.chunk);
        self.steps = steps;
        self.task = task;
        self.next = from;
    }

    fn spawn(
        query: &Arc<Query>,
        chain: &Arc<ChainParams>,
        sub: &crate::silent_scan::SpSub,
        from: u32,
        last: u32,
        chunk: u32,
    ) -> (
        tokio::sync::mpsc::Receiver<Result<SpScanStep, String>>,
        SpScanTask,
    ) {
        // One step of slack: the scan reads the next chunk while the session
        // writes this one, and no further ahead of a slow client.
        let (tx, steps) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(run_sp_scan(
            Arc::clone(query),
            Arc::clone(chain),
            sub.clone(),
            from,
            last,
            chunk,
            tx,
        ));
        (steps, SpScanTask(task))
    }
}

/// The address of the first scan with a step ready, and that step: a chunk,
/// the scan error, or `None` when the scan task died. Pending while no scan
/// has one. Cancel safe: a step stays queued until this returns it.
#[doc(hidden)]
pub async fn next_sp_scan_step(
    scans: &mut [SpScanRun],
) -> (String, Option<Result<SpScanStep, String>>) {
    std::future::poll_fn(|cx| {
        for run in scans.iter_mut() {
            if let std::task::Poll::Ready(step) = run.steps.poll_recv(cx) {
                return std::task::Poll::Ready((run.sub.address.clone(), step));
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// A reorg replaced the heights above its fork, and a running scan may have
/// read them. Each scan starts again at the first replaced height, or at
/// the first height it has not sent when that is lower: a step still in
/// flight is dropped with the old task.
#[doc(hidden)]
pub fn restart_sp_scans_after_reorg(
    scans: &mut [SpScanRun],
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    tip: &TipNotify,
) {
    let Some(fork) = tip.reorg_from_height else {
        return;
    };
    for run in scans.iter_mut() {
        let from = run.next.min(fork.saturating_add(1)).max(run.sub.start);
        // A range that ended above the new tip ends at the tip: the heights
        // above it are gone.
        let last = run
            .sub
            .end
            .map_or(tip.height, |end| end.min(tip.height))
            .max(from);
        run.restart(query, chain, from, last);
    }
}

/// Scan `from..=last`, `chunk` heights a step. A start with no end goes on
/// to the tip while it keeps moving. Stops when the receiver is gone.
/// Returns the chunks scanned.
#[doc(hidden)]
pub async fn run_sp_scan(
    query: Arc<Query>,
    chain: Arc<ChainParams>,
    sub: crate::silent_scan::SpSub,
    mut from: u32,
    mut last: u32,
    chunk: u32,
    steps: tokio::sync::mpsc::Sender<Result<SpScanStep, String>>,
) -> usize {
    use crate::silent_scan::SP_SCAN_PERMITS;
    static PERMITS: Semaphore = Semaphore::const_new(SP_SCAN_PERMITS);
    let mut chunks = 0usize;
    loop {
        let end = sp_chunk_end(from, last, chunk);
        let permit = tokio::select! {
            permit = PERMITS.acquire() => permit,
            _ = steps.closed() => return chunks,
        };
        let Ok(permit) = permit else {
            return chunks;
        };
        let (q, c, piece) = (Arc::clone(&query), Arc::clone(&chain), sub.clone());
        let outcome = budgeted_blocking(&query, move || {
            let _sp = permit;
            crate::silent_scan::scan_hits(&q, &c, &piece, from, end)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let hits = match outcome {
            Ok(hits) => hits,
            Err(message) => {
                let _ = steps.send(Err(message)).await;
                return chunks;
            }
        };
        chunks += 1;
        if end == last && sub.end.is_none() {
            // Blocks connected since `last` was read are history too.
            last = query.tip_height().map_or(last, |tip| tip.0.max(last));
        }
        let progress = if end == last {
            1.0
        } else {
            f64::from(end - sub.start + 1) / (f64::from(last - sub.start) + 1.0)
        };
        let step = SpScanStep {
            hits,
            through: end,
            progress,
        };
        if steps.send(Ok(step)).await.is_err() || end == last {
            return chunks;
        }
        from = end + 1;
    }
}

/// Frigate asks for a note with an empty history at regular intervals.
const SP_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Write one step of the history scan for `address`. The step at the tip
/// makes the subscription live.
#[doc(hidden)]
pub async fn write_sp_scan_step<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &ChainParams,
    conn: &mut ElectrumConn,
    address: &str,
    step: Option<Result<SpScanStep, String>>,
) -> Result<(), std::io::Error> {
    let Some(run) = conn
        .sp_scans
        .iter_mut()
        .find(|run| run.sub.address == address)
    else {
        return Ok(());
    };
    let step = match step {
        Some(Ok(step)) => step,
        failed => {
            let message = match failed {
                Some(Err(message)) => message,
                _ => "silent payment scan stopped".to_string(),
            };
            let note = sp_scan_error_note(&run.sub, message);
            drop_sp_address(conn, address);
            return writer.line(note).await;
        }
    };
    run.next = step.through.saturating_add(1);
    let done = step.progress >= 1.0;
    if done || !step.hits.is_empty() || run.noted.elapsed() >= SP_PROGRESS_INTERVAL {
        writer
            .line(sp_history_note(&run.sub, step.progress, step.hits))
            .await?;
        run.noted = Instant::now();
    }
    if !done {
        return Ok(());
    }
    let mut live = SpLive {
        sub: run.sub.clone(),
        through: step.through,
    };
    drop_sp_address(conn, address);
    let tip = query.tip_height().map_or(live.through, |tip| tip.0);
    // A block whose tip event came before this step had no subscription.
    // Read what connected since, up to the end of a closed range: a scan
    // that a reorg cut short ends below it. The heights above a range were
    // not asked for.
    let wanted = live.sub.end.map_or(tip, |end| end.min(tip));
    if wanted > live.through
        && !emit_sp_tip(writer, query, chain, &live.sub, live.through + 1, wanted).await?
    {
        return Ok(());
    }
    // Also down: a step that finished above the chain tip scanned heights
    // a reorg removed, and the blocks that connect there are not scanned yet.
    live.through = tip;
    conn.sp_subs.push(live);
    Ok(())
}

/// Frigate notification: positional `[subscription, progress, history]`.
fn sp_history_note(sub: &crate::silent_scan::SpSub, progress: f64, history: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "blockchain.silentpayments.subscribe",
        "params": [crate::silent_scan::subscribe_result(sub), progress, history],
    })
}

fn sp_scan_error_note(sub: &crate::silent_scan::SpSub, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "blockchain.silentpayments.subscribe",
        "params": {
            "subscription": crate::silent_scan::subscribe_result(sub),
            "error": {"code": 1, "message": message}
        }
    })
}

#[allow(clippy::too_many_arguments)] // matches handle_client call-site
pub async fn serve_tweaks_subscribe<R, W>(
    reader: &mut R,
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    peer: &SocketAddr,
    id: Value,
    params_v: &Value,
    max_line: usize,
    min_dust: u64,
) -> Result<(), std::io::Error>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    let req = match crate::tweaks::parse_req(params_v) {
        Ok(r) => r,
        Err(e) => {
            tweaks_sub_err(writer, peer, &id, params_v, 0, &e).await?;
            return Ok(());
        }
    };
    let tip = query.tip_height().map(|h| h.0);
    let last = crate::tweaks::last_height(req.start, req.count, tip);
    let t0 = Instant::now();
    let Some(last) = last else {
        return serve_tweaks_empty_last(
            writer, query, chain, peer, &id, params_v, req, t0, min_dust, tip,
        )
        .await;
    };
    let limits = crate::tweaks::subscribe_serve_limits(req.historical);
    let q = Arc::clone(query);
    let c = Arc::clone(chain);
    let start_h = req.start;
    let q_read = Arc::clone(&q);
    let c_read = Arc::clone(&c);
    let wave_fut = budgeted_blocking(&q, move || {
        crate::tweaks::first_subscribe_wave(&q_read, &c_read, start_h, last, limits, min_dust)
    });
    let wave = match wave_fut.await.unwrap_or_else(|e| Err(e.to_string())) {
        Ok(v) => v,
        Err(e) => {
            tweaks_sub_err(
                writer,
                peer,
                &id,
                params_v,
                t0.elapsed().as_millis() as u64,
                &e,
            )
            .await?;
            return Ok(());
        }
    };
    let wall_ms = t0.elapsed().as_millis() as u64;
    meter_dispatch_wall(t0.elapsed().as_micros() as u64);
    tweaks_sub_ok_log(peer, params_v, wall_ms);
    writer.rpc_result(&id, &wave.result_json).await?;
    writer.raw_lines(&wave.rest_notifies).await?;
    let mut next = req.start.saturating_add(wave.consumed.max(1));
    let limits = crate::tweaks::subscribe_serve_limits(req.historical);
    serve_tweaks_rest(
        reader, writer, query, chain, peer, last, limits, min_dust, max_line, t0, &mut next,
    )
    .await
}

/// After wave 0: more waves while the chunk is open, then Cake `done`.
#[allow(clippy::too_many_arguments)]
pub async fn serve_tweaks_rest<R, W>(
    reader: &mut R,
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    peer: &SocketAddr,
    last: u32,
    limits: rbitcoin_query::ThinTweakRangeLimits,
    min_dust: u64,
    max_line: usize,
    t0: Instant,
    next: &mut u32,
) -> Result<(), std::io::Error>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    let more = *next <= last;
    if crate::tweaks::chunk_still_open(t0.elapsed(), more) {
        serve_tweaks_follow_waves(
            reader, writer, query, chain, peer, last, limits, min_dust, max_line, t0, next,
        )
        .await?;
    }
    writer.line(crate::tweaks::done_notify()).await?;
    Ok(())
}

pub async fn tweaks_sub_err<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    peer: &SocketAddr,
    id: &Value,
    params_v: &Value,
    wall_ms: u64,
    e: &str,
) -> Result<(), std::io::Error> {
    rbitcoin_log::api_call(
        "electrum",
        &peer.to_string(),
        "blockchain.tweaks.subscribe",
        &serde_json::to_string(params_v).unwrap_or_else(|_| "[]".into()),
        wall_ms,
        Some(e),
    );
    writer
        .line(json!({"jsonrpc":"2.0","id": id, "error": {"code": 1, "message": e}}))
        .await
}

#[doc(hidden)]
pub fn tweaks_sub_ok_log(peer: &SocketAddr, params_v: &Value, wall_ms: u64) {
    rbitcoin_log::api_call(
        "electrum",
        &peer.to_string(),
        "blockchain.tweaks.subscribe",
        &serde_json::to_string(params_v).unwrap_or_else(|_| "[]".into()),
        wall_ms,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_tweaks_empty_last<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    peer: &SocketAddr,
    id: &Value,
    params_v: &Value,
    req: crate::tweaks::TweakReq,
    t0: Instant,
    min_dust: u64,
    tip: Option<u32>,
) -> Result<(), std::io::Error> {
    let first =
        // `tip` is the read that already put `start` past the chain. A later
        // block must not build a map, so cut-through is not consulted.
        match crate::tweaks::height_map_json_with_tip(
            query, chain, req.start, tip, false, min_dust,
        ) {
            Ok(v) => v,
            Err(e) => {
                tweaks_sub_err(
                    writer,
                    peer,
                    id,
                    params_v,
                    t0.elapsed().as_millis() as u64,
                    &e,
                )
                .await?;
                return Ok(());
            }
        };
    serve_tweaks_empty_ok(writer, peer, id, params_v, t0, &first).await
}

pub async fn serve_tweaks_empty_ok<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    peer: &SocketAddr,
    id: &Value,
    params_v: &Value,
    t0: Instant,
    first: &str,
) -> Result<(), std::io::Error> {
    let wall_ms = t0.elapsed().as_millis() as u64;
    meter_dispatch_wall(t0.elapsed().as_micros() as u64);
    tweaks_sub_ok_log(peer, params_v, wall_ms);
    writer.rpc_result(id, first).await?;
    writer.line(crate::tweaks::done_notify()).await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_tweaks_follow_waves<R, W>(
    reader: &mut R,
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    chain: &Arc<ChainParams>,
    peer: &SocketAddr,
    last: u32,
    limits: rbitcoin_query::ThinTweakRangeLimits,
    min_dust: u64,
    max_line: usize,
    t0: Instant,
    next: &mut u32,
) -> Result<(), std::io::Error>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    let spawn_wave = |batch_start: u32| {
        let q = Arc::clone(query);
        let c = Arc::clone(chain);
        let lim = limits;
        let last_h = last;
        tokio::spawn(async move {
            let permit = q
                .api_read_budget()
                .acquire_owned()
                .await
                .expect("store budget");
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _g = BlockingRegion::enter();
                crate::tweaks::remaining_notify_lines(&q, &c, batch_start, last_h, lim, min_dust)
            })
            .await
            {
                Ok(v) => v,
                Err(e) => Err(e.to_string()),
            }
        })
    };
    let mut batch_start = *next;
    let mut handle = spawn_wave(batch_start);
    loop {
        let batch = loop {
            match tweaks_wait_wave_or_ping(reader, writer, max_line, &mut handle).await? {
                TweaksWait::Wave(map) => break map,
                TweaksWait::Continue => continue,
                TweaksWait::Eof => return Ok(()),
            }
        };
        let batch = match batch {
            Ok(v) => v,
            Err(e) => {
                rbitcoin_log::api_call(
                    "electrum",
                    &peer.to_string(),
                    "blockchain.tweaks.subscribe",
                    &format!("[{batch_start},batch]"),
                    0,
                    Some(&e),
                );
                break;
            }
        };
        let n = batch.consumed.max(1);
        *next = batch_start.saturating_add(n);
        let more = *next <= last;
        let open = crate::tweaks::chunk_still_open(t0.elapsed(), more);
        if open {
            handle = spawn_wave(*next);
        }
        writer.raw_lines(&batch.lines).await?;
        if !open {
            break;
        }
        batch_start = *next;
    }
    Ok(())
}

#[doc(hidden)]
pub enum TweaksWait {
    Wave(Result<crate::tweaks::NotifyWave, String>),
    Continue,
    Eof,
}

pub async fn tweaks_wait_wave_or_ping<R, W>(
    reader: &mut R,
    writer: &mut ConnWrite<W>,
    max_line: usize,
    handle: &mut tokio::task::JoinHandle<Result<crate::tweaks::NotifyWave, String>>,
) -> Result<TweaksWait, std::io::Error>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    let idle = writer.idle();
    tokio::select! {
        biased;
        line = tokio::time::timeout(idle, read_line_capped(reader, max_line)) => {
            match line {
                Ok(Ok(Some(l))) => {
                    if !l.trim().is_empty() {
                        if let Ok(req) = serde_json::from_str::<Value>(&l) {
                            let ping_id = req.get("id").cloned().unwrap_or(Value::Null);
                            if req.get("method").and_then(|m| m.as_str()) == Some("server.ping") {
                                writer
                                    .line(json!({"jsonrpc":"2.0","id": ping_id, "result": null}))
                                    .await?;
                            }
                        }
                    }
                    Ok(TweaksWait::Continue)
                }
                Ok(Ok(None)) => Ok(TweaksWait::Eof),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "idle timeout",
                )),
            }
        }
        map = &mut *handle => {
            Ok(TweaksWait::Wave(map.unwrap_or_else(|e| Err(e.to_string()))))
        }
    }
}

/// One connection's writes. Each write takes a fresh `idle` timeout.
#[doc(hidden)]
pub struct ConnWrite<W> {
    inner: W,
    idle: Duration,
}

impl<W> ConnWrite<W> {
    pub fn new(inner: W, idle: Duration) -> Self {
        Self { inner, idle }
    }

    pub fn idle(&self) -> Duration {
        self.idle
    }
}

impl<W: AsyncWrite + Unpin> ConnWrite<W> {
    pub async fn line(&mut self, msg: Value) -> Result<(), std::io::Error> {
        write_line(&mut self.inner, msg, self.idle).await
    }

    pub async fn bytes(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        write_bytes_within(&mut self.inner, bytes, self.idle).await
    }

    pub async fn raw_lines(&mut self, lines: &[String]) -> Result<(), std::io::Error> {
        write_raw_lines(&mut self.inner, lines, self.idle).await
    }

    pub async fn rpc_result(
        &mut self,
        id: &Value,
        result_json: &str,
    ) -> Result<(), std::io::Error> {
        write_rpc_result(&mut self.inner, id, result_json, self.idle).await
    }
}

pub async fn write_bytes_within<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    deadline: Duration,
) -> Result<(), std::io::Error> {
    let write = async {
        writer.write_all(bytes).await?;
        writer.flush().await?;
        Ok(())
    };
    match tokio::time::timeout(deadline, write).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "write timeout",
        )),
    }
}

pub async fn write_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: Value,
    deadline: Duration,
) -> Result<(), std::io::Error> {
    let mut s = crate::json_cpu::encode_json_body(Arc::new(msg))
        .await
        .map_err(|_| std::io::Error::other("json encode join failed"))?;
    s.push(b'\n');
    write_bytes_within(writer, &s, deadline).await
}

#[doc(hidden)]
pub fn restatus_notes(
    query: &Query,
    mempool: Option<&MempoolHub>,
    subs: &[[u8; 32]],
    heights: Option<&[u32]>,
) -> Vec<([u8; 32], String)> {
    let mut out = Vec::new();
    // Each height's touch set is built at most once, and only when a sub with
    // postings needs it: a hash with no history can't be touched, so a
    // connection watching only fresh addresses never loads the block's
    // prevouts. (Probing per sub reloaded the block and its prevouts for each
    // subscribed hash with history.) A height whose set fails to load counts
    // as untouched, as the per-sub probe's `.ok().unwrap_or(false)` did.
    let mut touches: Option<Vec<rbitcoin_query::BlockTouch>> = None;
    for sh in subs {
        let hit = match heights {
            None => true,
            Some(hs) => {
                if !query.scripthash_has_postings(sh).unwrap_or(false) {
                    false
                } else {
                    let ts = touches.get_or_insert_with(|| {
                        hs.iter()
                            .filter_map(|h| query.block_touch(Height(*h)).ok())
                            .collect()
                    });
                    ts.iter()
                        .any(|t| query.scripthash_touched_by(sh, t).ok().unwrap_or(false))
                }
            }
        };
        if !hit {
            continue;
        }
        let status = if let Some(mp) = mempool {
            scripthash_status_full(query, mp, sh).ok()
        } else {
            query
                .scripthash_history(sh)
                .ok()
                .map(|h| scripthash_status(&h))
        };
        if let Some(status) = status {
            out.push((*sh, status));
        }
    }
    out
}

/// [`restatus_notes`] read while no connect or reorg holds the pool, else
/// `None`.
#[doc(hidden)]
pub fn settled_restatus_notes(
    query: &Query,
    mempool: Option<&MempoolHub>,
    subs: &[[u8; 32]],
    heights: Option<&[u32]>,
) -> Option<Vec<([u8; 32], String)>> {
    // A degraded or rebuilding index reads as an empty history. Pushing that
    // would tell a wallet its address emptied, so hold instead: the caller
    // reads every sub again once the index serves.
    // A read that a fault or an unlink batch overlapped is held the same way.
    let sh_guard = query.sh_read_enter()?;
    let gen = mempool.map(MempoolHub::pool_settle_gen);
    if gen.is_some_and(|g| g % 2 == 1) {
        return None;
    }
    let notes = restatus_notes(query, mempool, subs, heights);
    (mempool.map(MempoolHub::pool_settle_gen) == gen && sh_guard.still_valid()).then_some(notes)
}

#[doc(hidden)]
pub const TICK_SCAN_MAX_GAP: u32 = 32;

// `seen == now` already returned, so `>=` matches `>`.
#[cfg_attr(feature = "mutants", mutants::skip)]
fn tick_forward(b: u32, a: u32) -> bool {
    b > a
}

#[doc(hidden)]
pub fn tick_scan(seen: Option<u32>, now: Option<u32>) -> Option<Option<Vec<u32>>> {
    if seen == now {
        return None;
    }
    match (seen, now) {
        (Some(a), Some(b)) if tick_forward(b, a) => {
            let gap = b.saturating_sub(a);
            if gap > TICK_SCAN_MAX_GAP {
                Some(None)
            } else {
                Some(Some((a.saturating_add(1)..=b).collect()))
            }
        }
        _ => Some(None),
    }
}

// A same-sized leftover is not pushed. The sweep runs only once the map is larger.
#[cfg_attr(feature = "mutants", mutants::skip)]
fn last_sent_outgrew(last_sent: usize, subs: usize) -> bool {
    last_sent > subs
}

/// Backstop sweep of last-sent statuses for hashes no longer subscribed.
///
/// `scripthash.unsubscribe` already removes its entry exactly (post-dispatch),
/// so this only catches leftovers and only runs when `last_sent` outgrows the
/// sub set. Sweeping unconditionally on every push made each notification
/// O(subs), so a block touching many subs of a large wallet went quadratic.
#[doc(hidden)]
pub fn drop_unsubscribed_status(
    last_sent: &mut HashMap<[u8; 32], String>,
    sh_subs: &HashSet<[u8; 32]>,
) {
    if last_sent_outgrew(last_sent.len(), sh_subs.len()) {
        last_sent.retain(|k, _| sh_subs.contains(k));
    }
}

/// Electrum status on the wire: `null` for a script with no history, else the
/// hex digest. Internally "no history" is the empty string, so dedup compares
/// one type.
#[doc(hidden)]
pub fn status_json(status: String) -> Value {
    if status.is_empty() {
        Value::Null
    } else {
        Value::String(status)
    }
}

/// Remember the status a `scripthash.subscribe` reply sent, so the next push
/// for an unchanged hash (including a still-empty one) is deduplicated.
#[doc(hidden)]
pub fn record_sent_status(last_sent: &mut HashMap<[u8; 32], String>, sh: [u8; 32], sent: &Value) {
    match sent {
        Value::Null => {
            last_sent.insert(sh, String::new());
        }
        Value::String(s) => {
            last_sent.insert(sh, s.clone());
        }
        _ => {}
    }
}

#[doc(hidden)]
pub fn take_new_status(
    last_sent: &mut HashMap<[u8; 32], String>,
    sh_subs: &HashSet<[u8; 32]>,
    sh: [u8; 32],
    status: String,
) -> Option<String> {
    drop_unsubscribed_status(last_sent, sh_subs);
    if last_sent.get(&sh) == Some(&status) {
        return None;
    }
    last_sent.insert(sh, status.clone());
    Some(status)
}

/// Push the statuses that changed. `false` when a connect or reorg held the
/// pool during the read: nothing was pushed, and the caller reads again.
pub async fn emit_sh_notes<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    query: &Arc<Query>,
    mempool: Option<Arc<MempoolHub>>,
    sh_subs: &HashSet<[u8; 32]>,
    last_sent: &mut HashMap<[u8; 32], String>,
    heights: Option<Vec<u32>>,
) -> Result<bool, std::io::Error> {
    let q = Arc::clone(query);
    let subs: Vec<[u8; 32]> = sh_subs.iter().copied().collect();
    let notes = budgeted_blocking(query, move || {
        settled_restatus_notes(&q, mempool.as_deref(), &subs, heights.as_deref())
    })
    .await
    .unwrap_or(Some(Vec::new()));
    let Some(notes) = notes else {
        return Ok(false);
    };
    push_sh_notes(writer, sh_subs, last_sent, notes).await?;
    Ok(true)
}

/// Push each status in `notes` that differs from the last one sent.
#[doc(hidden)]
pub async fn push_sh_notes<W: AsyncWrite + Unpin>(
    writer: &mut ConnWrite<W>,
    sh_subs: &HashSet<[u8; 32]>,
    last_sent: &mut HashMap<[u8; 32], String>,
    notes: Vec<([u8; 32], String)>,
) -> Result<(), std::io::Error> {
    for (sh, status) in notes {
        let Some(status) = take_new_status(last_sent, sh_subs, sh, status) else {
            continue;
        };
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "blockchain.scripthash.subscribe",
            "params": [hash_hex_rev(&sh), status_json(status)]
        });
        writer.line(msg).await?;
    }
    Ok(())
}

pub async fn write_raw_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &str,
    deadline: Duration,
) -> Result<(), std::io::Error> {
    let mut bytes = Vec::with_capacity(msg.len() + 1);
    bytes.extend_from_slice(msg.as_bytes());
    bytes.push(b'\n');
    write_bytes_within(writer, &bytes, deadline).await
}

pub async fn write_raw_lines<W: AsyncWrite + Unpin>(
    writer: &mut W,
    lines: &[String],
    deadline: Duration,
) -> Result<(), std::io::Error> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut n = 0usize;
    for l in lines {
        n = n.saturating_add(l.len()).saturating_add(1);
    }
    let mut buf = Vec::with_capacity(n);
    for l in lines {
        buf.extend_from_slice(l.as_bytes());
        buf.push(b'\n');
    }
    write_bytes_within(writer, &buf, deadline).await
}

pub async fn write_rpc_result<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: &Value,
    result_json: &str,
    deadline: Duration,
) -> Result<(), std::io::Error> {
    let mut s = String::with_capacity(result_json.len() + 48);
    s.push_str("{\"jsonrpc\":\"2.0\",\"id\":");
    s.push_str(&serde_json::to_string(id).unwrap_or_else(|_| "null".into()));
    s.push_str(",\"result\":");
    s.push_str(result_json);
    s.push('}');
    write_raw_line(writer, &s, deadline).await
}

/// Broadcast runs scripts on `rbtc-scripts-*` and must not take a blocking thread.
fn method_awaits_scripts(method: &str) -> bool {
    matches!(
        method,
        "blockchain.transaction.broadcast" | "blockchain.transaction.broadcast_package"
    )
}

/// Instant methods that must stay on the connection task so a 1-worker runtime
/// can answer `server.ping` while another socket is in `spawn_blocking`.
#[doc(hidden)]
pub fn method_stays_on_worker(method: &str) -> bool {
    matches!(
        method,
        "server.ping"
            | "server.version"
            | "server.banner"
            | "server.donation_address"
            | "server.features"
            | "server.peers.subscribe"
            | "blockchain.relayfee"
            | "blockchain.scripthash.unsubscribe"
    )
}

#[doc(hidden)]
pub fn method_needs_sh_index(method: &str) -> bool {
    method.starts_with("blockchain.scripthash.")
        || method.starts_with("blockchain.tweaks.")
        || method.starts_with("blockchain.silentpayments.")
}

/// Chain-tip stamp and a trailing `asof:` share this set. A method that
/// stamps without accepting asof has to split the list on purpose.
#[doc(hidden)]
pub fn method_stamps_chain_tip(method: &str) -> bool {
    matches!(
        method,
        "blockchain.scripthash.get_history"
            | "blockchain.scripthash.get_balance"
            | "blockchain.scripthash.listunspent"
            | "blockchain.transaction.get"
            | "blockchain.transaction.get_merkle"
    )
}

#[doc(hidden)]
pub fn rpc_result(id: &Value, result: &Value, view: Option<&ChainView>) -> Value {
    let mut obj = json!({"jsonrpc":"2.0","id": id, "result": result});
    if let Some(v) = view {
        obj["chain_tip"] = json!(hash_hex_rev(&v.hash));
        obj["chain_tip_height"] = json!(v.height.0);
    }
    obj
}

#[doc(hidden)]
pub fn electrum_pin_kind(method: &str) -> ChainViewKind {
    if method.starts_with("blockchain.scripthash.") {
        ChainViewKind::ScriptHash
    } else {
        ChainViewKind::Tip
    }
}

#[doc(hidden)]
pub fn electrum_at_chain_view<F>(
    query: &Query,
    method: &str,
    params: &Value,
    mut f: F,
) -> (Result<Value, String>, Option<ChainView>)
where
    F: FnMut(&Query, &Value, Option<&ChainView>, bool) -> Result<Value, String>,
{
    // A leftover watermark must not choose the error. `asof:` ahead of that
    // mark is `asof not on chain`; at or behind it the pin succeeds and the
    // later gate says the index is off. Refuse before any pin.
    if method_needs_sh_index(method) && !query.sh_history_available() {
        return (Err(query.sh_unavailable_message().to_string()), None);
    }
    let kind = electrum_pin_kind(method);
    match take_trailing_asof(method, params) {
        Ok((stripped, Some(hash))) => match query.pin_view(kind, Some(&hash)) {
            Ok(Some(view)) => {
                let out = f(query, &stripped, Some(&view), true);
                match view.still_live(query) {
                    Ok(true) => (out, Some(view)),
                    Ok(false) => (Err("asof not on chain".into()), None),
                    Err(e) => (Err(e.to_string()), None),
                }
            }
            Ok(None) => (Err("asof not on chain".into()), None),
            Err(e) => (Err(e.to_string()), None),
        },
        Ok((stripped, None)) => match query.run_at_view(kind, |view| {
            Ok::<_, rbitcoin_query::QueryError>(f(query, &stripped, Some(view), false))
        }) {
            Ok((view, inner)) => (inner, Some(view)),
            Err(StoreError::NotFound) => (f(query, &stripped, None, false), None),
            Err(StoreError::Stale(_)) => (Err("chain view moved".into()), None),
            Err(e) => (Err(e.to_string()), None),
        },
        Err(e) => (Err(e), None),
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn dispatch(
    method: &str,
    params: &Value,
    query: &Query,
    config: &ElectrumConfig,
    chain: &ChainParams,
    mempool: Option<&MempoolHub>,
    header_sub: &mut bool,
    sh_subs: &mut HashSet<[u8; 32]>,
) -> Result<Value, String> {
    let mut conn = ElectrumConn {
        protocol: String::new(),
        header_sub: *header_sub,
        sh_subs: std::mem::take(sh_subs),
        sh_join: None,
        outpoint_subs: HashSet::new(),
        sp_subs: Vec::new(),
        sp_scans: Vec::new(),
    };
    let r = dispatch_with_join(method, params, query, config, chain, mempool, &mut conn);
    *header_sub = conn.header_sub;
    *sh_subs = conn.sh_subs;
    r
}

#[doc(hidden)]
pub fn dispatch_with_join(
    method: &str,
    params: &Value,
    query: &Query,
    config: &ElectrumConfig,
    chain: &ChainParams,
    mempool: Option<&MempoolHub>,
    conn: &mut ElectrumConn,
) -> Result<Value, String> {
    if method == "server.version" {
        if conn.protocol.is_empty() {
            conn.protocol = negotiate_protocol(params)?;
        }
        return Ok(json!([SERVER_VERSION, conn.protocol.as_str()]));
    }
    dispatch_pinned(
        method, params, query, config, chain, mempool, conn, None, false,
    )
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
#[doc(hidden)]
pub fn sh_at_view<T>(
    query: &Query,
    pinned: Option<&ChainView>,
    is_asof: bool,
    asof: Option<[u8; 32]>,
    sh_join: &mut Option<Arc<ShJoinSlot>>,
    asof_fn: impl FnOnce(&Query, &ChainView) -> Result<T, String>,
    slot_fn: impl FnOnce(&Query, &mut Option<Arc<ShJoinSlot>>, &ChainView) -> Result<T, String>,
    live_fn: impl FnOnce(&Query, &mut Option<Arc<ShJoinSlot>>) -> Result<T, String>,
) -> Result<T, String> {
    if let Some(view) = pinned {
        if is_asof {
            return asof_fn(query, view);
        }
        return slot_fn(query, sh_join, view);
    }
    if let Some(hash) = asof {
        let view = query
            .pin_sh_chain_view_at(&hash)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "asof not on chain".to_string())?;
        return asof_fn(query, &view);
    }
    live_fn(query, sh_join)
}

#[doc(hidden)]
pub fn features_hosts_json(config: &ElectrumConfig) -> Value {
    match config.onion_tcp.get() {
        Some((host, port)) => json!({ host: { "tcp_port": port } }),
        None => json!({}),
    }
}

fn decode_broadcast_tx(raw_hex: &str) -> Result<bitcoin::Transaction, String> {
    let raw = decode_hex_bytes(raw_hex)?;
    if raw.len() > 4_000_000 {
        return Err("transaction too large".into());
    }
    bitcoin::consensus::deserialize(&raw).map_err(|e| e.to_string())
}

fn broadcast_tx_from_params(
    params: &Value,
    config: &ElectrumConfig,
) -> Result<bitcoin::Transaction, String> {
    let raw_hex = param_str(params, 0)?;
    if raw_hex.len() > config.max_broadcast_hex {
        return Err(format!(
            "transaction hex too large (max {} chars)",
            config.max_broadcast_hex
        ));
    }
    decode_broadcast_tx(raw_hex)
}

/// Hex array plus the verbose flag. One size check for sync and async broadcast.
fn decode_broadcast_package(
    params: &Value,
    config: &ElectrumConfig,
) -> Result<(Vec<bitcoin::Transaction>, bool), String> {
    let arr = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_array())
        .ok_or_else(|| "broadcast_package expected array of hex txs".to_string())?;
    let verbose = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut txs = Vec::with_capacity(arr.len());
    let mut total_hex = 0usize;
    for h in arr {
        let raw_hex = h
            .as_str()
            .ok_or_else(|| "broadcast_package tx must be hex".to_string())?;
        total_hex = total_hex.saturating_add(raw_hex.len());
        if total_hex > config.max_broadcast_hex {
            return Err("package hex too large".into());
        }
        txs.push(decode_broadcast_tx(raw_hex)?);
    }
    Ok((txs, verbose))
}

fn package_broadcast_value(verbose: bool, accepted: &[rbitcoin_net::AcceptResult]) -> Value {
    if !verbose {
        return json!("success");
    }
    let mut tx_results = serde_json::Map::new();
    for r in accepted {
        tx_results.insert(
            r.txid.to_string(),
            json!({"txid": r.txid.to_string(), "allowed": true}),
        );
    }
    json!({
        "package_msg": "success",
        "tx-results": tx_results,
    })
}

#[doc(hidden)]
pub fn broadcast_raw_tx(
    params: &Value,
    config: &ElectrumConfig,
    mempool: Option<&MempoolHub>,
) -> Result<Value, String> {
    let tx = broadcast_tx_from_params(params, config)?;
    let mp = mempool.ok_or_else(|| "mempool not available".to_string())?;
    let r = mp
        .accept_tx_from(&tx, rbitcoin_net::TxOrigin::Local)
        .map_err(|e| format!("broadcast reject: {e}"))?;
    Ok(json!(format!("{}", r.txid)))
}

/// [`broadcast_raw_tx`] with script verify awaited on the connection task.
pub async fn broadcast_raw_tx_async(
    params: &Value,
    config: &ElectrumConfig,
    mempool: Option<&Arc<MempoolHub>>,
) -> Result<Value, String> {
    let tx = broadcast_tx_from_params(params, config)?;
    let mp = mempool
        .ok_or_else(|| "mempool not available".to_string())?
        .clone();
    let budget = mp.query.api_read_budget();
    let r = rbitcoin_net::with_store_budget(
        budget,
        mp.accept_tx_from_async(tx, rbitcoin_net::TxOrigin::Local),
    )
    .await
    .map_err(|e| format!("broadcast reject: {e}"))?;
    Ok(json!(format!("{}", r.txid)))
}

#[doc(hidden)]
pub fn broadcast_package(
    params: &Value,
    config: &ElectrumConfig,
    mempool: Option<&MempoolHub>,
) -> Result<Value, String> {
    let (txs, verbose) = decode_broadcast_package(params, config)?;
    let mp = mempool.ok_or_else(|| "mempool not available".to_string())?;
    let accepted = mp
        .accept_package(&txs)
        .map_err(|e| format!("broadcast_package reject: {e}"))?;
    Ok(package_broadcast_value(verbose, &accepted))
}

/// [`broadcast_package`] with each member's scripts awaited on the connection task.
pub async fn broadcast_package_async(
    params: &Value,
    config: &ElectrumConfig,
    mempool: Option<&Arc<MempoolHub>>,
) -> Result<Value, String> {
    let (txs, verbose) = decode_broadcast_package(params, config)?;
    let mp = mempool
        .ok_or_else(|| "mempool not available".to_string())?
        .clone();
    let budget = mp.query.api_read_budget();
    let accepted = rbitcoin_net::with_store_budget(budget, mp.accept_package_async(txs))
        .await
        .map_err(|e| format!("broadcast_package reject: {e}"))?;
    Ok(package_broadcast_value(verbose, &accepted))
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
#[doc(hidden)]
pub fn dispatch_pinned(
    method: &str,
    params: &Value,
    query: &Query,
    config: &ElectrumConfig,
    chain: &ChainParams,
    mempool: Option<&MempoolHub>,
    conn: &mut ElectrumConn,
    pinned: Option<&ChainView>,
    is_asof: bool,
) -> Result<Value, String> {
    // Held for the whole dispatch: the rebuild's wipe waits for it. A read a
    // fault, a rebuild step or an unlink batch overlapped is not an answer.
    let sh_guard = if method_needs_sh_index(method) {
        match query.sh_read_enter() {
            Some(g) => Some(g),
            None => return Err(query.sh_unavailable_message().to_string()),
        }
    } else {
        None
    };
    let out = dispatch_method(
        method, params, query, config, chain, mempool, conn, pinned, is_asof,
    );
    match sh_guard {
        Some(g) if !g.still_valid() => Err(query.sh_unavailable_message().to_string()),
        _ => out,
    }
}

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
fn dispatch_method(
    method: &str,
    params: &Value,
    query: &Query,
    config: &ElectrumConfig,
    chain: &ChainParams,
    mempool: Option<&MempoolHub>,
    conn: &mut ElectrumConn,
    pinned: Option<&ChainView>,
    is_asof: bool,
) -> Result<Value, String> {
    let protocol = conn.protocol.clone();
    let header_sub = &mut conn.header_sub;
    let sh_subs = &mut conn.sh_subs;
    let sh_join = &mut conn.sh_join;
    match method {
        "server.version" => Ok(json!([SERVER_VERSION, protocol])),
        "server.ping" => Ok(Value::Null),
        "server.banner" => Ok(json!(config.banner)),
        "server.donation_address" => Ok(json!(config.donation_address)),
        "server.features" => Ok(json!({
            "genesis_hash": config.genesis_hash_hex,
            "hosts": features_hosts_json(config),
            "protocol_max": PROTOCOL_MAX,
            "protocol_min": PROTOCOL_MIN,
            "server_version": SERVER_VERSION,
            "hash_function": "sha256",
            "pruning": null,
            // Cake gates SP on version[0] containing "electrs", then probes the
            // tweaks method — not features. Other clients (and future Cake) can
            // still see SP here without a dummy RPC. Cake electrs does not
            // implement server.features.
            "silent_payments": [0],
            "tweaks": true,
            "chain_tip": true,
            "asof": true,
            "asof_protocol": PROTOCOL_ASOF,
        })),
        "blockchain.headers.subscribe" => {
            *header_sub = true;
            tip_header_obj(query)
        }
        "blockchain.block.header" => {
            let height = param_u32(params, 0)?;
            let hdr = query
                .wire_header_at_height(Height(height))
                .map_err(|e| e.to_string())?;
            Ok(json!(header_hex(&hdr)))
        }
        "blockchain.block.headers" => {
            let start = param_u32(params, 0)?;
            let count = param_u32(params, 1)?.min(2016);
            let mut hexes = Vec::new();
            for h in start..start.saturating_add(count) {
                match query.wire_header_at_height(Height(h)) {
                    Ok(hdr) => hexes.push(header_hex(&hdr)),
                    Err(_) => break,
                }
            }
            let n = hexes.len() as u32;
            if protocol_ge_1_6(&protocol) {
                Ok(json!({"count": n, "headers": hexes, "max": 2016}))
            } else {
                Ok(json!({"count": n, "hex": hexes.concat(), "max": 2016}))
            }
        }
        "blockchain.scripthash.get_history" => {
            let (params, asof) = if pinned.is_some() {
                (params.clone(), None)
            } else {
                take_trailing_asof(method, params)?
            };
            let sh = param_scripthash(&params, 0)?;
            let (filter, mut include_mempool) = parse_get_history_window(&params)?;
            if is_asof || asof.is_some() {
                include_mempool = false;
            }
            let mut hist = sh_at_view(
                query,
                pinned,
                is_asof,
                asof,
                sh_join,
                |q, view| {
                    q.scripthash_history_filtered_in(&sh, &filter, view)
                        .map(|page| page.rows)
                        .map_err(|e| e.to_string())
                },
                |q, slot, view| {
                    q.scripthash_history_filtered_slot_in(&sh, &filter, slot, view)
                        .map(|page| page.rows)
                        .map_err(|e| e.to_string())
                },
                |q, slot| {
                    q.scripthash_history_filtered_slot(&sh, &filter, slot)
                        .map(|page| page.rows)
                        .map_err(|e| e.to_string())
                },
            )?;
            // Confirmed rows are height-asc from the filter. Mempool (if any) is
            // appended as a tail — Electrum Cash: only when to_height is -1/omitted.
            if include_mempool {
                if let Some(mp) = mempool {
                    append_mempool_history(&mut hist, mp, &sh);
                }
            }
            let arr: Vec<Value> = hist.iter().map(history_row_json).collect();
            Ok(Value::Array(arr))
        }
        "blockchain.scripthash.get_balance" => {
            let (params, asof) = if pinned.is_some() {
                (params.clone(), None)
            } else {
                take_trailing_asof(method, params)?
            };
            let sh = param_scripthash(&params, 0)?;
            let mut b = sh_at_view(
                query,
                pinned,
                is_asof,
                asof,
                sh_join,
                |q, view| {
                    q.scripthash_balance_in(&sh, view)
                        .map_err(|e| e.to_string())
                },
                |q, slot, view| {
                    q.scripthash_balance_slot_in(&sh, slot, view)
                        .map_err(|e| e.to_string())
                },
                |q, slot| {
                    q.scripthash_balance_slot(&sh, slot)
                        .map_err(|e| e.to_string())
                },
            )?;
            if !is_asof && asof.is_none() {
                if let Some(mp) = mempool {
                    b.unconfirmed = mp
                        .scripthash_unconfirmed_delta(&sh)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(json!({"confirmed": b.confirmed, "unconfirmed": b.unconfirmed}))
        }
        "blockchain.scripthash.listunspent" => {
            let (params, asof) = if pinned.is_some() {
                (params.clone(), None)
            } else {
                take_trailing_asof(method, params)?
            };
            let sh = param_scripthash(&params, 0)?;
            let u = sh_at_view(
                query,
                pinned,
                is_asof,
                asof,
                sh_join,
                |q, view| {
                    q.scripthash_listunspent_in(&sh, view)
                        .map_err(|e| e.to_string())
                },
                |q, slot, view| {
                    crate::unspent::scripthash_utxos_with_mempool_slot_in(
                        q, mempool, &sh, slot, view,
                    )
                    .map_err(|e| e.to_string())
                },
                |q, slot| {
                    crate::unspent::scripthash_utxos_with_mempool_slot(q, mempool, &sh, slot)
                        .map_err(|e| e.to_string())
                },
            )?;
            let arr: Vec<Value> = u
                .iter()
                .map(|x| {
                    json!({
                        "tx_hash": txid_hex(&x.tx_hash),
                        "tx_pos": x.tx_pos,
                        "height": x.height,
                        "value": x.value,
                    })
                })
                .collect();
            Ok(Value::Array(arr))
        }
        "blockchain.scripthash.subscribe" => {
            let sh = param_scripthash(params, 0)?;
            if !sh_subs.contains(&sh) && sh_subs.len() >= config.max_scripthash_subs {
                return Err(format!(
                    "too many scripthash subscriptions (max {})",
                    config.max_scripthash_subs
                ));
            }
            sh_subs.insert(sh);
            let status = if let Some(mp) = mempool {
                scripthash_status_full_slot(query, mp, &sh, sh_join)?
            } else {
                let hist = query
                    .scripthash_history_slot(&sh, sh_join)
                    .map_err(|e| e.to_string())?;
                scripthash_status(&hist)
            };
            Ok(status_json(status))
        }
        "blockchain.scripthash.unsubscribe" => {
            let sh = param_scripthash(params, 0)?;
            Ok(json!(sh_subs.remove(&sh)))
        }
        "blockchain.scripthash.get_mempool" => {
            let sh = param_scripthash(params, 0)?;
            let items = mempool
                .map(|m| m.scripthash_mempool(&sh))
                .unwrap_or_default();
            let visible = if items.is_empty() {
                None
            } else {
                query.sh_visible_through_height()
            };
            let mut arr = Vec::with_capacity(items.len());
            for i in items {
                if query
                    .sh_visible_confirm(&i.txid, visible)
                    .map_err(|e| e.to_string())?
                    == rbitcoin_query::ShVisibleConfirm::Covered
                {
                    continue;
                }
                arr.push(json!({
                    "height": i.height,
                    "tx_hash": txid_hex(&i.txid),
                    "fee": i.fee,
                }));
            }
            Ok(Value::Array(arr))
        }
        "blockchain.transaction.get" => {
            let txid = param_txid(params, 0)?;
            let verbose = params
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(fk) = query.tx_fk_by_txid(&txid).map_err(|e| e.to_string())? {
                let confirmed_ok = if is_asof {
                    let view = pinned.ok_or_else(|| "asof not on chain".to_string())?;
                    query
                        .store()
                        .is_confirmed_strong_at(fk, Some(view.height.0))
                        .map_err(|e| e.to_string())?
                } else {
                    true
                };
                if confirmed_ok {
                    let map_missing = |e: StoreError| {
                        if matches!(e, StoreError::Pruned { .. }) {
                            "pruned".to_string()
                        } else {
                            e.to_string()
                        }
                    };
                    if verbose {
                        let tx = query.reconstruct_tx(fk).map_err(map_missing)?;
                        return Ok(verbose_tx_from_tx(
                            query,
                            &tx,
                            &txid,
                            Some(fk),
                            chain.network,
                        ));
                    }
                    let raw = query.tx_wire_bytes(fk).map_err(map_missing)?;
                    return Ok(json!(rbitcoin_primitives::hex_encode(&raw)));
                }
            }
            if !is_asof {
                if let Some(mp) = mempool {
                    use bitcoin::hashes::Hash;
                    let tid = bitcoin::Txid::from_byte_array(txid);
                    if let Some(tx) = mp.get_tx(&tid) {
                        if verbose {
                            return Ok(verbose_tx_from_tx(query, &tx, &txid, None, chain.network));
                        }
                        let raw = bitcoin::consensus::serialize(&tx);
                        return Ok(json!(rbitcoin_primitives::hex_encode(&raw)));
                    }
                }
            }
            Err("tx not found".into())
        }
        "blockchain.transaction.get_merkle" => {
            let txid = param_txid(params, 0)?;
            let height = param_u32(params, 1)?;
            if let Some(view) = pinned {
                if height > view.height.0 {
                    return Err("asof not on chain".into());
                }
            }
            let proof = query
                .merkle_proof(Height(height), &txid)
                .map_err(|e| e.to_string())?;
            let merkle: Vec<String> = proof.merkle.iter().map(hash_hex_rev).collect();
            Ok(json!({
                "block_height": proof.block_height,
                "merkle": merkle,
                "pos": proof.pos,
            }))
        }
        "blockchain.transaction.broadcast" => broadcast_raw_tx(params, config, mempool),
        "blockchain.transaction.broadcast_package" => broadcast_package(params, config, mempool),
        "mempool.get_info" => {
            let inc = MempoolHub::relay_fee_btc_per_kb();
            let min_relay = mempool
                .map(|m| m.min_relay_sat_kvb() as f64 / 100_000_000.0)
                .unwrap_or(inc);
            let floor = mempool
                .map(|m| m.fee_floor_sat_kvb() as f64 / 100_000_000.0)
                .unwrap_or(inc);
            let unbroadcast = mempool.map(|m| m.unbroadcast_count()).unwrap_or(0);
            Ok(json!({
                "minrelaytxfee": min_relay,
                "mempoolminfee": floor,
                "incrementalrelayfee": inc,
                "unbroadcastcount": unbroadcast,
            }))
        }
        "blockchain.outpoint.get_status" => outpoint_status(query, mempool, params),
        "blockchain.outpoint.subscribe" => {
            let (txid, vout) = param_outpoint(params)?;
            admit_outpoint_sub(
                &mut conn.outpoint_subs,
                txid,
                vout,
                config.max_scripthash_subs,
            )?;
            outpoint_status(query, mempool, params)
        }
        "blockchain.outpoint.unsubscribe" => {
            let (txid, vout) = param_outpoint(params)?;
            Ok(json!(conn.outpoint_subs.remove(&(txid, vout))))
        }
        "blockchain.silentpayments.subscribe" => {
            let sub = parse_sp_sub_on_chain(params, query, chain)?;
            Ok(crate::silent_scan::subscribe_result(&sub))
        }
        "blockchain.silentpayments.unsubscribe" => {
            silentpayments_unsubscribe(conn, query, chain, params)
        }
        "blockchain.transaction.id_from_pos" => id_from_pos(query, params),
        "blockchain.estimatefee" => {
            let target = param_u32(params, 0).unwrap_or(2);
            let fee = mempool
                .and_then(|m| m.estimate_smart_fee_sat_kvb(target).1)
                .map_or(-1.0, |sat| sat as f64 / 100_000_000.0);
            Ok(json!(fee))
        }
        "blockchain.relayfee" => {
            let inc = MempoolHub::relay_fee_btc_per_kb();
            let fee = mempool
                .map(|m| m.fee_floor_sat_kvb() as f64 / 100_000_000.0)
                .unwrap_or(inc);
            Ok(json!(fee))
        }
        "mempool.get_fee_histogram" => {
            let hist = mempool.map(|m| m.fee_histogram()).unwrap_or_default();
            // Electrum: array of [feerate, cumulative_vsize] with cumulative sizes.
            let mut cum = 0u64;
            let mut arr = Vec::new();
            for (rate, vsize) in hist {
                cum = cum.saturating_add(vsize);
                arr.push(json!([rate, cum]));
            }
            Ok(Value::Array(arr))
        }
        "server.peers.subscribe" => Ok(json!([])),
        "blockchain.tweaks.subscribe" => {
            crate::tweaks::subscribe(query, params, chain, config.tweaks_min_dust)
        }
        other => Err(format!("unknown method: {other}")),
    }
}

#[doc(hidden)]
pub fn tip_header_obj(query: &Query) -> Result<Value, String> {
    let tip = query
        .tip_height()
        .ok_or_else(|| "no chain tip".to_string())?;
    let hdr = query
        .wire_header_at_height(tip)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "hex": header_hex(&hdr),
        "height": tip.0,
    }))
}

#[doc(hidden)]
pub fn header_hex(hdr: &bitcoin::block::Header) -> String {
    let mut buf = Vec::new();
    hdr.consensus_encode(&mut buf).expect("header encode");
    rbitcoin_primitives::hex_encode(&(buf))
}

pub fn param_u32(params: &Value, idx: usize) -> Result<u32, String> {
    params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| format!("param {idx} expected number"))
}

#[doc(hidden)]
pub fn param_i64(params: &Value, idx: usize) -> Result<i64, String> {
    params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| format!("param {idx} expected integer"))
}

#[doc(hidden)]
pub fn parse_blockhash32(s: &str) -> Option<[u8; 32]> {
    rbitcoin_primitives::parse_display_hash32(s).ok()
}

#[doc(hidden)]
pub fn protocol_tuple(s: &str) -> Option<Vec<u32>> {
    if s.is_empty() {
        return None;
    }
    s.split('.').map(|p| p.parse().ok()).collect()
}

#[doc(hidden)]
pub fn protocol_ge_1_6(protocol: &str) -> bool {
    match protocol_tuple(protocol) {
        Some(v) => v.first() == Some(&1) && v.get(1).copied().unwrap_or(0) >= 6,
        None => false,
    }
}

#[doc(hidden)]
pub fn admit_outpoint_sub(
    subs: &mut HashSet<([u8; 32], u32)>,
    txid: [u8; 32],
    vout: u32,
    cap: usize,
) -> Result<(), String> {
    let key = (txid, vout);
    if !subs.contains(&key) && subs.len() >= cap {
        return Err(format!("too many outpoint subscriptions (max {cap})"));
    }
    subs.insert(key);
    Ok(())
}

#[doc(hidden)]
pub fn param_outpoint(params: &Value) -> Result<([u8; 32], u32), String> {
    let txid = param_txid(params, 0)?;
    let vout = param_u32(params, 1)?;
    Ok((txid, vout))
}

#[doc(hidden)]
pub fn outpoint_status(
    query: &Query,
    mempool: Option<&MempoolHub>,
    params: &Value,
) -> Result<Value, String> {
    let (txid, vout) = param_outpoint(params)?;
    if query
        .is_outpoint_spent(&txid, vout)
        .map_err(|e| e.to_string())?
    {
        return Ok(json!({
            "txid": txid_hex(&txid),
            "vout": vout,
            "spent": true,
            "height": query.tip_height().map(|h| h.0).unwrap_or(0),
        }));
    }
    if let Some(mp) = mempool {
        use bitcoin::hashes::Hash;
        let op = bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array(txid),
            vout,
        };
        if let Some(spend) = mp.spending_txid(&op) {
            return Ok(json!({
                "txid": txid_hex(&txid),
                "vout": vout,
                "spent": true,
                "height": 0,
                "spending_txid": format!("{spend}"),
            }));
        }
    }
    Ok(json!({
        "txid": txid_hex(&txid),
        "vout": vout,
        "spent": false,
    }))
}

#[doc(hidden)]
pub fn protocol_string(parts: &[u32]) -> String {
    parts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

#[doc(hidden)]
pub fn pick_dotted(cmin_s: &str, cmax_s: &str) -> Result<String, String> {
    let cmin =
        protocol_tuple(cmin_s).ok_or_else(|| format!("unsupported protocol version {cmin_s}"))?;
    let cmax =
        protocol_tuple(cmax_s).ok_or_else(|| format!("unsupported protocol version {cmax_s}"))?;
    let smin = protocol_tuple(PROTOCOL_MIN).expect("PROTOCOL_MIN");
    let smax = protocol_tuple(PROTOCOL_MAX).expect("PROTOCOL_MAX");
    let lo = std::cmp::max(cmin, smin);
    let hi = std::cmp::min(cmax, smax);
    if hi < lo {
        return Err("unsupported protocol version".into());
    }
    Ok(protocol_string(&hi))
}

#[doc(hidden)]
pub fn negotiate_protocol(params: &Value) -> Result<String, String> {
    let pv = params
        .as_array()
        .and_then(|a| a.get(1))
        .unwrap_or(&Value::Null);
    if pv.is_null() {
        return Ok(PROTOCOL_MAX.to_string());
    }
    if let Some(s) = pv.as_str() {
        if s == PROTOCOL_ASOF {
            return Ok(PROTOCOL_ASOF.to_string());
        }
        return pick_dotted(s, s);
    }
    let Some(range) = pv.as_array() else {
        return Err("protocol_version expected string or [min, max]".into());
    };
    if range.len() != 2 {
        return Err("protocol_version range must be [min, max]".into());
    }
    let a = range[0]
        .as_str()
        .ok_or("protocol_version range expected strings")?;
    let b = range[1]
        .as_str()
        .ok_or("protocol_version range expected strings")?;
    if a == PROTOCOL_ASOF || b == PROTOCOL_ASOF {
        return Ok(PROTOCOL_ASOF.to_string());
    }
    pick_dotted(a, b)
}

#[doc(hidden)]
pub fn take_trailing_asof(
    method: &str,
    params: &Value,
) -> Result<(Value, Option<[u8; 32]>), String> {
    if !method_stamps_chain_tip(method) {
        return Ok((params.clone(), None));
    }
    let Some(arr) = params.as_array() else {
        return Ok((params.clone(), None));
    };
    let Some(last) = arr.last().and_then(|v| v.as_str()) else {
        return Ok((params.clone(), None));
    };
    let Some(hex) = last.strip_prefix("asof:") else {
        return Ok((params.clone(), None));
    };
    let Some(hash) = parse_blockhash32(hex) else {
        return Err("asof must be asof:<32-byte hex>".into());
    };
    let mut rest = arr.clone();
    rest.pop();
    Ok((Value::Array(rest), Some(hash)))
}

/// Electrum Cash optional height window after scripthash for `get_history`.
///
/// Returns `(confirmed HistoryFilter, include_mempool)`.
/// - 1-arg / omitted heights: open confirmed window + mempool (BTC 1.4 / BCH defaults).
/// - `to_height == -1` (or only `from_height`): open upper bound + mempool.
/// - Finite exclusive `to_height`: confirmed `[from, to)` only — **no** mempool.
#[doc(hidden)]
pub fn parse_get_history_window(params: &Value) -> Result<(HistoryFilter, bool), String> {
    let arr = params
        .as_array()
        .ok_or_else(|| "params expected array".to_string())?;
    let from = if arr.len() >= 2 {
        param_u32(params, 1)?
    } else {
        0
    };
    let (to_excl, include_mempool) = if arr.len() >= 3 {
        let to = param_i64(params, 2)?;
        if to == -1 {
            (None, true)
        } else if to < 0 {
            return Err("to_height must be -1 or non-negative".into());
        } else {
            // BCH: from_height <= to_height (treat -1 as infinity; already handled).
            if i64::from(from) > to {
                return Err("from_height must be <= to_height".into());
            }
            (Some(to), false)
        }
    } else {
        (None, true)
    };
    Ok((HistoryFilter::height_window(from, to_excl), include_mempool))
}

pub fn param_str(params: &Value, idx: usize) -> Result<&str, String> {
    params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("param {idx} expected string"))
}

#[doc(hidden)]
pub fn param_scripthash(params: &Value, idx: usize) -> Result<[u8; 32], String> {
    let s = param_str(params, idx)?;
    rbitcoin_primitives::parse_display_hash32(s).map_err(|e| e.to_string())
}

#[doc(hidden)]
pub fn param_txid(params: &Value, idx: usize) -> Result<[u8; 32], String> {
    let s = param_str(params, idx)?;
    rbitcoin_primitives::parse_display_hash32(s).map_err(|e| e.to_string())
}

#[doc(hidden)]
pub fn decode_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    rbitcoin_primitives::hex_decode(s).map_err(|e| e.to_string())
}

#[doc(hidden)]
pub fn txid_hex(txid: &[u8; 32]) -> String {
    hash_hex_rev(txid)
}

#[doc(hidden)]
pub fn id_from_pos(query: &Query, params: &Value) -> Result<Value, String> {
    let height = param_u32(params, 0)?;
    let tx_pos = param_u32(params, 1)? as usize;
    let want_merkle = params
        .as_array()
        .and_then(|a| a.get(2))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let txid = query.block_txid_at(Height(height), tx_pos).map_err(|e| {
        if matches!(e, StoreError::NotFound) {
            "pos out of range".to_string()
        } else {
            e.to_string()
        }
    })?;
    if !want_merkle {
        return Ok(json!(txid_hex(&txid)));
    }
    let proof = query
        .merkle_proof(Height(height), &txid)
        .map_err(|e| e.to_string())?;
    let merkle: Vec<String> = proof.merkle.iter().map(hash_hex_rev).collect();
    Ok(json!({
        "tx_hash": txid_hex(&txid),
        "merkle": merkle,
    }))
}

/// Electrs-shaped verbose `transaction.get`. `fk` Some = confirmed (header
/// stamp); None = mempool (`confirmations: 0`, no block fields).
#[doc(hidden)]
pub fn verbose_tx_from_tx(
    query: &Query,
    tx: &bitcoin::Transaction,
    txid: &[u8; 32],
    fk: Option<Fk>,
    network: bitcoin::Network,
) -> Value {
    let raw = bitcoin::consensus::serialize(tx);
    let mut obj = json!({
        "hex": rbitcoin_primitives::hex_encode(&raw),
        "txid": txid_hex(txid),
        "size": raw.len(),
        "version": tx.version.0,
        "locktime": tx.lock_time.to_consensus_u32(),
        "hash": hash_hex_rev(bitcoin::hashes::sha256d::Hash::hash(&raw).as_byte_array()),
        "vin": tx.input.iter().map(verbose_vin).collect::<Vec<_>>(),
        "vout": tx.output.iter().enumerate().map(|(n, o)| verbose_vout(n, o, network)).collect::<Vec<_>>(),
    });
    match fk {
        Some(fk) => {
            if let Ok(Some(h)) = query.store().tx_height_get(fk) {
                if let Ok(hdr) = query.wire_header_at_height(Height(h)) {
                    let ts = hdr.time;
                    obj["time"] = json!(ts);
                    obj["blocktime"] = json!(ts);
                    obj["blockhash"] = json!(hash_hex_rev(&hdr.block_hash().to_byte_array()));
                    let tip = query.tip_height().map(|t| t.0).unwrap_or(h);
                    obj["confirmations"] = json!(tip.saturating_sub(h).saturating_add(1));
                }
            }
        }
        None => {
            obj["confirmations"] = json!(0);
        }
    }
    obj
}

#[doc(hidden)]
pub fn verbose_vin(input: &bitcoin::TxIn) -> Value {
    if input.previous_output.is_null() {
        json!({
            "coinbase": rbitcoin_primitives::hex_encode(input.script_sig.as_bytes()),
            "sequence": input.sequence.0,
        })
    } else {
        let mut v = json!({
            "txid": input.previous_output.txid.to_string(),
            "vout": input.previous_output.vout,
            "scriptSig": {
                "asm": input.script_sig.to_asm_string(),
                "hex": rbitcoin_primitives::hex_encode(input.script_sig.as_bytes()),
            },
            "sequence": input.sequence.0,
        });
        if !input.witness.is_empty() {
            v["txinwitness"] = Value::Array(
                input
                    .witness
                    .iter()
                    .map(|w| json!(rbitcoin_primitives::hex_encode(w)))
                    .collect(),
            );
        }
        v
    }
}

#[doc(hidden)]
pub fn verbose_vout(n: usize, out: &bitcoin::TxOut, network: bitcoin::Network) -> Value {
    let mut spk = json!({
        "asm": out.script_pubkey.to_asm_string(),
        "hex": rbitcoin_primitives::hex_encode(out.script_pubkey.as_bytes()),
    });
    if let Ok(addr) = bitcoin::Address::from_script(&out.script_pubkey, network) {
        spk["address"] = json!(addr.to_string());
        spk["addresses"] = json!([addr.to_string()]);
    }
    json!({
        "value": out.value.to_btc(),
        "n": n,
        "scriptPubKey": spk,
    })
}

#[doc(hidden)]
pub fn hash_hex_rev(h: &[u8; 32]) -> String {
    rbitcoin_primitives::display_hash_hex(*(h))
}

#[doc(hidden)]
pub fn history_row_json(i: &rbitcoin_query::ScriptHashHistoryItem) -> Value {
    let mut row = json!({
        "height": i.height,
        "tx_hash": txid_hex(&i.txid),
    });
    if let Some(fee) = i.fee {
        row["fee"] = json!(fee);
    }
    row
}

#[doc(hidden)]
pub fn history_has_txid(hist: &[rbitcoin_query::ScriptHashHistoryItem], txid: &[u8; 32]) -> bool {
    hist.iter().any(|h| h.txid == *txid)
}

#[doc(hidden)]
pub fn append_mempool_history(
    hist: &mut Vec<rbitcoin_query::ScriptHashHistoryItem>,
    mp: &MempoolHub,
    sh: &[u8; 32],
) {
    for item in mp.scripthash_mempool(sh) {
        if history_has_txid(hist, &item.txid) {
            continue;
        }
        hist.push(rbitcoin_query::ScriptHashHistoryItem {
            height: item.height,
            txid: item.txid,
            tx_fk: Fk::NULL,
            fee: Some(item.fee),
        });
    }
}

/// Electrum protocol status: SHA-256 of `tx_hash:height:` for each row in
/// `get_history` order (confirmed by height, then the mempool tail). Clients
/// recompute it from `get_history` and drop the history when it differs.
#[doc(hidden)]
pub fn scripthash_status(hist: &[rbitcoin_query::ScriptHashHistoryItem]) -> String {
    if hist.is_empty() {
        return String::new();
    }
    use bitcoin::hashes::{sha256, Hash as _};
    let mut s = String::new();
    for i in hist {
        s.push_str(&format!("{}:{}:", txid_hex(&i.txid), i.height));
    }
    let hash = sha256::Hash::hash(s.as_bytes());
    rbitcoin_primitives::hex_encode(hash.to_byte_array())
}

#[doc(hidden)]
pub fn scripthash_status_full(
    query: &Query,
    mp: &MempoolHub,
    sh: &[u8; 32],
) -> Result<String, String> {
    let mut slot = None;
    scripthash_status_full_slot(query, mp, sh, &mut slot)
}

#[doc(hidden)]
pub fn scripthash_status_full_slot(
    query: &Query,
    mp: &MempoolHub,
    sh: &[u8; 32],
    slot: &mut Option<Arc<ShJoinSlot>>,
) -> Result<String, String> {
    let mut hist = query
        .scripthash_history_slot(sh, slot)
        .map_err(|e| e.to_string())?;
    append_mempool_history(&mut hist, mp, sh);
    Ok(scripthash_status(&hist))
}

/// Helper to compute electrum scripthash hex (reversed) from script bytes.
pub fn electrum_scripthash_hex(script: &[u8]) -> String {
    rbitcoin_primitives::scripthash_display_hex(script)
}
