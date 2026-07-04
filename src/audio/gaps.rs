//! `audio::gaps` — loopback silence-gap synthesis (`02-AV-SYNC-SPEC.md §2.3`).
//!
//! WASAPI loopback delivers **nothing** while the endpoint is silent (and a mic
//! can stall for a period or two during a USB hub power event). If packets are
//! appended without accounting for the missing time, the audio track becomes
//! shorter than the wall-clock span it covers, and everything after the quiet
//! moment plays early — the classic "clips are fine until the game goes quiet"
//! desync (`01-PROJECT-PLAN.md §3` pitfall 2).
//!
//! The fix, verbatim from `§2.3`: each packet's QPC PTS is compared to where the
//! previous packet said the next one should start; a positive gap beyond the
//! jitter threshold is filled with an exact run of digital silence, and a
//! negative gap (the device replayed time) drops the overlapped leading frames.
//!
//! Per packet: `gap = pts - (prev_pts + prev_frames * 10_000_000 / 48_000)`.
//!
//! - `|gap| <= 20_000 ticks` (2 ms): normal jitter — admit the packet unchanged.
//! - `gap > 20_000 ticks`: synthesize `round(gap * 48_000 / 10_000_000)` frames
//!   of silence, stamped to fill the hole, then admit the real packet.
//! - `gap < -20_000 ticks`: overlap — drop the overlapped leading frames of the
//!   new packet, admit the remainder contiguously.
//!
//! This module is pure logic (100% safe, no COM): one [`GapSynthesizer`] per
//! stream, driven packet-by-packet. It decides *what* to emit; the caller (the
//! capture/resample stage) produces the actual silence samples and trims the
//! overlap. The synthesizer is deliberately format-agnostic — it reasons only in
//! ticks and frame counts at a configured rate.
//!
//! **Rate parameter.** The spec writes the gap math with the literal `48_000`
//! because it assumes a 48 kHz canonical stream. clipd runs gap synthesis on the
//! *native-rate* input, before the resampler (so the resampler sees a continuous
//! stream), so [`GapSynthesizer::new`] takes the stream's rate; at 48 kHz it is
//! byte-for-byte the spec formula. The threshold itself is in ticks and so is
//! rate-independent.

use crate::spec_constants::audio::GAP_JITTER_THRESHOLD_TICKS;
use crate::spec_constants::units::TICKS_PER_SECOND;

/// What to do with an incoming audio packet, per `§2.3`. The caller applies the
/// decision *before* the real packet's own samples: prepend silence, or drop
/// leading frames, then admit the (possibly trimmed) packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// `|gap|` is within the jitter threshold (or this is the first packet):
    /// admit the packet unchanged, at its own QPC PTS.
    Admit,
    /// `gap` exceeds `+threshold`: emit `frames` of digital silence starting at
    /// `pts` to fill the hole, then admit the real packet unchanged.
    SynthesizeSilence {
        /// Number of 48 kHz silence frames to emit.
        frames: u32,
        /// PTS (ticks) of the first silence frame — where the previous packet
        /// said audio should continue.
        pts: i64,
    },
    /// `gap` is below `-threshold` (overlap — the device replayed time): drop
    /// `drop_frames` leading frames of the new packet and admit the remainder
    /// starting at `pts`. If `drop_frames` covers the whole packet the caller
    /// admits nothing (the packet is entirely overlap).
    DropOverlap {
        /// Number of leading 48 kHz frames of the new packet to discard.
        drop_frames: u32,
        /// PTS (ticks) at which the retained remainder begins (contiguous with
        /// the previously admitted audio).
        pts: i64,
    },
}

/// Per-stream silence-gap synthesizer (`§2.3`). Feed it every packet in arrival
/// order via [`Self::on_packet`]; it tracks the previous packet so the next
/// gap is measured from the spec's `prev_pts + prev_frames·ticks/rate` cursor.
#[derive(Debug, Clone)]
pub struct GapSynthesizer {
    /// Sample rate (Hz) the packets are at — governs the frame↔tick conversions.
    rate: u32,
    /// The previously admitted segment as `(start_pts, frame_count)`. `None`
    /// before the first packet. The "expected" start of the next packet is
    /// `start_pts + frames_to_ticks(frame_count)` — exactly the spec's
    /// `prev_pts + prev_frames * 10_000_000 / rate`.
    prev: Option<(i64, u32)>,
    /// Count of gaps filled with synthesized silence (diagnostic).
    silence_gaps: u64,
    /// Count of overlaps trimmed (diagnostic).
    overlaps: u64,
}

