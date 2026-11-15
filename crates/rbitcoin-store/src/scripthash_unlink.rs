//! Disconnect unlink for one scripthash.
//!
//! Tip creates are a suffix of the key. An extent drops that suffix from the
//! tail pages. The old page image is saved first, then the new tail is written,
//! then the head. A crash before the undo record is cleared restores the old
//! tail and the old head word on open.

use super::{
    sh_page_as_array, sh_page_chunk_ranges, sh_page_extent, sh_page_next,
    sh_page_pack_extent_last_fks, sh_page_pack_fks, write_alloc_header, AllocState, LocatedHome,
    ScriptHashTable, StoreError, TableFile, SH_PAGE_SIZE, SH_PAGE_SLAB_CLASS,
};
use crate::scripthash_layout::ShHeadValue;
use crate::scripthash_pages::{
    sh_page_entries_into, sh_page_extent_creates, sh_page_set_extent_creates,
};
use rbitcoin_primitives::{le_u16, le_u64, Fk};
#[cfg(debug_assertions)]
use std::cell::Cell;
use std::collections::BTreeSet;

#[cfg(debug_assertions)]
thread_local! {
    static WATCH_READABLE: Cell<bool> = const { Cell::new(false) };
    static READABLE_BROKE: Cell<bool> = const { Cell::new(false) };
    static FAIL_CLEAR_UNDO: Cell<bool> = const { Cell::new(false) };
    static FAIL_NEXT_DEFERRED_FREE: Cell<bool> = const { Cell::new(false) };
}

#[doc(hidden)]
pub const UNDO_AT: usize = 256;
#[doc(hidden)]
pub const UNDO_LEN: usize = 48;
#[doc(hidden)]
pub const UNDO_MAGIC: [u8; 4] = *b"SHUN";

#[doc(hidden)]
pub const HEAD_NONE: u8 = 0;
#[doc(hidden)]
pub const HEAD_MAIN: u8 = 1;
#[doc(hidden)]
pub const HEAD_INGEST: u8 = 2;
#[doc(hidden)]
pub const HEAD_SORTED_OVF: u8 = 3;
#[doc(hidden)]
pub const HEAD_OVF_L1: u8 = 4;

#[doc(hidden)]
pub struct HeadWord {
    pub tag: u8,
    pub index: u16,
    pub off: u64,
    pub old: [u8; 8],
}

#[doc(hidden)]
pub struct UndoRec {
    pub target: u64,
    pub image: u64,
    pub word: Option<HeadWord>,
}

#[derive(Default)]
#[doc(hidden)]
pub struct UnlinkBatch {
    pub rewrites: Vec<DeferredRewrite>,
    pub main_shards: Vec<usize>,
    pub ovf: bool,
}

#[doc(hidden)]
pub struct DeferredRewrite {
    pub scripthash: [u8; 32],
    pub home: LocatedHome,
    pub new_val: ShHeadValue,
    pub old: ShHeadValue,
    pub removed: u64,
}

/// A full-list unlink. The empty head and the page free wait until the
/// confirmed tip shrink is durable.
pub(super) struct PendingEmpty {
    scripthash: [u8; 32],
    home: LocatedHome,
    old: ShHeadValue,
    removed: u64,
    /// Empty head is durable. A retry must not write it again, and a second
    /// defer of this key must not replace the page list.
    head_synced: bool,
    /// Pages still to free. `None` means they have not been read yet.
    pages_left: Option<Vec<(u8, u64)>>,
    /// `live_count` already dropped `removed`. A header retry must not do it again.
    live_applied: bool,
}

pub enum SuffixPlan {
    NotSuffix,
    Absent,
    DropAll {
        removed: u64,
    },
    Rewrite {
        boundary: u64,
        keep: Vec<u64>,
        drop_pages: Vec<u64>,
        base: u64,
        old_n: u32,
        creates: u32,
        removed: u64,
    },
}

impl ScriptHashTable {
    /// Remove `create_tx_fk`s from one scripthash. Duplicate fks count once.
    ///
    /// `Ok(true)` when at least one fk was stored. A pure suffix of an extent
    /// rewrites the tail pages. Any other shape rewrites the key once, and the
    /// previous pages stay readable until the head names the replacement.
    ///
    /// Refused while any empty head is still queued. Disconnect publishes
    /// those through [`Self::commit_deferred_empty_heads`] after the tip flush.
    #[cfg(debug_assertions)]
    pub fn unlink_creates(
        &self,
        scripthash: &[u8; 32],
        create_tx_fks: &[Fk],
    ) -> Result<bool, StoreError> {
        if !self.pending_empty.lock().unwrap().is_empty() {
            return Err(StoreError::Rejected(
                "scripthash unlink while an empty head is still queued",
            ));
        }
        let mut batch = UnlinkBatch::default();
        let changed = self.unlink_one(scripthash, create_tx_fks, &mut batch)?;
        self.finish_unlink_batch(batch)?;
        // The queue was empty on entry, so this publishes only a full drop
        // this call just deferred.
        self.commit_deferred_for(scripthash)?;
        Ok(changed)
    }

    /// Unlink every group, then sync the new chains and heads once before any
    /// replaced page is freed. A full-list drop stays in memory until
    /// [`Self::commit_deferred_empty_heads`].
    pub fn unlink_creates_batch(&self, groups: &[([u8; 32], Vec<Fk>)]) -> Result<(), StoreError> {
        let mut batch = UnlinkBatch::default();
        for (scripthash, fks) in groups {
            self.unlink_one(scripthash, fks, &mut batch)?;
        }
        self.finish_unlink_batch(batch)
    }

