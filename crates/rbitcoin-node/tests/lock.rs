use rbitcoin_node::config::NodeConfig;
use rbitcoin_node::error::NodeError;
use rbitcoin_node::lock::*;
use std::path::PathBuf;

fn tmp() -> PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("dirlock");
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn lock_file_does_not_follow_a_symlink() {
    let dir = tmp();
    let outside = tmp();
    let link = dir.join(".lock");
    std::os::unix::fs::symlink(outside.join("elsewhere.lock"), &link).unwrap();
    let err = lock_dir(&dir).expect_err("symlink lock");
    match err {
        NodeError::Datadir { .. } => {}
        other => panic!("expected datadir open failure, got {other}"),
    }
    assert!(
        !outside.join("elsewhere.lock").exists(),
        "open followed the symlink"
    );
}

#[test]
fn second_lock_on_same_datadir_is_busy() {
    let dir = tmp();
    let cfg = NodeConfig::default().with_datadir(&dir).with_tiny_heads();
    let _held = lock_node_dirs(&cfg).expect("first lock");
    let err = lock_node_dirs(&cfg).expect_err("second lock");
    match err {
        NodeError::Locked(ref p) => assert_eq!(*p, dir, "locked {p:?}"),
        other => panic!("expected Locked, got {other}"),
    }
    assert!(
        format!("{err}").contains("Cannot obtain a lock on directory"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A duplicated fd shares the flock. Dropping the guard must release it,
/// or the next launch in this process reports the datadir as busy.
#[cfg(unix)]
#[test]
fn drop_releases_the_lock_even_if_the_fd_was_duplicated() {
    let dir = tmp();
    let cfg = NodeConfig::default().with_datadir(&dir).with_tiny_heads();
    let held = lock_node_dirs(&cfg).expect("lock");
    let dup = held._files[0].try_clone().expect("dup");
    drop(held);
    lock_node_dirs(&cfg).expect("drop released the flock while the dup is open");
    drop(dup);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lock_does_not_create_core_blocks_dir() {
    let dir = tmp();
    let cfg = NodeConfig::default().with_datadir(&dir).with_tiny_heads();
    let _held = lock_node_dirs(&cfg).expect("lock");
    assert!(
        !dir.join("blocks").exists(),
        "rbitcoin has no Core blocks/ product"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