impl GapSynthesizer {
    /// A fresh synthesizer for a stream at `rate` Hz, with no prior packet.
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            prev: None,
            silence_gaps: 0,
            overlaps: 0,
        }
    }

    /// Process one packet: its QPC PTS (`§2.2`) and its 48 kHz frame count.
    /// Returns the [`GapAction`] the caller must apply, and advances the internal
    /// cursor to reflect what will have been admitted.
    pub fn on_packet(&mut self, pts: i64, frames: u32) -> GapAction {
        let Some((prev_pts, prev_frames)) = self.prev else {
            // First packet of the stream: nothing to fill against. Admit as-is.
            self.prev = Some((pts, frames));
            return GapAction::Admit;
        };

        let expected = prev_pts + frames_to_ticks(prev_frames, self.rate);
        let gap = pts - expected;

        if gap.abs() <= GAP_JITTER_THRESHOLD_TICKS {
            // Jitter (or an exact fit): admit the packet at its true QPC PTS.
            // §2.2 keeps QPC as truth, so the cursor tracks the real PTS.
            self.prev = Some((pts, frames));
            GapAction::Admit
        } else if gap > GAP_JITTER_THRESHOLD_TICKS {
            // A real silence gap: fill `round(gap·rate/ticks)` frames from
            // `expected`, then admit the real packet at its own PTS.
            let silence_frames = ticks_to_frames_round(gap, self.rate);
            self.silence_gaps += 1;
            self.prev = Some((pts, frames));
            GapAction::SynthesizeSilence {
                frames: silence_frames,
                pts: expected,
            }
        } else {
            // Overlap (gap < -threshold): the device handed back time we already
            // covered. Drop the overlapped leading frames; admit the remainder
            // contiguously from `expected`.
            let overlap_frames = ticks_to_frames_round(-gap, self.rate);
            let drop_frames = overlap_frames.min(frames);
            let remaining = frames - drop_frames;
            self.overlaps += 1;
            // The retained remainder starts at `expected` and runs `remaining`
            // frames; that becomes the new cursor.
            self.prev = Some((expected, remaining));
            GapAction::DropOverlap {
                drop_frames,
                pts: expected,
            }
        }
    }

    /// Number of gaps filled with synthesized silence so far (diagnostic).
    #[inline]
    pub fn silence_gaps(&self) -> u64 {
        self.silence_gaps
    }

    /// Number of overlaps trimmed so far (diagnostic).
    #[inline]
    pub fn overlaps(&self) -> u64 {
        self.overlaps
    }
}

/// Ticks spanned by `frames` at `rate` Hz, matching the spec's integer form
/// `frames * 10_000_000 / rate` (floored). `i128` intermediate prevents overflow
/// at large frame counts.
#[inline]
fn frames_to_ticks(frames: u32, rate: u32) -> i64 {
    (frames as i128 * TICKS_PER_SECOND as i128 / rate.max(1) as i128) as i64
}

