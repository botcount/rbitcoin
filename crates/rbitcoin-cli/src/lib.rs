//! Datadir socket / bearer-token JSON-RPC client for the documented node subset.

#![forbid(unsafe_code)]

use rbitcoin_primitives::Network;
use serde_json::Value;
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Mirrors the non-string parameter positions and names the RPC handlers read
/// in `crates/rbitcoin-rpc/src/methods/`. A handler change that adds, moves,
/// or renames a non-string parameter must add or update the matching row here
/// in the same change, or a new argument ships as a string.
const CONVERT_PARAMS: &[(&str, usize, &str, ParamFormat)] = &[
    ("addconnection", 2, "v2transport", ParamFormat::Json),
    ("addnode", 2, "v2transport", ParamFormat::Json),
    ("addpeeraddress", 1, "port", ParamFormat::Json),
    ("addpeeraddress", 2, "tried", ParamFormat::Json),
    ("decoderawtransaction", 1, "iswitness", ParamFormat::Json),
    ("disconnectnode", 1, "nodeid", ParamFormat::Json),
    ("echojson", 0, "arg0", ParamFormat::Json),
    ("echojson", 0, "args", ParamFormat::Json),
    ("echojson", 1, "arg1", ParamFormat::Json),
    ("echojson", 2, "arg2", ParamFormat::Json),
    ("echojson", 3, "arg3", ParamFormat::Json),
    ("echojson", 4, "arg4", ParamFormat::Json),
    ("echojson", 5, "arg5", ParamFormat::Json),
    ("echojson", 6, "arg6", ParamFormat::Json),
    ("echojson", 7, "arg7", ParamFormat::Json),
    ("echojson", 8, "arg8", ParamFormat::Json),
    ("echojson", 9, "arg9", ParamFormat::Json),
    ("estimaterawfee", 0, "conf_target", ParamFormat::Json),
    ("estimaterawfee", 1, "threshold", ParamFormat::Json),
    ("estimatesmartfee", 0, "conf_target", ParamFormat::Json),
    ("estimatesmartfee", 2, "options", ParamFormat::Json),
    ("generate", 0, "nblocks", ParamFormat::Json),
    ("generate", 1, "maxtries", ParamFormat::Json),
    ("generateblock", 1, "transactions", ParamFormat::Json),
    ("generateblock", 2, "submit", ParamFormat::Json),
    ("generatetoaddress", 0, "nblocks", ParamFormat::Json),
    ("generatetoaddress", 2, "maxtries", ParamFormat::Json),
    ("generatetodescriptor", 0, "num_blocks", ParamFormat::Json),
    ("generatetodescriptor", 2, "maxtries", ParamFormat::Json),
    ("getblock", 1, "verbosity", ParamFormat::Json),
    ("getblock", 1, "verbose", ParamFormat::Json),
    ("getblockhash", 0, "height", ParamFormat::Json),
    ("getblockheader", 1, "verbose", ParamFormat::Json),
    (
        "getblockstats",
        0,
        "hash_or_height",
        ParamFormat::JsonOrString,
    ),
    ("getblockstats", 1, "stats", ParamFormat::Json),
    ("getblocktemplate", 0, "template_request", ParamFormat::Json),
    ("getmempoolancestors", 1, "verbose", ParamFormat::Json),
    ("getmempooldescendants", 1, "verbose", ParamFormat::Json),
    ("getnetworkhashps", 0, "nblocks", ParamFormat::Json),
    ("getnetworkhashps", 1, "height", ParamFormat::Json),
    ("getnodeaddresses", 0, "count", ParamFormat::Json),
    ("getorphantxs", 0, "verbosity", ParamFormat::Json),
    ("getrawmempool", 0, "verbose", ParamFormat::Json),
    ("getrawmempool", 1, "mempool_sequence", ParamFormat::Json),
    ("getrawtransaction", 1, "verbose", ParamFormat::Json),
    ("getrawtransaction", 1, "verbosity", ParamFormat::Json),
    ("gettxout", 1, "n", ParamFormat::Json),
    ("gettxout", 2, "include_mempool", ParamFormat::Json),
    ("gettxspendingprevout", 0, "outputs", ParamFormat::Json),
    ("importmempool", 1, "options", ParamFormat::Json),
    // The three bools share positional slot 1 with `options` in the handler, so
    // they are named-only: a later positional is not theirs and stays a string.
    ("importmempool", 1, "use_current_time", ParamFormat::Json),
    (
        "importmempool",
        1,
        "apply_fee_delta_priority",
        ParamFormat::Json,
    ),
    (
        "importmempool",
        1,
        "apply_unbroadcast_set",
        ParamFormat::Json,
    ),
    ("logging", 0, "include", ParamFormat::Json),
    ("logging", 1, "exclude", ParamFormat::Json),
    ("mockscheduler", 0, "delta_seconds", ParamFormat::Json),
    ("prioritisetransaction", 1, "dummy", ParamFormat::Json),
    ("prioritisetransaction", 2, "fee_delta", ParamFormat::Json),
    (
        "sendrawtransaction",
        1,
        "maxfeerate",
        ParamFormat::JsonOrString,
    ),
    (
        "sendrawtransaction",
        2,
        "maxburnamount",
        ParamFormat::JsonOrString,
    ),
    ("setban", 2, "bantime", ParamFormat::Json),
    ("setban", 3, "absolute", ParamFormat::Json),
    ("setmocktime", 0, "timestamp", ParamFormat::Json),
    ("stop", 0, "wait", ParamFormat::Json),
    ("submitpackage", 0, "package", ParamFormat::Json),
    ("submitpackage", 1, "maxfeerate", ParamFormat::JsonOrString),
    (
        "submitpackage",
        2,
        "maxburnamount",
        ParamFormat::JsonOrString,
    ),
    ("testmempoolaccept", 0, "rawtxs", ParamFormat::Json),
    (
        "testmempoolaccept",
        1,
        "maxfeerate",
        ParamFormat::JsonOrString,
    ),
    ("waitforblock", 1, "timeout", ParamFormat::Json),
    ("waitforblockheight", 0, "height", ParamFormat::Json),
    ("waitforblockheight", 1, "timeout", ParamFormat::Json),
    ("waitfornewblock", 0, "timeout", ParamFormat::Json),
];

