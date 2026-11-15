use bitcoin::hashes::Hash;
use bitcoin::Txid;
use rbitcoin_mempool::error::MempoolError;
use rbitcoin_mempool::packed::{VinAux, SIGOP_COST_OFF};
use rbitcoin_mempool::store::Mempool;
use rbitcoin_mempool::store::*;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// Rewrite a closed current pool under `dir` as schema 2 (drop each
/// record's sigops and accept time, stamp headers 2) — pre-upgrade fixture.
pub(crate) fn downgrade_to_schema2(dir: &Path) {
    downgrade_to(dir, 2);
}

/// Rewrite a closed current pool under `dir` as schema 3 (drop each
/// record's accept time, stamp headers 3) — pre-upgrade fixture.
pub(crate) fn downgrade_to_schema3(dir: &Path) {
    downgrade_to(dir, 3);
}

fn downgrade_to(dir: &Path, schema: u16) {
    let mut slots = fs::read(dir.join("slots")).unwrap();
    let body = fs::read(dir.join("tx.body")).unwrap();
    let cap = u32::from_le_bytes(slots[8..12].try_into().unwrap());
    let mut new_body = body[..BODY_HEADER].to_vec();
    for slot in 0..cap as usize {
        let off = SLOTS_HEADER + slot * SLOT_REC;
        if slots[off] != SLOT_LIVE {
            continue;
        }
        let b_off = u64::from_le_bytes(slots[off + 4..off + 12].try_into().unwrap()) as usize;
        let b_len = u32::from_le_bytes(slots[off + 12..off + 16].try_into().unwrap()) as usize;
        let mut rec = body[b_off..b_off + b_len].to_vec();
        // Accept time sits right after sigops; schema 2 drops both.
        let from = if schema == 2 {
            SIGOP_COST_OFF
        } else {
            SIGOP_COST_OFF + 8
        };
        rec.drain(from..SIGOP_COST_OFF + 16);
        slots[off + 4..off + 12].copy_from_slice(&(new_body.len() as u64).to_le_bytes());
        slots[off + 12..off + 16].copy_from_slice(&(rec.len() as u32).to_le_bytes());
        new_body.extend_from_slice(&rec);
    }
    let len = new_body.len() as u64;
    new_body[8..16].copy_from_slice(&len.to_le_bytes());
    new_body[4..6].copy_from_slice(&schema.to_le_bytes());
    slots[4..6].copy_from_slice(&schema.to_le_bytes());
    let mut meta = fs::read(dir.join("meta")).unwrap();
    meta[4..6].copy_from_slice(&schema.to_le_bytes());
    fs::write(dir.join("tx.body"), new_body).unwrap();
    fs::write(dir.join("slots"), slots).unwrap();
    fs::write(dir.join("meta"), meta).unwrap();
}

#[test]
fn schema2_converts_on_open_with_unknown_sigops() {
    let dir = tmp_dir();
    let tx = tiny_tx();
    let tid = tx.compute_txid();
    {
        let mut mp = Mempool::open_or_create(&dir).unwrap();
        let sh = [0x11u8; 32];
        let aux = [VinAux {
            prev_txid: tx.input[0].previous_output.txid,
            vout: tx.input[0].previous_output.vout,
            script_hash: Some(sh),
            create_fk: None,
        }];
        mp.append_live_tx(
            &tx,
            &tid,
            &tx.compute_wtxid(),
            55,
            400,
            12,
            1_700_000_000,
            &aux,
        )
        .unwrap();
        mp.flush().unwrap();
    }
    downgrade_to_schema2(&dir);
    let mut mp = Mempool::open_or_create(&dir).expect("schema 2 converts");
    for f in ["meta", "slots", "tx.body"] {
        let b = fs::read(dir.join(f)).unwrap();
        assert_eq!(&b[4..6], &MEM_SCHEMA.to_le_bytes(), "{f} stamped");
    }
    let live = mp.load_live_txs().unwrap();
    assert_eq!(live.len(), 1);
    let p = &live[0].packed;
    assert_eq!((p.fee_sat, p.weight, p.txid), (55, 400, tid));
    assert_eq!(p.sigop_cost, None, "schema 2 has no cost");
    assert_eq!(p.vins[0].script_hash, Some([0x11u8; 32]), "vin aux kept");

    mp.set_sigop_cost(live[0].slot, 12).unwrap();
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.load_live_txs().unwrap()[0].packed.sigop_cost, Some(12));
}

