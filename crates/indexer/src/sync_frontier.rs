//! The in-memory half of the sync-state contiguity contract (spec §5.3).
//!
//! `sync_state.last_contiguous_slot` means "there are no gaps below this slot". A live stream
//! alone cannot honour that: the moment the gRPC connection drops, whatever happened on chain
//! during the outage is missing, so every slot the *new* session commits is above a hole.
//! Advancing the frontier anyway would tell Task 4's catch-up backfill there is nothing to
//! catch up on, and the hole would be permanent and invisible.
//!
//! So the rule is: only advance while the frontier is *earned*, meaning
//!
//! 1. the historical backfill has completed (`backfill_complete`), and
//! 2. no gap is currently open -- the stream session has been unbroken since either process
//!    start (with the backfill finishing after the stream connected) or since the last time a
//!    catch-up backfill closed the previous gap.
//!
//! This struct owns that decision and nothing else; it does not touch the database. The
//! flusher asks [`SyncFrontier::may_advance`] after each successful commit, the reconnect loop
//! calls [`SyncFrontier::gap_opened`]/[`SyncFrontier::session_started`], and Task 4's catch-up
//! backfill will call [`SyncFrontier::gap_closed`] once it has filled the hole.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug)]
pub struct SyncFrontier {
    /// Mirrors `sync_state.backfill_complete`. Seeded from the DB at startup.
    backfill_complete: AtomicBool,
    /// True whenever there is a known hole below the current session's start slot.
    gap_open: AtomicBool,
    /// True between `session_started` and `gap_opened`. A frontier can only be advanced by a
    /// live session; a batch committed with no session attached (e.g. the `replay`
    /// subcommand's crawler) must not move it.
    session_active: AtomicBool,
    /// Slot the current session first saw. Informational -- logged, and useful to Task 4 as
    /// the upper bound of the gap it has to fill.
    session_start_slot: AtomicU64,
}

impl SyncFrontier {
    /// `backfill_complete` comes from the `sync_state` row at startup.
    pub fn new(backfill_complete: bool) -> Self {
        Self {
            backfill_complete: AtomicBool::new(backfill_complete),
            // A fresh process has not yet proven contiguity for anything the stream is about
            // to deliver, so it starts with a gap open. `session_started` clears it only when
            // the backfill has already completed; otherwise the running backfill will clear
            // it via `gap_closed` when it finishes.
            gap_open: AtomicBool::new(true),
            session_active: AtomicBool::new(false),
            session_start_slot: AtomicU64::new(0),
        }
    }

    /// A stream session has connected and delivered its first update at `slot`.
    ///
    /// If the backfill is already complete this is the reconnect case: the hole between the
    /// last committed slot and `slot` stays open until Task 4's catch-up closes it. If the
    /// backfill has not completed yet, the backfill is (by construction) still going to run
    /// past this point, so it will close the gap itself.
    pub fn session_started(&self, slot: u64) {
        self.session_start_slot.store(slot, Ordering::Relaxed);
        self.session_active.store(true, Ordering::Relaxed);
        log::info!("stream connected at slot {slot}");
    }

    /// The stream session ended (error, disconnect, shutdown). Everything after this point is
    /// potentially discontiguous until a catch-up backfill says otherwise.
    pub fn gap_opened(&self) {
        self.session_active.store(false, Ordering::Relaxed);
        self.gap_open.store(true, Ordering::Relaxed);
        log::warn!(
            "sync gap opened (stream session ended at/after slot {}); \
             last_contiguous_slot is frozen until a catch-up backfill closes it",
            self.session_start_slot.load(Ordering::Relaxed)
        );
    }

    /// A catch-up backfill has filled everything below the current session's start slot.
    /// Wired by Task 4; called here only at startup when the backfill completes.
    pub fn gap_closed(&self) {
        self.gap_open.store(false, Ordering::Relaxed);
        log::info!("sync gap closed; last_contiguous_slot may advance again");
    }

    /// Mirrors a write to `sync_state.backfill_complete`.
    pub fn set_backfill_complete(&self, complete: bool) {
        self.backfill_complete.store(complete, Ordering::Relaxed);
    }

    pub fn backfill_complete(&self) -> bool {
        self.backfill_complete.load(Ordering::Relaxed)
    }

    pub fn session_start_slot(&self) -> u64 {
        self.session_start_slot.load(Ordering::Relaxed)
    }

    /// May a batch that committed up to `max_slot` advance `last_contiguous_slot`?
    ///
    /// `max_slot` is accepted (rather than this being a bare predicate) so callers read as
    /// "may I advance *to this*"; the value itself is only used for the guard against slots
    /// below the session start, which cannot be contiguous evidence for this session.
    pub fn may_advance(&self, max_slot: u64) -> bool {
        self.backfill_complete.load(Ordering::Relaxed)
            && self.session_active.load(Ordering::Relaxed)
            && !self.gap_open.load(Ordering::Relaxed)
            && max_slot >= self.session_start_slot.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::SyncFrontier;

    #[test]
    fn fresh_process_does_not_advance_until_backfill_completes() {
        let f = SyncFrontier::new(false);
        f.session_started(100);
        // Backfill still running -> no advance, even with a live session.
        assert!(!f.may_advance(101));

        f.set_backfill_complete(true);
        f.gap_closed();
        assert!(f.may_advance(101));
    }

    #[test]
    fn reconnect_freezes_the_frontier_until_gap_closed() {
        let f = SyncFrontier::new(true);
        f.session_started(100);
        f.gap_closed();
        assert!(f.may_advance(150));

        // Stream dies: everything the next session sees sits above a hole.
        f.gap_opened();
        assert!(!f.may_advance(150));
        f.session_started(400);
        assert!(!f.may_advance(401));

        // Task 4's catch-up fills the hole.
        f.gap_closed();
        assert!(f.may_advance(401));
    }

    #[test]
    fn slots_below_the_session_start_are_not_contiguity_evidence() {
        let f = SyncFrontier::new(true);
        f.session_started(1_000);
        f.gap_closed();
        assert!(!f.may_advance(999));
        assert!(f.may_advance(1_000));
    }

    #[test]
    fn a_batch_with_no_live_session_never_advances() {
        // The `replay` subcommand's crawler commits batches without ever calling
        // `session_started`; those must not move the frontier.
        let f = SyncFrontier::new(true);
        f.gap_closed();
        assert!(!f.may_advance(123_456));
    }
}