const RPCWAIT_TIMEOUT: Duration = Duration::from_secs(30);
const RPCWAIT_DELAY: Duration = Duration::from_secs(1);
/// Core's `-rpcclienttimeout` default. The unix socket has no default.
const TCP_CLIENT_TIMEOUT: Duration = Duration::from_secs(900);
/// The shortest connect attempt: an attempt at the `--rpcwait` limit still
/// dials once.
const MIN_DIAL: Duration = Duration::from_millis(1);
/// Every long option and what it does, in `usage()` order. This table is the
/// parser: `--name` and Core's `-name` both look the name up here, so a new
/// option takes the single dash without a second list.
const LONG_OPTIONS: &[(&str, LongOpt)] = &[
    (
        "datadir",
        LongOpt::Value(|c, v| {
            c.datadir = PathBuf::from(v);
            Ok(())
        }),
    ),
    (
        "rpc-socket",
        LongOpt::Value(|c, v| {
            c.socket = Some(PathBuf::from(v));
            Ok(())
        }),
    ),
    (
        "network",
        LongOpt::Value(|c, v| {
            c.network = Network::parse(v).map_err(|e| format!("invalid --network: {e}"))?;
            Ok(())
        }),
    ),
    (
        "rpc-url",
        LongOpt::Value(|c, v| {
            c.rpc_url = Some(v.to_string());
            Ok(())
        }),
    ),
    (
        "rpc-token-file",
        LongOpt::Value(|c, v| {
            c.token_file = Some(PathBuf::from(v));
            Ok(())
        }),
    ),
    ("help", LongOpt::Help),
    ("version", LongOpt::Version),
    ("stdin", LongOpt::Switch(|c| c.stdin = true)),
    ("rpcwait", LongOpt::Switch(|c| c.rpcwait = true)),
    (
        "rpcclienttimeout",
        LongOpt::Value(|c, v| {
            c.rpcclienttimeout = TimeoutArg::parse("rpcclienttimeout", v)?;
            Ok(())
        }),
    ),
    (
        "rpcwaittimeout",
        LongOpt::Value(|c, v| {
            c.rpcwaittimeout = TimeoutArg::parse("rpcwaittimeout", v)?;
            Ok(())
        }),
    ),
    ("named", LongOpt::Switch(|c| c.named = true)),
    ("getinfo", LongOpt::Switch(|c| c.getinfo = true)),
];

/// What one long option does when it parses.
#[derive(Clone, Copy)]
enum LongOpt {
    Help,
    Version,
    /// Takes no value.
    Switch(fn(&mut CliConfig)),
    /// Takes `=VALUE` or the next argument.
    Value(fn(&mut CliConfig, &str) -> Result<(), String>),
}

#[doc(hidden)]
pub fn long_option_names() -> impl Iterator<Item = &'static str> {
    LONG_OPTIONS.iter().map(|(name, _)| *name)
}

fn long_option(name: &str) -> Option<LongOpt> {
    LONG_OPTIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, opt)| *opt)
}

#[derive(Clone, Copy)]
enum ParamFormat {
    Json,
    JsonOrString,
}

