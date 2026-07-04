//! Pure event-detection and offset statistics for the A/V sync rig
//! (`02-AV-SYNC-SPEC.md §5`). No I/O, no hardware — fed a time series of video
//! luma and audio energy (from ffmpeg, see `measure.rs`), it finds the flash and
//! click events, pairs them, and reports the click-vs-flash offset distribution
//! and drift. Unit-tested against synthetic series so the measurement math is
//! trustworthy before any clip is analysed.

/// AV-1 budget: |click − flash| offset must be within one frame @ 60 fps.
/// `02-AV-SYNC-SPEC.md §5` (≤ ±16.7 ms; expected ≤ 10 ms).
pub const AV1_BUDGET_MS: f64 = 16.7;

/// AV-2 budget: the offset must drift by ≤ 5 ms between the first and last event
/// of a 10-minute recording. `§5` — THE incumbent-killer test.
pub const AV2_DRIFT_MS: f64 = 5.0;

/// AV-2 window: `§5` measures the offset "in minute 1 vs minute 10", so the
/// endpoint drift compares the mean over the first vs the last 60 s of events.
pub const AV2_WINDOW_S: f64 = 60.0;

/// One detected flash↔click pairing (seconds; offset = click − flash).
#[derive(Debug, Clone, Copy)]
pub struct Pairing {
    /// Flash time (video), seconds — the x-axis for the drift fit.
    pub flash_t: f64,
    /// Signed offset `click − flash`, seconds (positive = audio late).
    pub offset_s: f64,
}

/// Detected rising edges: the times at which `value` first crosses **above**
/// `threshold` after having been below it, with a `refractory_s` guard so a
/// multi-frame flash (or noise near the threshold) counts once. `samples` must be
/// time-ordered.
pub fn rising_edges(samples: &[(f64, f64)], threshold: f64, refractory_s: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut armed = true; // seen a below-threshold sample since the last fire
    let mut last = f64::NEG_INFINITY;
    for &(t, v) in samples {
        if v >= threshold {
            if armed && (t - last) >= refractory_s {
                out.push(t);
                last = t;
            }
            armed = false;
        } else {
            armed = true;
        }
    }
    out
}

/// Pair each flash with the nearest click within `max_skew_s`; flashes with no
/// click inside the window (e.g. a dropped click) are skipped rather than paired
/// to a distant, wrong event.
pub fn pair_events(flashes: &[f64], clicks: &[f64], max_skew_s: f64) -> Vec<Pairing> {
    let mut out = Vec::new();
    for &f in flashes {
        let nearest = clicks
            .iter()
            .copied()
            .min_by(|a, b| (a - f).abs().total_cmp(&(b - f).abs()));
        if let Some(c) = nearest {
            if (c - f).abs() <= max_skew_s {
                out.push(Pairing {
                    flash_t: f,
                    offset_s: c - f,
                });
            }
        }
    }
    out
}

/// Summary statistics over a set of pairings, with pass/fail against the `§5`
/// acceptance budgets.
#[derive(Debug, Clone)]
pub struct Report {
    /// Number of paired events.
    pub n: usize,
    /// Mean offset (ms). A non-zero constant is an AAC-delay / rig-latency
    /// constant (`§5`: "Failing AV-1 by a constant = AAC delay constant wrong").
    pub mean_ms: f64,
    /// Minimum offset (ms).
    pub min_ms: f64,
    /// Maximum offset (ms).
    pub max_ms: f64,
    /// Offset standard deviation (ms) — jitter.
    pub std_ms: f64,
    /// Least-squares drift across the recording (ms): slope of offset vs time,
    /// times the span. Robust to per-event jitter (averages all points), but
    /// includes any first-minute convergence transient.
    pub drift_lsq_ms: f64,
    /// Spec-literal AV-2 drift (ms): mean offset over the LAST [`AV2_WINDOW_S`]
    /// minus the mean over the FIRST — "minute 10 vs minute 1" (`§5`). `None` when
    /// the clip is too short for two disjoint windows (e.g. an AV-1 clip).
    pub drift_endpoint_ms: Option<f64>,
    /// AV-1: every paired offset within [`AV1_BUDGET_MS`].
    pub av1_pass: bool,
    /// AV-2: |drift| within [`AV2_DRIFT_MS`] — uses [`Self::drift_endpoint_ms`]
    /// (the spec definition) when available, else the least-squares drift.
    pub av2_pass: bool,
}

