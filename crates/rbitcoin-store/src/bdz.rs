//! In-tree BDZ 3-graph MPHF (`u64` keys → `[0, n)`).
//!
//! A key **not** in the set still maps into `[0, n)`. Callers verify identity.

use crate::error::StoreError;
use crate::io_handle::IoHandle;
use crate::readonly_map::ReadonlyMap;
use rbitcoin_primitives::{le_u32, le_u64};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[doc(hidden)]
pub const MAGIC2: &[u8; 4] = b"BDZ2";
#[doc(hidden)]
pub const MAGIC3: &[u8; 4] = b"BDZ3";
#[doc(hidden)]
pub const VERSION: u32 = 1;
#[doc(hidden)]
pub const GAMMA_NUM: u64 = 123;
#[doc(hidden)]
pub const GAMMA_DEN: u64 = 100;
#[doc(hidden)]
pub const MAX_SEED: u32 = 256;
#[doc(hidden)]
pub const HEADER_LEN2: u64 = 32;
#[doc(hidden)]
pub const HEADER_LEN3: u64 = 32;
#[doc(hidden)]
pub const G_PAGE_BYTES: usize = 4096;
#[doc(hidden)]
pub const G_BITS_WORDS: u32 = 32;
#[doc(hidden)]
pub const COMPACT_G_BITS: u32 = 2;
#[doc(hidden)]
pub const RANK_SUPER_BITS: u32 = 512;

#[derive(Debug)]
#[doc(hidden)]
pub enum GStore {
    Ram(Box<[u32]>),
    Fd {
        file: File,
        path: PathBuf,
        off: u64,
        n_bytes: u64,
        g_bits: u32,
        page_preads: AtomicU64,
    },
}

#[doc(hidden)]
pub enum OccBits {
    Words(Box<[u64]>),
    Map {
        map: ReadonlyMap,
        off: usize,
        len: usize,
    },
}

impl std::fmt::Debug for OccBits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Words(w) => f.debug_tuple("Words").field(&w.len()).finish(),
            Self::Map { off, len, .. } => f
                .debug_struct("Map")
                .field("off", off)
                .field("len", len)
                .finish(),
        }
    }
}

#[derive(Debug)]
#[doc(hidden)]
pub struct CompactRank {
    pub m: u32,
    pub occ: OccBits,
    pub supers: Box<[u32]>,
}

#[derive(Debug)]
pub struct BdzMphf {
    pub n: u32,
    pub m: u32,
    pub seed: u64,
    pub modulus: u32,
    pub g: GStore,
    pub compact: Option<CompactRank>,
}

#[inline]
#[doc(hidden)]
pub fn splitmix64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[inline]
#[doc(hidden)]
pub fn mix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

#[inline]
#[doc(hidden)]
pub fn hash3(key: u64, seed: u64, m: u32) -> [u32; 3] {
    let h = mix64(key.wrapping_add(seed));
    let m = m as u64;
    let a = ((h as u128 * m as u128) >> 64) as u32;
    let b = ((h.rotate_left(21) as u128 * m as u128) >> 64) as u32;
    let c = ((h.rotate_left(42) as u128 * m as u128) >> 64) as u32;
    let mut out = [a, b, c];
    if out[1] == out[0] {
        out[1] = (out[1] + 1) % (m as u32);
    }
    if out[2] == out[0] || out[2] == out[1] {
        out[2] = (out[2] + 1) % (m as u32);
        if out[2] == out[0] || out[2] == out[1] {
            out[2] = (out[2] + 1) % (m as u32);
        }
    }
    out
}

#[inline]
#[doc(hidden)]
pub fn hash3_partite(key: u64, seed: u64, m: u32) -> [u32; 3] {
    let part = m / 3;
    let h = mix64(key.wrapping_add(seed));
    let p = part as u64;
    let a = ((h as u128 * p as u128) >> 64) as u32;
    let b = ((h.rotate_left(21) as u128 * p as u128) >> 64) as u32;
    let c = ((h.rotate_left(42) as u128 * p as u128) >> 64) as u32;
    [a, part + b, 2 * part + c]
}

#[inline]
#[doc(hidden)]
pub fn hash_verts(key: u64, seed: u64, m: u32, partite: bool) -> [u32; 3] {
    if partite {
        hash3_partite(key, seed, m)
    } else {
        hash3(key, seed, m)
    }
}

impl BdzMphf {
    pub fn n(&self) -> u32 {
        self.n
    }

    #[cfg(debug_assertions)]
    pub fn modulus(&self) -> u32 {
        self.modulus
    }

    pub fn g_bytes_resident(&self) -> usize {
        match &self.g {
            GStore::Ram(g) => g.len() * 4,
            GStore::Fd { .. } => 0,
        }
    }

    pub fn occ_bytes_resident(&self) -> usize {
        let Some(rank) = &self.compact else {
            return 0;
        };
        let occ = match &rank.occ {
            OccBits::Words(w) => w.len() * 8,
            OccBits::Map { .. } => 0,
        };
        occ + rank.supers.len() * 4
    }

    #[cfg(debug_assertions)]
    pub fn vertices(&self, key: u64) -> [u32; 3] {
        if self.n <= 1 {
            return [0, 0, 0];
        }
        hash_verts(key, self.seed, self.m, self.compact.is_some())
    }

    pub fn index(&self, key: u64) -> Result<u32, StoreError> {
        Ok(self.index_batch(&[key], &mut crate::IoCtx::none())?[0])
    }

