use rbitcoin_store::bulk_io::*;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::io_handle::IoHandle;

#[test]
fn unset_io_env_opens_a_session_except_on_darwin() {
    #[cfg(target_os = "macos")]
    assert!(!session_wanted(None), "Darwin defaults to libc pread");
    #[cfg(not(target_os = "macos"))]
    assert!(session_wanted(None));
    assert!(
        session_wanted(Some(IoToken::Pool)),
        "RBITCOIN_IO=pool still opens the pool session"
    );
    assert!(!session_wanted(Some(IoToken::Pread)));
}
use std::io::Write;

#[test]
fn unknown_io_token_takes_the_unset_default() {
    let unknown = parse_io_token_str("nonsense");
    assert_eq!(unknown, None);
    assert_eq!(session_kind_for(unknown), default_session_kind());
    assert_eq!(session_wanted(unknown), session_wanted(None));
}

/// `backend_name` is the engine the session probe built, or `pread` when no
/// session opened. With `RBITCOIN_IO` unset it is the OS default.
#[test]
fn backend_name_is_the_session_built_or_pread() {
    use rbitcoin_store::uring_session::UringSession;
    let live = backend_name();
    if io_uring_enabled() {
        let built = UringSession::try_open(32).expect("the probe opened one");
        assert_eq!(live, built.kind().name());
    } else {
        assert_eq!(live, "pread", "no session means libc pread");
    }
    if std::env::var_os("RBITCOIN_IO").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    assert_eq!(live, "pread", "Darwin defaults to libc pread");
    #[cfg(target_os = "linux")]
    assert!(matches!(live, "io_uring" | "pread"), "{live}");
    #[cfg(windows)]
    assert!(matches!(live, "iocp" | "pool" | "pread"), "{live}");
}

fn dummy_handle() -> IoHandle {
    #[cfg(unix)]
    {
        IoHandle::from_raw_fd(0)
    }
    #[cfg(windows)]
    {
        IoHandle::from_raw_handle(0)
    }
}

#[test]
fn pwrite_batch_fail_unfilled_is_not_success() {
    let buf = [1u8; 4];
    let mut ops = [WriteOp {
        fd: dummy_handle(),
        offset: 0,
        buf: &buf,
        result: i32::MIN,
    }];
    assert!(!finish_pwrite_wave(&mut ops));
    assert_eq!(ops[0].result, -5);
    ops[0].result = 4;
    assert!(finish_pwrite_wave(&mut ops));
}

