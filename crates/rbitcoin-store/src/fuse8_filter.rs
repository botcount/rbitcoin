//! Binary fuse8 membership for sealed `tx.head` / SH overflow segments.
//!
//! ~9 bits/key, no false negatives, FP ≈ 2⁻⁸. Built once at seal from mixed
//! probe keys (as u64). Open segments have no filter.
//!
//! On-disk: `BF8R` | u32 version LE | u64 body_len LE | body
//!
//! | Version | Body |
//! |--------:|------|
//! | **1** | Historical: bincode of xorf `BinaryFuse8` (pre in-tree port) — **not** decoded |
//! | **2** | Explicit LE: seed u64 · seg_len u32 · seg_mask u32 · seg_count_len u32 · fp_len u64 · fps |
//!
//! Opening a v1 file **refuses** the store. Wipe `store/tx.head` (and
//! `store/scripthash*` if SH overflow fuses are v1); Class A is kept.
//!
//! `build` owns fingerprints on the heap. After `write_then_map` /
//! `read_from`, fingerprints are a read-only file map (`fuse8=` heap 0).

use rbitcoin_primitives::{le_u32, le_u64};

use crate::binary_fuse8::{BinaryFuse8, Fingerprints};
use crate::error::StoreError;
use crate::readonly_map::ReadonlyMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[doc(hidden)]
pub const MAGIC: &[u8; 4] = b"BF8R";
/// Historical: bincode of xorf BinaryFuse8. Do not decode with the v2 algorithm.
pub const VERSION_V1: u32 = 1;
/// Explicit LE body (in-tree BinaryFuse8).
pub const VERSION_V2: u32 = 2;

/// One-line operator refuse for leftover fuse8 v1.
pub const INDEX_REFUSE_FUSE8_V1: &str = "index refuses fuse8 v1; wipe store/tx.head and store/scripthash* then restart (Class A kept; tx.head rebuilds, SH rematerializes with --sh-index)";

/// On-disk / in-memory sealed fuse for one head segment.
#[derive(Debug)]
pub struct SealedFuse8 {
    /// Empty segments store dummy key 0 so the filter is always present.
    pub filter: BinaryFuse8,
}

impl SealedFuse8 {
    /// Build from distinct mixed-key u64s (caller must dedupe).
    pub fn build(keys: &[u64]) -> Result<Self, StoreError> {
        if keys.is_empty() {
            // Empty sealed segment: dummy key so construction succeeds.
            return Ok(Self {
                filter: BinaryFuse8::try_from_keys(&[0u64])
                    .map_err(|_| StoreError::Corrupt("binary fuse8 empty build"))?,
            });
        }
        let filter = BinaryFuse8::try_from_keys(keys)
            .map_err(|_| StoreError::Corrupt("binary fuse8 build failed (dup keys?)"))?;
        Ok(Self { filter })
    }

    /// Membership test (no FN for keys passed to [`Self::build`]).
    #[inline]
    pub fn contains(&self, key: u64) -> bool {
        self.filter.contains(key)
    }

    /// Fingerprint array length (bytes).
    pub fn fingerprint_bytes(&self) -> usize {
        self.filter.len()
    }

    /// Heap-owned fingerprint bytes (0 when mapped from the sealed file).
    pub fn fingerprint_heap_bytes(&self) -> usize {
        self.filter.fingerprint_heap_bytes()
    }

    /// Write current (v2) layout.
    pub fn write_to(&self, path: &Path) -> Result<(), StoreError> {
        let body = encode_body(&self.filter);
        let mut f = File::create(path).map_err(|e| StoreError::io(path, e))?;
        f.write_all(MAGIC).map_err(|e| StoreError::io(path, e))?;
        f.write_all(&VERSION_V2.to_le_bytes())
            .map_err(|e| StoreError::io(path, e))?;
        f.write_all(&(body.len() as u64).to_le_bytes())
            .map_err(|e| StoreError::io(path, e))?;
        f.write_all(&body).map_err(|e| StoreError::io(path, e))?;
        f.sync_all().map_err(|e| StoreError::io(path, e))?;
        Ok(())
    }

    /// Write v2 then reopen as a file map (drop the build heap).
    pub fn write_then_map(self, path: &Path) -> Result<Self, StoreError> {
        self.write_to(path)?;
        Self::read_from(path)
    }

    pub fn read_from(path: &Path) -> Result<Self, StoreError> {
        open_file(path)
    }
}

