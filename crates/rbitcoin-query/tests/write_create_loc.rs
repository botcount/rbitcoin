use rbitcoin_primitives::Fk;
use rbitcoin_query::write_create_loc::*;
use rbitcoin_store::CreateLocPair;

fn pair(n: u64) -> CreateLocPair {
    CreateLocPair {
        txout: (n * 10, 8),
        spent: (n * 9, 8),
        n_out: 1,
    }
}

fn fks(ids: &[u64]) -> Vec<Fk> {
    ids.iter().copied().map(Fk).collect()
}

fn loc(ids: &[u64]) -> Vec<CreateLocPair> {
    ids.iter().copied().map(pair).collect()
}

fn prune(m: &mut WriteCreateLocRam, written_hi: u32) {
    m.prune_written_through(written_hi);
}

#[test]
fn cover_height_keeps_the_lower_pack_on_overlap() {
    let mut m = WriteCreateLocRam::default();
    m.note(2, 9, &fks(&[10, 11, 12]), &loc(&[10, 11, 12]));
    m.note(1, 9, &fks(&[11, 12, 13]), &loc(&[21, 22, 23]));
    assert_eq!(m.cover_height(Fk(10)), Some(2));
    assert_eq!(m.get(Fk(10)).unwrap().txout, pair(10).txout);
    assert_eq!(m.cover_height(Fk(11)), Some(1));
    assert_eq!(m.get(Fk(11)).unwrap().txout, pair(21).txout);
    assert_eq!(m.cover_height(Fk(13)), Some(1));
    assert_eq!(m.cover_height(Fk(9)), None);
    m.note(1, 9, &fks(&[30]), &loc(&[30]));
    assert_eq!(m.cover_height(Fk(11)), Some(2));
    assert_eq!(m.get(Fk(11)).unwrap().txout, pair(11).txout);
    assert_eq!(m.cover_height(Fk(30)), Some(1));
}

#[test]
fn cover_keeps_the_lower_pack_at_the_last_id() {
    let mut m = WriteCreateLocRam::default();
    m.note(2, 9, &fks(&[u64::MAX]), &loc(&[2]));
    m.note(1, 9, &fks(&[u64::MAX - 1, u64::MAX]), &loc(&[11, 12]));
    assert_eq!(m.cover_height(Fk(u64::MAX - 1)), Some(1));
    assert_eq!(m.get(Fk(u64::MAX - 1)).unwrap().txout, pair(11).txout);
    assert_eq!(m.cover_height(Fk(u64::MAX)), Some(1));
    assert_eq!(m.get(Fk(u64::MAX)).unwrap().txout, pair(12).txout);
    m.note(1, 9, &fks(&[u64::MAX]), &loc(&[7]));
    assert_eq!(m.cover_height(Fk(u64::MAX)), Some(1));
    assert_eq!(m.get(Fk(u64::MAX)).unwrap().txout, pair(7).txout);
    assert_eq!(m.cover_height(Fk(u64::MAX - 1)), None);
}

#[test]
fn note_get_by_fk() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &fks(&[10, 11]), &loc(&[10, 11]));
    assert_eq!(m.get(Fk(10)).unwrap().txout, (100, 8));
    assert_eq!(m.get(Fk(11)).unwrap().spent, (99, 8));
    assert!(m.get(Fk(12)).is_none());
    assert!(m.get(Fk(9)).is_none());
}

#[test]
fn prune_written_through_drops_at_keep_until() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &fks(&[10]), &loc(&[10]));
    prune(&mut m, 3);
    assert!(m.get(Fk(10)).is_some(), "written_hi < keep_until keeps");
    prune(&mut m, 4);
    assert!(
        m.get(Fk(10)).is_none(),
        "written_hi == keep_until drops (last overlapping batch finished write)"
    );
    assert_eq!(m.size_snapshot(), (0, 0, 0));
}

#[test]
fn prune_written_through_is_per_pack_keep_until() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &fks(&[10]), &loc(&[10]));
    m.note(2, 8, &fks(&[11]), &loc(&[11]));
    prune(&mut m, 4);
    assert!(m.get(Fk(10)).is_none());
    assert!(m.get(Fk(11)).is_some(), "later pack waits for keep_until 8");
    prune(&mut m, 8);
    assert!(m.get(Fk(11)).is_none());
}

