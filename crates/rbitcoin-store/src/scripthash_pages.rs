//! Scripthash **page-chain** layout (wired to `put_create` / `entries`).
//!
//! Durable head values are pack8 (8 B; slot **24 B** = key16 + pack8). See
//! [`crate::scripthash_layout`]. Create FKs and body offsets keep bit 63 clear
//! ([`SH_FLAG_BIT`]).
//!
//! # Body page (exactly [`SH_PAGE_SIZE`] = 4096, disk-aligned)
//!
//! ```text
//! offset  len   field
//! 0       8     next_page_off  (0 = end of chain); bit63 must be 0
//! 8       2     n_fks          (u16 LE); FKs stored in this page
//! 10      1     ver            (1 = uleb fk0+deltas; 0 + n>0 = leftover raw)
//! 11      5     reserved       (zero)
//! 16      …     delta stream   (uleb fk0 + uleb gaps), n = n_fks
//! ```
//!
//! [`SH_PAGE_FK_CAP`] = 510 is the raw-u64 historical fill. Delta pages hold
//! more when gaps are small (page full when the next uleb does not fit).
//!
//! Chain is **singly linked** first → … → last. Pack8 paged/extent stores the
//! last-page off; the last page header holds first-page / extent_base.

use crate::error::StoreError;
use crate::scripthash_layout::SH_ENTRY_LEN;
use crate::scripthash_slabs::{decode_fk_delta_stream_into, encode_fk_delta_stream_into};
use rbitcoin_primitives::{
    le_u16, le_u32, le_u64, read_uleb128, uleb128_len, write_uleb128_into, Fk,
};

/// Disk page size for SH FK chains (aligned allocations).
pub const SH_PAGE_SIZE: usize = 4096;
/// Bytes before the FK stream in a page (`ver | n_fks | LAST|idx`).
pub const SH_PAGE_HEADER_LEN: usize = 8;
/// Max create_tx_fks that fit in one page after the header (raw-u64 historical).
pub const SH_PAGE_FK_CAP: usize = (SH_PAGE_SIZE - SH_PAGE_HEADER_LEN) / SH_ENTRY_LEN;

/// Bit 63: must be clear on create FKs and body/page offsets.
pub const SH_FLAG_BIT: u64 = 1u64 << 63;

/// Offset of `ver` within a page.
pub const SH_PAGE_OFF_VER: usize = 0;
/// Offset of `n_fks` (u16 LE) within a page.
pub const SH_PAGE_OFF_N_FKS: usize = 1;
/// Offset of packed `LAST | page_index` (u40 LE) within a page.
pub const SH_PAGE_OFF_PACKED: usize = 3;
/// Bit 0 of packed u40: this page is last (`off` is first page index).
pub const SH_PAGE_LAST_BIT: u64 = 1;
/// Schema-17 megakey pages: uleb fk0 + uleb deltas (not raw u64 slots).
pub const SH_PAGE_DELTA_VER: u8 = 1;
/// Schema-19 last-in-extent / chain-last page: header also holds extent_base + extent_n.
pub const SH_PAGE_EXTENT_VER: u8 = 2;
/// Max FKs if every stream byte is a 1-byte uleb (`ver=1` header).
pub const SH_PAGE_STREAM_MAX: usize = SH_PAGE_SIZE - SH_PAGE_HEADER_LEN;
/// Offset of first FK / delta stream within a `ver=1` page.
pub const SH_PAGE_OFF_FKS: usize = SH_PAGE_HEADER_LEN;
/// `ver=2` header: 8 B packed + 8 B extent_base + 4 B extent_n + 4 B reserved.
pub const SH_PAGE_EXTENT_HEADER_LEN: usize = 24;
pub const SH_PAGE_OFF_EXTENT_BASE: usize = 8;
pub const SH_PAGE_OFF_EXTENT_N: usize = 16;
/// `ver=2` reserved u32: total create count for this megakey (`0` = unknown).
pub const SH_PAGE_OFF_EXTENT_CREATES: usize = 20;
/// Max stream bytes on a `ver=2` page.
pub const SH_PAGE_EXTENT_STREAM_MAX: usize = SH_PAGE_SIZE - SH_PAGE_EXTENT_HEADER_LEN;

