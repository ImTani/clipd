//! `audio::mixer` — the Slice-B (B4) software mixer for the always-first **Mix**
//! track (`AudioTrackKind::Mix`, container track 0).
//!
//! The Mix track is the −3 dB soft-clipped **sum** of two already-resampled 48 kHz
//! sources: the default-endpoint desktop loopback and the microphone (`SLICE-B-PLAN
//! §B4`, decision D3 "fan-out"). It replaces the B1/B2 interim where track 1 passed
//! the raw desktop loopback through (decision D2). One source → two tracks (the mic
//! also has its own standalone Mic track) is expressed by *fanning the mic's resampled
//! chunks* to this mixer, so nothing is captured or resampled twice.
//!
//! ## What this module is (pure) and is not (a thread)
//! [`TwoSourceMixer`] is 100 % safe, deterministic, deadline-agnostic logic —
//! `CLAUDE.md` puts the audio-clock pain in pure, exhaustively-tested math, and this
//! is that. The engine's `mix_process_thread` owns the WASAPI/COM/AAC objects and
//! feeds this core; the wall-clock warm-up grace (unblocking a mic that never opens)
//! is the thread's concern, surfaced here only as [`TwoSourceMixer::release_warmup`].
//!
//! ## Alignment model
//! Both inputs are **continuous from their anchor**: each source's [`StreamResampler`]
//! already synthesized silence for capture gaps (`§2.3`) and drift-corrected the rate
//! (`§2.4`), so a source's chunks tile the 48 kHz grid with no holes. The mixer picks
//! a global **anchor** = the earliest first-chunk PTS across the two sources, converts
//! each chunk's PTS to an absolute frame index off that anchor, and sums frame-for-
//! frame. A source that starts later (a mic that opens ~30 ms after the desktop) plays
//! silence until its anchor; the other plays alone there.
//!
//! ## Contiguity is load-bearing
//! The downstream AAC encoder is a **sample-counting clock** (an AU's PTS is
//! `anchor + au_index · frame_duration`, `mft_aac::stamp`), so the mixer's emitted
//! sample stream must be gap-free from the anchor or the whole track drifts. [`drain`]
//! therefore only ever advances a monotonic `emitted` cursor and silence-fills any
//! internal hole — the output frames `0..emitted` are always one contiguous run.
//!
//! [`StreamResampler`]: crate::audio::resample::StreamResampler
//! [`drain`]: TwoSourceMixer::drain

use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::units::TICKS_PER_SECOND;

use super::resample::ResampledChunk;

/// Samples per frame (interleaved stereo).
const CH: usize = CHANNELS as usize;

/// −3 dB of mix-bus headroom in linear gain (`10^(-3/20)`), applied to the summed
/// signal so two correlated full-scale sources do not hard-clip. A *solo* full-scale
/// source therefore lands at `0.708`, just under the soft clipper's linear [`KNEE`],
/// so normal levels pass through untouched (only genuine overshoot is softened). A
/// consequence: a desktop-only mix is 3 dB quieter than the raw loopback was under the
/// D2 pass-through — accepted, documented in DECISIONS "2026-07-08 — Slice B / B4".
pub const HEADROOM: f32 = 0.707_945_78;

/// Below this magnitude [`soft_clip`] is exactly unity — chosen so a −3 dB full-scale
/// solo signal ([`HEADROOM`] ≈ 0.708) is unaffected. Above it a C¹ cubic knee bends to
/// a hard ±1 limit.
const KNEE: f32 = 0.8;

/// Odd-symmetric soft clipper: exactly unity for `|x| ≤` [`KNEE`], a cubic-Hermite
/// knee up to `|x| = 1` (unity slope entering the knee, zero slope at the ±1 limit, so
/// it is C¹ at both joins), then a hard ±1 limit. Monotonic and bounded to `[-1, 1]`.
/// A `tanh`-style clipper (plan §B4) would compress moderate levels; this leaves the
/// normal range pristine and only tames overshoot from summing two sources.
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        return x;
    }
    if a >= 1.0 {
        return x.signum();
    }
    let w = 1.0 - KNEE;
    let t = (a - KNEE) / w; // 0..1 across the knee
                            // Cubic Hermite: p0 = KNEE, p1 = 1, m0 = w (unity slope in `a`-space), m1 = 0.
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let y = h00 * KNEE + h10 * w + h01; // (h11 · m1 term is 0)
    x.signum() * y
}

