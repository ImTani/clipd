//! Compiled-in constants from `02-AV-SYNC-SPEC.md` (v1.0, **FROZEN**) plus the
//! hard budgets in `01-PROJECT-PLAN.md §1`.
//!
//! `CLAUDE.md` mandate: *"Every constant from 02-AV-SYNC-SPEC.md lives in one
//! spec_constants.rs with a doc comment citing the spec section. No magic
//! numbers inline."* Downstream modules MUST reference these; a numeric literal
//! elsewhere that duplicates one of these values is a bug.
//!
//! The spec is frozen. Changing a value here is only valid after changing the
//! spec first; the citation on each item is the audit trail.

/// Working product name. The user-facing name is undecided (`CLAUDE.md`
/// "Naming placeholder"); this is the single source of truth referenced by the
/// tray tooltip, logs, and config header. A rename must touch only this
/// constant and the Cargo package name.
pub const PRODUCT_NAME: &str = "clipd";

/// Binary / crate name. Kept distinct from [`PRODUCT_NAME`] so renaming the
/// user-facing product need not rename the crate on disk.
pub const BINARY_NAME: &str = "clipd";

/// Config schema version. `01-PROJECT-PLAN.md §3` pitfall 30 and `§6`: the
/// config carries `config_version = 1`; a mismatch is surfaced, never silently
/// reset.
pub const CONFIG_VERSION: u32 = 1;

// ─────────────────────────────────────────────────────────────────────────────
// §0 — Units and the master clock domain
// ─────────────────────────────────────────────────────────────────────────────

/// Tick / timestamp units. `02-AV-SYNC-SPEC.md §0`.
///
/// ALL timestamps in the pipeline are `i64` ticks (identical to Windows
/// `MFTIME` / `REFERENCE_TIME`). One tick is 100 ns.
pub mod units {
    /// One tick = 100 nanoseconds. `§0`.
    pub const TICK_NANOSECONDS: i64 = 100;

    /// Ticks per second: `1 s = 10,000,000 ticks`. `§0`.
    pub const TICKS_PER_SECOND: i64 = 10_000_000;

    /// Ticks per millisecond: `1 ms = 10,000 ticks`. `§0`.
    pub const TICKS_PER_MILLISECOND: i64 = 10_000;

    /// Convert a whole number of milliseconds to ticks. Helper over
    /// [`TICKS_PER_MILLISECOND`] so callers never inline the `* 10_000`.
    pub const fn ms_to_ticks(ms: i64) -> i64 {
        ms * TICKS_PER_MILLISECOND
    }
}

/// Monotonicity guard, `§0`: any producer emitting `pts <= previous_pts` for a
/// stream MUST bump it to `previous_pts + 1` tick and increment a
/// `ts_violation` counter. This is a diagnostic canary; steady state is 0.
///
/// The reusable implementation lives in [`crate::clock::MonotonicGuard`]; this
/// constant fixes the logging cadence the spec dictates.
pub mod monotonicity {
    /// The `ts_violation` counter is logged every 60 s if nonzero. `§0` /
    /// `§6.3`.
    pub const TS_VIOLATION_LOG_INTERVAL_SECONDS: i64 = 60;
}

// ─────────────────────────────────────────────────────────────────────────────
// §1 — Video timestamping and the CFR grid
// ─────────────────────────────────────────────────────────────────────────────

/// Video pacing constants. `02-AV-SYNC-SPEC.md §1`.
pub mod video {
    use super::units::TICKS_PER_SECOND;

    /// Default output frame rate. `§1.2`. Tunable to 30/60/120; **120 is only
    /// exposed after Milestone 6 validation** (see [`FPS_120_GATED`]).
    pub const DEFAULT_FPS: u32 = 60;

    /// The frame rates the pipeline may be configured to. `§1.2`.
    pub const SUPPORTED_FPS: [u32; 3] = [30, 60, 120];

    /// 120 fps is gated behind Milestone 6 validation. `§1.2`. Until then the
    /// config validator rejects it (or warns), even though it is a listed
    /// [`SUPPORTED_FPS`] value.
    pub const FPS_120_GATED: u32 = 120;

