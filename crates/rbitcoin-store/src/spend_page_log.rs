//! Open spend-slot directory and the sequential page log behind `spent.body`.
//!
//! Confirm records 8-byte slots here, including a short tail page. Each dirty
//! page is composed into one image. A checkpoint rewrites `spend.pages` to the
//! images still named and renames `spend.pages.commit` before `spend_durable`
//! advances. The directory keeps that file's offset, not a second copy of the
//! image. A covered slot, including an explicit zero, is read from the file.
//! A slot past a short image stays on `spent.body`. Fold copies the file bytes
//! onto `spent.body` after that marker, starting at byte 16 on page 0 so the
//! file header stays put. A commit that names bytes refuses a missing or short
//! image, including `spend.pages.{generation}` left when a crash lands between
//! the commit rename and the pages rename.

use crate::error::StoreError;
use crate::file::{fsync_parent_dir, write_file_synced_tmp_rename, write_synced_tmp_file};
use crate::tx_table::BODY_PAGE_SIZE;
use rbitcoin_primitives::{
    le_u16, le_u32, le_u64, schema_file_openable, SCHEMA_VERSION, STORE_MAGIC,
};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub const SPEND_PAGES_NAME: &str = "spend.pages";
pub const SPEND_PAGES_COMMIT_NAME: &str = "spend.pages.commit";

/// Open slot deltas that make the next confirm insert wait for a checkpoint.
pub const OPEN_EDGE_CAP: u64 = 2_000_000;
/// Distinct open pages that ask for another checkpoint until the set drains.
///
/// One dirty page becomes a full image at compose. This budget asks for an
/// earlier checkpoint. Confirm still waits on the edge cap, not on this count.
pub const OPEN_PAGE_BUDGET: u64 = 65_536;
/// On-disk `spend.pages` length that asks for a checkpoint before the ten-minute timer.
pub const LOG_IMAGE_CAP: u64 = 1024 * 1024 * 1024;

const SLOT: usize = 8;
const COMMIT_HEAD: usize = 28;
const COMMIT_REC: usize = 24;
/// One bit per slot on a 4 KiB spend page (512 slots).
pub const OPEN_SLOT_PRESENCE_BYTES: usize = 64;

/// True when open-page slot `rel` is named in `present`.
///
/// `rel` past the 512 slots on a page is absent. A committed image is not
/// this index: the image itself is the slot array.
#[doc(hidden)]
pub fn open_slot_present(present: &[u64; OPEN_SLOT_PRESENCE_BYTES / 8], rel: u16) -> bool {
    let rel = rel as usize;
    if rel >= OPEN_SLOT_PRESENCE_BYTES * 8 {
        return false;
    }
    present[rel / 64] & (1u64 << (rel % 64)) != 0
}

fn presence_from(slots: &[(u16, [u8; SLOT])]) -> [u64; OPEN_SLOT_PRESENCE_BYTES / 8] {
    let mut present = [0u64; OPEN_SLOT_PRESENCE_BYTES / 8];
    for &(rel, _) in slots {
        let rel = rel as usize;
        if rel >= OPEN_SLOT_PRESENCE_BYTES * 8 {
            continue;
        }
        present[rel / 64] |= 1u64 << (rel % 64);
    }
    present
}

/// True when a spend checkpoint should run before the ten-minute timer.
pub fn spend_checkpoint_early(open_edges: u64, open_pages: u64, image_bytes: u64) -> bool {
    open_edges >= OPEN_EDGE_CAP || open_pages >= OPEN_PAGE_BUDGET || image_bytes >= LOG_IMAGE_CAP
}

/// True when a new note should wake the sync thread.
///
/// The image cap arms once. After that follow-up, a still-large file does
/// not arm again. The edge cap and the page budget still do.
pub fn spend_note_arms(
    open_edges: u64,
    open_pages: u64,
    image_bytes: u64,
    followup_done: bool,
) -> bool {
    open_edges >= OPEN_EDGE_CAP
        || open_pages >= OPEN_PAGE_BUDGET
        || (image_bytes >= LOG_IMAGE_CAP && !followup_done)
}

/// `(request another checkpoint, follow-up already used for this oversized log)`.
///
/// Crossing the image cap schedules one extra checkpoint so the generation
/// that filled the file can cool and fold. It does not spin while the file
/// stays large. The edge cap and the page budget keep requesting until the
/// open directory drains.
pub fn pace_after_checkpoint(
    open_edges: u64,
    open_pages: u64,
    image_bytes: u64,
    followup_done: bool,
) -> (bool, bool) {
    if open_edges >= OPEN_EDGE_CAP || open_pages >= OPEN_PAGE_BUDGET {
        return (true, followup_done);
    }
    if image_bytes >= LOG_IMAGE_CAP && !followup_done {
        return (true, true);
    }
    (false, image_bytes >= LOG_IMAGE_CAP)
}

fn open_page_count(g: &Inner) -> u64 {
    (g.open.len() + g.later.len()) as u64
}

/// `(page_off, log_off, image_len)` in one commit record.
type PageRec = (u64, u64, u32);
type OpenSlot = (u16, [u8; SLOT]);
type PageSlots = Vec<OpenSlot>;
type KeptPage = (u64, PageSlots);
/// Condvar `rbtc-spend-sync` waits on. The flag is the thread's stop bit.
pub type SpendSyncWake = Arc<(Mutex<bool>, Condvar)>;

/// Where an immutable generation file holds one page image.
///
/// `generation` is the file. `admitted` on the committed entry is the
/// checkpoint that made the page cool, and stays put when a later file
/// carries the same bytes forward.
#[derive(Clone, Copy, Debug)]
struct PageLoc {
    generation: u32,
    log_off: u64,
    image_len: u32,
}

impl PageLoc {
    fn covers(self, rel: u16) -> bool {
        let end = u64::from(rel).saturating_add(1).saturating_mul(SLOT as u64);
        end <= u64::from(self.image_len)
    }

    fn slot_at(self, rel: u16) -> u64 {
        self.log_off
            .saturating_add(u64::from(rel).saturating_mul(SLOT as u64))
    }
}

#[derive(Clone)]
struct OpenPage {
    /// Sorted by rel. [`OpenPage::hit`] searches with `partition_point`.
    slots: Vec<(u16, [u8; SLOT])>,
    /// Bit `rel` is set when `slots` names that slot.
    present: [u64; OPEN_SLOT_PRESENCE_BYTES / 8],
    /// Published image this note was taken against. Absent while a frozen
    /// generation or a held filter owns the page, so those notes win.
    base: Option<PageLoc>,
}

fn open_page(slots: Vec<(u16, [u8; SLOT])>, base: Option<PageLoc>) -> OpenPage {
    let mut page = OpenPage {
        slots,
        present: [0; OPEN_SLOT_PRESENCE_BYTES / 8],
        base,
    };
    page.reindex();
    page
}

impl OpenPage {
    fn reindex(&mut self) {
        self.slots.sort_by_key(|(rel, _)| *rel);
        self.present = presence_from(&self.slots);
    }

    fn hit(&self, rel: u16) -> Option<[u8; SLOT]> {
        if open_slot_present(&self.present, rel) {
            let i = self.slots.partition_point(|(r, _)| *r < rel);
            return self
                .slots
                .get(i)
                .filter(|(r, _)| *r == rel)
                .map(|(_, meta)| *meta);
        }
        None
    }
}

/// A slot `keep` rejected from an image that may already have been copied
/// onto `spent.body`.
struct BodyClear {
    page_off: u64,
    rel: u16,
    meta: [u8; SLOT],
}

#[derive(Clone, Copy)]
struct CommittedPage {
    loc: PageLoc,
    admitted: u32,
}

type SnapshotPages = Vec<(u64, OpenPage)>;

struct PendingInstall {
    generation: u32,
    height: u32,
    pages: Vec<PageRec>,
    rejected: Vec<u64>,
    kept: Vec<KeptPage>,
    /// New file offsets for pages this generation carried forward.
    carried_off: Vec<(u64, u64)>,
    /// Committed pages this publish replaces. Applied under the utxo view.
    drop_offs: Vec<u64>,
    /// When set, install keeps these committed pages plus the new images.
    keep_only: Option<Vec<u64>>,
    /// A deferred publish clears the hold only when no later disconnect bumped it.
    clear_hold: bool,
    /// `hold_ticks` sampled before `keep`. A later bump wins over `clear_hold`.
    hold_ticks: u64,
}

struct CommitSnapshot {
    pages: Vec<(u64, OpenPage)>,
}

/// Freeze captured while the caller held the utxo view.
enum StagedFreeze {
    /// `open` was empty. Nothing new to publish.
    Empty,
    Pages(CommitSnapshot),
}

/// The callback records a snapshot. The freeze before the cooled copy only swaps.
enum FreezeRecord {
    Snapshot,
    Swap,
}

/// `Clear` writes the commit with the held bit off unless a hold landed
/// during the image stream. RAM still has the old generation until that
/// rename succeeds.
enum PublishHold {
    Sample,
    Clear,
}

/// What install does with the previous directory after the commit is renamed.
struct PublishPlan {
    hold: PublishHold,
    drop_offs: Vec<u64>,
    keep_only: Option<Vec<u64>>,
    clear_hold: bool,
    hold_ticks: u64,
}

struct Inner {
    /// Slots confirm has noted since the last freeze.
    open: HashMap<u64, OpenPage>,
    /// Notes taken while a held commit is filtered. They belong to a later
    /// checkpoint: this one already sampled its height.
    later: HashMap<u64, OpenPage>,
    /// `later` is the directory `note_slots` writes until the snapshot is copied.
    holding_filter: bool,
    /// The generation the sync thread is composing. Readers still see it.
    frozen: HashMap<u64, OpenPage>,
    committed: HashMap<u64, CommittedPage>,
    /// Open generation files. The fd stays valid across the rename onto
    /// `spend.pages` and a later unlink. Readers pread it outside this lock.
    gens: HashMap<u32, Arc<File>>,
    /// Pages copied onto `spent.body` and not yet dropped from the directory.
    cooling: Vec<(u64, u32)>,
    pending: Option<PendingInstall>,
    generation: u32,
    /// Fold copies a committed page only when its generation is at or below this.
    cool_through: u32,
    /// Snapshot height named by the on-disk commit, when one is loaded.
    commit_height: Option<u32>,
    /// Snapshot height of the compose in `prepare_commit`. Cleared when that
    /// call returns. A disconnect can see it before the commit is installed.
    compose_height: Option<u32>,
    /// Commit height was above the tip. Fold must not copy that image.
    fold_held: bool,
    /// Confirmed tip sampled when a checkpoint froze. The snapshot height can
    /// sit below spends already in that image. Written after the page list.
    sealed_tip: Option<u32>,
    /// `spent.body` length sampled at the freeze. Unset means the caller’s
    /// argument. A note between that argument and the freeze can sit past it.
    published_at_freeze: Option<u64>,
    /// Snapshot taken while the caller held the utxo view.
    /// Absent after `before_freeze` returns is corrupt. `prepare_commit`
    /// does not freeze a second time.
    staged_freeze: Option<StagedFreeze>,
    /// Bumped each time a disconnect holds the fold.
    hold_ticks: u64,
    open_edges: u64,
    log_bytes: u64,
}

