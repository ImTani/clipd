//! `capture::pacing` — the constant-frame-rate (CFR) slot grid.
//!
//! Implements `02-AV-SYNC-SPEC.md §1.2/§1.3/§1.4` literally: the encoder must
//! receive **exactly `fps` frames per second per epoch**, each stamped with its
//! slot-boundary PTS, so the muxed video track is strictly CFR (editors hate
//! VFR — plan pitfall 13). This module is **pure, safe logic**: it decides which
//! output slot a frame arrival belongs to and whether each slot emits a fresh
//! frame or resubmits the previous one. It touches no COM and is exhaustively
//! unit-tested, including the spec's edge numbers.
//!
//! The capture thread owns the wall clock: it calls [`PacingGrid::on_arrival`]
//! for each WGC frame (with the frame's `SystemRelativeTime`) and
//! [`PacingGrid::poll`] at each slot deadline. The grid returns a [`SlotAction`]
//! carrying the exact PTS; the thread pairs it with the actual NV12 texture.
//!
//! ## Rules encoded here
//! - **Base** (`02-AV-SYNC-SPEC §1.2`): the epoch's grid origin is the
//!   `SystemRelativeTime` of the epoch's first captured frame.
//! - **Slot boundary** is `base + n·10_000_000/fps` computed exactly each time
//!   (never an accumulated rounded duration) via
//!   [`slot_boundary_ticks`](crate::spec_constants::video::slot_boundary_ticks).
//! - **PTS** (`§1.3`): the slot boundary, NOT the arrival time. Arrival only
//!   chooses the slot.
//! - **Keep-latest** (`§1.2`/`§1.4`): if several frames arrive before a slot is
//!   produced, only the newest is used; the rest are dropped (counted, never
//!   converted).
//! - **Gap/resubmit** (`§1.2`): at slot deadline + grace with no fresh frame,
//!   resubmit the previous frame at the new slot's PTS. `grace = grace_fraction
//!   · D`, `grace_fraction ∈ [0.25, 0.75]`, default 0.5.

use crate::spec_constants::units::TICKS_PER_SECOND;
use crate::spec_constants::video::{
    nominal_frame_duration_ticks, slot_boundary_ticks, DEFAULT_GRACE_FRACTION, GRACE_FRACTION_MAX,
    GRACE_FRACTION_MIN,
};

/// What a produced slot should emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotAction {
    /// Emit the newest captured frame at this slot's PTS (a fresh frame was
    /// available).
    Fresh {
        /// Output slot index within the epoch.
        slot: i64,
        /// Presentation timestamp in ticks (the exact slot boundary).
        pts: i64,
    },
    /// No fresh frame by the deadline — resubmit the previously emitted frame at
    /// this slot's PTS (static screen / occlusion; plan pitfall 13).
    Resubmit {
        /// Output slot index within the epoch.
        slot: i64,
        /// Presentation timestamp in ticks (the exact slot boundary).
        pts: i64,
    },
}

impl SlotAction {
    /// The slot index of this action.
    pub fn slot(&self) -> i64 {
        match *self {
            SlotAction::Fresh { slot, .. } | SlotAction::Resubmit { slot, .. } => slot,
        }
    }

    /// The PTS of this action.
    pub fn pts(&self) -> i64 {
        match *self {
            SlotAction::Fresh { pts, .. } | SlotAction::Resubmit { pts, .. } => pts,
        }
    }
}

/// The CFR slot grid for one capture epoch.
#[derive(Debug, Clone)]
pub struct PacingGrid {
    fps: u32,
    grace_ticks: i64,
    epoch_id: u32,
    /// Grid origin: `SystemRelativeTime` of the epoch's first frame. `None` until
    /// the first arrival.
    base: Option<i64>,
    /// Next output slot to produce.
    next_slot: i64,
    /// Slot of the newest arrival not yet consumed by a produced slot (keep-latest).
    pending_arrival: Option<i64>,
    /// Whether any frame has been emitted (so a resubmit has something to repeat).
    have_last: bool,
    fresh_count: u64,
    resubmit_count: u64,
    drop_count: u64,
}

impl PacingGrid {
    /// Create a grid for `fps` with `grace_fraction` (clamped to the spec's
    /// `[0.25, 0.75]`). Epoch id starts at 0; the base is set on the first
    /// arrival.
    pub fn new(fps: u32, grace_fraction: f64) -> Self {
        let clamped = grace_fraction.clamp(GRACE_FRACTION_MIN, GRACE_FRACTION_MAX);
        let grace_ticks = (nominal_frame_duration_ticks(fps) as f64 * clamped) as i64;
        Self {
            fps,
            grace_ticks,
            epoch_id: 0,
            base: None,
            next_slot: 0,
            pending_arrival: None,
            have_last: false,
            fresh_count: 0,
            resubmit_count: 0,
            drop_count: 0,
        }
    }

