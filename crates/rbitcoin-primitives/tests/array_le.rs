use rbitcoin_primitives::{bytes, le_i32, le_u16, le_u32, le_u64};

const WINDOW: [u8; 16] = [1, 4, 7, 10, 13, 16, 19, 22, 25, 28, 31, 34, 37, 40, 43, 46];

#[test]
fn fixed_window_reads_the_offset() {
    assert_eq!(le_u16!(&WINDOW, 0), u16::from_le_bytes([1, 4]));
    assert_eq!(le_u32!(&WINDOW, 2), u32::from_le_bytes([7, 10, 13, 16]));
    assert_eq!(le_i32!(&WINDOW, 2), i32::from_le_bytes([7, 10, 13, 16]));
    assert_eq!(
        le_u64!(&WINDOW, 8),
        u64::from_le_bytes([25, 28, 31, 34, 37, 40, 43, 46])
    );
    assert_eq!(bytes!(&WINDOW, 4, 4), [13, 16, 19, 22]);
    assert_eq!(bytes!(&WINDOW, 0, 16), WINDOW);
}
