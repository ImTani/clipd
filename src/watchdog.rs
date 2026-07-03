//! `watchdog` — minimal pipeline liveness accounting for Milestone 1.
//!
//! The full `02-AV-SYNC-SPEC.md §6.3` watchdog (queue-depth drops, epoch restart,
//! save-duration warnings) lands with the ring buffer and epoch controller. For
//! the M1 dumb recorder this tracks the three stage counters and warns when a
//! stage falls behind by more than the spec's `frames_in − frames_out`
//! divergence threshold (≈2 s), which the tray will surface later.

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
}