struct SpendPace {
    early: AtomicBool,
    image_followup: AtomicBool,
    sync_wake: Mutex<Option<SpendSyncWake>>,
    room: Condvar,
    room_mu: Mutex<()>,
}

impl SpendPace {
    fn new() -> Self {
        Self {
            early: AtomicBool::new(false),
            image_followup: AtomicBool::new(false),
            sync_wake: Mutex::new(None),
            room: Condvar::new(),
            room_mu: Mutex::new(()),
        }
    }

    fn arm(&self) {
        self.early.store(true, Ordering::Release);
        let wake = self
            .sync_wake
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(wake) = wake else {
            return;
        };
        let (lock, cv) = &*wake;
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        cv.notify_one();
    }

    fn notify_room(&self) {
        let _g = self.room_mu.lock().unwrap_or_else(|e| e.into_inner());
        self.room.notify_all();
    }
}

pub struct SpendPageLog {
    dir: PathBuf,
    inner: Mutex<Inner>,
    /// Serializes `spend.pages.commit` renames and the unlink of a finished
    /// generation. Hold, publish, and delete take this lock before the
    /// directory lock.
    commit_io: Mutex<()>,
    pace: Arc<SpendPace>,
}

/// True when the 8-byte slot lies inside the published body, including the
/// short tail page. Confirm logs that slot. A slot past `published` is cold.
pub fn is_historical(published: u64, abs: u64) -> bool {
    abs.saturating_add(SLOT as u64) <= published
}

impl SpendPageLog {
    pub fn create(dir: &Path) -> Self {
        Self::empty(dir)
    }

    pub fn load(dir: &Path) -> Result<Self, StoreError> {
        let log = Self::empty(dir);
        log.reload(None)?;
        Ok(log)
    }

    /// Load a commit, then drop pages at or past `body_end`.
    ///
    /// Open repairs `spent.body` before this log. A commit composed against
    /// the longer file names pages the repair removed. Those records are
    /// gone, not corrupt. A page that straddles `body_end` keeps the prefix.
    /// A drop or a shorter image is written back to `spend.pages.commit`
    /// before this returns. The image file stays. A crash before that rename
    /// leaves the old commit, and the next open clips it again.
    #[doc(hidden)]
    pub fn load_clipped(dir: &Path, body_end: u64) -> Result<Self, StoreError> {
        let log = Self::empty(dir);
        log.reload(Some(body_end))?;
        Ok(log)
    }

