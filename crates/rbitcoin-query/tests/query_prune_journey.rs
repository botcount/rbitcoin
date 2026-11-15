fn connect_range(
    q: &Query,
    mut prev: Fk,
    mut parent_hash: Option<[u8; 32]>,
    n: u32,
    witness: &[u8],
    hashes: &mut Vec<[u8; 32]>,
) -> Fk {
    let start = hashes.len() as u32;
    for h in start..start + n {
        let (mut header, mut ta) = coinbase_block(h, prev, parent_hash);
        if !witness.is_empty() {
            ta.inputs[0].witness = vec![witness.to_vec()];
        }
        bind_fixture_txid(&mut ta);
        // One coinbase: the merkle root is that txid. The fixture's stand-in
        // root fails tip revalidate and drops the height the spill is keyed by.
        if h > 0 {
            header.merkle_root = ta.tx.txid;
            let ph = parent_hash.expect("parent hash");
            header.hash = rbitcoin_store::block_header_hash(
                header.version,
                &ph,
                &header.merkle_root,
                header.timestamp,
                header.bits,
                header.nonce,
            );
        }
        parent_hash = Some(header.hash);
        hashes.push(header.hash);
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
    }
    prev
}

fn witness_of(q: &Query, fk: Fk) -> Vec<Vec<u8>> {
    let tx = q.get_tx(fk).unwrap();
    q.tx_input_at_fk(fk, &tx, 0).unwrap().witness
}

/// The store records this id, and a later connect checks it against the
/// hash of the wire transaction. A height-shaped stand-in fails that check.
fn bind_fixture_txid(ta: &mut TxApply) {
    ta.tx.txid = rbitcoin_query::testutil::tx_apply_to_tx(ta)
        .compute_txid()
        .to_byte_array();
}

fn resume(q: &Query, hashes: &mut Vec<[u8; 32]>) -> (Fk, [u8; 32]) {
    let tip = q.tip_height().unwrap().0;
    hashes.truncate(tip as usize + 1);
    let prev = q.tip_header_fk().unwrap().unwrap();
    (prev, hashes[tip as usize])
}

fn prune_life_window(q: Query, dir: &rbitcoin_query::testutil::TempDir, hashes: &mut Vec<[u8; 32]>) -> Query {
    connect_range(&q, Fk::NULL, None, 300, &[0x55; 72], hashes);
    let tip_fk = q.block_tx_fks(Height(299)).unwrap()[0];
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(witness_of(&q, tip_fk).len(), 1);
    // The synthetic genesis hash is not a header hash. A tip marker keeps
    // reopen on the default window; a missing marker walks from genesis.
    q.store()
        .sync_spend_durable(q.tip_height().unwrap().0)
        .unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(witness_of(&q, tip_fk).len(), 1);
    assert!(q.prune_seqsigwit());
    let (prev, parent) = resume(&q, hashes);
    q.set_ibd_mode(true);
    q.set_seqsigwit_ram_threshold_bytes(1 << 30).unwrap();
    connect_range(&q, prev, Some(parent), 320, &[0x44; 96], hashes);
    let (heights, fks, _, evictions) = q.seqsigwit_ram_window_stats();
    assert!(heights <= Query::SEQSIGWIT_KEEP_HEIGHTS as usize);
    assert!(fks <= Query::SEQSIGWIT_KEEP_HEIGHTS as usize);
    assert!(evictions > 0, "old heights must be evicted");
    let (parent_prev, parent_hash) = resume(&q, hashes);
    connect_range(&q, parent_prev, Some(parent_hash), 1, &[0x22; 16], hashes);
    let after = q.seqsigwit_ram_window_stats().1;
    assert!(after <= Query::SEQSIGWIT_KEEP_HEIGHTS as usize);
    q.disconnect_tip().unwrap();
    hashes.pop();
    assert!(q.seqsigwit_ram_window_stats().1 < after);
    connect_range(&q, parent_prev, Some(parent_hash), 1, &[0x33; 16], hashes);
    assert_eq!(q.seqsigwit_ram_window_stats().1, after);
    q
}

fn prune_life_thresholds(q: &Query, dir: &rbitcoin_query::testutil::TempDir, hashes: &mut Vec<[u8; 32]>) {
    let (prev, parent) = resume(q, hashes);
    q.set_seqsigwit_ram_threshold_bytes(80).unwrap();
    connect_range(q, prev, Some(parent), 8, &[0x77; 128], hashes);
    let (_heights, _fks, bytes, evictions) = q.seqsigwit_ram_window_stats();
    assert!(bytes <= 80, "bytes={bytes}");
    assert!(evictions > 0);
    let (prev, parent) = resume(q, hashes);
    q.set_seqsigwit_ram_threshold_bytes(0).unwrap();
    let tiny_h = hashes.len() as u32;
    connect_range(q, prev, Some(parent), 1, &[0x11], hashes);
    let (heights, fks, bytes, _) = q.seqsigwit_ram_window_stats();
    assert_eq!(heights, 0, "threshold 0 keeps no RAM heights");
    assert_eq!(fks, 0);
    assert_eq!(bytes, 0);
    let tiny = q.block_tx_fks(Height(tiny_h)).unwrap()[0];
    assert_eq!(witness_of(q, tiny), vec![vec![0x11]]);
    let spill = q.store.path().join(format!("seqsigwit.window/{tiny_h}.bin"));
    assert!(spill.is_file());
    #[cfg(unix)]
    {
        let outside = dir.path().join("outside.bin");
        std::fs::rename(&spill, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &spill).unwrap();
        q.clear_seqsigwit_ram_window();
        let tx = q.get_tx(tiny).unwrap();
        let err = q.tx_input_at_fk(tiny, &tx, 0).unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt(msg) if msg.contains("escaped")),
            "{err}"
        );
    }
    let _ = dir;
}

fn prune_life_watermark(q: Query, dir: &rbitcoin_query::testutil::TempDir, hashes: &mut Vec<[u8; 32]>) {
    let (prev, parent) = resume(&q, hashes);
    q.set_seqsigwit_ram_threshold_bytes(1 << 20).unwrap();
    let ram_h = hashes.len() as u32;
    connect_range(&q, prev, Some(parent), 1, &[0x11, 0x22], hashes);
    assert_eq!(q.seqsigwit_ram_window_stats().1, 1);
    let ram_fk = q.block_tx_fks(Height(ram_h)).unwrap()[0];
    assert_eq!(witness_of(&q, ram_fk), vec![vec![0x11, 0x22]]);
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, ram_fk), vec![vec![0x11, 0x22]]);
    let (prev, parent) = resume(&q, hashes);
    q.set_seqsigwit_ram_threshold_bytes(0).unwrap();
    let fall = hashes.len() as u32;
    let prev = connect_range(&q, prev, Some(parent), 2, &[0xab, 0xcd], hashes);
    let window = q.store.path().join("seqsigwit.window");
    assert!(window.join(format!("{fall}.bin")).is_file());
    assert!(window.join(format!("{}.bin", fall + 1)).is_file());
    q.set_pruneheight(Some(Height(fall))).unwrap();
    assert!(!window.join(format!("{fall}.bin")).exists());
    let kept = q
        .reconstruct_archived_block(&hashes[(fall + 1) as usize])
        .unwrap()
        .unwrap();
    assert_eq!(kept.txdata.len(), 1);
    q.set_pruneheight(Some(Height(fall + 1))).unwrap();
    assert!(!window.join(format!("{}.bin", fall + 1)).exists());
    std::fs::write(window.join("0.bin"), b"orphan").unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(q.pruneheight(), Some(Height(fall + 1)));
    assert!(!window.join("0.bin").exists());
    let err = q.disconnect_tip().unwrap_err();
    assert!(matches!(err, StoreError::Pruned { .. }), "{err:?}");
    let err = q.set_prune_seqsigwit(false).unwrap_err().to_string();
    assert!(err.contains("refusing to disable prune-seqsigwit"), "{err}");
    let err = q.reconstruct_archived_block(&hashes[0]).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height: 0 }), "{err:?}");
    let err = q.reconstruct_block_at_height(Height(0)).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height: 0 }), "{err:?}");
    let _ = (prev, std::fs::remove_dir_all(dir));
}