#[test]
fn keep_until_at_note_is_last_started() {
    assert_eq!(keep_until_at_note(360, Some(432)), 432);
    assert_eq!(keep_until_at_note(360, Some(1080)), 1080);
    assert_eq!(keep_until_at_note(360, None), 360);
    assert_eq!(keep_until_at_note(5, Some(3)), 5);
}

#[test]
fn prune_keeps_noting_pack_until_later_write() {
    let mut m = WriteCreateLocRam::default();
    m.note(3, 3, &fks(&[10]), &loc(&[10]));
    prune(&mut m, 3);
    assert!(
        m.get(Fk(10)).is_some(),
        "same-write prune must not drop the noting pack"
    );
    prune(&mut m, 4);
    assert!(m.get(Fk(10)).is_none());
    assert_eq!(m.size_snapshot(), (0, 0, 0));
}

#[test]
fn prune_drops_when_last_started_height_has_written() {
    let mut m = WriteCreateLocRam::default();
    m.note(
        360,
        keep_until_at_note(360, Some(432)),
        &fks(&[10]),
        &loc(&[10]),
    );
    prune(&mut m, 431);
    assert!(
        m.get(Fk(10)).is_some(),
        "intervening writes below last started keep"
    );
    prune(&mut m, 432);
    assert!(
        m.get(Fk(10)).is_none(),
        "drop after last started write (fill of that write already ran)"
    );
}

#[test]
fn prune_does_not_bump_keep_until() {
    let mut m = WriteCreateLocRam::default();
    m.note(
        360,
        keep_until_at_note(360, Some(432)),
        &fks(&[10]),
        &loc(&[10]),
    );
    prune(&mut m, 433);
    assert!(
        m.get(Fk(10)).is_none(),
        "keep_until is fixed at note; later lookup_started_hi is not applied"
    );
}

#[test]
fn no_count_cap_while_keep_until_open() {
    let mut m = WriteCreateLocRam::default();
    for i in 0..8u64 {
        m.note((i + 1) as u32, 100, &[Fk(1000 + i)], &[pair(1000 + i)]);
    }
    let (packs, pairs, bytes) = m.size_snapshot();
    assert_eq!(packs, 8);
    assert_eq!(pairs, 8);
    assert_eq!(bytes, 8 * PAIR_BYTES);
    assert!(m.get(Fk(1000)).is_some());
    assert!(m.get(Fk(1007)).is_some());
    prune(&mut m, 99);
    assert_eq!(m.size_snapshot().0, 8, "no silent FIFO drop");
    prune(&mut m, 100);
    assert_eq!(m.size_snapshot(), (0, 0, 0));
}

#[test]
fn drop_from_height_drops_suffix_and_clamps() {
    let mut m = WriteCreateLocRam::default();
    m.note(5, 20, &fks(&[10]), &loc(&[10]));
    m.note(8, 20, &fks(&[11]), &loc(&[11]));
    m.drop_from_height(8);
    assert!(m.get(Fk(10)).is_some());
    assert!(m.get(Fk(11)).is_none());
    prune(&mut m, 7);
    assert!(
        m.get(Fk(10)).is_none(),
        "keep_until clamped to disconnect-1 so remaining pipeline can retire the pack"
    );
}

#[test]
fn drop_from_height_zero_clears() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &fks(&[10]), &loc(&[10]));
    m.drop_from_height(0);
    assert!(m.get(Fk(10)).is_none());
    assert_eq!(m.size_snapshot(), (0, 0, 0));
}

#[test]
fn empty_or_len_mismatch_is_noop() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &[], &[]);
    m.note(1, 4, &fks(&[10]), &loc(&[10, 11]));
    assert_eq!(m.size_snapshot(), (0, 0, 0));
    m.note(1, 4, &fks(&[0]), &loc(&[0]));
    assert!(m.get(Fk(0)).is_none(), "null fk does not note");
}

#[test]
fn same_pack_height_replace_does_not_double_count_bytes() {
    let mut m = WriteCreateLocRam::default();
    m.note(1, 4, &fks(&[10, 11]), &loc(&[10, 11]));
    let (_, _, once) = m.size_snapshot();
    m.note(1, 8, &fks(&[10]), &loc(&[10]));
    let (packs, pairs, twice) = m.size_snapshot();
    assert_eq!(packs, 1);
    assert_eq!(pairs, 1);
    assert_eq!(twice, PAIR_BYTES);
    assert!(twice < once);
    assert!(m.get(Fk(11)).is_none());
    assert!(m.get(Fk(10)).is_some());
}