#[doc(hidden)]
pub fn usage() -> String {
    format!(
        "rbitcoin-cli {} — usage: rbitcoin-cli [OPTIONS] <COMMAND> [PARAMS...]\n\
         \n\
         Options:\n\
           --datadir PATH         node datadir (default ./datadir); unix socket PATH/rpc.sock\n\
           --rpc-socket PATH      unix socket the node binds with --rpc-socket\n\
           --network NET          mainnet|testnet|testnet4|testnet5|signet|regtest (default TCP port)\n\
           --rpc-url ADDR         plain HTTP host:port (else 127.0.0.1:<network port> if no socket)\n\
           --rpc-token-file PATH  Bearer token (default PATH/rpc.token from --datadir)\n\
           -h, --help             this message\n\
           -V, --version          print version\n\
           --stdin                read extra args from stdin, one per line until EOF\n\
           --rpcwait              retries a refused connection until --rpcwaittimeout\n\
           --rpcclienttimeout N   seconds an RPC may wait for its reply, 0 for none\n\
                                  (default 900 on TCP, none on the unix socket)\n\
           --rpcwaittimeout N     seconds --rpcwait retries, 0 for no limit (default 30)\n\
                                  (a negative N is refused; Core reads it as no limit)\n\
           --named                pass named (name=value) parameters instead of positional\n\
           --getinfo              print a Core-style chain/network summary\n\
         \n\
         Each option also takes Core's single dash (-rpcwait, -datadir=PATH).\n\
         Options are read anywhere before --, so a parameter spelled like an\n\
         option (-named) goes after --. A switch takes no =value.\n\
         \n\
         Local: --datadir talks to {{datadir}}/rpc.sock (no token). TCP uses Bearer\n\
         from {{datadir}}/rpc.token. Plain HTTP, same as the node.",
        env!("CARGO_PKG_VERSION")
    )
}

#[doc(hidden)]
pub struct CliConfig {
    pub datadir: PathBuf,
    pub network: Network,
    pub rpc_url: Option<String>,
    pub socket: Option<PathBuf>,
    pub token_file: Option<PathBuf>,
    pub command: Option<String>,
    pub params: Vec<String>,
    pub stdin: bool,
    pub rpcwait: bool,
    pub rpcclienttimeout: TimeoutArg,
    pub rpcwaittimeout: TimeoutArg,
    pub named: bool,
    pub getinfo: bool,
}

/// A seconds option where `0` means no limit, as Core reads it.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeoutArg {
    #[default]
    Omitted,
    Unlimited,
    After(Duration),
}

impl TimeoutArg {
    fn parse(flag: &str, raw: &str) -> Result<Self, String> {
        let secs: u64 = raw
            .parse()
            .map_err(|_| format!("invalid --{flag}: `{raw}` (seconds, 0 for no limit)"))?;
        Ok(match secs {
            0 => Self::Unlimited,
            n => Self::After(Duration::from_secs(n)),
        })
    }

    fn or(self, default: Option<Duration>) -> Option<Duration> {
        match self {
            Self::Omitted => default,
            Self::Unlimited => None,
            Self::After(d) => Some(d),
        }
    }
}

/// The read and write timeout of one call: `--rpcclienttimeout`, else 900 s
/// on TCP and none on the unix socket. `None` is no limit.
#[doc(hidden)]
pub fn client_timeout(arg: TimeoutArg, tcp: bool) -> Option<Duration> {
    arg.or(tcp.then_some(TCP_CLIENT_TIMEOUT))
}

/// How long a connect may take, and the option that sets it: the client
/// timeout or what is left of `--rpcwait`, whichever is shorter.
#[doc(hidden)]
pub fn connect_bound(
    client: Option<Duration>,
    wait_left: Option<Duration>,
) -> Option<(Duration, &'static str)> {
    match (client, wait_left) {
        (Some(c), Some(w)) if w < c => Some((w, "--rpcwaittimeout")),
        (Some(c), _) => Some((c, "--rpcclienttimeout")),
        (None, w) => w.map(|w| (w, "--rpcwaittimeout")),
    }
}

type Addrs = std::io::Result<Vec<SocketAddr>>;

/// A host name lookup on its own thread. A lookup cannot be cancelled, so
/// one that outlives an attempt's bound stays here and the next
/// `--rpcwait` attempt waits on it instead of starting another.
#[doc(hidden)]
#[derive(Default)]
pub struct HostLookup(Option<std::sync::mpsc::Receiver<Addrs>>);

