//! `audio::drift` — sample-rate drift measurement and correction
//! (`02-AV-SYNC-SPEC.md §2.4`).
//!
//! Even with every packet stamped by its QPC position (`§2.2`), the *samples*
//! still arrive at the audio device's crystal rate, which is off nominal by
//! 20–200 ppm on consumer hardware. 100 ppm is 6 ms/min — 30 ms over a 5-minute
//! buffer, comfortably audible lip-sync error if left uncorrected. This is THE
//! drift that AV-2 (the 10-minute test incumbents fail) exists to catch.
//!
//! The correction is a micro-resample ratio (applied later by `audio::resample`
//! via `rubato`, fixed-in/variable-ratio). This module is the pure controller
//! that decides the ratio; it holds no audio and does no DSP.
//!
//! Two pieces, both pure and unit-tested:
//!
//! - [`DriftWindow`] — accumulates `(qpc_span, samples)` observations over a
//!   sliding 30 s window and computes `err_ppm = (samples/48_000 −
//!   span_seconds) / span_seconds · 1e6`.
//! - [`DriftController`] — proportional-only (`ratio_adjust = −err_ppm`), clamped
//!   to ±300 ppm and slew-limited to 10 ppm per second, with a panic latch when
//!   `|err_ppm| > 1000` (a device that is resampling badly or lying — a
//!   user-visible hardware problem, not something to silently paper over).
//!
//! Sign convention: [`DriftController::applied_ppm`] is added to the nominal
//! resample ratio — `ratio = out/in = (48_000 / device_rate) · (1 +
//! applied_ppm / 1e6)`. When the device clock runs *fast* (more samples than QPC
//! time, `err_ppm > 0`), the correction is *negative*, telling the resampler to
//! emit proportionally fewer output frames so the 48 kHz timeline tracks QPC.

use std::collections::VecDeque;

use crate::spec_constants::audio::drift::{
    PANIC_PPM, RATIO_CLAMP_PPM, SLEW_PPM_PER_SECOND, WINDOW_SECONDS,
};
use crate::spec_constants::audio::SAMPLE_RATE_HZ;
use crate::spec_constants::units::TICKS_PER_SECOND;

/// One drift observation: over `span_ticks` of QPC time (100 ns units) the
/// stream delivered `samples` frames at (nominally) 48 kHz. `end_ticks` is the
/// QPC tick at the end of the observation, used only for sliding-window eviction.
#[derive(Debug, Clone, Copy)]
struct Obs {
    end_ticks: i64,
    span_ticks: i64,
    samples: u64,
}

/// Sliding-window drift measurement (`§2.4`). Observations are pushed as audio is
/// captured; anything older than the 30 s window (relative to the newest
/// observation's end) is evicted. [`Self::err_ppm`] returns the current estimate
/// once the window holds a usable span.
#[derive(Debug, Clone)]
pub struct DriftWindow {
    obs: VecDeque<Obs>,
    window_ticks: i64,
    sum_span: i64,
    sum_samples: u64,
}

impl Default for DriftWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftWindow {
    /// A fresh window sized to the spec's 30 s (`§2.4`).
    pub fn new() -> Self {
        Self {
            obs: VecDeque::new(),
            window_ticks: WINDOW_SECONDS * TICKS_PER_SECOND,
            sum_span: 0,
            sum_samples: 0,
        }
    }

    /// Record an observation: `span_ticks` of QPC time ending at `end_ticks`
    /// carried `samples` audio frames. Evicts observations that fall outside the
    /// trailing 30 s window. A non-positive `span_ticks` is ignored (nothing to
    /// measure).
    pub fn observe(&mut self, end_ticks: i64, span_ticks: i64, samples: u64) {
        if span_ticks <= 0 {
            return;
        }
        self.obs.push_back(Obs {
            end_ticks,
            span_ticks,
            samples,
        });
        self.sum_span += span_ticks;
        self.sum_samples += samples;

        let cutoff = end_ticks - self.window_ticks;
        while let Some(front) = self.obs.front() {
            // Evict whole observations whose end is at/before the cutoff. Keeping
            // whole observations (rather than splitting) keeps the estimate a
            // simple ratio of sums; at 10 ms granularity the ±1-observation edge
            // error is negligible against a 30 s window.
            if front.end_ticks <= cutoff {
                let old = self.obs.pop_front().expect("front exists");
                self.sum_span -= old.span_ticks;
                self.sum_samples -= old.samples;
            } else {
                break;
            }
        }
    }

