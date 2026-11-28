use rbitcoin_cli::*;
use rbitcoin_primitives::Network;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use std::net::TcpListener;

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

type CapturedCall = Arc<Mutex<Option<(String, serde_json::Value)>>>;

fn exit_ok(c: ExitCode) -> bool {
    format!("{c:?}") == format!("{:?}", ExitCode::SUCCESS)
}

fn tmp_datadir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("rbitcoin-cli-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_rpc_mock(token: &str, result_json: &str) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expect = format!("Bearer {token}");
    let result = result_json.to_string();
    let h = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut tmp = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        let req = String::from_utf8_lossy(&raw);
        let authorized = req.lines().any(|line| {
            line.trim()
                .strip_prefix("Authorization:")
                .map(|r| r.trim().eq_ignore_ascii_case(&expect))
                .unwrap_or(false)
        });
        let body = if authorized {
            format!("{{\"jsonrpc\":\"1.0\",\"id\":\"1\",\"result\":{result},\"error\":null}}\n")
        } else {
            String::new()
        };
        if authorized {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        } else {
            s.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"jsonrpc\"\r\nContent-Length: 13\r\nConnection: close\r\n\r\nUnauthorized\n").unwrap();
        }
    });
    (port, h)
}

fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[cfg(unix)]
fn reply_rpc_error(mut stream: impl Read + Write) {
    let body = "{\"jsonrpc\":\"1.0\",\"id\":\"1\",\"result\":null,\"error\":{\"code\":-1,\"message\":\"boom\"}}\n";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

#[test]
fn rpc_url_is_host_and_port() {
    assert_eq!(
        parse_rpc_addr("127.0.0.1:8332").unwrap(),
        ("127.0.0.1".into(), 8332)
    );
    assert_eq!(
        parse_rpc_addr("rpc.lan:18443").unwrap(),
        ("rpc.lan".into(), 18443)
    );
    assert_eq!(parse_rpc_addr("[::1]:8332").unwrap(), ("::1".into(), 8332));
    assert_eq!(
        parse_rpc_addr("[fe80::1%lo]:8332").unwrap(),
        ("fe80::1%lo".into(), 8332)
    );
    assert_eq!(
        parse_rpc_addr("[fe80::1%a_b-c.d]:8332").unwrap(),
        ("fe80::1%a_b-c.d".into(), 8332)
    );
    assert_eq!(parse_rpc_addr("0x:8332").unwrap(), ("0x".into(), 8332));
    assert_eq!(
        parse_rpc_addr("dead.beef:8332").unwrap(),
        ("dead.beef".into(), 8332)
    );
    assert_eq!(
        parse_rpc_addr("0x10.lan:8332").unwrap(),
        ("0x10.lan".into(), 8332)
    );
    assert_eq!(http_host_value("127.0.0.1", 8332), "127.0.0.1:8332");
    assert_eq!(http_host_value("::1", 8332), "[::1]:8332");
    for bad in [
        "http://127.0.0.1:8332",
        "https://127.0.0.1:8332",
        "127.0.0.1:8332/",
        "user:pass@127.0.0.1:8332",
        "::1:8332",
        "127.0.0.1",
        "010.0.0.1:8332",
        "0x7f.0.0.1:8332",
        "0x7f000001:8332",
        "127.1:8332",
        "2130706433:8332",
        "[fe80::1%]:8332",
    ] {
        let err = parse_rpc_addr(bad).unwrap_err();
        assert!(err.contains("host:port"), "{bad} -> {err}");
    }
    let u = usage();
    assert!(u.contains("--rpc-url ADDR"), "{u}");
    assert!(!u.contains("http://"), "{u}");
}

#[test]
fn param_tokens_json_or_string() {
    assert_eq!(
        param_value("getblockhash", 0, "0").unwrap(),
        serde_json::json!(0)
    );
    assert_eq!(
        param_value("getblock", 0, "0").unwrap(),
        serde_json::json!("0")
    );
    assert_eq!(
        param_value("getblock", 1, "true").unwrap(),
        serde_json::json!(true)
    );
    assert!(param_value("getblockhash", 0, "abc").is_err());
    assert_eq!(
        param_value("getblockstats", 0, "abc").unwrap(),
        serde_json::json!("abc")
    );
}

#[test]
fn setban_bantime_and_absolute_convert() {
    assert_eq!(
        param_value("setban", 0, "10.0.0.1").unwrap(),
        serde_json::json!("10.0.0.1")
    );
    assert_eq!(
        param_value("setban", 2, "3600").unwrap(),
        serde_json::json!(3600)
    );
    assert_eq!(
        param_value("setban", 3, "true").unwrap(),
        serde_json::json!(true)
    );
    assert!(param_value("setban", 2, "abc").is_err());
}

#[test]
fn logging_include_and_exclude_are_json_arrays() {
    assert_eq!(
        param_value("logging", 0, r#"["mempool","p2p"]"#).unwrap(),
        serde_json::json!(["mempool", "p2p"])
    );
    assert_eq!(
        param_value("logging", 1, r#"["all"]"#).unwrap(),
        serde_json::json!(["all"])
    );
}

#[test]
fn equals_form_datadir_is_accepted() {
    let dir = tmp_datadir();
    std::fs::write(dir.join("rpc.token"), "s3cret").unwrap();
    std::fs::write(dir.join("rpc.sock"), b"").unwrap();
    let (port, h) = spawn_rpc_mock("s3cret", "0");
    let flag = format!("--datadir={}", dir.display());
    let code = cli_main([
        "rbitcoin-cli",
        flag.as_str(),
        &format!("--rpc-url=127.0.0.1:{port}"),
        "getblockcount",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(exit_ok(code), "--foo=bar form must parse, got {code:?}");
    let _ = h.join();
}

#[test]
fn usage_describes_every_option() {
    let u = usage();
    for needle in [
        "--datadir PATH",
        "--network NET",
        "mainnet|testnet|testnet4|testnet5|signet|regtest",
        "--rpc-url ADDR",
        "--rpc-token-file PATH",
        "-h, --help",
        "-V, --version",
        "--stdin",
        "--rpcwait",
        "--rpcclienttimeout N",
        "--rpcwaittimeout N",
        "--named",
        "--getinfo",
    ] {
        assert!(u.contains(needle), "usage must list {needle}");
    }
    assert!(
        !u.contains("--rpcuser") && !u.contains("--rpcport") && !u.contains("--rpcconnect"),
        "dropped Core names must not be advertised: {u}"
    );
    assert!(
        u.contains("-V, --version") && u.contains("print version"),
        "version must have a description"
    );
}

#[test]
fn getblockcount_token_against_mock() {
    let dir = tmp_datadir();
    std::fs::write(dir.join("rpc.token"), "s3cret").unwrap();
    let (port, h) = spawn_rpc_mock("s3cret", "0");
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "--rpc-url",
        &format!("127.0.0.1:{port}"),
        "getblockcount",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        exit_ok(code),
        "token getblockcount must succeed against a live RPC mock, got {code:?}"
    );
    let _ = h.join();
}

#[test]
fn missing_auth_is_unauthorized() {
    let (port, _h) = spawn_rpc_mock("alice", "0");
    let dir = tmp_datadir();
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "--rpc-url",
        &format!("127.0.0.1:{port}"),
        "getblockcount",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!exit_ok(code));
}

#[test]
fn default_rpc_port_follows_network() {
    assert_eq!(Network::Mainnet.default_rpc_port(), 8332);
    assert_eq!(Network::Regtest.default_rpc_port(), 18443);
}

/// Serve one JSON-RPC request on `listener`, reply `"result": 7`, and
/// capture the request's method and `params` into the returned slot.
#[cfg(unix)]
fn serve_one_rpc(
    listener: std::os::unix::net::UnixListener,
) -> (CapturedCall, thread::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(None));
    let c = captured.clone();
    let h = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut tmp = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        let header_end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(raw.len());
        let content_length = String::from_utf8_lossy(&raw[..header_end])
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while raw.len() < header_end + content_length {
            let n = s.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        let body = &raw[header_end..header_end + content_length];
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
            let method = v
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            *c.lock().unwrap() = Some((method, v.get("params").cloned().unwrap_or_default()));
        }
        let body = "{\"jsonrpc\":\"1.0\",\"id\":\"1\",\"result\":7,\"error\":null}\n".to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).unwrap();
    });
    (captured, h)
}