/// Absolute 48 kHz frame index of `pts` relative to `anchor` (round-half-away),
/// mirroring the resampler's tick↔frame grid. May be negative for a chunk that lands
/// before the anchor (a late-arriving earlier source once emission has begun).
fn frame_of(pts: i64, anchor: i64) -> i64 {
    let d = (pts - anchor) as i128;
    let num = d * SAMPLE_RATE_HZ as i128;
    // Round half away from zero.
    let den = TICKS_PER_SECOND as i128;
    let q = if num >= 0 {
        (num + den / 2) / den
    } else {
        (num - den / 2) / den
    };
    q as i64
}

/// Master-domain PTS (ticks) of absolute frame `frame` off `anchor`.
fn pts_of(frame: u64, anchor: i64) -> i64 {
    anchor + (frame as i128 * TICKS_PER_SECOND as i128 / SAMPLE_RATE_HZ as i128) as i64
}

/// One source's contiguous window of buffered, resampled samples plus the flags that
/// tell the mixer whether it may still deliver more (blocks emission) or is
/// silent/finished (never blocks).
struct SourceBuf {
    /// Whether this source contributes at all. A mic that is off is `false` → it is
    /// permanent silence and never gates the frontier.
    expected: bool,
    /// Absolute frame index of `buf`'s first frame; `None` until the first chunk.
    start: Option<u64>,
    /// Interleaved stereo f32 for frames `[start, start + frames())`.
    buf: Vec<f32>,
    /// Capture ended (thread stopped): silence from `start + frames()` on; never blocks.
    ended: bool,
    /// Warm-up released: an un-anchored expected source stops blocking (the thread gave
    /// up waiting for it) and is treated as silence until it actually shows up.
    released: bool,
}

impl SourceBuf {
    fn new(expected: bool) -> Self {
        Self {
            expected,
            start: None,
            buf: Vec::new(),
            ended: false,
            released: false,
        }
    }

    /// Buffered frame count.
    fn frames(&self) -> u64 {
        (self.buf.len() / CH) as u64
    }

    /// Absolute frame index one past the last buffered frame (0 if empty).
    fn extent(&self) -> u64 {
        self.start.map_or(0, |s| s + self.frames())
    }

    /// Append a chunk starting at absolute frame `cf`, keeping `buf` contiguous from
    /// `start`: a gap is silence-padded, an overlap is trimmed. `cf` is clamped ≥ 0 by
    /// the caller.
    fn place(&mut self, cf: u64, samples: &[f32]) {
        match self.start {
            None => {
                self.start = Some(cf);
                self.buf.extend_from_slice(samples);
            }
            Some(s) => {
                let next = s + self.frames();
                if cf > next {
                    self.buf
                        .resize(self.buf.len() + (cf - next) as usize * CH, 0.0);
                    self.buf.extend_from_slice(samples);
                } else if cf < next {
                    let drop = ((next - cf) as usize * CH).min(samples.len());
                    self.buf.extend_from_slice(&samples[drop..]);
                } else {
                    self.buf.extend_from_slice(samples);
                }
            }
        }
    }

    /// The first frame this source cannot yet supply a definite value for — the cap it
    /// imposes on emission. Absent / ended / released-but-unanchored → [`u64::MAX`]
    /// (silence, never blocks); un-anchored and still expected → `emitted` (block until
    /// it anchors); anchored and live → its buffered extent.
    fn frontier(&self, emitted: u64) -> u64 {
        if !self.expected || self.ended {
            return u64::MAX;
        }
        match self.start {
            Some(s) => s + self.frames(),
            None => {
                if self.released {
                    u64::MAX
                } else {
                    emitted
                }
            }
        }
    }

    /// Interleaved `(left, right)` at absolute frame `f`; silence outside `buf`.
    fn at(&self, f: u64) -> (f32, f32) {
        if let Some(s) = self.start {
            if f >= s {
                let i = (f - s) as usize * CH;
                if i + 1 < self.buf.len() {
                    return (self.buf[i], self.buf[i + 1]);
                }
            }
        }
        (0.0, 0.0)
    }

    /// Drop buffered frames with index `< upto` (consumed or stale-before-emission).
    fn discard_before(&mut self, upto: u64) {
        if let Some(s) = self.start {
            if upto > s {
                let drop_frames = (upto - s).min(self.frames());
                self.buf.drain(..drop_frames as usize * CH);
                self.start = Some(s + drop_frames);
            }
        }
    }
}

