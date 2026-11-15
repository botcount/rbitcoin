use rbitcoin_primitives::Fk;
use rbitcoin_store::address_head::HeadLayout;
use rbitcoin_store::error::StoreError;
use rbitcoin_store::point_table::*;
use rbitcoin_store::spender_table::SpenderTable;
use rbitcoin_store::tx_table::TxTable;

use rbitcoin_store::tx_table::{InputRecord, OutputRecord, TxRecord};

fn tmp() -> std::path::PathBuf {
    let p = rbitcoin_store::testutil::fresh_path("point");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn put_create(txs: &TxTable, txid: [u8; 32], n_out: u32) -> Fk {
    let outs: Vec<_> = (0..n_out)
        .map(|i| OutputRecord::unspent(i as u64 + 1, vec![0x51]))
        .collect();
    let item = (
        TxRecord {
            txid,
            version: 1,
            locktime: 0,
            input_count: 1,
            output_count: n_out,
        },
        vec![InputRecord::coinbase(u32::MAX, vec![0x01], vec![])],
        outs,
    );
    txs.put_full_batch_indexed(&[item], true).unwrap()[0]
}

#[test]
fn spend_annotate_sole_multi_and_visit() {
    let dir = tmp();
    let layout = HeadLayout::new(rbitcoin_store::address_head::TINY_BITS).unwrap();
    let txs = TxTable::create_with_head_layout(&dir, layout).unwrap();
    let spenders = SpenderTable::create(&dir).unwrap();
    let create = put_create(&txs, [1u8; 32], 2);
    let s1 = put_create(&txs, [2u8; 32], 1);
    let s2 = put_create(&txs, [3u8; 32], 1);
    let s3 = put_create(&txs, [4u8; 32], 1);

    assert!(matches!(
        put_spend_on_create(&txs, &spenders, Fk::NULL, 0, s1, 0),
        Err(StoreError::InvalidFk)
    ));
    assert!(matches!(
        put_spend_on_create(&txs, &spenders, create, 0, Fk::NULL, 0),
        Err(StoreError::InvalidFk)
    ));

    put_spend_on_create(&txs, &spenders, create, 0, s1, 0).unwrap();
    put_spend_on_create(&txs, &spenders, create, 0, s1, 0).unwrap();
    put_spend_on_create(&txs, &spenders, create, 0, s2, 1).unwrap();
    put_spend_on_create(&txs, &spenders, create, 0, s3, 2).unwrap();

    let mut visited = Vec::new();
    for_each_spender_create(&txs, &spenders, create, 0, |fk, vin| {
        visited.push((fk, vin));
        Ok(true)
    })
    .unwrap();
    assert_eq!(visited.len(), 3);
    assert_eq!(visited[0], (s3, 2)); // newest head
    assert_eq!(visited[1], (s2, 1));
    assert_eq!(visited[2], (s1, 0));
    let mut n = 0;
    for_each_spender_create(&txs, &spenders, create, 0, |_, _| {
        n += 1;
        Ok(false)
    })
    .unwrap();
    assert_eq!(n, 1);
    for_each_spender_create(&txs, &spenders, Fk::NULL, 0, |_, _| unreachable!()).unwrap();
    for_each_spender_create(&txs, &spenders, Fk(9999), 0, |_, _| unreachable!()).unwrap();
    for_each_spender_create(&txs, &spenders, create, 1, |_, _| unreachable!()).unwrap();

    let (off, len) = txs.spent_range(create).unwrap();
    put_spend_on_create_at(&txs, &spenders, create, 1, s1, 7, Some((off, len))).unwrap();
    let mut one = None;
    for_each_spender_create(&txs, &spenders, create, 1, |fk, vin| {
        one = Some((fk, vin));
        Ok(true)
    })
    .unwrap();
    assert_eq!(one, Some((s1, 7)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn for_each_spender_create_cycle_is_corrupt() {
    let dir = tmp();
    let layout = HeadLayout::new(rbitcoin_store::address_head::TINY_BITS).unwrap();
    let txs = TxTable::create_with_head_layout(&dir, layout).unwrap();
    let spenders = SpenderTable::create(&dir).unwrap();
    let create = put_create(&txs, [1u8; 32], 1);
    let s1 = put_create(&txs, [2u8; 32], 1);
    let s2 = put_create(&txs, [3u8; 32], 1);
    put_spend_on_create(&txs, &spenders, create, 0, s1, 0).unwrap();
    put_spend_on_create(&txs, &spenders, create, 0, s2, 0).unwrap();
    let (_multi, head, _vin) = txs.get_output_spender_meta(create, 0).unwrap();
    let (_sfk, _vin, older) = spenders.get(head).unwrap();
    let older_id = older.get().unwrap();
    let off = rbitcoin_store::file::FILE_HEADER_LEN as u64
        + (older_id - 1) * rbitcoin_store::spender_table::SPENDER_RECORD_LEN as u64;
    let ovf = rbitcoin_store::file::TableFile::open(
        dir.join(rbitcoin_store::spender_table::SPENT_OVF_NAME),
        rbitcoin_primitives::TableKind::Spender,
    )
    .unwrap();
    let mut rec = [0u8; rbitcoin_store::spender_table::SPENDER_RECORD_LEN];
    ovf.read_at(off, &mut rec).unwrap();
    rec[8..16].copy_from_slice(&head.0.to_le_bytes());
    ovf.write_at(off, &rec).unwrap();
    match for_each_spender_create(&txs, &spenders, create, 0, |_, _| Ok(true)) {
        Err(StoreError::Corrupt(m)) => {
            assert!(m.contains("cycle"), "{m}");
        }
        other => panic!("expected cycle Corrupt, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