impl HostLookup {
    /// The addresses, or `None` once `limit` passes. `lookup` runs only
    /// when no earlier lookup is in flight.
    pub fn wait(
        &mut self,
        limit: Option<Duration>,
        lookup: impl FnOnce() -> Addrs + Send + 'static,
    ) -> std::io::Result<Option<Vec<SocketAddr>>> {
        use std::sync::mpsc::RecvTimeoutError;
        let rx = match self.0.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = std::sync::mpsc::channel();
                // Not joined: a late lookup finishes on its own.
                let _detached = std::thread::Builder::new()
                    .name("rbitcoin-cli-lookup".into())
                    .spawn(move || {
                        let _ = tx.send(lookup());
                    })?;
                rx
            }
        };
        let got = match limit {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(limit) => rx.recv_timeout(limit),
        };
        match got {
            Ok(addrs) => addrs.map(Some),
            Err(RecvTimeoutError::Timeout) => {
                self.0 = Some(rx);
                Ok(None)
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(std::io::Error::other("the name lookup thread ended"))
            }
        }
    }
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            datadir: PathBuf::from(".").join("datadir"),
            network: Network::Mainnet,
            rpc_url: None,
            socket: None,
            token_file: None,
            command: None,
            params: Vec::new(),
            stdin: false,
            rpcwait: false,
            rpcclienttimeout: TimeoutArg::Omitted,
            rpcwaittimeout: TimeoutArg::Omitted,
            named: false,
            getinfo: false,
        }
    }
}

#[doc(hidden)]
pub enum Action {
    Help,
    Version,
    Call(CliConfig),
}

enum RpcStream {
    Unix(socket2::Socket),
    Tcp {
        stream: TcpStream,
        host: String,
        port: u16,
    },
}

#[doc(hidden)]
pub fn take_value(args: &[OsString], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[doc(hidden)]
pub fn flag_name_and_eq(raw: &str) -> Option<(&str, Option<&str>)> {
    let rest = raw.strip_prefix("--")?;
    match rest.split_once('=') {
        Some((name, val)) => Some((name, Some(val))),
        None => Some((rest, None)),
    }
}

fn is_negative_number(a: &str) -> bool {
    a.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

/// Core's single-dash spelling of a known long option (`-rpcwait`,
/// `-datadir=PATH`) as its `--` form. Any other single-dash word, `-h`, `-V`
/// and a negative number are left alone.
fn core_single_dash(a: &str) -> Option<String> {
    let rest = a.strip_prefix('-').filter(|r| !r.starts_with('-'))?;
    let name = rest.split_once('=').map_or(rest, |(name, _)| name);
    long_option(name).map(|_| format!("-{a}"))
}

/// Applies one `--name`, `--name=value` or `--name value` option. `i` moves
/// past a space-separated value. `Some` ends parsing with that action.
fn apply_long_option(
    cfg: &mut CliConfig,
    args: &[OsString],
    i: &mut usize,
    name: &str,
    eq: Option<&str>,
) -> Result<Option<Action>, String> {
    let opt = long_option(name).ok_or_else(|| format!("unknown argument `--{name}`"))?;
    let set = match opt {
        LongOpt::Value(set) => set,
        _ if eq.is_some() => return Err(format!("--{name} takes no value")),
        LongOpt::Help => return Ok(Some(Action::Help)),
        LongOpt::Version => return Ok(Some(Action::Version)),
        LongOpt::Switch(set) => {
            set(cfg);
            return Ok(None);
        }
    };
    let val = match eq {
        Some("") => return Err(format!("--{name} requires a value")),
        Some(v) => v.to_string(),
        None => take_value(args, i, &format!("--{name}"))?,
    };
    set(cfg, &val)?;
    Ok(None)
}

#[doc(hidden)]
pub fn parse_args(args: &[OsString]) -> Result<Action, String> {
    let mut cfg = CliConfig::default();
    let mut positional_only = false;
    let mut i = 1usize;
    while i < args.len() {
        let mut a = args[i].to_string_lossy();
        if !positional_only {
            if let Some(long) = core_single_dash(a.as_ref()) {
                a = long.into();
            }
            if a == "-h" {
                return Ok(Action::Help);
            }
            if a == "-V" {
                return Ok(Action::Version);
            }
            if a == "--" {
                positional_only = true;
                i += 1;
                continue;
            }
            if a.starts_with('-') && !a.starts_with("--") && !is_negative_number(a.as_ref()) {
                return Err(format!("unknown argument `{a}`"));
            }
            if let Some((name, eq)) = flag_name_and_eq(a.as_ref()) {
                if let Some(action) = apply_long_option(&mut cfg, args, &mut i, name, eq)? {
                    return Ok(action);
                }
                i += 1;
                continue;
            }
        }
        if cfg.command.is_none() {
            cfg.command = Some(a.into_owned());
        } else {
            cfg.params.push(a.into_owned());
        }
        i += 1;
    }
    Ok(Action::Call(cfg))
}

#[doc(hidden)]
pub fn socket_path(cfg: &CliConfig) -> PathBuf {
    cfg.socket
        .clone()
        .unwrap_or_else(|| cfg.datadir.join("rpc.sock"))
}

#[doc(hidden)]
pub fn token_path(cfg: &CliConfig) -> PathBuf {
    cfg.token_file
        .clone()
        .unwrap_or_else(|| cfg.datadir.join("rpc.token"))
}

#[doc(hidden)]
pub fn parse_rpc_addr(addr: &str) -> Result<(String, u16), String> {
    let bad = || format!("--rpc-url takes host:port (plain HTTP, no scheme; got {addr})");
    if addr.contains("://") || addr.contains('/') || addr.contains('@') {
        return Err(bad());
    }
    let (raw_host, port_s) = addr.rsplit_once(':').ok_or_else(bad)?;
    let host = if let Some(inner) = raw_host.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or_else(bad)?;
        if !inner.contains(':') {
            return Err(bad());
        }
        inner
    } else if raw_host.contains(':') {
        return Err(bad());
    } else {
        raw_host
    };
    if host.is_empty() || !acceptable_host(host) {
        return Err(bad());
    }
    let port: u16 = port_s
        .parse()
        .map_err(|_| format!("invalid --rpc-url port in {addr}"))?;
    Ok((host.to_string(), port))
}

/// A strict IP, a scoped IPv6 address, or a DNS name. inet_aton spellings
/// (`010.0.0.1`, `0x7f.0.0.1`) are rejected so libc does not retarget them.
fn acceptable_host(host: &str) -> bool {
    if let Some((addr, zone)) = host.split_once('%') {
        return addr.parse::<Ipv6Addr>().is_ok() && valid_zone(zone);
    }
    host.parse::<IpAddr>().is_ok() || !inet_aton_literal(host)
}

fn valid_zone(zone: &str) -> bool {
    !zone.is_empty()
        && !zone.contains('%')
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn inet_aton_literal(host: &str) -> bool {
    host.split('.').all(inet_aton_label)
}

fn inet_aton_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let Some(hex) = label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"))
    else {
        return false;
    };
    !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[doc(hidden)]
#[derive(Debug)]
pub struct ConnectError {
    pub message: String,
    pub transient: bool,
}

impl ConnectError {
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
        }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
        }
    }

    fn from_io(message: impl FnOnce() -> String, e: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        let transient = matches!(
            e.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::NotFound
                | ErrorKind::TimedOut
                | ErrorKind::WouldBlock
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
        );
        Self {
            message: message(),
            transient,
        }
    }
}