    #[doc(hidden)]
    pub fn unlink_one(
        &self,
        scripthash: &[u8; 32],
        create_tx_fks: &[Fk],
        batch: &mut UnlinkBatch,
    ) -> Result<bool, StoreError> {
        let mut remove = BTreeSet::new();
        for fk in create_tx_fks {
            if !fk.is_null() {
                remove.insert(fk.0);
            }
        }
        if remove.is_empty() {
            return Ok(false);
        }
        let Some((val, home)) = self.locate_head(scripthash)? else {
            return Ok(false);
        };
        if val.is_empty() {
            return Ok(false);
        }
        let ShHeadValue::Extent { last_page } = val else {
            return self.unlink_by_rewrite(scripthash, home, &val, &remove, batch);
        };
        let body = self.body_for(scripthash, super::KeyHome::from(home));
        match plan_extent_suffix(body, last_page, &remove)? {
            SuffixPlan::Absent => Ok(false),
            SuffixPlan::NotSuffix => self.unlink_by_rewrite(scripthash, home, &val, &remove, batch),
            plan => self.apply_suffix_plan(scripthash, home, &val, plan),
        }
    }

    /// Unlink one create_tx_fk. One tx pays a scripthash once, however many
    /// outputs share that script. Disconnect uses [`Self::unlink_creates_batch`].
    #[cfg(debug_assertions)]
    pub fn unlink_create(
        &self,
        scripthash: &[u8; 32],
        create_tx_fk: Fk,
        _vout: u32,
    ) -> Result<bool, StoreError> {
        self.unlink_creates(scripthash, &[create_tx_fk])
    }

    pub fn recover_unlink_undos(&self) -> Result<(), StoreError> {
        for (i, body) in self.bodies.iter().enumerate() {
            self.recover_one_undo(body, &self.allocs[i])?;
        }
        if let (Some(body), Some(mu)) = (&self.ovf_body, &self.ovf_alloc) {
            self.recover_one_undo(body, mu)?;
        }
        Ok(())
    }

    /// Leave a torn last page and an empty head word. Open restores both.
    #[cfg(debug_assertions)]
    pub fn test_tear_extent_tail(&self, scripthash: &[u8; 32]) -> Result<(), StoreError> {
        let Some((val, home)) = self.locate_head(scripthash)? else {
            return Err(StoreError::Corrupt("scripthash tear: missing key"));
        };
        let ShHeadValue::Extent { last_page } = val else {
            return Err(StoreError::Corrupt("scripthash tear: not an extent"));
        };
        let body = self.body_for(scripthash, super::KeyHome::from(home));
        let alloc_mu = self.alloc_for(scripthash, super::KeyHome::from(home));
        let mut alloc = alloc_mu.lock().unwrap();
        let word = self.capture_head_word(scripthash, home)?;
        let rec = arm_undo(self, body, &mut alloc, last_page, word)?;
        let mut torn = [0u8; SH_PAGE_SIZE];
        body.read_at(last_page, &mut torn)?;
        torn[0] = 0;
        body.write_at(last_page, &torn)?;
        let _ = rec;
        self.unlink_write_home(scripthash, home, &ShHeadValue::Empty)?;
        self.sync_home(home, scripthash)?;
        body.sync_data_only()?;
        Ok(())
    }

    #[cfg(debug_assertions)]
    pub fn watch_readable_during_unlink(&self, on: bool) {
        WATCH_READABLE.with(|c| c.set(on));
        if on {
            READABLE_BROKE.with(|c| c.set(false));
        }
    }

    #[cfg(debug_assertions)]
    pub fn readable_broke(&self) -> bool {
        READABLE_BROKE.with(|c| c.get())
    }

    #[cfg(debug_assertions)]
    pub fn fail_next_undo_clear(&self) {
        FAIL_CLEAR_UNDO.with(|c| c.set(true));
    }

    #[doc(hidden)]
    pub fn unlink_by_rewrite(
        &self,
        scripthash: &[u8; 32],
        home: LocatedHome,
        old: &ShHeadValue,
        remove: &BTreeSet<u64>,
        batch: &mut UnlinkBatch,
    ) -> Result<bool, StoreError> {
        let body = self.body_for(scripthash, super::KeyHome::from(home));
        let mut live = self.collect_entries_from(body, old)?;
        let before = live.len();
        live.retain(|fk| !remove.contains(&fk.0));
        if live.len() == before {
            return Ok(false);
        }
        if live.is_empty() {
            self.defer_full_unlink(scripthash, home, old.clone(), before as u64);
            return Ok(true);
        }
        let alloc_mu = self.alloc_for(scripthash, super::KeyHome::from(home));
        let mut alloc = alloc_mu.lock().unwrap();
        let saved = *alloc;
        let new_val = match self.rewrite_entries_for_key(body, &mut alloc, &live) {
            Ok(v) => v,
            Err(e) => {
                *alloc = saved;
                return Err(e);
            }
        };
        if let Err(e) = write_alloc_header(body, &alloc) {
            *alloc = saved;
            return Err(e);
        }
        let removed = (before - live.len()) as u64;
        if frees_old_pages(old) {
            // Head stays on the old chain until the batch syncs these bytes.
            note_batch_body(self, batch, scripthash, home);
            batch.rewrites.push(DeferredRewrite {
                scripthash: *scripthash,
                home,
                new_val,
                old: old.clone(),
                removed,
            });
            return Ok(true);
        }
        if let Err(e) = self.unlink_write_home(scripthash, home, &new_val) {
            *alloc = saved;
            let _ = write_alloc_header(body, &alloc);
            return Err(e);
        }
        alloc.live_count = alloc.live_count.saturating_sub(removed);
        write_alloc_header(body, &alloc)?;
        Ok(true)
    }