#[doc(hidden)]
const _: () = assert!(SH_PAGE_SIZE == 4096);
#[doc(hidden)]
const _: () = assert!(SH_PAGE_HEADER_LEN == 8);
#[doc(hidden)]
const _: () = assert!(SH_ENTRY_LEN == 8);
#[doc(hidden)]
const _: () = assert!(SH_PAGE_FK_CAP == 511);
#[doc(hidden)]
const _: () = assert!(SH_PAGE_OFF_FKS + SH_PAGE_FK_CAP * SH_ENTRY_LEN == SH_PAGE_SIZE);
#[doc(hidden)]
const _: () = assert!(SH_PAGE_EXTENT_HEADER_LEN == 24);
#[doc(hidden)]
const _: () = assert!(SH_PAGE_EXTENT_STREAM_MAX == 4072);

/// Require a full 4 KiB page buffer (rejects unaligned / short slices).
#[inline]
pub fn sh_page_as_array(buf: &[u8]) -> Result<&[u8; SH_PAGE_SIZE], StoreError> {
    buf.try_into()
        .map_err(|_| StoreError::Corrupt("scripthash page: buffer must be exactly 4096 bytes"))
}

/// Mutable full-page view (rejects wrong length).
#[inline]
pub fn sh_page_as_array_mut(buf: &mut [u8]) -> Result<&mut [u8; SH_PAGE_SIZE], StoreError> {
    buf.try_into()
        .map_err(|_| StoreError::Corrupt("scripthash page: buffer must be exactly 4096 bytes"))
}

#[doc(hidden)]
pub fn sh_page_write_packed(
    page: &mut [u8; SH_PAGE_SIZE],
    last: bool,
    byte_off: u64,
) -> Result<(), StoreError> {
    if !byte_off.is_multiple_of(SH_PAGE_SIZE as u64) {
        return Err(StoreError::Corrupt("scripthash page off not 4 KiB aligned"));
    }
    let idx = byte_off / SH_PAGE_SIZE as u64;
    if idx >= (1u64 << 39) {
        return Err(StoreError::Corrupt("scripthash page index overflow"));
    }
    let v = (idx << 1) | u64::from(last);
    let b = v.to_le_bytes();
    page[SH_PAGE_OFF_PACKED..SH_PAGE_OFF_PACKED + 5].copy_from_slice(&b[..5]);
    Ok(())
}

#[doc(hidden)]
pub fn sh_page_read_packed(page: &[u8; SH_PAGE_SIZE]) -> Result<(bool, u64), StoreError> {
    let mut b = [0u8; 8];
    b[..5].copy_from_slice(&page[SH_PAGE_OFF_PACKED..SH_PAGE_OFF_PACKED + 5]);
    let v = u64::from_le_bytes(b);
    let last = v & SH_PAGE_LAST_BIT != 0;
    let idx = v >> 1;
    Ok((last, idx * SH_PAGE_SIZE as u64))
}

/// Zero a page buffer and write header (LAST, first=0, n_fks=0).
#[inline]
pub fn sh_page_init_empty(page: &mut [u8; SH_PAGE_SIZE]) {
    let _ = sh_page_as_array_mut(page);
    page.fill(0);
    page[SH_PAGE_OFF_VER] = SH_PAGE_DELTA_VER;
    let _ = sh_page_write_packed(page, true, 0);
}

/// True when this page is the chain tail (`off` is first, not next).
#[inline]
pub fn sh_page_is_last(page: &[u8; SH_PAGE_SIZE]) -> Result<bool, StoreError> {
    Ok(sh_page_read_packed(page)?.0)
}

/// First page byte off when LAST; error if this page is not last.
#[inline]
pub fn sh_page_first_off(page: &[u8; SH_PAGE_SIZE]) -> Result<u64, StoreError> {
    let (last, off) = sh_page_read_packed(page)?;
    if !last {
        return Err(StoreError::Corrupt(
            "scripthash page: first_off on a non-last page",
        ));
    }
    Ok(off)
}

/// Mark LAST with `first_off` (do not follow as next).
#[inline]
pub fn sh_page_set_last(page: &mut [u8; SH_PAGE_SIZE], first_off: u64) -> Result<(), StoreError> {
    sh_page_write_packed(page, true, first_off)
}

/// Next page byte off (0 if this page is LAST).
#[inline]
pub fn sh_page_next(page: &[u8; SH_PAGE_SIZE]) -> Result<u64, StoreError> {
    let (last, off) = sh_page_read_packed(page)?;
    if last {
        Ok(0)
    } else {
        Ok(off)
    }
}

/// Set next page (clears LAST).
#[inline]
pub fn sh_page_set_next(page: &mut [u8; SH_PAGE_SIZE], next_off: u64) -> Result<(), StoreError> {
    sh_page_write_packed(page, false, next_off)
}