/// Dense surface: empty ops, empty bufs, multi-worker fallback,
/// workers/io_uring helpers (without racing env with parallel tests for mode).
#[test]
fn bulk_io_edges_empty_serial_rmw_workers() {
    let _ = io_uring_enabled(); // may probe once
    let w = bulk_io_workers();
    assert!(w >= 1);

    pread_batch(&mut []);
    pwrite_batch(&mut []);

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-bulk-edge-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let mut f = std::fs::File::create(&path).unwrap();
    let data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    f.write_all(&data).unwrap();
    f.flush().unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);

    let mut empty = [];
    let mut ops = [ReadOp {
        fd,
        offset: 0,
        buf: &mut empty[..],
        result: i32::MIN,
    }];
    pread_batch(&mut ops);
    assert_eq!(ops[0].result, 0);

    let mut wops = [WriteOp {
        fd,
        offset: 0,
        buf: &[],
        result: i32::MIN,
    }];
    pwrite_batch(&mut wops);
    assert_eq!(wops[0].result, 0);

    // Force fallback multi-worker path: many preads (≥8)
    let mut bufs: Vec<[u8; 64]> = vec![[0u8; 64]; 16];
    let mut read_ops: Vec<ReadOp<'_>> = Vec::new();
    for (i, b) in bufs.iter_mut().enumerate() {
        read_ops.push(ReadOp {
            fd,
            offset: (i * 64) as u64,
            buf: b.as_mut_slice(),
            result: i32::MIN,
        });
    }
    pread_batch_fallback(&mut read_ops);
    for op in &read_ops {
        assert_eq!(op.result, 64, "result={}", op.result);
    }
    assert_eq!(&bufs[0][..], &data[0..64]);

    let mut past = [0u8; 16];
    let mut ro = ReadOp {
        fd,
        offset: 10_000,
        buf: &mut past,
        result: i32::MIN,
    };
    pread_one(&mut ro);
    assert_eq!(ro.result, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pread_batch_roundtrip_tmpfile() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-uring-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let mut f = std::fs::File::create(&path).unwrap();
    let data: Vec<u8> = (0u8..200).collect();
    f.write_all(&data).unwrap();
    f.flush().unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);

    let mut b0 = [0u8; 50];
    let mut b1 = [0u8; 50];
    let mut b2 = [0u8; 50];
    {
        let mut ops = [
            ReadOp {
                fd,
                offset: 0,
                buf: &mut b0[..],
                result: i32::MIN,
            },
            ReadOp {
                fd,
                offset: 50,
                buf: &mut b1[..],
                result: i32::MIN,
            },
            ReadOp {
                fd,
                offset: 100,
                buf: &mut b2[..],
                result: i32::MIN,
            },
        ];
        pread_batch(&mut ops);
        for op in &ops {
            assert!(op.result >= 50, "result={}", op.result);
        }
    }
    assert_eq!(&b0[..], &data[0..50]);
    assert_eq!(&b1[..], &data[50..100]);
    assert_eq!(&b2[..], &data[100..150]);

    // Many small reads (stress completion mapping).
    let mut bufs: Vec<[u8; 1]> = (0..120).map(|_| [0u8; 1]).collect();
    {
        let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        let mut ops: Vec<ReadOp<'_>> = Vec::new();
        for (i, sl) in slices.iter_mut().enumerate() {
            ops.push(ReadOp {
                fd,
                offset: i as u64,
                buf: sl,
                result: i32::MIN,
            });
        }
        pread_batch(&mut ops);
        for (i, op) in ops.iter().enumerate() {
            assert_eq!(op.result, 1, "i={i}");
        }
    }
    for (i, b) in bufs.iter().enumerate() {
        assert_eq!(b[0], data[i], "i={i}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pread_batch_on_pool_session() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-pread-pool-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"pool-session-bytes!!").unwrap();
    f.flush().unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);
    // Held-session path (head-resolve ID / idx) on an explicit pool open.
    let mut sess = rbitcoin_store::uring_session::UringSession::try_open_kind(
        rbitcoin_store::uring_session::SessionKind::Pool,
        32,
    )
    .expect("held pool");
    let mut b2 = [0u8; 4];
    let mut ops2 = [ReadOp {
        fd,
        offset: 5,
        buf: &mut b2[..],
        result: i32::MIN,
    }];
    assert!(
        pread_batch_on_ctx(&mut rbitcoin_store::IoCtx::held(&mut sess), &mut ops2)
            .expect("held pool pread"),
        "pread_batch_on_ctx(held) must succeed on pool (not a linux-only stub)"
    );
    assert_eq!(ops2[0].result, 4);
    assert_eq!(&b2, b"sess");
    sess.drain_all().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn held_pread_errno_on_live_ring_is_ok_true() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-held-errno-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"abcd").unwrap();
        f.flush().unwrap();
    }
    let ro = std::fs::File::open(&path).unwrap();
    let wo = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let fd_ok = rbitcoin_store::io_handle::IoHandle::from_file(&ro);
    let fd_bad = rbitcoin_store::io_handle::IoHandle::from_file(&wo);
    let mut sess = rbitcoin_store::uring_session::UringSession::try_open(32).unwrap_or_else(|_| {
        rbitcoin_store::uring_session::UringSession::try_open_kind(
            rbitcoin_store::uring_session::SessionKind::Pool,
            32,
        )
        .expect("pool")
    });
    let mut good = [0u8; 4];
    let mut bad = [0u8; 4];
    let (used, r0, r1) = {
        let mut ops = [
            ReadOp {
                fd: fd_ok,
                offset: 0,
                buf: &mut good[..],
                result: i32::MIN,
            },
            ReadOp {
                fd: fd_bad,
                offset: 0,
                buf: &mut bad[..],
                result: i32::MIN,
            },
        ];
        let used = pread_batch_on_ctx(&mut rbitcoin_store::IoCtx::held(&mut sess), &mut ops)
            .expect("per-op errno on a live ring is not session death");
        (used, ops[0].result, ops[1].result)
    };
    assert!(used);
    assert!(
        !sess.is_poisoned(),
        "write-only pread errno must not poison"
    );
    assert_eq!(r0, 4);
    assert_eq!(&good, b"abcd");
    assert!(r1 < 0, "write-only pread must be errno, got {r1}");
    sess.drain_all().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn held_pread_poison_stays_fail_closed() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-held-poison-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"abcd").unwrap();
        f.flush().unwrap();
    }
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);
    let mut sess = rbitcoin_store::uring_session::UringSession::try_open(32).unwrap_or_else(|_| {
        rbitcoin_store::uring_session::UringSession::try_open_kind(
            rbitcoin_store::uring_session::SessionKind::Pool,
            32,
        )
        .expect("pool")
    });
    sess.poison();
    let mut b = [0u8; 4];
    let mut ops = [ReadOp {
        fd,
        offset: 0,
        buf: &mut b[..],
        result: i32::MIN,
    }];
    match pread_batch_on_ctx(&mut rbitcoin_store::IoCtx::held(&mut sess), &mut ops) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("io_uring"), "{m}");
            assert!(m.contains("poisoned"), "{m}");
        }
        other => panic!("poisoned held pread must stay fail-closed, got {other:?}"),
    }
    assert_eq!(b, [0u8; 4], "must not libc-complete on a poisoned ring");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_held_pread_batch_fail_is_not_recover_abort() {
    match held_pread_after_inner(false, false) {
        Ok(true) => {}
        other => panic!(
            "empty CQ / submit fail on a live ring must libc-complete (Ok(true)), got {other:?}"
        ),
    }
    match held_pread_after_inner(false, true) {
        Err(e) if e.is_uring_session_fault() => {}
        other => panic!("poisoned ring must stay fail-closed, got {other:?}"),
    }
    assert!(held_pread_after_inner(true, false).unwrap());
}

