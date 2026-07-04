//! `audio::resample` — device-native → 48 kHz resampling with drift correction
//! (`02-AV-SYNC-SPEC.md §2.4`).
//!
//! Capture (`wasapi_stream`) delivers f32 stereo at the device's **native** rate,
//! stamped with QPC PTS. This stage turns that into the canonical **48 kHz**
//! stream the AAC encoder and muxer want, doing three jobs at once:
//!
//! 1. **Silence-gap synthesis (`§2.3`)** on the native input, via
//!    [`GapSynthesizer`]. Running it *before* the resampler means the resampler
//!    always sees a continuous stream, so a loopback silence never shortens the
//!    track. Filled silence is native-rate zeros.
//! 2. **Drift correction (`§2.4`)** via [`DriftWindow`] + [`DriftController`].
//!    Drift is measured feed-forward on the native device clock (native samples
//!    vs QPC span, over contiguous audio only — gaps are excluded, being
//!    QPC-exact by construction). Every 10 s the controller sets the resampler's
//!    ratio to `(48_000 / native) · (1 + applied_ppm/1e6)`, holding the 48 kHz
//!    output locked to QPC.
//! 3. **Resampling** via `rubato` `SincFixedIn` (sinc, fixed-input /
//!    variable-ratio — exactly what `§2.4` calls for).
//!
//! ## Output PTS
//! The 48 kHz output is a single continuous timeline anchored at the first
//! packet's QPC PTS: chunk PTS = `anchor + out_frames_emitted · ticks/48_000`.
//! This is honest sample counting *because* the stream is gap-filled (continuous)
//! and drift-corrected (locked to QPC) — the two properties `§2.2` says you must
//! have before you may count samples. The AAC priming offset (Task 4) and any
//! residual drift are the only remaining error terms, both inside the `§5`
//! budget and caught by the click/flash rig.
//!
//! ## `unsafe`
//! None — pure DSP over `rubato` and the safe `audio::{gaps,drift}` controllers.
//! The resampler math is deterministic, so this module is unit-tested in CI; only
//! the end-to-end A/V offset needs the click/flash rig.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use tracing::warn;

use crate::audio::drift::{DriftController, DriftWindow};
use crate::audio::gaps::{GapAction, GapSynthesizer};
use crate::audio::wasapi_stream::AudioPacket;
use crate::spec_constants::audio::drift::COMPUTE_INTERVAL_SECONDS;
use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::units::TICKS_PER_SECOND;

/// Input frames per `rubato` process call. 480 = 10 ms at 48 kHz — one WASAPI
/// period's worth, keeping latency and per-chunk overhead low.
const CHUNK_FRAMES: usize = 480;

/// Errors from the resampler.
#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    /// `rubato` resampler construction failed.
    #[error("resampler construction: {0}")]
    Construct(String),
    /// A `rubato` process/ratio call failed.
    #[error("resampler process: {0}")]
    Process(String),
}

/// One resampled 48 kHz output chunk: interleaved stereo f32, with the PTS of its
/// first frame in the master domain.
#[derive(Debug, Clone)]
pub struct ResampledChunk {
    /// PTS (ticks) of the first frame.
    pub pts: i64,
    /// Frame count (samples per channel) in this chunk.
    pub frames: u32,
    /// Interleaved stereo f32 samples (`frames × 2`), at 48 kHz.
    pub samples: Vec<f32>,
}

/// Per-stream native→48 kHz resampler with gap synthesis and drift correction.
/// Feed it capture packets in order via [`Self::process`]; call [`Self::finish`]
/// at end of stream to flush the resampler's internal delay line.
pub struct StreamResampler {
    native_rate: u32,
    nominal_ratio: f64,
    resampler: SincFixedIn<f32>,
    /// De-interleaved input accumulation (channel 0, channel 1) awaiting a full
    /// [`CHUNK_FRAMES`] to process.
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    gap: GapSynthesizer,
    drift_win: DriftWindow,
    drift_ctl: DriftController,
    /// PTS of the first output frame (set from the first packet).
    anchor_pts: Option<i64>,
    /// Total 48 kHz frames emitted so far (drives output PTS).
    out_frames: u64,
    /// PTS of the last contiguous real packet, for the drift span.
    last_contiguous_pts: Option<i64>,
    /// PTS at which the drift controller last ran.
    last_drift_pts: Option<i64>,
}