/// `now + d`, or an error for a `d` the clock cannot hold.
fn deadline_after(d: Duration, opt: &str) -> Result<Instant, ConnectError> {
    Instant::now().checked_add(d).ok_or_else(|| {
        ConnectError::permanent(format!(
            "{opt}={} is too large (0 means no limit)",
            d.as_secs()
        ))
    })
}

/// What is left before `deadline`, never under [`MIN_DIAL`].
fn left_until(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(MIN_DIAL)
}

/// A connect error that names the option whose time ran out.
fn dial_error(target: &str, e: &std::io::Error, bound: Option<(Duration, &str)>) -> ConnectError {
    use std::io::ErrorKind;
    ConnectError::from_io(
        || match (e.kind(), bound) {
            (ErrorKind::TimedOut | ErrorKind::WouldBlock, Some((_, opt))) => {
                format!("connect {target}: {opt} expired: {e}")
            }
            _ => format!("connect {target}: {e}"),
        },
        e,
    )
}

/// The host name lookup, bounded like the connect.
fn resolve_tcp(
    host: &str,
    port: u16,
    deadline: Option<(Instant, &'static str)>,
    lookups: &mut HostLookup,
) -> Result<Vec<SocketAddr>, ConnectError> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let target = format!("{host}:{port}");
    let lookup = {
        let target = target.clone();
        move || target.to_socket_addrs().map(Iterator::collect)
    };
    let (limit, opt) = deadline.map_or((None, ""), |(at, opt)| (Some(left_until(at)), opt));
    lookups
        .wait(limit, lookup)
        .map_err(|e| dial_error(&target, &e, None))?
        .ok_or_else(|| {
            ConnectError::transient(format!(
                "connect {target}: {opt} expired in the name lookup"
            ))
        })
}