/// Frames at `rate` Hz spanned by `ticks`, rounded to nearest (half away from
/// zero) — the spec's `round(gap * rate / 10_000_000)`. `ticks` is expected
/// non-negative here (callers pass `gap` or `-gap`); a non-positive value is
/// clamped to 0.
#[inline]
fn ticks_to_frames_round(ticks: i64, rate: u32) -> u32 {
    if ticks <= 0 {
        return 0;
    }
    let num = ticks as i128 * rate.max(1) as i128;
    let den = TICKS_PER_SECOND as i128;
    // Round half up: (num + den/2) / den.
    ((num + den / 2) / den) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests exercise the 48 kHz canonical case (rate == the spec's literal),
    /// so they read identically to the spec formulas.
    const R: u32 = 48_000;

    /// Ticks spanned by `n` whole 48 kHz frames (exact for the test cases used).
    fn frames_ticks(n: u32) -> i64 {
        frames_to_ticks(n, R)
    }

    #[test]
    fn first_packet_is_always_admitted() {
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(1_000_000, 480), GapAction::Admit);
        assert_eq!(g.silence_gaps(), 0);
        assert_eq!(g.overlaps(), 0);
    }

    #[test]
    fn contiguous_packets_admit_without_synthesis() {
        // Each 10 ms period is 480 frames = 100_000 ticks. Perfectly contiguous
        // packets must all admit, no gaps, no overlaps.
        let mut g = GapSynthesizer::new(R);
        let dur = frames_ticks(480);
        let mut pts = 5_000_000;
        for _ in 0..10 {
            assert_eq!(g.on_packet(pts, 480), GapAction::Admit);
            pts += dur;
        }
        assert_eq!(g.silence_gaps(), 0);
        assert_eq!(g.overlaps(), 0);
    }

    #[test]
    fn gap_exactly_at_threshold_is_jitter() {
        // §2.3 boundary: |gap| == 20_000 ticks (2 ms) is jitter — admit, no fill.
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(0, 480), GapAction::Admit);
        let expected = frames_ticks(480);
        // Positive gap of exactly the threshold.
        assert_eq!(
            g.on_packet(expected + GAP_JITTER_THRESHOLD_TICKS, 480),
            GapAction::Admit
        );
        assert_eq!(g.silence_gaps(), 0);
    }

    #[test]
    fn gap_one_tick_over_threshold_synthesizes() {
        // Just past the threshold must synthesize silence.
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(0, 480), GapAction::Admit);
        let expected = frames_ticks(480);
        let gap = GAP_JITTER_THRESHOLD_TICKS + 1; // 20_001 ticks
        let action = g.on_packet(expected + gap, 480);
        // round(20_001 * 48_000 / 10_000_000) = round(96.0048) = 96 frames.
        assert_eq!(
            action,
            GapAction::SynthesizeSilence {
                frames: 96,
                pts: expected,
            }
        );
        assert_eq!(g.silence_gaps(), 1);
    }

    #[test]
    fn one_second_of_silence_synthesizes_48000_frames() {
        // A full second of loopback silence: 48_000 frames of fill, stamped at
        // the expected continuation point.
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(0, 480), GapAction::Admit);
        let expected = frames_ticks(480);
        let one_second = TICKS_PER_SECOND;
        let action = g.on_packet(expected + one_second, 480);
        assert_eq!(
            action,
            GapAction::SynthesizeSilence {
                frames: 48_000,
                pts: expected,
            }
        );
    }

    #[test]
    fn after_silence_the_next_gap_measures_from_the_real_packet() {
        // The real packet is admitted at its own PTS after a fill, so a following
        // contiguous packet must admit cleanly (no double-count of the gap).
        let mut g = GapSynthesizer::new(R);
        g.on_packet(0, 480);
        let expected = frames_ticks(480);
        let real_pts = expected + TICKS_PER_SECOND;
        g.on_packet(real_pts, 480); // synthesizes, then admits real at real_pts
        let dur = frames_ticks(480);
        assert_eq!(g.on_packet(real_pts + dur, 480), GapAction::Admit);
        assert_eq!(g.silence_gaps(), 1);
    }

    #[test]
    fn overlap_exactly_at_threshold_is_jitter() {
        // §2.3 boundary: gap == -20_000 is still jitter (|gap| <= threshold).
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(0, 480), GapAction::Admit);
        let expected = frames_ticks(480);
        assert_eq!(
            g.on_packet(expected - GAP_JITTER_THRESHOLD_TICKS, 480),
            GapAction::Admit
        );
        assert_eq!(g.overlaps(), 0);
    }

    #[test]
    fn overlap_past_threshold_drops_leading_frames() {
        let mut g = GapSynthesizer::new(R);
        assert_eq!(g.on_packet(0, 480), GapAction::Admit);
        let expected = frames_ticks(480);
        let gap = -(GAP_JITTER_THRESHOLD_TICKS + 1); // -20_001 ticks
        let action = g.on_packet(expected + gap, 480);
        // round(20_001 * 48_000 / 10_000_000) = 96 frames dropped.
        assert_eq!(
            action,
            GapAction::DropOverlap {
                drop_frames: 96,
                pts: expected,
            }
        );
        assert_eq!(g.overlaps(), 1);
    }

    #[test]
    fn overlap_larger_than_packet_drops_the_whole_packet() {
        // If the overlap exceeds the packet, drop_frames is clamped to the packet
        // size (admit nothing) and the cursor does not advance.
        let mut g = GapSynthesizer::new(R);
        g.on_packet(0, 480);
        let expected = frames_ticks(480);
        // Overlap of ~2 full periods but a 480-frame packet.
        let gap = -frames_ticks(1000);
        let action = g.on_packet(expected + gap, 480);
        assert_eq!(
            action,
            GapAction::DropOverlap {
                drop_frames: 480,
                pts: expected,
            }
        );
        // Cursor unchanged (remaining = 0): the next contiguous packet at
        // `expected` still admits.
        assert_eq!(g.on_packet(expected, 480), GapAction::Admit);
    }

    #[test]
    fn frames_to_ticks_matches_spec_integer_form() {
        // §2.3 uses prev_frames * 10_000_000 / 48_000 (floored).
        assert_eq!(frames_to_ticks(48_000, R), 10_000_000); // exactly 1 s
        assert_eq!(frames_to_ticks(480, R), 100_000); // exactly 10 ms
                                                      // 1 frame = 10_000_000/48_000 = 208.33 → 208 (floored).
        assert_eq!(frames_to_ticks(1, R), 208);
    }

    #[test]
    fn ticks_to_frames_rounds_half_up() {
        // 208 ticks ≈ 0.9984 frame → 1; 104 ticks ≈ 0.4992 → 0.
        assert_eq!(ticks_to_frames_round(208, R), 1);
        assert_eq!(ticks_to_frames_round(104, R), 0);
        assert_eq!(ticks_to_frames_round(0, R), 0);
        assert_eq!(ticks_to_frames_round(-5, R), 0);
        assert_eq!(ticks_to_frames_round(10_000_000, R), 48_000);
    }
}
