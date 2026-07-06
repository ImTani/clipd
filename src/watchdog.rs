//! `watchdog` — pipeline liveness accounting (`02-AV-SYNC-SPEC.md §6.3`).
//!
//! [`PipelineStats`] tracks the three stage counters (capture → encode → mux) and
//! exposes the `frames_in − frames_out` divergence signal (≈2 s). [`Watchdog`]
//! wraps that flag with hysteresis so the M5 tray flips to WARNING once when a
//! threshold is crossed and back to OK on recovery (driven from the ring thread;
//! `engine.rs`). Queue-depth drops and save-duration warnings live at their
//! respective stages (the encoder input path and the save worker).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::warn;

use crate::spec_constants::watchdog::FRAMES_DIVERGENCE_MAX;

/// Shared per-stage frame counters (capture → encode → mux).
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    /// Grid slots produced by the capture stage.
    pub captured: Arc<AtomicU64>,
    /// Packets emitted by the encoder.
    pub encoded: Arc<AtomicU64>,
    /// Packets written by the muxer.
    pub muxed: Arc<AtomicU64>,
}

impl PipelineStats {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current `(captured, encoded, muxed)` counts.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.captured.load(Ordering::Relaxed),
            self.encoded.load(Ordering::Relaxed),
            self.muxed.load(Ordering::Relaxed),
        )
    }

    /// Log a WARNING if any stage has fallen behind the previous one by more than
    /// the spec's divergence threshold (`02-AV-SYNC-SPEC §6.3`).
    pub fn check_divergence(&self) {
        let (captured, encoded, muxed) = self.snapshot();
        let threshold = FRAMES_DIVERGENCE_MAX as u64;
        if captured.saturating_sub(encoded) > threshold {
            warn!(captured, encoded, "encode is falling behind capture (>2s)");
        }
        if encoded.saturating_sub(muxed) > threshold {
            warn!(encoded, muxed, "mux is falling behind encode (>2s)");
        }
    }

    /// Whether any stage is more than the `§6.3` divergence threshold behind the
    /// previous one — the "encoder stall / starvation" (or mux/disk stall) signal
    /// that drives the tray WARNING state (M5). Reads the live counters.
    pub fn is_diverged(&self) -> bool {
        let (captured, encoded, muxed) = self.snapshot();
        let threshold = FRAMES_DIVERGENCE_MAX as u64;
        captured.saturating_sub(encoded) > threshold || encoded.saturating_sub(muxed) > threshold
    }
}

/// The health level the [`Watchdog`] reports. Deliberately UI-neutral (the shell
/// maps it to a `TrayState`) so this logic module does not depend on the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogState {
    /// Healthy — the pipeline is keeping up.
    Ok,
    /// A `§6.3` threshold is crossed — degraded but running.
    Warning,
}

/// A tiny hysteresis wrapper that turns the raw `§6.3` divergence flag into
/// state *transitions* (WARNING when it first crosses, back to OK on recovery),
/// so the tray flips once per change rather than every poll. Pure + unit-tested.
#[derive(Debug, Default)]
pub struct Watchdog {
    /// Whether we are currently latched in the WARNING state.
    warned: bool,
}

impl Watchdog {
    /// A fresh watchdog in the OK state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current divergence flag (e.g. [`PipelineStats::is_diverged`]).
    /// Returns `Some(state)` **only on a transition** (OK→WARNING or WARNING→OK),
    /// `None` when the level is unchanged.
    pub fn observe(&mut self, diverged: bool) -> Option<WatchdogState> {
        match (self.warned, diverged) {
            (false, true) => {
                self.warned = true;
                Some(WatchdogState::Warning)
            }
            (true, false) => {
                self.warned = false;
                Some(WatchdogState::Ok)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_emits_only_on_transitions() {
        let mut wd = Watchdog::new();
        // Healthy → no signal.
        assert_eq!(wd.observe(false), None);
        // First divergence → WARNING.
        assert_eq!(wd.observe(true), Some(WatchdogState::Warning));
        // Still diverged → no repeat.
        assert_eq!(wd.observe(true), None);
        // Recovery → back to OK.
        assert_eq!(wd.observe(false), Some(WatchdogState::Ok));
        // Still healthy → no repeat.
        assert_eq!(wd.observe(false), None);
    }

    #[test]
    fn is_diverged_matches_the_spec_threshold() {
        let stats = PipelineStats::new();
        // Exactly at the threshold is NOT over it (§6.3 is strictly `>`).
        stats
            .captured
            .store(FRAMES_DIVERGENCE_MAX as u64, Ordering::Relaxed);
        stats.encoded.store(0, Ordering::Relaxed);
        stats.muxed.store(0, Ordering::Relaxed);
        assert!(!stats.is_diverged(), "== threshold is not diverged");
        // One past the threshold trips it.
        stats
            .captured
            .store(FRAMES_DIVERGENCE_MAX as u64 + 1, Ordering::Relaxed);
        assert!(stats.is_diverged(), "> threshold is diverged");
        // Mux falling behind encode also trips it.
        stats.captured.store(0, Ordering::Relaxed);
        stats
            .encoded
            .store(FRAMES_DIVERGENCE_MAX as u64 + 1, Ordering::Relaxed);
        stats.muxed.store(0, Ordering::Relaxed);
        assert!(stats.is_diverged(), "encode→mux divergence trips it too");
    }
}