fn connect_tcp(
    host: &str,
    port: u16,
    bound: Option<(Duration, &'static str)>,
    lookups: &mut HostLookup,
) -> Result<TcpStream, ConnectError> {
    let target = format!("{host}:{port}");
    let deadline = match bound {
        None => None,
        Some((d, opt)) => Some((deadline_after(d, opt)?, opt)),
    };
    let mut last = None;
    for addr in resolve_tcp(host, port, deadline, lookups)? {
        let dial = match deadline {
            None => TcpStream::connect(addr),
            Some((at, _)) => TcpStream::connect_timeout(&addr, left_until(at)),
        };
        match dial {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(dial_error(&target, &e, bound)),
        }
    }
    Err(last.unwrap_or_else(|| ConnectError::permanent(format!("connect {target}: no address"))))
}

#[doc(hidden)]
pub fn http_host_value(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[doc(hidden)]
pub fn read_token(path: &Path) -> Result<String, String> {
    let line =
        std::fs::read_to_string(path).map_err(|e| format!("read token {}: {e}", path.display()))?;
    let t = line.trim();
    if t.is_empty() {
        return Err(format!("token file {}: empty", path.display()));
    }
    Ok(t.to_string())
}

#[doc(hidden)]
pub fn read_stdin_args(reader: impl BufRead) -> Result<Vec<String>, String> {
    reader
        .lines()
        .map(|line| line.map_err(|e| format!("read stdin: {e}")))
        .collect()
}

#[doc(hidden)]
pub fn apply_stdin_args(cfg: &mut CliConfig, extra: Vec<String>) {
    let mut extra = extra.into_iter();
    if cfg.command.is_none() {
        cfg.command = extra.next();
    }
    cfg.params.extend(extra);
}

/// Retry a transient connect error every `delay` until `limit` has passed
/// (`None`: forever), as Core's `-rpcwaittimeout` does. The last pause is
/// cut short so the last attempt lands on the limit. Each attempt gets the
/// time left, to bound its own connect.
#[doc(hidden)]
pub fn retry_connect<T>(
    mut attempt: impl FnMut(Option<Duration>) -> Result<T, ConnectError>,
    limit: Option<Duration>,
    delay: Duration,
) -> Result<T, String> {
    let start = Instant::now();
    let left = || limit.map(|l| l.saturating_sub(start.elapsed()));
    loop {
        let e = match attempt(left()) {
            Ok(value) => return Ok(value),
            Err(e) if !e.transient => return Err(e.message),
            Err(e) => e,
        };
        let pause = match left() {
            None => delay,
            Some(Duration::ZERO) => return Err(e.message),
            Some(left) => delay.min(left),
        };
        std::thread::sleep(pause);
    }
}

fn convert_format(method: &str, index: usize) -> Option<ParamFormat> {
    for &(m, i, _name, f) in CONVERT_PARAMS {
        if m == method && i == index {
            return Some(f);
        }
    }
    None
}

fn convert_named_format(method: &str, name: &str) -> Option<ParamFormat> {
    for &(m, _i, n, f) in CONVERT_PARAMS {
        if m == method && n == name {
            return Some(f);
        }
    }
    None
}

#[doc(hidden)]
pub fn param_value(method: &str, index: usize, raw: &str) -> Result<Value, String> {
    param_convert(convert_format(method, index), raw)
}

fn named_param_value(method: &str, name: &str, raw: &str) -> Result<Value, String> {
    param_convert(convert_named_format(method, name), raw)
}

fn param_convert(format: Option<ParamFormat>, raw: &str) -> Result<Value, String> {
    match format {
        None => Ok(Value::String(raw.to_string())),
        Some(ParamFormat::Json) => {
            serde_json::from_str(raw).map_err(|_| format!("Error parsing JSON: {raw}"))
        }
        Some(ParamFormat::JsonOrString) => {
            Ok(serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())))
        }
    }
}

fn is_param_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn named_params(method: &str, params: &[String]) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    let mut positional = Vec::new();
    for raw in params {
        match raw.split_once('=') {
            Some((name, value)) if is_param_name(name) => {
                map.insert(name.to_string(), named_param_value(method, name, value)?);
            }
            _ => positional.push(param_value(method, positional.len(), raw)?),
        }
    }
    // Core's RPCConvertNamedValues pushKVEnd's the positional list, so an
    // explicit `args=` keeps its place and is the one the server reads.
    if !positional.is_empty() && !map.contains_key("args") {
        map.insert("args".to_string(), Value::Array(positional));
    }
    Ok(Value::Object(map))
}