    /// Create a grid with the spec's default grace fraction (0.5).
    pub fn with_default_grace(fps: u32) -> Self {
        Self::new(fps, DEFAULT_GRACE_FRACTION)
    }

    /// Record a frame arrival at `tick` (its `SystemRelativeTime`). The first
    /// arrival of the epoch establishes the base. Keep-latest: displacing an
    /// unconsumed arrival counts a drop (the older frame is never converted).
    pub fn on_arrival(&mut self, tick: i64) {
        if self.base.is_none() {
            self.base = Some(tick);
        }
        let slot = self.slot_for(tick).expect("base set above");
        if self.pending_arrival.is_some() {
            // A previous arrival hasn't been consumed by a produced slot yet —
            // keep the newer one, drop the older (high-refresh / duplicate-in-slot).
            self.drop_count += 1;
        }
        self.pending_arrival = Some(slot);
    }

    /// Produce the next slot if its deadline (`slot boundary + grace`) has passed
    /// at `now`. Returns `None` if the epoch has no base yet or the deadline is
    /// still in the future.
    pub fn poll(&mut self, now: i64) -> Option<SlotAction> {
        let base = self.base?;
        let slot = self.next_slot;
        let pts = slot_boundary_ticks(base, slot, self.fps);
        let deadline = pts + self.grace_ticks;
        if now < deadline {
            return None;
        }

        let action = if self.pending_arrival.take().is_some() {
            self.have_last = true;
            self.fresh_count += 1;
            SlotAction::Fresh { slot, pts }
        } else if self.have_last {
            self.resubmit_count += 1;
            SlotAction::Resubmit { slot, pts }
        } else {
            // Base is set only by an arrival, which also sets `pending_arrival`,
            // so the first `poll` after base always takes the Fresh branch; this
            // branch is unreachable in practice. Emit nothing rather than a
            // resubmit with no prior frame.
            return None;
        };
        self.next_slot += 1;
        Some(action)
    }

    /// Restart the epoch: bump the epoch id and clear the grid so the next
    /// arrival re-establishes the base (`02-AV-SYNC-SPEC §0/§7` — a clip must not
    /// span epochs).
    pub fn restart_epoch(&mut self) {
        self.epoch_id += 1;
        self.base = None;
        self.next_slot = 0;
        self.pending_arrival = None;
        self.have_last = false;
        // Counters are cumulative diagnostics across epochs; not reset.
    }

    /// The slot a `tick` maps to (round to nearest), or `None` before the base is
    /// set. `N = round((tick − base) · fps / 10_000_000)` in 128-bit math.
    pub fn slot_for(&self, tick: i64) -> Option<i64> {
        let base = self.base?;
        let delta = (tick - base) as i128;
        let num = delta * self.fps as i128;
        // Round half up: add half a second's worth of the numerator scale.
        let half = TICKS_PER_SECOND as i128 / 2;
        let n = (num + half).div_euclid(TICKS_PER_SECOND as i128);
        Some(n as i64)
    }

    /// The exact PTS (slot boundary) of slot `n`, or `None` before the base is set.
    pub fn slot_pts(&self, n: i64) -> Option<i64> {
        self.base.map(|base| slot_boundary_ticks(base, n, self.fps))
    }

    /// The grid origin, if established.
    pub fn base(&self) -> Option<i64> {
        self.base
    }

    /// The current epoch id.
    pub fn epoch_id(&self) -> u32 {
        self.epoch_id
    }

    /// The grace window in ticks.
    pub fn grace_ticks(&self) -> i64 {
        self.grace_ticks
    }

    /// Frames emitted fresh, resubmitted, and dropped (keep-latest) so far.
    pub fn counters(&self) -> PacingCounters {
        PacingCounters {
            fresh: self.fresh_count,
            resubmits: self.resubmit_count,
            drops: self.drop_count,
        }
    }
}