impl StreamResampler {
    /// Build a resampler for a stream captured at `native_rate` Hz.
    pub fn new(native_rate: u32) -> Result<Self, ResampleError> {
        let native_rate = native_rate.max(1);
        let nominal_ratio = SAMPLE_RATE_HZ as f64 / native_rate as f64;
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        // max relative ratio 1.1 comfortably covers the ±300 ppm drift range.
        let resampler =
            SincFixedIn::<f32>::new(nominal_ratio, 1.1, params, CHUNK_FRAMES, CHANNELS as usize)
                .map_err(|e| ResampleError::Construct(e.to_string()))?;

        Ok(Self {
            native_rate,
            nominal_ratio,
            resampler,
            in_l: Vec::new(),
            in_r: Vec::new(),
            gap: GapSynthesizer::new(native_rate),
            drift_win: DriftWindow::new(native_rate),
            drift_ctl: DriftController::new(),
            anchor_pts: None,
            out_frames: 0,
            last_contiguous_pts: None,
            last_drift_pts: None,
        })
    }

    /// The device's native capture rate (Hz) this resampler converts from.
    #[inline]
    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }

    /// The drift correction currently applied, in ppm (diagnostic / probe).
    #[inline]
    pub fn applied_ppm(&self) -> f64 {
        self.drift_ctl.applied_ppm()
    }

    /// Process one capture packet, returning any 48 kHz chunks that completed.
    pub fn process(&mut self, pkt: &AudioPacket) -> Result<Vec<ResampledChunk>, ResampleError> {
        if self.anchor_pts.is_none() {
            self.anchor_pts = Some(pkt.pts);
            self.last_drift_pts = Some(pkt.pts);
        }

        // Gap synthesis on the native stream (§2.3): fill silence / trim overlap
        // so the resampler input is continuous. Only contiguous real audio feeds
        // the drift window.
        match self.gap.on_packet(pkt.pts, pkt.frames) {
            GapAction::Admit => {
                if let Some(prev) = self.last_contiguous_pts {
                    let span = pkt.pts - prev;
                    self.drift_win.observe(pkt.pts, span, pkt.frames as u64);
                }
                self.push_interleaved(&pkt.samples);
            }
            GapAction::SynthesizeSilence { frames, .. } => {
                self.push_silence(frames);
                self.push_interleaved(&pkt.samples);
            }
            GapAction::DropOverlap { drop_frames, .. } => {
                let skip = drop_frames as usize * CHANNELS as usize;
                if skip < pkt.samples.len() {
                    self.push_interleaved(&pkt.samples[skip..]);
                }
            }
        }
        // A gap resets the contiguity anchor (the next span is measured fresh).
        self.last_contiguous_pts = Some(pkt.pts);

        self.maybe_update_drift(pkt.pts)?;
        self.drain_chunks()
    }

    /// Run the drift controller every `W = 10 s` and push the new ratio to the
    /// resampler (`§2.4`).
    fn maybe_update_drift(&mut self, now_pts: i64) -> Result<(), ResampleError> {
        let last = self.last_drift_pts.unwrap_or(now_pts);
        let interval = COMPUTE_INTERVAL_SECONDS * TICKS_PER_SECOND;
        if now_pts - last < interval {
            return Ok(());
        }
        self.last_drift_pts = Some(now_pts);
        let Some(err_ppm) = self.drift_win.err_ppm() else {
            return Ok(());
        };
        let dt = (now_pts - last) as f64 / TICKS_PER_SECOND as f64;
        let update = self.drift_ctl.update(err_ppm, dt);
        if update.panicked {
            warn!(
                err_ppm,
                "audio drift > 1000 ppm — device clock unreliable; correction clamped (§2.4)"
            );
        }
        let ratio = self.nominal_ratio * self.drift_ctl.ratio_multiplier();
        self.resampler
            .set_resample_ratio(ratio, true)
            .map_err(|e| ResampleError::Process(e.to_string()))
    }

    /// Drain the input buffer in [`CHUNK_FRAMES`] chunks through the resampler.
    fn drain_chunks(&mut self) -> Result<Vec<ResampledChunk>, ResampleError> {
        let mut chunks = Vec::new();
        while self.in_l.len() >= CHUNK_FRAMES {
            let l: Vec<f32> = self.in_l.drain(..CHUNK_FRAMES).collect();
            let r: Vec<f32> = self.in_r.drain(..CHUNK_FRAMES).collect();
            let out = self
                .resampler
                .process(&[l, r], None)
                .map_err(|e| ResampleError::Process(e.to_string()))?;
            if let Some(chunk) = self.emit(out) {
                chunks.push(chunk);
            }
        }
        Ok(chunks)
    }

    /// Flush the sub-chunk remainder at end of stream. Any input shorter than one
    /// chunk is zero-padded to a full chunk and processed; the resampler's own
    /// delay line (< `sinc_len` frames ≈ 2.7 ms) is left unflushed — well within
    /// the `§4` head/tail slack and simpler than a partial-flush dance.
    pub fn finish(&mut self) -> Result<Vec<ResampledChunk>, ResampleError> {
        let mut chunks = self.drain_chunks()?;
        if !self.in_l.is_empty() {
            self.in_l.resize(CHUNK_FRAMES, 0.0);
            self.in_r.resize(CHUNK_FRAMES, 0.0);
            let l: Vec<f32> = self.in_l.drain(..CHUNK_FRAMES).collect();
            let r: Vec<f32> = self.in_r.drain(..CHUNK_FRAMES).collect();
            let out = self
                .resampler
                .process(&[l, r], None)
                .map_err(|e| ResampleError::Process(e.to_string()))?;
            if let Some(chunk) = self.emit(out) {
                chunks.push(chunk);
            }
        }
        Ok(chunks)
    }

    /// Interleave a `rubato` planar output `[L, R]` into a [`ResampledChunk`],
    /// stamping the running PTS. Returns `None` for an empty output.
    fn emit(&mut self, out: Vec<Vec<f32>>) -> Option<ResampledChunk> {
        let frames = out.first().map(|c| c.len()).unwrap_or(0);
        if frames == 0 {
            return None;
        }
        let pts = self.anchor_pts.unwrap_or(0)
            + (self.out_frames as i128 * TICKS_PER_SECOND as i128 / SAMPLE_RATE_HZ as i128) as i64;
        let mut samples = Vec::with_capacity(frames * CHANNELS as usize);
        // rubato emits equal-length planar channels; interleave L/R.
        for (lv, rv) in out[0].iter().zip(out[1].iter()) {
            samples.push(*lv);
            samples.push(*rv);
        }
        self.out_frames += frames as u64;
        Some(ResampledChunk {
            pts,
            frames: frames as u32,
            samples,
        })
    }

    /// De-interleave interleaved stereo `samples` into the input buffers.
    fn push_interleaved(&mut self, samples: &[f32]) {
        let ch = CHANNELS as usize;
        for frame in samples.chunks_exact(ch) {
            self.in_l.push(frame[0]);
            self.in_r.push(frame[1]);
        }
    }

    /// Push `frames` of stereo silence into the input buffers.
    fn push_silence(&mut self, frames: u32) {
        self.in_l.extend(std::iter::repeat_n(0.0, frames as usize));
        self.in_r.extend(std::iter::repeat_n(0.0, frames as usize));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wasapi_stream::AudioStreamKind;

    /// Build a contiguous stereo packet of `frames` at `rate`, starting at `pts`,
    /// filled with a mild ramp (non-zero so resampler output is observable).
    fn packet(pts: i64, frames: u32, rate: u32) -> AudioPacket {
        let mut samples = Vec::with_capacity(frames as usize * 2);
        for i in 0..frames {
            let v = ((i % 100) as f32 / 100.0) - 0.5;
            samples.push(v);
            samples.push(v);
        }
        AudioPacket {
            stream: AudioStreamKind::Desktop,
            pts,
            frames,
            sample_rate: rate,
            samples,
            silent: false,
            discontinuity: false,
        }
    }

    fn frames_ticks(frames: u32, rate: u32) -> i64 {
        (frames as i128 * TICKS_PER_SECOND as i128 / rate as i128) as i64
    }

    #[test]
    fn constructs_for_common_rates() {
        for rate in [48_000u32, 44_100, 96_000] {
            assert!(StreamResampler::new(rate).is_ok(), "rate {rate}");
        }
    }

    #[test]
    fn passthrough_48k_preserves_total_frames_within_latency() {
        let mut rs = StreamResampler::new(48_000).unwrap();
        let mut total_out = 0u64;
        let mut pts = 0i64;
        let per = 480u32;
        // 50 packets = 24_000 input frames at ratio 1.0.
        for _ in 0..50 {
            for c in rs.process(&packet(pts, per, 48_000)).unwrap() {
                total_out += c.frames as u64;
            }
            pts += frames_ticks(per, 48_000);
        }
        for c in rs.finish().unwrap() {
            total_out += c.frames as u64;
        }
        // Output tracks input within the sinc delay line (< 2·sinc_len).
        let input = 50 * per as u64;
        let diff = (total_out as i64 - input as i64).unsigned_abs();
        assert!(diff < 300, "in {input} out {total_out} diff {diff}");
    }

    #[test]
    fn upsamples_44100_to_48000() {
        let mut rs = StreamResampler::new(44_100).unwrap();
        let mut total_out = 0u64;
        let mut pts = 0i64;
        let per = 441u32; // ~10 ms at 44.1 kHz
        for _ in 0..100 {
            for c in rs.process(&packet(pts, per, 44_100)).unwrap() {
                total_out += c.frames as u64;
            }
            pts += frames_ticks(per, 44_100);
        }
        for c in rs.finish().unwrap() {
            total_out += c.frames as u64;
        }
        let input = 100 * per as u64;
        let expected = input * 48_000 / 44_100;
        let diff = (total_out as i64 - expected as i64).unsigned_abs();
        // Within the resampler's warmup latency.
        assert!(
            diff < 400,
            "expected ~{expected}, got {total_out} (diff {diff})"
        );
    }

    #[test]
    fn output_pts_is_monotonic() {
        let mut rs = StreamResampler::new(48_000).unwrap();
        let mut pts = 1_000_000i64;
        let mut last = i64::MIN;
        for _ in 0..40 {
            for c in rs.process(&packet(pts, 480, 48_000)).unwrap() {
                assert!(c.pts > last, "pts went backwards: {} <= {last}", c.pts);
                last = c.pts;
            }
            pts += frames_ticks(480, 48_000);
        }
        // First chunk's PTS is anchored at the first packet.
        assert!(last >= 1_000_000);
    }

    #[test]
    fn silence_gap_lengthens_the_output() {
        // A 1 s loopback gap must inject ~48_000 frames of silence so the track
        // stays as long as wall-clock time.
        let mut with_gap = StreamResampler::new(48_000).unwrap();
        let mut without = StreamResampler::new(48_000).unwrap();

        let count = |rs: &mut StreamResampler, pkts: &[AudioPacket]| -> u64 {
            let mut n = 0;
            for p in pkts {
                for c in rs.process(p).unwrap() {
                    n += c.frames as u64;
                }
            }
            for c in rs.finish().unwrap() {
                n += c.frames as u64;
            }
            n
        };

        let dur = frames_ticks(480, 48_000);
        // Two contiguous packets.
        let contiguous = [packet(0, 480, 48_000), packet(dur, 480, 48_000)];
        // Same, but the second packet is 1 s late (a silence gap).
        let gapped = [
            packet(0, 480, 48_000),
            packet(dur + TICKS_PER_SECOND, 480, 48_000),
        ];

        let n_no_gap = count(&mut without, &contiguous);
        let n_gap = count(&mut with_gap, &gapped);
        let extra = n_gap as i64 - n_no_gap as i64;
        // ~48_000 frames of fill (within resampler latency tolerance).
        assert!(
            (47_000..49_000).contains(&extra),
            "expected ~48_000 extra frames, got {extra}"
        );
        assert_eq!(with_gap.gap.silence_gaps(), 1);
    }
}