/// Chain taller than the keep window. Two withheld locator headers leave
/// the new blocks confirmed in RAM. Prune must stay on the last sealed tip,
/// so the block at `sealed - 288` still has its witness.
fn withheld_locator_keeps_prune(direct: bool) {
    let (dir, q) = temp_query(if direct {
        "withheld-locator-prune-ibd"
    } else {
        "withheld-locator-prune"
    });
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 290, &[0x55; 8], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(289)));
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(1)));
    let kept = q.block_tx_fks(Height(2)).unwrap()[0];
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, kept), vec![vec![0x55; 8]]);
    if direct {
        q.enter_direct_index_mode_sh(false).unwrap();
    }

    let loc = q.store().txs.create_loc.loc.read_fd();
    rbitcoin_store::io_handle::arm_fail_hwm_pwrites(loc, 16);
    let _disarm = Disarm(loc);
    let (prev, parent) = resume(&q, &mut hashes);
    connect_range(&q, prev, Some(parent), 2, &[0x66; 8], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(291)));
    assert_eq!(
        q.pruneheight(),
        Some(Height(1)),
        "a second withheld locator header does not move seqsigwit.prune"
    );
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, kept), vec![vec![0x55; 8]]);
    if !direct {
        // Drop one unsealed block. The shrink flush must not become the
        // prune anchor, and the next withheld connect must not move it.
        q.disconnect_tip().unwrap();
        assert_eq!(q.tip_height(), Some(Height(290)));
        rbitcoin_store::io_handle::arm_fail_hwm_pwrites(loc, 16);
        let (prev, parent) = resume(&q, &mut hashes);
        connect_range(&q, prev, Some(parent), 1, &[0x77; 8], &mut hashes);
        assert_eq!(q.tip_height(), Some(Height(291)));
        assert_eq!(
            q.pruneheight(),
            Some(Height(1)),
            "disconnect of an unsealed tip does not move seqsigwit.prune"
        );
        q.clear_seqsigwit_ram_window();
        assert_eq!(witness_of(&q, kept), vec![vec![0x55; 8]]);
    }
    let _ = dir;
}

#[test]
fn withheld_locator_does_not_prune_a_confirmed_witness() {
    withheld_locator_keeps_prune(false);
}

#[test]
fn withheld_locator_during_ibd_does_not_prune_a_confirmed_witness() {
    withheld_locator_keeps_prune(true);
}

struct Disarm(rbitcoin_store::io_handle::IoHandle);

impl Drop for Disarm {
    fn drop(&mut self) {
        rbitcoin_store::io_handle::disarm_pwrite(self.0);
    }
}

fn assert_witness_stem_unwritten(q: &Query, body_len: u64) {
    let path = q.store().path().join("seqsigwit.body");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), body_len);
    assert_eq!(
        q.store().txs.seqsigwit.body_logical_len(),
        rbitcoin_store::file::FILE_HEADER_LEN as u64
    );
    assert_eq!(q.store().txs.seqsigwit_loc.count(), 0);
    assert_eq!(q.store().txs.input.count(), q.store().txs.count());
    assert!(q.store().txs.count() > Query::SEQSIGWIT_KEEP_HEIGHTS as u64);
}

/// Prune from an empty store. Confirm keeps the witness stem at the size
/// `create` left (header plus slack) and serves the keep window from spill files.
#[test]
fn pruned_from_genesis_does_not_grow_seqsigwit_body() {
    let (dir, q) = temp_query("prune-from-genesis");
    q.set_prune_seqsigwit(true).unwrap();
    let body = q.store().path().join("seqsigwit.body");
    let body_len = std::fs::metadata(&body).unwrap().len();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 300, &[0x55], &mut hashes);
    let tip = q.tip_height().unwrap().0;
    let ph = q.pruneheight().unwrap().0;
    assert_eq!(ph, tip - Query::SEQSIGWIT_KEEP_HEIGHTS);
    assert_witness_stem_unwritten(&q, body_len);
    for h in (ph + 1)..=tip {
        let block = q.reconstruct_block_at_height(Height(h)).unwrap();
        assert_eq!(
            block.txdata[0].input[0].witness.to_vec(),
            vec![vec![0x55]]
        );
        assert!(q
            .store()
            .path()
            .join(format!("seqsigwit.window/{h}.bin"))
            .is_file());
    }
    let err = q.reconstruct_block_at_height(Height(ph)).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height } if height == ph), "{err:?}");
    q.store().sync_spend_durable(tip).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_witness_stem_unwritten(&q, body_len);
    let block = q.reconstruct_block_at_height(Height(tip)).unwrap();
    assert_eq!(
        block.txdata[0].input[0].witness.to_vec(),
        vec![vec![0x55]]
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn assert_witness_stem_cut(q: &Query) {
    let header = rbitcoin_store::file::FILE_HEADER_LEN as u64;
    for name in ["seqsigwit.body", "seqsigwit.loc"] {
        let len = std::fs::metadata(q.store().path().join(name)).unwrap().len();
        assert_eq!(len, header, "{name}");
    }
    assert_eq!(q.store().txs.seqsigwit_loc.count(), 0);
    assert_eq!(
        q.store().txs.seqsigwit.body_logical_len(),
        header
    );
}

/// Archive first, then enable prune. The keep window is copied out and the
/// witness stem's allocated size returns to the header.
#[test]
fn enabling_prune_cuts_seqsigwit_body() {
    let (dir, q) = temp_query("prune-cut-on-enable");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 300, &[0x42], &mut hashes);
    let n_tx = q.store().txs.count();
    let n_in = q.store().txs.input.count();
    let n_id = q.store().txs.txids.count();
    let tip = q.tip_height().unwrap().0;
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    let ph = q.pruneheight().unwrap().0;
    assert_eq!(ph, tip - Query::SEQSIGWIT_KEEP_HEIGHTS);
    assert_eq!(q.store().txs.count(), n_tx);
    assert_eq!(q.store().txs.input.count(), n_in);
    assert_eq!(q.store().txs.txids.count(), n_id);
    assert_witness_stem_cut(&q);
    let kept = q.reconstruct_block_at_height(Height(tip)).unwrap();
    assert_eq!(kept.txdata[0].input[0].witness.to_vec(), vec![vec![0x42]]);
    let err = q.reconstruct_block_at_height(Height(ph)).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height } if height == ph), "{err:?}");
    q.store().sync_spend_durable(tip).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_witness_stem_cut(&q);
    assert_eq!(q.store().txs.count(), n_tx);
    connect_range(&q, q.tip_header_fk().unwrap().unwrap(), Some(hashes[tip as usize]), 1, &[0x43], &mut hashes);
    assert_witness_stem_cut(&q);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sidecar and a fat stem, one keep-window file absent. Open refills that