    fn empty(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            commit_io: Mutex::new(()),
            inner: Mutex::new(Inner {
                open: HashMap::new(),
                later: HashMap::new(),
                holding_filter: false,
                frozen: HashMap::new(),
                committed: HashMap::new(),
                gens: HashMap::new(),
                cooling: Vec::new(),
                pending: None,
                generation: 0,
                cool_through: 0,
                commit_height: None,
                compose_height: None,
                fold_held: false,
                sealed_tip: None,
                published_at_freeze: None,
                staged_freeze: None,
                hold_ticks: 0,
                open_edges: 0,
                log_bytes: 0,
            }),
            pace: Arc::new(SpendPace::new()),
        }
    }

    /// `rbtc-spend-sync` waits on `wake`. An edge or image cap notifies it.
    pub fn bind_sync_wake(&self, wake: SpendSyncWake) {
        *self
            .pace
            .sync_wake
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(wake);
    }

    #[doc(hidden)]
    pub fn checkpoint_early(&self) -> bool {
        self.pace.early.load(Ordering::Acquire)
    }

    /// After a checkpoint, arm one follow-up when the log is still over the
    /// image cap, and keep arming while the open directory is over the edge
    /// cap or at the page budget.
    #[doc(hidden)]
    pub fn relax_pace(&self) {
        let (edges, pages, images) = {
            let g = self.lock();
            (g.open_edges, open_page_count(&g), g.log_bytes)
        };
        let follow = self.pace.image_followup.load(Ordering::Acquire);
        let (early, follow) = pace_after_checkpoint(edges, pages, images, follow);
        self.pace.image_followup.store(follow, Ordering::Release);
        self.pace.early.store(early, Ordering::Release);
        if early {
            self.pace.arm();
        }
    }

    /// Open slot deltas [`Self::wait_open_edge_room`] waits on.
    #[doc(hidden)]
    pub fn open_edge_count(&self) -> u64 {
        self.lock().open_edges
    }

    /// Block while the open directory is still at the edge cap.
    ///
    /// Returns at once when no sync thread is bound. Startup replay runs
    /// before that thread exists, and nothing else would wake this wait.
    /// Confirm calls this before it takes the utxo view. `note_slots` does
    /// not wait, so one batch may pass the cap and the next batch waits
    /// outside the view. The page-log lock is not held across the wait.
    /// A freeze wakes this wait, including when the caller then fails.
    pub fn wait_open_edge_room(&self) {
        loop {
            let room = self.pace.room_mu.lock().unwrap_or_else(|e| e.into_inner());
            if self.lock().open_edges < OPEN_EDGE_CAP {
                return;
            }
            let bound = self
                .pace
                .sync_wake
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            if !bound {
                return;
            }
            self.pace.arm();
            drop(self.pace.room.wait(room).unwrap_or_else(|e| e.into_inner()));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Heap bytes of page images retained in the directory.
    ///
    /// Installed pages are coordinates into an immutable generation file.
    /// The file is read on demand and dropped. Nothing here retains the image.
    #[doc(hidden)]
    pub fn resident_image_bytes(&self) -> u64 {
        0
    }

    fn pages_path(&self) -> PathBuf {
        self.dir.join(SPEND_PAGES_NAME)
    }

    /// Current 8-byte image for `abs`, from the open directory, the frozen
    /// generation, or a committed page. `None` means the body still holds the slot.
    ///
    /// A covered slot is eight bytes of the generation file. A batch of slots
    /// on one page reads that image once in [`Self::slots_at`].
    /// [`Self::covers`] answers the same directory question without that read.
    pub fn slot(&self, abs: u64) -> Option<[u8; SLOT]> {
        #[cfg(debug_assertions)]
        if crate::io_handle::hidden_after_pread() {
            return None;
        }
        let page_off = abs & !(BODY_PAGE_SIZE - 1);
        let rel = ((abs - page_off) / SLOT as u64) as u16;
        let (file, loc) = {
            let g = self.lock();
            match covered_slot(&g, page_off, rel)? {
                Covered::Noted(meta) => return Some(meta),
                Covered::File(loc) => (generation_file(&g, loc.generation), loc),
            }
        };
        let mut buf = [0u8; SLOT];
        pread_generation(&file, &self.pages_path(), loc.slot_at(rel), &mut buf);
        Some(buf)
    }

    /// True when the directory serves `abs`, including an explicit zero.
    ///
    /// This does not read the generation file. [`Self::slot`] loads the bytes.
    pub fn covers(&self, abs: u64) -> bool {
        #[cfg(debug_assertions)]
        if crate::io_handle::hidden_after_pread() {
            return false;
        }
        let page_off = abs & !(BODY_PAGE_SIZE - 1);
        let rel = ((abs - page_off) / SLOT as u64) as u16;
        let g = self.lock();
        covered_slot(&g, page_off, rel).is_some()
    }

    /// Same bytes as [`Self::slot`] for each offset. One directory lock for
    /// the whole slice. Each covered page is read once, then dropped. The
    /// caller reads `spent.body` for the misses after this returns.
    pub fn slots_at(&self, abs: &[u64]) -> Vec<Option<[u8; SLOT]>> {
        #[cfg(debug_assertions)]
        if crate::io_handle::hidden_after_pread() {
            return vec![None; abs.len()];
        }
        if abs.is_empty() {
            return Vec::new();
        }
        struct PendingRead {
            i: usize,
            rel: u16,
            loc: PageLoc,
        }
        let mut order: Vec<(u64, u16, usize)> = abs
            .iter()
            .enumerate()
            .map(|(i, &off)| {
                let page_off = off & !(BODY_PAGE_SIZE - 1);
                let rel = ((off - page_off) / SLOT as u64) as u16;
                (page_off, rel, i)
            })
            .collect();
        order.sort_unstable_by_key(|(page, _, i)| (*page, *i));
        let mut out = vec![None; abs.len()];
        let mut reads: Vec<PendingRead> = Vec::new();
        let mut files: HashMap<u32, Arc<File>> = HashMap::new();
        {
            let g = self.lock();
            let mut start = 0;
            while start < order.len() {
                let page_off = order[start].0;
                let mut end = start + 1;
                while end < order.len() && order[end].0 == page_off {
                    end += 1;
                }
                for &(_, rel, i) in &order[start..end] {
                    match covered_slot(&g, page_off, rel) {
                        Some(Covered::Noted(meta)) => out[i] = Some(meta),
                        Some(Covered::File(loc)) => {
                            files
                                .entry(loc.generation)
                                .or_insert_with(|| generation_file(&g, loc.generation));
                            reads.push(PendingRead { i, rel, loc });
                        }
                        None => {}
                    }
                }
                start = end;
            }
        }
        reads.sort_by_key(|read| (read.loc.generation, read.loc.log_off, read.i));
        let path = self.pages_path();
        let mut start = 0;
        while start < reads.len() {
            let loc = reads[start].loc;
            let mut end = start + 1;
            while end < reads.len()
                && reads[end].loc.generation == loc.generation
                && reads[end].loc.log_off == loc.log_off
            {
                end += 1;
            }
            let image = read_loc_bytes(&files[&loc.generation], &path, loc);
            for read in &reads[start..end] {
                out[read.i] = slot_in_image(&image, read.rel);
            }
            start = end;
        }
        out
    }

    pub fn note_slots(&self, slots: &[(u64, [u8; SLOT])]) -> Result<(), StoreError> {
        if slots.is_empty() {
            return Ok(());
        }
        let mut added = false;
        {
            let mut g = self.lock();
            for &(abs, meta) in slots {
                let page_off = abs & !(BODY_PAGE_SIZE - 1);
                let rel = ((abs - page_off) / SLOT as u64) as u16;
                if (rel as usize) >= (BODY_PAGE_SIZE as usize / SLOT) {
                    return Err(StoreError::Corrupt("spend page slot past page"));
                }
                let base = if g.holding_filter || g.frozen.contains_key(&page_off) {
                    None
                } else {
                    g.committed.get(&page_off).map(|c| c.loc)
                };
                let pages = if g.holding_filter {
                    &mut g.later
                } else {
                    &mut g.open
                };
                if insert_note(pages, page_off, rel, meta, base)? {
                    g.open_edges = g.open_edges.saturating_add(1);
                    added = true;
                }
            }
            let follow = self.pace.image_followup.load(Ordering::Acquire);
            let pages = open_page_count(&g);
            added = added && spend_note_arms(g.open_edges, pages, g.log_bytes, follow);
        }
        if added {
            self.pace.arm();
        }
        Ok(())
    }

    /// Remember the confirmed tip this checkpoint is freezing.
    ///
    /// A snapshot height can be below blocks whose spends are already in the
    /// open directory. A later disconnect below this tip holds the fold even
    /// when the commit file still names the older snapshot.
    pub fn cover_confirmed_tip(&self, tip: u32) {
        let mut g = self.lock();
        if g.fold_held {
            g.sealed_tip = Some(g.sealed_tip.map(|prev| prev.max(tip)).unwrap_or(tip));
        } else {
            g.sealed_tip = Some(tip);
        }
    }

    /// Body length the freeze saw. Compose uses this instead of an earlier sample.
    #[doc(hidden)]
    pub fn cover_published_len(&self, published: u64) {
        self.lock().published_at_freeze = Some(published);
    }

    fn sealed_tip_or(&self, height: u32) -> u32 {
        self.lock().sealed_tip.unwrap_or(height)
    }

    /// Raise the sealed tip. A drop during the keep must not lower it.
    fn raise_sealed_tip(&self, tip: u32) {
        let mut g = self.lock();
        g.sealed_tip = Some(g.sealed_tip.map(|prev| prev.max(tip)).unwrap_or(tip));
    }

    /// A loaded commit above `height` must not be folded. The held bit may
    /// still be clear. Call this before [`Self::widen_cool_through`].
    pub fn hold_loaded_commit_above(&self, height: u32) {
        let mut g = self.lock();
        if g.commit_height.is_some_and(|committed| committed > height) {
            g.fold_held = true;
        }
    }

    /// Hold a commit above `height`, then widen the cool range.
    ///
    /// The quiet-page fold runs after this returns. A held generation stays.
    pub fn widen_for_checkpoint(&self, height: u32) {
        self.hold_loaded_commit_above(height);
        self.widen_cool_through();
    }

    /// True when a disconnect to `tip` must hold the fold.
    ///
    /// The commit height and the freeze tip both count. The spend marker can
    /// still be at or below `tip`.
    pub fn above_tip(&self, tip: u32) -> bool {
        let g = self.lock();
        g.commit_height.is_some_and(|h| h > tip) || g.sealed_tip.is_some_and(|h| h > tip)
    }

    /// Fold quiet pages from the previous generation before the next compose
    /// copies them into a new `spend.pages` file.
    ///
    /// A held generation stays. A compose still in flight stays. Callers fold
    /// after this returns.
    pub fn widen_cool_through(&self) {
        let mut g = self.lock();
        if g.fold_held || g.compose_height.is_some() || g.pending.is_some() {
            return;
        }
        if g.generation > g.cool_through {
            g.cool_through = g.generation;
        }
    }

    /// An installed commit, a compose still in flight, a publish waiting to
    /// be installed, or a freeze tip names a height above `tip`. Keep serving
    /// those slots, and do not fold them. Only an installed commit height is
    /// lowered to `tip`. A freeze tip above `tip` sets the hold without
    /// rewriting that height. The RAM hold is stored before the commit file
    /// is patched, so a failed read or rename still blocks the fold.
    pub fn hold_fold_above(&self, tip: u32) -> Result<(), StoreError> {
        let _io = self.commit_io.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.dir.join(SPEND_PAGES_COMMIT_NAME);
        let lower_height = {
            let mut g = self.lock();
            let installed = g.commit_height.is_some_and(|h| h > tip);
            let in_flight = g.compose_height.is_some_and(|h| h > tip)
                || g.pending.as_ref().is_some_and(|p| p.height > tip);
            let sealed = g.sealed_tip.is_some_and(|h| h > tip);
            if !installed && !in_flight && !sealed {
                return Ok(());
            }
            if installed {
                g.commit_height = Some(tip);
            }
            g.fold_held = true;
            g.hold_ticks = g.hold_ticks.saturating_add(1);
            installed
        };
        if !path.exists() {
            return Ok(());
        }
        let mut buf = std::fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
        if buf.len() < COMMIT_HEAD {
            return Ok(());
        }
        buf[6..8].copy_from_slice(&1u16.to_le_bytes());
        if lower_height {
            buf[8..12].copy_from_slice(&tip.to_le_bytes());
        }
        // The hold is already stored. A directory sync failure after the
        // rename stays success. A read or rename error still returns.
        write_commit_file(&path, &buf)?;
        Ok(())
    }

    /// Compose, publish, and install one generation.
    ///
    /// Callers that must hold the utxo view across the directory swap use
    /// [`Self::prepare_commit`] and [`Self::install_commit`] instead.
    pub fn commit_open<F, K>(
        &self,
        height: u32,
        published: u64,
        read_page: F,
        keep: K,
    ) -> Result<(), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
        K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
    {
        self.prepare_commit(height, published, read_page, keep)?;
        self.install_commit()
    }

    /// Compose open pages into one generation and publish the commit record.
    ///
    /// `read_page` supplies neighbor bytes from `spent.body` when the page has
    /// no base image. `keep` drops a slot that is no longer best-chain so a
    /// later fold does not write it. An empty open set leaves an existing
    /// commit in place, unless that commit's height is above this checkpoint.
    /// A restart can load that height with the held bit still clear. Those
    /// slots are filtered before an empty open set marks the image cool.
    ///
    /// A frozen snapshot left by a failed compose is retried together with
    /// every note sitting in `open`. A page in both directories merges slots,
    /// and the later note wins on the same slot. Notes that arrive after this
    /// snapshot is copied stay in `open` for the next generation.
    ///
    /// A later generation rewrites `spend.pages` to the images still named.
    /// The previous file stays until that commit is renamed. A crash between
    /// the commit rename and the pages rename reloads `spend.pages.{generation}`.
    /// A page whose new notes are all rejected still names that older image.
    #[doc(hidden)]
    pub fn prepare_commit<F, K>(
        &self,
        height: u32,
        published: u64,
        read_page: F,
        keep: K,
    ) -> Result<(), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
        K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
    {
        self.prepare_commit_in(
            height,
            published,
            read_page,
            keep,
            |log| {
                log.freeze_open();
                Ok(())
            },
            |log| Ok(log.sealed_tip_or(height)),
        )
    }

    /// Freeze `open` while the caller holds the utxo view.
    ///
    /// [`Self::prepare_commit_in`] publishes this snapshot. A note that arrives
    /// after the view is released waits for the next checkpoint.
    #[doc(hidden)]
    pub fn freeze_open(&self) {
        let staged = match self.freeze_directory(FreezeRecord::Snapshot) {
            Some(pages) => StagedFreeze::Pages(pages),
            None => StagedFreeze::Empty,
        };
        self.lock().staged_freeze = Some(staged);
    }

    /// Move `open` into `frozen` and wake an edge waiter before the cooled copy.
    ///
    /// Does not stamp the confirmed tip. A disconnect below the checkpoint
    /// height, and at or above the generation already sealed, must still fold
    /// that generation. Does not clone a snapshot: the callback freeze records
    /// the one compose publishes. An empty open set still advances
    /// `cool_through` and leaves `frozen` empty, so that callback freeze is a
    /// fresh swap and notifies.
    #[doc(hidden)]
    pub fn freeze_before_cooled_copy(&self) {
        let _ = self.freeze_directory(FreezeRecord::Swap);
        self.lock().staged_freeze = None;
    }

    #[doc(hidden)]
    pub fn prepare_commit_in<F, K, B, S>(
        &self,
        height: u32,
        published: u64,
        mut read_page: F,
        mut keep: K,
        before_freeze: B,
        seal_tip: S,
    ) -> Result<(), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
        K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
        B: FnOnce(&Self) -> Result<(), StoreError>,
        S: FnOnce(&Self) -> Result<u32, StoreError>,
    {
        if self.lock().pending.is_some() {
            return Ok(());
        }
        self.lock().published_at_freeze = None;
        let explode_published = published;
        let sampled_ticks = {
            let mut g = self.lock();
            if g.commit_height.is_some_and(|committed| committed > height) {
                g.fold_held = true;
            }
            g.compose_height = Some(height);
            // Notes taken while `keep` walks the held image are not part of
            // this height. The walk does not hold the utxo view, so a
            // replacement block can land here.
            if g.fold_held {
                g.holding_filter = true;
            }
            g.hold_ticks
        };
        let _clear_compose = ClearCompose(self);
        let _release_later = ReleaseLater(self);
        let mut clears = self.explode_held(explode_published, sampled_ticks, &mut keep)?;
        // Every slot was rejected. The commit file is still the retry record
        // until compose publishes a replacement or decides the body has none.
        let deferred = {
            let g = self.lock();
            g.fold_held && !g.committed.is_empty()
        };
        // The caller covers the freeze tip and freezes `open` while it holds
        // the utxo view. Notes taken during the quiet fold are already in
        // `open`. A note after this waits for the next checkpoint.
        before_freeze(self)?;
        let published = {
            let mut g = self.lock();
            g.published_at_freeze.take().unwrap_or(published)
        };
        let staged = {
            let mut g = self.lock();
            g.staged_freeze.take()
        };
        let Some(staged) = staged else {
            return Err(StoreError::Corrupt("spend freeze was not staged"));
        };
        let snap = match staged {
            StagedFreeze::Empty => None,
            StagedFreeze::Pages(snap) => Some(snap),
        };
        let Some(snap) = snap else {
            self.scrub_open_notes(&mut keep)?;
            return self.settle_deferred_clear(
                height,
                published,
                &[],
                &clears,
                deferred,
                sampled_ticks,
                seal_tip,
                &mut read_page,
            );
        };
        let (mut kept, mut rejected, more) = filter_kept_pages(&mut keep, snap.pages)?;
        clears.extend(more);
        let composed = self.pages_that_compose(&mut read_page, published, &kept, &clears)?;
        let composed_set: HashSet<u64> = composed.iter().copied().collect();
        let unpublished: Vec<u64> = kept
            .iter()
            .map(|(off, _)| *off)
            .filter(|off| !composed_set.contains(off))
            .collect();
        kept.retain(|(off, _)| composed_set.contains(off));
        rejected.extend(unpublished);
        if composed.is_empty() {
            self.drop_frozen_pages(&rejected);
            return Ok(());
        }
        self.scrub_open_notes(&mut keep)?;
        self.raise_sealed_tip(seal_tip(self)?);
        let mut replaced: Vec<u64> = clears.iter().map(|clear| clear.page_off).collect();
        replaced.extend(kept.iter().map(|(off, _)| *off));
        let carried = {
            let g = self.lock();
            let mut carried = carried_pages(&g.committed, &kept);
            if deferred {
                let skip: HashSet<u64> = replaced.iter().copied().collect();
                carried.retain(|(off, _)| !skip.contains(off));
            }
            carried.retain(|(off, _)| *off < published);
            carried
        };
        let (clear_hold, hold) = self.publish_hold(deferred, sampled_ticks);
        self.store_pending(
            height,
            published,
            &composed,
            &kept,
            &clears,
            rejected,
            &carried,
            PublishPlan {
                hold,
                drop_offs: if deferred { replaced } else { Vec::new() },
                keep_only: None,
                clear_hold,
                hold_ticks: sampled_ticks,
            },
            &mut read_page,
        )?;
        Ok(())
    }

    /// Publish a clearing image, or drop a rejected generation the body does not hold.
    ///
    /// A page this checkpoint did not reject stays named. Deleting the file, or
    /// keeping only a zero page, would serve that best-chain spend as unspent.
    /// The commit file stays in place until this returns. A `read_page` error
    /// never reaches here, so a restart still loads the old image.
    #[allow(clippy::too_many_arguments)] // the seal runs only when this publishes
    fn settle_deferred_clear<F, S>(
        &self,
        height: u32,
        published: u64,
        kept: &[(u64, OpenPage)],
        clears: &[BodyClear],
        deferred: bool,
        sampled_ticks: u64,
        seal_tip: S,
        read_page: &mut F,
    ) -> Result<(), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
        S: FnOnce(&Self) -> Result<u32, StoreError>,
    {
        let composed = self.pages_that_compose(read_page, published, kept, clears)?;
        let fresh: HashSet<u64> = composed.iter().copied().collect();
        let cleared: HashSet<u64> = clears.iter().map(|clear| clear.page_off).collect();
        let mut carried = if deferred {
            self.carry_untouched(&fresh, &cleared)
        } else {
            Vec::new()
        };
        carried.retain(|(off, _)| *off < published);
        if composed.is_empty() && carried.is_empty() {
            if deferred {
                self.drop_rejected_generation();
            }
            return Ok(());
        }
        self.raise_sealed_tip(seal_tip(self)?);
        let (clear_hold, hold) = self.publish_hold(deferred, sampled_ticks);
        self.store_pending(
            height,
            published,
            &composed,
            kept,
            clears,
            Vec::new(),
            &carried,
            PublishPlan {
                hold,
                drop_offs: Vec::new(),
                keep_only: deferred.then(|| carried.iter().map(|(off, _)| *off).collect()),
                clear_hold,
                hold_ticks: sampled_ticks,
            },
            read_page,
        )?;
        Ok(())
    }

    /// `Clear` only when this deferred publish has not seen a disconnect since
    /// `keep` started. The file bit and the RAM flag both follow that choice,
    /// and install applies the RAM clear.
    fn publish_hold(&self, deferred: bool, sampled_ticks: u64) -> (bool, PublishHold) {
        let clear_hold = deferred && self.lock().hold_ticks == sampled_ticks;
        let hold = if clear_hold {
            PublishHold::Clear
        } else {
            PublishHold::Sample
        };
        (clear_hold, hold)
    }

    fn forget_held_directory(&self) {
        let mut g = self.lock();
        g.committed.clear();
        g.fold_held = false;
        g.commit_height = None;
    }

    fn drop_rejected_generation(&self) {
        self.forget_held_directory();
        self.delete_generation_files();
    }

    fn committed_locs(&self) -> HashMap<u64, PageLoc> {
        let g = self.lock();
        g.committed
            .iter()
            .map(|(off, page)| (*off, page.loc))
            .collect()
    }

    fn read_loc(&self, loc: PageLoc) -> Vec<u8> {
        let file = {
            let g = self.lock();
            generation_file(&g, loc.generation)
        };
        read_loc_bytes(&file, &self.pages_path(), loc)
    }

    /// Committed pages this checkpoint did not recompose and did not reject.
    fn carry_untouched(
        &self,
        fresh: &HashSet<u64>,
        cleared: &HashSet<u64>,
    ) -> Vec<(u64, CommittedPage)> {
        let g = self.lock();
        let mut out = Vec::new();
        for (off, page) in &g.committed {
            if fresh.contains(off) || cleared.contains(off) {
                continue;
            }
            out.push((*off, *page));
        }
        out
    }

    /// True when every open note is already in the image this generation wrote.
    ///
    /// The snapshot merges `frozen` with notes taken during the cooled copy,
    /// so `open` can be a subset of the kept slots.
    fn open_notes_are_kept(slots: &[OpenSlot], kept: Option<&HashSet<OpenSlot>>) -> bool {
        match kept {
            Some(set) => slots.iter().all(|slot| set.contains(slot)),
            None => slots.is_empty(),
        }
    }

    /// Publish the composed generation into the directory.
    ///
    /// Call under `hold_utxo_view` so a reader cannot observe the swap. A hold
    /// taken while the generation was composed stays set: this does not clear it.
    #[doc(hidden)]
    pub fn install_commit(&self) -> Result<(), StoreError> {
        let pending = {
            let mut g = self.lock();
            g.pending.take()
        };
        let Some(pending) = pending else {
            return Ok(());
        };
        let PendingInstall {
            generation,
            height,
            pages,
            rejected,
            kept,
            carried_off,
            drop_offs,
            keep_only,
            clear_hold,
            hold_ticks,
        } = pending;
        let inserted: HashSet<u64> = pages.iter().map(|rec| rec.0).collect();
        let kept_by_page: HashMap<u64, HashSet<OpenSlot>> = kept
            .iter()
            .map(|(off, slots)| (*off, slots.iter().copied().collect()))
            .collect();
        let file = {
            let g = self.lock();
            generation_file(&g, generation)
        };
        let path = self.pages_path();
        let page_at: HashMap<u64, (u64, u32)> = pages
            .iter()
            .map(|&(off, log_off, image_len)| (off, (log_off, image_len)))
            .collect();
        loop {
            let snaps = {
                let g = self.lock();
                pages
                    .iter()
                    .filter_map(|&(page_off, _, _)| {
                        let page = g.open.get(&page_off)?;
                        let unchanged =
                            Self::open_notes_are_kept(&page.slots, kept_by_page.get(&page_off));
                        if unchanged {
                            None
                        } else {
                            Some((page_off, page.slots.clone()))
                        }
                    })
                    .collect::<Vec<_>>()
            };
            let mut fresh = HashMap::new();
            for (page_off, slots) in &snaps {
                let Some(&(log_off, image_len)) = page_at.get(page_off) else {
                    continue;
                };
                let loc = PageLoc {
                    generation,
                    log_off,
                    image_len,
                };
                let image = read_loc_bytes(&file, &path, loc);
                let mut got = Vec::with_capacity(slots.len());
                for &(rel, _) in slots {
                    got.push(slot_in_image(&image, rel).unwrap_or([0u8; SLOT]));
                }
                fresh.insert(*page_off, got);
            }
            let mut g = self.lock();
            let stale = snaps.iter().any(|(page_off, slots)| {
                g.open.get(page_off).is_none_or(|page| page.slots != *slots)
            });
            if stale {
                continue;
            }
            let reads = fresh;
            g.generation = generation;
            if clear_hold && g.hold_ticks == hold_ticks {
                g.fold_held = false;
            }
            if !g.fold_held {
                g.commit_height = Some(height);
                g.cool_through = generation.wrapping_sub(1);
            }
            for page_off in &rejected {
                g.frozen.remove(page_off);
            }
            for &(page_off, log_off, image_len) in &pages {
                g.frozen.remove(&page_off);
                let loc = PageLoc {
                    generation,
                    log_off,
                    image_len,
                };
                let unchanged = match g.open.get(&page_off) {
                    None => true,
                    Some(page) => {
                        Self::open_notes_are_kept(&page.slots, kept_by_page.get(&page_off))
                    }
                };
                if unchanged {
                    if let Some(page) = g.open.remove(&page_off) {
                        g.open_edges = g.open_edges.saturating_sub(page.slots.len() as u64);
                    }
                } else if let Some(page) = g.open.get_mut(&page_off) {
                    let before = page.slots.len() as u64;
                    let got = reads.get(&page_off);
                    let mut live = Vec::with_capacity(page.slots.len());
                    for (i, slot) in page.slots.iter().copied().enumerate() {
                        let differs = match got.and_then(|bytes| bytes.get(i)) {
                            Some(file_meta) => file_meta != &slot.1,
                            None => true,
                        };
                        if differs {
                            live.push(slot);
                        }
                    }
                    page.slots = live;
                    page.base = Some(loc);
                    page.reindex();
                    let left = page.slots.len() as u64;
                    g.open_edges = g.open_edges.saturating_sub(before.saturating_sub(left));
                    if left == 0 {
                        g.open.remove(&page_off);
                    }
                }
                g.committed.insert(
                    page_off,
                    CommittedPage {
                        loc,
                        admitted: generation,
                    },
                );
            }
            for (page_off, log_off) in carried_off {
                let Some(loc) = g.committed.get_mut(&page_off).map(|page| {
                    page.loc.generation = generation;
                    page.loc.log_off = log_off;
                    page.loc
                }) else {
                    continue;
                };
                retarget_base(g.open.get_mut(&page_off), loc);
                retarget_base(g.later.get_mut(&page_off), loc);
                retarget_base(g.frozen.get_mut(&page_off), loc);
            }
            if let Some(keep) = keep_only {
                let keep: HashSet<u64> = keep.into_iter().collect();
                g.committed
                    .retain(|off, _| keep.contains(off) || inserted.contains(off));
            } else if !drop_offs.is_empty() {
                let drop_offs: HashSet<u64> = drop_offs.into_iter().collect();
                g.committed
                    .retain(|off, _| !drop_offs.contains(off) || inserted.contains(off));
            }
            gc_gens(&mut g);
            break;
        }
        self.pace.notify_room();
        Ok(())
    }

    /// A disconnect left this commit above the tip. Do not copy it onto the body.
    ///
    /// `keep` runs before any directory change. A `keep` error leaves the
    /// commit file and the committed images where they were. When a page
    /// still has a rejected slot, live slots are restored into `frozen` when
    /// that set already holds a page, and into `open` otherwise. The old image
    /// stays until that compose's rename returns. A zero image is
    /// folded. An image whose slots are all still best-chain stays in
    /// `committed` and is republished from that image, not from the body.
    fn explode_held<K>(
        &self,
        published: u64,
        sampled_ticks: u64,
        keep: &mut K,
    ) -> Result<Vec<BodyClear>, StoreError>
    where
        K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
    {
        let held: Vec<(u64, PageLoc)> = {
            let g = self.lock();
            if !g.fold_held {
                return Ok(Vec::new());
            }
            g.committed
                .iter()
                .map(|(off, page)| (*off, page.loc))
                .collect()
        };
        let mut restored: Vec<KeptPage> = Vec::new();
        let mut cleared: Vec<BodyClear> = Vec::new();
        let mut live: HashSet<u64> = HashSet::new();
        for (page_off, loc) in held {
            let image = self.read_loc(loc);
            if image_has_live_spend(page_off, &image, published) {
                live.insert(page_off);
            }
            let mut slots = Vec::new();
            let n = image.len() / SLOT;
            for rel in 0..n {
                if page_off == 0 && rel < 2 {
                    continue;
                }
                if slot_past_published(page_off, rel as u16, published) {
                    continue;
                }
                let meta = slot_from_image(&image, rel as u16);
                if meta == [0u8; SLOT] {
                    continue;
                }
                if !keep(&meta)? {
                    cleared.push(BodyClear {
                        page_off,
                        rel: rel as u16,
                        meta,
                    });
                    continue;
                }
                slots.push((rel as u16, meta));
            }
            if !slots.is_empty() {
                restored.push((page_off, slots));
            }
        }
        let mut g = self.lock();
        if !g.fold_held {
            return Ok(Vec::new());
        }
        Ok(apply_held_explode(
            &mut g,
            &live,
            restored,
            cleared,
            sampled_ticks,
        ))
    }

    /// Freeze a new open set, or reuse the snapshot a failed compose left.
    ///
    /// The freeze is a swap. Confirm notes the next generation into `open`
    /// while this one is composed. A leftover frozen map stays put, and the
    /// edges those later notes added stay counted. [`FreezeRecord::Swap`]
    /// skips the page clone. The callback freeze builds the snapshot after
    /// the cooled copy.
    fn freeze_directory(&self, record: FreezeRecord) -> Option<CommitSnapshot> {
        let mut g = self.lock();
        g.holding_filter = false;
        let reuse = !g.frozen.is_empty();
        if !reuse && g.open.is_empty() {
            if !g.fold_held {
                g.cool_through = g.generation;
            }
            merge_later(&mut g, false);
            return None;
        }
        let reset_edges = if !reuse {
            g.frozen = std::mem::take(&mut g.open);
            true
        } else {
            false
        };
        let pages = if matches!(record, FreezeRecord::Snapshot) {
            let mut pages: SnapshotPages = g
                .frozen
                .iter()
                .map(|(off, page)| (*off, page.clone()))
                .collect();
            fold_open_into_snapshot(&mut pages, &g.open, &g.committed);
            Some(pages)
        } else {
            None
        };
        merge_later(&mut g, reset_edges);
        let froze = !reuse;
        drop(g);
        // Confirm may already be asleep on the edge cap. The caller can
        // fail after this freeze and never reach the install wake.
        if froze {
            self.pace.notify_room();
        }
        pages.map(|pages| CommitSnapshot { pages })
    }

    fn scrub_open_notes<K>(&self, keep: &mut K) -> Result<(), StoreError>
    where
        K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
    {
        // Slots noted during compose are not in the image. Drop ones `keep`
        // rejects before the directory swap, so a disconnected spend does not
        // become the committed image. Replace the page only after every `keep`
        // returns, so an error still leaves the note for the next checkpoint.
        let mut g = self.lock();
        let offs: Vec<u64> = g.open.keys().copied().collect();
        for off in offs {
            let Some(page) = g.open.get_mut(&off) else {
                continue;
            };
            let mut live = Vec::with_capacity(page.slots.len());
            let mut dropped = 0u64;
            for slot in &page.slots {
                if keep(&slot.1)? {
                    live.push(*slot);
                } else {
                    dropped = dropped.saturating_add(1);
                }
            }
            page.slots = live;
            page.reindex();
            let remove = page.slots.is_empty() && page.base.is_none();
            g.open_edges = g.open_edges.saturating_sub(dropped);
            if remove {
                g.open.remove(&off);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn store_pending<F>(
        &self,
        height: u32,
        published: u64,
        composed: &[u64],
        kept: &[(u64, OpenPage)],
        clears: &[BodyClear],
        mut rejected: Vec<u64>,
        carried: &[(u64, CommittedPage)],
        plan: PublishPlan,
        read_page: &mut F,
    ) -> Result<(), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
    {
        let (generation, file, recs) = self.publish_generation(
            height,
            published,
            composed,
            kept,
            clears,
            carried,
            read_page,
            plan.hold,
            plan.hold_ticks,
        )?;
        let fresh: HashSet<u64> = composed.iter().copied().collect();
        let written: HashSet<u64> = recs
            .iter()
            .map(|rec| rec.0)
            .filter(|off| fresh.contains(off))
            .collect();
        for off in composed {
            if !written.contains(off) {
                rejected.push(*off);
            }
        }
        let pages: Vec<PageRec> = recs
            .iter()
            .copied()
            .filter(|rec| fresh.contains(&rec.0))
            .collect();
        let carried_off: Vec<(u64, u64)> = recs
            .iter()
            .filter(|rec| !fresh.contains(&rec.0))
            .map(|rec| (rec.0, rec.1))
            .collect();
        let kept_slots: Vec<KeptPage> = kept
            .iter()
            .filter(|(off, _)| written.contains(off))
            .map(|(off, page)| (*off, page.slots.clone()))
            .collect();
        let mut g = self.lock();
        g.gens.insert(generation, file);
        g.pending = Some(PendingInstall {
            generation,
            height,
            pages,
            rejected,
            kept: kept_slots,
            carried_off,
            drop_offs: plan.drop_offs,
            keep_only: plan.keep_only,
            clear_hold: plan.clear_hold,
            hold_ticks: plan.hold_ticks,
        });
        Ok(())
    }

    fn drop_frozen_pages(&self, pages: &[u64]) {
        if pages.is_empty() {
            return;
        }
        let mut g = self.lock();
        for off in pages {
            g.frozen.remove(off);
        }
        drop(g);
        self.pace.notify_room();
    }

    /// Rewrite `spend.pages` to the images still named.
    ///
    /// Carried images are written first, in their previous order, then the
    /// new pages. The bytes go to `spend.pages.{generation}` and the commit
    /// names that blob before the generation file is renamed over
    /// `spend.pages`. A crash before the commit rename leaves the old commit,
    /// which does not name the generation file. A crash after it reloads that
    /// file. Once that rename succeeds, this returns Ok even when the pages
    /// rename or the directory sync fails. The next checkpoint writes the
    /// next generation instead of replacing the file the commit already
    /// names. A failed pages rename leaves that file in place. `commit_io`
    /// covers the hold sample through the commit write. The image stream
    /// stays outside that lock: `read_page` may call `hold_fold_above`,
    /// which takes `commit_io` itself.
    #[allow(clippy::too_many_arguments)]
    fn publish_generation<F>(
        &self,
        height: u32,
        published: u64,
        composed: &[u64],
        kept: &[(u64, OpenPage)],
        clears: &[BodyClear],
        carried: &[(u64, CommittedPage)],
        read_page: &mut F,
        hold: PublishHold,
        sampled_ticks: u64,
    ) -> Result<(u32, Arc<File>, Vec<PageRec>), StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
    {
        let fresh: HashSet<u64> = composed.iter().copied().collect();
        let mut carried_rows: Vec<(u64, PageLoc)> = carried
            .iter()
            .filter(|(off, _)| !fresh.contains(off))
            .map(|(off, page)| (*off, page.loc))
            .collect();
        carried_rows.sort_by_key(|(_, loc)| loc.log_off);
        let generation = self.lock().generation.wrapping_add(1);
        let gen_path = self.dir.join(format!("{SPEND_PAGES_NAME}.{generation}"));
        let pages_path = self.dir.join(SPEND_PAGES_NAME);
        let commit_path = self.dir.join(SPEND_PAGES_COMMIT_NAME);
        let mut recs: Vec<PageRec> = Vec::new();
        let mut committed_len = 0u64;
        let kept_at: HashMap<u64, &OpenPage> =
            kept.iter().map(|(off, page)| (*off, page)).collect();
        let clear_at = clears_by_page(clears);
        let stored = self.committed_locs();
        // `read_page` runs inside this write and may take `commit_io`.
        write_synced_tmp_file(&gen_path, |f| {
            use std::io::Write;
            for (page_off, loc) in &carried_rows {
                let image = self.read_loc(*loc);
                let log_off = committed_len;
                f.write_all(&image)
                    .map_err(|e| StoreError::io(&gen_path, e))?;
                committed_len = committed_len.saturating_add(image.len() as u64);
                recs.push((*page_off, log_off, image.len() as u32));
            }
            for &page_off in composed {
                let loc = compose_base(
                    kept_at.get(&page_off).copied(),
                    stored.get(&page_off).copied(),
                );
                let image = loc.map(|loc| self.read_loc(loc));
                let Some(buf) = compose_one_page(
                    read_page,
                    published,
                    page_off,
                    kept_at.get(&page_off).copied(),
                    clear_at.get(&page_off),
                    image.as_deref(),
                )?
                else {
                    continue;
                };
                let log_off = committed_len;
                f.write_all(&buf)
                    .map_err(|e| StoreError::io(&gen_path, e))?;
                committed_len = committed_len.saturating_add(buf.len() as u64);
                recs.push((page_off, log_off, buf.len() as u32));
            }
            Ok(())
        })?;
        let file = Arc::new(File::open(&gen_path).map_err(|e| StoreError::io(&gen_path, e))?);
        // Sample the hold and write the commit under one lock. A hold that
        // lands during the image stream bumps `hold_ticks`. Clear still
        // writes the held bit when those ticks differ. A hold that arrives
        // after this lock patches the commit just written.
        let _io = self.commit_io.lock().unwrap_or_else(|e| e.into_inner());
        let (held, sealed_tip) = {
            let g = self.lock();
            let held = match hold {
                PublishHold::Sample => g.fold_held,
                PublishHold::Clear => g.hold_ticks != sampled_ticks,
            };
            (held, g.sealed_tip.unwrap_or(height))
        };
        let commit = encode_commit(height, generation, committed_len, &recs, held, sealed_tip);
        let dir_synced = write_commit_file(&commit_path, &commit)?;
        {
            let mut g = self.lock();
            g.log_bytes = committed_len;
        }
        // The commit rename is already visible. Renaming this generation
        // file away before that directory entry is durable can leave the
        // old commit naming a file that is gone. The open fd keeps the inode.
        if dir_synced {
            self.finish_pages_rename(&gen_path, &pages_path);
        }
        Ok((generation, file, recs))
    }

    fn delete_generation_files(&self) {
        // A hold that already sampled a live compose will rename this commit
        // back if it runs between the two unlinks. Keep the file names and the
        // RAM height behind the same lock the hold uses.
        let _io = self.commit_io.lock().unwrap_or_else(|e| e.into_inner());
        let commit_path = self.dir.join(SPEND_PAGES_COMMIT_NAME);
        let commit_gone = match std::fs::remove_file(&commit_path) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        // The commit name has to be gone, and that directory entry durable,
        // before the page files go. A directory sync does not prove the
        // unlink happened. A crash that keeps the commit and loses the page
        // file reloads the new records against an older image, or refuses
        // to open.
        if commit_gone && fsync_parent_dir(&self.dir).is_ok() {
            let _ = std::fs::remove_file(self.dir.join(SPEND_PAGES_NAME));
            self.remove_page_sidecars();
            let mut g = self.lock();
            g.log_bytes = 0;
            g.commit_height = None;
            gc_gens(&mut g);
        }
    }

    /// Move the generation file over `spend.pages` after the commit names it.
    ///
    /// A failed rename leaves `spend.pages.{generation}` for reload. Deleting
    /// sidecars here would remove the file the commit still names. A directory
    /// sync failure after a successful rename still keeps the publish.
    fn finish_pages_rename(&self, gen_path: &Path, pages_path: &Path) {
        if std::fs::rename(gen_path, pages_path).is_ok() {
            let _ = fsync_parent_dir(&self.dir);
            self.remove_page_sidecars();
        }
    }

    /// Drop `spend.pages.{generation}` leftovers. The commit file stays.
    fn remove_page_sidecars(&self) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("spend.pages.") && name != SPEND_PAGES_COMMIT_NAME {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }

    /// Copy cooled pages, then drop those directory entries.
    ///
    /// Callers that must not hold a reader lock across the body copy use
    /// [`Self::copy_cooled`] and [`Self::finish_cooled`] instead.
    pub fn fold_into<W, S>(&self, write_page: W, sync: S) -> Result<(), StoreError>
    where
        W: FnMut(u64, &[u8]) -> Result<(), StoreError>,
        S: FnMut() -> Result<(), StoreError>,
    {
        self.copy_cooled(write_page, sync)?;
        self.finish_cooled()
    }

    /// Copy committed pages that confirm has not re-dirtied onto `spent.body`.
    ///
    /// A held snapshot is not copied. The directory entries stay until
    /// [`Self::finish_cooled`], so a crash during the copy still serves the log.
    pub(crate) fn copy_cooled<W, S>(&self, mut write_page: W, mut sync: S) -> Result<(), StoreError>
    where
        W: FnMut(u64, &[u8]) -> Result<(), StoreError>,
        S: FnMut() -> Result<(), StoreError>,
    {
        let quiet: Vec<(u64, u32, PageLoc)> = {
            let mut g = self.lock();
            if g.fold_held {
                g.cooling.clear();
                return Ok(());
            }
            let cool_through = g.cool_through;
            let mut quiet: Vec<(u64, u32, PageLoc)> = g
                .committed
                .iter()
                .filter(|(off, page)| {
                    page.admitted <= cool_through
                        && !g.open.contains_key(*off)
                        && !g.later.contains_key(*off)
                        && !g.frozen.contains_key(*off)
                })
                .map(|(off, page)| (*off, page.admitted, page.loc))
                .collect();
            quiet.sort_by_key(|(off, _, _)| *off);
            quiet
        };
        if quiet.is_empty() {
            return Ok(());
        }
        let mut wrote: Vec<(u64, u32)> = Vec::new();
        for (off, admitted, loc) in quiet {
            {
                let g = self.lock();
                if g.fold_held {
                    break;
                }
                if g.open.contains_key(&off)
                    || g.later.contains_key(&off)
                    || g.frozen.contains_key(&off)
                {
                    continue;
                }
            }
            let image = self.read_loc(loc);
            let Some((write_off, payload)) = fold_payload(off, &image) else {
                continue;
            };
            write_page(write_off, payload)?;
            wrote.push((off, admitted));
        }
        if wrote.is_empty() {
            return Ok(());
        }
        sync()?;
        let mut g = self.lock();
        // Disconnect can hold the fold during the copy. Leave the log in place.
        if g.fold_held {
            g.cooling.clear();
            return Ok(());
        }
        g.cooling = wrote;
        Ok(())
    }

    /// Drop directory entries for pages [`Self::copy_cooled`] already synced.
    ///
    /// Does not delete the files while an open, frozen, pending, or in-flight
    /// compose still names slots. Those maps can be the only copy.
    pub(crate) fn finish_cooled(&self) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.fold_held {
            g.cooling.clear();
            return Ok(());
        }
        let wrote = std::mem::take(&mut g.cooling);
        if wrote.is_empty() {
            return Ok(());
        }
        for (off, generation) in wrote {
            if g.open.contains_key(&off) || g.frozen.contains_key(&off) {
                continue;
            }
            if g.committed
                .get(&off)
                .is_some_and(|page| page.admitted == generation)
            {
                g.committed.remove(&off);
            }
        }
        gc_gens(&mut g);
        if g.committed.is_empty()
            && g.open.is_empty()
            && g.later.is_empty()
            && g.frozen.is_empty()
            && g.pending.is_none()
            && g.compose_height.is_none()
        {
            g.commit_height = None;
            g.fold_held = false;
            drop(g);
            self.delete_generation_files();
        }
        Ok(())
    }

    fn reload(&self, body_end: Option<u64>) -> Result<(), StoreError> {
        let commit_path = self.dir.join(SPEND_PAGES_COMMIT_NAME);
        if !commit_path.exists() {
            let _ = std::fs::remove_file(self.dir.join(SPEND_PAGES_NAME));
            self.remove_page_sidecars();
            return Ok(());
        }
        let buf = std::fs::read(&commit_path).map_err(|e| StoreError::io(&commit_path, e))?;
        let parsed = parse_commit(&buf)?;
        let gen_path = self
            .dir
            .join(format!("{SPEND_PAGES_NAME}.{}", parsed.generation));
        let pages_path = if gen_path.exists() {
            gen_path
        } else {
            self.dir.join(SPEND_PAGES_NAME)
        };
        let opened =
            open_committed_pages(&pages_path, parsed.committed_len, parsed.recs.is_empty())?;
        let limit = parsed.committed_len;
        for rec in &parsed.recs {
            let end = rec.log_off.saturating_add(u64::from(rec.image_len));
            if rec.log_off > limit || end > limit {
                return Err(StoreError::Corrupt(
                    "spend page commit names bytes past the committed length",
                ));
            }
        }
        let mut g = self.lock();
        g.generation = parsed.generation;
        g.cool_through = 0;
        g.commit_height = Some(parsed.height);
        g.fold_held = parsed.fold_held;
        g.sealed_tip = Some(parsed.sealed_tip);
        g.log_bytes = parsed.committed_len;
        g.open_edges = 0;
        g.committed.clear();
        let mut kept_recs: Vec<(u64, u64, u32)> = Vec::new();
        let mut clipped = false;
        let mut named_pages = HashSet::new();
        for rec in parsed.recs {
            if rec.page_off % BODY_PAGE_SIZE != 0 {
                return Err(StoreError::Corrupt(
                    "spend page commit offset is not page aligned",
                ));
            }
            if u64::from(rec.image_len) > BODY_PAGE_SIZE {
                return Err(StoreError::Corrupt(
                    "spend page commit image is longer than one page",
                ));
            }
            if !named_pages.insert(rec.page_off) {
                return Err(StoreError::Corrupt("spend page commit names a page twice"));
            }
            if body_end.is_some_and(|end| rec.page_off >= end) {
                clipped = true;
                continue;
            }
            let mut image_len = u64::from(rec.image_len);
            if let Some(end) = body_end {
                let room = end.saturating_sub(rec.page_off);
                if room == 0 {
                    clipped = true;
                    continue;
                }
                if image_len > room {
                    image_len = room;
                    clipped = true;
                }
            }
            if image_len == 0 {
                if body_end.is_some() {
                    clipped = true;
                }
                continue;
            }
            // `image_len` starts as the record's `u32` and only shrinks.
            let image_len = image_len as u32;
            kept_recs.push((rec.page_off, rec.log_off, image_len));
            g.committed.insert(
                rec.page_off,
                CommittedPage {
                    loc: PageLoc {
                        generation: parsed.generation,
                        log_off: rec.log_off,
                        image_len,
                    },
                    admitted: parsed.generation,
                },
            );
        }
        if !g.committed.is_empty() {
            if let Some(file) = opened {
                g.gens.insert(parsed.generation, Arc::new(file));
            }
        }
        let rewrite = clipped.then(|| {
            encode_commit(
                parsed.height,
                parsed.generation,
                parsed.committed_len,
                &kept_recs,
                parsed.fold_held,
                parsed.sealed_tip,
            )
        });
        drop(g);
        if let Some(bytes) = rewrite {
            // `hold_fold_above` takes `commit_io` before the directory lock.
            let _io = self.commit_io.lock().unwrap_or_else(|e| e.into_inner());
            if !write_commit_file(&commit_path, &bytes)? {
                return Err(StoreError::io(
                    &commit_path,
                    std::io::Error::other("spend.pages.commit directory sync failed"),
                ));
            }
        }
        Ok(())
    }
}

/// Open the generation file named by a commit, or `None` when that commit
/// stores no page bytes and the file is already gone.
fn open_committed_pages(
    pages_path: &Path,
    committed_len: u64,
    recs_empty: bool,
) -> Result<Option<File>, StoreError> {
    let file = match File::open(pages_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && committed_len == 0 && recs_empty => {
            return Ok(None);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::Corrupt("spend.pages short"));
        }
        Err(e) => return Err(StoreError::io(pages_path, e)),
    };
    let file_len = file
        .metadata()
        .map_err(|e| StoreError::io(pages_path, e))?
        .len();
    if file_len < committed_len {
        return Err(StoreError::Corrupt("spend.pages short"));
    }
    Ok(Some(file))
}

/// Clears `compose_height` on every `prepare_commit` return, including errors.
struct ClearCompose<'a>(&'a SpendPageLog);

impl Drop for ClearCompose<'_> {
    fn drop(&mut self) {
        self.0.lock().compose_height = None;
    }
}