/// Schema 3 records keep fee, weight, sigops and vin aux, and get the
/// migrate's wall-clock second as their accept time.
#[test]
fn schema3_converts_on_open_with_migrate_accept_time() {
    let dir = tmp_dir();
    let tx = tiny_tx();
    let tid = tx.compute_txid();
    {
        let mut mp = Mempool::open_or_create(&dir).unwrap();
        mp.append_live_tx(
            &tx,
            &tid,
            &tx.compute_wtxid(),
            55,
            400,
            12,
            1_700_000_000,
            &[],
        )
        .unwrap();
        mp.flush().unwrap();
    }
    downgrade_to_schema3(&dir);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mp = Mempool::open_or_create(&dir).expect("schema 3 converts");
    for f in ["meta", "slots", "tx.body"] {
        let b = fs::read(dir.join(f)).unwrap();
        assert_eq!(&b[4..6], &MEM_SCHEMA.to_le_bytes(), "{f} stamped");
    }
    let live = mp.load_live_txs().unwrap();
    assert_eq!(live.len(), 1);
    let p = &live[0].packed;
    assert_eq!((p.fee_sat, p.weight, p.txid), (55, 400, tid));
    assert_eq!(p.sigop_cost, Some(12), "schema 3 cost kept");
    assert!(
        p.accept_secs >= before && p.accept_secs < before + 60,
        "migrate stamps its own time: {} vs {before}",
        p.accept_secs
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(
        mp.load_live_txs().unwrap()[0].packed.accept_secs,
        p.accept_secs,
        "the stamp is durable, not redone each open"
    );
}

/// A legacy LIVE slot whose range runs past the body, or is shorter than
/// the 16-byte fee‖weight prefix, refuses open as Corrupt (no panic).
#[test]
fn schema2_bad_slot_range_is_corrupt() {
    let tx = tiny_tx();
    let tid = tx.compute_txid();
    let range_err = "legacy live slot body range";
    for (len, want) in [
        (None, range_err),
        (Some(V1_BODY_PREFIX - 1), range_err),
        (Some(V1_BODY_PREFIX), "packed live short"),
    ] {
        let dir = tmp_dir();
        {
            let mut mp = Mempool::open_or_create(&dir).unwrap();
            mp.append_live_tx(
                &tx,
                &tid,
                &tx.compute_wtxid(),
                55,
                400,
                12,
                1_700_000_000,
                &[],
            )
            .unwrap();
            mp.flush().unwrap();
        }
        downgrade_to_schema2(&dir);
        let mut slots = fs::read(dir.join("slots")).unwrap();
        let body_len = fs::metadata(dir.join("tx.body")).unwrap().len() as usize;
        let off = SLOTS_HEADER + 12;
        let len = len.unwrap_or(body_len); // body_off > 0, so off+len > logical
        slots[off..off + 4].copy_from_slice(&(len as u32).to_le_bytes());
        fs::write(dir.join("slots"), slots).unwrap();
        let r = Mempool::open_or_create(&dir).err();
        assert!(
            matches!(r, Some(MempoolError::Corrupt(m)) if m == want),
            "len {len}: {r:?}"
        );
    }
}

/// Crash after the migrate renamed body+slots but before meta (or with a
/// stale slots header): open restamps the lagging header, keeps the record.
#[test]
fn stale_meta_or_slots_schema_restamped_on_open() {
    for stale in ["meta", "slots"] {
        let dir = tmp_dir();
        let tid = Txid::from_byte_array([0x21; 32]);
        {
            let mut mp = Mempool::open_or_create(&dir).unwrap();
            put_live(&mut mp, &tid, 5, 400);
            mp.flush().unwrap();
        }
        let mut b = fs::read(dir.join(stale)).unwrap();
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        fs::write(dir.join(stale), b).unwrap();
        let mp = Mempool::open_or_create(&dir).unwrap();
        let b = fs::read(dir.join(stale)).unwrap();
        assert_eq!(&b[4..6], &MEM_SCHEMA.to_le_bytes(), "{stale} restamped");
        assert_eq!(mp.load_live_txs().unwrap()[0].packed.txid, tid);
    }
}

fn tmp_dir() -> rbitcoin_store::testutil::TempDir {
    rbitcoin_store::testutil::TempDir::labeled("mempool").unwrap()
}

fn tiny_tx() -> bitcoin::Transaction {
    bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::MAX,
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1),
            script_pubkey: bitcoin::ScriptBuf::new(),
        }],
    }
}

