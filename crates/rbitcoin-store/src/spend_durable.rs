//! Sidecar for heights whose spend annotations and Class A bodies are durable.
//!
//! Not a [`SCHEMA_VERSION`](rbitcoin_primitives::SCHEMA_VERSION) bump. A missing
//! file means nothing has been synced yet.

use crate::error::StoreError;
use crate::file::write_synced_tmp_rename;
use rbitcoin_primitives::{le_u16, le_u32, schema_file_openable, SCHEMA_VERSION, STORE_MAGIC};
use std::path::Path;

/// `store/spend_durable`. Annotated-through and durable-through heights.
pub const SPEND_DURABLE_NAME: &str = "spend_durable";

/// Wall time between spend `sync_data` checkpoints. Not a knob.
pub const SPEND_DURABLE_INTERVAL_MS: u64 = 600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpendDurable {
    pub annotated_through: u32,
    pub durable_through: u32,
}

impl SpendDurable {
    pub fn new(annotated_through: u32, durable_through: u32) -> Self {
        Self {
            annotated_through,
            durable_through,
        }
    }

    pub fn annotated_through(self) -> u32 {
        self.annotated_through
    }

    pub fn durable_through(self) -> u32 {
        self.durable_through
    }

    pub fn load(dir: &Path) -> Result<Option<Self>, StoreError> {
        let path = dir.join(SPEND_DURABLE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let buf = std::fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
        let Ok(raw) = <[u8; 16]>::try_from(buf.as_slice()) else {
            return Err(StoreError::Corrupt("spend_durable short"));
        };
        if raw[0..4] != STORE_MAGIC {
            return Err(StoreError::BadMagic);
        }
        let ver = le_u16!(raw, 4);
        if !schema_file_openable(ver) {
            return Err(StoreError::BadSchema(ver));
        }
        Ok(Some(Self {
            annotated_through: le_u32!(raw, 8),
            durable_through: le_u32!(raw, 12),
        }))
    }

    pub fn store(&self, dir: &Path) -> Result<(), StoreError> {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&STORE_MAGIC);
        buf[4..6].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
        buf[8..12].copy_from_slice(&self.annotated_through.to_le_bytes());
        buf[12..16].copy_from_slice(&self.durable_through.to_le_bytes());
        write_synced_tmp_rename(&dir.join(SPEND_DURABLE_NAME), &buf)
    }
}

#[cfg(debug_assertions)]
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True when a spend checkpoint is due. Batch count is not an input.
pub fn spend_sync_due(elapsed_ms: u64) -> bool {
    elapsed_ms >= SPEND_DURABLE_INTERVAL_MS
}

/// A failed publish does not start a new interval.
pub fn elapsed_after_checkpoint(elapsed_ms: u64, published: bool) -> u64 {
    if published {
        0
    } else {
        elapsed_ms
    }
}

/// `n == 0` stays "whole chain". A missing marker keeps `n`. Otherwise the
/// window is at least six and reaches back to `durable_through`.
pub fn widen_checkblocks(n: u32, tip: Option<u32>, durable_through: Option<u32>) -> u32 {
    if n == 0 {
        return 0;
    }
    let (Some(tip), Some(d)) = (tip, durable_through) else {
        return n;
    };
    let span = tip.saturating_sub(d);
    n.max(crate::VERIFY_TIP_BLOCKS).max(span)
}
