//! Little-endian fields and fixed byte windows of a `[u8; N]`.
//!
//! `le_u16!`, `le_u32!`, `le_i32!`, `le_u64!`, and `bytes!` take a literal
//! offset. A window that runs past `N` fails to compile.

mod seal {
    pub trait Seal<const N: usize> {}
    impl<const N: usize> Seal<N> for [u8; N] {}
}

const fn window_fits(off: usize, len: usize, n: usize) -> bool {
    match off.checked_add(len) {
        Some(end) => end <= n,
        None => false,
    }
}

/// Receiver for the fixed-window macros. Not a second call-site API.
#[doc(hidden)]
pub trait ArrayLe<const N: usize>: seal::Seal<N> {
    fn le_u16_at<const OFF: usize>(&self) -> u16;
    fn le_u32_at<const OFF: usize>(&self) -> u32;
    fn le_i32_at<const OFF: usize>(&self) -> i32;
    fn le_u64_at<const OFF: usize>(&self) -> u64;
    fn bytes_at<const OFF: usize, const LEN: usize>(&self) -> [u8; LEN];
}

impl<const N: usize> ArrayLe<N> for [u8; N] {
    fn le_u16_at<const OFF: usize>(&self) -> u16 {
        u16::from_le_bytes(self.bytes_at::<OFF, 2>())
    }

    fn le_u32_at<const OFF: usize>(&self) -> u32 {
        u32::from_le_bytes(self.bytes_at::<OFF, 4>())
    }

    fn le_i32_at<const OFF: usize>(&self) -> i32 {
        i32::from_le_bytes(self.bytes_at::<OFF, 4>())
    }

    fn le_u64_at<const OFF: usize>(&self) -> u64 {
        u64::from_le_bytes(self.bytes_at::<OFF, 8>())
    }

    fn bytes_at<const OFF: usize, const LEN: usize>(&self) -> [u8; LEN] {
        // A literal index pasted by the macro is not a compile error in
        // another crate: `unconditional_panic` does not fire for an external
        // macro. This assert is the check those callers actually hit.
        const {
            assert!(window_fits(OFF, LEN, N), "window runs past the array");
        }
        let mut out = [0u8; LEN];
        out.copy_from_slice(&self[OFF..OFF + LEN]);
        out
    }
}

/// Little-endian `u16` at a literal offset of a `[u8; N]`.
#[macro_export]
macro_rules! le_u16 {
    ($buf:expr, $off:literal) => {{
        use $crate::array_le::ArrayLe as _;
        $buf.le_u16_at::<$off>()
    }};
}

/// Little-endian `u32` at a literal offset of a `[u8; N]`.
#[macro_export]
macro_rules! le_u32 {
    ($buf:expr, $off:literal) => {{
        use $crate::array_le::ArrayLe as _;
        $buf.le_u32_at::<$off>()
    }};
}

/// Little-endian `i32` at a literal offset of a `[u8; N]`.
#[macro_export]
macro_rules! le_i32 {
    ($buf:expr, $off:literal) => {{
        use $crate::array_le::ArrayLe as _;
        $buf.le_i32_at::<$off>()
    }};
}

/// Little-endian `u64` at a literal offset of a `[u8; N]`.
#[macro_export]
macro_rules! le_u64 {
    ($buf:expr, $off:literal) => {{
        use $crate::array_le::ArrayLe as _;
        $buf.le_u64_at::<$off>()
    }};
}

/// `LEN` bytes at a literal offset of a `[u8; N]`, as `[u8; LEN]`.
#[macro_export]
macro_rules! bytes {
    ($buf:expr, $off:literal, $len:literal) => {{
        use $crate::array_le::ArrayLe as _;
        $buf.bytes_at::<$off, $len>()
    }};
}