    /// Total QPC span currently in the window, in ticks.
    #[inline]
    pub fn span_ticks(&self) -> i64 {
        self.sum_span
    }

    /// The current drift estimate in ppm, or `None` if the window holds no span
    /// yet. Positive means the device clock is *fast* (more samples than QPC
    /// time): `err_ppm = (samples/48_000 − span_s) / span_s · 1e6`.
    pub fn err_ppm(&self) -> Option<f64> {
        if self.sum_span <= 0 {
            return None;
        }
        let span_seconds = self.sum_span as f64 / TICKS_PER_SECOND as f64;
        let sample_seconds = self.sum_samples as f64 / SAMPLE_RATE_HZ as f64;
        Some((sample_seconds - span_seconds) / span_seconds * 1e6)
    }
}

/// The result of one controller step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftUpdate {
    /// The correction to apply, in ppm, after clamp + slew. Added to the nominal
    /// resample ratio (see the module-level sign convention).
    pub applied_ppm: f64,
    /// `true` when `|err_ppm|` exceeded the panic threshold this step — the
    /// device is drifting > 1000 ppm (0.1%). The correction stays clamped at
    /// ±300 ppm and the caller raises a tray warning (`§2.4`).
    pub panicked: bool,
}

/// Proportional-only drift controller (`§2.4`). One instance per stream. Call
/// [`Self::update`] every `W = 10 s` (the spec's compute interval) with the
/// window's `err_ppm` and the elapsed seconds; it clamps the target to ±300 ppm
/// and slews the applied correction toward it at ≤ 10 ppm/s.
#[derive(Debug, Clone)]
pub struct DriftController {
    applied_ppm: f64,
    clamp_ppm: f64,
    slew_ppm_per_second: f64,
    panic_ppm: f64,
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftController {
    /// A controller with the spec's ±300 ppm clamp, 10 ppm/s slew, and 1000 ppm
    /// panic threshold, starting from zero correction.
    pub fn new() -> Self {
        Self {
            applied_ppm: 0.0,
            clamp_ppm: RATIO_CLAMP_PPM,
            slew_ppm_per_second: SLEW_PPM_PER_SECOND,
            panic_ppm: PANIC_PPM,
        }
    }

    /// The correction currently applied, in ppm.
    #[inline]
    pub fn applied_ppm(&self) -> f64 {
        self.applied_ppm
    }

    /// The resample ratio multiplier the applied correction implies:
    /// `1 + applied_ppm / 1e6`. Multiply the nominal `48_000 / device_rate` by
    /// this to get the target output/input ratio.
    #[inline]
    pub fn ratio_multiplier(&self) -> f64 {
        1.0 + self.applied_ppm / 1e6
    }

    /// Advance the controller by `dt_seconds`, given the measured `err_ppm`.
    ///
    /// - Target correction is `−err_ppm`, clamped to ±300 ppm (`§2.4`: P-only;
    ///   the plant is a constant offset, so integral action only adds overshoot).
    /// - The applied correction moves toward the target by at most
    ///   `10 ppm × dt_seconds` (slew limit — an instantaneous step > ~50 ppm is
    ///   audible as pitch flicker).
    /// - When `|err_ppm| > 1000` the step is flagged `panicked`; the target is
    ///   already clamped at ±300, so the correction simply holds there.
    pub fn update(&mut self, err_ppm: f64, dt_seconds: f64) -> DriftUpdate {
        let panicked = err_ppm.abs() > self.panic_ppm;

        let target = (-err_ppm).clamp(-self.clamp_ppm, self.clamp_ppm);
        let max_step = self.slew_ppm_per_second * dt_seconds.max(0.0);
        let delta = (target - self.applied_ppm).clamp(-max_step, max_step);
        self.applied_ppm += delta;
        // Guard against any accumulation past the clamp (defensive; the target is
        // already clamped so this only matters if the clamp were ever tightened).
        self.applied_ppm = self.applied_ppm.clamp(-self.clamp_ppm, self.clamp_ppm);

        DriftUpdate {
            applied_ppm: self.applied_ppm,
            panicked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    // ── DriftWindow ──────────────────────────────────────────────────────────

    #[test]
    fn empty_window_has_no_estimate() {
        let w = DriftWindow::new();
        assert_eq!(w.err_ppm(), None);
    }

    #[test]
    fn perfect_clock_reads_zero_ppm() {
        // Exactly 48_000 samples per 1 s of QPC → 0 ppm.
        let mut w = DriftWindow::new();
        let mut end = 0;
        for _ in 0..5 {
            end += TICKS_PER_SECOND;
            w.observe(end, TICKS_PER_SECOND, 48_000);
        }
        assert!(approx(w.err_ppm().unwrap(), 0.0, 1e-6));
    }

    #[test]
    fn fast_clock_reads_positive_ppm() {
        // +100 ppm: 48_000 + 4.8 samples per second. Over 10 s, 48_048 extra? Use
        // a clean case: 100 ppm means sample_seconds/span_seconds = 1.0001.
        // 480_048 samples over 10 s of QPC → +100 ppm.
        let mut w = DriftWindow::new();
        w.observe(10 * TICKS_PER_SECOND, 10 * TICKS_PER_SECOND, 480_048);
        assert!(
            approx(w.err_ppm().unwrap(), 100.0, 0.5),
            "got {}",
            w.err_ppm().unwrap()
        );
    }

    #[test]
    fn slow_clock_reads_negative_ppm() {
        let mut w = DriftWindow::new();
        // 479_952 samples over 10 s → −100 ppm.
        w.observe(10 * TICKS_PER_SECOND, 10 * TICKS_PER_SECOND, 479_952);
        assert!(
            approx(w.err_ppm().unwrap(), -100.0, 0.5),
            "got {}",
            w.err_ppm().unwrap()
        );
    }

    #[test]
    fn window_evicts_observations_older_than_30s() {
        let mut w = DriftWindow::new();
        // Fill 40 s of 1 s observations at a perfect rate; only ~30 s stay.
        let mut end = 0;
        for _ in 0..40 {
            end += TICKS_PER_SECOND;
            w.observe(end, TICKS_PER_SECOND, 48_000);
        }
        // Span retained is within the 30 s window (whole-observation eviction may
        // keep exactly 30 observations = 30 s).
        assert!(w.span_ticks() <= WINDOW_SECONDS * TICKS_PER_SECOND);
        assert!(w.span_ticks() >= (WINDOW_SECONDS - 1) * TICKS_PER_SECOND);
    }

    #[test]
    fn eviction_keeps_recent_drift_not_stale() {
        // First 30 s at +200 ppm, then 30 s at 0 ppm. After the window slides, the
        // estimate should reflect the recent (near-zero) drift, not the old.
        let mut w = DriftWindow::new();
        let mut end = 0;
        for _ in 0..30 {
            end += TICKS_PER_SECOND;
            w.observe(end, TICKS_PER_SECOND, 48_010); // ~+208 ppm
        }
        assert!(w.err_ppm().unwrap() > 150.0);
        for _ in 0..30 {
            end += TICKS_PER_SECOND;
            w.observe(end, TICKS_PER_SECOND, 48_000); // 0 ppm
        }
        assert!(
            w.err_ppm().unwrap().abs() < 20.0,
            "stale drift leaked: {}",
            w.err_ppm().unwrap()
        );
    }

    #[test]
    fn non_positive_span_is_ignored() {
        let mut w = DriftWindow::new();
        w.observe(0, 0, 480);
        w.observe(0, -100, 480);
        assert_eq!(w.err_ppm(), None);
    }

    // ── DriftController ──────────────────────────────────────────────────────

    #[test]
    fn zero_error_holds_zero_correction() {
        let mut c = DriftController::new();
        let u = c.update(0.0, 10.0);
        assert_eq!(u.applied_ppm, 0.0);
        assert!(!u.panicked);
    }

    #[test]
    fn correction_opposes_error_sign() {
        // Device fast (+err) → negative correction (emit fewer output frames).
        let mut c = DriftController::new();
        let u = c.update(50.0, 10.0);
        assert!(u.applied_ppm < 0.0, "got {}", u.applied_ppm);
    }

    #[test]
    fn slew_limits_change_to_10ppm_per_second() {
        // A large error wants a big correction, but one 10 s step may move at most
        // 100 ppm (10 ppm/s · 10 s).
        let mut c = DriftController::new();
        let u = c.update(-500.0, 10.0); // target +300 (clamped), slew caps at 100
        assert!(approx(u.applied_ppm, 100.0, 1e-9), "got {}", u.applied_ppm);
    }

    #[test]
    fn full_300ppm_swing_takes_30s_of_slew() {
        // §2.4: a full 300 ppm swing takes 30 s. Three 10 s steps toward a clamped
        // target reach 300 ppm.
        let mut c = DriftController::new();
        c.update(-1_000.0, 10.0); // +100 (panic, but clamped target +300)
        c.update(-1_000.0, 10.0); // +200
        let u = c.update(-1_000.0, 10.0); // +300
        assert!(approx(u.applied_ppm, 300.0, 1e-9), "got {}", u.applied_ppm);
    }

    #[test]
    fn target_clamps_at_300ppm() {
        // Even after converging, the correction never exceeds ±300 ppm.
        let mut c = DriftController::new();
        for _ in 0..100 {
            c.update(-5_000.0, 10.0);
        }
        assert!(
            approx(c.applied_ppm(), 300.0, 1e-9),
            "got {}",
            c.applied_ppm()
        );
    }

    #[test]
    fn panic_latches_above_1000ppm_but_stays_clamped() {
        let mut c = DriftController::new();
        let u = c.update(1_500.0, 10.0);
        assert!(u.panicked);
        // Correction still bounded by the ±300 clamp and the slew.
        assert!(u.applied_ppm >= -300.0 && u.applied_ppm <= 0.0);
    }

    #[test]
    fn panic_boundary_is_strictly_greater_than_1000() {
        let mut c = DriftController::new();
        // Exactly 1000 ppm is not a panic (spec: |err_ppm| > 1000).
        assert!(!c.update(1_000.0, 10.0).panicked);
        assert!(c.update(1_000.1, 10.0).panicked);
    }

    #[test]
    fn ratio_multiplier_tracks_applied_ppm() {
        let mut c = DriftController::new();
        c.update(-100.0, 10.0); // moves +100 ppm toward target
        assert!(approx(c.ratio_multiplier(), 1.0 + 100.0 / 1e6, 1e-12));
    }

    #[test]
    fn converges_and_settles_at_the_correction() {
        // A steady +80 ppm device: after enough 10 s steps the correction settles
        // at −80 ppm and stops moving (target reached, slew delta → 0).
        let mut c = DriftController::new();
        for _ in 0..20 {
            c.update(80.0, 10.0);
        }
        assert!(
            approx(c.applied_ppm(), -80.0, 1e-6),
            "got {}",
            c.applied_ppm()
        );
        // One more step makes no change.
        let before = c.applied_ppm();
        c.update(80.0, 10.0);
        assert!(approx(c.applied_ppm(), before, 1e-9));
    }
}
