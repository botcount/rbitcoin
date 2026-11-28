//! Esplora-compatible REST HTTP (plain HTTP; TLS via reverse proxy).
//!
//! Wallet clients (exact address/scripthash, tx/block by id, broadcast) and
//! **mempool/electrs HTTP drop-in** (`/internal/*` on unix listen) except
//! address-prefix. Surface: [`COMPAT.md`](../../../COMPAT.md).
//! `GET …/txs/summary` is a mempool.space-shaped compact dialect (not
//! Blockstream Esplora `API.md`). Opt-in `GET /block-template` is GBT.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub mod blindbit;
#[doc(hidden)]
pub mod handlers;
#[doc(hidden)]
pub mod internal;
#[doc(hidden)]
pub mod script_fields;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod tx_json;

pub use server::{
    perf_totals, run_esplora, sample_reset_perf, BlockTemplateFn, EsploraConfig, EsploraHandle,
    EsploraListen, DEFAULT_ACCEPT_MULTIPLIER, DEFAULT_MIN_DRAIN_RATE,
};
