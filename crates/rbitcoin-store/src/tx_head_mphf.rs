//! Sealed `tx.head` shard: value-assigned BDZ MPHF (one candidate).
//!
//! `base.mphf` is BDZ2: `index(key) = newest_rel - 1`. Optional `base.mlt`
//! holds extra older rels for BIP30. A miss is fuse-gated by the caller.

use crate::bdz::BdzMphf;
use crate::error::StoreError;
use rbitcoin_primitives::{le_u16, le_u32};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct TxHeadMphf {
    pub mphf: BdzMphf,
    pub mlt: HashMap<u32, Vec<u32>>,
}

pub struct AssignedKeys {
    pub keys: Vec<u64>,
    pub values: Vec<u32>,
    pub mlt: HashMap<u32, Vec<u32>>,
    pub modulus: u32,
}

/// Unique mixed keys; highest rel per key is the MPHF value, older rels in `mlt`.
pub fn group_assigned_pairs(pairs: &mut [(u64, u32)]) -> Result<AssignedKeys, StoreError> {
    let mut modulus = 0u32;
    for &(_, rel) in pairs.iter() {
        if rel == 0 {
            return Err(StoreError::Corrupt("tx.head mphf: rel 0"));
        }
        modulus = modulus.max(rel);
    }
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut mlt = HashMap::new();
    let mut i = 0usize;
    while i < pairs.len() {
        let k = pairs[i].0;
        let mut j = i + 1;
        while j < pairs.len() && pairs[j].0 == k {
            j += 1;
        }
        let newest = pairs[j - 1].1;
        let slot = newest - 1;
        keys.push(k);
        values.push(slot);
        if j - i > 1 {
            let mut extra: Vec<u32> = pairs[i..j - 1].iter().map(|p| p.1).collect();
            extra.reverse();
            mlt.insert(slot, extra);
        }
        i = j;
    }
    keys.shrink_to_fit();
    values.shrink_to_fit();
    Ok(AssignedKeys {
        keys,
        values,
        mlt,
        modulus,
    })
}

pub fn mphf_path(base: &Path) -> PathBuf {
    sidecar(base, ".mphf")
}

pub fn rel_path(base: &Path) -> PathBuf {
    sidecar(base, ".rel")
}

#[doc(hidden)]
pub fn mlt_path(base: &Path) -> PathBuf {
    sidecar(base, ".mlt")
}

#[doc(hidden)]
pub fn sidecar(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(ext);
    PathBuf::from(s)
}

impl TxHeadMphf {
    pub fn exists(base: &Path) -> bool {
        mphf_path(base).is_file()
    }

    #[cfg(debug_assertions)]
    pub fn write(base: impl AsRef<Path>, pairs: &[(u64, u32)]) -> Result<Self, StoreError> {
        let mut owned = pairs.to_vec();
        let grouped = group_assigned_pairs(&mut owned)?;
        drop(owned);
        Self::write_grouped(base, grouped)
    }

    pub fn write_grouped(
        base: impl AsRef<Path>,
        grouped: AssignedKeys,
    ) -> Result<Self, StoreError> {
        let base = base.as_ref();
        if let Some(parent) = base.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mphf = BdzMphf::build_assigned(&grouped.keys, &grouped.values, grouped.modulus)?;
        mphf.write_packed_to(&mphf_path(base))?;
        drop(mphf);
        let rp = rel_path(base);
        if rp.is_file() {
            let _ = std::fs::remove_file(&rp);
        }
        write_mlt(&mlt_path(base), &grouped.mlt)?;
        Self::open(base)
    }

    pub fn open(base: impl AsRef<Path>) -> Result<Self, StoreError> {
        let base = base.as_ref().to_path_buf();
        let mp = mphf_path(&base);
        let mphf = BdzMphf::read_packed_from(&mp)?;
        let rp = rel_path(&base);
        if rp.is_file() {
            eprintln!("store: dropping leftover tx.head .rel (schema 20 MPHF is value-assigned)");
            let _ = std::fs::remove_file(&rp);
        }
        let mlt = read_mlt(&mlt_path(&base))?;
        Ok(Self { mphf, mlt })
    }

    #[cfg(debug_assertions)]
    pub fn slots_for(&self, mixed_u64: &[u64]) -> Result<Vec<u32>, StoreError> {
        self.slots_for_ctx(mixed_u64, &mut crate::IoCtx::none())
    }

    pub fn slots_for_ctx(
        &self,
        mixed_u64: &[u64],
        ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<u32>, StoreError> {
        self.mphf.index_batch(mixed_u64, ctx)
    }

    pub fn g_bytes_resident(&self) -> u64 {
        self.mphf.g_bytes_resident() as u64
    }

    pub fn read_rels_batch(
        &self,
        slots: &[u32],
        _ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<Vec<u32>>, StoreError> {
        let mut out = vec![Vec::new(); slots.len()];
        for (i, &slot) in slots.iter().enumerate() {
            out[i].push(slot + 1);
            if let Some(extra) = self.mlt.get(&slot) {
                out[i].extend_from_slice(extra);
            }
        }
        Ok(out)
    }
}

#[doc(hidden)]
pub fn write_mlt(path: &Path, mlt: &HashMap<u32, Vec<u32>>) -> Result<(), StoreError> {
    if mlt.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(&(mlt.len() as u32).to_le_bytes());
    for (&slot, rels) in mlt {
        buf.extend_from_slice(&slot.to_le_bytes());
        buf.extend_from_slice(&(rels.len() as u16).to_le_bytes());
        for r in rels {
            buf.extend_from_slice(&r.to_le_bytes());
        }
    }
    std::fs::write(path, &buf).map_err(|e| StoreError::io(path, e))?;
    Ok(())
}

#[doc(hidden)]
pub fn read_mlt(path: &Path) -> Result<HashMap<u32, Vec<u32>>, StoreError> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let buf = std::fs::read(path).map_err(|e| StoreError::io(path, e))?;
    let Some(count) = buf.first_chunk::<4>() else {
        return Err(StoreError::Corrupt("tx.head mlt short"));
    };
    let n = le_u32!(count, 0) as usize;
    let mut rest = &buf[4..];
    let mut out = HashMap::new();
    for _ in 0..n {
        let Some((head, next)) = rest.split_first_chunk::<6>() else {
            return Err(StoreError::Corrupt("tx.head mlt truncated"));
        };
        let slot = le_u32!(head, 0);
        let nrel = le_u16!(head, 4) as usize;
        rest = next;
        let rel_len = nrel * 4;
        if rest.len() < rel_len {
            return Err(StoreError::Corrupt("tx.head mlt rels"));
        }
        let (rels_buf, next) = rest.split_at(rel_len);
        rest = next;
        let (cells, _) = rels_buf.as_chunks::<4>();
        let rels = cells.iter().map(|cell| le_u32!(cell, 0)).collect();
        out.insert(slot, rels);
    }
    Ok(out)
}
