//! Tor control-port AUTH (cookie or password) for hidden-service setup.

use crate::error::NodeError;
use base64::Engine;
use bitcoin_hashes::{hmac, sha256, Hash, HashEngine};
use rbitcoin_log::{debug, info, warn};
use rbitcoin_net::PeerHub;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

#[doc(hidden)]
pub const SAFECOOKIE_SERVER_KEY: &[u8] =
    b"Tor safe cookie authentication server-to-controller hash";
#[doc(hidden)]
pub const SAFECOOKIE_CLIENT_KEY: &[u8] =
    b"Tor safe cookie authentication controller-to-server hash";

/// Longest wait for the control port to connect, or to answer one command.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest reply line taken from the control port.
pub const MAX_REPLY_LINE: usize = 64 * 1024;
/// Most bytes one reply may span across its lines.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// ADD_ONION key spec that asks Tor for a fresh v3 key.
const ONION_KEY_NEW: &str = "NEW:ED25519-V3";

pub const RECONNECT_START: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(600);

pub fn next_reconnect_wait(wait: Duration) -> Duration {
    wait.mul_f64(1.5).min(RECONNECT_MAX)
}

pub const DEFAULT_CONTROL_PORT: u16 = 9051;
pub const DEFAULT_COOKIE_PATH: &str = "/run/tor/control.authcookie";

pub fn default_control_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], DEFAULT_CONTROL_PORT))
}

#[derive(Clone, Debug)]
pub enum TorAuth {
    Cookie(PathBuf),
    Password(String),
}

pub struct TorControl {
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    addr: SocketAddr,
    auth: TorAuth,
    timeout: Duration,
}

