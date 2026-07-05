//! `capture::resize` — debouncing WGC window-resize events into a single epoch
//! restart.
//!
//! Observed on hardware (M4-2 `window-events-probe`, 2026-07-05): during a window
//! drag-resize WGC reports a NEW `Direct3D11CaptureFrame::ContentSize` on essentially
//! every delivered frame — dozens of changes per second, through a continuous range
//! of (often odd) sizes, including the big jumps of a monitor/DPI switch — and then
//! STOPS delivering once the drag settles (a static window delivers nothing,
//! `02-AV-SYNC-SPEC §1.2`). Restarting the epoch on each change would rebuild the
//! whole pipeline dozens of times a second; instead this tracks the pending size and
//! reports a single *settled* size once it has held steady for `settle_ticks`.
//!
//! Two entry points because the drag ends with silence: [`ResizeTracker::observe`]
//! records the size of each delivered frame, and [`ResizeTracker::poll`] — called
//! every capture-loop iteration, not only on frame arrival — is what actually fires
//! the settle (no frame arrives after the drag stops, so a frame-driven check would
//! never trigger).
//!
//! Pure logic — 100% safe, no COM, unit-tested (`CLAUDE.md`).

use crate::spec_constants::units::TICKS_PER_SECOND;

/// Master-domain ticks a new size must hold before a resize is considered settled and
/// the epoch restarts at it. Tunable; 400 ms balances "don't rebuild mid-drag" against
/// "adopt the final size promptly". Not a spec constant (M4 window mode is outside the
/// frozen A/V-sync spec).
pub const DEFAULT_SETTLE_TICKS: i64 = 4 * TICKS_PER_SECOND / 10; // 400 ms

/// Debounces the `ContentSize` stream of a window resize into one settled size.
#[derive(Debug, Clone)]
pub struct ResizeTracker {
    /// The size the current epoch is capturing at (the pool/converter size).
    current: (u32, u32),
    /// A size different from `current` and the master time it first appeared, if a
    /// resize is in progress. Reset each time the size changes again (active drag).
    pending: Option<((u32, u32), i64)>,
    settle_ticks: i64,
}

impl ResizeTracker {
    /// Track resizes away from `current` (the epoch's capture size), settling after
    /// `settle_ticks` of stability.
    pub fn new(current: (u32, u32), settle_ticks: i64) -> Self {
        Self {
            current,
            pending: None,
            settle_ticks,
        }
    }

    /// Record the `content` size of a frame delivered at master time `now`. A size
    /// equal to `current` cancels any pending resize (the window returned to its
    /// active size); a new differing size (re)starts the settle timer.
    pub fn observe(&mut self, content: (u32, u32), now: i64) {
        if content == self.current {
            self.pending = None;
            return;
        }
        match self.pending {
            // Same pending size persisting — keep its original start time so the
            // settle window measures "stable since", not "seen again".
            Some((sz, _)) if sz == content => {}
            // First sight of this size (or it changed again mid-drag) — reset timer.
            _ => self.pending = Some((content, now)),
        }
    }

    /// Check at master time `now` whether a pending resize has held steady for the
    /// settle window. Returns the settled size once — adopting it as the new
    /// `current` — so the caller restarts the epoch at it. Call every loop iteration:
    /// WGC stops delivering frames once the drag settles, so this, not [`Self::observe`],
    /// is what fires.
    pub fn poll(&mut self, now: i64) -> Option<(u32, u32)> {
        if let Some((sz, since)) = self.pending {
            if now.saturating_sub(since) >= self.settle_ticks {
                self.current = sz;
                self.pending = None;
                return Some(sz);
            }
        }
        None
    }

    /// The size the current epoch is capturing at.
    pub fn current(&self) -> (u32, u32) {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: i64 = DEFAULT_SETTLE_TICKS; // 400 ms in ticks
    const MS: i64 = TICKS_PER_SECOND / 1000;

    #[test]
    fn no_change_never_settles() {
        let mut t = ResizeTracker::new((1920, 1080), SETTLE);
        for i in 0..100 {
            t.observe((1920, 1080), i * 10 * MS);
            assert_eq!(t.poll(i * 10 * MS), None);
        }
        assert_eq!(t.current(), (1920, 1080));
    }

    #[test]
    fn active_drag_does_not_settle_until_stable() {
        // Mirrors the probe: a NEW size every ~16 ms (per frame) through the drag.
        let mut t = ResizeTracker::new((1321, 859), SETTLE);
        let sizes = [
            (1320, 858),
            (1280, 811),
            (1158, 688),
            (895, 493),
            (770, 423),
            (735, 412),
        ];
        let mut now = 0i64;
        for &sz in &sizes {
            now += 16 * MS;
            t.observe(sz, now);
            // Never settles mid-drag: each new size resets the timer.
            assert_eq!(t.poll(now), None, "settled mid-drag at {sz:?}");
        }
        // Drag stops at the last size; WGC goes quiet (no more observe). Time passes.
        assert_eq!(t.poll(now + SETTLE - 1), None, "settled one tick early");
        assert_eq!(t.poll(now + SETTLE), Some((735, 412)), "did not settle");
        // Adopted; settles only once.
        assert_eq!(t.current(), (735, 412));
        assert_eq!(t.poll(now + SETTLE + 10 * MS), None);
    }

    #[test]
    fn settles_exactly_at_the_window_boundary() {
        let mut t = ResizeTracker::new((100, 100), SETTLE);
        t.observe((200, 200), 0);
        assert_eq!(t.poll(SETTLE - 1), None);
        assert_eq!(t.poll(SETTLE), Some((200, 200)));
    }

    #[test]
    fn revert_to_current_cancels_pending() {
        let mut t = ResizeTracker::new((800, 600), SETTLE);
        t.observe((640, 480), 0);
        // User drags back to the original size before the settle fires.
        t.observe((800, 600), 100 * MS);
        assert_eq!(t.poll(SETTLE + 1), None, "settled after revert");
        assert_eq!(t.current(), (800, 600));
    }

    #[test]
    fn a_new_resize_settles_after_a_prior_one() {
        let mut t = ResizeTracker::new((1000, 1000), SETTLE);
        t.observe((900, 900), 0);
        assert_eq!(t.poll(SETTLE), Some((900, 900)));
        // A second, later resize settles independently.
        let base = SETTLE + 5000 * MS;
        t.observe((800, 800), base);
        assert_eq!(t.poll(base + SETTLE - 1), None);
        assert_eq!(t.poll(base + SETTLE), Some((800, 800)));
        assert_eq!(t.current(), (800, 800));
    }

    #[test]
    fn monitor_switch_big_jump_settles_like_any_resize() {
        // The probe's 735x412 -> 922x517 style jump (DPI/monitor change).
        let mut t = ResizeTracker::new((735, 412), SETTLE);
        t.observe((922, 517), 0);
        t.observe((1014, 627), 30 * MS); // still adjusting after the jump
        assert_eq!(t.poll(30 * MS + SETTLE), Some((1014, 627)));
    }
}