/// One mixed output run: interleaved stereo f32 at 48 kHz, PTS of its first frame in
/// the master domain. Contiguous with the previous [`MixChunk`] (no gap between runs).
#[derive(Debug, Clone)]
pub struct MixChunk {
    /// PTS (ticks) of the first frame.
    pub pts: i64,
    /// Interleaved stereo f32 (`frames × 2`) at 48 kHz.
    pub samples: Vec<f32>,
}

/// The two-source PTS-aligning mixer (Mix track). Feed it resampled chunks from the
/// desktop loopback and (optionally) the mic; pull mixed output with [`Self::drain`].
pub struct TwoSourceMixer {
    /// PTS of mixed frame 0 (the earliest first-chunk PTS seen). Lowered only before
    /// any output has been emitted.
    anchor: Option<i64>,
    /// Next mixed frame to emit (monotonic; the output is contiguous below it).
    emitted: u64,
    desktop: SourceBuf,
    mic: SourceBuf,
}

impl TwoSourceMixer {
    /// A mixer whose Mix track sums the desktop loopback and, when `mic_present`, the
    /// mic; with `mic_present = false` the Mix track is the desktop loopback alone
    /// (still through [`HEADROOM`] + [`soft_clip`]).
    pub fn new(mic_present: bool) -> Self {
        Self {
            anchor: None,
            emitted: 0,
            desktop: SourceBuf::new(true),
            mic: SourceBuf::new(mic_present),
        }
    }

    /// Add a desktop-loopback chunk.
    pub fn push_desktop(&mut self, chunk: &ResampledChunk) {
        self.push(true, chunk);
    }

    /// Add a mic chunk (a no-op-shaped call is fine even if `mic_present` was false,
    /// but the thread only calls this when the mic is present).
    pub fn push_mic(&mut self, chunk: &ResampledChunk) {
        self.push(false, chunk);
    }

    fn push(&mut self, is_desktop: bool, chunk: &ResampledChunk) {
        // Establish / lower the anchor. It may only drop before emission has started;
        // a later chunk that predates an already-used anchor is trimmed at read time.
        match self.anchor {
            None => self.anchor = Some(chunk.pts),
            Some(a) if self.emitted == 0 && chunk.pts < a => self.anchor = Some(chunk.pts),
            _ => {}
        }
        let anchor = self.anchor.expect("anchor set above");
        let cf = frame_of(chunk.pts, anchor).max(0) as u64;
        let src = if is_desktop {
            &mut self.desktop
        } else {
            &mut self.mic
        };
        src.place(cf, &chunk.samples);
    }

    /// The desktop capture ended (its thread stopped): the Mix plays the mic alone
    /// after the desktop's last buffered frame.
    pub fn desktop_ended(&mut self) {
        self.desktop.ended = true;
    }

    /// The mic capture ended: the Mix plays the desktop alone after the mic's last frame.
    pub fn mic_ended(&mut self) {
        self.mic.ended = true;
    }

    /// Stop blocking on a source that has not anchored yet (the thread's warm-up grace
    /// elapsed — e.g. a mic device that never opened). The mix proceeds with whoever is
    /// present; a source that shows up later simply joins from its own anchor.
    pub fn release_warmup(&mut self) {
        self.desktop.released = true;
        self.mic.released = true;
    }

    /// Both sources have ended (or the mic is absent) and nothing more will arrive — the
    /// thread should do a final [`Self::drain`] and flush the encoder.
    pub fn finished(&self) -> bool {
        self.desktop.ended && (self.mic.ended || !self.mic.expected)
    }

