//! `take_queued_batch` on the confirm write drain. `None` is the cold arm.
//! `Some` returns that vec.
use rbitcoin_query::confirm_run::write::take_queued_batch;

#[test]
fn none_queued_batch_is_empty_and_some_is_that_vec() {
    let mut none = None;
    assert!(take_queued_batch(&mut none).is_empty());

    let row = ([9u8; 32], rbitcoin_primitives::Fk(7));
    let mut some = Some(vec![row]);
    assert_eq!(take_queued_batch(&mut some), vec![row]);
    assert!(some.is_none());
}
