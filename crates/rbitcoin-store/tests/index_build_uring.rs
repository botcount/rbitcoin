use rbitcoin_primitives::{Fk, Height};
use rbitcoin_store::error::StoreError;
use rbitcoin_store::index_build_uring::{
    read_index_window, require_body_hwm, run_on_session, with_index_error_after_push, IndexHeight,
    IndexWindow, ReadJob,
};
use rbitcoin_store::tx_table::{OutputRecord, TxTable};

use rbitcoin_store::tx_table::{InputRecord, TxRecord};

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-index-hwm-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An inner error must drain leftover SQEs while `jobs` still own the buffers
/// they point at. Without the drain in `run_on_session`, the session keeps the
/// `user_data` and the caller frees `jobs[0].buf` underneath the worker.
#[test]
fn run_on_session_drains_leftover_sqes_on_error() {
    use rbitcoin_store::io_handle::IoHandle;
    use rbitcoin_store::uring_session::{SessionKind, UringSession};
    let dir = tmp("drain-on-error");
    let path = dir.join("body");
    std::fs::write(&path, [0x5Au8; 4]).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let handle = IoHandle::from_file(&file);
    let mut session = UringSession::try_open_kind(SessionKind::Pool, 32).expect("pool");
    let mut jobs = vec![ReadJob {
        fd: handle,
        path: path.as_path(),
        off: 0,
        buf: vec![0u8; 4],
    }];
    // The seam fails the machine after it has pushed a real SQE.
    let err = with_index_error_after_push(|| run_on_session(&mut session, &mut jobs)).unwrap_err();
    assert!(
        matches!(&err, StoreError::Corrupt(msg) if msg.contains("index build error")),
        "the wrapper must keep the index-build error, got {err:?}"
    );
    assert_eq!(
        session.in_flight(),
        0,
        "the error path must drain before `jobs` drops their buffers"
    );
    drop(jobs);
    drop(file);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn index_read_above_the_cap_is_refused() {
    let dir = tmp("read-cap");
    let file = std::fs::File::create(dir.join("body")).unwrap();
    let handle = rbitcoin_store::io_handle::IoHandle::from_file(&file);
    let len = rbitcoin_store::var_table::VarTable::BODY_READ_MAX + 1;
    match ReadJob::new((handle, dir.as_path()), 0, len) {
        Err(StoreError::Rejected(msg)) => assert!(msg.contains("too large"), "{msg}"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("above the body read cap allocated"),
    }
}

fn tx(mark: u8) -> TxRecord {
    let mut txid = [0u8; 32];
    txid[0] = mark;
    TxRecord {
        txid,
        version: 1,
        locktime: 0,
        input_count: 1,
        output_count: 1,
    }
}

fn spend(parent: Fk) -> InputRecord {
    InputRecord {
        prev_txid: [0u8; 32],
        parent: rbitcoin_store::InputParent::Prevout {
            create_fk: parent,
            vout: 0,
        },
        sequence: u32::MAX,
        script_sig: vec![1],
        witness: vec![],
    }
}

/// Child fk 1 spends parent fk 2, and the parent body is written second.
fn child_then_parent(script: Vec<u8>) -> (std::path::PathBuf, TxTable) {
    let dir = tmp("window");
    let t = TxTable::create_tiny(&dir).unwrap();
    let fks = t
        .put_full_batch_indexed(
            &[
                (
                    tx(1),
                    vec![spend(Fk(2))],
                    vec![OutputRecord::unspent(1, script)],
                ),
                (
                    tx(2),
                    vec![InputRecord::coinbase(u32::MAX, vec![2], vec![])],
                    vec![OutputRecord::unspent(50, vec![0x51])],
                ),
            ],
            true,
        )
        .unwrap();
    assert_eq!(fks, vec![Fk(1), Fk(2)]);
    (dir, t)
}

fn one_child(tweaks: bool) -> [IndexHeight; 1] {
    [IndexHeight {
        height: Height(1),
        header_fk: Fk(1),
        first: Fk(1),
        n: 1,
        tweaks,
    }]
}

fn expect_published(err: Result<IndexWindow, StoreError>) {
    match err {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("published"), "{msg}"),
        Ok(_) => panic!("a span past the published end was read"),
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn txout_span_past_the_body_hwm_is_corrupt() {
    require_body_hwm(80, 80).unwrap();
    match require_body_hwm(80, 81) {
        Err(StoreError::Corrupt(msg)) => assert!(msg.contains("published"), "{msg}"),
        Ok(()) => panic!("a span past the body HWM was accepted"),
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn parent_txout_past_the_published_end_is_corrupt() {
    let (dir, t) = child_then_parent(vec![0x51]);
    let locs = t.create_loc.range_batch(&[Fk(1), Fk(2)]).unwrap();
    let child = locs[0].unwrap();
    let parent = locs[1].unwrap();
    let child_end = child.txout.0 + child.txout.1;
    assert!(
        parent.txout.0 + parent.txout.1 > child_end,
        "the parent txout must sit past the child"
    );
    t.body.truncate_body_to(2, child_end).unwrap();
    expect_published(read_index_window(&t, &one_child(false)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seqsigwit_past_the_published_end_is_corrupt() {
    let mut script = vec![0x51, 0x20];
    script.extend_from_slice(&[0xab; 32]);
    let (dir, t) = child_then_parent(script);
    let range = t.seqsigwit_loc.range_batch(&[Fk(1)]).unwrap()[0].unwrap();
    assert!(range.1 > 0, "the child seqsigwit span is non-empty");
    t.seqsigwit.truncate_body_to(2, range.0).unwrap();
    expect_published(read_index_window(&t, &one_child(true)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tweak_window_reads_the_parent_txid() {
    let mut script = vec![0x51, 0x20];
    script.extend_from_slice(&[0xab; 32]);
    let dir = tmp("parent-txid");
    let t = TxTable::create_tiny(&dir).unwrap();
    t.put_full_batch_indexed(
        &[
            (
                tx(1),
                vec![spend(Fk(2))],
                vec![OutputRecord::unspent(1, script)],
            ),
            (
                tx(2),
                vec![InputRecord::coinbase(u32::MAX, vec![2], vec![])],
                vec![OutputRecord::unspent(50, vec![0x51])],
            ),
        ],
        true,
    )
    .unwrap();
    let window = read_index_window(&t, &one_child(true)).unwrap();
    let mut expect = [0u8; 32];
    expect[0] = 2;
    assert_eq!(window.txid(Fk(2)), Some(expect));
    let txid_path = t.txids.file.path().to_path_buf();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&txid_path)
        .unwrap()
        .set_len(
            rbitcoin_store::txid_body::TXID_BODY_HEADER
                + rbitcoin_store::txid_body::TXID_ENTRY_LEN
                + 8,
        )
        .unwrap();
    match read_index_window(&t, &one_child(true)) {
        Err(err) => {
            let msg = err.to_string();
            assert!(msg.contains("short"), "{msg}");
        }
        Ok(_) => panic!("a short txid body was read"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