    #[cfg(debug_assertions)]
    pub fn take_g_page_preads(&self) -> u64 {
        match &self.g {
            GStore::Fd { page_preads, .. } => page_preads.swap(0, Ordering::Relaxed),
            GStore::Ram(_) => 0,
        }
    }

    pub fn index_batch(
        &self,
        keys: &[u64],
        ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<u32>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if self.n == 0 {
            return Ok(vec![0; keys.len()]);
        }
        if self.n == 1 {
            return Ok(vec![self.one_key_index()?; keys.len()]);
        }
        if self.modulus == 0 || self.m == 0 {
            return Err(StoreError::Corrupt("bdz mphf: zero modulus"));
        }
        match &self.g {
            GStore::Ram(g) => Ok(keys.iter().map(|&k| self.index_from_g(g, k)).collect()),
            GStore::Fd { .. } => self.index_batch_fd(keys, ctx),
        }
    }

    #[doc(hidden)]
    pub fn index_from_g(&self, g: &[u32], key: u64) -> u32 {
        let partite = self.compact.is_some();
        let [a, b, c] = hash_verts(key, self.seed, self.m, partite);
        let ga = g[a as usize];
        let gb = g[b as usize];
        let gc = g[c as usize];
        self.finish_index(ga, gb, gc, [a, b, c])
    }

    #[doc(hidden)]
    pub fn finish_index(&self, ga: u32, gb: u32, gc: u32, verts: [u32; 3]) -> u32 {
        if let Some(rank) = self.compact.as_ref() {
            let i = (ga.wrapping_add(gb).wrapping_add(gc) % 3) as usize;
            let v = verts[i];
            rank.rank1(v).min(self.n.saturating_sub(1))
        } else {
            ga.wrapping_add(gb).wrapping_add(gc) % self.modulus
        }
    }

    #[doc(hidden)]
    pub fn index_batch_fd(
        &self,
        keys: &[u64],
        ctx: &mut crate::IoCtx<'_>,
    ) -> Result<Vec<u32>, StoreError> {
        let GStore::Fd {
            file,
            path,
            off,
            n_bytes,
            g_bits,
            page_preads,
            ..
        } = &self.g
        else {
            return Err(StoreError::Corrupt("bdz mphf: fd batch"));
        };
        let g_bits = *g_bits;
        let partite = self.compact.is_some();
        let verts: Vec<[u32; 3]> = keys
            .iter()
            .map(|&k| hash_verts(k, self.seed, self.m, partite))
            .collect();
        let mut page_ids: Vec<u32> = verts
            .iter()
            .flat_map(|v| {
                v.iter()
                    .copied()
                    .flat_map(|vert| vertex_pages(vert, g_bits))
            })
            .collect();
        page_ids.sort_unstable();
        page_ids.dedup();
        let mut scatter = GPageScatter::from_verts(g_bits, &verts, &page_ids);
        stream_or_load_pages(
            file,
            path,
            *off,
            *n_bytes,
            &page_ids,
            page_preads,
            ctx,
            &mut |page, buf| scatter.fill(page, buf),
        )?;
        Ok(scatter
            .words
            .iter()
            .zip(verts.iter())
            .map(|([ga, gb, gc], verts)| self.finish_index(*ga, *gb, *gc, *verts))
            .collect())
    }

    #[doc(hidden)]
    pub fn one_key_index(&self) -> Result<u32, StoreError> {
        if self.compact.is_some() {
            return Ok(0);
        }
        match &self.g {
            GStore::Ram(g) => Ok(g.first().copied().unwrap_or(0)),
            GStore::Fd {
                file,
                path,
                off,
                g_bits,
                ..
            } => {
                if *g_bits == G_BITS_WORDS {
                    let mut buf = [0u8; 4];
                    pread_exact(file, path, *off, &mut buf)?;
                    return Ok(u32::from_le_bytes(buf));
                }
                let nbytes = (*g_bits as usize).div_ceil(8);
                let mut buf = vec![0u8; nbytes.max(1)];
                pread_exact(file, path, *off, &mut buf)?;
                Ok(unpack_g_at(&buf, 0, *g_bits))
            }
        }
    }

    #[cfg(debug_assertions)]
    pub fn build(keys: &[u64]) -> Result<Self, StoreError> {
        let n = keys.len() as u32;
        let ranks: Vec<u32> = (0..n).collect();
        Self::build_from_ranks(keys, &ranks, n)
    }

    pub fn build_assigned(keys: &[u64], values: &[u32], modulus: u32) -> Result<Self, StoreError> {
        if keys.len() != values.len() {
            return Err(StoreError::Corrupt("bdz mphf: assigned len"));
        }
        if keys.is_empty() {
            return Self::build_from_ranks(keys, values, modulus);
        }
        if modulus == 0 {
            return Err(StoreError::Corrupt("bdz mphf: assigned modulus"));
        }
        let mut seen = vec![false; modulus as usize];
        for &v in values {
            if v >= modulus {
                return Err(StoreError::Corrupt("bdz mphf: assigned value"));
            }
            if seen[v as usize] {
                return Err(StoreError::Corrupt("bdz mphf: assigned duplicate"));
            }
            seen[v as usize] = true;
        }
        Self::build_from_ranks(keys, values, modulus)
    }

