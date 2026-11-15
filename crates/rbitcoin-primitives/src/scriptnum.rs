//! Bitcoin script integer encode/decode (Core scriptnum).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptNumError {
    Overflow,
    NonMinimal,
}

impl fmt::Display for ScriptNumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => f.write_str("scriptnum overflow"),
            Self::NonMinimal => f.write_str("SCRIPTNUM"),
        }
    }
}

impl std::error::Error for ScriptNumError {}

pub fn scriptnum_encode(mut n: i64) -> Vec<u8> {
    if n == 0 {
        return vec![];
    }
    let neg = n < 0;
    if neg {
        n = -n;
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push((n & 0xff) as u8);
        n >>= 8;
    }
    if out.last().map(|b| b & 0x80 != 0).unwrap_or(false) {
        out.push(if neg { 0x80 } else { 0x00 });
    } else if neg {
        let last = out.last_mut().unwrap();
        *last |= 0x80;
    }
    out
}

pub fn scriptnum_is_minimal(vch: &[u8]) -> bool {
    if vch.is_empty() {
        return true;
    }
    if vch[vch.len() - 1] & 0x7f == 0 && (vch.len() <= 1 || (vch[vch.len() - 2] & 0x80) == 0) {
        return false;
    }
    true
}

pub fn scriptnum_decode(v: &[u8], require_minimal: bool) -> Result<i64, ScriptNumError> {
    scriptnum_decode_width(v, 4, require_minimal)
}

/// `max_len` 4 is arithmetic; CLTV/CSV use 5 so a full u32 encodes as positive.
pub fn scriptnum_decode_width(
    v: &[u8],
    max_len: usize,
    require_minimal: bool,
) -> Result<i64, ScriptNumError> {
    if v.len() > max_len {
        return Err(ScriptNumError::Overflow);
    }
    if require_minimal && !scriptnum_is_minimal(v) {
        return Err(ScriptNumError::NonMinimal);
    }
    let Some((&sign, _)) = v.split_last() else {
        return Ok(0);
    };
    let mut result: i64 = 0;
    for (i, &b) in v.iter().enumerate() {
        result |= (b as i64) << (8 * i);
    }
    if sign & 0x80 != 0 {
        result &= !(0x80i64 << (8 * (v.len() - 1)));
        result = -result;
    }
    Ok(result)
}