#[test]
fn empty_cq_after_wait_is_retry_not_batch_fail() {
    assert_eq!(empty_harvest_next(true), EmptyHarvest::Retry);
    assert_eq!(empty_harvest_next(false), EmptyHarvest::Fail);
}

#[test]
fn pread_batch_fallback_matches() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-pread-fb-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"hello-fallback-path!!").unwrap();
    f.flush().unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);
    let mut b = [0u8; 5];
    {
        let mut ops = [ReadOp {
            fd,
            offset: 0,
            buf: &mut b[..],
            result: i32::MIN,
        }];
        pread_batch_fallback(&mut ops);
    }
    assert_eq!(&b, b"hello");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pwrite_batch_roundtrip_tmpfile() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-pwrite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        f.set_len(300).unwrap();
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);
    let d0 = [1u8; 50];
    let d1 = [2u8; 50];
    let d2 = [3u8; 50];
    let mut ops = [
        WriteOp {
            fd,
            offset: 0,
            buf: &d0[..],
            result: i32::MIN,
        },
        WriteOp {
            fd,
            offset: 50,
            buf: &d1[..],
            result: i32::MIN,
        },
        WriteOp {
            fd,
            offset: 100,
            buf: &d2[..],
            result: i32::MIN,
        },
    ];
    pwrite_batch(&mut ops);
    for op in &ops {
        assert!(op.result >= 50, "result={}", op.result);
    }
    let mut got = vec![0u8; 150];
    let n = fd.pread(0, &mut got);
    assert_eq!(n, 150);
    assert_eq!(&got[0..50], &d0[..]);
    assert_eq!(&got[50..100], &d1[..]);
    assert_eq!(&got[100..150], &d2[..]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Multiple sequential waves on one thread reuse the TL ring and match
/// libc pread for identical ranges (batch identity under reuse).
#[test]
fn pread_batch_thread_local_reuse_matches_fallback() {
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-uring-tl-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let data: Vec<u8> = (0u16..512).map(|i| (i % 251) as u8).collect();
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&data).unwrap();
        f.flush().unwrap();
    }
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);

    // Three waves — first opens TL ring; later waves must reuse it correctly.
    for wave in 0..3u64 {
        let base = (wave * 64) as usize;
        let mut b0 = [0u8; 32];
        let mut b1 = [0u8; 32];
        {
            let mut ops = [
                ReadOp {
                    fd,
                    offset: base as u64,
                    buf: &mut b0[..],
                    result: i32::MIN,
                },
                ReadOp {
                    fd,
                    offset: (base + 32) as u64,
                    buf: &mut b1[..],
                    result: i32::MIN,
                },
            ];
            pread_batch(&mut ops);
            assert_eq!(ops[0].result, 32, "wave={wave}");
            assert_eq!(ops[1].result, 32, "wave={wave}");
        }
        assert_eq!(&b0[..], &data[base..base + 32], "wave={wave}");
        assert_eq!(&b1[..], &data[base + 32..base + 64], "wave={wave}");

        // Fallback path must agree (same bytes, independent of ring).
        let mut c0 = [0u8; 32];
        let mut c1 = [0u8; 32];
        let mut fops = [
            ReadOp {
                fd,
                offset: base as u64,
                buf: &mut c0[..],
                result: i32::MIN,
            },
            ReadOp {
                fd,
                offset: (base + 32) as u64,
                buf: &mut c1[..],
                result: i32::MIN,
            },
        ];
        pread_batch_fallback(&mut fops);
        assert_eq!(&c0[..], &b0[..]);
        assert_eq!(&c1[..], &b1[..]);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Forces multi-fill of the ring (N > RING_ENTRIES) and checks every byte —
/// exercises pipelined refill, not just a single wave.
#[test]
fn pread_batch_pipeline_over_ring_depth() {
    const N: usize = 200;
    assert!(N > RING_ENTRIES as usize);

    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-uring-pipe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    let data: Vec<u8> = (0..N).map(|i| (i % 251) as u8).collect();
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&data).unwrap();
        f.flush().unwrap();
    }
    let f = std::fs::File::open(&path).unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);

    let mut bufs: Vec<[u8; 1]> = (0..N).map(|_| [0u8; 1]).collect();
    {
        let mut slices: Vec<&mut [u8]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        let mut ops: Vec<ReadOp<'_>> = Vec::with_capacity(N);
        for (i, sl) in slices.iter_mut().enumerate() {
            ops.push(ReadOp {
                fd,
                offset: i as u64,
                buf: sl,
                result: i32::MIN,
            });
        }
        pread_batch(&mut ops);
        for (i, op) in ops.iter().enumerate() {
            assert_eq!(op.result, 1, "i={i} result={}", op.result);
        }
    }
    for (i, b) in bufs.iter().enumerate() {
        assert_eq!(b[0], data[i], "i={i}");
    }

    // Empty buffer interleaved — must not be submitted; index mapping intact.
    let mut b0 = [0u8; 1];
    let mut empty: [u8; 0] = [];
    let mut b1 = [0u8; 1];
    {
        let mut ops2 = [
            ReadOp {
                fd,
                offset: 0,
                buf: &mut b0[..],
                result: i32::MIN,
            },
            ReadOp {
                fd,
                offset: 0,
                buf: &mut empty[..],
                result: i32::MIN,
            },
            ReadOp {
                fd,
                offset: 1,
                buf: &mut b1[..],
                result: i32::MIN,
            },
        ];
        pread_batch(&mut ops2);
        assert_eq!(ops2[0].result, 1);
        assert_eq!(ops2[1].result, 0);
        assert_eq!(ops2[2].result, 1);
    }
    assert_eq!(b0[0], data[0]);
    assert_eq!(b1[0], data[1]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Poisoned session `begin_batch` is batch-false, not process `URING_MODE=2`.
#[test]
fn poisoned_session_batch_false_does_not_disable_uring() {
    if !io_uring_enabled() {
        return;
    }
    let mut session = rbitcoin_store::uring_session::UringSession::try_open(32).expect("session");
    session.poison();
    let dir = std::env::temp_dir().join(format!(
        "rbitcoin-mid-wave-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("blob");
    {
        let f = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.set_len(8).unwrap();
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let fd = rbitcoin_store::io_handle::IoHandle::from_file(&f);
    let payload = [0xABu8; 4];
    let mut ops = [WriteOp {
        fd,
        offset: 0,
        buf: &payload[..],
        result: i32::MIN,
    }];
    assert!(
        !pwrite_batch_on_session(&mut session, &mut ops, 1),
        "poisoned begin_batch must be batch-false"
    );
    assert!(
        io_uring_enabled(),
        "mid-wave session false must not store URING_MODE=2"
    );
    pwrite_batch_fallback(&mut ops);
    assert_eq!(ops[0].result, 4);
    let mut got = [0u8; 4];
    assert_eq!(fd.pread(0, &mut got), 4);
    assert_eq!(got, payload);
    let _ = std::fs::remove_dir_all(&dir);
}