impl TorControl {
    pub async fn connect_and_auth(
        addr: SocketAddr,
        auth: TorAuth,
        timeout: Duration,
    ) -> Result<Self, NodeError> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| no_reply(addr, timeout))?
            .map_err(|e| NodeError::Init(format!("tor control connect {addr}: {e}")))?;
        let (r, w) = stream.into_split();
        let mut ctl = Self {
            reader: BufReader::new(r),
            writer: w,
            addr,
            auth: auth.clone(),
            timeout,
        };
        ctl.authenticate(&auth).await?;
        ctl.command("GETINFO version").await?;
        Ok(ctl)
    }

    pub async fn connect_if_configured(
        addr: Option<SocketAddr>,
        cookie: Option<&Path>,
        password: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<Self>, NodeError> {
        let Some(addr) = addr else {
            return Ok(None);
        };
        let auth = match password {
            Some(p) if !p.is_empty() => TorAuth::Password(p.to_string()),
            _ => TorAuth::Cookie(
                cookie
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_COOKIE_PATH)),
            ),
        };
        Ok(Some(Self::connect_and_auth(addr, auth, timeout).await?))
    }

    pub async fn authenticate(&mut self, auth: &TorAuth) -> Result<String, NodeError> {
        match auth {
            TorAuth::Password(p) => {
                let line = format!("AUTHENTICATE \"{}\"", escape_quoted(p));
                self.command(&line).await
            }
            TorAuth::Cookie(path) => {
                let info = self.protocol_auth_info().await?;
                if info.methods.iter().any(|m| m == "SAFECOOKIE") {
                    self.authenticate_safecookie(path).await
                } else {
                    Err(NodeError::Init(
                        "tor control: SAFECOOKIE is not advertised; refusing plain cookie AUTHENTICATE"
                            .into(),
                    ))
                }
            }
        }
    }

    pub async fn authenticate_safecookie(&mut self, path: &Path) -> Result<String, NodeError> {
        let cookie = std::fs::read(path)
            .map_err(|e| NodeError::Init(format!("tor control cookie {}: {e}", path.display())))?;
        if cookie.len() != 32 {
            return Err(NodeError::Init(
                "tor control: SAFECOOKIE cookie must be 32 bytes".into(),
            ));
        }
        let mut client_nonce = [0u8; 32];
        getrandom::fill(&mut client_nonce).map_err(|e| {
            NodeError::Init(format!("tor control SAFECOOKIE client nonce rng: {e}"))
        })?;
        let reply = self
            .command(&format!(
                "AUTHCHALLENGE SAFECOOKIE {}",
                rbitcoin_primitives::hex_encode(client_nonce)
            ))
            .await?;
        let challenge = parse_authchallenge_reply(&reply)?;
        let mut mat = Vec::with_capacity(96);
        mat.extend_from_slice(&cookie);
        mat.extend_from_slice(&client_nonce);
        mat.extend_from_slice(&challenge.server_nonce);
        let want_server = hmac_sha256(SAFECOOKIE_SERVER_KEY, &mat);
        if want_server != challenge.server_hash {
            return Err(NodeError::Init(format!(
                "tor control SAFECOOKIE server hash mismatch want={} got={}",
                rbitcoin_primitives::hex_encode(want_server),
                rbitcoin_primitives::hex_encode(challenge.server_hash)
            )));
        }
        let client = hmac_sha256(SAFECOOKIE_CLIENT_KEY, &mat);
        self.command(&format!(
            "AUTHENTICATE {}",
            rbitcoin_primitives::hex_encode(client)
        ))
        .await
    }

    async fn closed(&mut self) {
        loop {
            let n = match self.reader.fill_buf().await {
                Ok([]) | Err(_) => return,
                Ok(buf) => buf.len(),
            };
            self.reader.consume(n);
        }
    }

    pub async fn protocol_auth_info(&mut self) -> Result<ProtocolAuthInfo, NodeError> {
        let body = self.command("PROTOCOLINFO 1").await?;
        parse_protocol_auth_info(&body)
    }

    /// One command and its reply, within the control timeout.
    pub async fn command(&mut self, cmd: &str) -> Result<String, NodeError> {
        self.command_reply(cmd).await?.map_err(refused)
    }

    async fn command_reply(&mut self, cmd: &str) -> Result<Result<String, String>, NodeError> {
        let (addr, timeout) = (self.addr, self.timeout);
        tokio::time::timeout(timeout, self.exchange(cmd))
            .await
            .map_err(|_| no_reply(addr, timeout))?
    }

    async fn exchange(&mut self, cmd: &str) -> Result<Result<String, String>, NodeError> {
        self.writer
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| NodeError::Init(format!("tor control write: {e}")))?;
        self.writer
            .write_all(b"\r\n")
            .await
            .map_err(|e| NodeError::Init(format!("tor control write: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| NodeError::Init(format!("tor control write: {e}")))?;
        read_reply_or_refusal(&mut self.reader).await
    }

    async fn add_onion_persistent(
        &mut self,
        key_path: &Path,
        virt: u16,
        target: SocketAddr,
    ) -> Result<HiddenService, AddFailure> {
        let stored = match std::fs::read_to_string(key_path) {
            Ok(s) => {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(AddFailure::Refused(NodeError::Init(format!(
                    "tor control key {}: {e}",
                    key_path.display()
                ))));
            }
        };
        if stored.as_deref().is_some_and(|k| !onion_key_well_formed(k)) {
            return Err(AddFailure::Refused(NodeError::Init(format!(
                "tor control key {}: malformed; remove the file to generate a new key",
                key_path.display()
            ))));
        }
        let spec = stored.as_deref().unwrap_or(ONION_KEY_NEW);
        let cmd = format!("ADD_ONION {spec} Port={virt},{target}");
        let reply = match self.command_reply(&cmd).await {
            Ok(Ok(body)) => body,
            Ok(Err(line)) => return Err(AddFailure::Refused(refused(line))),
            Err(e) => return Err(AddFailure::Lost(e)),
        };
        let hs = parse_add_onion_reply(&reply).map_err(AddFailure::Refused)?;
        if stored.is_none() {
            let Some(ref pk) = hs.private_key else {
                return Err(AddFailure::Refused(NodeError::Init(
                    "tor control ADD_ONION NEW missing PrivateKey".into(),
                )));
            };
            write_key_file(key_path, pk).map_err(AddFailure::Refused)?;
        }
        Ok(hs)
    }
}

enum AddFailure {
    Refused(NodeError),
    Lost(NodeError),
}

fn refused(line: String) -> NodeError {
    NodeError::Init(format!("tor control: {line}"))
}

pub type OnionAdded = Box<dyn FnOnce(&HiddenService) + Send + Sync>;

enum OnionKind {
    P2p,
    Wallet,
}

struct Onion {
    key_path: PathBuf,
    virt: u16,
    target: SocketAddr,
    kind: OnionKind,
    added: Option<OnionAdded>,
}

struct OnionAsk {
    onion: Onion,
    reply: oneshot::Sender<Result<(), NodeError>>,
}

pub struct TorKeeper {
    asks: mpsc::Sender<OnionAsk>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TorKeeper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TorKeeper {
    pub fn spawn(ctl: TorControl, peers: Arc<PeerHub>, first_wait: Duration) -> Self {
        let (asks, rx) = mpsc::channel(8);
        let task = tokio::spawn(keep(ctl, peers, rx, first_wait));
        Self { asks, task }
    }

    pub async fn add_p2p_onion(
        &self,
        datadir: &Path,
        bound: SocketAddr,
        virt: u16,
    ) -> Result<(), NodeError> {
        self.add(Onion {
            key_path: datadir.join("onion").join("p2p.priv"),
            virt,
            target: SocketAddr::from(([127, 0, 0, 1], bound.port())),
            kind: OnionKind::P2p,
            added: Some(Box::new(move |hs| {
                info!("p2p onion {}.onion:{virt}", hs.service_id)
            })),
        })
        .await
    }

    pub async fn add_named_onion(
        &self,
        datadir: &Path,
        name: &str,
        bound: SocketAddr,
        added: OnionAdded,
    ) -> Result<(), NodeError> {
        self.add(Onion {
            key_path: datadir.join("onion").join(format!("{name}.priv")),
            virt: bound.port(),
            target: SocketAddr::from(([127, 0, 0, 1], bound.port())),
            kind: OnionKind::Wallet,
            added: Some(added),
        })
        .await
    }

    async fn add(&self, onion: Onion) -> Result<(), NodeError> {
        let (reply, answer) = oneshot::channel();
        self.asks
            .send(OnionAsk { onion, reply })
            .await
            .map_err(|_| keeper_gone())?;
        answer.await.map_err(|_| keeper_gone())?
    }
}

fn keeper_gone() -> NodeError {
    NodeError::Init("tor control: the onion keeper stopped".into())
}

async fn keep(
    ctl: TorControl,
    peers: Arc<PeerHub>,
    mut asks: mpsc::Receiver<OnionAsk>,
    first_wait: Duration,
) {
    let (addr, auth, timeout) = (ctl.addr, ctl.auth.clone(), ctl.timeout);
    let mut live = Some(ctl);
    let mut onions: Vec<Onion> = Vec::new();
    let mut wait = first_wait;
    let mut retry_at = tokio::time::Instant::now();
    loop {
        let ask = match live.as_mut() {
            Some(ctl) => tokio::select! {
                ask = asks.recv() => ask,
                () = ctl.closed() => {
                    warn!("tor control {addr}: connection closed; onion services are down until it reconnects");
                    peers.set_tor_onions_down(true);
                    live = None;
                    wait = first_wait;
                    retry_at = tokio::time::Instant::now() + wait;
                    continue;
                }
            },
            None => tokio::select! {
                ask = asks.recv() => ask,
                () = tokio::time::sleep_until(retry_at) => {
                    live = restore(addr, &auth, timeout, &mut onions, &peers).await;
                    wait = next_reconnect_wait(wait);
                    retry_at = tokio::time::Instant::now() + wait;
                    continue;
                }
            },
        };
        let Some(OnionAsk { mut onion, reply }) = ask else {
            return;
        };
        if let Some(ctl) = live.as_mut() {
            match ctl
                .add_onion_persistent(&onion.key_path, onion.virt, onion.target)
                .await
            {
                Ok(hs) => {
                    publish(&onion, &hs, &peers);
                    if let Some(added) = onion.added.take() {
                        added(&hs);
                    }
                }
                Err(AddFailure::Refused(e)) => {
                    let _ = reply.send(Err(e));
                    continue;
                }
                Err(AddFailure::Lost(e)) => {
                    warn!("tor control {addr}: {e}; onion services are down until it reconnects");
                    peers.set_tor_onions_down(true);
                    live = None;
                    wait = first_wait;
                    retry_at = tokio::time::Instant::now() + wait;
                }
            }
        }
        onions.retain(|kept| kept.key_path != onion.key_path);
        onions.push(onion);
        let _ = reply.send(Ok(()));
    }
}

async fn restore(
    addr: SocketAddr,
    auth: &TorAuth,
    timeout: Duration,
    onions: &mut [Onion],
    peers: &PeerHub,
) -> Option<TorControl> {
    let mut ctl = match TorControl::connect_and_auth(addr, auth.clone(), timeout).await {
        Ok(ctl) => ctl,
        Err(e) => {
            debug!("tor control {addr}: reconnect failed: {e}");
            return None;
        }
    };
    let mut services = Vec::with_capacity(onions.len());
    for onion in onions.iter() {
        match ctl
            .add_onion_persistent(&onion.key_path, onion.virt, onion.target)
            .await
        {
            Ok(hs) => {
                publish(onion, &hs, peers);
                services.push(hs);
            }
            Err(AddFailure::Refused(e) | AddFailure::Lost(e)) => {
                warn!(
                    "tor control {addr}: restoring onion {}: {e}",
                    onion.key_path.display()
                );
                return None;
            }
        }
    }
    peers.set_tor_onions_down(false);
    for (onion, hs) in onions.iter_mut().zip(&services) {
        if let Some(added) = onion.added.take() {
            added(hs);
        }
    }
    info!(
        "tor control {addr}: reconnected, {} onion service(s) restored",
        onions.len()
    );
    Some(ctl)
}

fn publish(onion: &Onion, hs: &HiddenService, peers: &PeerHub) {
    let host = format!("{}.onion", hs.service_id);
    match onion.kind {
        OnionKind::P2p => peers.set_p2p_onion(host, onion.virt),
        OnionKind::Wallet => peers.set_tor_wallet_onion(host, onion.virt),
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HiddenService {
    pub service_id: String,
    pub private_key: Option<String>,
}

impl std::fmt::Debug for HiddenService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HiddenService")
            .field("service_id", &self.service_id)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct ProtocolAuthInfo {
    pub methods: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct SafeCookieChallenge {
    pub server_hash: [u8; 32],
    pub server_nonce: [u8; 32],
}

#[doc(hidden)]
pub fn escape_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn no_reply(addr: SocketAddr, timeout: Duration) -> NodeError {
    NodeError::Init(format!("--tor-control {addr}: no reply within {timeout:?}"))
}

/// One reply: `250-` lines up to a `250 ` line. A line over
/// [`MAX_REPLY_LINE`] or a reply over [`MAX_REPLY_BYTES`] is refused before
/// it is held.
pub async fn read_reply(reader: &mut BufReader<OwnedReadHalf>) -> Result<String, NodeError> {
    read_reply_or_refusal(reader).await?.map_err(refused)
}

async fn read_reply_or_refusal(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<Result<String, String>, NodeError> {
    let mut body = String::new();
    let mut read = 0usize;
    loop {
        let mut line = String::new();
        let n = (&mut *reader)
            .take(MAX_REPLY_LINE as u64 + 1)
            .read_line(&mut line)
            .await
            .map_err(|e| NodeError::Init(format!("tor control read: {e}")))?;
        if n == 0 {
            return Err(NodeError::Init("tor control: connection closed".into()));
        }
        if n > MAX_REPLY_LINE {
            return Err(NodeError::Init(format!(
                "--tor-control: reply line over {} KiB",
                MAX_REPLY_LINE >> 10
            )));
        }
        read += n;
        if read > MAX_REPLY_BYTES {
            return Err(NodeError::Init(format!(
                "--tor-control: reply over {} MiB",
                MAX_REPLY_BYTES >> 20
            )));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.len() < 4 {
            return Err(NodeError::Init(format!(
                "tor control: short reply `{line}`"
            )));
        }
        let unexpected = || NodeError::Init(format!("tor control: unexpected reply `{line}`"));
        let Some((code, tail)) = line.split_at_checked(3) else {
            return Err(unexpected());
        };
        if !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(unexpected());
        }
        let mut tail = tail.chars();
        let sep = tail.next();
        let rest = tail.as_str();
        if code.starts_with('5') {
            return Ok(Err(line.to_string()));
        }
        match sep {
            Some('-') => {
                body.push_str(rest);
                body.push('\n');
            }
            Some(' ') => {
                if !rest.is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(rest);
                }
                return Ok(Ok(body));
            }
            _ => return Err(unexpected()),
        }
    }
}

#[doc(hidden)]
pub fn parse_add_onion_reply(body: &str) -> Result<HiddenService, NodeError> {
    let mut service_id = None;
    let mut private_key = None;
    for line in body.lines() {
        if let Some(id) = line.strip_prefix("ServiceID=") {
            service_id = Some(id.trim().to_string());
        } else if let Some(pk) = line.strip_prefix("PrivateKey=") {
            let pk = pk.trim();
            if !onion_key_well_formed(pk) {
                return Err(NodeError::Init(
                    "tor control ADD_ONION returned a malformed PrivateKey".into(),
                ));
            }
            private_key = Some(pk.to_string());
        }
    }
    let Some(service_id) = service_id else {
        return Err(NodeError::Init(
            "tor control ADD_ONION missing ServiceID".into(),
        ));
    };
    if rbitcoin_net::netaddr::decode_onion_name(&service_id).is_none() {
        return Err(NodeError::Init(
            "tor control ADD_ONION returned an invalid ServiceID".into(),
        ));
    }
    Ok(HiddenService {
        service_id,
        private_key,
    })
}

/// `ED25519-V3:` and Base64 of a 64-byte expanded secret key: the only key
/// Tor returns for a v3 service, and the only one sent back in ADD_ONION.
fn onion_key_well_formed(key: &str) -> bool {
    key.strip_prefix("ED25519-V3:")
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .is_some_and(|raw| raw.len() == 64)
}

#[doc(hidden)]
pub fn parse_protocol_auth_info(body: &str) -> Result<ProtocolAuthInfo, NodeError> {
    for line in body.lines() {
        if !line.starts_with("AUTH ") {
            continue;
        }
        let methods = kv_token(line, "METHODS")
            .ok_or_else(|| NodeError::Init(format!("tor control AUTH METHODS missing: {line}")))?
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        return Ok(ProtocolAuthInfo { methods });
    }
    Err(NodeError::Init(
        "tor control PROTOCOLINFO missing AUTH line".into(),
    ))
}

#[doc(hidden)]
pub fn parse_authchallenge_reply(body: &str) -> Result<SafeCookieChallenge, NodeError> {
    let mut server_hash = None;
    let mut server_nonce = None;
    for line in body.lines() {
        if let Some(v) = kv_token(line, "SERVERHASH") {
            server_hash = Some(hex32(v, "SERVERHASH")?);
        }
        if let Some(v) = kv_token(line, "SERVERNONCE") {
            server_nonce = Some(hex32(v, "SERVERNONCE")?);
        }
    }
    let Some(server_hash) = server_hash else {
        return Err(NodeError::Init(
            "tor control AUTHCHALLENGE missing SERVERHASH".into(),
        ));
    };
    let Some(server_nonce) = server_nonce else {
        return Err(NodeError::Init(
            "tor control AUTHCHALLENGE missing SERVERNONCE".into(),
        ));
    };
    Ok(SafeCookieChallenge {
        server_hash,
        server_nonce,
    })
}

#[doc(hidden)]
pub fn kv_token<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(&format!("{key}=")))
}

#[doc(hidden)]
pub fn hex32(s: &str, field: &str) -> Result<[u8; 32], NodeError> {
    if s.len() != 64 {
        return Err(NodeError::Init(format!(
            "tor control {field} must be 64 hex chars"
        )));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_nybble(chunk[0])
            .ok_or_else(|| NodeError::Init(format!("tor control {field} has non-hex content")))?;
        let lo = hex_nybble(chunk[1])
            .ok_or_else(|| NodeError::Init(format!("tor control {field} has non-hex content")))?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

#[doc(hidden)]
pub fn hex_nybble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[doc(hidden)]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut engine = hmac::HmacEngine::<sha256::Hash>::new(key);
    engine.input(data);
    hmac::Hmac::<sha256::Hash>::from_engine(engine).to_byte_array()
}

#[doc(hidden)]
pub fn write_key_file(path: &Path, key: &str) -> Result<(), NodeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            NodeError::Init(format!("tor control key dir {}: {e}", parent.display()))
        })?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| NodeError::Init(format!("tor control key {}: {e}", path.display())))?;
    writeln!(f, "{key}")
        .map_err(|e| NodeError::Init(format!("tor control key {}: {e}", path.display())))?;
    Ok(())
}