/// height from the stem, then cuts the stem.
#[test]
fn startup_refills_missing_window_then_cuts() {
    let (dir, q) = temp_query("prune-refill-cut");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 300, &[0x44], &mut hashes);
    let n_tx = q.store().txs.count();
    let tip = q.tip_height().unwrap().0;
    let ph = tip - Query::SEQSIGWIT_KEEP_HEIGHTS;
    let missing = tip - 1;
    for h in (ph + 1)..=tip {
        if h == missing {
            continue;
        }
        let fks = q.block_tx_fks(Height(h)).unwrap();
        let mut owned = Vec::new();
        for fk in fks {
            let tx = q.get_tx(fk).unwrap();
            let mut ins = Vec::new();
            for i in 0..tx.input_count {
                ins.push(q.tx_input_at_fk(fk, &tx, i).unwrap());
            }
            owned.push((fk, ins));
        }
        let rows: Vec<(Fk, &[InputRecord])> = owned
            .iter()
            .map(|(fk, ins)| (*fk, ins.as_slice()))
            .collect();
        q.persist_seqsigwit_spill_height(Height(h), &rows).unwrap();
    }
    std::fs::write(
        q.store().path().join("seqsigwit.prune"),
        ph.to_le_bytes(),
    )
    .unwrap();
    q.store().sync_spend_durable(tip).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert!(q
        .store()
        .path()
        .join(format!("seqsigwit.window/{missing}.bin"))
        .is_file());
    assert_witness_stem_cut(&q);
    assert_eq!(q.store().txs.count(), n_tx);
    let block = q.reconstruct_block_at_height(Height(missing)).unwrap();
    assert_eq!(
        block.txdata[0].input[0].witness.to_vec(),
        vec![vec![0x44]]
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Locator count is already 0 and the body is still the archive. Open
/// must serve the synced spills and finish the cut, not drop the chain.
#[test]
fn open_finishes_cut_when_locator_is_already_zero() {
    let (dir, q) = temp_query("prune-loc-already-zero");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 300, &[0x46], &mut hashes);
    let n_tx = q.store().txs.count();
    let tip = q.tip_height().unwrap().0;
    let ph = tip - Query::SEQSIGWIT_KEEP_HEIGHTS;
    for h in (ph + 1)..=tip {
        let fks = q.block_tx_fks(Height(h)).unwrap();
        let mut owned = Vec::new();
        for fk in fks {
            let tx = q.get_tx(fk).unwrap();
            let mut ins = Vec::new();
            for i in 0..tx.input_count {
                ins.push(q.tx_input_at_fk(fk, &tx, i).unwrap());
            }
            owned.push((fk, ins));
        }
        let rows: Vec<(Fk, &[InputRecord])> = owned
            .iter()
            .map(|(fk, ins)| (*fk, ins.as_slice()))
            .collect();
        q.persist_seqsigwit_spill_height(Height(h), &rows).unwrap();
    }
    q.persist_pruneheight(ph).unwrap();
    q.store()
        .txs
        .seqsigwit_loc
        .truncate_to_count(0)
        .unwrap();
    let body_len = std::fs::metadata(q.store().path().join("seqsigwit.body"))
        .unwrap()
        .len();
    assert!(body_len > rbitcoin_store::file::FILE_HEADER_LEN as u64);
    q.store().sync_spend_durable(tip).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_witness_stem_cut(&q);
    assert_eq!(q.store().txs.count(), n_tx);
    let block = q.reconstruct_block_at_height(Height(tip)).unwrap();
    assert_eq!(
        block.txdata[0].input[0].witness.to_vec(),
        vec![vec![0x46]]
    );
    let err = q.reconstruct_block_at_height(Height(ph)).unwrap_err();
    assert!(
        matches!(err, StoreError::Pruned { height } if height == ph),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The create locator landed and the witness locator stopped short. Open
/// drops that tail, then copies the keep window from the rows that remain.
#[test]
fn open_truncates_a_short_witness_locator_before_the_cut() {
    let (dir, q) = temp_query("prune-loc-prefix");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 20, &[0x47], &mut hashes);
    let kept = 12u64;
    q.store()
        .txs
        .seqsigwit_loc
        .truncate_to_count(kept)
        .unwrap();
    q.persist_pruneheight(u32::MAX).unwrap();
    q.store().sync_spend_durable(19).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(q.store().txs.count(), kept);
    assert_eq!(q.tip_height(), Some(Height(11)));
    assert_witness_stem_cut(&q);
    let block = q.reconstruct_block_at_height(Height(11)).unwrap();
    assert_eq!(
        block.txdata[0].input[0].witness.to_vec(),
        vec![vec![0x47]]
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `store/` has to be synced after `seqsigwit.window` exists. The sidecar
/// rename syncs `store/` before that directory is created, and the spill
/// rename only syncs the window directory.
#[cfg(unix)]
fn assert_store_synced_after_window(store: &std::path::Path, dirs: &[std::path::PathBuf]) {
    let window = store.join("seqsigwit.window");
    let win_at = dirs.iter().position(|p| p == &window);
    let store_after = dirs.iter().rposition(|p| p == store);
    assert!(
        win_at.is_some() && store_after.is_some() && store_after.unwrap() > win_at.unwrap(),
        "store fsync must follow the window dirent; synced {dirs:?}"
    );
}

#[test]
#[cfg(unix)]
fn enabling_prune_syncs_the_window_dirent_before_the_cut() {
    let (dir, q) = temp_query("prune-fsync-parent");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x49], &mut hashes);
    rbitcoin_store::file::arm_fsync_dir_trace();
    q.set_prune_seqsigwit(true).unwrap();
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    assert_witness_stem_cut(&q);
    assert_store_synced_after_window(q.store().path(), &synced);
    let _ = std::fs::remove_dir_all(dir);
}

/// The spill write creates `seqsigwit.window`. Syncing that directory does
/// not sync its dirent in `store/`. A later confirm flush is not this write.
#[test]
#[cfg(unix)]
fn spill_write_syncs_the_window_dirent() {
    let (dir, q) = temp_query("prune-fsync-spill");
    rbitcoin_store::file::arm_fsync_dir_trace();
    q.persist_seqsigwit_spill_height(Height(0), &[]).unwrap();
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    assert_store_synced_after_window(q.store().path(), &synced);
    let window = q.store().path().join("seqsigwit.window");
    let n = synced.iter().filter(|p| p.as_path() == window).count();
    assert_eq!(
        n, 1,
        "one spill write syncs the window directory once; synced {synced:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `store/` is synced when `seqsigwit.window` is created. A later spill
/// syncs that window directory and leaves `store/` alone.
#[test]
#[cfg(unix)]
fn later_spill_sync_skips_the_store_directory() {
    let (dir, q) = temp_query("prune-fsync-later");
    q.persist_seqsigwit_spill_height(Height(0), &[]).unwrap();
    rbitcoin_store::file::arm_fsync_dir_trace();
    q.persist_seqsigwit_spill_height(Height(1), &[]).unwrap();
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    let window = q.store().path().join("seqsigwit.window");
    assert!(
        synced.iter().any(|p| p.as_path() == window),
        "the window directory is still synced; synced {synced:?}"
    );
    assert!(
        synced.iter().all(|p| p != q.store().path()),
        "a later spill must not fsync store/; synced {synced:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A confirm of several heights writes each file, then syncs the window
/// directory once. The parent `store/` sync still follows that directory.
#[test]
#[cfg(unix)]
fn confirm_batch_syncs_the_window_directory_once() {
    let (dir, q) = temp_query("prune-fsync-batch");
    q.set_prune_seqsigwit(true).unwrap();
    let (h0, t0) = coinbase_block(0, Fk::NULL, None);
    let fk0 = q.commit_class_a_only(&h0, std::slice::from_ref(&t0)).unwrap();
    let (h1, t1) = coinbase_block(1, fk0, Some(h0.hash));
    let fk1 = q.commit_class_a_only(&h1, std::slice::from_ref(&t1)).unwrap();
    let run = vec![
        prepared_at(&q, Height(0), fk0),
        prepared_at(&q, Height(1), fk1),
    ];
    rbitcoin_store::file::arm_fsync_dir_trace();
    q.confirm_blocks_run(&run).unwrap();
    let synced = rbitcoin_store::file::take_fsynced_dirs();
    assert_eq!(q.tip_height(), Some(Height(1)));
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    assert!(q.store().path().join("seqsigwit.window/1.bin").is_file());
    let window = q.store().path().join("seqsigwit.window");
    let n = synced.iter().filter(|p| p.as_path() == window).count();
    assert_eq!(
        n, 1,
        "a confirm batch syncs the window directory once; synced {synced:?}"
    );
    assert_store_synced_after_window(q.store().path(), &synced);
    let _ = std::fs::remove_dir_all(dir);
}

/// Tweaks are on and not caught up. Confirm must not delete a witness the
/// index has not sealed. Once that height is sealed, the prune can move.
#[test]
fn tweaks_hold_the_witness_until_the_row_is_sealed() {
    let (dir, q) = temp_query("prune-tweak-hold");
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    let n = Query::SEQSIGWIT_KEEP_HEIGHTS + 1;
    connect_range(&q, Fk::NULL, None, n, &[0x71], &mut hashes);
    let tip = n - 1;
    assert_eq!(q.tip_height(), Some(Height(tip)));
    assert_eq!(q.tweak_index_next(), Some(0));
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    assert_eq!(q.pruneheight(), None);
    let header0 = q.header_at_height(Height(0)).unwrap().unwrap().0;
    q.put_sp_tweaks_block(Height(0), header0, &[Some([0x02; 33])])
        .unwrap();
    assert_eq!(q.tweak_index_next(), Some(1));
    q.prune_seqsigwit_after_tweak_seal().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    assert!(!q.store().path().join("seqsigwit.window/0.bin").exists());
    assert!(q.store().path().join("seqsigwit.window/1.bin").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

/// Prune and tweaks are on from an empty chain. The first batch past height
/// 288 holds the prune watermark at unset and leaves `0.bin` on disk. A
/// restart must not unlink that file, and tweaks must still turn on.
#[test]
fn held_prune_reopen_keeps_the_window_for_tweaks() {
    let (dir, q) = temp_query("prune-hold-reopen");
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    let n = Query::SEQSIGWIT_KEEP_HEIGHTS + 1;
    connect_range(&q, Fk::NULL, None, n, &[0x71], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(n - 1)));
    assert_eq!(q.pruneheight(), None);
    assert_eq!(q.tweak_index_next(), Some(0));
    let spill = q.store().path().join("seqsigwit.window/0.bin");
    assert!(spill.is_file());
    q.store()
        .sync_spend_durable(q.tip_height().unwrap().0)
        .unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    assert!(
        spill.is_file(),
        "a held prune must not unlink the window on startup"
    );
    assert_eq!(q.pruneheight(), None);
    q.set_sptweaks_enabled(true, Height(0))
        .expect("tweaks still cover the chain while the window is on disk");
    assert_eq!(q.tweak_index_next(), Some(0));
    assert!(spill.is_file(), "enable must not delete the repair copy");
    let _ = std::fs::remove_dir_all(dir);
}

/// Direct mode publishes the tip before `flush_class_c_tip` lands.
/// Sealing tweaks must not prune to that RAM tip. Height 0 stays until
/// the flush is joined and the sealed tip is 288.
#[test]
fn direct_tweak_seal_prunes_only_the_flushed_tip() {
    let (dir, q) = temp_query("prune-tweak-seal-direct");
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    let keep = Query::SEQSIGWIT_KEEP_HEIGHTS;
    connect_range(&q, Fk::NULL, None, keep, &[0x73], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(keep - 1)));
    assert_eq!(q.sealed_tip.get(), Some(Height(keep - 1)));
    q.enter_direct_index_mode_sh(false).unwrap();
    let (prev, parent) = resume(&q, &mut hashes);
    connect_range(&q, prev, Some(parent), 1, &[0x73], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(keep)));
    assert_eq!(q.sealed_tip.get(), Some(Height(keep - 1)));
    assert_eq!(q.pruneheight(), None);
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    let header0 = q.header_at_height(Height(0)).unwrap().unwrap().0;
    q.put_sp_tweaks_block(Height(0), header0, &[Some([0x02; 33])])
        .unwrap();
    assert_eq!(q.tweak_index_next(), Some(1));
    q.prune_seqsigwit_after_tweak_seal().unwrap();
    assert_eq!(
        q.pruneheight(),
        None,
        "the RAM tip is 288 and its flush has not landed"
    );
    assert!(
        q.store().path().join("seqsigwit.window/0.bin").is_file(),
        "height 0 is still inside the flushed tip's keep window"
    );
    q.join_tip_flush().unwrap();
    assert_eq!(q.sealed_tip.get(), Some(Height(keep)));
    q.prune_seqsigwit_after_tweak_seal().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    assert!(!q.store().path().join("seqsigwit.window/0.bin").exists());
    let _ = std::fs::remove_dir_all(dir);
}

/// The sidecar can still say nothing was dropped after the stem is gone.
/// Enabling tweaks has to refuse before it creates an index it cannot fill.
#[test]
fn cut_without_a_watermark_refuses_tweaks() {
    let (dir, q) = temp_query("prune-cut-no-watermark");
    let mut hashes = Vec::new();
    let n = Query::SEQSIGWIT_KEEP_HEIGHTS + 1;
    connect_range(&q, Fk::NULL, None, n, &[0x72], &mut hashes);
    q.set_prune_seqsigwit(true).unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    q.store()
        .sync_spend_durable(q.tip_height().unwrap().0)
        .unwrap();
    q.persist_pruneheight(u32::MAX).unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(q.pruneheight(), None);
    assert!(q.prune_seqsigwit(), "reopen keeps prune mode");
    assert_eq!(q.tip_height(), Some(Height(n - 1)));
    assert!(
        q.store().txs.seqsigwit_loc.count() < q.store().txs.count(),
        "loc {} creates {}",
        q.store().txs.seqsigwit_loc.count(),
        q.store().txs.count()
    );
    let err = q.set_sptweaks_enabled(true, Height(0)).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("taproot") || msg.contains("does not cover"),
        "{msg}"
    );
    assert!(!rbitcoin_store::SpTweaksTable::files_present(
        q.store().cold_files_dir()
    ));
    let _ = std::fs::remove_dir_all(dir);
}

fn already_committed_batch(
    q: &Query,
    header_fk: Fk,
) -> rbitcoin_query::confirm_run::ScriptOkBatch {
    use rbitcoin_query::confirm_run::Prepared;
    let hash = q.get_header(header_fk).unwrap().hash;
    rbitcoin_query::confirm_run::ScriptOkBatch {
        prepared: vec![Prepared {
            height: Height(0),
            header_fk,
            tx_fks: q.block_tx_fks(Height(0)).unwrap(),
            jobs: vec![],
            spends: vec![],
            fees: 0,
            check_scripts: false,
            time: 0,
            bits: bitcoin::CompactTarget::from_consensus(0x207f_ffff),
            hash,
            txids: Vec::new(),
            prev_mtp: 0,
        }],
        wire_blocks: vec![],
        batch_parents: BatchParents::new(),
        archive_plan: None,
        index_seal: rbitcoin_query::confirm_run::index::IndexSeal::default(),
        trust_script_warns: Vec::new(),
    }
}

fn retry_committed_write(q: &Query, header_fk: Fk) {
    rbitcoin_query::confirm_run::confirm_write_phase(
        q,
        &rbitcoin_consensus::ChainParams::regtest(),
        rbitcoin_consensus::Milestone::NONE,
        already_committed_batch(q, header_fk),
    )
    .unwrap();
}

/// The tip is already this height. A second confirm leaves the keep file
/// alone. After that file is removed, the same retry writes it again from
/// the block being connected. The write stage does the same when its batch
/// is already committed.
#[test]
fn pruned_retry_seals_a_missing_keep_file() {
    let (dir, q) = temp_query("prune-retry-keep");
    q.set_prune_seqsigwit(true).unwrap();
    let (header, mut tx) = coinbase_block(0, Fk::NULL, None);
    tx.inputs[0].witness = vec![vec![0x42; 8]];
    bind_fixture_txid(&mut tx);
    let fk = q.connect_block(Height(0), &header, &[tx.clone()]).unwrap();
    let tx_fk = q.block_tx_fks(Height(0)).unwrap()[0];
    let spill = q.store().path().join("seqsigwit.window/0.bin");
    let first = std::fs::read(&spill).unwrap();
    assert_eq!(witness_of(&q, tx_fk), vec![vec![0x42; 8]]);

    q.confirm_blocks_run(std::slice::from_ref(&prepared_at(&q, Height(0), fk)))
        .unwrap();
    retry_committed_write(&q, fk);
    assert_eq!(std::fs::read(&spill).unwrap(), first, "a present keep file stays");
    assert_eq!(q.tip_height(), Some(Height(0)));

    std::fs::remove_file(&spill).unwrap();
    q.clear_seqsigwit_ram_window();
    q.commit_class_a_only(&header, &[tx.clone()]).unwrap();
    q.confirm_blocks_run(std::slice::from_ref(&prepared_at(&q, Height(0), fk)))
        .unwrap();
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, tx_fk), vec![vec![0x42; 8]]);

    std::fs::remove_file(&spill).unwrap();
    q.clear_seqsigwit_ram_window();
    q.commit_class_a_only(&header, &[tx]).unwrap();
    retry_committed_write(&q, fk);
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, tx_fk), vec![vec![0x42; 8]]);
    assert_eq!(q.tip_height(), Some(Height(0)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pruned_seqsigwit_life() {
    let (dir, q) = temp_query("pruned-seqsigwit-life");
    let mut hashes = Vec::new();
    let q = prune_life_window(q, &dir, &mut hashes);
    prune_life_thresholds(&q, &dir, &mut hashes);
    prune_life_watermark(q, &dir, &mut hashes);
}

/// Tip 287 is the whole keep window. Genesis stays, and no watermark is written.
#[test]
fn enabling_prune_at_keep_window_keeps_genesis() {
    let (dir, q) = temp_query("prune-at-287");
    let mut hashes = Vec::new();
    let n = Query::SEQSIGWIT_KEEP_HEIGHTS;
    connect_range(&q, Fk::NULL, None, n, &[0x47], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(n - 1)));
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), None);
    assert_witness_stem_cut(&q);
    q.clear_seqsigwit_ram_window();
    // Genesis uses the fixture header hash. Witness is the served fact.
    let genesis = q.block_tx_fks(Height(0)).unwrap()[0];
    assert_eq!(witness_of(&q, genesis), vec![vec![0x47]]);
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    q.require_seqsigwit_at(Height(0)).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// The stem cut is the step that frees the witness file. An operator has to
/// see that line once. A later start, with the stem already a header, does
/// not say it again.
#[test]
fn cutting_the_witness_stem_logs_once() {
    let (dir, q) = temp_query("prune-cut-log");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x11], &mut hashes);
    rbitcoin_log::capture_logs(true);
    q.set_prune_seqsigwit(true).unwrap();
    let logs = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(
        logs.iter()
            .any(|(_, line)| line.contains("cut the witness stem")),
        "the cut has to say why the witness file shrank, got {logs:?}"
    );
    rbitcoin_log::capture_logs(true);
    q.set_prune_seqsigwit(true).unwrap();
    let again = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(
        again
            .iter()
            .all(|(_, line)| !line.contains("cut the witness stem")),
        "a header-only stem must not log another cut, got {again:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Tip 288 is one past the keep window. The stem cut drops genesis, so the
/// watermark has to say so. A missing watermark lets reconstruct fail as
/// pruned for a height the node still claims to have.
#[test]
fn enabling_prune_at_288_marks_genesis_pruned() {
    let (dir, q) = temp_query("prune-at-288");
    let mut hashes = Vec::new();
    let n = Query::SEQSIGWIT_KEEP_HEIGHTS + 1;
    connect_range(&q, Fk::NULL, None, n, &[0x48], &mut hashes);
    let tip = q.tip_height().unwrap().0;
    assert_eq!(tip, Query::SEQSIGWIT_KEEP_HEIGHTS);
    q.set_prune_seqsigwit(true).unwrap();
    assert_eq!(
        q.pruneheight(),
        Some(Height(0)),
        "the watermark is on disk before the stem cut returns"
    );
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(0)));
    assert_witness_stem_cut(&q);
    q.clear_seqsigwit_ram_window();
    let err = q.require_seqsigwit_at(Height(0)).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height: 0 }), "{err:?}");
    let err = q.reconstruct_block_at_height(Height(0)).unwrap_err();
    assert!(matches!(err, StoreError::Pruned { height: 0 }), "{err:?}");
    q.require_seqsigwit_at(Height(1)).unwrap();
    let kept = q.reconstruct_block_at_height(Height(1)).unwrap();
    assert_eq!(kept.txdata[0].input[0].witness.to_vec(), vec![vec![0x48]]);
    assert!(!q.store().path().join("seqsigwit.window/0.bin").exists());
    assert!(q.store().path().join("seqsigwit.window/1.bin").is_file());
    assert!(q
        .store()
        .path()
        .join(format!("seqsigwit.window/{tip}.bin"))
        .is_file());
    let _ = std::fs::remove_dir_all(dir);
}