#[cfg(unix)]
#[test]
fn unix_socket_needs_no_token() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (_, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "getblockcount",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        exit_ok(code),
        "unix getblockcount must succeed, got {code:?}"
    );
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn rpc_socket_reaches_a_socket_outside_the_datadir() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc-elsewhere.sock");
    let (_, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.join("no-such-datadir").to_str().unwrap(),
        "--rpc-socket",
        sock.to_str().unwrap(),
        "getblockcount",
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        exit_ok(code),
        "--rpc-socket getblockcount must succeed, got {code:?}"
    );
    let _ = h.join();
}

/// The decimal neighbor of `getnetworkhashps_negative_nblocks_is_a_number`,
/// which sends `-1` through `cli_main`.
#[test]
fn parse_args_negative_decimal_is_a_param() {
    match parse_args(&os_args(&["rbitcoin-cli", "getnetworkhashps", "-0.5"])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.params, vec!["-0.5".to_string()]);
        }
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("expected Action::Call, got Help or Version"),
    }
}

#[test]
fn parse_args_dash_alpha_is_still_rejected() {
    for bad in ["-bogus", "-x"] {
        let err = match parse_args(&os_args(&["rbitcoin-cli", bad])) {
            Ok(_) => panic!("{bad} should have been rejected"),
            Err(e) => e,
        };
        assert!(err.contains("unknown argument"), "{bad} -> {err}");
    }
}

#[test]
fn parse_args_double_dash_ends_option_parsing() {
    match parse_args(&os_args(&[
        "rbitcoin-cli",
        "getblock",
        "--",
        "--datadir",
        "x",
    ])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.command.as_deref(), Some("getblock"));
            assert_eq!(cfg.datadir, PathBuf::from(".").join("datadir"));
            assert_eq!(cfg.params, vec!["--datadir".to_string(), "x".to_string()]);
        }
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("expected Action::Call"),
    }
}

#[cfg(unix)]
#[test]
fn getblock_keeps_a_digit_only_hash_a_string() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "getblock",
        "123456789",
        "2",
    ]);
    assert!(exit_ok(code), "getblock must succeed, got {code:?}");
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!(["123456789", 2]));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn getnetworkhashps_negative_nblocks_is_a_number() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "getnetworkhashps",
        "-1",
    ]);
    assert!(exit_ok(code), "getnetworkhashps must succeed, got {code:?}");
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!([-1]));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}
/// Help and version are requested output: stdout, exit 0, nothing on stderr.
#[test]
fn help_and_version_print_to_stdout() {
    for (flag, want) in [
        ("--help", "rbitcoin-cli"),
        ("-h", "rbitcoin-cli"),
        ("--version", env!("CARGO_PKG_VERSION")),
        ("-V", env!("CARGO_PKG_VERSION")),
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_rbitcoin-cli"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag} must exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains(want), "{flag} stdout: {stdout:?}");
        assert!(out.stderr.is_empty(), "{flag} wrote to stderr");
    }
}

