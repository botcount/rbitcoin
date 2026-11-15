//! Bitcoin CompactSize and unsigned LEB128.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackError(pub &'static str);

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PackError {}

#[inline]
pub fn compact_size_len(n: u64) -> usize {
    if n < 253 {
        1
    } else if n <= u16::MAX as u64 {
        3
    } else if n <= u32::MAX as u64 {
        5
    } else {
        9
    }
}

pub fn write_compact_size(out: &mut Vec<u8>, n: u64) {
    if n < 253 {
        out.push(n as u8);
    } else if n <= u16::MAX as u64 {
        out.push(253);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(254);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(255);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

pub fn read_compact_size(buf: &[u8]) -> Result<(u64, usize), PackError> {
    if buf.is_empty() {
        return Err(PackError("compact size empty"));
    }
    match buf[0] {
        n @ 0..=252 => Ok((u64::from(n), 1)),
        253 => {
            let Some(raw) = buf.first_chunk::<3>() else {
                return Err(PackError("compact size u16 truncated"));
            };
            let v = crate::le_u16!(raw, 1);
            Ok((u64::from(v), 3))
        }
        254 => {
            let Some(raw) = buf.first_chunk::<5>() else {
                return Err(PackError("compact size u32 truncated"));
            };
            let v = crate::le_u32!(raw, 1);
            Ok((u64::from(v), 5))
        }
        255 => {
            let Some(raw) = buf.first_chunk::<9>() else {
                return Err(PackError("compact size u64 truncated"));
            };
            let v = crate::le_u64!(raw, 1);
            Ok((v, 9))
        }
    }
}

#[inline]
pub fn uleb128_len(mut n: u64) -> usize {
    let mut len = 1usize;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

fn emit_uleb128(mut n: u64, mut emit: impl FnMut(u8)) -> usize {
    let mut i = 0usize;
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        emit(b);
        i += 1;
        if n == 0 {
            return i;
        }
    }
}

pub fn write_uleb128_into(dst: &mut [u8], n: u64) -> Result<usize, PackError> {
    let mut i = 0usize;
    let used = emit_uleb128(n, |b| {
        if let Some(slot) = dst.get_mut(i) {
            *slot = b;
        }
        i += 1;
    });
    if used > dst.len() {
        Err(PackError("uleb128 dest short"))
    } else {
        Ok(used)
    }
}

pub fn write_uleb128(out: &mut Vec<u8>, n: u64) {
    emit_uleb128(n, |b| out.push(b));
}

pub fn read_uleb128(buf: &[u8]) -> Result<(u64, usize), PackError> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return Err(PackError("uleb128 overflow"));
        }
        let piece = u64::from(b & 0x7f);
        // Shift 63 has one bit left. A wider payload would be shifted away.
        if shift == 63 && piece > 1 {
            return Err(PackError("uleb128 overflow"));
        }
        result |= piece << shift;
        if b & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    Err(PackError("uleb128 truncated"))
}

pub fn read_compact_size_from(rdr: &mut &[u8]) -> Result<u64, PackError> {
    let (v, n) = read_compact_size(rdr)?;
    *rdr = &rdr[n..];
    Ok(v)
}