/// Cumulative pacing diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacingCounters {
    /// Slots that emitted a freshly captured frame.
    pub fresh: u64,
    /// Slots that resubmitted the previous frame (static screen).
    pub resubmits: u64,
    /// Arrivals dropped before conversion (keep-latest / high-refresh).
    pub drops: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec_constants::units::TICKS_PER_SECOND;

    const FPS: u32 = 60;

    #[test]
    fn base_is_set_by_first_arrival() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        assert_eq!(grid.base(), None);
        assert_eq!(grid.slot_for(1_000), None);
        grid.on_arrival(5_000);
        assert_eq!(grid.base(), Some(5_000));
        assert_eq!(grid.slot_for(5_000), Some(0));
    }

    #[test]
    fn slot_boundaries_are_exact_and_non_accumulating() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(0);
        // 60 slots at 60 fps == exactly one second, with no accumulated rounding.
        assert_eq!(grid.slot_pts(60), Some(TICKS_PER_SECOND));
        assert_eq!(grid.slot_pts(120), Some(2 * TICKS_PER_SECOND));
        // Individual boundary is the floor of n·1e7/fps.
        assert_eq!(grid.slot_pts(1), Some(166_666));
        assert_eq!(grid.slot_pts(3), Some(500_000));
    }

    #[test]
    fn slot_for_rounds_to_nearest() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(0);
        // Just after slot 0's boundary → slot 0.
        assert_eq!(grid.slot_for(1_000), Some(0));
        // Near slot 1's boundary (166_666) → slot 1.
        assert_eq!(grid.slot_for(166_000), Some(1));
        // Exactly on a boundary → that slot.
        assert_eq!(grid.slot_for(TICKS_PER_SECOND), Some(60));
    }

    #[test]
    fn slot_for_round_half_goes_up() {
        // fps = 2 → D = 5_000_000, exact midpoint at 2_500_000.
        let mut grid = PacingGrid::with_default_grace(2);
        grid.on_arrival(0);
        assert_eq!(grid.slot_for(2_499_999), Some(0));
        assert_eq!(grid.slot_for(2_500_000), Some(1)); // round-half up
    }

    #[test]
    fn poll_waits_for_deadline_then_emits_fresh() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(0);
        let grace = grid.grace_ticks();
        // Before slot 0 deadline (pts 0 + grace): nothing.
        assert_eq!(grid.poll(grace - 1), None);
        // At the deadline exactly: emit fresh at pts 0.
        assert_eq!(
            grid.poll(grace),
            Some(SlotAction::Fresh { slot: 0, pts: 0 })
        );
    }

    #[test]
    fn static_screen_resubmits_after_the_first_fresh() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(0);
        // Advance well past several deadlines with no further arrivals.
        let a0 = grid.poll(10_000_000).unwrap();
        assert!(matches!(a0, SlotAction::Fresh { slot: 0, .. }));
        let a1 = grid.poll(10_000_000).unwrap();
        assert!(matches!(a1, SlotAction::Resubmit { slot: 1, .. }));
        let a2 = grid.poll(10_000_000).unwrap();
        assert!(matches!(a2, SlotAction::Resubmit { slot: 2, .. }));
        let c = grid.counters();
        assert_eq!(c.fresh, 1);
        assert_eq!(c.resubmits, 2);
    }

    #[test]
    fn duplicate_in_slot_keeps_later_and_counts_a_drop() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(1_000); // base, slot 0
        grid.on_arrival(2_000); // same slot 0 → older dropped
        assert_eq!(grid.counters().drops, 1);
        // The slot still emits one Fresh frame (the later one).
        let a = grid.poll(10_000_000).unwrap();
        assert!(matches!(a, SlotAction::Fresh { slot: 0, .. }));
        assert_eq!(grid.counters().fresh, 1);
    }

    #[test]
    fn high_refresh_drops_all_but_latest_before_conversion() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        // Four arrivals before slot 0 is produced (e.g. a 240 Hz source).
        grid.on_arrival(0);
        grid.on_arrival(10);
        grid.on_arrival(20);
        grid.on_arrival(30);
        assert_eq!(grid.counters().drops, 3);
        let a = grid.poll(10_000_000).unwrap();
        assert!(matches!(a, SlotAction::Fresh { slot: 0, .. }));
        assert_eq!(grid.counters().fresh, 1);
    }

    #[test]
    fn gap_exactly_at_grace_boundary_produces() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(0);
        // Consume slot 0.
        grid.poll(grid.grace_ticks()).unwrap();
        // Slot 1 boundary is 166_666; deadline = +grace.
        let deadline = grid.slot_pts(1).unwrap() + grid.grace_ticks();
        assert_eq!(grid.poll(deadline - 1), None);
        let a = grid.poll(deadline).unwrap();
        assert!(matches!(a, SlotAction::Resubmit { slot: 1, .. }));
    }

    #[test]
    fn epoch_restart_rebases_and_bumps_id() {
        let mut grid = PacingGrid::with_default_grace(FPS);
        grid.on_arrival(1_000);
        grid.poll(10_000_000).unwrap();
        assert_eq!(grid.epoch_id(), 0);

        grid.restart_epoch();
        assert_eq!(grid.epoch_id(), 1);
        assert_eq!(grid.base(), None);
        // New base is the first arrival of the new epoch; slot numbering restarts.
        grid.on_arrival(9_000_000);
        assert_eq!(grid.base(), Some(9_000_000));
        let a = grid.poll(9_000_000 + grid.grace_ticks()).unwrap();
        assert_eq!(
            a,
            SlotAction::Fresh {
                slot: 0,
                pts: 9_000_000
            }
        );
    }

    #[test]
    fn grace_fraction_is_clamped_to_spec_range() {
        let low = PacingGrid::new(FPS, 0.0);
        let high = PacingGrid::new(FPS, 1.0);
        let d = nominal_frame_duration_ticks(FPS) as f64;
        assert_eq!(low.grace_ticks(), (d * GRACE_FRACTION_MIN) as i64);
        assert_eq!(high.grace_ticks(), (d * GRACE_FRACTION_MAX) as i64);
    }
}