    /// Default encode-height ceiling for the fixed output canvas (`config.encode
    /// .max_height`). The canvas is the capture monitor's resolution scaled to fit
    /// within this height (M4-2 amendment, DECISIONS 2026-07-05 / pitfall 11). 2160
    /// (4K) means 1080p/1440p monitors encode at native resolution and only 4K+ is
    /// capped — a generous default the user can lower to cap encode load.
    pub const DEFAULT_MAX_ENCODE_HEIGHT: u32 = 2160;

    /// Permitted range for `config.encode.max_height` (canvas ceiling). 480p floor;
    /// 4320 (8K) ceiling.
    pub const MAX_ENCODE_HEIGHT_MIN: u32 = 480;
    /// See [`MAX_ENCODE_HEIGHT_MIN`].
    pub const MAX_ENCODE_HEIGHT_MAX: u32 = 4320;

    /// Exact slot-boundary time in ticks for slot `n` of an epoch whose first
    /// frame is at `base`. `§1.2`:
    ///
    /// > slot N boundary = `base + N*10_000_000/fps` computed as integer
    /// > `base + (N*10_000_000)/fps` each time (no accumulation of a rounded D
    /// > — accumulation of 166,667 drifts +20 ms/hour).
    ///
    /// This is the canonical encoding of the CFR grid formula. It MUST be used
    /// instead of accumulating a per-frame duration. `i128` intermediate keeps
    /// `n * TICKS_PER_SECOND` from overflowing at large `n`.
    pub const fn slot_boundary_ticks(base: i64, n: i64, fps: u32) -> i64 {
        base + ((n as i128 * TICKS_PER_SECOND as i128) / fps as i128) as i64
    }

    /// Nominal frame duration `D = 10_000_000 / fps` ticks. `§1.2`.
    ///
    /// WARNING (per `§1.2`): this rounds down (166,666 at 60 fps, true value
    /// 166,666.67). It is provided for sizing/estimates ONLY. Never accumulate
    /// it to derive slot boundaries — use [`slot_boundary_ticks`], which keeps
    /// the exact rational.
    pub const fn nominal_frame_duration_ticks(fps: u32) -> i64 {
        TICKS_PER_SECOND / fps as i64
    }

    /// Default gap-grace as a fraction of the frame duration `D`. `§1.2`:
    /// grace = 0.5 × D (8.3 ms @ 60 fps).
    pub const DEFAULT_GRACE_FRACTION: f64 = 0.5;
    /// Tunable lower bound for the gap grace fraction. `§1.2` (0.25–0.75 D).
    pub const GRACE_FRACTION_MIN: f64 = 0.25;
    /// Tunable upper bound for the gap grace fraction. `§1.2` (0.25–0.75 D).
    pub const GRACE_FRACTION_MAX: f64 = 0.75;

    /// Number of GPU textures held to enable last-frame resubmission on a gap:
    /// last-delivered + in-flight. `§1.2`.
    pub const HELD_TEXTURES: usize = 2;
}

// ─────────────────────────────────────────────────────────────────────────────
// §2 — Audio timestamping
// ─────────────────────────────────────────────────────────────────────────────

/// Audio capture, drift, and AAC constants. `02-AV-SYNC-SPEC.md §2`.
pub mod audio {
    /// Canonical internal sample rate. `§2.1`. Everything is resampled to this.
    pub const SAMPLE_RATE_HZ: u32 = 48_000;

    /// Canonical internal channel count (mic mono → stereo by duplication at
    /// capture, before any DSP). `§2.1`.
    pub const CHANNELS: u16 = 2;

    /// Requested device period. `§2.1`: request the device default (10 ms on
    /// virtually all hardware). Do NOT request smaller periods.
    pub const PERIOD_MS: i64 = 10;

    /// Frames per 10 ms period at 48 kHz. `§2.1` (= 480).
    pub const PERIOD_FRAMES: u32 = 480;

    /// Capture buffer size as a multiple of the period. `§2.1`: 4 × period
    /// (40 ms) of overrun headroom.
    pub const BUFFER_PERIODS: u32 = 4;

    /// Bad-QPC tolerance before a stream is declared timestamp-unreliable and
    /// switches (this session) to sample counting. `§2.2` / `§6.3`.
    pub const BAD_QPC_PER_MINUTE_THRESHOLD: u32 = 100;