/// Puts notes taken during a held filter back into `open` if the freeze
/// did not.
struct ReleaseLater<'a>(&'a SpendPageLog);

impl Drop for ReleaseLater<'_> {
    fn drop(&mut self) {
        let mut g = self.0.lock();
        if g.holding_filter || !g.later.is_empty() {
            merge_later(&mut g, false);
        }
    }
}

/// Pull notes taken after a freeze into the snapshot compose will publish.
///
/// A restored page has no base. The stored image is still that base, including
/// a zero fold has not written. Compose zeros a rejected slot in that image
/// only when the body still holds it. The old image stays in `committed`
/// until the replacement rename returns.
fn fold_open_into_snapshot(
    pages: &mut SnapshotPages,
    open: &HashMap<u64, OpenPage>,
    committed: &HashMap<u64, CommittedPage>,
) {
    let mut index: HashMap<u64, usize> = HashMap::new();
    for (i, (off, _)) in pages.iter().enumerate() {
        index.entry(*off).or_insert(i);
    }
    for (off, noted) in open {
        if let Some(&i) = index.get(off) {
            overlay_slots(&mut pages[i].1.slots, &noted.slots);
            pages[i].1.reindex();
        } else {
            index.insert(*off, pages.len());
            pages.push((*off, noted.clone()));
        }
    }
    for (off, page) in pages.iter_mut() {
        if !committed.contains_key(off) {
            page.base = None;
        } else if page.base.is_none() {
            // A note taken while the filter ran, or a page this checkpoint
            // is clearing, has no base. The stored image is still the page,
            // including a zero fold has not written. Compose zeros a
            // rejected slot in that image only when the body still holds it.
            if let Some(stored) = committed.get(off) {
                page.base = Some(stored.loc);
            }
        }
    }
}