/// A reader that already closed stdout is not an error: exit 0, no panic.
#[test]
fn help_and_version_to_a_closed_stdout_exit_zero() {
    for args in [&["--help"][..], &["-h"], &["--version"], &["-V"], &["help"]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_rbitcoin-cli"))
            .args(args)
            .stdout(writer)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {:?} {stderr}", out.status);
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
    }
}

#[test]
fn read_stdin_args_reads_one_per_line_until_eof() {
    assert_eq!(
        read_stdin_args(std::io::Cursor::new(b"pass\nphrase\n")).unwrap(),
        vec!["pass".to_string(), "phrase".to_string()]
    );
    assert!(read_stdin_args(std::io::Cursor::new(b""))
        .unwrap()
        .is_empty());
    assert_eq!(
        read_stdin_args(std::io::Cursor::new(b"a\r\nb\r\n")).unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn stdin_flag_appends_args_from_stdin() {
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_rbitcoin-cli"))
        .arg("--rpc-socket")
        .arg(&sock)
        .arg("--stdin")
        .arg("getblock")
        .arg("123456789")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"2\n").unwrap();
    }
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "rbitcoin-cli --stdin must exit 0, got {status:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();

    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!(["123456789", 2]));
}

#[cfg(unix)]
#[test]
fn stdin_flag_takes_the_method_from_stdin_when_argv_has_none() {
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_rbitcoin-cli"))
        .arg("--rpc-socket")
        .arg(&sock)
        .arg("--stdin")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"getblockcount\n").unwrap();
    }
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "echo getblockcount | rbitcoin-cli --stdin must exit 0, got {status:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();

    let (method, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(method, "getblockcount");
    assert_eq!(params, serde_json::json!([]));
}

#[test]
fn apply_stdin_args_takes_the_method_from_stdin_only_when_argv_has_none() {
    let mut cfg = call_cfg(&["rbitcoin-cli", "--stdin"]);
    apply_stdin_args(&mut cfg, vec!["getblockcount".to_string()]);
    assert_eq!(cfg.command.as_deref(), Some("getblockcount"));
    assert!(cfg.params.is_empty());

    let mut cfg = call_cfg(&["rbitcoin-cli", "--stdin"]);
    apply_stdin_args(
        &mut cfg,
        vec![
            "getblock".to_string(),
            "123456789".to_string(),
            "2".to_string(),
        ],
    );
    assert_eq!(cfg.command.as_deref(), Some("getblock"));
    assert_eq!(cfg.params, vec!["123456789".to_string(), "2".to_string()]);

    let mut cfg = call_cfg(&["rbitcoin-cli", "--stdin", "getblock", "123456789"]);
    apply_stdin_args(&mut cfg, vec!["2".to_string()]);
    assert_eq!(cfg.command.as_deref(), Some("getblock"));
    assert_eq!(cfg.params, vec!["123456789".to_string(), "2".to_string()]);

    let mut cfg = call_cfg(&["rbitcoin-cli", "--stdin"]);
    apply_stdin_args(&mut cfg, Vec::new());
    assert_eq!(cfg.command, None);
    assert!(cfg.params.is_empty());
}

#[test]
fn retry_connect_retries_until_success() {
    let mut calls = 0;
    let start = Instant::now();
    let out = retry_connect(
        |left: Option<Duration>| {
            assert_eq!(left, None);
            calls += 1;
            if calls < 3 {
                Err(ConnectError::transient("connection refused"))
            } else {
                Ok(42)
            }
        },
        None,
        Duration::from_millis(10),
    );
    assert_eq!(out, Ok(42));
    assert_eq!(calls, 3);
    assert!(
        start.elapsed() >= Duration::from_millis(20),
        "no pause between attempts"
    );
}

#[test]
fn retry_connect_gives_up_at_the_wait_limit() {
    let mut calls = 0;
    let mut lefts = Vec::new();
    let start = Instant::now();
    let out = retry_connect(
        |left: Option<Duration>| {
            calls += 1;
            lefts.push(left.expect("a wait limit gives each attempt the time left"));
            Err::<i32, ConnectError>(ConnectError::transient("connection refused"))
        },
        Some(Duration::from_millis(300)),
        Duration::from_millis(50),
    );
    let waited = start.elapsed();
    assert_eq!(out, Err("connection refused".to_string()));
    assert!(calls >= 2, "a transient error must be retried, got {calls}");
    assert!(lefts[0] <= Duration::from_millis(300), "{lefts:?}");
    assert!(lefts.windows(2).all(|w| w[1] < w[0]), "{lefts:?}");
    assert!(
        waited >= Duration::from_millis(300),
        "gave up before the limit: {waited:?}"
    );
    assert!(waited < SLOW, "wait not capped: {waited:?}");
}

/// A bad address or socket path must not be retried: one attempt, then out.
#[test]
fn retry_connect_does_not_retry_a_permanent_error() {
    let mut calls = 0;
    let out = retry_connect(
        |left: Option<Duration>| {
            assert_eq!(left, None);
            calls += 1;
            Err::<i32, ConnectError>(ConnectError::permanent("bad rpc url"))
        },
        None,
        Duration::ZERO,
    );
    assert_eq!(out, Err("bad rpc url".to_string()));
    assert_eq!(calls, 1, "a permanent error must not be retried");
}

fn call_cfg(args: &[&str]) -> CliConfig {
    match parse_args(&os_args(args)) {
        Ok(Action::Call(cfg)) => cfg,
        Err(e) => panic!("{args:?} must parse, got: {e}"),
        Ok(_) => panic!("{args:?}: expected Action::Call"),
    }
}

