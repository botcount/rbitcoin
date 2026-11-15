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

type CapturedParams = Arc<Mutex<Option<serde_json::Value>>>;

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
/// capture the request's `params` into the returned slot.
#[cfg(unix)]
fn serve_one_rpc(
    listener: std::os::unix::net::UnixListener,
) -> (CapturedParams, thread::JoinHandle<()>) {
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
            *c.lock().unwrap() = v.get("params").cloned();
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

#[test]
fn parse_args_negative_numbers_are_params() {
    match parse_args(&os_args(&[
        "rbitcoin-cli",
        "getnetworkhashps",
        "-1",
        "-0.5",
    ])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.command.as_deref(), Some("getnetworkhashps"));
            assert_eq!(cfg.params, vec!["-1".to_string(), "-0.5".to_string()]);
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
fn parse_args_stdin_is_a_flag() {
    match parse_args(&os_args(&["rbitcoin-cli", "--stdin", "getblockcount"])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.command.as_deref(), Some("getblockcount"));
            assert!(cfg.stdin, "--stdin must set the stdin flag");
        }
        Err(e) => panic!("--stdin must parse, got: {e}"),
        Ok(_) => panic!("expected Action::Call"),
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

    let params = captured.lock().unwrap().take().unwrap();
    assert_eq!(params, serde_json::json!(["123456789", 2]));
}

#[test]
fn retry_connect_retries_until_success() {
    let mut calls = 0;
    let out = retry_connect(
        || {
            calls += 1;
            if calls < 3 {
                Err(ConnectError::transient("connection refused"))
            } else {
                Ok(42)
            }
        },
        3,
        std::time::Duration::ZERO,
    );
    assert_eq!(out, Ok(42));
    assert_eq!(calls, 3);
}

#[test]
fn retry_connect_gives_up_after_attempts() {
    let mut calls = 0;
    let out = retry_connect(
        || {
            calls += 1;
            Err::<i32, ConnectError>(ConnectError::transient("connection refused"))
        },
        2,
        std::time::Duration::ZERO,
    );
    assert_eq!(out, Err("connection refused".to_string()));
    assert_eq!(calls, 2);
}

/// A bad address or socket path must not be retried: one attempt, then out.
#[test]
fn retry_connect_does_not_retry_a_permanent_error() {
    let mut calls = 0;
    let out = retry_connect(
        || {
            calls += 1;
            Err::<i32, ConnectError>(ConnectError::permanent("bad rpc url"))
        },
        30,
        std::time::Duration::ZERO,
    );
    assert_eq!(out, Err("bad rpc url".to_string()));
    assert_eq!(calls, 1, "a permanent error must not be retried");
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
    use std::time::Instant;

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

#[test]
fn parse_args_named_is_a_flag() {
    match parse_args(&os_args(&[
        "rbitcoin-cli",
        "--named",
        "getblockhash",
        "height=1",
    ])) {
        Ok(Action::Call(cfg)) => {
            assert_eq!(cfg.command.as_deref(), Some("getblockhash"));
            assert_eq!(cfg.params, vec!["height=1".to_string()]);
            assert!(cfg.named, "--named must set the named flag");
        }
        Err(e) => panic!("--named must parse, got: {e}"),
        Ok(_) => panic!("expected Action::Call"),
    }
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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
    let params = captured.lock().unwrap().take().unwrap();
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

#[test]
fn parse_args_getinfo_is_a_flag() {
    match parse_args(&os_args(&["rbitcoin-cli", "--getinfo"])) {
        Ok(Action::Call(cfg)) => {
            assert!(cfg.getinfo, "--getinfo must set the getinfo flag");
            assert!(cfg.command.is_none(), "--getinfo takes no command");
        }
        Err(e) => panic!("--getinfo must parse, got: {e}"),
        Ok(_) => panic!("expected Action::Call"),
    }
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

/// `--stdin` appends to `params` without setting a command, so the guard has to
/// look at both.
#[test]
fn getinfo_rejects_stray_parameters() {
    let mut cfg = match parse_args(&os_args(&["rbitcoin-cli", "--getinfo"])) {
        Ok(Action::Call(cfg)) => cfg,
        Ok(_) => panic!("expected Action::Call"),
        Err(e) => panic!("--getinfo must parse, got: {e}"),
    };
    cfg.params.push("getblockcount".to_string());
    assert_eq!(
        dispatch_call(&cfg).unwrap_err(),
        "--getinfo takes no arguments"
    );
}