fn overlay_slots(dst: &mut PageSlots, src: &[(u16, [u8; SLOT])]) {
    let mut at: HashMap<u16, usize> = HashMap::new();
    for (i, (rel, _)) in dst.iter().enumerate() {
        at.entry(*rel).or_insert(i);
    }
    for (rel, meta) in src {
        if let Some(&i) = at.get(rel) {
            dst[i].1 = *meta;
        } else {
            at.insert(*rel, dst.len());
            dst.push((*rel, *meta));
        }
    }
}

/// Name a committed page this generation does not recompose.
///
/// `keep` can reject every new note on a page. The sealed slots stay in the
/// previous image. Leaving that page out of the new commit drops them before
/// fold copies the body.
fn carried_pages(
    committed: &HashMap<u64, CommittedPage>,
    kept: &[(u64, OpenPage)],
) -> Vec<(u64, CommittedPage)> {
    let recomposed: HashSet<u64> = kept.iter().map(|(off, _)| *off).collect();
    let mut out = Vec::new();
    for (off, page) in committed {
        if recomposed.contains(off) {
            continue;
        }
        out.push((*off, *page));
    }
    out
}

fn filter_kept_pages<K>(
    keep: &mut K,
    snapshot: SnapshotPages,
) -> Result<(SnapshotPages, Vec<u64>, Vec<BodyClear>), StoreError>
where
    K: FnMut(&[u8; SLOT]) -> Result<bool, StoreError>,
{
    let mut kept = Vec::new();
    let mut rejected = Vec::new();
    let mut clears = Vec::new();
    for (page_off, mut page) in snapshot {
        let from_body = page.base.is_none();
        let mut slots = Vec::new();
        for slot in page.slots.drain(..) {
            if keep(&slot.1)? {
                slots.push(slot);
            } else if from_body {
                clears.push(BodyClear {
                    page_off,
                    rel: slot.0,
                    meta: slot.1,
                });
            }
        }
        page.slots = slots;
        page.reindex();
        if page.slots.is_empty() {
            rejected.push(page_off);
        } else {
            kept.push((page_off, page));
        }
    }
    Ok((kept, rejected, clears))
}