#[test]
fn parse_args_reads_client_and_wait_timeouts() {
    use TimeoutArg::{After, Omitted, Unlimited};
    let secs = |n| After(Duration::from_secs(n));
    let cases: [(&[&str], TimeoutArg, TimeoutArg); 6] = [
        (&["--rpcclienttimeout=0"], Unlimited, Omitted),
        (&["-rpcclienttimeout=0"], Unlimited, Omitted),
        (&["--rpcclienttimeout", "5"], secs(5), Omitted),
        (&["--rpcwaittimeout=7"], Omitted, secs(7)),
        (&["-rpcwaittimeout=0", "--rpcwait"], Omitted, Unlimited),
        (&[], Omitted, Omitted),
    ];
    for (flags, client, wait) in cases {
        let mut args = vec!["rbitcoin-cli"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["importmempool", "mempool.dat"]);
        let cfg = call_cfg(&args);
        assert_eq!(cfg.rpcclienttimeout, client, "{flags:?}");
        assert_eq!(cfg.rpcwaittimeout, wait, "{flags:?}");
        assert_eq!(cfg.command.as_deref(), Some("importmempool"), "{flags:?}");
        assert_eq!(cfg.params, vec!["mempool.dat".to_string()], "{flags:?}");
    }
}

#[test]
fn parse_args_refuses_a_bad_timeout() {
    for (flag, want) in [
        ("--rpcclienttimeout", "--rpcclienttimeout requires a value"),
        ("-rpcwaittimeout=", "--rpcwaittimeout requires a value"),
        ("--rpcwaittimeout=-1", "invalid --rpcwaittimeout"),
        ("--rpcclienttimeout=1.5", "invalid --rpcclienttimeout"),
        (
            "-rpcclienttimeouts=1",
            "unknown argument `-rpcclienttimeouts=1`",
        ),
    ] {
        let err = match parse_args(&os_args(&["rbitcoin-cli", flag])) {
            Ok(_) => panic!("{flag} should have been rejected"),
            Err(e) => e,
        };
        assert!(err.contains(want), "{flag} -> {err}");
    }
}

#[test]
fn parse_args_takes_core_single_dash_for_any_option() {
    let cfg = call_cfg(&[
        "rbitcoin-cli",
        "-rpcwait",
        "-named",
        "-stdin",
        "-getinfo",
        "-datadir=/d",
        "-network",
        "regtest",
        "-rpc-url=127.0.0.1:18443",
        "getnetworkhashps",
        "-1",
    ]);
    assert!(cfg.rpcwait && cfg.named && cfg.stdin && cfg.getinfo);
    assert_eq!(cfg.datadir, PathBuf::from("/d"));
    assert_eq!(cfg.network, Network::Regtest);
    assert_eq!(cfg.rpc_url.as_deref(), Some("127.0.0.1:18443"));
    assert_eq!(cfg.command.as_deref(), Some("getnetworkhashps"));
    assert_eq!(cfg.params, vec!["-1".to_string()]);
    let cfg = call_cfg(&["rbitcoin-cli", "-datadir", "/e", "getblockcount"]);
    assert_eq!(cfg.datadir, PathBuf::from("/e"));
    assert_eq!(cfg.command.as_deref(), Some("getblockcount"));
    for (flag, help) in [
        ("-h", true),
        ("-help", true),
        ("-V", false),
        ("-version", false),
    ] {
        match parse_args(&os_args(&["rbitcoin-cli", flag, "getblockcount"])) {
            Ok(Action::Help) => assert!(help, "{flag}"),
            Ok(Action::Version) => assert!(!help, "{flag}"),
            Ok(Action::Call(_)) => panic!("{flag}: expected Help or Version"),
            Err(e) => panic!("{flag}: {e}"),
        }
    }
}

/// Every option in the parser's table parses with one dash and with two, and
/// is listed in `usage()`.
#[test]
fn every_long_option_parses_with_either_dash() {
    let value = |name: &str| match name {
        "network" => "regtest",
        "rpcclienttimeout" | "rpcwaittimeout" => "5",
        _ => "x",
    };
    let names: Vec<_> = long_option_names().collect();
    assert!(names.len() >= 13, "{names:?}");
    for name in names {
        assert!(
            usage().contains(&format!("--{name}")),
            "--{name} not in usage"
        );
        for dash in ["-", "--"] {
            let bare = format!("{dash}{name}");
            let alone = parse_args(&os_args(&["rbitcoin-cli", &bare]));
            let flag = match alone {
                Err(e) if e == format!("--{name} requires a value") => {
                    format!("{bare}={}", value(name))
                }
                _ => bare.clone(),
            };
            let parsed = parse_args(&os_args(&["rbitcoin-cli", &flag, "getblockcount"]));
            match parsed {
                Ok(Action::Call(cfg)) => {
                    assert_eq!(cfg.command.as_deref(), Some("getblockcount"), "{bare}");
                    assert!(cfg.params.is_empty(), "{bare}: {:?}", cfg.params);
                }
                Ok(_) => assert!(name == "help" || name == "version", "{bare}"),
                Err(e) => panic!("{bare}: {e}"),
            }
        }
    }
}

#[test]
fn parse_args_refuses_unknown_or_misspelled_options() {
    for (flag, want) in [
        ("-rpcport=8332", "unknown argument `-rpcport=8332`"),
        ("-rpcuser", "unknown argument `-rpcuser`"),
        ("-", "unknown argument `-`"),
        ("-hV", "unknown argument `-hV`"),
        ("--bogus=1", "unknown argument `--bogus`"),
        ("-rpcwait=1", "--rpcwait takes no value"),
        ("--named=0", "--named takes no value"),
        ("-help=1", "--help takes no value"),
        ("-datadir", "--datadir requires a value"),
        ("-network=nonet", "invalid --network"),
    ] {
        let err = match parse_args(&os_args(&["rbitcoin-cli", flag])) {
            Ok(_) => panic!("{flag} should have been rejected"),
            Err(e) => e,
        };
        assert!(err.contains(want), "{flag} -> {err}");
    }
}