    /// Silence/overlap gap discrimination threshold: ±20,000 ticks (2 ms).
    /// `§2.3`. `|gap| <= this` is jitter (ignore); `gap >` this synthesizes
    /// silence; `gap < -this` is overlap (drop leading samples).
    pub const GAP_JITTER_THRESHOLD_TICKS: i64 = 20_000;

    /// Track layout, `§2.5`: two AAC tracks, desktop first, mic second. No
    /// mixed track in v1.
    pub const TRACK_DESKTOP: usize = 0;
    /// Mic is the second track. `§2.5`.
    pub const TRACK_MIC: usize = 1;

    /// Drift measurement/correction, `§2.4`.
    pub mod drift {
        /// `W` — recompute the correction every 10 s. `§2.4`.
        pub const COMPUTE_INTERVAL_SECONDS: i64 = 10;

        /// Sliding window over which `err_ppm` is measured. `§2.4` (30 s).
        pub const WINDOW_SECONDS: i64 = 30;

        /// Total ratio-adjust clamp: ±300 ppm. `§2.4`.
        pub const RATIO_CLAMP_PPM: f64 = 300.0;

        /// Slew limit on adjustment change: 10 ppm per second. `§2.4`.
        pub const SLEW_PPM_PER_SECOND: f64 = 10.0;

        /// If `|err_ppm|` exceeds this, stop chasing: clamp at
        /// [`RATIO_CLAMP_PPM`], set tray warning, log. `§2.4` (1000 ppm = 0.1%).
        pub const PANIC_PPM: f64 = 1000.0;

        /// Expected residual A/V error contribution from audio once converged.
        /// `§2.4` / `§5` (< 2 ms indefinitely).
        pub const RESIDUAL_BUDGET_MS: f64 = 2.0;

        /// Expected time for correction to converge after stream start. `§2.4`.
        pub const CONVERGENCE_SECONDS: i64 = 60;
    }

    /// AAC framing and encoder delay. `§2.6`.
    pub mod aac {
        /// Default AAC bitrate per track: 160 kbps CBR. `§2.6` (tunable
        /// 96–256 kbps).
        pub const BITRATE_DEFAULT_BPS: u32 = 160_000;
        /// Tunable lower bound for AAC bitrate. `§2.6`.
        pub const BITRATE_MIN_BPS: u32 = 96_000;
        /// Tunable upper bound for AAC bitrate. `§2.6`.
        pub const BITRATE_MAX_BPS: u32 = 256_000;

        /// Samples per AAC frame: 1024 (= 213,333.3 ticks = 21.33 ms). `§2.6`.
        pub const FRAME_SAMPLES: u32 = 1024;

        /// Fallback encoder priming delay if the Milestone-0 impulse
        /// measurement is skipped: assume 1024 samples (21.33 ms). `§2.6`. Some
        /// encoders use 2112; the measured value is compiled in with a runtime
        /// assert once known.
        pub const DELAY_SAMPLES_FALLBACK: u32 = 1024;

        /// The alternate priming value some encoders report, noted by `§2.6`
        /// for the measurement's sanity check.
        pub const DELAY_SAMPLES_ALT: u32 = 2112;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §3 — Ring buffer timestamps and eviction
// ─────────────────────────────────────────────────────────────────────────────

/// Ring buffer sizing and eviction. `02-AV-SYNC-SPEC.md §3`.
pub mod ring {
    /// Default retained buffer duration. `§3` (default 120 s).
    pub const DEFAULT_BUFFER_SECONDS: u32 = 120;

    /// Maximum retained buffer duration. `§3` (max 600 s).
    pub const MAX_BUFFER_SECONDS: u32 = 600;

    /// Byte-cap headroom multiplier over `buffer_seconds × est_bitrate`.
    /// `§3` / `§6.2` (× 1.5).
    pub const BYTE_CAP_HEADROOM: f64 = 1.5;

    /// Audio eviction slack: pop audio packets with
    /// `pts < video_front_pts − 500 ms`, guaranteeing audio always fully covers
    /// any surviving video range. `§3`.
    pub const AUDIO_EVICTION_SLACK_MS: i64 = 500;

    /// IDR / GOP cadence: an IDR every 2 s, closed GOPs, no B-frames in v1.
    /// `§3`.
    pub const IDR_INTERVAL_SECONDS: i64 = 2;