    #[doc(hidden)]
    pub fn build_from_ranks(keys: &[u64], ranks: &[u32], modulus: u32) -> Result<Self, StoreError> {
        let n = keys.len() as u32;
        if n == 0 {
            return Ok(Self {
                n: 0,
                m: 0,
                seed: 0,
                modulus,
                g: GStore::Ram(Box::new([])),
                compact: None,
            });
        }
        if n == 1 {
            return Ok(Self {
                n: 1,
                m: 1,
                seed: 1,
                modulus,
                g: GStore::Ram(vec![ranks[0]].into_boxed_slice()),
                compact: None,
            });
        }
        let m = (u64::from(n) * GAMMA_NUM)
            .div_ceil(GAMMA_DEN)
            .max(u64::from(n) + 3)
            .max(3) as u32;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut scratch = PeelScratch::default();
        for _try in 0..MAX_SEED {
            let seed = splitmix64(&mut rng);
            scratch.prepare(m as usize, keys.len());
            if peel_order(keys, seed, m, &mut scratch, false) {
                let order = std::mem::take(&mut scratch.order);
                drop(scratch);
                let g = assign_g(keys, ranks, seed, m, modulus, &order);
                return Ok(Self {
                    n,
                    m,
                    seed,
                    modulus,
                    g: GStore::Ram(g.into_boxed_slice()),
                    compact: None,
                });
            }
        }
        Err(StoreError::Corrupt("bdz mphf: graph did not peel"))
    }

    pub fn write_packed_to(&self, path: &Path) -> Result<(), StoreError> {
        let GStore::Ram(g) = &self.g else {
            return Err(StoreError::Corrupt("bdz mphf: write requires RAM g"));
        };
        let g_bits = g_bits_for_modulus(self.modulus);
        crate::file::write_synced_tmp_file(path, |f| {
            let mut hdr = [0u8; HEADER_LEN2 as usize];
            hdr[0..4].copy_from_slice(MAGIC2);
            hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
            hdr[8..12].copy_from_slice(&self.n.to_le_bytes());
            hdr[12..16].copy_from_slice(&self.m.to_le_bytes());
            hdr[16..24].copy_from_slice(&self.seed.to_le_bytes());
            hdr[24..28].copy_from_slice(&self.modulus.to_le_bytes());
            hdr[28..32].copy_from_slice(&g_bits.to_le_bytes());
            f.write_all(&hdr).map_err(|e| StoreError::io(path, e))?;
            pack_g_write(g, g_bits, f).map_err(|e| StoreError::io(path, e))?;
            Ok(())
        })
    }

    pub fn read_packed_from(path: &Path) -> Result<Self, StoreError> {
        let file = File::open(path).map_err(|e| StoreError::io(path, e))?;
        let mut hdr = [0u8; HEADER_LEN2 as usize];
        pread_exact(&file, path, 0, &mut hdr)?;
        if &hdr[0..4] != MAGIC2 {
            return Err(StoreError::Corrupt("bdz mphf: bad magic"));
        }
        let ver = le_u32!(hdr, 4);
        if ver != VERSION {
            return Err(StoreError::Corrupt("bdz mphf: bad version"));
        }
        let n = le_u32!(hdr, 8);
        let m = le_u32!(hdr, 12);
        let seed = le_u64!(hdr, 16);
        let modulus = le_u32!(hdr, 24);
        let g_bits = le_u32!(hdr, 28);
        if g_bits == 0 || g_bits > 32 {
            return Err(StoreError::Corrupt("bdz mphf: g_bits"));
        }
        if n == 0 {
            return Ok(Self {
                n: 0,
                m: 0,
                seed: 0,
                modulus,
                g: GStore::Ram(Box::new([])),
                compact: None,
            });
        }
        if n > 1 && (m == 0 || modulus == 0) {
            return Err(StoreError::Corrupt("bdz mphf: zero modulus"));
        }
        let n_verts = if n == 1 { 1 } else { m };
        let n_bytes = packed_g_bytes(n_verts, g_bits);
        let meta = file.metadata().map_err(|e| StoreError::io(path, e))?;
        if meta.len() < HEADER_LEN2 + n_bytes {
            return Err(StoreError::Corrupt("bdz mphf: g length"));
        }
        Ok(Self {
            n,
            m: n_verts,
            seed,
            modulus,
            g: GStore::Fd {
                file,
                path: path.to_path_buf(),
                off: HEADER_LEN2,
                n_bytes,
                g_bits,
                page_preads: AtomicU64::new(0),
            },
            compact: None,
        })
    }

    pub fn build_compact(keys: &[u64]) -> Result<Self, StoreError> {
        let n = keys.len() as u32;
        if n == 0 {
            return Ok(Self {
                n: 0,
                m: 0,
                seed: 0,
                modulus: 0,
                g: GStore::Ram(Box::new([])),
                compact: Some(CompactRank::empty()),
            });
        }
        if n == 1 {
            let mut occ = vec![0u64; 1];
            occ_set(&mut occ, 0);
            return Ok(Self {
                n: 1,
                m: 1,
                seed: 1,
                modulus: 1,
                g: GStore::Ram(vec![0].into_boxed_slice()),
                compact: Some(CompactRank::from_occ(occ.into_boxed_slice(), 1)),
            });
        }
        let m = compact_vertex_count(n);
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut scratch = PeelScratch::default();
        for _try in 0..MAX_SEED {
            let seed = splitmix64(&mut rng);
            scratch.prepare(m as usize, keys.len());
            if peel_order(keys, seed, m, &mut scratch, true) {
                let order = std::mem::take(&mut scratch.order);
                drop(scratch);
                let (g, occ) = assign_compact(keys, seed, m, &order);
                return Ok(Self {
                    n,
                    m,
                    seed,
                    modulus: n,
                    g: GStore::Ram(g.into_boxed_slice()),
                    compact: Some(CompactRank::from_occ(occ, m)),
                });
            }
        }
        Err(StoreError::Corrupt("bdz mphf: graph did not peel"))
    }