/// A client timeout short enough that a test never sleeps a whole second.
const SHORT: Duration = Duration::from_millis(50);
/// A reply held longer than [`SHORT`] but far under any default.
const HOLD: Duration = Duration::from_millis(150);
/// Upper bound for a call that must return on its own timeout.
const SLOW: Duration = Duration::from_secs(5);

/// Answer one RPC after `hold`. With no `hold` the reply never comes: the
/// mock reads until the client hangs up (or 30 s pass).
fn serve_held_rpc(mut s: impl Read + Write, hold: Option<Duration>) {
    let mut tmp = [0u8; 1024];
    let Some(hold) = hold else {
        while matches!(s.read(&mut tmp), Ok(n) if n > 0) {}
        return;
    };
    let mut raw = Vec::new();
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut tmp) {
            Ok(n) if n > 0 => raw.extend_from_slice(&tmp[..n]),
            _ => return,
        }
    }
    thread::sleep(hold);
    let body = "{\"jsonrpc\":\"1.0\",\"id\":\"1\",\"result\":7,\"error\":null}\n";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(resp.as_bytes());
}

fn spawn_held_tcp_rpc(hold: Option<Duration>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        serve_held_rpc(s, hold);
    });
    (port, h)
}

/// Accept one connection and never read it, so a large request cannot
/// drain. The socket is held until `release` is dropped.
fn spawn_unread_tcp() -> (u16, std::sync::mpsc::Sender<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let h = thread::spawn(move || {
        let (_s, _) = listener.accept().unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    (port, release, h)
}

/// Twice the 4 MiB TCP send buffer cap (Linux `tcp_wmem`, macOS
/// `autosndbufmax`). A peer that never reads takes about 2 MiB on Linux.
const TCP_STALL: usize = 8 << 20;
/// A unix socket stalls after a few hundred KiB.
const UNIX_STALL: usize = 1 << 20;

fn tcp_call_cfg(dir: &std::path::Path, host_port: &str, flags: &[&str]) -> CliConfig {
    std::fs::write(dir.join("rpc.token"), "s3cret").unwrap();
    let mut args = vec![
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "--rpc-url",
        host_port,
    ];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["importmempool", "mempool.dat"]);
    call_cfg(&args)
}

fn local(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// `dispatch_call` with a deadline, so a call that never returns fails
/// the test instead of hanging the job.
fn dispatch_within(cfg: CliConfig, limit: Duration) -> (Result<String, String>, Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    let start = Instant::now();
    thread::spawn(move || {
        let _ = tx.send(dispatch_call(&cfg));
    });
    let out = rx
        .recv_timeout(limit)
        .unwrap_or_else(|_| panic!("the call did not return within {limit:?}"));
    (out, start.elapsed())
}

fn assert_timed_out(
    out: Result<String, String>,
    waited: Duration,
    op: &str,
    opt: &str,
    floor: Duration,
) {
    let err = out.unwrap_err();
    assert!(
        err.starts_with(op) && err.contains(&format!("{opt} expired")),
        "want `{op}` naming {opt}, got: {err}"
    );
    assert!(waited >= floor, "gave up early: {waited:?}");
}

#[test]
fn client_timeout_lifts_or_keeps_the_default() {
    use TimeoutArg::{After, Omitted, Unlimited};
    let five = Duration::from_secs(5);
    assert_eq!(
        client_timeout(Omitted, true),
        Some(Duration::from_secs(900))
    );
    assert_eq!(client_timeout(Omitted, false), None);
    assert_eq!(client_timeout(Unlimited, true), None);
    assert_eq!(client_timeout(Unlimited, false), None);
    assert_eq!(client_timeout(After(five), true), Some(five));
    assert_eq!(client_timeout(After(five), false), Some(five));
}

#[test]
fn connect_bound_takes_the_shorter_limit() {
    let s = Duration::from_secs;
    assert_eq!(connect_bound(None, None), None);
    assert_eq!(
        connect_bound(Some(s(9)), None),
        Some((s(9), "--rpcclienttimeout"))
    );
    assert_eq!(
        connect_bound(None, Some(s(2))),
        Some((s(2), "--rpcwaittimeout"))
    );
    assert_eq!(
        connect_bound(Some(s(9)), Some(s(2))),
        Some((s(2), "--rpcwaittimeout"))
    );
    assert_eq!(
        connect_bound(Some(s(2)), Some(s(9))),
        Some((s(2), "--rpcclienttimeout"))
    );
}

/// A lookup past its bound is kept: the next wait takes it over and starts
/// no second lookup.
#[test]
fn host_lookup_keeps_one_lookup_in_flight() {
    use std::sync::atomic::AtomicUsize;
    let calls = Arc::new(AtomicUsize::new(0));
    let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let mut lookups = HostLookup::default();
    let c = calls.clone();
    let start = Instant::now();
    let first = lookups.wait(Some(SHORT), move || {
        c.fetch_add(1, Ordering::SeqCst);
        let _ = held.recv();
        Ok(vec![addr])
    });
    assert_eq!(first.unwrap(), None);
    assert!(start.elapsed() < SLOW, "{:?}", start.elapsed());
    let c = calls.clone();
    let second = lookups.wait(Some(SHORT), move || {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    });
    assert_eq!(second.unwrap(), None);
    release.send(()).unwrap();
    let c = calls.clone();
    let third = lookups.wait(Some(SLOW), move || {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    });
    assert_eq!(third.unwrap(), Some(vec![addr]));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one lookup in flight");
    let fresh = lookups.wait(None, || Err(std::io::Error::other("no such host")));
    assert_eq!(fresh.unwrap_err().to_string(), "no such host");
}

/// `Instant` cannot hold every `u64` of seconds. Such a bound is refused,
/// not a panic; the socket timeouts clamp it.
#[test]
fn a_timeout_past_the_clock_is_refused_on_tcp() {
    let dir = tmp_datadir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = tcp_call_cfg(
        &dir,
        &local(port),
        &["--rpcclienttimeout=18446744073709551615"],
    );
    let (out, _) = dispatch_within(cfg, SLOW);
    let _ = std::fs::remove_dir_all(&dir);
    let err = out.unwrap_err();
    assert!(
        err.contains("--rpcclienttimeout=18446744073709551615 is too large"),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn a_timeout_past_the_clock_still_calls_over_the_unix_socket() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let (_, served) = serve_one_rpc(UnixListener::bind(dir.join("rpc.sock")).unwrap());
    let cfg = call_cfg(&[
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "--rpcclienttimeout=18446744073709551615",
        "getblockcount",
    ]);
    let (out, _) = dispatch_within(cfg, SLOW);
    let _ = served.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out, Ok("7".to_string()));
}

#[test]
fn rpcclienttimeout_fails_a_tcp_reply_that_outlasts_it() {
    let dir = tmp_datadir();
    let (port, h) = spawn_held_tcp_rpc(None);
    let mut cfg = tcp_call_cfg(&dir, &local(port), &[]);
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "read: ", "--rpcclienttimeout", SHORT);
}

/// The same held reply passes with `0` and fails with a shorter limit.
#[test]
fn rpcclienttimeout_zero_waits_for_a_held_tcp_reply() {
    for (flag, short) in [
        ("-rpcclienttimeout=0", false),
        ("--rpcclienttimeout=0", true),
    ] {
        let dir = tmp_datadir();
        let (port, h) = spawn_held_tcp_rpc(Some(HOLD));
        let mut cfg = tcp_call_cfg(&dir, &local(port), &[flag]);
        if short {
            cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
        } else {
            assert_eq!(cfg.rpcclienttimeout, TimeoutArg::Unlimited);
        }
        let (out, waited) = dispatch_within(cfg, SLOW);
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&dir);
        if short {
            assert_timed_out(out, waited, "read: ", "--rpcclienttimeout", SHORT);
        } else {
            assert_eq!(out, Ok("7".to_string()));
            assert!(waited >= HOLD, "{waited:?}");
        }
    }
}