/// Tip 299 with pruneheight 11 has already deleted `11.bin`. Disconnecting
/// two blocks and connecting a fork at 298 must not lower the watermark to
/// 10. The next block still has to connect, and height 12 stays readable.
#[test]
fn shorter_fork_does_not_lower_pruneheight() {
    let (dir, q) = temp_query("prune-reorg-watermark");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 300, &[0x55; 8], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(299)));
    q.set_prune_seqsigwit(true).unwrap();
    q.apply_prune_seqsigwit_tip().unwrap();
    assert_eq!(q.pruneheight(), Some(Height(11)));
    let window = q.store().path().join("seqsigwit.window");
    assert!(!window.join("11.bin").exists());
    assert!(window.join("12.bin").is_file());
    q.disconnect_tip().unwrap();
    q.disconnect_tip().unwrap();
    assert_eq!(q.tip_height(), Some(Height(297)));
    let (prev, parent) = resume(&q, &mut hashes);
    connect_range(&q, prev, Some(parent), 1, &[0x66; 8], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(298)));
    assert_eq!(
        q.pruneheight(),
        Some(Height(11)),
        "a shorter fork must not lower pruneheight onto a deleted spill"
    );
    assert!(!window.join("11.bin").exists());
    assert!(window.join("12.bin").is_file());
    let (prev, parent) = resume(&q, &mut hashes);
    connect_range(&q, prev, Some(parent), 1, &[0x77; 8], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(299)));
    assert_eq!(q.pruneheight(), Some(Height(11)));
    q.clear_seqsigwit_ram_window();
    let kept = q.block_tx_fks(Height(12)).unwrap()[0];
    assert_eq!(witness_of(&q, kept), vec![vec![0x55; 8]]);
    // The fixture txid ignores witness, so this tip is the same header.
    // The spill stores the witness from the block just connected.
    let tip = q.block_tx_fks(Height(299)).unwrap()[0];
    assert_eq!(witness_of(&q, tip), vec![vec![0x77; 8]]);
    let _ = std::fs::remove_dir_all(dir);
}

