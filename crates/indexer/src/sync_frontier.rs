//! The in-memory half of the sync-state contiguity contract (spec §5.3).
//!
//! `sync_state.last_contiguous_slot` means "there are no gaps below this slot". Who is allowed
//! to advance it is the whole question, and the answer changed between Task 3 and Task 4.
//!
//! ## Why the live stream does not advance it (controller ruling, Task 4)
//!
//! Task 3 advanced the frontier from the live gRPC batches while a stream "session" was
//! unbroken. Two findings killed that:
//!
//! 1. carbon's Yellowstone datasource **re-subscribes internally** on a stream error (and even
//!    swallows auth/plan rejections in a retry loop), so the process cannot reliably observe
//!    that a session ended. "No gap was recorded" is therefore not evidence that no gap exists.
//! 2. This program is **idle for days at a time**, so update-driven session tracking never even
//!    arms: with no updates there is no first-update slot, and the frontier would sit frozen
//!    while the stream is perfectly healthy.
//!
//! So contiguity is now **crawler-driven**: the reconciliation supervisor
//! ([`crate::reconcile`]) periodically re-walks `getSignaturesForAddress` from the chain tip
//! down to the current `last_contiguous_slot`, re-writes everything it finds (all writes are
//! idempotent), and only then advances the frontier to the tip slot it recorded *before* that
//! walk. The live stream's job is FRESHNESS; the crawler's job is COMPLETENESS.
//!
//! ## What is left in this struct
//!
//! Two gates, both of which have to be open before the reconciler may advance:
//!
//! * `backfill_complete` -- mirrors `sync_state.backfill_complete`. Until the historical
//!   backfill has walked down to `backfill_floor_slot`, everything below the first indexed slot
//!   is missing, and "no gaps below T" would be a lie regardless of how good the recent crawl
//!   was.
//! * `gap_open` -- set at process start (the downtime before we started *is* a gap) and by the
//!   reconnect loop, cleared by whatever proves contiguity again: the backfill finishing, or a
//!   reconciliation crawl completing. It is belt-and-braces next to the reconciler's re-walk,
//!   which would fill such a hole anyway; keeping it means a freshly started process cannot
//!   advance the frontier before its first successful crawl.

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub struct SyncFrontier {
    /// Mirrors `sync_state.backfill_complete`. Seeded from the DB at startup.
    backfill_complete: AtomicBool,
    /// True whenever there is a known-or-possible hole that no crawl has closed yet.
    gap_open: AtomicBool,
}

impl SyncFrontier {
    /// `backfill_complete` comes from the `sync_state` row at startup.
    pub fn new(backfill_complete: bool) -> Self {
        Self {
            backfill_complete: AtomicBool::new(backfill_complete),
            // A fresh process has proven nothing about the period it was not running.
            gap_open: AtomicBool::new(true),
        }
    }

    /// The live stream session ended (error, disconnect). Whatever happened on chain during the
    /// outage may be missing until a crawl re-covers that range.
    pub fn gap_opened(&self) {
        if !self.gap_open.swap(true, Ordering::Relaxed) {
            log::warn!(
                "sync gap opened (gRPC stream session ended); last_contiguous_slot is frozen \
                 until the reconciliation crawl re-covers the range"
            );
        }
    }

    /// A crawl has covered everything from `last_contiguous_slot` up to the slot it is about to
    /// advance to. Called by the history backfill on completion and by every successful
    /// reconciliation cycle.
    pub fn gap_closed(&self) {
        if self.gap_open.swap(false, Ordering::Relaxed) {
            log::info!("sync gap closed; last_contiguous_slot may advance again");
        }
    }

    /// Mirrors a write to `sync_state.backfill_complete`.
    pub fn set_backfill_complete(&self, complete: bool) {
        self.backfill_complete.store(complete, Ordering::Relaxed);
    }

    pub fn backfill_complete(&self) -> bool {
        self.backfill_complete.load(Ordering::Relaxed)
    }

    pub fn gap_open(&self) -> bool {
        self.gap_open.load(Ordering::Relaxed)
    }

    /// May `sync_state.last_contiguous_slot` be advanced right now?
    ///
    /// Deliberately takes no slot: the only caller is the reconciler, which has just proven
    /// contiguity up to a tip slot it recorded itself. Any other caller (a live batch, a
    /// standalone backfill) must not advance the frontier at all.
    pub fn may_advance(&self) -> bool {
        self.backfill_complete() && !self.gap_open()
    }
}

#[cfg(test)]
mod tests {
    use super::SyncFrontier;

    #[test]
    fn a_fresh_process_cannot_advance_until_a_crawl_has_run() {
        let f = SyncFrontier::new(true);
        // Gap open from process start: the downtime before startup is itself a hole.
        assert!(!f.may_advance());
        f.gap_closed();
        assert!(f.may_advance());
    }

    #[test]
    fn an_incomplete_backfill_blocks_the_advance_even_after_a_clean_crawl() {
        let f = SyncFrontier::new(false);
        f.gap_closed();
        assert!(!f.may_advance());

        // The backfill reaching the floor is what unblocks it.
        f.set_backfill_complete(true);
        assert!(f.may_advance());
    }

    #[test]
    fn a_stream_drop_freezes_the_frontier_until_the_next_crawl() {
        let f = SyncFrontier::new(true);
        f.gap_closed();
        assert!(f.may_advance());

        f.gap_opened();
        assert!(!f.may_advance());

        // The reconciliation crawl re-covers the outage window and reopens the door.
        f.gap_closed();
        assert!(f.may_advance());
    }
}