#[test]
fn rpcclienttimeout_fails_a_tcp_request_that_cannot_drain() {
    let dir = tmp_datadir();
    let (port, release, h) = spawn_unread_tcp();
    let mut cfg = tcp_call_cfg(&dir, &local(port), &[]);
    cfg.params = vec!["x".repeat(TCP_STALL)];
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    drop(release);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "write: ", "--rpcclienttimeout", SHORT);
}

/// A name, not an IP: the lookup runs under the connect bound, and with
/// `0` without one. `localhost` may list `::1` first, which refuses.
#[test]
fn rpc_url_host_name_connects_with_and_without_a_bound() {
    for flags in [&[][..], &["--rpcclienttimeout=0"][..]] {
        let dir = tmp_datadir();
        let (port, h) = spawn_held_tcp_rpc(Some(Duration::ZERO));
        let cfg = tcp_call_cfg(&dir, &format!("localhost:{port}"), flags);
        let (out, _) = dispatch_within(cfg, SLOW);
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out, Ok("7".to_string()), "{flags:?}");
    }
}

/// A listener with backlog 0 and one queued connection: Linux drops the
/// next SYN, so a connect hangs like one to a filtered port.
#[cfg(target_os = "linux")]
fn full_tcp_listener() -> (socket2::Socket, std::net::TcpStream, u16) {
    use socket2::{Domain, Socket, Type};
    let l = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    l.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    l.listen(0).unwrap();
    let port = l.local_addr().unwrap().as_socket().unwrap().port();
    let queued = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    (l, queued, port)
}

#[cfg(target_os = "linux")]
#[test]
fn rpcclienttimeout_bounds_a_tcp_connect() {
    let dir = tmp_datadir();
    let (_l, _queued, port) = full_tcp_listener();
    let mut cfg = tcp_call_cfg(&dir, &local(port), &[]);
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "connect ", "--rpcclienttimeout", SHORT);
}

#[cfg(target_os = "linux")]
#[test]
fn rpcwaittimeout_bounds_a_tcp_connect_under_the_900s_default() {
    let dir = tmp_datadir();
    let (_l, _queued, port) = full_tcp_listener();
    let mut cfg = tcp_call_cfg(&dir, &local(port), &["--rpcwait"]);
    cfg.rpcwaittimeout = TimeoutArg::After(HOLD);
    let (out, waited) = dispatch_within(cfg, SLOW);
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "connect ", "--rpcwaittimeout", HOLD);
}

/// Backlog 0 and one queued connection: a unix connect waits for room.
#[cfg(target_os = "linux")]
#[test]
fn rpcclienttimeout_bounds_a_unix_connect() {
    use socket2::{Domain, SockAddr, Socket, Type};
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let l = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
    l.bind(&SockAddr::unix(&sock).unwrap()).unwrap();
    l.listen(0).unwrap();
    let _queued = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let mut cfg = call_cfg(&[
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "getblockcount",
    ]);
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    drop(l);
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "connect ", "--rpcclienttimeout", SHORT);
}