    /// `precise_mode` tightens the GOP to 1 s (~+10% bitrate) for tighter clip
    /// starts. `§3` (tunable, default off).
    pub const PRECISE_MODE_IDR_INTERVAL_SECONDS: i64 = 1;

    /// GOP length in frames = `idr_interval_seconds × fps`. `§3`
    /// (`gop_frames = 2 × fps`).
    pub const fn gop_frames(idr_interval_seconds: i64, fps: u32) -> u32 {
        (idr_interval_seconds * fps as i64) as u32
    }

    /// Estimated audio bitrate for the byte cap: two AAC tracks at the `§2.6`
    /// default (160 kbps each) — the "+0.4" audio addend in the `§6.2` table.
    pub const EST_AUDIO_BPS: u64 = 2 * 160_000;

    /// Estimated total stream bitrate (bits/s) for the byte cap, from the `§6.2`
    /// table: H.264 video average (by resolution tier, scaled linearly by fps) plus
    /// two AAC tracks. Tiers are the `§6.2` rows — 1080p60 → 16, 1440p60 → 26,
    /// 4K60 → 50 Mbps of video — selected by frame pixel area. Used only to size
    /// the byte cap (a safety ceiling with 1.5× headroom); real bitrate is CQP
    /// content-adaptive and the `§6.2` auto-QP-relief rule handles sustained
    /// overshoot, so an estimate is sufficient.
    pub fn est_bitrate_bps(width: u32, height: u32, fps: u32) -> u64 {
        let area = width as u64 * height as u64;
        let video_mbps_at_60: f64 = if area <= 1920 * 1080 {
            16.0
        } else if area <= 2560 * 1440 {
            26.0
        } else {
            50.0
        };
        let video_bps = (video_mbps_at_60 * 1_000_000.0 * fps as f64 / 60.0) as u64;
        video_bps + EST_AUDIO_BPS
    }