/// Build a [`Report`] from pairings, or `None` if there is nothing to summarize.
pub fn summarize(pairs: &[Pairing]) -> Option<Report> {
    if pairs.is_empty() {
        return None;
    }
    let offs: Vec<f64> = pairs.iter().map(|p| p.offset_s * 1000.0).collect();
    let n = offs.len();
    let mean = offs.iter().sum::<f64>() / n as f64;
    let min = offs.iter().copied().fold(f64::INFINITY, f64::min);
    let max = offs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let std = (offs.iter().map(|o| (o - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let drift_lsq = linear_drift_ms(pairs);
    let drift_endpoint = windowed_means(pairs, AV2_WINDOW_S).map(|(first, last)| last - first);
    // The spec measures minute-1 vs minute-10; fall back to the least-squares
    // drift on clips too short for two windows.
    let av2_drift = drift_endpoint.unwrap_or(drift_lsq);
    Some(Report {
        n,
        mean_ms: mean,
        min_ms: min,
        max_ms: max,
        std_ms: std,
        drift_lsq_ms: drift_lsq,
        drift_endpoint_ms: drift_endpoint,
        av1_pass: offs.iter().all(|o| o.abs() <= AV1_BUDGET_MS),
        av2_pass: av2_drift.abs() <= AV2_DRIFT_MS,
    })
}

/// Mean offset (ms) over the first vs the last `window_s` of events. `None` if the
/// span is shorter than two disjoint windows. Implements the `§5` "minute 1 vs
/// minute 10" comparison.
fn windowed_means(pairs: &[Pairing], window_s: f64) -> Option<(f64, f64)> {
    let t0 = pairs.first()?.flash_t;
    let t1 = pairs.last()?.flash_t;
    if t1 - t0 < 2.0 * window_s {
        return None;
    }
    let mean_in = |lo: f64, hi: f64| -> Option<f64> {
        let vs: Vec<f64> = pairs
            .iter()
            .filter(|p| p.flash_t >= lo && p.flash_t <= hi)
            .map(|p| p.offset_s * 1000.0)
            .collect();
        (!vs.is_empty()).then(|| vs.iter().sum::<f64>() / vs.len() as f64)
    };
    Some((mean_in(t0, t0 + window_s)?, mean_in(t1 - window_s, t1)?))
}

/// Least-squares slope of offset(ms) vs flash time(s), scaled by the time span:
/// the modelled offset change from the first to the last event. Robust to jitter,
/// unlike a raw last−first difference.
fn linear_drift_ms(pairs: &[Pairing]) -> f64 {
    if pairs.len() < 2 {
        return 0.0;
    }
    let n = pairs.len() as f64;
    let xs: Vec<f64> = pairs.iter().map(|p| p.flash_t).collect();
    let ys: Vec<f64> = pairs.iter().map(|p| p.offset_s * 1000.0).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for i in 0..pairs.len() {
        sxy += (xs[i] - mx) * (ys[i] - my);
        sxx += (xs[i] - mx).powi(2);
    }
    if sxx == 0.0 {
        return 0.0;
    }
    let slope = sxy / sxx; // ms per second
    let span = xs.last().unwrap() - xs.first().unwrap();
    slope * span
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a square-wave series: `value` is `high` for `pulse_s` at each of
    /// `pulses` events spaced `period_s` apart, else `low`, sampled every `dt`.
    fn square_series(
        pulses: usize,
        period_s: f64,
        pulse_s: f64,
        dt: f64,
        low: f64,
        high: f64,
        phase_s: f64,
    ) -> Vec<(f64, f64)> {
        let end = pulses as f64 * period_s + 1.0;
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < end {
            let mut v = low;
            for k in 0..pulses {
                let start = phase_s + k as f64 * period_s;
                if t >= start && t < start + pulse_s {
                    v = high;
                }
            }
            out.push((t, v));
            t += dt;
        }
        out
    }

    #[test]
    fn rising_edges_finds_each_pulse_once() {
        // 5 flashes, 2 s apart, 16 ms wide, sampled at 60 fps (≈16.7 ms).
        let s = square_series(5, 2.0, 0.05, 1.0 / 60.0, 0.0, 255.0, 0.5);
        let edges = rising_edges(&s, 128.0, 0.5);
        assert_eq!(edges.len(), 5, "one edge per flash");
        // Edges land near the pulse starts (0.5, 2.5, 4.5, ...).
        for (k, e) in edges.iter().enumerate() {
            let expect = 0.5 + k as f64 * 2.0;
            assert!(
                (e - expect).abs() < 0.05,
                "edge {k} at {e}, expected ~{expect}"
            );
        }
    }

    #[test]
    fn rising_edges_refractory_suppresses_double_counts() {
        // A flash that flickers (drops one sample mid-pulse) must still count once
        // thanks to the refractory window.
        let mut s = square_series(1, 2.0, 0.2, 1.0 / 240.0, 0.0, 255.0, 0.5);
        // Punch a single-sample dip into the middle of the pulse.
        if let Some(mid) = s.iter_mut().find(|(t, _)| (*t - 0.6).abs() < 1.0 / 480.0) {
            mid.1 = 0.0;
        }
        let edges = rising_edges(&s, 128.0, 0.3);
        assert_eq!(edges.len(), 1, "flicker within refractory counts once");
    }

    #[test]
    fn pairing_matches_nearest_and_drops_unpaired() {
        let flashes = vec![1.0, 3.0, 5.0];
        // Click for flash@1 is 8 ms late; flash@3 late by 9 ms; flash@5 has NO
        // click within the window (nearest is 0.5 s away).
        let clicks = vec![1.008, 3.009, 5.5];
        let pairs = pair_events(&flashes, &clicks, 0.05);
        assert_eq!(pairs.len(), 2, "the far click is not paired");
        assert!((pairs[0].offset_s - 0.008).abs() < 1e-9);
        assert!((pairs[1].offset_s - 0.009).abs() < 1e-9);
    }

    #[test]
    fn summarize_constant_offset_passes_av1_and_av2() {
        // A constant 8 ms audio-late offset: within AV-1, zero drift → AV-2 pass.
        let pairs: Vec<Pairing> = (0..300)
            .map(|k| {
                let f = k as f64 * 2.0;
                Pairing {
                    flash_t: f,
                    offset_s: 0.008,
                }
            })
            .collect();
        let r = summarize(&pairs).unwrap();
        assert!((r.mean_ms - 8.0).abs() < 1e-6);
        assert!(r.drift_lsq_ms.abs() < 1e-6);
        assert!(r.drift_endpoint_ms.unwrap().abs() < 1e-6);
        assert!(r.av1_pass && r.av2_pass);
    }

    #[test]
    fn summarize_flags_a_linear_drift() {
        // Offset ramps 0 → 12 ms over 600 s (a drift-controller failure): AV-1
        // still passes (all ≤ 16.7 ms) but AV-2 fails (drift > 5 ms).
        let n = 300;
        let span = 600.0;
        let pairs: Vec<Pairing> = (0..n)
            .map(|k| {
                let f = k as f64 / (n - 1) as f64 * span;
                let off = 0.012 * (f / span); // 0 → 12 ms
                Pairing {
                    flash_t: f,
                    offset_s: off,
                }
            })
            .collect();
        let r = summarize(&pairs).unwrap();
        assert!(
            (r.drift_lsq_ms - 12.0).abs() < 0.1,
            "lsq drift ~12 ms, got {}",
            r.drift_lsq_ms
        );
        let ep = r.drift_endpoint_ms.expect("600 s clip has two windows");
        assert!((ep - 10.8).abs() < 0.5, "endpoint drift ~10.8 ms, got {ep}");
        assert!(r.av1_pass, "12 ms peak is within AV-1");
        assert!(!r.av2_pass, "12 ms drift must fail AV-2");
    }

    #[test]
    fn endpoint_drift_ignores_a_first_minute_transient() {
        // The controller converges in the first 60 s (offset ramps 0 → 8 ms) then
        // holds flat at 8 ms. The least-squares fit sees a mild slope from the
        // transient, but the spec's minute-1-vs-minute-10 comparison is small →
        // AV-2 PASS. This is exactly why the endpoint metric is the gate.
        let span = 600.0;
        let n = 300;
        let pairs: Vec<Pairing> = (0..n)
            .map(|k| {
                let f = k as f64 / (n - 1) as f64 * span;
                let off = if f < 60.0 { 0.008 * (f / 60.0) } else { 0.008 };
                Pairing {
                    flash_t: f,
                    offset_s: off,
                }
            })
            .collect();
        let r = summarize(&pairs).unwrap();
        let ep = r.drift_endpoint_ms.expect("two windows");
        // Minute-1 mean ≈ 4 ms (ramp midpoint), minute-10 mean = 8 ms → ~4 ms,
        // under the 5 ms gate, so AV-2 PASS despite the lsq slope.
        assert!(ep < AV2_DRIFT_MS, "endpoint drift {ep} should pass AV-2");
        assert!(r.av2_pass);
    }

    #[test]
    fn summarize_flags_av1_violation() {
        // A constant 20 ms offset exceeds the one-frame budget.
        let pairs: Vec<Pairing> = (0..10)
            .map(|k| {
                let f = k as f64;
                Pairing {
                    flash_t: f,
                    offset_s: 0.020,
                }
            })
            .collect();
        let r = summarize(&pairs).unwrap();
        assert!(!r.av1_pass, "20 ms exceeds AV-1 ±16.7 ms");
    }
}