fn next_block(q: &Query, parent_hash: [u8; 32], witness: u8) -> (HeaderRecord, TxApply) {
    let h = q.tip_height().unwrap().0.saturating_add(1);
    let (mut header, mut ta) =
        coinbase_block(h, q.tip_header_fk().unwrap().unwrap(), Some(parent_hash));
    ta.inputs[0].witness = vec![vec![witness]];
    bind_fixture_txid(&mut ta);
    header.merkle_root = ta.tx.txid;
    header.hash = rbitcoin_store::block_header_hash(
        header.version,
        &parent_hash,
        &header.merkle_root,
        header.timestamp,
        header.bits,
        header.nonce,
    );
    (header, ta)
}

/// A spill write that fails must not leave the new tip in RAM. The same
/// block connects once the file can be written.
#[test]
fn spill_failure_does_not_publish_the_tip() {
    let (dir, q) = temp_query("prune-spill-fail");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 1, &[0x11], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(0)));
    let window = q.store().path().join("seqsigwit.window");
    std::fs::create_dir_all(&window).unwrap();
    let blocker = window.join("1.bin.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let (header, ta) = next_block(&q, hashes[0], 0x22);
    let err = q
        .connect_block(Height(1), &header, std::slice::from_ref(&ta))
        .unwrap_err();
    assert_eq!(
        q.tip_height(),
        Some(Height(0)),
        "a failed spill must not publish the tip ({err})"
    );
    std::fs::remove_dir(&blocker).unwrap();
    q.connect_block(Height(1), &header, &[ta]).unwrap();
    assert_eq!(q.tip_height(), Some(Height(1)));
    q.clear_seqsigwit_ram_window();
    let fk = q.block_tx_fks(Height(1)).unwrap()[0];
    assert_eq!(witness_of(&q, fk), vec![vec![0x22]]);
    assert!(window.join("1.bin").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

/// Shutdown must not persist a tip whose keep-window file is missing.
#[test]
fn spill_failure_shutdown_keeps_the_old_tip() {
    let (dir, q) = temp_query("prune-spill-shutdown");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    // Tall enough that reopen checks the real headers, not the fixture genesis.
    connect_range(&q, Fk::NULL, None, 8, &[0x11], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(7)));
    let window = q.store().path().join("seqsigwit.window");
    std::fs::create_dir_all(&window).unwrap();
    std::fs::create_dir(window.join("8.bin.tmp")).unwrap();
    let (header, ta) = next_block(&q, hashes[7], 0x22);
    let _ = q.connect_block(Height(8), &header, &[ta]).unwrap_err();
    assert_eq!(q.tip_height(), Some(Height(7)));
    q.store().sync_spend_durable(7).unwrap();
    q.flush_for_shutdown().unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(q.tip_height(), Some(Height(7)));
    assert!(q.prune_seqsigwit());
    assert!(!q.store().path().join("seqsigwit.window/8.bin").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

fn tagged_block(
    q: &Query,
    parent_hash: [u8; 32],
    witness: u8,
    tag: u8,
) -> (HeaderRecord, TxApply) {
    let (mut header, mut ta) = next_block(q, parent_hash, witness);
    // The tag has to change the coinbase script. Witness does not change txid,
    // and a flipped id byte would no longer be the hash of this transaction.
    ta.inputs[0].script_sig.push(tag);
    bind_fixture_txid(&mut ta);
    header.merkle_root = ta.tx.txid;
    header.hash = rbitcoin_store::block_header_hash(
        header.version,
        &parent_hash,
        &header.merkle_root,
        header.timestamp,
        header.bits,
        header.nonce,
    );
    (header, ta)
}

/// Connect A, replace it with B, then connect A again. A is already archived,
/// so the second connect has to take A's witness from the wire. The spill
/// still holds B.
#[test]
fn reorg_restores_the_archived_witness() {
    let (dir, q) = temp_query("prune-reorg-witness");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 1, &[0x11], &mut hashes);
    let parent = hashes[0];
    let (header_a, ta_a) = tagged_block(&q, parent, 0x11, 1);
    q.connect_block(Height(1), &header_a, std::slice::from_ref(&ta_a))
        .unwrap();
    q.disconnect_tip().unwrap();
    let (header_b, ta_b) = tagged_block(&q, parent, 0x22, 2);
    q.connect_block(Height(1), &header_b, &[ta_b]).unwrap();
    q.clear_seqsigwit_ram_window();
    let b_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    assert_eq!(witness_of(&q, b_fk), vec![vec![0x22]]);
    q.disconnect_tip().unwrap();
    q.connect_block(Height(1), &header_a, &[ta_a]).unwrap();
    q.clear_seqsigwit_ram_window();
    let a_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    assert_eq!(witness_of(&q, a_fk), vec![vec![0x11]]);
    let _ = std::fs::remove_dir_all(dir);
}

/// Spend replay runs before the node serves. It only needs parent edges.
/// Those live in `input`, which this prune does not drop.
#[test]
fn pruned_reopen_replays_spends_without_the_witness() {
    let (dir, q) = temp_query("prune-spend-replay");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 8, &[0x11], &mut hashes);
    assert_eq!(q.tip_height(), Some(Height(7)));
    q.store().sync_spend_durable(0).unwrap();
    q.flush_for_shutdown().unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_eq!(q.tip_height(), Some(Height(7)));
    rbitcoin_query::confirm_run::replay_spend_annotations(&q).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// Verbosity 2 reads prevouts from `input`. Prune mode must not hide a
/// height the node still serves.
#[test]
fn pruned_keep_serves_input_prevouts() {
    let (dir, q) = temp_query("prune-prevouts");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x11], &mut hashes);
    let fk = q.block_tx_fks(Height(1)).unwrap()[0];
    let (tx, prevs) = q.store().get_tx_meta_and_prevouts(fk).unwrap();
    assert_eq!(tx.input_count, 1);
    assert_eq!(prevs.len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// A reader of `seqsigwit.window/{height}.bin` must not see the scripts.
/// The node still serves them with the store secret.
#[test]
fn seqsigwit_window_file_is_not_plaintext() {
    let (dir, q) = temp_query("prune-window-secret");
    q.set_prune_seqsigwit(true).unwrap();
    let witness = b"window-secret-marker";
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 1, witness, &mut hashes);
    let raw = std::fs::read(q.store().path().join("seqsigwit.window/0.bin")).unwrap();
    assert!(
        !raw.windows(witness.len()).any(|w| w == witness.as_slice()),
        "keep-window file must not store the witness in the clear"
    );
    q.clear_seqsigwit_ram_window();
    let fk = q.block_tx_fks(Height(0)).unwrap()[0];
    assert_eq!(witness_of(&q, fk), vec![witness.to_vec()]);
    let _ = std::fs::remove_dir_all(dir);
}

/// Same reorg through the wire confirm path. Lookup skips Class A once the
/// body exists, and the spill still has to be rewritten from that block.
#[test]
fn wire_reorg_restores_the_archived_witness() {
    use bitcoin::absolute::LockTime;
    use bitcoin::block::{Header, Version};
    use bitcoin::{
        Amount, Block, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxMerkleNode,
        TxOut, Witness,
    };
    use rbitcoin_consensus::{ChainParams, Milestone};
    use rbitcoin_query::accept_and_connect_block;

    fn coinbase(height: u32, tag: u8) -> Transaction {
        let mut ss = rbitcoin_consensus::bip34_height_script(height);
        ss.push(tag);
        Transaction {
            version: bitcoin::transaction::Version::ONE,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(ss),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_0000_0000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        }
    }
    fn mine(prev: bitcoin::BlockHash, time: u32, height: u32, tag: u8) -> Block {
        let bits = CompactTarget::from_consensus(0x207f_ffff);
        let mut block = Block {
            header: Header {
                version: Version::from_consensus(4),
                prev_blockhash: prev,
                merkle_root: TxMerkleNode::from_byte_array([0; 32]),
                time,
                bits,
                nonce: 0,
            },
            txdata: vec![coinbase(height, tag)],
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        let target = bitcoin::Target::from_compact(bits);
        for nonce in 0..u32::MAX {
            block.header.nonce = nonce;
            if block.header.validate_pow(target).is_ok() {
                break;
            }
        }
        block
    }

    let (dir, q) = temp_query("prune-wire-reorg");
    q.set_prune_seqsigwit(true).unwrap();
    let params = ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    let a = mine(genesis.block_hash(), 1_700_000_001, 1, 0x11);
    accept_and_connect_block(&q, &params, Height(1), &a, Milestone::NONE).unwrap();
    q.disconnect_tip().unwrap();
    let b = mine(genesis.block_hash(), 1_700_000_002, 1, 0x22);
    accept_and_connect_block(&q, &params, Height(1), &b, Milestone::NONE).unwrap();
    q.clear_seqsigwit_ram_window();
    let b_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    let b_tx = q.get_tx(b_fk).unwrap();
    assert!(q.tx_input_at_fk(b_fk, &b_tx, 0).unwrap().script_sig.ends_with(&[0x22]));
    q.disconnect_tip().unwrap();
    accept_and_connect_block(&q, &params, Height(1), &a, Milestone::NONE).unwrap();
    q.clear_seqsigwit_ram_window();
    let a_fk = q.block_tx_fks(Height(1)).unwrap()[0];
    let a_tx = q.get_tx(a_fk).unwrap();
    assert!(
        q.tx_input_at_fk(a_fk, &a_tx, 0)
            .unwrap()
            .script_sig
            .ends_with(&[0x11]),
        "reconnecting A must serve A's script, not B's"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Master wrote `seqsigwit.window` in the clear. Open must copy the keep
/// window from the stem again before it cuts that stem.
#[test]
fn master_plaintext_window_is_rewritten_before_the_cut() {
    let (dir, q) = temp_query("prune-master-plaintext");
    let witness: &[u8] = b"spar-witness-marker";
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 8, witness, &mut hashes);
    let tip = q.tip_height().unwrap().0;
    for h in 0..=tip {
        let fks = q.block_tx_fks(Height(h)).unwrap();
        let mut raw = Vec::new();
        for fk in fks {
            let (_tx, ins, _outs) = q.store().get_tx_full(fk).unwrap();
            let id = fk.get().unwrap();
            raw.extend_from_slice(&id.to_le_bytes());
            let mut enc = Vec::new();
            rbitcoin_store::encode_seqsigwit_with_secret(&ins, &mut enc, None);
            raw.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            raw.extend_from_slice(&enc);
        }
        let path = q.store().path().join(format!("seqsigwit.window/{h}.bin"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, raw).unwrap();
    }
    q.persist_pruneheight(u32::MAX).unwrap();
    q.store().sync_spend_durable(tip).unwrap();
    q.flush_for_shutdown().unwrap();
    drop(q);
    let q = Query::open_or_create_tiny(dir.path()).unwrap();
    assert_witness_stem_cut(&q);
    q.clear_seqsigwit_ram_window();
    let fk = q.block_tx_fks(Height(4)).unwrap()[0];
    assert_eq!(witness_of(&q, fk), vec![witness.to_vec()]);
    let _ = std::fs::remove_dir_all(dir);
}

/// A height at or below the prune height has no witness file on purpose.
/// Confirming it again is a no-op.
#[test]
fn confirm_of_a_pruned_height_stays_ok() {
    let (dir, q) = temp_query("prune-reconfirm-old");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x61], &mut hashes);
    q.set_pruneheight(Some(Height(0))).unwrap();
    assert!(!q.store().path().join("seqsigwit.window/0.bin").is_file());
    let fk = q.confirm_block(Height(0), &hashes[0]).unwrap();
    assert_eq!(q.tip_height(), Some(Height(1)));
    assert_eq!(q.header_at_height(Height(0)).unwrap().unwrap().0, fk);
    assert!(!q.store().path().join("seqsigwit.window/0.bin").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

/// Turning prune on writes each keep file once. The stem rewrite is that write.
#[test]
fn enabling_prune_writes_each_keep_file_once() {
    let (dir, q) = temp_query("prune-one-write");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x41], &mut hashes);
    rbitcoin_query::arm_keep_io_trace();
    q.set_prune_seqsigwit(true).unwrap();
    assert_eq!(rbitcoin_query::take_keep_writes(), 2);
    q.clear_seqsigwit_ram_window();
    let fk = q.block_tx_fks(Height(1)).unwrap()[0];
    assert_eq!(witness_of(&q, fk), vec![vec![0x41]]);
    let _ = std::fs::remove_dir_all(dir);
}

/// Coverage of one height reads that height file once, however many txs it holds.
#[test]
fn pruned_spill_cover_reads_the_height_file_once() {
    let (dir, q) = temp_query("prune-one-read");
    q.set_prune_seqsigwit(true).unwrap();
    let (header, ta) = coinbase_block(0, Fk::NULL, None);
    let mut second = ta.clone();
    second.tx.txid[30] = 0x22;
    q.connect_block(Height(0), &header, &[ta, second]).unwrap();
    assert_eq!(q.block_tx_fks(Height(0)).unwrap().len(), 2);
    rbitcoin_query::arm_keep_io_trace();
    q.confirm_block(Height(0), &header.hash).unwrap();
    assert_eq!(rbitcoin_query::take_keep_reads(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// A replacement block whose txid does not match the stored create is refused.
/// The stored txid stays, and the missing keep file stays missing.
#[test]
fn resubmit_rejects_a_coinbase_whose_txid_does_not_match() {
    use rbitcoin_consensus::{ChainParams, Milestone};
    use rbitcoin_query::accept_and_connect_block;

    let (dir, q) = temp_query("prune-resubmit-txid");
    q.set_prune_seqsigwit(true).unwrap();
    let params = ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    let tx_fk = q.block_tx_fks(Height(0)).unwrap()[0];
    let stored = q.store().txs.body_txid(tx_fk).unwrap();
    let spill = q.store().path().join("seqsigwit.window/0.bin");
    std::fs::remove_file(&spill).unwrap();
    q.clear_seqsigwit_ram_window();
    let mut evil = genesis.clone();
    evil.txdata[0].input[0].script_sig = bitcoin::ScriptBuf::from_bytes(vec![0x04, 1, 2, 3, 4]);
    assert_ne!(evil.txdata[0].compute_txid().to_byte_array(), stored);
    let header_fk = q.header_at_height(Height(0)).unwrap().unwrap().0;
    let err = q
        .note_pruned_archived_seqsigwit(header_fk, &evil)
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("txid"), "{err:?}");
    assert_eq!(q.store().txs.body_txid(tx_fk).unwrap(), stored);
    assert!(!spill.is_file());
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    assert!(spill.is_file());
    assert_eq!(q.store().txs.body_txid(tx_fk).unwrap(), stored);
    let _ = std::fs::remove_dir_all(dir);
}

/// The wire facade used to reject a block that is already the tip once its
/// keep file was gone. Resubmitting that block writes the file again.
#[test]
fn resubmit_rewrites_a_missing_keep_file() {
    use rbitcoin_consensus::{ChainParams, Milestone};
    use rbitcoin_query::accept_and_connect_block;

    let (dir, q) = temp_query("prune-resubmit-keep");
    q.set_prune_seqsigwit(true).unwrap();
    let params = ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    let spill = q.store().path().join("seqsigwit.window/0.bin");
    let before = std::fs::read(&spill).unwrap();
    std::fs::remove_file(&spill).unwrap();
    q.clear_seqsigwit_ram_window();
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    q.clear_seqsigwit_ram_window();
    assert_eq!(std::fs::read(&spill).unwrap(), before);
    assert_eq!(q.tip_height(), Some(Height(0)));
    let _ = std::fs::remove_dir_all(dir);
}

/// Witness is not in the txid. A resubmit that adds a witness and skips the
/// commitment must not become the kept copy of this tip.
#[test]
fn resubmit_rejects_a_witness_without_a_commitment() {
    use rbitcoin_consensus::{ChainParams, Milestone};
    use rbitcoin_query::accept_and_connect_block;

    let (dir, q) = temp_query("prune-resubmit-wit");
    q.set_prune_seqsigwit(true).unwrap();
    let params = ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    let spill = q.store().path().join("seqsigwit.window/0.bin");
    std::fs::remove_file(&spill).unwrap();
    q.clear_seqsigwit_ram_window();
    let mut evil = genesis.clone();
    evil.txdata[0].input[0].witness = bitcoin::Witness::from_slice(&[&[0u8; 32]]);
    assert_eq!(evil.block_hash(), genesis.block_hash());
    let err =
        accept_and_connect_block(&q, &params, Height(0), &evil, Milestone::NONE).unwrap_err();
    assert!(
        err.to_string().contains("missing witness commitment"),
        "{err}"
    );
    assert!(!spill.is_file());
    accept_and_connect_block(&q, &params, Height(0), &genesis, Milestone::NONE).unwrap();
    assert!(spill.is_file());
    let _ = std::fs::remove_dir_all(dir);
}

/// The append cache is empty on a later seal. The keep file is the witness.
/// A height at or below the prune height is not read. A missing file above
/// that height fails the seal.
#[test]
fn seal_uses_the_keep_file_when_the_append_cache_is_empty() {
    let (dir, q) = temp_query("prune-seal-cache-miss");
    q.set_prune_seqsigwit(true).unwrap();
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x5a], &mut hashes);
    let kept = q.block_tx_fks(Height(1)).unwrap();
    q.clear_seqsigwit_ram_window();
    q.note_seqsigwit_ram_for_confirmed(Height(1), &kept).unwrap();
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, kept[0]), vec![vec![0x5a]]);

    q.set_pruneheight(Some(Height(0))).unwrap();
    q.clear_seqsigwit_ram_window();
    q.note_seqsigwit_ram_for_confirmed(Height(1), &kept).unwrap();
    q.clear_seqsigwit_ram_window();
    assert_eq!(witness_of(&q, kept[0]), vec![vec![0x5a]]);

    let dropped = q.block_tx_fks(Height(0)).unwrap();
    let err = q
        .note_seqsigwit_ram_for_confirmed(Height(0), &dropped)
        .unwrap_err();
    assert!(err.to_string().contains("window miss"), "{err}");

    std::fs::remove_file(q.store().path().join("seqsigwit.window/1.bin")).unwrap();
    q.clear_seqsigwit_ram_window();
    let err = q
        .note_seqsigwit_ram_for_confirmed(Height(1), &kept)
        .unwrap_err();
    assert!(err.to_string().contains("window miss"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn spill_file_roundtrip_reads_the_row_and_rejects_a_short_row() {
    let (dir, q) = temp_query("spill-width");
    let mut hashes = Vec::new();
    connect_range(&q, Fk::NULL, None, 2, &[0x44], &mut hashes);
    q.set_prune_seqsigwit(true).unwrap();
    q.set_pruneheight(Some(Height(0))).unwrap();
    let fk = q.block_tx_fks(Height(1)).unwrap()[0];
    let tx = q.get_tx(fk).unwrap();
    let ins = vec![q.tx_input_at_fk(fk, &tx, 0).unwrap()];
    let rows = [(fk, ins.as_slice())];
    q.persist_seqsigwit_spill_height(Height(1), &rows).unwrap();
    q.clear_seqsigwit_ram_window();
    let got = q
        .seqsigwit_spill_inputs_with_count(fk, tx.input_count)
        .unwrap()
        .expect("spill row");
    assert_eq!(got[0].witness, vec![vec![0x44]]);
    let again = q.confirm_block(Height(1), &hashes[1]).unwrap();
    assert_eq!(again, q.tip_header_fk().unwrap().unwrap());
    let path = q.store().path().join("seqsigwit.window/1.bin");
    let mut raw = std::fs::read(&path).unwrap();
    raw.truncate(12);
    raw[8..12].copy_from_slice(&100u32.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    match q.seqsigwit_spill_inputs_with_count(fk, tx.input_count) {
        Err(err) => assert!(
            err.to_string().contains("seqsigwit spill short row"),
            "{err}"
        ),
        Ok(_) => panic!("a short spill row was read"),
    }
    match q.confirm_block(Height(1), &hashes[1]) {
        Err(err) => assert!(
            err.to_string().contains("seqsigwit spill short row"),
            "{err}"
        ),
        Ok(_) => panic!("confirm treated a short spill as covering"),
    }
    let _ = std::fs::remove_dir_all(dir);
}