    /// Emit every mixed frame currently resolvable, or `None` if none are (still warming
    /// up, or waiting on a live source). Advances `emitted` and drops consumed input.
    /// Call repeatedly until it returns `None`.
    pub fn drain(&mut self) -> Option<MixChunk> {
        let anchor = self.anchor?;
        // Cap by whoever might still deliver more data, and by the furthest real sample
        // available — so once both sources are silent/ended we stop rather than emit
        // unbounded silence.
        let blocking = self
            .desktop
            .frontier(self.emitted)
            .min(self.mic.frontier(self.emitted));
        let data_extent = self.desktop.extent().max(self.mic.extent());
        let frontier = blocking.min(data_extent);
        if frontier <= self.emitted {
            return None;
        }

        let start = self.emitted;
        let n = (frontier - start) as usize;
        let mut samples = Vec::with_capacity(n * CH);
        for f in start..frontier {
            let (dl, dr) = self.desktop.at(f);
            let (ml, mr) = self.mic.at(f);
            samples.push(soft_clip((dl + ml) * HEADROOM));
            samples.push(soft_clip((dr + mr) * HEADROOM));
        }

        let pts = pts_of(start, anchor);
        self.emitted = frontier;
        self.desktop.discard_before(frontier);
        self.mic.discard_before(frontier);
        Some(MixChunk { pts, samples })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 48 kHz chunk of `frames` frames whose first frame is at `pts`, both channels
    /// set to `value`.
    fn chunk(pts: i64, frames: u32, value: f32) -> ResampledChunk {
        ResampledChunk {
            pts,
            frames,
            samples: vec![value; frames as usize * CH],
        }
    }

    /// Ticks spanned by `frames` at 48 kHz — the exact per-chunk PTS step.
    fn span(frames: u64) -> i64 {
        (frames as i128 * TICKS_PER_SECOND as i128 / SAMPLE_RATE_HZ as i128) as i64
    }

    fn drain_all(m: &mut TwoSourceMixer) -> Vec<MixChunk> {
        let mut out = Vec::new();
        while let Some(c) = m.drain() {
            out.push(c);
        }
        out
    }

    /// Concatenate every drained sample (channel 0) — the mixed left channel timeline.
    fn flat_left(chunks: &[MixChunk]) -> Vec<f32> {
        chunks
            .iter()
            .flat_map(|c| c.samples.iter().step_by(CH).copied())
            .collect()
    }

    #[test]
    fn soft_clip_is_unity_below_knee_and_bounded_above() {
        assert_eq!(soft_clip(0.0), 0.0);
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(HEADROOM), HEADROOM); // solo full-scale −3 dB passes
        assert_eq!(soft_clip(-KNEE), -KNEE);
        // Bounded to [-1, 1] at and beyond full scale.
        assert!(soft_clip(1.0).abs() <= 1.0 + 1e-6);
        assert!((soft_clip(2.0) - 1.0).abs() < 1e-6);
        assert!((soft_clip(-2.0) + 1.0).abs() < 1e-6);
        // C0 continuity at the knee join.
        assert!((soft_clip(KNEE + 1e-4) - KNEE).abs() < 1e-3);
    }

    #[test]
    fn soft_clip_is_monotonic_and_odd() {
        let mut prev = f32::NEG_INFINITY;
        let mut x = -1.5f32;
        while x <= 1.5 {
            let y = soft_clip(x);
            assert!(y >= prev - 1e-6, "not monotonic at x={x}: {y} < {prev}");
            prev = y;
            assert!((soft_clip(-x) + y).abs() < 1e-6, "not odd at x={x}");
            assert!(y.abs() <= 1.0 + 1e-6);
            x += 0.001;
        }
    }

    #[test]
    fn frame_and_pts_round_trip_on_the_grid() {
        // 480 frames @ 48 kHz = 100_000 ticks.
        assert_eq!(span(480), 100_000);
        assert_eq!(frame_of(100_000, 0), 480);
        assert_eq!(pts_of(480, 0), 100_000);
        // Round-half-away on a non-grid pts.
        assert_eq!(frame_of(104, 0), 0); // 104 ticks ≈ 0.499 frame → 0
        assert_eq!(frame_of(105, 0), 1); // ≈ 0.504 frame → 1
    }