    pub fn write_compact_to(&self, path: &Path) -> Result<(), StoreError> {
        let GStore::Ram(g) = &self.g else {
            return Err(StoreError::Corrupt("bdz mphf: write requires RAM g"));
        };
        let Some(rank) = self.compact.as_ref() else {
            return Err(StoreError::Corrupt("bdz mphf: compact write"));
        };
        crate::file::write_synced_tmp_file(path, |f| {
            let mut hdr = [0u8; HEADER_LEN3 as usize];
            hdr[0..4].copy_from_slice(MAGIC3);
            hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
            hdr[8..12].copy_from_slice(&self.n.to_le_bytes());
            hdr[12..16].copy_from_slice(&self.m.to_le_bytes());
            hdr[16..24].copy_from_slice(&self.seed.to_le_bytes());
            hdr[24..28].copy_from_slice(&0u32.to_le_bytes());
            hdr[28..32].copy_from_slice(&COMPACT_G_BITS.to_le_bytes());
            f.write_all(&hdr).map_err(|e| StoreError::io(path, e))?;
            if self.n > 0 {
                pack_g_write(g, COMPACT_G_BITS, f).map_err(|e| StoreError::io(path, e))?;
                let OccBits::Words(occ) = &rank.occ else {
                    return Err(StoreError::Corrupt("bdz mphf: write requires RAM occ"));
                };
                write_occ(occ, rank.m, f).map_err(|e| StoreError::io(path, e))?;
            }
            Ok(())
        })
    }

    pub fn read_compact_from(path: &Path) -> Result<Self, StoreError> {
        let file = File::open(path).map_err(|e| StoreError::io(path, e))?;
        let meta = file.metadata().map_err(|e| StoreError::io(path, e))?;
        if meta.len() < HEADER_LEN3 {
            return Err(StoreError::Corrupt("bdz mphf: g length"));
        }
        let mut hdr = [0u8; HEADER_LEN3 as usize];
        pread_exact(&file, path, 0, &mut hdr)?;
        if &hdr[0..4] != MAGIC3 {
            return Err(StoreError::Corrupt("bdz mphf: bad magic"));
        }
        let ver = le_u32!(hdr, 4);
        if ver != VERSION {
            return Err(StoreError::Corrupt("bdz mphf: bad version"));
        }
        let n = le_u32!(hdr, 8);
        let m = le_u32!(hdr, 12);
        let seed = le_u64!(hdr, 16);
        let g_bits = le_u32!(hdr, 28);
        if g_bits != COMPACT_G_BITS {
            return Err(StoreError::Corrupt("bdz mphf: g_bits"));
        }
        if n == 0 {
            return Ok(Self {
                n: 0,
                m: 0,
                seed: 0,
                modulus: 0,
                g: GStore::Ram(Box::new([])),
                compact: Some(CompactRank::empty()),
            });
        }
        let n_verts = if n == 1 { 1 } else { m };
        let g_bytes = packed_g_bytes(n_verts, COMPACT_G_BITS);
        let occ_n = occ_packed_bytes(n_verts) as usize;
        if meta.len() < HEADER_LEN3 + g_bytes + occ_n as u64 {
            return Err(StoreError::Corrupt("bdz mphf: g length"));
        }
        let prefix = (HEADER_LEN3 + g_bytes + occ_n as u64) as usize;
        let map = ReadonlyMap::map_prefix(path, prefix)?;
        let occ_off = HEADER_LEN3 as usize + g_bytes as usize;
        Ok(Self {
            n,
            m: n_verts,
            seed,
            modulus: n,
            g: GStore::Fd {
                file,
                path: path.to_path_buf(),
                off: HEADER_LEN3,
                n_bytes: g_bytes,
                g_bits: COMPACT_G_BITS,
                page_preads: AtomicU64::new(0),
            },
            compact: Some(CompactRank::from_mapped(map, occ_off, occ_n, n_verts)?),
        })
    }

    pub fn trailer_off(&self) -> u64 {
        if self.n == 0 {
            return HEADER_LEN3;
        }
        HEADER_LEN3 + packed_g_bytes(self.m, COMPACT_G_BITS) + occ_packed_bytes(self.m)
    }
}

#[doc(hidden)]
pub fn pread_exact(
    file: &File,
    path: &Path,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), StoreError> {
    let h = IoHandle::from_file(file);
    let mut done = 0usize;
    while done < buf.len() {
        let n = h.pread(offset + done as u64, &mut buf[done..]);
        if n < 0 {
            return Err(StoreError::io(path, std::io::Error::from_raw_os_error(-n)));
        }
        if n == 0 {
            return Err(StoreError::Corrupt("bdz mphf: short pread"));
        }
        done += n as usize;
    }
    Ok(())
}

#[doc(hidden)]
pub fn g_page_need(n_bytes: u64, page: u32) -> usize {
    let page_base = u64::from(page) * G_PAGE_BYTES as u64;
    if page_base >= n_bytes {
        return 0;
    }
    ((n_bytes - page_base) as usize).min(G_PAGE_BYTES)
}