#[inline]
#[doc(hidden)]
pub fn sh_page_stream_off(page: &[u8; SH_PAGE_SIZE]) -> usize {
    if page[SH_PAGE_OFF_VER] == SH_PAGE_EXTENT_VER {
        SH_PAGE_EXTENT_HEADER_LEN
    } else {
        SH_PAGE_HEADER_LEN
    }
}

#[inline]
#[doc(hidden)]
pub fn sh_page_stream_cap(page: &[u8; SH_PAGE_SIZE]) -> usize {
    SH_PAGE_SIZE - sh_page_stream_off(page)
}

/// `(extent_base, extent_n)` when `ver=2`; `None` on `ver=1`.
pub fn sh_page_extent(page: &[u8; SH_PAGE_SIZE]) -> Result<Option<(u64, u32)>, StoreError> {
    if page[SH_PAGE_OFF_VER] != SH_PAGE_EXTENT_VER {
        return Ok(None);
    }
    let base = le_u64!(page, 8);
    let n = le_u32!(page, 16);
    if n == 0 {
        return Err(StoreError::Corrupt("scripthash page: extent_n is 0"));
    }
    if !base.is_multiple_of(SH_PAGE_SIZE as u64) || base == 0 {
        return Err(StoreError::Corrupt(
            "scripthash page: extent_base not 4 KiB aligned",
        ));
    }
    Ok(Some((base, n)))
}

/// Stamp `ver=2` extent fields. Packed next/LAST is unchanged.
pub fn sh_page_set_extent(
    page: &mut [u8; SH_PAGE_SIZE],
    extent_base: u64,
    extent_n: u32,
) -> Result<(), StoreError> {
    if extent_n == 0 {
        return Err(StoreError::Corrupt("scripthash page: extent_n is 0"));
    }
    if !extent_base.is_multiple_of(SH_PAGE_SIZE as u64) || extent_base == 0 {
        return Err(StoreError::Corrupt(
            "scripthash page: extent_base not 4 KiB aligned",
        ));
    }
    page[SH_PAGE_OFF_VER] = SH_PAGE_EXTENT_VER;
    page[SH_PAGE_OFF_EXTENT_BASE..SH_PAGE_OFF_EXTENT_BASE + 8]
        .copy_from_slice(&extent_base.to_le_bytes());
    page[SH_PAGE_OFF_EXTENT_N..SH_PAGE_OFF_EXTENT_N + 4].copy_from_slice(&extent_n.to_le_bytes());
    Ok(())
}

pub fn sh_page_extent_creates(page: &[u8; SH_PAGE_SIZE]) -> u32 {
    if page[SH_PAGE_OFF_VER] != SH_PAGE_EXTENT_VER {
        return 0;
    }
    le_u32!(page, 20)
}

pub fn sh_page_set_extent_creates(page: &mut [u8; SH_PAGE_SIZE], n: u32) {
    if page[SH_PAGE_OFF_VER] != SH_PAGE_EXTENT_VER {
        return;
    }
    page[SH_PAGE_OFF_EXTENT_CREATES..SH_PAGE_OFF_EXTENT_CREATES + 4]
        .copy_from_slice(&n.to_le_bytes());
}

#[doc(hidden)]
pub fn sh_page_write_stream(page: &mut [u8; SH_PAGE_SIZE], fks: &[u64]) -> Result<(), StoreError> {
    if fks.len() > u16::MAX as usize {
        return Err(StoreError::Corrupt(
            "scripthash page pack: entries exceed page capacity",
        ));
    }
    let off = sh_page_stream_off(page);
    match encode_fk_delta_stream_into(&mut page[off..], fks) {
        Ok(_) => {
            sh_page_set_n_fks(page, fks.len() as u16);
            Ok(())
        }
        Err(StoreError::Corrupt("uleb128 dest short")) => Err(StoreError::Corrupt(
            "scripthash page pack: entries exceed page capacity",
        )),
        Err(e) => Err(e),
    }
}

/// Pack a last-in-extent page from raw create fks.
pub fn sh_page_pack_extent_last_fks(
    page: &mut [u8; SH_PAGE_SIZE],
    fks: &[u64],
    extent_base: u64,
    extent_n: u32,
    next_off: u64,
) -> Result<(), StoreError> {
    sh_page_init_empty(page);
    if next_off == 0 {
        sh_page_set_last(page, extent_base)?;
    } else {
        sh_page_set_next(page, next_off)?;
    }
    sh_page_set_extent(page, extent_base, extent_n)?;
    sh_page_write_stream(page, fks)
}