#[doc(hidden)]
pub fn rpc_http(
    mut stream: impl Read + Write,
    host: &str,
    port: u16,
    auth: Option<&str>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let body = serde_json::json!({
        "jsonrpc": "1.0",
        "id": "1",
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let auth_h = match auth {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let host_hdr = http_host_value(host, port);
    let req = format!(
        "POST / HTTP/1.1\r\n\
         Host: {host_hdr}\r\n\
         {auth_h}\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    let mut wire = req.into_bytes();
    wire.extend_from_slice(&body);
    stream.write_all(&wire).map_err(|e| io_error("write", &e))?;
    let (status, resp_body) = read_http(&mut stream)?;
    if status == 401 {
        return Err("RPC unauthorized (check rpc.token or --rpc-token-file)".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {resp_body}"));
    }
    let v: Value = serde_json::from_str(&resp_body).map_err(|e| format!("rpc json: {e}"))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("rpc error");
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        return Err(format!("RPC error {code}: {msg}"));
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

#[doc(hidden)]
/// A unix connect waits on a full listen queue for the send timeout, so
/// `bound` is set as that timeout first.
pub fn connect_unix(
    path: &Path,
    bound: Option<(Duration, &str)>,
) -> Result<socket2::Socket, ConnectError> {
    let sock = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
        .map_err(|e| ConnectError::from_io(|| format!("unix socket: {e}"), &e))?;
    let addr = socket2::SockAddr::unix(path)
        .map_err(|e| ConnectError::permanent(format!("unix addr: {e}")))?;
    sock.set_write_timeout(bound.map(|(d, _)| d.max(MIN_DIAL)))
        .map_err(|e| ConnectError::permanent(format!("socket timeout: {e}")))?;
    sock.connect(&addr)
        .map_err(|e| dial_error(&path.display().to_string(), &e, bound))?;
    Ok(sock)
}

/// A socket timeout reads `EAGAIN` on unix and `TimedOut` on Windows.
fn io_error(op: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            format!("{op}: --rpcclienttimeout expired: {e}")
        }
        _ => format!("{op}: {e}"),
    }
}

#[doc(hidden)]
pub fn read_http(stream: &mut impl Read) -> Result<(u16, String), String> {
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| io_error("read", &e))?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or("invalid HTTP response")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

#[doc(hidden)]
pub fn format_result(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn connect_stream(
    cfg: &CliConfig,
    tcp: Option<&(String, u16)>,
    wait_left: Option<Duration>,
    lookups: &mut HostLookup,
) -> Result<RpcStream, ConnectError> {
    let sock = socket_path(cfg);
    if cfg.rpc_url.is_none() && (cfg.socket.is_some() || sock.exists()) {
        let timeout = client_timeout(cfg.rpcclienttimeout, false);
        let stream = connect_unix(&sock, connect_bound(timeout, wait_left))?;
        set_request_timeout((&stream).into(), timeout)?;
        Ok(RpcStream::Unix(stream))
    } else {
        let (host, port) = match tcp {
            Some((h, p)) => (h.clone(), *p),
            None => ("127.0.0.1".into(), cfg.network.default_rpc_port()),
        };
        let timeout = client_timeout(cfg.rpcclienttimeout, true);
        let stream = connect_tcp(&host, port, connect_bound(timeout, wait_left), lookups)?;
        set_request_timeout((&stream).into(), timeout)?;
        Ok(RpcStream::Tcp { stream, host, port })
    }
}

/// `None` clears the timeout: a zero `Duration` is refused by the socket.
fn set_request_timeout(
    sock: socket2::SockRef<'_>,
    timeout: Option<Duration>,
) -> Result<(), ConnectError> {
    sock.set_read_timeout(timeout)
        .and_then(|()| sock.set_write_timeout(timeout))
        .map_err(|e| ConnectError::permanent(format!("socket timeout: {e}")))
}

fn rpc_call(
    stream: RpcStream,
    cfg: &CliConfig,
    cmd: &str,
    params: &Value,
) -> Result<Value, String> {
    match stream {
        RpcStream::Unix(s) => rpc_http(s, "localhost", 0, None, cmd, params),
        RpcStream::Tcp { stream, host, port } => {
            let token = read_token(&token_path(cfg))?;
            rpc_http(stream, &host, port, Some(&token), cmd, params)
        }
    }
}

#[doc(hidden)]
pub fn dispatch_call(cfg: &CliConfig) -> Result<String, String> {
    if cfg.getinfo {
        if cfg.command.is_some() || !cfg.params.is_empty() {
            return Err("--getinfo takes no arguments".to_string());
        }
        return getinfo(cfg);
    }
    let cmd = cfg.command.as_deref().ok_or_else(usage)?;
    if cmd == "help" {
        return Ok(usage());
    }
    let params = if cfg.named {
        named_params(cmd, &cfg.params)?
    } else {
        let positional: Vec<Value> = cfg
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| param_value(cmd, i, p))
            .collect::<Result<_, _>>()?;
        Value::Array(positional)
    };
    let stream = connect(cfg)?;
    Ok(format_result(&rpc_call(stream, cfg, cmd, &params)?))
}

/// Requested output on stdout. A reader that closed the pipe (`| head -1`)
/// is not an error; any other write error is exit 1.
fn print_stdout(text: &str) -> ExitCode {
    match writeln!(std::io::stdout().lock(), "{text}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: stdout: {e}");
            ExitCode::from(1)
        }
    }
}

fn connect(cfg: &CliConfig) -> Result<RpcStream, String> {
    let tcp = cfg.rpc_url.as_deref().map(parse_rpc_addr).transpose()?;
    let mut lookups = HostLookup::default();
    let mut attempt = |wait_left| connect_stream(cfg, tcp.as_ref(), wait_left, &mut lookups);
    if cfg.rpcwait {
        let limit = cfg.rpcwaittimeout.or(Some(RPCWAIT_TIMEOUT));
        retry_connect(attempt, limit, RPCWAIT_DELAY)
    } else {
        attempt(None).map_err(|e| e.message)
    }
}

fn getinfo(cfg: &CliConfig) -> Result<String, String> {
    let chain = rpc_call(
        connect(cfg)?,
        cfg,
        "getblockchaininfo",
        &Value::Array(Vec::new()),
    )?;
    let network = rpc_call(
        connect(cfg)?,
        cfg,
        "getnetworkinfo",
        &Value::Array(Vec::new()),
    )?;
    format_getinfo(&chain, &network)
}

fn format_getinfo(chain: &Value, network: &Value) -> Result<String, String> {
    let chain_name = getinfo_field(chain, "chain")?
        .as_str()
        .ok_or_else(|| "getinfo: chain not a string".to_string())?;
    let blocks = getinfo_field(chain, "blocks")?.to_string();
    let headers = getinfo_field(chain, "headers")?.to_string();
    let progress = getinfo_field(chain, "verificationprogress")?
        .as_f64()
        .ok_or_else(|| "getinfo: verificationprogress not a number".to_string())?;
    let difficulty = getinfo_field(chain, "difficulty")?.to_string();
    let connections_in = getinfo_field(network, "connections_in")?.to_string();
    let connections_out = getinfo_field(network, "connections_out")?.to_string();
    let connections = getinfo_field(network, "connections")?.to_string();
    let version = getinfo_field(network, "version")?.to_string();
    let timeoffset = getinfo_field(network, "timeoffset")?.to_string();
    let relayfee = getinfo_field(network, "relayfee")?
        .as_f64()
        .ok_or_else(|| "getinfo: relayfee not a number".to_string())?;
    let bar = if progress < 0.99 {
        progress_bar(progress)
    } else {
        String::new()
    };
    let percent = progress * 100.0;
    let proxies = format_proxies(network);
    let warnings = format_warnings(network);
    Ok(format!(
        "Chain: {chain_name}\n\
         Blocks: {blocks}\n\
         Headers: {headers}\n\
         Verification progress: {bar}{percent:.4}%\n\
         Difficulty: {difficulty}\n\
         \n\
         Network: in {connections_in}, out {connections_out}, total {connections}\n\
         Version: {version}\n\
         Time offset (s): {timeoffset}\n\
         Proxies: {proxies}\n\
         Min tx relay fee rate (BTC/kvB): {relayfee:.8}\n\
         \n\
         Warnings: {warnings}"
    ))
}

#[doc(hidden)]
pub fn progress_bar(progress: f64) -> String {
    const INCREMENT: f64 = 0.05;
    if !(0.0..=1.0).contains(&progress) {
        return String::new();
    }
    let cells = |ratio: f64| {
        let mut n = 0usize;
        while (n as f64) < ratio {
            n += 1;
        }
        n
    };
    format!(
        "{}{} ",
        "\u{2592}".repeat(cells(progress / INCREMENT)),
        "\u{2591}".repeat(cells((1.0 - progress) / INCREMENT))
    )
}

#[doc(hidden)]
pub fn format_proxies(network: &Value) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(nets) = network.get("networks").and_then(Value::as_array) {
        for net in nets {
            let Some(proxy) = net
                .get("proxy")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            let name = net.get("name").and_then(Value::as_str).unwrap_or("");
            match groups.iter().position(|(p, _)| p == proxy) {
                Some(i) => groups[i].1.push(name.to_string()),
                None => groups.push((proxy.to_string(), vec![name.to_string()])),
            }
        }
    }
    if groups.is_empty() {
        return "n/a".to_string();
    }
    let formatted: Vec<String> = groups
        .iter()
        .map(|(proxy, names)| format!("{proxy} ({})", names.join(", ")))
        .collect();
    formatted.join(", ")
}

#[doc(hidden)]
pub fn format_warnings(network: &Value) -> String {
    let warnings: Vec<&str> = network
        .get("warnings")
        .and_then(Value::as_array)
        .map(|w| w.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if warnings.is_empty() {
        "(none)".to_string()
    } else {
        warnings.join(", ")
    }
}

fn getinfo_field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, String> {
    v.get(key).ok_or_else(|| format!("getinfo: missing {key}"))
}

/// Process entry used by `main` and high-level scenarios.
pub fn cli_main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let action = match parse_args(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match action {
        Action::Help => print_stdout(&usage()),
        Action::Version => print_stdout(concat!("rbitcoin-cli ", env!("CARGO_PKG_VERSION"))),
        Action::Call(mut cfg) => {
            if cfg.stdin {
                match read_stdin_args(std::io::stdin().lock()) {
                    Ok(extra) => apply_stdin_args(&mut cfg, extra),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            match dispatch_call(&cfg) {
                Ok(out) => print_stdout(&out),
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}