fn clears_by_page(clears: &[BodyClear]) -> HashMap<u64, Vec<(u16, [u8; SLOT])>> {
    let mut clear_at: HashMap<u64, Vec<(u16, [u8; SLOT])>> = HashMap::new();
    for clear in clears {
        clear_at
            .entry(clear.page_off)
            .or_default()
            .push((clear.rel, clear.meta));
    }
    clear_at
}

fn compose_offs(kept: &[(u64, OpenPage)], clears: &[BodyClear]) -> Vec<u64> {
    let mut offs: Vec<u64> = kept.iter().map(|(off, _)| *off).collect();
    offs.extend(clears.iter().map(|clear| clear.page_off));
    offs.sort_unstable();
    offs.dedup();
    offs
}

fn compose_base(kept: Option<&OpenPage>, stored: Option<PageLoc>) -> Option<PageLoc> {
    kept.and_then(|page| page.base).or(stored)
}

impl SpendPageLog {
    fn pages_that_compose<F>(
        &self,
        read_page: &mut F,
        published: u64,
        kept: &[(u64, OpenPage)],
        clears: &[BodyClear],
    ) -> Result<Vec<u64>, StoreError>
    where
        F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
    {
        let stored = self.committed_locs();
        let clear_at = clears_by_page(clears);
        let kept_at: HashMap<u64, &OpenPage> =
            kept.iter().map(|(off, page)| (*off, page)).collect();
        let mut out = Vec::new();
        for page_off in compose_offs(kept, clears) {
            let loc = compose_base(
                kept_at.get(&page_off).copied(),
                stored.get(&page_off).copied(),
            );
            let image = loc.map(|loc| self.read_loc(loc));
            if compose_one_page(
                read_page,
                published,
                page_off,
                kept_at.get(&page_off).copied(),
                clear_at.get(&page_off),
                image.as_deref(),
            )?
            .is_some()
            {
                out.push(page_off);
            }
        }
        Ok(out)
    }
}