/// Number of FKs stored in this page.
#[inline]
pub fn sh_page_n_fks(page: &[u8; SH_PAGE_SIZE]) -> Result<u16, StoreError> {
    let n = le_u16!(page, 1);
    if n as usize > sh_page_stream_cap(page) {
        return Err(StoreError::Corrupt("scripthash page n_fks > capacity"));
    }
    Ok(n)
}

#[doc(hidden)]
pub fn sh_page_require_delta(page: &[u8; SH_PAGE_SIZE]) -> Result<(), StoreError> {
    let ver = page[SH_PAGE_OFF_VER];
    let n = sh_page_n_fks(page)?;
    if ver == SH_PAGE_DELTA_VER || ver == SH_PAGE_EXTENT_VER {
        return Ok(());
    }
    if ver == 0 && n == 0 {
        return Ok(());
    }
    Err(StoreError::Corrupt(
        "scripthash page leftover raw-u64; rematerialize",
    ))
}

#[doc(hidden)]
pub fn sh_page_stream(page: &[u8; SH_PAGE_SIZE]) -> &[u8] {
    &page[sh_page_stream_off(page)..]
}

#[doc(hidden)]
pub fn sh_page_stream_tail(page: &[u8; SH_PAGE_SIZE]) -> Result<(usize, Option<Fk>), StoreError> {
    sh_page_require_delta(page)?;
    let n = sh_page_n_fks(page)? as usize;
    if n == 0 {
        return Ok((0, None));
    }
    let buf = sh_page_stream(page);
    let cap = sh_page_stream_cap(page);
    let mut off = 0usize;
    let (first, used) = read_uleb128(buf.get(off..).unwrap_or(&[]))?;
    if first == 0 {
        return Err(StoreError::Corrupt("scripthash fk stream null first fk"));
    }
    if first & SH_FLAG_BIT != 0 {
        return Err(StoreError::Corrupt("scripthash fk stream flag bit set"));
    }
    off += used;
    if off > cap {
        return Err(StoreError::Corrupt("scripthash page stream overrun"));
    }
    let mut last = first;
    for _ in 1..n {
        let (d, used) = read_uleb128(buf.get(off..).unwrap_or(&[]))?;
        off += used;
        if off > cap {
            return Err(StoreError::Corrupt("scripthash page stream overrun"));
        }
        if d == 0 {
            return Err(StoreError::Corrupt(
                "invariant: scripthash fk stream zero delta",
            ));
        }
        last = last
            .checked_add(d)
            .ok_or(StoreError::Corrupt("scripthash fk stream delta overflow"))?;
        if last & SH_FLAG_BIT != 0 {
            return Err(StoreError::Corrupt("scripthash fk stream flag bit set"));
        }
    }
    Ok((off, Some(Fk(last))))
}

#[inline]
#[doc(hidden)]
pub fn sh_page_set_n_fks(page: &mut [u8; SH_PAGE_SIZE], n: u16) {
    page[SH_PAGE_OFF_N_FKS..SH_PAGE_OFF_N_FKS + 2].copy_from_slice(&n.to_le_bytes());
}

/// Append page FKs onto `out` (**strictly increasing** create_tx_fk).
pub fn sh_page_entries_into(
    page: &[u8; SH_PAGE_SIZE],
    out: &mut Vec<Fk>,
) -> Result<(), StoreError> {
    sh_page_require_delta(page)?;
    let n = sh_page_n_fks(page)? as usize;
    decode_fk_delta_stream_into(sh_page_stream(page), n, out)
}

/// Entries currently stored in the page (**strictly increasing** create_tx_fk).
#[cfg(debug_assertions)]
pub fn sh_page_entries(page: &[u8; SH_PAGE_SIZE]) -> Result<Vec<Fk>, StoreError> {
    let mut out = Vec::new();
    sh_page_entries_into(page, &mut out)?;
    Ok(out)
}

/// Last create_tx_fk on this page (`None` if empty).
#[inline]
pub fn sh_page_last_fk(page: &[u8; SH_PAGE_SIZE]) -> Result<Option<Fk>, StoreError> {
    Ok(sh_page_stream_tail(page)?.1)
}