#[doc(hidden)]
pub fn load_g_page(
    file: &File,
    path: &Path,
    g_off: u64,
    n_bytes: u64,
    page: u32,
    buf: &mut [u8; G_PAGE_BYTES],
) -> Result<usize, StoreError> {
    let need = g_page_need(n_bytes, page);
    if need == 0 {
        return Ok(0);
    }
    pread_exact(
        file,
        path,
        g_off + u64::from(page) * G_PAGE_BYTES as u64,
        &mut buf[..need],
    )?;
    Ok(need)
}

#[allow(clippy::too_many_arguments)] // IO/session args stay unbundled
#[doc(hidden)]
pub fn stream_or_load_pages(
    file: &File,
    path: &Path,
    off: u64,
    n_bytes: u64,
    page_ids: &[u32],
    page_preads: &AtomicU64,
    ctx: &mut crate::IoCtx<'_>,
    fill: &mut impl FnMut(u32, &[u8]),
) -> Result<(), StoreError> {
    match ctx.session() {
        Some(session) => stream_g_pages(
            session,
            file,
            path,
            off,
            n_bytes,
            page_ids,
            page_preads,
            fill,
        ),
        None => {
            for &page in page_ids {
                let mut buf = [0u8; G_PAGE_BYTES];
                let n = load_g_page(file, path, off, n_bytes, page, &mut buf)?;
                page_preads.fetch_add(1, Ordering::Relaxed);
                fill(page, &buf[..n]);
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)] // IO/session args stay unbundled
#[doc(hidden)]
pub fn stream_g_pages(
    session: &mut crate::uring_session::UringSession,
    file: &File,
    path: &Path,
    g_off: u64,
    n_bytes: u64,
    page_ids: &[u32],
    page_preads: &AtomicU64,
    fill: &mut impl FnMut(u32, &[u8]),
) -> Result<(), StoreError> {
    let n_pages = page_ids.len();
    if n_pages == 0 {
        return Ok(());
    }
    let fd = IoHandle::from_file(file);
    let pool_n = (crate::uring_session::DEFAULT_ENTRIES as usize)
        .min(n_pages)
        .max(1);
    let mut bufs: Vec<[u8; G_PAGE_BYTES]> = vec![[0u8; G_PAGE_BYTES]; pool_n];
    let mut slot_page: Vec<Option<usize>> = vec![None; pool_n];
    let mut free_slots: Vec<usize> = (0..pool_n).collect();
    let mut next_page = 0usize;
    let mut in_flight = 0usize;
    session.begin_batch()?;
    let epoch = session.epoch();
    let run = (|| -> Result<(), StoreError> {
        loop {
            while next_page < n_pages
                && !free_slots.is_empty()
                && session.free_sq() > 0
                && in_flight < pool_n
            {
                let Some(slot) = free_slots.pop() else {
                    break;
                };
                let pi = next_page;
                next_page += 1;
                let page = page_ids[pi];
                let need = g_page_need(n_bytes, page);
                if need == 0 {
                    free_slots.push(slot);
                    continue;
                }
                let off = g_off + u64::from(page) * G_PAGE_BYTES as u64;
                let buf = &mut bufs[slot][..need];
                buf.fill(0);
                let ud = crate::uring_session::pack_ud(
                    crate::uring_session::KIND_MPHF_G,
                    epoch,
                    slot as u32,
                );
                // SAFETY: `bufs[slot]` lives until this g-page wave drains.
                unsafe { session.push_pread(fd, off, buf, ud) }?;
                slot_page[slot] = Some(pi);
                in_flight += 1;
            }
            session.sync_submission();
            let _ = session.submit();
            if in_flight == 0 {
                break;
            }
            let cqes = session.harvest_ready()?;
            if cqes.is_empty() {
                session.submit_and_wait_one()?;
                continue;
            }
            for (ud, res) in cqes {
                let (kind, ep, slot) = crate::uring_session::unpack_ud(ud);
                let slot = slot as usize;
                if kind != crate::uring_session::KIND_MPHF_G || ep != epoch || slot >= pool_n {
                    session.poison();
                    return Err(StoreError::Corrupt("invariant: io_uring leftover cqe"));
                }
                let Some(pi) = slot_page[slot].take() else {
                    session.poison();
                    return Err(StoreError::Corrupt("invariant: io_uring leftover cqe"));
                };
                in_flight = in_flight.saturating_sub(1);
                let page = page_ids[pi];
                let need = g_page_need(n_bytes, page);
                if res < 0 {
                    return Err(StoreError::io(
                        path,
                        std::io::Error::from_raw_os_error(-res),
                    ));
                }
                let mut n = res as usize;
                if n < need {
                    load_g_page(file, path, g_off, n_bytes, page, &mut bufs[slot])?;
                    n = need;
                }
                n = n.min(need);
                page_preads.fetch_add(1, Ordering::Relaxed);
                fill(page, &bufs[slot][..n]);
                free_slots.push(slot);
            }
        }
        Ok(())
    })();
    session.drain_all()?;
    run
}

#[doc(hidden)]
pub fn g_bits_for_modulus(modulus: u32) -> u32 {
    if modulus <= 1 {
        return 1;
    }
    32 - (modulus - 1).leading_zeros()
}

#[doc(hidden)]
pub fn packed_g_bytes(n_verts: u32, g_bits: u32) -> u64 {
    let bits = u64::from(n_verts) * u64::from(g_bits);
    bits.div_ceil(8)
}

#[doc(hidden)]
pub fn pack_g_write<W: Write>(g: &[u32], g_bits: u32, w: &mut W) -> std::io::Result<()> {
    const PAGE: usize = 4096;
    let mask = if g_bits == 32 {
        u32::MAX
    } else {
        (1u32 << g_bits) - 1
    };
    let mut page = [0u8; PAGE];
    let mut n = 0usize;
    let mut acc = 0u64;
    let mut acc_bits = 0u32;
    for &val in g {
        acc |= u64::from(val & mask) << acc_bits;
        acc_bits += g_bits;
        while acc_bits >= 8 {
            page[n] = acc as u8;
            n += 1;
            acc >>= 8;
            acc_bits -= 8;
            if n == PAGE {
                w.write_all(&page)?;
                n = 0;
            }
        }
    }
    if acc_bits > 0 {
        page[n] = acc as u8;
        n += 1;
    }
    if n > 0 {
        w.write_all(&page[..n])?;
    }
    Ok(())
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn pack_g(g: &[u32], g_bits: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(packed_g_bytes(g.len() as u32, g_bits) as usize);
    pack_g_write(g, g_bits, &mut out).unwrap();
    out
}

#[doc(hidden)]
pub fn unpack_g_at(buf: &[u8], v: u32, g_bits: u32) -> u32 {
    let start = u64::from(v) * u64::from(g_bits);
    let byte = (start / 8) as usize;
    if byte >= buf.len() {
        return 0;
    }
    extract_g_window(&buf[byte..], None, 0, (start % 8) as u32, g_bits)
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn unpack_g_bits_from_page_window(
    page: &[u8],
    next: Option<&[u8]>,
    page_id: u32,
    v: u32,
    g_bits: u32,
) -> u32 {
    let start = u64::from(v) * u64::from(g_bits);
    let start_byte = start / 8;
    let page_base = u64::from(page_id) * G_PAGE_BYTES as u64;
    if start_byte < page_base {
        return 0;
    }
    extract_g_window(
        page,
        next,
        (start_byte - page_base) as usize,
        (start % 8) as u32,
        g_bits,
    )
}

#[doc(hidden)]
pub fn extract_g_window(
    page: &[u8],
    next: Option<&[u8]>,
    off: usize,
    rem: u32,
    g_bits: u32,
) -> u32 {
    let last = rem.saturating_add(g_bits.saturating_sub(1)) / 8;
    let need = (last as usize + 1).min(8);
    let mut acc = 0u64;
    for i in 0..need {
        let src = off + i;
        let b = if src < page.len() {
            page[src]
        } else {
            match next {
                Some(n) => n.get(src - page.len()).copied().unwrap_or(0),
                None => 0,
            }
        };
        acc |= u64::from(b) << (8 * i);
    }
    let mask = if g_bits >= 32 {
        u32::MAX
    } else {
        (1u32 << g_bits) - 1
    };
    ((acc >> rem) as u32) & mask
}

#[derive(Clone, Copy)]
#[doc(hidden)]
pub struct GExtract {
    pub ki: u32,
    pub slot: u8,
    pub off: u16,
    pub rem: u8,
    pub start_page: u32,
}

#[doc(hidden)]
pub fn g_extract(ki: u32, slot: u8, vert: u32, g_bits: u32) -> (u32, GExtract) {
    let start = u64::from(vert) * u64::from(g_bits);
    let start_byte = start / 8;
    let start_page = (start_byte / G_PAGE_BYTES as u64) as u32;
    let end = start + u64::from(g_bits.saturating_sub(1));
    let end_page = (end / 8 / G_PAGE_BYTES as u64) as u32;
    (
        end_page,
        GExtract {
            ki,
            slot,
            off: (start_byte % G_PAGE_BYTES as u64) as u16,
            rem: (start % 8) as u8,
            start_page,
        },
    )
}

#[doc(hidden)]
pub struct GPageScatter {
    pub g_bits: u32,
    pub bucket_of: Vec<u32>,
    pub buckets: Vec<Vec<GExtract>>,
    pub tails: Vec<Option<(u16, [u8; 8])>>,
    pub pending: Vec<Vec<(GExtract, [u8; 8])>>,
    pub words: Vec<[u32; 3]>,
}

impl GPageScatter {
    #[doc(hidden)]
    pub fn from_verts(g_bits: u32, verts: &[[u32; 3]], page_ids: &[u32]) -> Self {
        let max_p = page_ids.iter().copied().max().unwrap_or(0) as usize;
        let mut bucket_of = vec![u32::MAX; max_p + 1];
        for (i, &p) in page_ids.iter().enumerate() {
            bucket_of[p as usize] = i as u32;
        }
        let mut buckets = vec![Vec::new(); page_ids.len()];
        for (ki, v) in verts.iter().enumerate() {
            for (slot, &vert) in v.iter().enumerate() {
                let (end_page, e) = g_extract(ki as u32, slot as u8, vert, g_bits);
                let Some(&bi) = bucket_of.get(end_page as usize) else {
                    continue;
                };
                if bi == u32::MAX {
                    continue;
                }
                buckets[bi as usize].push(e);
            }
        }
        Self {
            g_bits,
            bucket_of,
            buckets,
            tails: vec![None; page_ids.len()],
            pending: vec![Vec::new(); page_ids.len()],
            words: vec![[0u32; 3]; verts.len()],
        }
    }

    #[doc(hidden)]
    pub fn put(&mut self, e: GExtract, val: u32) {
        self.words[e.ki as usize][e.slot as usize] = val;
    }

    #[doc(hidden)]
    pub fn fill(&mut self, page: u32, buf: &[u8]) {
        let Some(&bi32) = self.bucket_of.get(page as usize) else {
            return;
        };
        if bi32 == u32::MAX {
            return;
        }
        let bi = bi32 as usize;
        let mut tail = [0u8; 8];
        let n = buf.len().min(8);
        if n > 0 {
            tail[8 - n..].copy_from_slice(&buf[buf.len() - n..]);
        }
        self.tails[bi] = Some((buf.len() as u16, tail));
        let waiters = std::mem::take(&mut self.pending[bi]);
        for (e, prefix) in waiters {
            let val = extract_g_window(
                &tail,
                Some(&prefix),
                8 + e.off as usize - buf.len(),
                e.rem as u32,
                self.g_bits,
            );
            self.put(e, val);
        }
        let extracts = std::mem::take(&mut self.buckets[bi]);
        for e in extracts {
            if e.start_page == page {
                let val = extract_g_window(buf, None, e.off as usize, e.rem as u32, self.g_bits);
                self.put(e, val);
                continue;
            }
            let Some(&sbi32) = self.bucket_of.get(e.start_page as usize) else {
                continue;
            };
            if sbi32 == u32::MAX {
                continue;
            }
            let sbi = sbi32 as usize;
            if let Some((plen, head)) = self.tails[sbi] {
                let val = extract_g_window(
                    &head,
                    Some(buf),
                    8 + e.off as usize - plen as usize,
                    e.rem as u32,
                    self.g_bits,
                );
                self.put(e, val);
            } else {
                let mut prefix = [0u8; 8];
                let pn = buf.len().min(8);
                if pn > 0 {
                    prefix[..pn].copy_from_slice(&buf[..pn]);
                }
                self.pending[sbi].push((e, prefix));
            }
        }
    }
}

#[doc(hidden)]
pub fn vertex_pages(v: u32, g_bits: u32) -> impl Iterator<Item = u32> {
    let start = u64::from(v) * u64::from(g_bits);
    let end = start + u64::from(g_bits.saturating_sub(1));
    let p0 = (start / 8 / G_PAGE_BYTES as u64) as u32;
    let p1 = (end / 8 / G_PAGE_BYTES as u64) as u32;
    p0..=p1
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn vertex_straddles_page(v: u32, g_bits: u32) -> bool {
    let mut it = vertex_pages(v, g_bits);
    let a = it.next();
    let b = it.next();
    a.is_some() && b.is_some()
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn scatter_g_words(g_bits: u32, verts: &[[u32; 3]], pages: &[(u32, &[u8])]) -> Vec<[u32; 3]> {
    let mut page_ids: Vec<u32> = pages.iter().map(|&(p, _)| p).collect();
    page_ids.sort_unstable();
    page_ids.dedup();
    let mut scatter = GPageScatter::from_verts(g_bits, verts, &page_ids);
    for &(p, buf) in pages {
        scatter.fill(p, buf);
    }
    scatter.words
}

#[derive(Default)]
#[doc(hidden)]
pub struct PeelScratch {
    pub deg: Vec<u8>,
    pub xor_e: Vec<u32>,
    pub q: Vec<u32>,
    pub order: Vec<(u32, u32)>,
}

impl PeelScratch {
    #[doc(hidden)]
    pub fn prepare(&mut self, m: usize, n: usize) {
        self.deg.clear();
        self.deg.resize(m, 0);
        self.xor_e.clear();
        self.xor_e.resize(m, 0);
        self.q.clear();
        self.order.clear();
        self.order.reserve(n);
    }
}

#[doc(hidden)]
pub fn peel_order(
    keys: &[u64],
    seed: u64,
    m: u32,
    scratch: &mut PeelScratch,
    partite: bool,
) -> bool {
    let n = keys.len();
    for (i, &k) in keys.iter().enumerate() {
        let ei = i as u32;
        for v in hash_verts(k, seed, m, partite) {
            let d = &mut scratch.deg[v as usize];
            if *d == 255 {
                return false;
            }
            *d += 1;
            scratch.xor_e[v as usize] ^= ei;
        }
    }
    for (v, &d) in scratch.deg.iter().enumerate() {
        if d == 1 {
            scratch.q.push(v as u32);
        }
    }
    let mut qi = 0usize;
    while qi < scratch.q.len() {
        let v = scratch.q[qi];
        qi += 1;
        if scratch.deg[v as usize] != 1 {
            continue;
        }
        let ei = scratch.xor_e[v as usize];
        if ei as usize >= n {
            return false;
        }
        scratch.order.push((ei, v));
        for u in hash_verts(keys[ei as usize], seed, m, partite) {
            let d = &mut scratch.deg[u as usize];
            if *d == 0 {
                return false;
            }
            *d -= 1;
            scratch.xor_e[u as usize] ^= ei;
            if *d == 1 {
                scratch.q.push(u);
            }
        }
    }
    scratch.order.len() == n
}

#[doc(hidden)]
pub fn assign_g(
    keys: &[u64],
    ranks: &[u32],
    seed: u64,
    m: u32,
    modulus: u32,
    order: &[(u32, u32)],
) -> Vec<u32> {
    let mut g = vec![0u32; m as usize];
    let mut assigned = vec![false; m as usize];
    for &(ei, v) in order.iter().rev() {
        let h = hash3(keys[ei as usize], seed, m);
        let mut s = 0u32;
        for u in h {
            if assigned[u as usize] {
                s = s.wrapping_add(g[u as usize]);
            }
        }
        let rank = ranks[ei as usize];
        g[v as usize] = (rank + modulus - (s % modulus)) % modulus;
        assigned[v as usize] = true;
    }
    g
}

#[doc(hidden)]
pub fn compact_vertex_count(n: u32) -> u32 {
    let m = (u64::from(n) * GAMMA_NUM)
        .div_ceil(GAMMA_DEN)
        .max(u64::from(n) + 3)
        .max(3);
    let m = m.div_ceil(3) * 3;
    m as u32
}

#[doc(hidden)]
pub fn assign_compact(
    keys: &[u64],
    seed: u64,
    m: u32,
    order: &[(u32, u32)],
) -> (Vec<u32>, Box<[u64]>) {
    let mut g = vec![0u32; m as usize];
    let mut assigned = vec![false; m as usize];
    let mut occ = vec![0u64; (m as usize).div_ceil(64)];
    let part = m / 3;
    for &(ei, v) in order.iter().rev() {
        let h = hash3_partite(keys[ei as usize], seed, m);
        let i = if v < part {
            0
        } else if v < 2 * part {
            1
        } else {
            2
        };
        let mut s = 0u32;
        for u in h {
            if assigned[u as usize] {
                s = s.wrapping_add(g[u as usize]);
            }
        }
        g[v as usize] = (i + 3 - (s % 3)) % 3;
        assigned[v as usize] = true;
        occ_set(&mut occ, v);
    }
    (g, occ.into_boxed_slice())
}

#[doc(hidden)]
pub fn occ_packed_bytes(m: u32) -> u64 {
    u64::from(m).div_ceil(8)
}

#[doc(hidden)]
pub fn occ_set(occ: &mut [u64], v: u32) {
    occ[(v / 64) as usize] |= 1u64 << (v % 64);
}

#[doc(hidden)]
pub fn write_occ(occ: &[u64], m: u32, w: &mut impl Write) -> std::io::Result<()> {
    let n = occ_packed_bytes(m) as usize;
    let bytes = u64_words_as_bytes(occ);
    w.write_all(&bytes[..n])
}

#[doc(hidden)]
pub fn u64_words_as_bytes(occ: &[u64]) -> &[u8] {
    // SAFETY: occupancy bits are the LE byte image of the u64 words.
    unsafe { std::slice::from_raw_parts(occ.as_ptr().cast::<u8>(), occ.len() * 8) }
}

#[doc(hidden)]
pub fn popcount_bits(occ: &[u8], lo: usize, hi: usize) -> u32 {
    if lo >= hi {
        return 0;
    }
    let mut n = 0u32;
    let mut b = lo;
    while b < hi {
        let byte_i = b / 8;
        let bit = b % 8;
        let take = (8 - bit).min(hi - b);
        let byte = occ.get(byte_i).copied().unwrap_or(0);
        let mask = if take == 8 {
            0xFFu8
        } else {
            ((1u8 << take) - 1) << bit
        };
        n += (byte & mask).count_ones();
        b += take;
    }
    n
}

#[doc(hidden)]
pub fn supers_from_occ_bytes(occ: &[u8], m: u32) -> Box<[u32]> {
    let n_supers = (m as usize).div_ceil(RANK_SUPER_BITS as usize);
    let mut supers = vec![0u32; n_supers + 1];
    let mut acc = 0u32;
    for (i, slot) in supers.iter_mut().enumerate().take(n_supers) {
        *slot = acc;
        let bit0 = i * RANK_SUPER_BITS as usize;
        let bit1 = ((i + 1) * RANK_SUPER_BITS as usize).min(m as usize);
        acc += popcount_bits(occ, bit0, bit1);
    }
    supers[n_supers] = acc;
    supers.into_boxed_slice()
}

impl CompactRank {
    #[doc(hidden)]
    pub fn empty() -> Self {
        Self {
            m: 0,
            occ: OccBits::Words(Box::new([])),
            supers: Box::new([0]),
        }
    }

    #[doc(hidden)]
    pub fn from_occ(occ: Box<[u64]>, m: u32) -> Self {
        let supers = supers_from_occ_bytes(u64_words_as_bytes(&occ), m);
        Self {
            m,
            occ: OccBits::Words(occ),
            supers,
        }
    }

    #[doc(hidden)]
    pub fn from_mapped(
        map: ReadonlyMap,
        off: usize,
        len: usize,
        m: u32,
    ) -> Result<Self, StoreError> {
        let bytes = map.as_file_bytes();
        if off.saturating_add(len) > bytes.len() {
            return Err(StoreError::Corrupt("bdz mphf: occ truncated"));
        }
        let need = occ_packed_bytes(m) as usize;
        if len < need {
            return Err(StoreError::Corrupt("bdz mphf: occ truncated"));
        }
        let supers = supers_from_occ_bytes(&bytes[off..off + len], m);
        Ok(Self {
            m,
            occ: OccBits::Map { map, off, len },
            supers,
        })
    }

    #[doc(hidden)]
    pub fn occ_bytes(&self) -> &[u8] {
        match &self.occ {
            OccBits::Words(w) => {
                let n = occ_packed_bytes(self.m) as usize;
                let b = u64_words_as_bytes(w);
                &b[..n.min(b.len())]
            }
            OccBits::Map { map, off, len } => &map.as_file_bytes()[*off..*off + *len],
        }
    }

    #[doc(hidden)]
    pub fn rank1(&self, v: u32) -> u32 {
        let v = v.min(self.m);
        let si = (v / RANK_SUPER_BITS) as usize;
        let rem_lo = si * RANK_SUPER_BITS as usize;
        self.supers[si] + popcount_bits(self.occ_bytes(), rem_lo, v as usize)
    }
}