/// `None` when the page has no kept slot, the body does not still hold a
/// rejected spend, and the stored image is not hiding a body spend with a
/// zero. A matching body slot is zeroed in the stored image so the next
/// fold clears it. The body is not the base when an image is already stored.
fn compose_one_page<F>(
    read_page: &mut F,
    published: u64,
    page_off: u64,
    kept: Option<&OpenPage>,
    clears: Option<&Vec<(u16, [u8; SLOT])>>,
    image: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, StoreError>
where
    F: FnMut(u64, usize) -> Result<Vec<u8>, StoreError>,
{
    if page_off >= published {
        return Ok(None);
    }
    let len = BODY_PAGE_SIZE.min(published - page_off) as usize;
    let short = image.is_some_and(|image| image.len() < len);
    let body = if image.is_none() || clears.is_some() || short {
        Some(fit_page(read_page(page_off, len)?, len))
    } else {
        None
    };
    let mut buf = if let Some(image) = image {
        image_with_body_tail(image, body.as_deref(), len)
    } else {
        body.clone().unwrap_or_else(|| vec![0u8; len])
    };
    let mut wrote_kept = false;
    if let Some(page) = kept {
        for &(rel, meta) in &page.slots {
            if slot_past_published(page_off, rel, published) {
                continue;
            }
            write_slot(&mut buf, rel, &meta)?;
            wrote_kept = true;
        }
    }
    let mut zeroed = false;
    if let Some(body) = body.as_deref() {
        if let Some(clears) = clears {
            for &(rel, meta) in clears {
                if slot_past_published(page_off, rel, published) {
                    continue;
                }
                if kept.is_some_and(|page| page.slots.iter().any(|(r, _)| *r == rel)) {
                    continue;
                }
                if slot_from_image(body, rel) == meta {
                    write_slot(&mut buf, rel, &[0u8; SLOT])?;
                    zeroed = true;
                } else if image.is_some() {
                    let on_body = slot_from_image(body, rel);
                    write_slot(&mut buf, rel, &on_body)?;
                }
            }
        }
    }
    let hides = match (image, body.as_deref()) {
        (Some(image), Some(body)) => image_hides_body(page_off, image, body, len),
        _ => false,
    };
    let keep_page = wrote_kept || zeroed || hides;
    if !keep_page {
        return Ok(None);
    }
    Ok(Some(buf))
}

fn fit_page(mut buf: Vec<u8>, len: usize) -> Vec<u8> {
    if buf.len() < len {
        buf.resize(len, 0);
    }
    buf.truncate(len);
    buf
}

/// Copy `image`, then the body bytes past it.
///
/// A short tail padded with zeros would hide a later body slot, and the next
/// fold would erase that spend.
fn image_with_body_tail(image: &[u8], body: Option<&[u8]>, len: usize) -> Vec<u8> {
    let take = len.min(image.len());
    let mut buf = image[..take].to_vec();
    if buf.len() < len {
        if let Some(body) = body {
            let from = buf.len().min(body.len());
            let to = len.min(body.len());
            if from < to {
                buf.extend_from_slice(&body[from..to]);
            }
        }
        if buf.len() < len {
            buf.resize(len, 0);
        }
    }
    buf.truncate(len);
    buf
}

/// A zero in the stored image is hiding a spend fold has not written.
///
/// Bytes past the stored image are not part of it. The body still holds
/// those slots.
fn image_hides_body(page_off: u64, image: &[u8], body: &[u8], len: usize) -> bool {
    let n = (image.len() / SLOT).min(len / SLOT);
    (0..n).any(|rel| {
        if page_off == 0 && rel < 2 {
            return false;
        }
        let rel = rel as u16;
        slot_from_image(image, rel) == [0u8; SLOT] && slot_from_image(body, rel) != [0u8; SLOT]
    })
}

/// The slot's last byte sits past the published body. It is not a clear.
fn slot_past_published(page_off: u64, rel: u16, published: u64) -> bool {
    let end = page_off.saturating_add(u64::from(rel).saturating_add(1).saturating_mul(SLOT as u64));
    end > published
}

fn write_slot(buf: &mut [u8], rel: u16, meta: &[u8; SLOT]) -> Result<(), StoreError> {
    let i = rel as usize * SLOT;
    if i + SLOT > buf.len() {
        return Err(StoreError::Corrupt("spend page slot past published"));
    }
    buf[i..i + SLOT].copy_from_slice(meta);
    Ok(())
}

fn insert_note(
    pages: &mut HashMap<u64, OpenPage>,
    page_off: u64,
    rel: u16,
    meta: [u8; SLOT],
    base_if_new: Option<PageLoc>,
) -> Result<bool, StoreError> {
    pages
        .entry(page_off)
        .or_insert_with(|| open_page(Vec::new(), base_if_new));
    let Some(page) = pages.get_mut(&page_off) else {
        return Err(StoreError::Corrupt("spend page directory missing"));
    };
    let i = page.slots.partition_point(|(r, _)| *r < rel);
    if page.slots.get(i).is_some_and(|(r, _)| *r == rel) {
        page.slots[i].1 = meta;
        return Ok(false);
    }
    page.slots.insert(i, (rel, meta));
    let rel = rel as usize;
    if rel < OPEN_SLOT_PRESENCE_BYTES * 8 {
        page.present[rel / 64] |= 1u64 << (rel % 64);
    }
    Ok(true)
}

/// Notes taken during a held filter join `open` after the snapshot is copied.
///
/// `reset_edges` is the freeze path: `open` was just emptied, so the edge
/// count starts at the notes that waited in `later`.
fn merge_later(g: &mut Inner, reset_edges: bool) {
    g.holding_filter = false;
    let later = std::mem::take(&mut g.later);
    if reset_edges {
        g.open_edges = 0;
    }
    for (off, page) in later {
        let n = page.slots.len() as u64;
        if let Some(dst) = g.open.get_mut(&off) {
            let before = dst.slots.len() as u64;
            overlay_slots(&mut dst.slots, &page.slots);
            dst.reindex();
            let added = (dst.slots.len() as u64).saturating_sub(before);
            if reset_edges {
                g.open_edges = g.open_edges.saturating_add(dst.slots.len() as u64);
            } else {
                g.open_edges = g.open_edges.saturating_sub(n.saturating_sub(added));
            }
        } else if reset_edges {
            g.open_edges = g.open_edges.saturating_add(n);
            g.open.insert(off, page);
        } else {
            g.open.insert(off, page);
        }
    }
}

fn page_hit(page: Option<&OpenPage>, rel: u16) -> Option<[u8; SLOT]> {
    page?.hit(rel)
}

enum Covered {
    Noted([u8; SLOT]),
    File(PageLoc),
}

/// Later note, open note, open base, frozen note, frozen base, then the
/// committed file. A covered zero is `File`, not a miss.
fn covered_slot(g: &Inner, page_off: u64, rel: u16) -> Option<Covered> {
    if let Some(meta) = page_hit(g.later.get(&page_off), rel) {
        return Some(Covered::Noted(meta));
    }
    if let Some(page) = g.open.get(&page_off) {
        if let Some(meta) = page.hit(rel) {
            return Some(Covered::Noted(meta));
        }
        if let Some(loc) = page.base.filter(|loc| loc.covers(rel)) {
            return Some(Covered::File(loc));
        }
    }
    if let Some(page) = g.frozen.get(&page_off) {
        if let Some(meta) = page.hit(rel) {
            return Some(Covered::Noted(meta));
        }
        if let Some(loc) = page.base.filter(|loc| loc.covers(rel)) {
            return Some(Covered::File(loc));
        }
    }
    let page = g.committed.get(&page_off)?;
    page.loc.covers(rel).then_some(Covered::File(page.loc))
}

fn generation_file(g: &Inner, generation: u32) -> Arc<File> {
    match g.gens.get(&generation) {
        Some(file) => Arc::clone(file),
        None => panic!("spend generation {generation} is not open"),
    }
}

fn pread_generation(file: &File, path: &Path, offset: u64, buf: &mut [u8]) {
    if let Err(err) = crate::file::handle_pread_all(file, path, offset, buf) {
        panic!("spend generation read failed at {offset}: {err}");
    }
}

fn read_loc_bytes(file: &File, path: &Path, loc: PageLoc) -> Vec<u8> {
    let mut buf = vec![0u8; loc.image_len as usize];
    if !buf.is_empty() {
        pread_generation(file, path, loc.log_off, &mut buf);
    }
    buf
}

fn retarget_base(page: Option<&mut OpenPage>, loc: PageLoc) {
    if let Some(page) = page {
        if page.base.is_some() {
            page.base = Some(loc);
        }
    }
}

fn gc_gens(g: &mut Inner) {
    let mut live = HashSet::new();
    for page in g.committed.values() {
        live.insert(page.loc.generation);
    }
    for map in [&g.open, &g.later, &g.frozen] {
        for page in map.values() {
            if let Some(loc) = page.base {
                live.insert(loc.generation);
            }
        }
    }
    if let Some(pending) = &g.pending {
        live.insert(pending.generation);
    }
    g.gens.retain(|gen, _| live.contains(gen));
}

/// `None` when `rel` sits past the stored image. The body still holds it.
fn slot_in_image(image: &[u8], rel: u16) -> Option<[u8; SLOT]> {
    let i = rel as usize * SLOT;
    if i + SLOT > image.len() {
        return None;
    }
    Some(slot_from_image(image, rel))
}

/// Page 0's first 16 bytes are the file header. Fold never pwrites them.
fn fold_payload(page_off: u64, image: &[u8]) -> Option<(u64, &[u8])> {
    if page_off == 0 {
        let skip = crate::file::FILE_HEADER_LEN.min(image.len());
        if skip >= image.len() {
            return None;
        }
        return Some((skip as u64, &image[skip..]));
    }
    if image.is_empty() {
        return None;
    }
    Some((page_off, image))
}

/// Decide what a held image does at this checkpoint.
///
/// A pure-zero image is folded: the file stays, and nothing is rebuilt from
/// the body. An image that still has a spend, and no rejected slot, keeps
/// those slots and stays in `committed`. A frozen note on that page stays.
/// A slot the frozen page does not name yet is added beside it. When
/// `frozen` already holds a page, the slots stay there. Otherwise they
/// move into `open`. Compose copies the stored image, so a zero on the
/// page is kept, and this checkpoint does not fold
/// it. The hold stays until the publish after the second `keep`. Clearing
/// it here lets a disconnect during the first `keep` fold the old image.
/// A rejected slot leaves the old image in place and clears `base` so
/// compose reads the body.
fn apply_held_explode(
    g: &mut Inner,
    live: &HashSet<u64>,
    restored: Vec<KeptPage>,
    cleared: Vec<BodyClear>,
    sampled_ticks: u64,
) -> Vec<BodyClear> {
    if cleared.is_empty() {
        let spend_offs = live;
        if spend_offs.is_empty() {
            // No slot was walked. A disconnect that landed anyway must keep
            // the hold, or the next checkpoint folds this image.
            if g.hold_ticks == sampled_ticks {
                g.fold_held = false;
            }
            return Vec::new();
        }
        for (page_off, slots) in restored {
            if spend_offs.contains(&page_off) {
                restore_kept_slots(g, page_off, slots);
            }
        }
        return Vec::new();
    }
    let cleared_offs: HashSet<u64> = cleared.iter().map(|clear| clear.page_off).collect();
    for off in &cleared_offs {
        if let Some(page) = g.open.get_mut(off) {
            page.base = None;
        }
        if let Some(page) = g.frozen.get_mut(off) {
            page.base = None;
        }
    }
    for (page_off, slots) in restored {
        if !cleared_offs.contains(&page_off) {
            continue;
        }
        restore_kept_slots(g, page_off, slots);
    }
    cleared
}

/// Keep restored slots without covering a newer frozen note.
///
/// When `frozen` already holds a page, the slots stay there and do not count
/// toward the edge cap. The early freeze only swaps, so there is no staged
/// snapshot yet. An empty freeze leaves `frozen` empty: the slots park in
/// `open`, and the callback freeze resets that count. A newer note already
/// in `frozen` stays.
fn restore_kept_slots(g: &mut Inner, page_off: u64, slots: PageSlots) {
    if !g.frozen.is_empty() {
        park_frozen_slots(g, page_off, slots);
    } else {
        park_open_slots(g, page_off, slots);
    }
}

/// Put restored slots on the frozen page. Do not count them as open edges.
fn park_frozen_slots(g: &mut Inner, page_off: u64, slots: PageSlots) {
    if let Some(page) = g.frozen.get_mut(&page_off) {
        overlay_missing_slots(page, slots);
        page.base = None;
        return;
    }
    g.frozen.insert(page_off, open_page(slots, None));
}

/// Copy slots that this page does not already name. A newer note stays.
fn overlay_missing_slots(page: &mut OpenPage, slots: PageSlots) -> u64 {
    let mut have: HashSet<u16> = page.slots.iter().map(|(rel, _)| *rel).collect();
    let mut added = 0u64;
    for (rel, meta) in slots {
        if !have.insert(rel) {
            continue;
        }
        page.slots.push((rel, meta));
        added = added.saturating_add(1);
    }
    page.reindex();
    added
}

fn park_open_slots(g: &mut Inner, page_off: u64, slots: PageSlots) {
    if let Some(page) = g.open.get_mut(&page_off) {
        let added = overlay_missing_slots(page, slots);
        page.base = None;
        g.open_edges = g.open_edges.saturating_add(added);
    } else {
        g.open_edges = g.open_edges.saturating_add(slots.len() as u64);
        g.open.insert(page_off, open_page(slots, None));
    }
}

fn image_has_live_spend(page_off: u64, image: &[u8], published: u64) -> bool {
    let n = image.len() / SLOT;
    (0..n).any(|rel| {
        if page_off == 0 && rel < 2 {
            return false;
        }
        if slot_past_published(page_off, rel as u16, published) {
            return false;
        }
        slot_from_image(image, rel as u16) != [0u8; SLOT]
    })
}

fn slot_from_image(image: &[u8], rel: u16) -> [u8; SLOT] {
    let i = rel as usize * SLOT;
    let mut out = [0u8; SLOT];
    if i + SLOT <= image.len() {
        out.copy_from_slice(&image[i..i + SLOT]);
    }
    out
}

struct CommitRec {
    page_off: u64,
    log_off: u64,
    image_len: u32,
}

struct ParsedCommit {
    height: u32,
    generation: u32,
    committed_len: u64,
    fold_held: bool,
    /// Confirmed tip frozen with this snapshot. Older commits omit the
    /// trailer and are read as the snapshot height.
    sealed_tip: u32,
    recs: Vec<CommitRec>,
}

fn encode_commit(
    height: u32,
    generation: u32,
    committed_len: u64,
    recs: &[(u64, u64, u32)],
    fold_held: bool,
    sealed_tip: u32,
) -> Vec<u8> {
    let tail = COMMIT_HEAD + recs.len() * COMMIT_REC;
    let mut buf = vec![0u8; tail + 4];
    buf[..4].copy_from_slice(&STORE_MAGIC);
    buf[4..6].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
    if fold_held {
        buf[6..8].copy_from_slice(&1u16.to_le_bytes());
    }
    buf[8..12].copy_from_slice(&height.to_le_bytes());
    buf[12..16].copy_from_slice(&generation.to_le_bytes());
    buf[16..24].copy_from_slice(&committed_len.to_le_bytes());
    buf[24..28].copy_from_slice(&(recs.len() as u32).to_le_bytes());
    for (i, (page_off, log_off, image_len)) in recs.iter().enumerate() {
        let at = COMMIT_HEAD + i * COMMIT_REC;
        buf[at..at + 8].copy_from_slice(&page_off.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&log_off.to_le_bytes());
        buf[at + 16..at + 20].copy_from_slice(&image_len.to_le_bytes());
    }
    buf[tail..tail + 4].copy_from_slice(&sealed_tip.to_le_bytes());
    buf
}

/// Rename `bytes` over `path`. The rename is the commit.
///
/// `Ok(false)` means the parent-directory sync failed after the new name
/// is already visible. Returning that as an error would make the next
/// checkpoint write the same generation again. Callers that rename the
/// generation file away wait for `Ok(true)`.
fn write_commit_file(path: &Path, bytes: &[u8]) -> Result<bool, StoreError> {
    write_file_synced_tmp_rename(path, bytes)?;
    let Some(parent) = path.parent() else {
        return Ok(true);
    };
    if parent.as_os_str().is_empty() {
        return Ok(true);
    }
    Ok(fsync_parent_dir(parent).is_ok())
}

// llvm-cov drops the counters when this parser is inlined into `reload`.
#[inline(never)]
fn parse_commit(buf: &[u8]) -> Result<ParsedCommit, StoreError> {
    if buf.len() < COMMIT_HEAD {
        return Err(StoreError::Corrupt("spend.pages.commit short"));
    }
    if buf[..4] != STORE_MAGIC {
        return Err(StoreError::BadMagic);
    }
    let mut head = [0u8; COMMIT_HEAD];
    head.copy_from_slice(&buf[..COMMIT_HEAD]);
    let ver = le_u16!(head, 4);
    if !schema_file_openable(ver) {
        return Err(StoreError::BadSchema(ver));
    }
    let fold_held = le_u16!(head, 6) != 0;
    let height = le_u32!(head, 8);
    let generation = le_u32!(head, 12);
    let committed_len = le_u64!(head, 16);
    let n = le_u32!(head, 24) as usize;
    let need = COMMIT_HEAD.saturating_add(n.saturating_mul(COMMIT_REC));
    if buf.len() < need {
        return Err(StoreError::Corrupt("spend.pages.commit short"));
    }
    let sealed_tip = if buf.len() >= need + 4 {
        let mut tip = [0u8; 4];
        tip.copy_from_slice(&buf[need..need + 4]);
        le_u32!(tip, 0)
    } else {
        height
    };
    let mut recs = Vec::with_capacity(n);
    for i in 0..n {
        let at = COMMIT_HEAD + i * COMMIT_REC;
        let mut rec = [0u8; COMMIT_REC];
        rec.copy_from_slice(&buf[at..at + COMMIT_REC]);
        recs.push(CommitRec {
            page_off: le_u64!(rec, 0),
            log_off: le_u64!(rec, 8),
            image_len: le_u32!(rec, 16),
        });
    }
    Ok(ParsedCommit {
        height,
        generation,
        committed_len,
        fold_held,
        sealed_tip,
        recs,
    })
}