/// Split strictly increasing FKs into page-sized delta-stream chunks.
///
/// Intermediate pages use [`SH_PAGE_STREAM_MAX`] (`ver=1`). The last chunk is
/// sized for [`SH_PAGE_EXTENT_STREAM_MAX`] so `ver=2` extent packing cannot
/// overflow the 16 B extra header.
pub fn sh_page_chunk_ranges(fks: &[Fk]) -> Result<Vec<(usize, usize)>, StoreError> {
    if fks.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut used = 0usize;
    for (i, fk) in fks.iter().enumerate() {
        let add = if i == start {
            uleb128_len(fk.0)
        } else {
            let d = fk.0.saturating_sub(fks[i - 1].0);
            uleb128_len(d)
        };
        if i > start && used + add > SH_PAGE_STREAM_MAX {
            out.push((start, i));
            start = i;
            used = uleb128_len(fk.0);
        } else {
            used += add;
        }
    }
    out.push((start, fks.len()));
    split_last_chunk_for_extent(fks, &mut out);
    Ok(out)
}

#[doc(hidden)]
pub fn split_last_chunk_for_extent(fks: &[Fk], chunks: &mut Vec<(usize, usize)>) {
    loop {
        let Some(&(start, end)) = chunks.last() else {
            return;
        };
        if start >= end {
            return;
        }
        let mut used = 0usize;
        let mut split = end;
        for i in start..end {
            let add = if i == start {
                uleb128_len(fks[i].0)
            } else {
                uleb128_len(fks[i].0.saturating_sub(fks[i - 1].0))
            };
            if i > start && used + add > SH_PAGE_EXTENT_STREAM_MAX {
                split = i;
                break;
            }
            used += add;
        }
        if split == end {
            return;
        }
        chunks.pop();
        chunks.push((start, split));
        chunks.push((split, end));
    }
}

/// Pack a `ver=1` page from raw create fks.
pub fn sh_page_pack_fks(
    page: &mut [u8; SH_PAGE_SIZE],
    fks: &[u64],
    next_off: u64,
) -> Result<(), StoreError> {
    sh_page_init_empty(page);
    if next_off == 0 {
        sh_page_set_last(page, 0)?;
    } else {
        sh_page_set_next(page, next_off)?;
    }
    sh_page_write_stream(page, fks)
}

/// Append one create FK to the page. Returns `Ok(true)` if appended, `Ok(false)` if full.
///
/// Requires `fk` **strictly greater** than the page's last FK when non-empty
/// (sorted page chain invariant). Equal/lower is a pack/encode bug — durable
/// re-queue of lower FKs is filtered **before** append by table writers.
pub fn sh_page_try_append(page: &mut [u8; SH_PAGE_SIZE], fk: Fk) -> Result<bool, StoreError> {
    if fk.is_null() {
        return Err(StoreError::InvalidFk);
    }
    if fk.0 & SH_FLAG_BIT != 0 {
        return Err(StoreError::Corrupt(
            "scripthash: create_fk must have bit63 clear",
        ));
    }
    let (used, last) = sh_page_stream_tail(page)?;
    let n = sh_page_n_fks(page)? as usize;
    let delta = match last {
        None => fk.0,
        Some(prev) => {
            if fk.0 <= prev.0 {
                return Err(StoreError::Corrupt(
                    "invariant: scripthash page append create_fk not strictly increasing",
                ));
            }
            fk.0 - prev.0
        }
    };
    let add = uleb128_len(delta);
    if used + add > sh_page_stream_cap(page) {
        return Ok(false);
    }
    let mut tmp = [0u8; 10];
    let wrote = write_uleb128_into(&mut tmp, delta)?;
    debug_assert_eq!(wrote, add);
    let off = sh_page_stream_off(page) + used;
    page[off..off + wrote].copy_from_slice(&tmp[..wrote]);
    if page[SH_PAGE_OFF_VER] != SH_PAGE_EXTENT_VER {
        page[SH_PAGE_OFF_VER] = SH_PAGE_DELTA_VER;
    }
    sh_page_set_n_fks(page, (n + 1) as u16);
    Ok(true)
}

/// Decode next-off and append page FKs onto `out` (rejects len ≠ 4096).
pub fn sh_page_decode_slice_into(buf: &[u8], out: &mut Vec<Fk>) -> Result<u64, StoreError> {
    let page = sh_page_as_array(buf)?;
    let next = sh_page_next(page)?;
    sh_page_entries_into(page, out)?;
    Ok(next)
}

/// Decode page fields from an arbitrary slice (rejects len ≠ 4096).
#[cfg(debug_assertions)]
pub fn sh_page_decode_slice(buf: &[u8]) -> Result<(u64, Vec<Fk>), StoreError> {
    let page = sh_page_as_array(buf)?;
    Ok((sh_page_next(page)?, sh_page_entries(page)?))
}