fn put_live(mp: &mut Mempool, tid: &Txid, fee: u64, weight: u64) -> u32 {
    let tx = tiny_tx();
    mp.append_live_tx(
        &tx,
        tid,
        &tx.compute_wtxid(),
        fee,
        weight,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap()
}

#[test]
fn empty_create_reopen_flush() {
    let dir = tmp_dir();
    {
        let mut mp = Mempool::open_or_create(&dir).expect("create");
        let m = mp.meta();
        assert_eq!(m.generation, 0);
        assert_eq!(m.live_count, 0);
        assert_eq!(m.slot_cap, DEFAULT_SLOT_CAP);
        mp.flush().expect("flush");
        assert_eq!(mp.generation(), 1);
    }
    {
        let mp = Mempool::open_or_create(&dir).expect("reopen");
        assert_eq!(mp.generation(), 1);
        assert_eq!(mp.live_count(), 0);
        assert!(mp.dir().join("meta").exists());
        assert!(mp.dir().join("slots").exists());
        assert!(mp.dir().join("tx.body").exists());
    }
    assert_eq!(
        fs::metadata(dir.join("tx.body")).unwrap().len(),
        BODY_HEADER as u64,
        "an empty body file is the 16-byte header"
    );
    let slots_path = dir.join("slots");
    let slots = fs::read(&slots_path).unwrap();
    let mut longer = slots.clone();
    longer.push(0xab);
    fs::write(&slots_path, &longer).unwrap();
    Mempool::open_or_create(&dir).expect("slots longer than the table still open");
    fs::write(&slots_path, &slots[..slots.len() - 1]).unwrap();
    let err = match Mempool::open_or_create(&dir) {
        Err(e) => e,
        Ok(_) => panic!("a short slots file must not open"),
    };
    assert!(
        matches!(err, MempoolError::Corrupt("slots file short")),
        "{err}"
    );
    fs::write(&slots_path, &slots).unwrap();
    let body_path = dir.join("tx.body");
    let body = fs::read(&body_path).unwrap();
    assert_eq!(body.len(), BODY_HEADER);
    fs::write(&body_path, &body[..BODY_HEADER - 1]).unwrap();
    let err = match Mempool::open_or_create(&dir) {
        Err(e) => e,
        Ok(_) => panic!("a short body file must not open"),
    };
    assert!(
        matches!(err, MempoolError::Corrupt("body too short")),
        "{err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn persist_due_waits_five_seconds() {
    let dir = tmp_dir();
    {
        let mut mp = Mempool::open_or_create(&dir).unwrap();
        mp.set_now_ms(0);
        let tid = Txid::from_byte_array([0x22; 32]);
        put_live(&mut mp, &tid, 1, 400);
        mp.persist_due().unwrap();
        assert_eq!(mp.live_count(), 1);
    }
    {
        let mp = Mempool::open_or_create(&dir).unwrap();
        assert_eq!(mp.live_count(), 0, "admit at t=0 is not durable");
    }
    {
        let mut mp = Mempool::open_or_create(&dir).unwrap();
        mp.set_now_ms(0);
        let tid = Txid::from_byte_array([0x33; 32]);
        put_live(&mut mp, &tid, 1, 400);
        mp.set_now_ms(PERSIST_INTERVAL_MS);
        mp.persist_due().unwrap();
    }
    {
        let mp = Mempool::open_or_create(&dir).unwrap();
        assert_eq!(mp.live_count(), 1, "persist_due after 5s is durable");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn death_batch_fsyncs_meta_once() {
    let dir = tmp_dir();
    let n = 8u32;
    let mut slots = Vec::with_capacity(n as usize);
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    for i in 0..n {
        let mut id = [0u8; 32];
        id[0] = i as u8;
        id[1] = 0xD0;
        slots.push(put_live(&mut mp, &Txid::from_byte_array(id), 1, 400));
    }
    mp.flush().unwrap();
    let body_before = fs::read(dir.join("tx.body")).unwrap();
    let meta_syncs = mp.meta_syncs();
    let slots_syncs = mp.slots_syncs();
    for slot in &slots {
        mp.mark_slot_dead(*slot).unwrap();
    }
    assert_eq!(
        mp.meta_syncs(),
        meta_syncs,
        "DEAD pwrites must not fdatasync meta per removed tx"
    );
    assert_eq!(mp.slots_syncs(), slots_syncs);
    mp.sync_deaths().unwrap();
    assert_eq!(
        mp.meta_syncs(),
        meta_syncs + 1,
        "the batch ends with one meta fsync"
    );
    assert_eq!(
        mp.slots_syncs(),
        slots_syncs + 1,
        "the batch ends with one slots fsync"
    );
    assert_eq!(
        fs::read(dir.join("tx.body")).unwrap(),
        body_before,
        "a death batch must not rewrite tx.body"
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert!(
        mp.load_live_txs().unwrap().is_empty(),
        "batched DEAD marks must not resurrect"
    );
    assert_eq!(mp.live_count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dead_pwrite_of_flushed_slot_is_immediate() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let tid = Txid::from_byte_array([0x11; 32]);
    let slot = put_live(&mut mp, &tid, 1, 400);
    mp.flush().unwrap();
    let body_before = fs::read(dir.join("tx.body")).unwrap();
    let slots_syncs = mp.slots_syncs();
    mp.mark_slot_dead(slot).unwrap();
    assert_eq!(mp.slots_syncs(), slots_syncs, "the pwrite is not the sync");
    mp.sync_deaths().unwrap();
    assert_eq!(mp.slots_syncs(), slots_syncs + 1);
    assert_eq!(mp.live_count(), 0);
    let body_after = fs::read(dir.join("tx.body")).unwrap();
    assert_eq!(
        body_before, body_after,
        "DEAD pwrite must not rewrite tx.body"
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(
        mp.live_count(),
        0,
        "DEAD of a flushed slot is durable without 5s"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dead_of_unpersisted_admit_does_not_resurrect() {
    let dir = tmp_dir();
    {
        let mut mp = Mempool::open_or_create(&dir).unwrap();
        mp.set_now_ms(0);
        let tid = Txid::from_byte_array([0x44; 32]);
        let slot = put_live(&mut mp, &tid, 1, 400);
        mp.mark_slot_dead(slot).unwrap();
        mp.persist_slots_without_body().unwrap();
    }
    {
        let mp = Mempool::open_or_create(&dir).unwrap();
        assert_eq!(mp.live_count(), 0);
        assert!(
            mp.load_live_txs().unwrap().is_empty(),
            "unpersisted admit DEAD must not resurrect"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn persist_slots_without_tail_does_not_point_live_past_body() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let raw = tiny_tx();
    let t1 = Txid::from_byte_array([0x01; 32]);
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    let persisted = mp.body_persisted_len();
    mp.persist_slots_without_body().unwrap();
    let on_disk = fs::read(dir.join("slots")).unwrap();
    assert_eq!(
        on_disk[SLOTS_HEADER], SLOT_FREE,
        "a live row past the durable body is free on disk"
    );
    assert_eq!(
        &on_disk[SLOTS_HEADER + 16..SLOTS_HEADER + 48],
        t1.as_byte_array(),
        "demotion keeps the txid and only clears LIVE"
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    let live = mp
        .load_live_txs()
        .expect("FREE-not-LIVE past EOF must load");
    assert!(
        live.is_empty(),
        "LIVE must not be written past body_persisted_len={persisted}"
    );
    assert_eq!(
        mp.slots[SLOTS_HEADER], SLOT_FREE,
        "a torn LIVE slot past the body is free after open"
    );
    let _ = fs::remove_dir_all(&dir);

    // Slots can reach disk ahead of the published body length. Demoted
    // writes do not create that image; a crash between the two files does.
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let raw = tiny_tx();
    let torn = Txid::from_byte_array([0x11; 32]);
    mp.append_live_tx(
        &raw,
        &torn,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.flush().unwrap();
    drop(mp);
    let mut body = fs::read(dir.join("tx.body")).unwrap();
    let published = u64::from_le_bytes(body[8..16].try_into().unwrap());
    assert!(published > BODY_HEADER as u64);
    body[8..16].copy_from_slice(&(BODY_HEADER as u64).to_le_bytes());
    fs::write(dir.join("tx.body"), &body).unwrap();
    assert_eq!(
        fs::read(dir.join("slots")).unwrap()[SLOTS_HEADER],
        SLOT_LIVE,
        "the slots file still names the row live"
    );
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert!(
        mp.load_live_txs().unwrap().is_empty(),
        "a row past the published body is not a live tx"
    );
    assert_eq!(
        mp.slots[SLOTS_HEADER], SLOT_FREE,
        "open frees a live row whose body was not published"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn persist_deaths_writes_slots_not_body() {
    let dir = tmp_dir();
    let n = 8u32;
    let mut slots = Vec::with_capacity(n as usize);
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    for i in 0..n {
        let mut id = [0u8; 32];
        id[0] = i as u8;
        id[1] = (i >> 8) as u8;
        let tid = Txid::from_byte_array(id);
        slots.push(put_live(&mut mp, &tid, 1, 400));
    }
    mp.flush().unwrap();
    let body_before = fs::read(dir.join("tx.body")).unwrap();
    for slot in &slots {
        mp.mark_slot_dead(*slot).unwrap();
    }
    mp.persist_if_dirty().unwrap();
    let body_after = fs::read(dir.join("tx.body")).unwrap();
    assert_eq!(
        body_before, body_after,
        "DEAD persist must not rewrite mempool/tx.body"
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.live_count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn incremental_body_persist_writes_only_dirty_tail() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let raw = tiny_tx();
    let t1 = Txid::from_byte_array([0x01; 32]);
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.set_now_ms(PERSIST_INTERVAL_MS);
    mp.persist_due().unwrap();
    let first_end = mp.body_persisted_len();
    let body_after_first = fs::read(dir.join("tx.body")).unwrap();
    let t2 = Txid::from_byte_array([0x02; 32]);
    mp.append_live_tx(
        &raw,
        &t2,
        &raw.compute_wtxid(),
        2,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.set_now_ms(PERSIST_INTERVAL_MS * 2);
    mp.persist_due().unwrap();
    let body_after_second = fs::read(dir.join("tx.body")).unwrap();
    assert_eq!(
        &body_after_second[BODY_HEADER..first_end as usize],
        &body_after_first[BODY_HEADER..],
        "first payload bytes must be unchanged"
    );
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.load_live_txs().unwrap().len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn persist_due_pwrites_only_new_live_slots() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let raw = tiny_tx();
    let t1 = Txid::from_byte_array([0x01; 32]);
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.set_now_ms(PERSIST_INTERVAL_MS);
    mp.persist_due().unwrap();
    let t2 = Txid::from_byte_array([0x02; 32]);
    mp.append_live_tx(
        &raw,
        &t2,
        &raw.compute_wtxid(),
        2,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    let t3 = Txid::from_byte_array([0x03; 32]);
    mp.append_live_tx(
        &raw,
        &t3,
        &raw.compute_wtxid(),
        3,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    // A byte just past the new slot records. A widened record range rewrites it.
    let past = SLOTS_HEADER + 3 * SLOT_REC;
    {
        let mut slots = OpenOptions::new()
            .write(true)
            .open(dir.join("slots"))
            .unwrap();
        slots.seek(SeekFrom::Start(past as u64)).unwrap();
        slots.write_all(&[0xab]).unwrap();
        slots.sync_all().unwrap();
    }
    // Cap the walk at the live rows so a divided slot offset cannot alias
    // slot 1 from an index thousands of rows later.
    mp.testing_pin_full_slot_table();
    mp.set_now_ms(PERSIST_INTERVAL_MS * 2);
    mp.persist_due().unwrap();
    let on_disk = fs::read(dir.join("slots")).unwrap();
    assert_eq!(
        on_disk[past], 0xab,
        "persist must not rewrite the slot past the new records"
    );
    assert_eq!(on_disk[SLOTS_HEADER + SLOT_REC], SLOT_LIVE);
    assert_eq!(on_disk[SLOTS_HEADER + 2 * SLOT_REC], SLOT_LIVE);
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.load_live_txs().unwrap().len(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flush_bumps_generation_monotone() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    for i in 1..=5 {
        mp.flush().unwrap();
        assert_eq!(mp.generation(), i);
    }
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.generation(), 5);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn abandon_live_clears_reopen() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let tid = Txid::from_byte_array([0x22; 32]);
    put_live(&mut mp, &tid, 1, 400);
    mp.flush().unwrap();
    assert_eq!(mp.live_count(), 1);
    assert_eq!(mp.abandon_live().unwrap(), 1);
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.live_count(), 0);
    assert!(mp.load_live_txs().unwrap().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn append_mark_dead_stats_and_bad_magic() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let txid = Txid::from_byte_array([0x11; 32]);
    let slot = mp
        .append_live_tx(
            &tiny_tx(),
            &txid,
            &tiny_tx().compute_wtxid(),
            10,
            400,
            0,
            1_700_000_000,
            &[],
        )
        .unwrap();
    assert_eq!(mp.live_count(), 1);
    let (free, live, dead) = mp.slot_stats();
    assert_eq!(live, 1);
    assert!(free + live + dead >= 1);
    mp.flush().unwrap();
    drop(mp);
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.live_count(), 1);
    let (free2, live2, _dead2) = mp.slot_stats();
    assert_eq!(live2, 1);
    assert!(free2 + live2 >= 1);
    mp.mark_slot_dead(slot).unwrap();
    assert_eq!(mp.live_count(), 0);
    assert!(mp.mark_slot_dead(u32::MAX).is_err());
    drop(mp);

    let dir2 = tmp_dir();
    {
        let _ = Mempool::open_or_create(&dir2).unwrap();
    }
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(dir2.join("meta"))
            .unwrap();
        f.write_all(b"BAD!").unwrap();
    }
    match Mempool::open_or_create(&dir2) {
        Err(MempoolError::BadMagic) => {}
        Ok(_) => panic!("expected BadMagic, got Ok"),
        Err(e) => panic!("expected BadMagic, got {e}"),
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&dir2);
}

/// Legacy tiny slot table must grow instead of returning Corrupt("slot table full").
#[test]
fn full_live_table_grows_not_corrupt() {
    let dir = tmp_dir();
    // Seed a 4-slot sidecar (legacy mainnet shape).
    fs::create_dir_all(&dir).unwrap();
    let tiny = 4u32;
    {
        let mut meta = [0u8; META_LEN];
        write_meta_bytes(&mut meta, 0, tiny, 0);
        fs::write(dir.join("meta"), meta).unwrap();
        let mut slots = vec![0u8; SLOTS_HEADER + (tiny as usize) * SLOT_REC];
        slots[0..4].copy_from_slice(&MEM_MAGIC);
        slots[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        slots[8..12].copy_from_slice(&tiny.to_le_bytes());
        fs::write(dir.join("slots"), &slots).unwrap();
        let mut body = vec![0u8; BODY_HEADER];
        body[0..4].copy_from_slice(&MEM_MAGIC);
        body[4..6].copy_from_slice(&MEM_SCHEMA.to_le_bytes());
        body[8..16].copy_from_slice(&(BODY_HEADER as u64).to_le_bytes());
        fs::write(dir.join("tx.body"), &body).unwrap();
    }
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.meta().slot_cap, tiny);
    for i in 0..tiny {
        let mut tid = [0u8; 32];
        tid[0] = i as u8 + 1;
        let txid = Txid::from_byte_array(tid);
        put_live(&mut mp, &txid, 1, 400);
    }
    assert!(!mp.has_free_slot());
    // 5th append must grow, not Corrupt.
    let tid5 = Txid::from_byte_array([0x55; 32]);
    let tx = tiny_tx();
    let r = mp.append_live_tx(
        &tx,
        &tid5,
        &tx.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    );
    assert!(
        r.is_ok(),
        "expected grow on full table, got {:?}",
        r.err().map(|e| e.to_string())
    );
    assert_eq!(mp.meta().slot_cap, 16_388);
    assert_eq!(mp.live_count(), tiny + 1);
    drop(mp);
    assert_eq!(
        fs::metadata(dir.join("slots")).unwrap().len(),
        786_640,
        "grown slots file is the 16-byte header plus 16388 records"
    );
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.meta().slot_cap, 16_388);
    // Grow persists the four rows already in the table. The fifth slot is
    // RAM until the next sidecar write, so a reopen keeps those four.
    assert_eq!(mp.load_live_txs().unwrap().len(), 4);
    assert!(!format!("{:?}", MempoolError::Full).contains("corrupt"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn persist_new_body_old_slots_reopens() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let raw = tiny_tx();
    let t1 = Txid::from_byte_array([0x01; 32]);
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.flush().unwrap();
    let slots_first = fs::read(dir.join("slots")).unwrap();
    let t2 = Txid::from_byte_array([0x02; 32]);
    mp.append_live_tx(
        &raw,
        &t2,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.flush().unwrap();
    drop(mp);
    fs::write(dir.join("slots"), &slots_first).unwrap();
    let mp = Mempool::open_or_create(&dir).unwrap();
    let live = mp.load_live_txs().expect("new body + old slots must load");
    assert!(live.len() <= 1);
    let _ = fs::remove_dir_all(&dir);

    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    let off = SLOTS_HEADER;
    mp.slots[off + 12..off + 16].copy_from_slice(&88u32.to_le_bytes());
    let short = mp.load_live_txs().unwrap_err().to_string();
    assert!(
        !short.contains("live slot body range"),
        "an 88-byte live record is past the prefix check: {short}"
    );
    mp.slots[off + 12..off + 16].copy_from_slice(&87u32.to_le_bytes());
    let shorter = mp.load_live_txs().unwrap_err().to_string();
    assert!(shorter.contains("live slot body range"), "{shorter}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compact_both_tmps_discarded_on_open() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.flush().unwrap();
    drop(mp);
    fs::copy(dir.join("tx.body"), dir.join("tx.body.tmp")).unwrap();
    fs::copy(dir.join("slots"), dir.join("slots.tmp")).unwrap();
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert!(!dir.join("tx.body.tmp").exists());
    assert!(!dir.join("slots.tmp").exists());
    mp.load_live_txs().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compact_crash_after_slots_rename_before_meta_still_loads() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let raw = tiny_tx();
    let t1 = Txid::from_byte_array([0x01; 32]);
    mp.append_live_tx(
        &raw,
        &t1,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    let t2 = Txid::from_byte_array([0x02; 32]);
    mp.append_live_tx(
        &raw,
        &t2,
        &raw.compute_wtxid(),
        1,
        400,
        0,
        1_700_000_000,
        &[],
    )
    .unwrap();
    mp.flush().unwrap();
    assert_eq!(mp.live_count(), 2);
    let meta_before_compact = fs::read(dir.join("meta")).unwrap();
    mp.mark_slot_dead(0).unwrap();
    mp.compact().unwrap();
    drop(mp);
    fs::write(dir.join("meta"), &meta_before_compact).unwrap();
    let mp = Mempool::open_or_create(&dir).unwrap();
    let live = mp
        .load_live_txs()
        .expect("packed images + stale live_count must load");
    assert_eq!(live.len(), 1, "packed live set");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn leftover_schema_v1_converts_on_open() {
    use bitcoin::consensus::encode::serialize;
    let dir = tmp_dir();
    fs::create_dir_all(&dir).unwrap();
    let tx = tiny_tx();
    let raw = serialize(&tx);
    let tid = tx.compute_txid();
    let fee = 123u64;
    let weight = 400u64;
    {
        let mut meta = [0u8; META_LEN];
        write_meta_bytes(&mut meta, 0, 4, 1);
        meta[4..6].copy_from_slice(&1u16.to_le_bytes());
        fs::write(dir.join("meta"), meta).unwrap();
        let mut slots = vec![0u8; SLOTS_HEADER + 4 * SLOT_REC];
        slots[0..4].copy_from_slice(&MEM_MAGIC);
        slots[4..6].copy_from_slice(&1u16.to_le_bytes());
        slots[8..12].copy_from_slice(&4u32.to_le_bytes());
        let off = SLOTS_HEADER;
        slots[off] = SLOT_LIVE;
        let body_off = BODY_HEADER as u64;
        let body_len = (16 + raw.len()) as u32;
        slots[off + 4..off + 12].copy_from_slice(&body_off.to_le_bytes());
        slots[off + 12..off + 16].copy_from_slice(&body_len.to_le_bytes());
        slots[off + 16..off + 48].copy_from_slice(tid.as_byte_array());
        fs::write(dir.join("slots"), &slots).unwrap();
        let mut body = vec![0u8; BODY_HEADER + body_len as usize];
        body[0..4].copy_from_slice(&MEM_MAGIC);
        body[4..6].copy_from_slice(&1u16.to_le_bytes());
        body[8..16].copy_from_slice(&((BODY_HEADER + body_len as usize) as u64).to_le_bytes());
        body[BODY_HEADER..BODY_HEADER + 8].copy_from_slice(&fee.to_le_bytes());
        body[BODY_HEADER + 8..BODY_HEADER + 16].copy_from_slice(&weight.to_le_bytes());
        body[BODY_HEADER + 16..].copy_from_slice(&raw);
        fs::write(dir.join("tx.body"), &body).unwrap();
    }
    {
        let mp = Mempool::open_or_create(&dir).expect("v1 leftover converts");
        let live = mp.load_live_txs().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].packed.fee_sat, fee);
        assert_eq!(live[0].packed.weight, weight);
        assert_eq!(live[0].packed.txid, tid);
        assert_eq!(live[0].packed.wtxid, tx.compute_wtxid());
        assert_eq!(serialize(&live[0].packed.tx), raw);
        assert!(live[0]
            .packed
            .vins
            .iter()
            .all(|v| v.script_hash.is_none() && v.create_fk.is_none()));
    }
    let meta = fs::read(dir.join("meta")).unwrap();
    assert_eq!(
        &meta[4..6],
        &MEM_SCHEMA.to_le_bytes(),
        "meta stamped current schema"
    );
    let body = fs::read(dir.join("tx.body")).unwrap();
    assert_eq!(
        &body[4..6],
        &MEM_SCHEMA.to_le_bytes(),
        "body stamped current schema"
    );
    {
        let mp = Mempool::open_or_create(&dir).unwrap();
        let live = mp.load_live_txs().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].packed.txid, tid);
        assert_eq!(serialize(&live[0].packed.tx), raw);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn leftover_schema_unknown_is_refused() {
    let dir = tmp_dir();
    let next = MEM_SCHEMA + 1;
    fs::create_dir_all(&dir).unwrap();
    {
        let mut meta = [0u8; META_LEN];
        write_meta_bytes(&mut meta, 0, 4, 0);
        meta[4..6].copy_from_slice(&next.to_le_bytes());
        fs::write(dir.join("meta"), meta).unwrap();
        let mut slots = vec![0u8; SLOTS_HEADER + 4 * SLOT_REC];
        slots[0..4].copy_from_slice(&MEM_MAGIC);
        slots[4..6].copy_from_slice(&next.to_le_bytes());
        fs::write(dir.join("slots"), &slots).unwrap();
        let mut body = vec![0u8; BODY_HEADER];
        body[0..4].copy_from_slice(&MEM_MAGIC);
        body[4..6].copy_from_slice(&next.to_le_bytes());
        body[8..16].copy_from_slice(&(BODY_HEADER as u64).to_le_bytes());
        fs::write(dir.join("tx.body"), &body).unwrap();
    }
    match Mempool::open_or_create(&dir) {
        Err(MempoolError::BadSchema(s)) if s == next => {}
        Ok(_) => panic!("expected BadSchema({next}), got Ok"),
        Err(e) => panic!("expected BadSchema({next}), got {e}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A clean pool skips the next meta fsync. The txs are already on disk
/// either way. The client reads the same set. The skip is what a quiet
/// node does on the persist interval.
#[test]
fn clear_dirty_keeps_a_second_interval_from_rewriting_meta() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    mp.set_now_ms(0);
    let tid = Txid::from_byte_array([0x71; 32]);
    put_live(&mut mp, &tid, 1, 400);
    mp.set_now_ms(PERSIST_INTERVAL_MS);
    mp.persist_due().unwrap();
    let syncs = mp.meta_syncs();
    mp.set_now_ms(PERSIST_INTERVAL_MS * 2);
    mp.persist_due().unwrap();
    assert_eq!(
        mp.meta_syncs(),
        syncs,
        "a clean pool does not fsync meta again"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn open_or_init_meta_reads_written_fields_and_rejects_bad_files() {
    let dir = tmp_dir();
    let path = dir.join("meta");
    let mut buf = [0u8; META_LEN];
    write_meta_bytes(&mut buf, 17, 8, 3);
    fs::write(&path, buf).unwrap();
    let (_file, gen, cap, live, schema) = match open_or_init_meta(&path) {
        Ok(opened) => opened,
        Err(err) => panic!("{err}"),
    };
    assert_eq!((gen, cap, live, schema), (17, 8, 3, MEM_SCHEMA));
    write_meta_bytes(&mut buf, 1, 0, 0);
    fs::write(&path, buf).unwrap();
    match open_or_init_meta(&path) {
        Err(MempoolError::Corrupt(m)) => assert!(m.contains("slot_cap zero"), "{m}"),
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("zero slot_cap opened"),
    }
    let mut junk = [0u8; META_LEN];
    junk[0..4].copy_from_slice(b"BAD!");
    fs::write(&path, junk).unwrap();
    match open_or_init_meta(&path) {
        Err(MempoolError::BadMagic) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("bad magic opened"),
    }
    fs::write(&path, b"rBMP").unwrap();
    match open_or_init_meta(&path) {
        Err(MempoolError::Io { .. }) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("short meta opened"),
    }
    let fresh = dir.join("missing-meta");
    let (_file, gen, cap, live, schema) = match open_or_init_meta(&fresh) {
        Ok(opened) => opened,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(
        (gen, cap, live, schema),
        (0, DEFAULT_SLOT_CAP, 0, MEM_SCHEMA)
    );
}

/// Two new slot rows share one `pwrite` when they are adjacent. Separate
/// writes leave the same bytes a reopen reads. The merge is the persist
/// path for a burst of admits.
#[test]
fn adjacent_slot_records_touch_when_the_next_starts_at_the_end() {
    assert!(Mempool::adjacent_slot_records(64, 64));
    assert!(!Mempool::adjacent_slot_records(64, 112));
}

#[test]
fn abandon_live_clears_every_slot_on_reopen() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    put_live(&mut mp, &Txid::from_byte_array([0x81; 32]), 1, 400);
    put_live(&mut mp, &Txid::from_byte_array([0x82; 32]), 1, 400);
    mp.flush().unwrap();
    // Two live rows and a cap of 2: slot 1 is only reached by `slot * SLOT_REC`.
    mp.testing_pin_full_slot_table();
    assert_eq!(mp.abandon_live().unwrap(), 2);
    drop(mp);
    let mp = Mempool::open_or_create(&dir).unwrap();
    assert_eq!(mp.live_count(), 0);
    assert!(mp.load_live_txs().unwrap().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

/// Free, live, and dead slot counts. No RPC returns the split. Eviction
/// and compact both read it to decide whether a slot is reusable.
#[test]
fn slot_stats_counts_free_live_and_dead() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let cap = mp.meta().slot_cap;
    let slot = put_live(&mut mp, &Txid::from_byte_array([0x91; 32]), 1, 400);
    let (free, live, dead) = mp.slot_stats();
    assert_eq!((free, live, dead), (cap - 1, 1, 0));
    mp.mark_slot_dead(slot).unwrap();
    let (free, live, dead) = mp.slot_stats();
    assert_eq!((free, live, dead), (cap - 1, 0, 1));
    let _ = fs::remove_dir_all(&dir);
}

/// A live slot whose body length runs past the file. Compact refuses it.
/// A normal reopen never builds this row. Startup compact of a torn
/// slots file is the path that returns the error.
#[test]
fn compact_rejects_a_live_slot_whose_body_range_is_not_in_the_file() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    put_live(&mut mp, &Txid::from_byte_array([0xa1; 32]), 1, 400);
    let off = SLOTS_HEADER;
    // A prefix-sized span inside the file is a copy, not a range error.
    mp.slots[off + 12..off + 16].copy_from_slice(&(BODY_TX_PREFIX as u32).to_le_bytes());
    let (n, _) = mp.compact().unwrap();
    assert_eq!(n, 1);

    put_live(&mut mp, &Txid::from_byte_array([0xa2; 32]), 1, 400);
    let off = SLOTS_HEADER;
    let logical = mp.body_logical_len().unwrap();
    let past = (logical as u32).saturating_add(8);
    mp.slots[off + 12..off + 16].copy_from_slice(&past.to_le_bytes());
    match mp.compact() {
        Err(MempoolError::Corrupt("live slot body range")) => {}
        other => panic!("expected live slot body range, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn body_logical_len_reads_the_word_and_rejects_short() {
    let mut body = vec![0u8; BODY_HEADER];
    body[8..16].copy_from_slice(&24u64.to_le_bytes());
    body.extend_from_slice(&[0u8; 8]);
    assert_eq!(body_logical_len(&body).unwrap(), 24);
    match body_logical_len(&body[..8]) {
        Err(MempoolError::Corrupt("body too short")) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("a short body reported a length"),
    }
    body[8..16].copy_from_slice(&4u64.to_le_bytes());
    match body_logical_len(&body) {
        Err(MempoolError::Corrupt("body logical len")) => {}
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("a logical length under the header was accepted"),
    }
}

#[test]
fn live_slot_accept_secs_and_txid_roundtrip() {
    let dir = tmp_dir();
    let mut mp = Mempool::open_or_create(&dir).unwrap();
    let tx = tiny_tx();
    let tid = tx.compute_txid();
    let slot = put_live(&mut mp, &tid, 15, 80);
    assert_eq!(mp.live_accept_secs(slot), Some(1_700_000_000));
    let live = mp.load_live_txs().unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].packed.txid, tid);
    assert_eq!(live[0].packed.fee_sat, 15);
    assert_eq!(live[0].slot, slot);
}