/// Open a BF8R fuse file. v1 and unreadable v2 refuse (no always-probe).
pub fn open_file(path: &Path) -> Result<SealedFuse8, StoreError> {
    let mut map = ReadonlyMap::map_path(path)?;
    let geo = {
        let bytes = map.as_file_bytes();
        let Some(head) = bytes.first_chunk::<16>() else {
            return Err(StoreError::Corrupt("tx.head fuse magic"));
        };
        if &head[0..4] != MAGIC {
            return Err(StoreError::Corrupt("tx.head fuse magic"));
        }
        let ver = le_u32!(head, 4);
        let len = le_u64!(head, 8);
        let Some(end) = 16u64.checked_add(len) else {
            return Err(StoreError::Corrupt("fuse8 v2 body unreadable"));
        };
        let Ok(end) = usize::try_from(end) else {
            return Err(StoreError::Corrupt("fuse8 v2 body unreadable"));
        };
        if bytes.len() < end {
            return Err(StoreError::Corrupt("fuse8 v2 body unreadable"));
        }
        match ver {
            VERSION_V1 => return Err(StoreError::Corrupt(INDEX_REFUSE_FUSE8_V1)),
            VERSION_V2 => {
                let payload = &bytes[16..end];
                parse_v2_geometry(payload)
                    .map_err(|_| StoreError::Corrupt("fuse8 v2 body unreadable"))?
            }
            _ => return Err(StoreError::Corrupt("tx.head fuse version")),
        }
    };
    map.set_fingerprint_range(16 + geo.fp_off, geo.fp_len)?;
    Ok(SealedFuse8 {
        filter: BinaryFuse8 {
            seed: geo.seed,
            segment_length: geo.segment_length,
            segment_length_mask: geo.segment_length_mask,
            segment_count_length: geo.segment_count_length,
            fingerprints: Fingerprints::Map(map),
        },
    })
}

#[doc(hidden)]
pub struct V2Geometry {
    pub seed: u64,
    pub segment_length: u32,
    pub segment_length_mask: u32,
    pub segment_count_length: u32,
    pub fp_off: usize,
    pub fp_len: usize,
}

#[doc(hidden)]
pub fn parse_v2_geometry(payload: &[u8]) -> Result<V2Geometry, StoreError> {
    let Some(head) = payload.first_chunk::<28>() else {
        return Err(StoreError::Corrupt("fuse8 body short"));
    };
    let seed = le_u64!(head, 0);
    let segment_length = le_u32!(head, 8);
    let segment_length_mask = le_u32!(head, 12);
    let segment_count_length = le_u32!(head, 16);
    let fp_len = le_u64!(head, 20) as usize;
    let o = 28usize;
    if fp_len > 512 * 1024 * 1024 {
        return Err(StoreError::Corrupt("fuse8 fingerprints too large"));
    }
    if payload.len() < o + fp_len {
        return Err(StoreError::Corrupt("fuse8 fingerprints truncated"));
    }
    if segment_length == 0 || segment_length_mask != segment_length.saturating_sub(1) {
        return Err(StoreError::Corrupt("fuse8 segment_length invalid"));
    }
    // Binary Fuse8 indexes with a bit mask. A non-power-of-two length aliases slots.
    if !segment_length.is_power_of_two() {
        return Err(StoreError::Corrupt(
            "fuse8 segment length must be a power of two",
        ));
    }
    if segment_count_length < segment_length || !segment_count_length.is_multiple_of(segment_length)
    {
        return Err(StoreError::Corrupt(
            "fuse8 segment count length must be a multiple of the segment length",
        ));
    }
    let min_fp = (segment_count_length as u64).saturating_add(2 * u64::from(segment_length));
    if (fp_len as u64) < min_fp {
        return Err(StoreError::Corrupt(
            "fuse8 fingerprints shorter than hash geometry",
        ));
    }
    Ok(V2Geometry {
        seed,
        segment_length,
        segment_length_mask,
        segment_count_length,
        fp_off: o,
        fp_len,
    })
}

#[doc(hidden)]
pub fn encode_body(filter: &BinaryFuse8) -> Vec<u8> {
    let fp = filter.fingerprints.as_slice();
    let mut body = Vec::with_capacity(8 + 4 + 4 + 4 + 8 + fp.len());
    body.extend_from_slice(&filter.seed.to_le_bytes());
    body.extend_from_slice(&filter.segment_length.to_le_bytes());
    body.extend_from_slice(&filter.segment_length_mask.to_le_bytes());
    body.extend_from_slice(&filter.segment_count_length.to_le_bytes());
    body.extend_from_slice(&(fp.len() as u64).to_le_bytes());
    body.extend_from_slice(fp);
    body
}

/// Fold a 32-byte mixed head key into a u64 fuse key (stable, keyed via mix).
#[inline]
pub fn fuse_key_from_mixed(mixed: &[u8; 32]) -> u64 {
    le_u64!(mixed, 0) ^ le_u64!(mixed, 8) ^ le_u64!(mixed, 16) ^ le_u64!(mixed, 24)
}