    #[doc(hidden)]
    pub fn finish_unlink_batch(&self, batch: UnlinkBatch) -> Result<(), StoreError> {
        if batch.rewrites.is_empty() {
            return Ok(());
        }
        for si in &batch.main_shards {
            self.bodies[*si].sync_data_only()?;
        }
        if batch.ovf {
            if let Some(body) = &self.ovf_body {
                body.sync_data_only()?;
            }
        }
        for job in &batch.rewrites {
            self.unlink_write_home(&job.scripthash, job.home, &job.new_val)?;
        }
        self.sync_batch_homes(&batch)?;
        for job in &batch.rewrites {
            let body = self.body_for(&job.scripthash, super::KeyHome::from(job.home));
            let alloc_mu = self.alloc_for(&job.scripthash, super::KeyHome::from(job.home));
            let mut alloc = alloc_mu.lock().unwrap();
            alloc.live_count = alloc.live_count.saturating_sub(job.removed);
            if let Err(e) = self.free_if_paged(body, &mut alloc, &job.old) {
                let _ = write_alloc_header(body, &alloc);
                return Err(e);
            }
            write_alloc_header(body, &alloc)?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn sync_batch_homes(&self, batch: &UnlinkBatch) -> Result<(), StoreError> {
        for si in &batch.main_shards {
            if let Some(slot) = self.sorted_main.get(*si) {
                if let Some(h) = slot.read().unwrap().as_ref() {
                    h.flush()?;
                }
            }
        }
        let ingest = batch
            .rewrites
            .iter()
            .any(|job| matches!(job.home, LocatedHome::Main | LocatedHome::Ingest));
        let ovf = batch
            .rewrites
            .iter()
            .any(|job| matches!(job.home, LocatedHome::SealedOvf));
        if ingest {
            self.ingest.lock().unwrap().flush()?;
        }
        if ovf {
            for h in self.sealed_ovf.lock().unwrap().iter() {
                h.flush()?;
            }
            if let Some(l1) = self.ovf_l1.lock().unwrap().as_ref() {
                l1.head.flush()?;
            }
        }
        Ok(())
    }

    fn defer_full_unlink(
        &self,
        scripthash: &[u8; 32],
        home: LocatedHome,
        old: ShHeadValue,
        removed: u64,
    ) {
        let mut q = self.pending_empty.lock().unwrap();
        // A retry before the empty head is durable is the same chain. A later
        // chain, queued after that head synced, has to sit beside it or the
        // new pages are never freed.
        if let Some(job) = q
            .iter_mut()
            .find(|job| &job.scripthash == scripthash && !job.head_synced)
        {
            job.home = home;
            job.old = old;
            job.removed = removed;
            job.pages_left = None;
            return;
        }
        q.push(PendingEmpty {
            scripthash: *scripthash,
            home,
            old,
            removed,
            head_synced: false,
            pages_left: None,
            live_applied: false,
        });
    }

    /// True while a full-list unlink is waiting for the tip shrink.
    ///
    /// The disk head may still be the previous value, or it may already be
    /// empty while a free is still queued. Callers that refresh a RAM cache
    /// must treat this as empty.
    pub fn deferred_empty(&self, scripthash: &[u8; 32]) -> bool {
        self.pending_empty
            .lock()
            .unwrap()
            .iter()
            .any(|job| &job.scripthash == scripthash)
    }

    /// Sync empty heads and free their pages. Disconnect calls this after
    /// `flush_confirmed_only` has returned.
    ///
    /// A free that fails after the head sync stays queued. The retry frees
    /// only the pages that are still on the list.
    pub fn commit_deferred_empty_heads(&self) -> Result<(), StoreError> {
        // One free error used to stop the loop and sit at the end, so the
        // next pop retried it before any key that had not been tried. Drain
        // the jobs already queued. Failures go back after that pass.
        let mut failed = Vec::new();
        let mut first_err = None;
        loop {
            let Some(mut job) = self.pending_empty.lock().unwrap().pop() else {
                break;
            };
            if let Err(e) = self.commit_one_empty(&mut job) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                failed.push(job);
            }
        }
        if !failed.is_empty() {
            let mut q = self.pending_empty.lock().unwrap();
            // Jobs deferred while this pass ran stay at the end, so the next
            // pop sees them before these retries.
            for job in failed.into_iter().rev() {
                q.insert(0, job);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Publish empty heads for `scripthash` only. Jobs for other keys stay queued.
    #[cfg(debug_assertions)]
    fn commit_deferred_for(&self, scripthash: &[u8; 32]) -> Result<(), StoreError> {
        let mine = {
            let mut q = self.pending_empty.lock().unwrap();
            let mut mine = Vec::new();
            let mut keep = Vec::new();
            for job in q.drain(..) {
                if &job.scripthash == scripthash {
                    mine.push(job);
                } else {
                    keep.push(job);
                }
            }
            *q = keep;
            mine
        };
        let mut failed = Vec::new();
        let mut first_err = None;
        for mut job in mine {
            if let Err(e) = self.commit_one_empty(&mut job) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                failed.push(job);
            }
        }
        if !failed.is_empty() {
            let mut q = self.pending_empty.lock().unwrap();
            for job in failed.into_iter().rev() {
                q.insert(0, job);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn commit_one_empty(&self, job: &mut PendingEmpty) -> Result<(), StoreError> {
        if !job.head_synced {
            self.unlink_write_home(&job.scripthash, job.home, &ShHeadValue::Empty)?;
            self.sync_home(job.home, &job.scripthash)?;
            job.head_synced = true;
        }
        if job.pages_left.is_none() {
            job.pages_left = Some(self.deferred_page_list(job)?);
        }
        let mut pages = job.pages_left.take().unwrap_or_default();
        let body = self.body_for(&job.scripthash, super::KeyHome::from(job.home));
        let alloc_mu = self.alloc_for(&job.scripthash, super::KeyHome::from(job.home));
        let mut alloc = alloc_mu.lock().unwrap();
        let mut next = 0;
        while next < pages.len() {
            let (class, off) = pages[next];
            if let Err(e) = self.free_deferred_page(body, &mut alloc, class, off) {
                job.pages_left = Some(pages.split_off(next));
                return Err(e);
            }
            next = next.saturating_add(1);
        }
        if !job.live_applied {
            alloc.live_count = alloc.live_count.saturating_sub(job.removed);
            job.live_applied = true;
        }
        if let Err(e) = write_alloc_header(body, &alloc) {
            job.pages_left = Some(Vec::new());
            return Err(e);
        }
        Ok(())
    }

    /// Page offsets, read before any freelist write. A later free error
    /// retries this list instead of walking a page that is already a freelist link.
    fn deferred_page_list(&self, job: &PendingEmpty) -> Result<Vec<(u8, u64)>, StoreError> {
        let body = self.body_for(&job.scripthash, super::KeyHome::from(job.home));
        match &job.old {
            ShHeadValue::Extent { last_page } => {
                let first = super::paged_first_from_last(body, *last_page)?;
                let mut pages = Vec::new();
                super::follow_page_chain(body, first, super::SH_PAGE_CHAIN_MAX, |_body, off| {
                    pages.push((SH_PAGE_SLAB_CLASS, off));
                    Ok(())
                })?;
                Ok(pages)
            }
            ShHeadValue::Slab { class, off, .. } => Ok(vec![(*class, *off)]),
            _ => Ok(Vec::new()),
        }
    }

    fn free_deferred_page(
        &self,
        body: &TableFile,
        alloc: &mut AllocState,
        class: u8,
        off: u64,
    ) -> Result<(), StoreError> {
        #[cfg(debug_assertions)]
        if FAIL_NEXT_DEFERRED_FREE.with(|c| c.replace(false)) {
            return Err(StoreError::Corrupt("scripthash deferred free failed"));
        }
        self.free_slab(body, alloc, class, off)
    }

    /// The next deferred page free fails once, before the freelist changes.
    #[cfg(debug_assertions)]
    pub fn fail_next_deferred_free() {
        FAIL_NEXT_DEFERRED_FREE.with(|c| c.set(true));
    }

    fn apply_suffix_plan(
        &self,
        scripthash: &[u8; 32],
        home: LocatedHome,
        old: &ShHeadValue,
        plan: SuffixPlan,
    ) -> Result<bool, StoreError> {
        let body = self.body_for(scripthash, super::KeyHome::from(home));
        let alloc_mu = self.alloc_for(scripthash, super::KeyHome::from(home));
        let mut alloc = alloc_mu.lock().unwrap();
        match plan {
            SuffixPlan::DropAll { removed } => {
                drop(alloc);
                self.defer_full_unlink(scripthash, home, old.clone(), removed);
                Ok(removed > 0)
            }
            SuffixPlan::Rewrite {
                boundary,
                keep,
                drop_pages,
                base,
                old_n,
                creates,
                removed,
            } => {
                let old_last = match old {
                    ShHeadValue::Extent { last_page } => *last_page,
                    _ => boundary,
                };
                let extent_n = new_extent_n(base, old_n, boundary);
                let creates = new_creates(creates, removed);
                if boundary != old_last {
                    // The head still names `old_last`. Shrink that page's
                    // extent first so `boundary` is not an interior page
                    // when it is rewritten.
                    self.retire_head_page(
                        body,
                        &mut alloc,
                        scripthash,
                        home,
                        old_last,
                        &BoundaryPack {
                            base,
                            extent_n,
                            boundary,
                            creates,
                        },
                    )?;
                }
                let prepared = prepare_boundary(
                    self,
                    body,
                    &mut alloc,
                    &BoundaryPack {
                        base,
                        extent_n,
                        boundary,
                        creates,
                    },
                    &keep,
                )?;
                let new_head = if prepared.new_last == old_last && drop_pages.is_empty() {
                    None
                } else {
                    Some(ShHeadValue::extent(prepared.new_last))
                };
                self.commit_tail_page(
                    body,
                    &mut alloc,
                    scripthash,
                    home,
                    TailWrite {
                        target: prepared.at,
                        bytes: &prepared.bytes,
                        new_head,
                        head_page: old_last,
                        removed,
                    },
                )?;
                for off in drop_pages {
                    self.free_slab(body, &mut alloc, SH_PAGE_SLAB_CLASS, off)?;
                }
                write_alloc_header(body, &alloc)?;
                Ok(true)
            }
            SuffixPlan::Absent | SuffixPlan::NotSuffix => Ok(false),
        }
    }

    #[doc(hidden)]
    pub fn commit_tail_page(
        &self,
        body: &TableFile,
        alloc: &mut AllocState,
        scripthash: &[u8; 32],
        home: LocatedHome,
        write: TailWrite<'_>,
    ) -> Result<(), StoreError> {
        let word = if write.new_head.is_some() {
            self.capture_head_word(scripthash, home)?
        } else {
            None
        };
        let rec = arm_undo(self, body, alloc, write.target, word)?;
        if let Err(e) = body.write_at(write.target, write.bytes) {
            self.rollback_undo(body, alloc, &rec)?;
            return Err(e);
        }
        body.sync_data_only()?;
        note_readable(body, write.head_page);
        if let Some(val) = write.new_head {
            if let Err(e) = self.unlink_write_home(scripthash, home, &val) {
                self.rollback_undo(body, alloc, &rec)?;
                return Err(e);
            }
            if let Err(e) = self.sync_home(home, scripthash) {
                self.rollback_undo(body, alloc, &rec)?;
                return Err(e);
            }
            if let ShHeadValue::Extent { last_page } = val {
                note_readable(body, last_page);
            }
        }
        alloc.live_count = alloc.live_count.saturating_sub(write.removed);
        if let Err(e) = clear_undo_keep_image(body, alloc) {
            alloc.live_count = alloc.live_count.saturating_add(write.removed);
            self.rollback_undo(body, alloc, &rec)?;
            return Err(e);
        }
        self.free_slab(body, alloc, SH_PAGE_SLAB_CLASS, rec.image)?;
        Ok(())
    }

    /// Publish a shorter extent on the page the head already names.
    ///
    /// The page stays `ver=2` and empty. Readers follow the predecessor's
    /// `next` onto it and stop, so a later rewrite of that predecessor is
    /// not an interior-link break.
    #[doc(hidden)]
    pub fn retire_head_page(
        &self,
        body: &TableFile,
        alloc: &mut AllocState,
        scripthash: &[u8; 32],
        home: LocatedHome,
        head_page: u64,
        pack: &BoundaryPack,
    ) -> Result<(), StoreError> {
        let mut page = [0u8; SH_PAGE_SIZE];
        sh_page_pack_extent_last_fks(&mut page, &[], pack.base, pack.extent_n, 0)?;
        sh_page_set_extent_creates(&mut page, pack.creates);
        self.commit_tail_page(
            body,
            alloc,
            scripthash,
            home,
            TailWrite {
                target: head_page,
                bytes: &page,
                new_head: None,
                head_page,
                removed: 0,
            },
        )
    }

    #[doc(hidden)]
    pub fn rollback_undo(
        &self,
        body: &TableFile,
        alloc: &mut AllocState,
        rec: &UndoRec,
    ) -> Result<(), StoreError> {
        restore_page(body, rec)?;
        if let Some(word) = &rec.word {
            self.restore_head_word(word)?;
        }
        clear_undo_keep_image(body, alloc)?;
        self.free_slab(body, alloc, SH_PAGE_SLAB_CLASS, rec.image)?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn recover_one_undo(
        &self,
        body: &TableFile,
        alloc_mu: &std::sync::Mutex<AllocState>,
    ) -> Result<(), StoreError> {
        let Some(rec) = read_undo(body)? else {
            return Ok(());
        };
        let mut alloc = alloc_mu.lock().unwrap();
        self.rollback_undo(body, &mut alloc, &rec)?;
        write_alloc_header(body, &alloc)?;
        body.sync_data_only()?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn capture_head_word(
        &self,
        scripthash: &[u8; 32],
        home: LocatedHome,
    ) -> Result<Option<HeadWord>, StoreError> {
        let hk = super::head_key_from_full(scripthash);
        match home {
            LocatedHome::Main => {
                let si = self.shard_index(scripthash);
                if let Some(slot) = self.sorted_main.get(si) {
                    let g = slot.read().unwrap();
                    if let Some(h) = g.as_ref() {
                        if let Some(off) = h.value_word_offset(&hk)? {
                            let old = h.read_value_word(off)?;
                            return Ok(Some(HeadWord {
                                tag: HEAD_MAIN,
                                index: si as u16,
                                off,
                                old,
                            }));
                        }
                    }
                }
                self.capture_ingest_word(scripthash)
            }
            LocatedHome::Ingest => self.capture_ingest_word(scripthash),
            LocatedHome::SealedOvf => {
                let g = self.sealed_ovf.lock().unwrap();
                for (i, h) in g.iter().enumerate().rev() {
                    if let Some(off) = h.value_word_offset(&hk)? {
                        let old = h.read_value_word(off)?;
                        return Ok(Some(HeadWord {
                            tag: HEAD_SORTED_OVF,
                            index: i as u16,
                            off,
                            old,
                        }));
                    }
                }
                drop(g);
                if let Some(l1) = self.ovf_l1.lock().unwrap().as_ref() {
                    if let Some(off) = l1.head.value_word_offset(&hk)? {
                        let old = l1.head.read_value_word(off)?;
                        return Ok(Some(HeadWord {
                            tag: HEAD_OVF_L1,
                            index: 0,
                            off,
                            old,
                        }));
                    }
                }
                Ok(None)
            }
        }
    }

    #[doc(hidden)]
    pub fn capture_ingest_word(
        &self,
        scripthash: &[u8; 32],
    ) -> Result<Option<HeadWord>, StoreError> {
        let ingest = self.ingest.lock().unwrap();
        let Some(off) = ingest.value_word_offset(scripthash)? else {
            return Ok(None);
        };
        let old = ingest.read_value_word(off)?;
        Ok(Some(HeadWord {
            tag: HEAD_INGEST,
            index: 0,
            off,
            old,
        }))
    }

    #[doc(hidden)]
    pub fn restore_head_word(&self, word: &HeadWord) -> Result<(), StoreError> {
        match word.tag {
            HEAD_NONE => Ok(()),
            HEAD_MAIN => {
                let si = usize::from(word.index);
                let Some(slot) = self.sorted_main.get(si) else {
                    return Err(StoreError::Corrupt("scripthash undo: main shard missing"));
                };
                let g = slot.read().unwrap();
                let Some(h) = g.as_ref() else {
                    return Err(StoreError::Corrupt("scripthash undo: main head missing"));
                };
                h.write_value_word(word.off, &word.old)?;
                h.flush()
            }
            HEAD_INGEST => {
                let ingest = self.ingest.lock().unwrap();
                ingest.write_value_word(word.off, &word.old)?;
                ingest.flush()
            }
            HEAD_SORTED_OVF => {
                let g = self.sealed_ovf.lock().unwrap();
                let Some(h) = g.get(usize::from(word.index)) else {
                    return Err(StoreError::Corrupt("scripthash undo: ovf head missing"));
                };
                h.write_value_word(word.off, &word.old)?;
                h.flush()
            }
            HEAD_OVF_L1 => {
                let g = self.ovf_l1.lock().unwrap();
                let Some(l1) = g.as_ref() else {
                    return Err(StoreError::Corrupt("scripthash undo: ovf l1 missing"));
                };
                l1.head.write_value_word(word.off, &word.old)?;
                l1.head.flush()
            }
            _ => Err(StoreError::Corrupt("scripthash undo: bad head tag")),
        }
    }

    #[doc(hidden)]
    pub fn sync_home(&self, home: LocatedHome, scripthash: &[u8; 32]) -> Result<(), StoreError> {
        match home {
            LocatedHome::Main => {
                let si = self.shard_index(scripthash);
                if let Some(slot) = self.sorted_main.get(si) {
                    if let Some(h) = slot.read().unwrap().as_ref() {
                        h.flush()?;
                    }
                }
                self.ingest.lock().unwrap().flush()
            }
            LocatedHome::Ingest => self.ingest.lock().unwrap().flush(),
            LocatedHome::SealedOvf => {
                for h in self.sealed_ovf.lock().unwrap().iter() {
                    h.flush()?;
                }
                if let Some(l1) = self.ovf_l1.lock().unwrap().as_ref() {
                    l1.head.flush()?;
                }
                Ok(())
            }
        }
    }
}

#[doc(hidden)]
pub struct PreparedTail {
    pub at: u64,
    pub bytes: [u8; SH_PAGE_SIZE],
    pub new_last: u64,
}

#[doc(hidden)]
pub struct BoundaryPack {
    pub base: u64,
    pub extent_n: u32,
    pub boundary: u64,
    pub creates: u32,
}

#[doc(hidden)]
pub struct TailWrite<'a> {
    pub target: u64,
    pub bytes: &'a [u8; SH_PAGE_SIZE],
    pub new_head: Option<ShHeadValue>,
    /// Page the head names while `target` is written.
    pub head_page: u64,
    pub removed: u64,
}

#[doc(hidden)]
pub fn prepare_boundary(
    table: &ScriptHashTable,
    body: &TableFile,
    alloc: &mut AllocState,
    pack: &BoundaryPack,
    keep: &[u64],
) -> Result<PreparedTail, StoreError> {
    if keep.is_empty() {
        return Err(StoreError::Corrupt("scripthash suffix keep is empty"));
    }
    let fks: Vec<Fk> = keep.iter().copied().map(Fk).collect();
    let chunks = sh_page_chunk_ranges(&fks)?;
    if chunks.len() > 2 {
        return Err(StoreError::Corrupt(
            "scripthash suffix keep exceeds two pages",
        ));
    }
    if chunks.len() == 1 {
        let mut page = [0u8; SH_PAGE_SIZE];
        sh_page_pack_extent_last_fks(&mut page, keep, pack.base, pack.extent_n, 0)?;
        sh_page_set_extent_creates(&mut page, pack.creates);
        return Ok(PreparedTail {
            at: pack.boundary,
            bytes: page,
            new_last: pack.boundary,
        });
    }
    let (s0, e0) = chunks[0];
    let (s1, e1) = chunks[1];
    let tail_off = table.alloc_page(body, alloc)?;
    let mut tail = [0u8; SH_PAGE_SIZE];
    sh_page_pack_extent_last_fks(&mut tail, &keep[s1..e1], pack.base, pack.extent_n, 0)?;
    sh_page_set_extent_creates(&mut tail, pack.creates);
    body.write_at(tail_off, &tail)?;
    let mut mid = [0u8; SH_PAGE_SIZE];
    sh_page_pack_fks(&mut mid, &keep[s0..e0], tail_off)?;
    Ok(PreparedTail {
        at: pack.boundary,
        bytes: mid,
        new_last: tail_off,
    })
}

#[doc(hidden)]
pub fn new_extent_n(base: u64, old_n: u32, boundary: u64) -> u32 {
    if old_n == 0
        || base == 0
        || boundary < base
        || !(boundary - base).is_multiple_of(SH_PAGE_SIZE as u64)
    {
        return old_n;
    }
    let idx = (boundary - base) / SH_PAGE_SIZE as u64;
    let end =
        base.saturating_add(u64::from(old_n.saturating_sub(1)).saturating_mul(SH_PAGE_SIZE as u64));
    if boundary <= end && idx < u64::from(u32::MAX) {
        (idx as u32).saturating_add(1)
    } else {
        old_n
    }
}

#[doc(hidden)]
pub fn new_creates(old: u32, removed: u64) -> u32 {
    if old == 0 {
        0
    } else {
        old.saturating_sub(u32::try_from(removed).unwrap_or(u32::MAX))
    }
}

#[doc(hidden)]
pub fn plan_extent_suffix(
    body: &TableFile,
    last_page: u64,
    remove: &BTreeSet<u64>,
) -> Result<SuffixPlan, StoreError> {
    if remove.is_empty() {
        return Ok(SuffixPlan::Absent);
    }
    let last = read_page(body, last_page)?;
    let Some((base, old_n)) = sh_page_extent(sh_page_as_array(&last)?)? else {
        return Err(StoreError::Corrupt("scripthash extent last page not ver=2"));
    };
    let creates = sh_page_extent_creates(sh_page_as_array(&last)?);
    let mut off = last_page;
    let mut dropped_pages = Vec::new();
    let mut found = BTreeSet::new();
    let cap = remove.len().saturating_add(2);
    for _ in 0..cap {
        let page = read_page(body, off)?;
        let fks = page_fks(&page)?;
        if fks.is_empty() {
            let Some(pred) = predecessor(body, base, off)? else {
                return if found.is_empty() {
                    Ok(SuffixPlan::Absent)
                } else {
                    Ok(SuffixPlan::DropAll {
                        removed: found.len() as u64,
                    })
                };
            };
            if pred == off {
                return Err(StoreError::Corrupt("scripthash extent empty tail loops"));
            }
            dropped_pages.push(off);
            off = pred;
            continue;
        }
        let first = fks[0].0;
        let mut keep = Vec::new();
        let mut dropped_here = Vec::new();
        for fk in &fks {
            if remove.contains(&fk.0) {
                dropped_here.push(fk.0);
            } else {
                keep.push(fk.0);
            }
        }
        if let Some(max_keep) = keep.iter().copied().max() {
            if dropped_here.iter().any(|&d| d < max_keep) {
                return Ok(SuffixPlan::NotSuffix);
            }
        }
        found.extend(dropped_here.iter().copied());
        if !keep.is_empty() {
            if remove.iter().any(|r| *r < first && !found.contains(r)) {
                return Ok(SuffixPlan::NotSuffix);
            }
            if found.is_empty() {
                return Ok(SuffixPlan::Absent);
            }
            return Ok(SuffixPlan::Rewrite {
                boundary: off,
                keep,
                drop_pages: dropped_pages,
                base,
                old_n,
                creates,
                removed: found.len() as u64,
            });
        }
        dropped_pages.push(off);
        let more_below = remove.iter().any(|r| *r < first && !found.contains(r));
        if !more_below {
            return finish_dropped_page(
                body,
                base,
                old_n,
                creates,
                off,
                dropped_pages,
                found.len() as u64,
            );
        }
        let Some(pred) = predecessor(body, base, off)? else {
            return if found.is_empty() {
                Ok(SuffixPlan::Absent)
            } else {
                Ok(SuffixPlan::DropAll {
                    removed: found.len() as u64,
                })
            };
        };
        off = pred;
    }
    Ok(SuffixPlan::NotSuffix)
}

#[doc(hidden)]
pub fn finish_dropped_page(
    body: &TableFile,
    base: u64,
    old_n: u32,
    creates: u32,
    off: u64,
    dropped_pages: Vec<u64>,
    removed: u64,
) -> Result<SuffixPlan, StoreError> {
    let Some(pred) = predecessor(body, base, off)? else {
        return Ok(SuffixPlan::DropAll { removed });
    };
    let pred_page = read_page(body, pred)?;
    let pred_fks = page_fks(&pred_page)?;
    if pred_fks.is_empty() {
        return Err(StoreError::Corrupt(
            "scripthash extent predecessor page is empty",
        ));
    }
    Ok(SuffixPlan::Rewrite {
        boundary: pred,
        keep: pred_fks.iter().map(|fk| fk.0).collect(),
        drop_pages: dropped_pages,
        base,
        old_n,
        creates,
        removed,
    })
}

#[doc(hidden)]
pub fn predecessor(body: &TableFile, base: u64, page: u64) -> Result<Option<u64>, StoreError> {
    if page >= base.saturating_add(SH_PAGE_SIZE as u64)
        && (page - base).is_multiple_of(SH_PAGE_SIZE as u64)
    {
        let pred = page - SH_PAGE_SIZE as u64;
        if pred >= base {
            let prev = read_page(body, pred)?;
            if sh_page_next(sh_page_as_array(&prev)?)? == page {
                return Ok(Some(pred));
            }
        }
    }
    let mut prev = None;
    let mut cur = base;
    if cur == 0 {
        return Ok(None);
    }
    for _ in 0..100_000 {
        if cur == page {
            return Ok(prev);
        }
        let page_bytes = read_page(body, cur)?;
        let next = sh_page_next(sh_page_as_array(&page_bytes)?)?;
        if next == 0 {
            return Ok(None);
        }
        prev = Some(cur);
        cur = next;
    }
    Err(StoreError::Corrupt(
        "scripthash extent predecessor walk ran away",
    ))
}

#[doc(hidden)]
pub fn read_page(body: &TableFile, off: u64) -> Result<[u8; SH_PAGE_SIZE], StoreError> {
    let mut page = [0u8; SH_PAGE_SIZE];
    body.read_at(off, &mut page)?;
    Ok(page)
}

#[doc(hidden)]
pub fn page_fks(page: &[u8; SH_PAGE_SIZE]) -> Result<Vec<Fk>, StoreError> {
    let mut out = Vec::new();
    sh_page_entries_into(page, &mut out)?;
    Ok(out)
}

#[doc(hidden)]
pub fn arm_undo(
    table: &ScriptHashTable,
    body: &TableFile,
    alloc: &mut AllocState,
    target: u64,
    word: Option<HeadWord>,
) -> Result<UndoRec, StoreError> {
    let old_page = read_page(body, target)?;
    let image = table.alloc_page(body, alloc)?;
    body.write_at(image, &old_page)?;
    // The image and the header are different blocks. Publish SHUN only after
    // the image is durable, or recovery copies an unwritten page onto the tail.
    body.sync_data_only()?;
    let rec = UndoRec {
        target,
        image,
        word,
    };
    persist_undo(body, alloc, Some(&rec))?;
    body.sync_data_only()?;
    Ok(rec)
}

#[doc(hidden)]
pub fn restore_page(body: &TableFile, rec: &UndoRec) -> Result<(), StoreError> {
    let image = read_page(body, rec.image)?;
    body.write_at(rec.target, &image)?;
    body.sync_data_only()?;
    Ok(())
}

#[doc(hidden)]
pub fn clear_undo_keep_image(body: &TableFile, alloc: &AllocState) -> Result<(), StoreError> {
    #[cfg(debug_assertions)]
    if FAIL_CLEAR_UNDO.with(|c| c.replace(false)) {
        return Err(StoreError::Corrupt("scripthash undo clear failed"));
    }
    persist_undo(body, alloc, None)?;
    body.sync_data_only()?;
    Ok(())
}

#[doc(hidden)]
pub fn frees_old_pages(old: &ShHeadValue) -> bool {
    matches!(old, ShHeadValue::Extent { .. } | ShHeadValue::Slab { .. })
}

#[doc(hidden)]
pub fn note_batch_body(
    table: &ScriptHashTable,
    batch: &mut UnlinkBatch,
    scripthash: &[u8; 32],
    home: LocatedHome,
) {
    match home {
        LocatedHome::Main => {
            let si = table.shard_index(scripthash);
            if !batch.main_shards.contains(&si) {
                batch.main_shards.push(si);
            }
        }
        LocatedHome::Ingest | LocatedHome::SealedOvf => batch.ovf = true,
    }
}

#[doc(hidden)]
pub fn note_readable(body: &TableFile, last_page: u64) {
    #[cfg(debug_assertions)]
    if WATCH_READABLE.with(|c| c.get()) && super::collect_extent_then_tail(body, last_page).is_err()
    {
        READABLE_BROKE.with(|c| c.set(true));
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (body, last_page);
    }
}

#[doc(hidden)]
pub fn persist_undo(
    body: &TableFile,
    alloc: &AllocState,
    undo: Option<&UndoRec>,
) -> Result<(), StoreError> {
    let mut buf = vec![0u8; UNDO_AT + UNDO_LEN];
    fill_alloc_prefix(&mut buf, alloc);
    if let Some(rec) = undo {
        encode_undo(&mut buf[UNDO_AT..], rec)?;
    }
    body.write_at(crate::file::FILE_HEADER_LEN as u64, &buf)
}

#[doc(hidden)]
pub fn fill_alloc_prefix(buf: &mut [u8], state: &AllocState) {
    buf[0..4].copy_from_slice(&super::SH_ALLOC_MAGIC);
    buf[4..6].copy_from_slice(&super::SH_ALLOC_VERSION.to_le_bytes());
    buf[8..16].copy_from_slice(&state.live_count.to_le_bytes());
    buf[16..24].copy_from_slice(&state.bump.to_le_bytes());
    let mut off = 24usize;
    for h in &state.free_head {
        if off + 8 > UNDO_AT {
            break;
        }
        buf[off..off + 8].copy_from_slice(&h.to_le_bytes());
        off += 8;
    }
}

#[doc(hidden)]
pub fn encode_undo(buf: &mut [u8], rec: &UndoRec) -> Result<(), StoreError> {
    if buf.len() < UNDO_LEN {
        return Err(StoreError::Corrupt("scripthash undo record short"));
    }
    buf[..4].copy_from_slice(&UNDO_MAGIC);
    let word = rec.word.as_ref();
    buf[4] = word.map(|w| w.tag).unwrap_or(HEAD_NONE);
    buf[6..8].copy_from_slice(&word.map(|w| w.index).unwrap_or(0).to_le_bytes());
    buf[8..16].copy_from_slice(&rec.target.to_le_bytes());
    buf[16..24].copy_from_slice(&rec.image.to_le_bytes());
    buf[24..32].copy_from_slice(&word.map(|w| w.off).unwrap_or(0).to_le_bytes());
    buf[32..40].copy_from_slice(word.map(|w| &w.old).unwrap_or(&[0u8; 8]));
    Ok(())
}

#[doc(hidden)]
pub fn read_undo(body: &TableFile) -> Result<Option<UndoRec>, StoreError> {
    let mut buf = [0u8; UNDO_LEN];
    let at = crate::file::FILE_HEADER_LEN as u64 + UNDO_AT as u64;
    if body.logical_len() < at + UNDO_LEN as u64 {
        return Ok(None);
    }
    body.read_at(at, &mut buf)?;
    if buf[..4] != UNDO_MAGIC {
        return Ok(None);
    }
    let tag = buf[4];
    let index = le_u16!(buf, 6);
    let target = le_u64!(buf, 8);
    let image = le_u64!(buf, 16);
    let head_off = le_u64!(buf, 24);
    let mut old = [0u8; 8];
    old.copy_from_slice(&buf[32..40]);
    if target == 0
        || image == 0
        || !target.is_multiple_of(SH_PAGE_SIZE as u64)
        || !image.is_multiple_of(SH_PAGE_SIZE as u64)
        || image >= body.logical_len()
        || target >= body.logical_len()
    {
        return Err(StoreError::Corrupt("scripthash undo record is not a page"));
    }
    let word = if tag == HEAD_NONE {
        None
    } else {
        Some(HeadWord {
            tag,
            index,
            off: head_off,
            old,
        })
    };
    Ok(Some(UndoRec {
        target,
        image,
        word,
    }))
}