    #[test]
    fn sums_two_aligned_signals_with_headroom() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 480, 0.3));
        m.push_mic(&chunk(0, 480, 0.2));
        let out = drain_all(&mut m);
        let left = flat_left(&out);
        assert_eq!(left.len(), 480);
        for &s in &left {
            assert!((s - 0.5 * HEADROOM).abs() < 1e-6);
        }
        // Contiguous from the anchor.
        assert_eq!(out[0].pts, 0);
    }

    #[test]
    fn exact_minus_3db_gain_for_solo_full_scale() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 480, 1.0));
        m.push_mic(&chunk(0, 480, 0.0)); // silent mic present
        let left = flat_left(&drain_all(&mut m));
        for &s in &left {
            assert!((s - HEADROOM).abs() < 1e-6, "expected −3 dB, got {s}");
        }
    }

    #[test]
    fn one_source_silent_passes_the_other() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 240, 0.4));
        m.push_mic(&chunk(0, 240, 0.0));
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 240);
        assert!(left.iter().all(|&s| (s - 0.4 * HEADROOM).abs() < 1e-6));
    }

    #[test]
    fn misaligned_anchors_play_solo_then_mixed() {
        // Mic starts 100 frames after desktop. Both anchor during warm-up, so the mixer
        // aligns them exactly: frames 0..100 are desktop-alone, 100..200 mixed.
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 200, 0.5)); // frames 0..200
        m.push_mic(&chunk(span(100), 100, 0.5)); // frames 100..200
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 200);
        for (i, &s) in left.iter().enumerate() {
            let expected = if i < 100 { 0.5 } else { 1.0 } * HEADROOM;
            assert!((s - expected).abs() < 1e-6, "frame {i}: {s}");
        }
    }

    #[test]
    fn blocks_until_both_sources_anchor() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 480, 0.5));
        // Mic expected but not yet anchored → nothing may be emitted.
        assert!(m.drain().is_none());
        m.push_mic(&chunk(0, 480, 0.5));
        assert!(m.drain().is_some());
    }

    #[test]
    fn warmup_release_unblocks_desktop_alone() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 480, 0.6));
        assert!(m.drain().is_none()); // still waiting on the mic
        m.release_warmup();
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 480);
        assert!(left.iter().all(|&s| (s - 0.6 * HEADROOM).abs() < 1e-6));
    }

    #[test]
    fn mic_ending_lets_desktop_play_alone() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 480, 0.5));
        m.push_mic(&chunk(0, 240, 0.5)); // mic only covers the first half
        m.mic_ended();
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 480);
        for (i, &s) in left.iter().enumerate() {
            let expected = if i < 240 { 1.0 } else { 0.5 } * HEADROOM;
            assert!((s - expected).abs() < 1e-6, "frame {i}: {s}");
        }
    }

    #[test]
    fn mic_absent_is_desktop_through_gain() {
        let mut m = TwoSourceMixer::new(false);
        m.push_desktop(&chunk(0, 480, 0.5));
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 480);
        assert!(left.iter().all(|&s| (s - 0.5 * HEADROOM).abs() < 1e-6));
    }

    #[test]
    fn output_is_contiguous_across_incremental_drains() {
        // Feed desktop in three pieces and mic in two, interleaving drains; the emitted
        // stream must be one gap-free run whose length equals the mixed frame count and
        // whose per-chunk PTS tile the grid exactly.
        let mut m = TwoSourceMixer::new(true);
        let mut chunks = Vec::new();

        m.push_desktop(&chunk(0, 100, 0.5));
        m.push_mic(&chunk(0, 100, 0.5));
        chunks.extend(drain_all(&mut m));

        m.push_desktop(&chunk(span(100), 150, 0.5));
        m.push_mic(&chunk(span(100), 150, 0.5));
        chunks.extend(drain_all(&mut m));

        m.push_desktop(&chunk(span(250), 50, 0.5));
        m.push_mic(&chunk(span(250), 50, 0.5));
        m.desktop_ended();
        m.mic_ended();
        chunks.extend(drain_all(&mut m));

        assert_eq!(flat_left(&chunks).len(), 300);
        // Per-chunk PTS are contiguous on the 48 kHz grid.
        let mut expected_frame = 0u64;
        for c in &chunks {
            assert_eq!(c.pts, pts_of(expected_frame, 0));
            expected_frame += (c.samples.len() / CH) as u64;
        }
    }

    #[test]
    fn internal_gap_in_one_source_is_silence_filled_not_shifted() {
        // Desktop has a hole (frames 100..200 missing); the mixer silence-pads it so the
        // mic (continuous) is never time-shifted.
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 100, 0.8)); // frames 0..100
        m.push_desktop(&chunk(span(200), 100, 0.8)); // frames 200..300 (gap 100..200)
        m.push_mic(&chunk(0, 300, 0.1)); // continuous 0..300
        m.desktop_ended();
        m.mic_ended();
        let left = flat_left(&drain_all(&mut m));
        assert_eq!(left.len(), 300);
        for (i, &s) in left.iter().enumerate() {
            let d = if (100..200).contains(&i) { 0.0 } else { 0.8 };
            let expected = (d + 0.1) * HEADROOM;
            assert!((s - expected).abs() < 1e-6, "frame {i}: {s}");
        }
    }

    #[test]
    fn both_ended_stops_without_emitting_infinite_silence() {
        let mut m = TwoSourceMixer::new(true);
        m.push_desktop(&chunk(0, 48, 0.5));
        m.push_mic(&chunk(0, 48, 0.5));
        m.desktop_ended();
        m.mic_ended();
        assert!(m.finished());
        let out = drain_all(&mut m);
        assert_eq!(flat_left(&out).len(), 48);
        // No further output once drained.
        assert!(m.drain().is_none());
    }
}