    /// Byte cap in bytes = `buffer_seconds × est_bitrate × 1.5` headroom.
    /// `§3` / `§6.2` ("Byte cap = table × 1.5").
    pub fn byte_cap_bytes(buffer_seconds: u32, est_bitrate_bps: u64) -> u64 {
        let bytes_per_second = est_bitrate_bps / 8;
        ((buffer_seconds as u64 * bytes_per_second) as f64 * BYTE_CAP_HEADROOM) as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §4 — Save-path rebasing / the mux contract
// ─────────────────────────────────────────────────────────────────────────────

/// fMP4 container numbers and the save/rebase contract. `02-AV-SYNC-SPEC.md §4`.
pub mod mux {
    /// Movie timescale. `§4.5` (1000).
    pub const MOVIE_TIMESCALE: u32 = 1000;

    /// Video track timescale = `fps × 1000` (60,000 at 60 fps). `§4.5`. With a
    /// constant [`VIDEO_SAMPLE_DELTA`] this is exact CFR and keeps
    /// 59.94-family rates representable.
    pub const fn video_timescale(fps: u32) -> u32 {
        fps * 1000
    }

    /// Video sample delta, constant 1000. `§4.5`.
    pub const VIDEO_SAMPLE_DELTA: u32 = 1000;

    /// Audio track timescale. `§4.5` (48,000).
    pub const AUDIO_TIMESCALE: u32 = 48_000;

    /// Audio sample delta. `§4.5` (1024).
    pub const AUDIO_SAMPLE_DELTA: u32 = 1024;

    /// One `moof`/`mdat` fragment per 1 s of content. `§4.6`. A crash mid-write
    /// loses at most the final fragment.
    pub const FRAGMENT_SECONDS: i64 = 1;

    /// Atomic-write temp suffix: write `name.mp4.part`, `FlushFileBuffers`,
    /// rename to `name.mp4`. `§4.7`.
    pub const PART_SUFFIX: &str = ".part";

    /// Worst-case pre-roll slack when rebasing: one GOP = 2 s. `§4.2` (see
    /// [`super::ring::IDR_INTERVAL_SECONDS`]).
    pub const PREROLL_SLACK_SECONDS: i64 = 2;
}

// ─────────────────────────────────────────────────────────────────────────────
// §5 — Sync budget and acceptance criteria
// ─────────────────────────────────────────────────────────────────────────────

/// End-to-end sync error budget and the AV-1..AV-5 acceptance thresholds.
/// `02-AV-SYNC-SPEC.md §5`.
pub mod sync_budget {
    /// Video grid quantization bound at 60 fps. `§5` (±8.3 ms).
    pub const VIDEO_GRID_QUANT_MS: f64 = 8.3;
    /// Audio QPC stamp accuracy. `§5` (±0.5 ms).
    pub const AUDIO_QPC_ACCURACY_MS: f64 = 0.5;
    /// Residual drift after control. `§5` (±2.0 ms).
    pub const RESIDUAL_DRIFT_MS: f64 = 2.0;
    /// Muxer rounding. `§5` (±0.01 ms).
    pub const MUXER_ROUNDING_MS: f64 = 0.01;
    /// Total RSS worst-ish end-to-end sync error. `§5` (≈ ±9 ms).
    pub const TOTAL_RSS_MS: f64 = 9.0;

    /// AV-1: 30 s clip, click-vs-flash offset must be ≤ one frame @ 60 fps.
    /// `§5` (±16.7 ms; expected ≤ 10 ms).
    pub const AV1_MAX_OFFSET_MS: f64 = 16.7;
    /// AV-1 expected result per budget. `§5`.
    pub const AV1_EXPECTED_OFFSET_MS: f64 = 10.0;

    /// AV-2 (drift): 10-minute recording; minute-1 vs minute-10 offset must
    /// differ by ≤ 5 ms. `§5` — the test incumbents fail.
    pub const AV2_MAX_DRIFT_MS: f64 = 5.0;

    /// AV-4 (device chaos): recovery gap ≤ 750 ms (250 ms debounce + 500 ms
    /// rebuild). `§5` / `§7`.
    pub const AV4_MAX_RECOVERY_GAP_MS: i64 = 750;
}

// ─────────────────────────────────────────────────────────────────────────────
// §6 — Dictated tuning tables
// ─────────────────────────────────────────────────────────────────────────────

/// Per-vendor CQP encoder quality defaults and the auto-relief rule.
/// `02-AV-SYNC-SPEC.md §6.1` / `§6.2`.
pub mod encoder {
    /// NVENC CQ defaults, indexed `[1080p60, 1440p60, 4K60]`. `§6.1`.
    pub const NVENC_CQ: [u8; 3] = [23, 23, 24];
    /// AMF QP defaults, indexed `[1080p60, 1440p60, 4K60]`. `§6.1`.
    pub const AMF_QP: [u8; 3] = [21, 21, 22];
    /// QSV ICQ defaults, indexed `[1080p60, 1440p60, 4K60]`. `§6.1`.
    pub const QSV_ICQ: [u8; 3] = [22, 22, 23];

    /// Auto-QP-relief default. `§6.2`: if the byte cap evicts below 90% of
    /// `buffer_seconds` for > 60 s continuously, raise QP by 1 for the session
    /// (`auto_qp_relief = true`, default on).
    pub const AUTO_QP_RELIEF_DEFAULT: bool = true;
    /// Fill fraction below which sustained eviction triggers QP relief. `§6.2`.
    pub const RELIEF_FILL_FRACTION: f64 = 0.90;
    /// Sustained duration before QP relief engages. `§6.2` (> 60 s).
    pub const RELIEF_SUSTAIN_SECONDS: i64 = 60;
}

/// Watchdog trigger thresholds. `02-AV-SYNC-SPEC.md §6.3`.
pub mod watchdog {
    /// Encoder input queue depth over which we drop-before-convert and count.
    /// `§6.3` (> 6 frames).
    pub const ENCODER_QUEUE_DEPTH_MAX: usize = 6;

    /// `frames_in − frames_out` divergence over which the tray goes WARNING and
    /// we keep dropping. `§6.3` (> 120 = 2 s @ 60 fps).
    pub const FRAMES_DIVERGENCE_MAX: i64 = 120;

    /// No WGC frame AND no resubmit possible for this long → epoch restart.
    /// `§6.3` (> 1 s).
    pub const NO_WGC_FRAME_RESTART_MS: i64 = 1000;

    /// No audio event on a stream for this long → stream rebuild (§7). `§6.3`
    /// (> 500 ms).
    pub const NO_AUDIO_EVENT_REBUILD_MS: i64 = 500;

    /// Save duration over which we log a WARN (disk suspect). `§6.3`
    /// (> 1000 ms).
    pub const SAVE_DURATION_WARN_MS: i64 = 1000;
}

/// Expected steady-state resource envelope. `02-AV-SYNC-SPEC.md §6.4` and the
/// hard budgets in `01-PROJECT-PLAN.md §1`. The `*_BUDGET_*` values are
/// CI/manual-test failure thresholds, not aspirations.
pub mod resource_budget {
    /// Total process CPU budget while buffering. `§6.4` / plan `§1` (fail over
    /// 2%).
    pub const CPU_BUDGET_PERCENT: f64 = 2.0;
    /// GPU 3D-engine budget attributable to us. `§6.4` / plan `§1` (fail over
    /// 3%; expected ~0% on the VideoProcessor path).
    pub const GPU_3D_BUDGET_PERCENT: f64 = 3.0;
    /// Process RAM beyond the ring buffer. `§6.4` / plan `§1` (fail over
    /// 75 MB).
    pub const RAM_OVERHEAD_BUDGET_MB: u64 = 75;
    /// Game frametime impact at the 99th percentile. `§6.4` (fail over 4%).
    pub const FRAMETIME_IMPACT_BUDGET_PERCENT: f64 = 4.0;

    /// Binary size budget: < 10 MB, zero runtime dependencies. plan `§1`; the
    /// `just release` recipe prints the built size against this.
    pub const BINARY_SIZE_BUDGET_BYTES: u64 = 10 * 1024 * 1024;

    /// Save-clip latency budget: file exists and is playable < 1 s after
    /// hotkey. plan `§1`.
    pub const SAVE_LATENCY_BUDGET_MS: i64 = 1000;
}

// ─────────────────────────────────────────────────────────────────────────────
// §7 — Device-change state machine timing
// ─────────────────────────────────────────────────────────────────────────────

/// Audio/video device-change timing. `02-AV-SYNC-SPEC.md §7`.
pub mod device {
    /// `IMMNotificationClient` events are debounced 250 ms (Windows fires
    /// bursts of 3–6 events on a default switch). `§7`.
    pub const IMM_DEBOUNCE_MS: i64 = 250;

    /// Audio stream rebuild budget: release, re-enumerate, initialize, start.
    /// `§7` (500 ms).
    pub const AUDIO_REBUILD_BUDGET_MS: i64 = 500;

    /// Total worst-case audio hole across a default switch, filled with
    /// synthesized silence: debounce + rebuild. `§7` (750 ms).
    pub const AUDIO_WORST_CASE_HOLE_MS: i64 = 750;

    /// Video device-loss epoch-restart budget (device removed/reset,
    /// sleep/resume, driver update). `§7` (2 s); the buffer is retained across
    /// the epoch.
    pub const VIDEO_EPOCH_RESTART_BUDGET_MS: i64 = 2000;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_are_consistent() {
        assert_eq!(units::TICKS_PER_SECOND, 10_000_000);
        assert_eq!(units::TICKS_PER_SECOND, units::TICKS_PER_MILLISECOND * 1000);
        assert_eq!(
            units::TICKS_PER_MILLISECOND,
            1_000_000 / units::TICK_NANOSECONDS
        );
        assert_eq!(units::ms_to_ticks(2), audio::GAP_JITTER_THRESHOLD_TICKS);
    }

    #[test]
    fn nominal_frame_duration_matches_spec_examples() {
        // §1.2: at 60 fps, D rounds down to 166,666 (true value 166,666.67).
        assert_eq!(video::nominal_frame_duration_ticks(60), 166_666);
        assert_eq!(video::nominal_frame_duration_ticks(30), 333_333);
    }

    #[test]
    fn slot_boundary_is_exact_and_non_accumulating() {
        // §1.2: the whole point of the integer formula is that it does not
        // accumulate a rounded D. Over one hour at 60 fps (216,000 frames) the
        // exact grid must land on exactly one hour, whereas accumulating the
        // rounded 166,666 would drift.
        let base = 0;
        let one_hour_frames = 60 * 60 * 60; // 216_000
        let exact = video::slot_boundary_ticks(base, one_hour_frames, 60);
        assert_eq!(exact, 3600 * units::TICKS_PER_SECOND);

        // The naive accumulation the spec warns against:
        let naive = one_hour_frames * video::nominal_frame_duration_ticks(60);
        let drift_ticks = exact - naive;
        // Spec: accumulation "drifts +20 ms/hour". 216_000 * 0.67 ticks ≈
        // 144_000 ticks ≈ 14.4 ms; assert the exact grid is meaningfully ahead.
        assert!(drift_ticks > 100_000, "drift was {drift_ticks} ticks");
    }

    #[test]
    fn slot_boundary_first_slot_is_base() {
        assert_eq!(video::slot_boundary_ticks(1_000, 0, 60), 1_000);
    }

    #[test]
    fn slot_boundary_no_overflow_at_large_n() {
        // A day of 120 fps frames: n is large; the i128 intermediate must not
        // overflow (n * 10_000_000 exceeds i64 for n > ~9.2e11).
        let n = 120 * 60 * 60 * 24; // 10_368_000
        let t = video::slot_boundary_ticks(0, n, 120);
        assert_eq!(t, 24 * 3600 * units::TICKS_PER_SECOND);
    }

    #[test]
    fn gop_frames_matches_spec() {
        // §3: gop_frames = 2 × fps.
        assert_eq!(ring::gop_frames(ring::IDR_INTERVAL_SECONDS, 60), 120);
        assert_eq!(ring::gop_frames(ring::IDR_INTERVAL_SECONDS, 30), 60);
        assert_eq!(
            ring::gop_frames(ring::PRECISE_MODE_IDR_INTERVAL_SECONDS, 60),
            60
        );
    }

    #[test]
    fn byte_cap_matches_spec_table() {
        // §6.2 table (Byte cap = table × 1.5). The table's row bytes are
        // `est_bitrate_mbps × seconds / 8`; our est video Mbps + 0.32 audio ≈ the
        // table's "+0.4", so the cap lands within a couple percent of "table × 1.5".
        // 1080p60 @ 120 s: table 246 MB → cap ≈ 369 MB.
        let bps_1080 = ring::est_bitrate_bps(1920, 1080, 60);
        assert_eq!(bps_1080, 16_000_000 + ring::EST_AUDIO_BPS);
        let cap_1080 = ring::byte_cap_bytes(120, bps_1080);
        assert!(
            (360_000_000..375_000_000).contains(&cap_1080),
            "1080p60/120s cap {cap_1080} not ≈ 369 MB"
        );
        // 1440p60 tier = 26 Mbps video; 4K60 tier = 50 Mbps video.
        assert_eq!(
            ring::est_bitrate_bps(2560, 1440, 60),
            26_000_000 + ring::EST_AUDIO_BPS
        );
        assert_eq!(
            ring::est_bitrate_bps(3840, 2160, 60),
            50_000_000 + ring::EST_AUDIO_BPS
        );
        // fps scales the video term: 30 fps ≈ half the video bitrate.
        assert_eq!(
            ring::est_bitrate_bps(1920, 1080, 30),
            8_000_000 + ring::EST_AUDIO_BPS
        );
    }

    #[test]
    fn mux_timescales_match_spec() {
        // §4.5: video timescale = fps × 1000 = 60,000 at 60 fps.
        assert_eq!(mux::video_timescale(60), 60_000);
        assert_eq!(mux::AUDIO_TIMESCALE, audio::SAMPLE_RATE_HZ);
    }

    #[test]
    fn device_hole_is_debounce_plus_rebuild() {
        // §7: worst-case hole = debounce (250) + rebuild (500) = 750 ms.
        assert_eq!(
            device::AUDIO_WORST_CASE_HOLE_MS,
            device::IMM_DEBOUNCE_MS + device::AUDIO_REBUILD_BUDGET_MS
        );
        assert_eq!(
            sync_budget::AV4_MAX_RECOVERY_GAP_MS,
            device::AUDIO_WORST_CASE_HOLE_MS
        );
    }

    #[test]
    fn audio_period_frames_match_rate() {
        // §2.1: 10 ms @ 48 kHz = 480 frames.
        assert_eq!(
            audio::PERIOD_FRAMES,
            audio::SAMPLE_RATE_HZ / (1000 / audio::PERIOD_MS as u32)
        );
    }
}