#[cfg(unix)]
fn unix_call_cfg(dir: &std::path::Path, flags: &[&str]) -> CliConfig {
    let mut args = vec!["rbitcoin-cli", "--datadir", dir.to_str().unwrap()];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["importmempool", "mempool.dat"]);
    call_cfg(&args)
}

#[cfg(unix)]
#[test]
fn rpcclienttimeout_also_bounds_a_unix_reply() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let listener = UnixListener::bind(dir.join("rpc.sock")).unwrap();
    let h = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        serve_held_rpc(s, None);
    });
    let mut cfg = unix_call_cfg(&dir, &[]);
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "read: ", "--rpcclienttimeout", SHORT);
}

#[cfg(unix)]
#[test]
fn rpcclienttimeout_also_bounds_a_unix_request_that_cannot_drain() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let listener = UnixListener::bind(dir.join("rpc.sock")).unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let h = thread::spawn(move || {
        let (_s, _) = listener.accept().unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    let mut cfg = unix_call_cfg(&dir, &[]);
    cfg.params = vec!["x".repeat(UNIX_STALL)];
    cfg.rpcclienttimeout = TimeoutArg::After(SHORT);
    let (out, waited) = dispatch_within(cfg, SLOW);
    drop(release);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_timed_out(out, waited, "write: ", "--rpcclienttimeout", SHORT);
}

#[cfg(unix)]
#[test]
fn rpcwaittimeout_caps_the_wait_for_a_socket() {
    let dir = tmp_datadir();
    let sock = dir.join("absent.sock");
    let mut cfg = call_cfg(&[
        "rbitcoin-cli",
        "--rpc-socket",
        sock.to_str().unwrap(),
        "--rpcwait",
        "getblockcount",
    ]);
    cfg.rpcwaittimeout = TimeoutArg::After(HOLD);
    let (out, waited) = dispatch_within(cfg, SLOW);
    let _ = std::fs::remove_dir_all(&dir);
    let err = out.unwrap_err();
    assert!(err.contains("absent.sock"), "{err}");
    assert!(waited >= HOLD, "gave up early: {waited:?}");
}

/// Unlimited retries are `retry_connect_retries_until_success`; this is
/// the parsed `0` reaching a socket through `--rpcwait`.
#[cfg(unix)]
#[test]
fn rpcwait_with_no_wait_limit_reaches_the_socket() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (_, served) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let cfg = call_cfg(&[
        "rbitcoin-cli",
        "--rpc-socket",
        sock.to_str().unwrap(),
        "--rpcwait",
        "-rpcwaittimeout=0",
        "getblockcount",
    ]);
    assert_eq!(cfg.rpcwaittimeout, TimeoutArg::Unlimited);
    let (out, _) = dispatch_within(cfg, SLOW);
    let _ = served.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out, Ok("7".to_string()));
}

#[test]
fn parse_args_rpcwait_is_a_flag() {
    match parse_args(&os_args(&["rbitcoin-cli", "--rpcwait", "getblockcount"])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.command.as_deref(), Some("getblockcount"));
            assert!(cfg.rpcwait, "--rpcwait must set the rpcwait flag");
        }
        Err(e) => panic!("--rpcwait must parse, got: {e}"),
        Ok(_) => panic!("expected Action::Call"),
    }
}

#[cfg(unix)]
#[test]
fn rpcwait_does_not_repeat_the_call() {
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (req, halt) = (requests.clone(), stop.clone());
    let h = thread::spawn(move || {
        while !halt.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, _)) => {
                    req.fetch_add(1, Ordering::SeqCst);
                    reply_rpc_error(s);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_rbitcoin-cli"))
        .arg("--rpc-socket")
        .arg(&sock)
        .arg("--rpcwait")
        .arg("getblockcount")
        .spawn()
        .unwrap();

    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    let mut exited = false;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            exited = true;
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        thread::sleep(std::time::Duration::from_millis(20));
    };
    stop.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();

    assert!(
        exited,
        "--rpcwait must return the RPC error, not retry the call"
    );
    assert!(!status.success(), "the RPC error must exit non-zero");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the call must be sent exactly once"
    );
}

