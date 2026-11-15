use rbitcoin_node::tor_control::{
    hex32, hmac_sha256, TorAuth, TorControl, TorKeeper, SAFECOOKIE_CLIENT_KEY,
    SAFECOOKIE_SERVER_KEY,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Notify;

const P2P_ID: &str = "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd";
const WALLET_ID: &str = "vww6ybal4bd7szmgncyruucpgfkqahzddi37ktceo3ah7ngmcopnpyyd";
const FAST: Duration = Duration::from_millis(10);
const WALLET_ADD: &str = "Port=50001,";

#[derive(Default)]
struct Script {
    drop_on: Option<(usize, &'static str)>,
    refuse_on: Option<(usize, &'static str)>,
}

struct FakeTor {
    addr: SocketAddr,
    cookie: PathBuf,
    sessions: Arc<Mutex<Vec<Vec<String>>>>,
    restart: Arc<Notify>,
    up: Arc<AtomicBool>,
}

impl FakeTor {
    async fn start(dir: &std::path::Path, script: Script) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let cookie = dir.join("control_auth_cookie");
        std::fs::write(&cookie, [1u8; 32]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let restart = Arc::new(Notify::new());
        let up = Arc::new(AtomicBool::new(true));
        let (log, drop_now, is_up, cookie_file) = (
            Arc::clone(&sessions),
            Arc::clone(&restart),
            Arc::clone(&up),
            cookie.clone(),
        );
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                if !is_up.load(Ordering::SeqCst) {
                    continue;
                }
                let session = {
                    let mut all = log.lock().unwrap();
                    all.push(Vec::new());
                    all.len() - 1
                };
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                let mut nonces = None;
                loop {
                    let line = tokio::select! {
                        line = lines.next_line() => match line {
                            Ok(Some(line)) => line,
                            _ => break,
                        },
                        () = drop_now.notified() => break,
                    };
                    log.lock().unwrap()[session].push(line.clone());
                    let hit = |rule: Option<(usize, &str)>| {
                        rule.is_some_and(|(at, text)| at == session && line.contains(text))
                    };
                    if hit(script.drop_on) {
                        break;
                    }
                    let reply = if hit(script.refuse_on) {
                        "551 Unable to add onion\r\n".to_string()
                    } else if line.starts_with("PROTOCOLINFO") {
                        "250-PROTOCOLINFO 1\r\n250-AUTH METHODS=COOKIE,SAFECOOKIE\r\n250 OK\r\n"
                            .to_string()
                    } else if let Some(hex) = line.strip_prefix("AUTHCHALLENGE SAFECOOKIE ") {
                        let client = hex32(hex.trim(), "nonce").unwrap();
                        let server = [7u8; 32];
                        nonces = Some((client, server));
                        let hash = hmac_sha256(
                            SAFECOOKIE_SERVER_KEY,
                            &cookie_material(&cookie_file, client, server),
                        );
                        format!(
                            "250 AUTHCHALLENGE SERVERHASH={} SERVERNONCE={}\r\n",
                            rbitcoin_primitives::hex_encode(hash),
                            rbitcoin_primitives::hex_encode(server)
                        )
                    } else if let Some(hex) = line.strip_prefix("AUTHENTICATE ") {
                        let (client, server) = nonces.expect("AUTHCHALLENGE first");
                        let want = hmac_sha256(
                            SAFECOOKIE_CLIENT_KEY,
                            &cookie_material(&cookie_file, client, server),
                        );
                        if hex.trim() == rbitcoin_primitives::hex_encode(want) {
                            log.lock().unwrap()[session].push("AUTH OK".into());
                            "250 OK\r\n".to_string()
                        } else {
                            "515 Authentication failed\r\n".to_string()
                        }
                    } else if line.starts_with("GETINFO version") {
                        "250-version=0.4.8.10\r\n250 OK\r\n".to_string()
                    } else if let Some(rest) = line.strip_prefix("ADD_ONION ") {
                        let id = if rest.contains(WALLET_ADD) {
                            WALLET_ID
                        } else {
                            P2P_ID
                        };
                        let key = if rest.starts_with("NEW:") {
                            format!("250-PrivateKey={}\r\n", key_of(id))
                        } else {
                            String::new()
                        };
                        format!("250-ServiceID={id}\r\n{key}250 OK\r\n")
                    } else {
                        "250 OK\r\n".to_string()
                    };
                    if w.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            addr,
            cookie,
            sessions,
            restart,
            up,
        }
    }

    fn restart_with_new_cookie(&self, byte: u8) {
        std::fs::write(&self.cookie, [byte; 32]).unwrap();
        self.restart.notify_one();
    }

    fn session(&self, n: usize) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .get(n)
            .cloned()
            .unwrap_or_default()
    }

    fn sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    async fn keeper(&self, peers: &Arc<rbitcoin_net::PeerHub>) -> TorKeeper {
        self.keeper_waiting(peers, FAST).await
    }

    async fn keeper_waiting(
        &self,
        peers: &Arc<rbitcoin_net::PeerHub>,
        first_wait: Duration,
    ) -> TorKeeper {
        let ctl = TorControl::connect_and_auth(
            self.addr,
            TorAuth::Cookie(self.cookie.clone()),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        TorKeeper::spawn(ctl, Arc::clone(peers), first_wait)
    }
}

fn cookie_material(path: &std::path::Path, client: [u8; 32], server: [u8; 32]) -> Vec<u8> {
    let mut mat = std::fs::read(path).unwrap();
    mat.extend_from_slice(&client);
    mat.extend_from_slice(&server);
    mat
}

fn key_of(id: &str) -> String {
    use base64::Engine;
    let raw = [id.as_bytes()[0]; 64];
    format!(
        "ED25519-V3:{}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rbitcoin-tor-{tag}-{nanos}"))
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(5), "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn wallet_bind() -> SocketAddr {
    "127.0.0.1:50001".parse().unwrap()
}

fn p2p_bind() -> SocketAddr {
    "127.0.0.1:40001".parse().unwrap()
}

fn counted(calls: &Arc<AtomicUsize>) -> rbitcoin_node::tor_control::OnionAdded {
    let calls = Arc::clone(calls);
    Box::new(move |hs| {
        assert_eq!(hs.service_id, WALLET_ID);
        calls.fetch_add(1, Ordering::SeqCst);
    })
}

fn lists(peers: &rbitcoin_net::PeerHub, id: &str) -> bool {
    peers
        .rpc_local_addresses()
        .iter()
        .any(|row| row.0 == format!("{id}.onion"))
}

#[tokio::test]
async fn tor_restart_brings_every_onion_back_with_the_same_key() {
    let dir = temp_dir("restart");
    let tor = FakeTor::start(&dir, Script::default()).await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper(&peers).await;
    keeper.add_p2p_onion(&dir, p2p_bind(), 18444).await.unwrap();
    let added = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        keeper
            .add_named_onion(&dir, "electrum", wallet_bind(), counted(&added))
            .await
            .unwrap();
    }
    let onion = peers.p2p_onion().expect("p2p onion set");
    assert_eq!(peers.announced_p2p_onion(), Some(onion));
    assert!(lists(&peers, WALLET_ID));
    let p2p_key = std::fs::read_to_string(dir.join("onion/p2p.priv")).unwrap();
    let wallet_key = std::fs::read_to_string(dir.join("onion/electrum.priv")).unwrap();

    tor.up.store(false, Ordering::SeqCst);
    tor.restart_with_new_cookie(2);
    wait_for("a closed control port stops announcing the onion", || {
        peers.announced_p2p_onion().is_none()
    })
    .await;
    assert!(!lists(&peers, WALLET_ID), "nor lists the wallet onion");
    assert!(peers.is_own_address(onion));

    tor.up.store(true, Ordering::SeqCst);
    wait_for("the onion is announced again once Tor is back", || {
        peers.announced_p2p_onion() == Some(onion)
    })
    .await;
    assert!(lists(&peers, WALLET_ID));
    let again = tor.session(1);
    assert!(
        again.iter().any(|l| l == "AUTH OK"),
        "logs in with the new cookie: {again:?}"
    );
    for want in [
        format!("ADD_ONION {} Port=18444,{}", p2p_key.trim(), p2p_bind()),
        format!(
            "ADD_ONION {} Port=50001,{}",
            wallet_key.trim(),
            wallet_bind()
        ),
    ] {
        assert_eq!(
            again.iter().filter(|l| **l == want).count(),
            1,
            "{want} must be re-added once: {again:?}"
        );
    }
    assert_eq!(
        added.load(Ordering::SeqCst),
        2,
        "each add reports its onion once"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_half_restored_tor_announces_nothing() {
    let dir = temp_dir("half");
    let tor = FakeTor::start(
        &dir,
        Script {
            drop_on: Some((1, WALLET_ADD)),
            ..Script::default()
        },
    )
    .await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper(&peers).await;
    keeper.add_p2p_onion(&dir, p2p_bind(), 18444).await.unwrap();
    keeper
        .add_named_onion(&dir, "electrum", wallet_bind(), Box::new(|_| {}))
        .await
        .unwrap();
    let onion = peers.p2p_onion().expect("p2p onion set");

    tor.up.store(false, Ordering::SeqCst);
    tor.restart_with_new_cookie(2);
    wait_for("Tor goes away", || peers.announced_p2p_onion().is_none()).await;
    tor.up.store(true, Ordering::SeqCst);
    wait_for("the restore reaches the wallet onion", || {
        tor.session(1).iter().any(|l| l.contains(WALLET_ADD))
    })
    .await;
    assert_eq!(peers.announced_p2p_onion(), None);
    assert!(!lists(&peers, WALLET_ID));
    wait_for("the next restore brings both back", || {
        peers.announced_p2p_onion() == Some(onion) && lists(&peers, WALLET_ID)
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_onion_asked_while_tor_is_away_is_added_when_it_returns() {
    let dir = temp_dir("away");
    let tor = FakeTor::start(&dir, Script::default()).await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper(&peers).await;
    keeper.add_p2p_onion(&dir, p2p_bind(), 18444).await.unwrap();
    tor.up.store(false, Ordering::SeqCst);
    tor.restart_with_new_cookie(2);
    wait_for("Tor goes away", || peers.announced_p2p_onion().is_none()).await;

    let added = Arc::new(AtomicUsize::new(0));
    tokio::time::timeout(
        Duration::from_secs(1),
        keeper.add_named_onion(&dir, "electrum", wallet_bind(), counted(&added)),
    )
    .await
    .expect("the caller does not wait for Tor")
    .expect("a missing Tor is not an error");
    assert_eq!(added.load(Ordering::SeqCst), 0);
    assert!(!lists(&peers, WALLET_ID));

    tor.up.store(true, Ordering::SeqCst);
    wait_for("the wallet onion is added once Tor is back", || {
        added.load(Ordering::SeqCst) == 1 && lists(&peers, WALLET_ID)
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_onion_lost_mid_add_is_added_on_reconnect() {
    let dir = temp_dir("mid");
    let tor = FakeTor::start(
        &dir,
        Script {
            drop_on: Some((0, WALLET_ADD)),
            ..Script::default()
        },
    )
    .await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper(&peers).await;
    let added = Arc::new(AtomicUsize::new(0));
    keeper
        .add_named_onion(&dir, "electrum", wallet_bind(), counted(&added))
        .await
        .expect("a connection lost mid add is not an error");
    wait_for("the reconnect adds the wallet onion", || {
        added.load(Ordering::SeqCst) == 1 && lists(&peers, WALLET_ID)
    })
    .await;
    assert!(tor.session(1).iter().any(|l| l.contains(WALLET_ADD)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_onion_tor_refuses_is_an_error_and_not_kept() {
    let dir = temp_dir("refused");
    let tor = FakeTor::start(
        &dir,
        Script {
            refuse_on: Some((0, WALLET_ADD)),
            ..Script::default()
        },
    )
    .await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper(&peers).await;
    keeper.add_p2p_onion(&dir, p2p_bind(), 18444).await.unwrap();
    let err = keeper
        .add_named_onion(&dir, "electrum", wallet_bind(), Box::new(|_| {}))
        .await
        .expect_err("Tor's refusal reaches the caller");
    assert!(err.to_string().contains("551"), "{err}");

    tor.restart_with_new_cookie(2);
    wait_for("the next session restores the kept onions", || {
        tor.sessions() == 2 && tor.session(1).iter().any(|l| l.contains("Port=18444,"))
    })
    .await;
    assert!(
        !tor.session(1).iter().any(|l| l.contains(WALLET_ADD)),
        "a refused onion is not re-added: {:?}",
        tor.session(1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn asks_during_an_outage_do_not_push_the_reconnect_back() {
    let dir = temp_dir("deadline");
    let tor = FakeTor::start(&dir, Script::default()).await;
    let peers = rbitcoin_net::PeerHub::new();
    let keeper = tor.keeper_waiting(&peers, Duration::from_millis(50)).await;
    keeper.add_p2p_onion(&dir, p2p_bind(), 18444).await.unwrap();
    tor.restart_with_new_cookie(2);
    wait_for("Tor goes away", || peers.announced_p2p_onion().is_none()).await;
    for _ in 0..20 {
        keeper
            .add_named_onion(&dir, "electrum", wallet_bind(), Box::new(|_| {}))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        tor.sessions() >= 2,
        "the reconnect is due 50ms after the drop, whatever is asked meanwhile"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reconnect_waits_grow_like_core() {
    use rbitcoin_node::tor_control::{next_reconnect_wait, RECONNECT_MAX, RECONNECT_START};
    assert_eq!(RECONNECT_START, Duration::from_secs(1));
    assert_eq!(RECONNECT_MAX, Duration::from_secs(600));
    let second = next_reconnect_wait(RECONNECT_START);
    assert_eq!(second, Duration::from_millis(1500));
    assert_eq!(next_reconnect_wait(second), Duration::from_millis(2250));
    let mut wait = RECONNECT_START;
    for _ in 0..40 {
        wait = next_reconnect_wait(wait);
    }
    assert_eq!(wait, RECONNECT_MAX);
}
