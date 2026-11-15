use rbitcoin_rpc::auth::*;
use std::fs;
use std::path::{Path, PathBuf};

fn tmp() -> PathBuf {
    rbitcoin_store::testutil::fresh_path("rpc-auth")
}

#[test]
fn debug_does_not_print_the_token() {
    let auth = RpcAuth::new("secret-token");
    let shown = format!("{auth:?}");
    assert!(!shown.contains("secret-token"), "{shown}");
    assert!(shown.contains("redacted"), "{shown}");
}

#[test]
fn token_roundtrip() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    let a = write_token_file(&path).unwrap();
    let b = read_token_file(&path).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.token.len(), 64);
    assert!(a
        .token
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn resolve_reuses_existing_token() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    fs::write(&path, "fixed-token\n").unwrap();
    // fs::write follows umask (0644 here). The node only reuses an owner-only token.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (a, p) = resolve_rpc_auth(&dir, None).unwrap();
    assert_eq!(a.token, "fixed-token");
    assert_eq!(p, path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn token_is_csprng_hex() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let mut seen = std::collections::HashSet::new();
    for i in 0..32 {
        let path = dir.join(format!("rpc.token-{i}"));
        let a = write_token_file(&path).unwrap();
        assert_eq!(a.token.len(), 64);
        assert!(seen.insert(a.token.clone()));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn auth_scheme_matches_in_any_case() {
    assert_eq!(parse_bearer_auth("BEARER abc"), Some("abc"));
    assert_eq!(parse_bearer_auth("bEaReR abc"), Some("abc"));
    assert_eq!(
        parse_basic_auth("BASIC dXNlcjpwYXNz").as_deref(),
        Some("user:pass")
    );
    assert_eq!(
        parse_basic_auth("BaSiC dXNlcjpwYXNz").as_deref(),
        Some("user:pass")
    );
    assert!(parse_bearer_auth("Bearerabc").is_none());
    assert!(parse_basic_auth("Basicdxnlcjpwyxnz").is_none());
    assert!(parse_basic_auth("Bearer dXNlcjpwYXNz").is_none());
}

#[test]
fn bearer_parse_and_token_paths() {
    assert_eq!(parse_bearer_auth("Bearer abc"), Some("abc"));
    assert_eq!(parse_bearer_auth("bearer xyz"), Some("xyz"));
    assert!(parse_bearer_auth("Bearer ").is_none());
    assert!(parse_bearer_auth("Basic abc").is_none());
    assert_eq!(
        parse_basic_auth("Basic dXNlcjpwYXNz").as_deref(),
        Some("user:pass")
    );
    assert!(parse_basic_auth("Basic !!!").is_none());
    let a = RpcAuth::new("s3cret");
    assert!(a.matches_token("s3cret"));
    assert!(!a.matches_token("nope"));
    assert!(a.matches_token("s3cret"));
    let mut one_bit = a.token.clone().into_bytes();
    one_bit[0] ^= 0x01;
    let one_bit = String::from_utf8(one_bit).unwrap();
    assert!(!a.matches_token(&one_bit));
    assert!(ct_eq(b"s3cret", b"s3cret"));
    assert!(!ct_eq(b"s3cret", one_bit.as_bytes()));
    assert!(
        !ct_eq(b"s3cret", b"s3cre"),
        "shorter equal prefix is not the token"
    );
    assert!(
        !ct_eq(b"s3cret", b"s3cret!"),
        "longer equal prefix is not the token"
    );
    assert!(ct_eq(b"", b""));
    assert!(!ct_eq(b"", b"x"));
    assert_eq!(
        default_socket_path(Path::new("/d")),
        PathBuf::from("/d/rpc.sock")
    );
    assert_eq!(
        default_token_path(Path::new("/d")),
        PathBuf::from("/d/rpc.token")
    );
}

#[test]
fn empty_token_file_errors() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    fs::write(&path, "  \n").unwrap();
    // Owner-only so the failure is the empty token, not the mode check.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let err = read_token_file(&path).unwrap_err();
    assert!(err.contains("empty"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn resolve_mints_a_token_over_a_zero_length_file() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    fs::write(&path, "").unwrap();
    let (a, p) = resolve_rpc_auth(&dir, None).unwrap();
    assert_eq!(p, path);
    assert_eq!(a.token.len(), 64);
    assert_eq!(read_token_file(&path).unwrap(), a);
    let (again, _) = resolve_rpc_auth(&dir, None).unwrap();
    assert_eq!(again, a);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn resolve_still_refuses_a_blank_token() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    fs::write(&path, "  \n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let err = resolve_rpc_auth(&dir, None).unwrap_err();
    assert!(err.contains("empty"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "  \n");
    let _ = fs::remove_dir_all(&dir);
}

/// The token is staged whole and renamed into place, so a crash leaves at
/// most a staged file, never a prefix in `rpc.token`. The next start drops it.
#[test]
fn resolve_mints_a_token_past_a_staged_file_left_by_a_crash() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let staged = dir.join("rpc.token.tmp");
    fs::write(&staged, "partial").unwrap();
    let (a, p) = resolve_rpc_auth(&dir, None).unwrap();
    assert_eq!(read_token_file(&p).unwrap(), a);
    assert!(!staged.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn write_token_file_refuses_an_existing_token() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    let a = write_token_file(&path).unwrap();
    assert!(write_token_file(&path).is_err());
    assert_eq!(read_token_file(&path).unwrap(), a);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn token_file_refuses_group_or_world_bits() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    fs::write(&path, "pass").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let err = read_token_file(&path).unwrap_err();
    assert!(err.contains("0600"), "{err}");
    assert!(err.contains("Delete the file"), "{err}");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(read_token_file(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_token_file(&path).unwrap().token, "pass");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cookie_file_requires_one_nonempty_credential_line() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".cookie");
    fs::write(&path, "mempool:secret").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let cookie = read_cookie_file(&path, RpcCookiePerms::Owner).unwrap();
    assert!(cookie.matches_credentials("mempool:secret"));
    assert!(!cookie.matches_credentials("mempool:wrong"));
    for contents in [
        "",
        "\n",
        "mempool",
        ":secret",
        "mempool:",
        "mempool:secret\nnext",
    ] {
        fs::write(&path, contents).unwrap();
        assert!(
            read_cookie_file(&path, RpcCookiePerms::Owner).is_err(),
            "{contents:?}"
        );
    }
    // mempool sends the file verbatim, so a line ending would never match:
    // refuse it with an error that names the fix.
    for contents in ["mempool:secret\n", "mempool:secret\r\n", "mempool:secret\r"] {
        fs::write(&path, contents).unwrap();
        let err = read_cookie_file(&path, RpcCookiePerms::Owner)
            .err()
            .expect("line ending rejected");
        assert!(err.contains("line ending"), "{contents:?}: {err}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn cookie_mode_must_match_declared_perms() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".cookie");
    fs::write(&path, "mempool:secret").unwrap();
    let chmod = |mode: u32| {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    };
    chmod(0o644);
    let err = read_cookie_file(&path, RpcCookiePerms::Owner)
        .err()
        .expect("0644 is wider than owner");
    assert!(err.contains("644"), "{err}");
    assert!(err.contains("--rpc-cookie-perms owner"), "{err}");
    chmod(0o600);
    assert!(read_cookie_file(&path, RpcCookiePerms::Owner).is_ok());
    chmod(0o640);
    assert!(read_cookie_file(&path, RpcCookiePerms::Group).is_ok());
    let err = read_cookie_file(&path, RpcCookiePerms::Owner)
        .err()
        .expect("0640 is wider than owner");
    assert!(err.contains("--rpc-cookie-perms owner"), "{err}");
    chmod(0o644);
    let err = read_cookie_file(&path, RpcCookiePerms::Group)
        .err()
        .expect("0644 is wider than group");
    assert!(err.contains("--rpc-cookie-perms group"), "{err}");
    chmod(0o660);
    let err = read_cookie_file(&path, RpcCookiePerms::Group)
        .err()
        .expect("0660 is group-writable");
    assert!(err.contains("--rpc-cookie-perms group"), "{err}");
    chmod(0o666);
    let err = read_cookie_file(&path, RpcCookiePerms::All)
        .err()
        .expect("0666 is wider than world-read");
    assert!(err.contains("--rpc-cookie-perms all"), "{err}");
    chmod(0o644);
    assert!(read_cookie_file(&path, RpcCookiePerms::All).is_ok());
    assert_eq!(
        RpcCookiePerms::parse("owner").unwrap(),
        RpcCookiePerms::Owner
    );
    assert_eq!(
        RpcCookiePerms::parse("group").unwrap(),
        RpcCookiePerms::Group
    );
    assert_eq!(RpcCookiePerms::parse("all").unwrap(), RpcCookiePerms::All);
    assert!(RpcCookiePerms::parse("0640").is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn token_file_is_created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.token");
    let _ = write_token_file(&path).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "token file mode {mode:o}");
    let _ = fs::remove_dir_all(&dir);
}