#[cfg(unix)]
#[test]
fn named_sends_positionals_in_args_array() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "getblockhash",
        "100",
    ]);
    assert!(
        exit_ok(code),
        "--named getblockhash 100 must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!({"args": [100]}));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

/// A positional whose value contains `=` is not a named argument: a JSON array
/// such as `["tx","a=b"]` would otherwise split at that `=` into a bogus name.
#[cfg(unix)]
#[test]
fn named_keeps_a_value_containing_eq_positional() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "submitpackage",
        r#"["tx","a=b"]"#,
    ]);
    assert!(
        exit_ok(code),
        "--named submitpackage with an = inside a value must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!({"args": [["tx", "a=b"]]}));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn named_keeps_an_explicit_args_over_the_positional_list() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "echojson",
        "args=[1,2]",
        r#"["a=b"]"#,
    ]);
    assert!(exit_ok(code), "--named echojson must succeed, got {code:?}");
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!({"args": [1, 2]}));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn importmempool_positional_past_options_stays_a_string() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--datadir",
        dir.to_str().unwrap(),
        "importmempool",
        "file.dat",
        "{}",
        "leftover",
    ]);
    assert!(exit_ok(code), "importmempool must succeed, got {code:?}");
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(
        params,
        serde_json::json!(["file.dat", {}, "leftover"]),
        "a positional past `options` must ship as a string, not be JSON-parsed"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn importmempool_named_bools_convert() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "importmempool",
        "filepath=file.dat",
        "use_current_time=true",
    ]);
    assert!(
        exit_ok(code),
        "--named importmempool must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(
        params,
        serde_json::json!({"filepath": "file.dat", "use_current_time": true})
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn named_duplicate_name_last_wins() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "getblockhash",
        "height=1",
        "height=2",
    ]);
    assert!(
        exit_ok(code),
        "--named getblockhash height=1 height=2 must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!({"height": 2}));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn named_params_reach_server_as_object() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "getblockhash",
        "height=1",
    ]);
    assert!(
        exit_ok(code),
        "--named getblockhash height=1 must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!({"height": 1}));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[cfg(unix)]
#[test]
fn named_string_and_bool_params_keep_their_types() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let (captured, h) = serve_one_rpc(UnixListener::bind(&sock).unwrap());
    let code = cli_main([
        "rbitcoin-cli",
        "--named",
        "--datadir",
        dir.to_str().unwrap(),
        "getrawtransaction",
        "txid=deadbeef",
        "verbose=true",
    ]);
    assert!(
        exit_ok(code),
        "--named getrawtransaction must succeed, got {code:?}"
    );
    let (_, params) = captured.lock().unwrap().take().unwrap();
    assert_eq!(
        params,
        serde_json::json!({"txid": "deadbeef", "verbose": true})
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

/// Serve two JSON-RPC requests, replying per-method, and record the methods.
#[cfg(unix)]
fn serve_getinfo_mock(
    listener: std::os::unix::net::UnixListener,
) -> (Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let seen = methods.clone();
    let h = thread::spawn(move || {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut tmp = [0u8; 1024];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&tmp[..n]);
            }
            let header_end = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(raw.len());
            let content_length = String::from_utf8_lossy(&raw[..header_end])
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while raw.len() < header_end + content_length {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&tmp[..n]);
            }
            let body = &raw[header_end..header_end + content_length];
            let method = serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("method")
                        .and_then(serde_json::Value::as_str)
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();
            seen.lock().unwrap().push(method.clone());
            let result = match method.as_str() {
                "getblockchaininfo" => {
                    r#"{"chain":"regtest","blocks":100,"headers":100,"verificationprogress":0.5,"difficulty":1}"#
                }
                "getnetworkinfo" => {
                    r#"{"version":259900,"timeoffset":0,"connections":2,"connections_in":1,"connections_out":1,"relayfee":0.00001,"networks":[{"name":"ipv4","proxy":"127.0.0.1:9050"},{"name":"ipv6","proxy":""},{"name":"onion","proxy":"127.0.0.1:9050"}],"warnings":["fee estimation disabled"]}"#
                }
                _ => "null",
            };
            let resp_body = format!(
                "{{\"jsonrpc\":\"1.0\",\"id\":\"1\",\"result\":{result},\"error\":null}}\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        }
    });
    (methods, h)
}

#[cfg(unix)]
#[test]
fn getinfo_prints_a_core_style_summary() {
    use std::os::unix::net::UnixListener;
    let dir = tmp_datadir();
    let sock = dir.join("rpc.sock");
    let cfg = match parse_args(&os_args(&[
        "rbitcoin-cli",
        "--getinfo",
        "--datadir",
        dir.to_str().unwrap(),
    ])) {
        Ok(Action::Call(cfg)) => cfg,
        Ok(_) => panic!("expected Action::Call"),
        Err(e) => panic!("--getinfo must parse, got: {e}"),
    };
    let (methods, h) = serve_getinfo_mock(UnixListener::bind(&sock).unwrap());
    let out = dispatch_call(&cfg).unwrap();
    assert_eq!(
        out,
        "Chain: regtest\nBlocks: 100\nHeaders: 100\nVerification progress: ▒▒▒▒▒▒▒▒▒▒░░░░░░░░░░ 50.0000%\nDifficulty: 1\n\nNetwork: in 1, out 1, total 2\nVersion: 259900\nTime offset (s): 0\nProxies: 127.0.0.1:9050 (ipv4, onion)\nMin tx relay fee rate (BTC/kvB): 0.00001000\n\nWarnings: fee estimation disabled"
    );
    let mut got = methods.lock().unwrap().clone();
    got.sort();
    assert_eq!(
        got,
        vec![
            "getblockchaininfo".to_string(),
            "getnetworkinfo".to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = h.join();
}

#[test]
fn getinfo_rejects_a_command() {
    let cfg = match parse_args(&os_args(&["rbitcoin-cli", "--getinfo", "getblockcount"])) {
        Ok(Action::Call(cfg)) => cfg,
        Ok(_) => panic!("expected Action::Call"),
        Err(e) => panic!("--getinfo must parse, got: {e}"),
    };
    assert_eq!(
        dispatch_call(&cfg).unwrap_err(),
        "--getinfo takes no arguments"
    );
}

/// `--stdin` takes its first line as the command when argv has none and
/// appends the rest to `params`, so the guard has to look at both.
#[test]
fn getinfo_rejects_stray_parameters() {
    let mut cfg = match parse_args(&os_args(&["rbitcoin-cli", "--getinfo", "--stdin"])) {
        Ok(Action::Call(cfg)) => cfg,
        Ok(_) => panic!("expected Action::Call"),
        Err(e) => panic!("--getinfo must parse, got: {e}"),
    };
    apply_stdin_args(
        &mut cfg,
        vec!["getblockcount".to_string(), "extra".to_string()],
    );
    assert_eq!(cfg.command.as_deref(), Some("getblockcount"));
    assert_eq!(cfg.params, vec!["extra".to_string()]);
    assert_eq!(
        dispatch_call(&cfg).unwrap_err(),
        "--getinfo takes no arguments"
    );
}
