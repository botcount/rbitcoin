//! Spender-meta annotation for Class A.
//!
//! Unknown meta reads a page, decides sole / multi / promote, then pwrites
//! each changed 8-byte slot. Confirm does not use that path. It logs every
//! published slot, including the short tail. A full page image is not
//! written: a torn page rewrite can zero a neighbor the spend marker already
//! covers. Queue depth stays 128 pages.

use crate::compact::output_flags;
use crate::error::StoreError;
use crate::io_handle::IoHandle;
use crate::spender_table::SpenderTable;
use crate::tx_table::{decode_spent_slot, encode_spent_slot, OutputRecord, TxTable};
use crate::uring_session::{self, UringSession};
use rbitcoin_primitives::Fk;
use std::collections::VecDeque;

#[doc(hidden)]
pub const META_LEN: usize = OutputRecord::SPENT_SLOT_LEN;
#[doc(hidden)]
pub const MAX_SLOTS: usize = 128;

#[cfg(debug_assertions)]
thread_local! {
    static SPEND_ANNOTATE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SPEND_ANNOTATE_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_spend_annotate_counts() {
    SPEND_ANNOTATE_READS.with(|n| n.set(0));
    SPEND_ANNOTATE_WRITES.with(|n| n.set(0));
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn spend_annotate_counts() -> (usize, usize) {
    (
        SPEND_ANNOTATE_READS.with(|n| n.get()),
        SPEND_ANNOTATE_WRITES.with(|n| n.get()),
    )
}

pub fn note_spend_read_for_test() {
    #[cfg(debug_assertions)]
    SPEND_ANNOTATE_READS.with(|n| n.set(n.get() + 1));
}

pub fn note_spend_write_for_test() {
    #[cfg(debug_assertions)]
    SPEND_ANNOTATE_WRITES.with(|n| n.set(n.get() + 1));
}
#[doc(hidden)]
pub type SpentAbsWrite = (u64, Fk, u32, Fk, u32, [u8; META_LEN]);

/// Annotate spends at absolute meta offsets via io_uring RMW.
///
/// `edges`: `(abs_off, create_tx_fk, vout, spending_tx_fk)`.
/// Returns edges that could not be annotated here (OOB abs, IO error deferred
/// as cold — empty when all succeed). On hard errors returns `Err`.
pub fn put_spend_batch_by_abs_meta_uring(
    txs: &TxTable,
    spenders: &SpenderTable,
    edges: &[(u64, Fk, u32, Fk, u32)],
) -> Result<Vec<(Fk, u32, Fk, u32)>, StoreError> {
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    for &(_, _, _, sfk, _) in edges {
        if sfk.is_null() {
            return Err(StoreError::InvalidFk);
        }
    }

    let body_pub = txs.spent.body_published_len();

    let mut cold: Vec<(Fk, u32, Fk, u32)> = Vec::new();
    let mut work: Vec<(u64, Fk, u32, Fk, u32)> = Vec::with_capacity(edges.len());
    for &(abs, cfk, vout, sfk, vin) in edges {
        if abs.saturating_add(META_LEN as u64) > body_pub {
            cold.push((cfk, vout, sfk, vin));
        } else {
            work.push((abs, cfk, vout, sfk, vin));
        }
    }
    if work.is_empty() {
        return Ok(cold);
    }

    let groups = group_edges_by_spent_page(&work, body_pub);
    let extra = annotate_unknown_pages_uring(txs, spenders, &groups)?;
    cold.extend(extra);
    Ok(cold)
}

#[doc(hidden)]
pub struct EdgePage {
    pub off: u64,
    pub len: usize,
    pub edges: Vec<(u64, Fk, u32, Fk, u32)>,
}

#[doc(hidden)]
pub fn group_edges_by_spent_page(
    edges: &[(u64, Fk, u32, Fk, u32)],
    body_pub: u64,
) -> Vec<EdgePage> {
    let mut sorted = edges.to_vec();
    sorted.sort_by_key(|e| e.0);
    let mut groups = Vec::new();
    let mut cur_lo = 0u64;
    let mut cur_hi = 0u64;
    let mut cur: Vec<(u64, Fk, u32, Fk, u32)> = Vec::new();
    let flush =
        |groups: &mut Vec<EdgePage>, lo: u64, hi: u64, cur: Vec<(u64, Fk, u32, Fk, u32)>| {
            if cur.is_empty() {
                return;
            }
            let Some((off, len)) = clip_spent_page_window(lo, hi, body_pub) else {
                return;
            };
            groups.push(EdgePage {
                off,
                len,
                edges: cur,
            });
        };
    for e in sorted {
        let (lo, hi) = spent_meta_page_span(e.0);
        if cur.is_empty() {
            cur_lo = lo;
            cur_hi = hi;
            cur.push(e);
            continue;
        }
        if lo < cur_hi {
            cur_hi = cur_hi.max(hi);
            cur.push(e);
        } else {
            flush(&mut groups, cur_lo, cur_hi, std::mem::take(&mut cur));
            cur_lo = lo;
            cur_hi = hi;
            cur.push(e);
        }
    }
    flush(&mut groups, cur_lo, cur_hi, cur);
    groups
}

#[doc(hidden)]
pub fn poke_edge_page(
    buf: &mut [u8],
    off: u64,
    edges: &[(u64, Fk, u32, Fk, u32)],
    spenders: &SpenderTable,
    cold: &mut Vec<(Fk, u32, Fk, u32)>,
) -> Result<Vec<(u64, [u8; META_LEN])>, StoreError> {
    let mut wrote = Vec::new();
    for &(abs, cfk, vout, sfk, vin) in edges {
        let i = abs.saturating_sub(off) as usize;
        if i.saturating_add(META_LEN) > buf.len() {
            cold.push((cfk, vout, sfk, vin));
            continue;
        }
        let Ok((flags, field, field_vin)) = decode_spent_slot(&buf[i..i + META_LEN]) else {
            cold.push((cfk, vout, sfk, vin));
            continue;
        };
        match decide_annotate(field, flags, field_vin, sfk, vin, spenders)? {
            AnnotateOp::Skip => {}
            AnnotateOp::Write(meta) => {
                buf[i..i + META_LEN].copy_from_slice(&meta);
                wrote.push((abs, meta));
            }
        }
    }
    Ok(wrote)
}

fn write_spent_slots(txs: &TxTable, slots: &[(u64, [u8; META_LEN])]) -> Result<(), StoreError> {
    for (abs, meta) in slots {
        txs.spent.write_body_pwrite(*abs, meta)?;
    }
    Ok(())
}

pub fn annotate_unknown_pages_pwrite(
    txs: &TxTable,
    spenders: &SpenderTable,
    edges: &[(u64, Fk, u32, Fk, u32)],
) -> Result<Vec<(Fk, u32, Fk, u32)>, StoreError> {
    let body_pub = txs.spent.body_published_len();
    let mut cold = Vec::new();
    let mut live = Vec::new();
    for &e in edges {
        if e.3.is_null() {
            return Err(StoreError::InvalidFk);
        }
        if e.0.saturating_add(META_LEN as u64) > body_pub {
            cold.push((e.1, e.2, e.3, e.4));
        } else {
            live.push(e);
        }
    }
    for g in group_edges_by_spent_page(&live, body_pub) {
        note_spend_read_for_test();
        let mut buf = vec![0u8; g.len];
        if txs
            .spent
            .read_prefix_at_published(body_pub, g.off, g.len as u64, &mut buf)
            .is_err()
        {
            for e in &g.edges {
                cold.push((e.1, e.2, e.3, e.4));
            }
            continue;
        }
        let wrote = poke_edge_page(&mut buf, g.off, &g.edges, spenders, &mut cold)?;
        if !wrote.is_empty() {
            note_spend_write_for_test();
            if write_spent_slots(txs, &wrote).is_err() {
                for e in &g.edges {
                    cold.push((e.1, e.2, e.3, e.4));
                }
            }
        }
    }
    Ok(cold)
}

#[doc(hidden)]
pub fn annotate_unknown_pages_uring(
    txs: &TxTable,
    spenders: &SpenderTable,
    groups: &[EdgePage],
) -> Result<Vec<(Fk, u32, Fk, u32)>, StoreError> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let body_fd = txs.spent.body_read_fd();
    let body_path = txs.spent.body_file_path().to_path_buf();
    struct PageSlot {
        group_i: usize,
        buf: Vec<u8>,
    }
    uring_session::with_thread_local(uring_session::DEFAULT_ENTRIES, |session| {
        session.begin_batch()?;
        let epoch = session.epoch();
        let mut pending: VecDeque<usize> = (0..groups.len()).collect();
        let nslots = MAX_SLOTS.min(groups.len().max(1));
        let mut free_slots: Vec<usize> = (0..nslots).collect();
        let mut slots: Vec<Option<PageSlot>> = (0..nslots).map(|_| None).collect();
        let mut session = session.drain_guard();
        let mut in_flight = 0usize;
        let mut cold = Vec::new();

        let arm = |session: &mut UringSession,
                   free_slots: &mut Vec<usize>,
                   slots: &mut [Option<PageSlot>],
                   pending: &mut VecDeque<usize>,
                   groups: &[EdgePage],
                   in_flight: &mut usize,
                   body_fd: IoHandle|
         -> Result<(), StoreError> {
            while *in_flight < slots.len() && session.free_sq() > 0 && !free_slots.is_empty() {
                let Some(gi) = pending.pop_front() else {
                    break;
                };
                let Some(slot) = free_slots.pop() else {
                    pending.push_front(gi);
                    break;
                };
                slots[slot] = Some(PageSlot {
                    group_i: gi,
                    buf: vec![0u8; groups[gi].len],
                });
                {
                    let s = slots[slot].as_mut().unwrap();
                    let ud = uring_session::pack_ud(
                        uring_session::KIND_SPEND_PAGE_READ,
                        epoch,
                        slot as u32,
                    );
                    note_spend_read_for_test();
                    // SAFETY: `s.buf` lives until this page-read wave drains.
                    unsafe { session.push_pread(body_fd, groups[gi].off, &mut s.buf, ud) }?;
                }
                *in_flight += 1;
            }
            Ok(())
        };

        arm(
            &mut session,
            &mut free_slots,
            &mut slots,
            &mut pending,
            groups,
            &mut in_flight,
            body_fd,
        )?;
        session.sync_submission();
        let _ = session.submit();

        while in_flight > 0 {
            let cqes = session.harvest_ready()?;
            if cqes.is_empty() {
                session.submit_and_wait_one()?;
                continue;
            }
            for (ud, res) in cqes {
                let (kind, _ep, slot) = uring_session::unpack_ud(ud);
                let slot = slot as usize;
                if slot >= slots.len() {
                    return Err(StoreError::Corrupt("spend annotate bad user_data"));
                }
                let mut st = slots[slot]
                    .take()
                    .ok_or(StoreError::Corrupt("spend annotate empty slot"))?;
                if kind != uring_session::KIND_SPEND_PAGE_READ {
                    return Err(StoreError::Corrupt("spend annotate bad user_data"));
                }
                in_flight = in_flight.saturating_sub(1);
                let gi = st.group_i;
                let want = st.buf.len();
                if let Err(e) = uring_session::require_full_cqe(res, want, &body_path) {
                    free_slots.push(slot);
                    return Err(e);
                }
                let wrote = poke_edge_page(
                    &mut st.buf,
                    groups[gi].off,
                    &groups[gi].edges,
                    spenders,
                    &mut cold,
                )?;
                if wrote.is_empty() {
                    free_slots.push(slot);
                    continue;
                }
                note_spend_write_for_test();
                // Slot pwrites, not the page image. A torn page write
                // would zero neighbors this read just observed.
                if write_spent_slots(txs, &wrote).is_err() {
                    for e in &groups[gi].edges {
                        cold.push((e.1, e.2, e.3, e.4));
                    }
                }
                free_slots.push(slot);
            }
            arm(
                &mut session,
                &mut free_slots,
                &mut slots,
                &mut pending,
                groups,
                &mut in_flight,
                body_fd,
            )?;
            session.sync_submission();
            let _ = session.submit();
        }
        while let Some(gi) = pending.pop_front() {
            for e in &groups[gi].edges {
                cold.push((e.1, e.2, e.3, e.4));
            }
        }
        Ok(cold)
    })?
}

/// Resolve pure-write annotate backend from env hierarchy.
#[inline]
pub fn spend_ann_backend() -> crate::io_backend::WriteIoBackend {
    crate::io_backend::write_io_backend()
}

/// Decision from structural snapshot + spend_fk (no body read).
#[doc(hidden)]
pub enum AnnotateOp {
    Skip,
    Write([u8; META_LEN]),
}

#[doc(hidden)]
pub fn decide_annotate(
    field: Fk,
    flags: u8,
    field_vin: u32,
    spend_fk: Fk,
    spend_vin: u32,
    spenders: &SpenderTable,
) -> Result<AnnotateOp, StoreError> {
    let multi = flags & output_flags::MULTI_SPENDER != 0;
    if !multi && field.is_null() {
        let meta = encode_spent_slot(flags & !output_flags::MULTI_SPENDER, spend_fk, spend_vin)?;
        return Ok(AnnotateOp::Write(meta));
    }
    if !multi && field == spend_fk && field_vin == spend_vin {
        return Ok(AnnotateOp::Skip);
    }
    if !multi {
        let e1 = spenders.append(field, field_vin, Fk::NULL)?;
        let e2 = spenders.append(spend_fk, spend_vin, e1)?;
        let meta = encode_spent_slot(flags | output_flags::MULTI_SPENDER, e2, 0)?;
        return Ok(AnnotateOp::Write(meta));
    }
    if multi_list_contains(spenders, field, spend_fk, spend_vin)? {
        return Ok(AnnotateOp::Skip);
    }
    let e = spenders.append(spend_fk, spend_vin, field)?;
    let meta = encode_spent_slot(flags | output_flags::MULTI_SPENDER, e, 0)?;
    Ok(AnnotateOp::Write(meta))
}

#[doc(hidden)]
pub fn multi_list_contains(
    spenders: &SpenderTable,
    head: Fk,
    spend_fk: Fk,
    spend_vin: u32,
) -> Result<bool, StoreError> {
    let mut cur = head;
    let mut n = 0u32;
    while let Some(id) = cur.get() {
        n = n.saturating_add(1);
        if n > 1_000_000 {
            return Err(StoreError::Corrupt("invariant: spender multi-list cycle"));
        }
        let (sfk, vin, next) = spenders.get(Fk(id))?;
        if sfk == spend_fk && vin == spend_vin {
            return Ok(true);
        }
        cur = next;
    }
    Ok(false)
}

/// One RMW window on `spent.body` (usually one 4 KiB page, clipped to the
/// published range and past the 16-byte file header).
#[doc(hidden)]
pub struct SpentPageGroup {
    pub off: u64,
    pub len: usize,
    pub writes: Vec<SpentAbsWrite>,
}

/// Page span covering the 8-byte slot at `abs` (`[lo, hi)`).
#[inline]
#[doc(hidden)]
pub fn spent_meta_page_span(abs: u64) -> (u64, u64) {
    let page = crate::tx_table::BODY_PAGE_SIZE;
    let lo = abs & !(page - 1);
    let last = abs.saturating_add(META_LEN as u64 - 1);
    let hi = (last & !(page - 1)).saturating_add(page);
    (lo, hi)
}

#[inline]
#[doc(hidden)]
pub fn clip_spent_page_window(span_lo: u64, span_hi: u64, body_pub: u64) -> Option<(u64, usize)> {
    let off = span_lo.max(crate::file::FILE_HEADER_LEN as u64);
    let end = span_hi.min(body_pub);
    if end <= off {
        return None;
    }
    Some((off, (end - off) as usize))
}

/// Group abs-sorted writes into non-overlapping `spent.body` page spans.
///
/// Same-page slots share one RMW. An 8 B slot that straddles a page boundary
/// extends the span so the next page's writes merge (no overlapping in-flight
/// RMWs). Adjacent pages without a straddle stay separate.
#[doc(hidden)]
pub fn group_writes_by_spent_page(writes: &[SpentAbsWrite], body_pub: u64) -> Vec<SpentPageGroup> {
    let mut groups = Vec::new();
    let mut cur_lo = 0u64;
    let mut cur_hi = 0u64;
    let mut cur: Vec<SpentAbsWrite> = Vec::new();

    let flush = |groups: &mut Vec<SpentPageGroup>, lo: u64, hi: u64, cur: Vec<SpentAbsWrite>| {
        if cur.is_empty() {
            return;
        }
        let Some((off, len)) = clip_spent_page_window(lo, hi, body_pub) else {
            return;
        };
        groups.push(SpentPageGroup {
            off,
            len,
            writes: cur,
        });
    };

    for &w in writes {
        let (lo, hi) = spent_meta_page_span(w.0);
        if cur.is_empty() {
            cur_lo = lo;
            cur_hi = hi;
            cur.push(w);
            continue;
        }
        if lo < cur_hi {
            cur_hi = cur_hi.max(hi);
            cur.push(w);
        } else {
            flush(&mut groups, cur_lo, cur_hi, std::mem::take(&mut cur));
            cur_lo = lo;
            cur_hi = hi;
            cur.push(w);
        }
    }
    flush(&mut groups, cur_lo, cur_hi, cur);
    groups
}

/// Pure-write annotate: `known[i]` is structural `(field, flags)` for `abs_edges[i]`.
///
/// A published slot, including the short tail, is a directory insert.
/// Returns edges past the published body as cold.
pub fn put_spend_batch_by_abs_meta_known(
    txs: &TxTable,
    spenders: &SpenderTable,
    abs_edges: &[(u64, Fk, u32, Fk, u32)],
    known: &[(Fk, u8, u32)],
) -> Result<Vec<(Fk, u32, Fk, u32)>, StoreError> {
    if abs_edges.is_empty() {
        return Ok(Vec::new());
    }
    if abs_edges.len() != known.len() {
        return Err(StoreError::Corrupt("spend annotate known length mismatch"));
    }
    for &(_, _, _, sfk, _) in abs_edges {
        if sfk.is_null() {
            return Err(StoreError::InvalidFk);
        }
    }

    let mut order: Vec<usize> = (0..abs_edges.len()).collect();
    order.sort_unstable_by_key(|&i| abs_edges[i].0);

    let body_pub = txs.spent.body_published_len();
    let mut cold: Vec<(Fk, u32, Fk, u32)> = Vec::new();
    let mut writes: Vec<SpentAbsWrite> = Vec::with_capacity(order.len());

    for &i in &order {
        let (abs, cfk, vout, sfk, vin) = abs_edges[i];
        if abs.saturating_add(META_LEN as u64) > body_pub {
            cold.push((cfk, vout, sfk, vin));
            continue;
        }
        let (field, flags, field_vin) = known[i];
        match decide_annotate(field, flags, field_vin, sfk, vin, spenders)? {
            AnnotateOp::Skip => {
                // `keep` can drop the image after this batch decided to skip.
                // A slot the log still serves is written again. The body is
                // read only on a miss, so a torn page does not fail that rewrite.
                let rewrite = txs.spend_log_serves(abs)
                    || !txs.body_slot_matches(abs, field, flags, field_vin)?;
                if rewrite {
                    let meta = encode_spent_slot(flags, field, field_vin)?;
                    writes.push((abs, cfk, vout, sfk, vin, meta));
                }
            }
            AnnotateOp::Write(meta) => writes.push((abs, cfk, vout, sfk, vin, meta)),
        }
    }

    if writes.is_empty() {
        return Ok(cold);
    }
    let more = apply_prepared_spend_writes(txs, &writes)?;
    cold.extend(more);
    Ok(cold)
}

/// Every slot inside the published body lands in the page directory.
///
/// A slot past that length is cold. This path does not write `spent.body`.
fn apply_prepared_spend_writes(
    txs: &TxTable,
    writes: &[SpentAbsWrite],
) -> Result<Vec<(Fk, u32, Fk, u32)>, StoreError> {
    let published = txs.spent.body_published_len();
    let mut cold = Vec::new();
    let mut hist: Vec<(u64, [u8; META_LEN])> = Vec::new();
    for w in writes {
        if crate::spend_page_log::is_historical(published, w.0) {
            hist.push((w.0, w.5));
        } else {
            cold.push((w.1, w.2, w.3, w.4));
        }
    }
    txs.note_historical_spend_slots(&hist)?;
    Ok(cold)
}
