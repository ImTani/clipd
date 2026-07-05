//! Pure assertion logic for a saved `clipd` clip — the testable brain of the
//! verifier (the ffprobe/ffmpeg shell lives in [`crate::probe`]). Every check is a
//! pure function over already-extracted numbers, so the whole module is unit-tested
//! without a clip or a subprocess (the split mirrors `avrig`'s `analysis.rs`).
//!
//! What "correct" means is fixed by the frozen `02-AV-SYNC-SPEC.md`:
//! - stream shape: 1 H.264 video + N AAC-LC 48 kHz stereo tracks (`§2.5`/§2.6);
//! - monotonic PTS on every track (`§0` monotonicity guard — steady state is 0
//!   violations);
//! - video is strictly CFR: every PTS delta equals `1/fps` (`§1.3`/`§4.5`);
//! - the `§4` save contract: video rebased to origin 0 (`§4.3`), audio head-silence
//!   ≤ one AAC frame (`§4.4`), and track end-times aligned within one AAC frame
//!   (`§4.4` trailing rule / `§5 AV-3`).

/// One AAC frame in seconds: 1024 samples @ 48 kHz = 21.333 ms.
/// `02-AV-SYNC-SPEC.md §2.6` (FRAME_SAMPLES = 1024) / `§4.5` (audio timescale 48000).
pub const AAC_FRAME_S: f64 = 1024.0 / 48_000.0;

/// Track end-times must align within one AAC frame. `§5 AV-3` ("audio track
/// duration within 1 AAC frame of video duration") / `§4.4` (trailing audio
/// included until `pts >= last_video_pts + D`).
pub const DURATION_TOL_S: f64 = AAC_FRAME_S;

/// Max audio head-silence: one AAC frame. `§4.4` ("max 21.33 ms of head silence …
/// accepted by design instead of partial-AAC-frame surgery").
pub const HEAD_SILENCE_MAX_S: f64 = AAC_FRAME_S;

/// CFR delta tolerance. Video PTS are exact grid multiples (`§1.3`/`§4.5`), so
/// consecutive `pts_time` deltas differ only by microsecond print-rounding; a real
/// VFR glitch is a whole missing/duplicated frame (≥ a full delta off), far outside
/// 1 ms.
pub const CFR_TOL_S: f64 = 0.001;

/// Video origin tolerance: after `§4.3` rebasing the first video PTS is exactly 0.
pub const ORIGIN_TOL_S: f64 = 0.001;

/// A single named assertion outcome.
#[derive(Debug, Clone)]
pub struct Check {
    /// Short check name (shown in the report).
    pub name: String,
    /// Whether the assertion held.
    pub pass: bool,
    /// Human-readable evidence (numbers that support pass or explain the failure).
    pub detail: String,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            pass: true,
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            pass: false,
            detail: detail.into(),
        }
    }
}

/// One stream's identifying fields, as read from `ffprobe -show_entries stream=…`.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// `codec_type`: "video" / "audio" / …
    pub codec_type: String,
    /// `codec_name`: "h264" / "aac" / …
    pub codec_name: String,
    /// `sample_rate` (audio only; `None` for video / N/A).
    pub sample_rate: Option<u32>,
    /// `channels` (audio only).
    pub channels: Option<u32>,
}

/// Stream shape: exactly one H.264 video track, and one or more AAC-LC 48 kHz
/// stereo audio tracks (`§2.5` desktop first, mic second; `§2.6` AAC-LC/48k).
pub fn check_stream_shape(streams: &[StreamInfo]) -> Check {
    const NAME: &str = "stream shape (1 h264 + N aac 48k/2ch, §2.5/§2.6)";
    let video: Vec<&StreamInfo> = streams.iter().filter(|s| s.codec_type == "video").collect();
    let audio: Vec<&StreamInfo> = streams.iter().filter(|s| s.codec_type == "audio").collect();

    if video.len() != 1 {
        return Check::fail(
            NAME,
            format!("expected exactly 1 video stream, found {}", video.len()),
        );
    }
    if video[0].codec_name != "h264" {
        return Check::fail(
            NAME,
            format!("video codec is '{}', expected 'h264'", video[0].codec_name),
        );
    }
    if audio.is_empty() {
        return Check::fail(NAME, "no audio streams (expected ≥ 1 AAC track)");
    }
    for (i, a) in audio.iter().enumerate() {
        if a.codec_name != "aac" {
            return Check::fail(
                NAME,
                format!(
                    "audio track {i} codec is '{}', expected 'aac'",
                    a.codec_name
                ),
            );
        }
        if a.sample_rate != Some(48_000) {
            return Check::fail(
                NAME,
                format!(
                    "audio track {i} sample rate is {:?}, expected 48000",
                    a.sample_rate
                ),
            );
        }
        if a.channels != Some(2) {
            return Check::fail(
                NAME,
                format!("audio track {i} channels is {:?}, expected 2", a.channels),
            );
        }
    }
    Check::pass(
        NAME,
        format!(
            "1 h264 video + {} aac-LC 48 kHz stereo track(s)",
            audio.len()
        ),
    )
}

/// Strictly increasing PTS across a track (`§0`: any `pts <= prev` is a violation;
/// steady state is zero). Empty is a failure — a valid track has frames.
pub fn check_monotonic(label: &str, pts: &[f64]) -> Check {
    let name = format!("monotonic PTS [{label}] (§0)");
    if pts.is_empty() {
        return Check::fail(name, "no frames extracted");
    }
    for i in 1..pts.len() {
        if pts[i] <= pts[i - 1] {
            return Check::fail(
                name,
                format!(
                    "non-monotonic at frame {i}: {:.6}s follows {:.6}s",
                    pts[i],
                    pts[i - 1]
                ),
            );
        }
    }
    Check::pass(name, format!("{} frames strictly increasing", pts.len()))
}

/// Video CFR: every consecutive PTS delta equals `expected_delta` (= `1/fps`)
/// within [`CFR_TOL_S`] (`§1.3`/`§4.5`).
pub fn check_cfr(pts: &[f64], expected_delta: f64) -> Check {
    const NAME: &str = "video CFR (constant 1/fps deltas, §1.3/§4.5)";
    if pts.len() < 2 {
        return Check::fail(
            NAME,
            format!("too few video frames ({}) to check CFR", pts.len()),
        );
    }
    let (mut min_d, mut max_d) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut worst = 0.0f64;
    for i in 1..pts.len() {
        let d = pts[i] - pts[i - 1];
        min_d = min_d.min(d);
        max_d = max_d.max(d);
        worst = worst.max((d - expected_delta).abs());
    }
    if worst > CFR_TOL_S {
        return Check::fail(
            NAME,
            format!(
                "delta spread [{:.6}, {:.6}]s vs expected {:.6}s — worst deviation {:.3} ms > {:.0} ms tol",
                min_d, max_d, expected_delta, worst * 1000.0, CFR_TOL_S * 1000.0
            ),
        );
    }
    Check::pass(
        NAME,
        format!(
            "{} deltas, all within {:.3} ms of {:.6}s (1/fps)",
            pts.len() - 1,
            worst * 1000.0,
            expected_delta
        ),
    )
}

/// Track end-time alignment: each audio track ends within one AAC frame of the
/// video track (`§4.4` trailing-audio rule / `§5 AV-3`). Ends are `last_pts +
/// nominal_frame_duration`, computed by the caller.
pub fn check_end_alignment(video_end_s: f64, audio_ends: &[(String, f64)]) -> Check {
    const NAME: &str = "track end alignment (within 1 AAC frame, §4.4/§5 AV-3)";
    for (label, end) in audio_ends {
        let skew = (end - video_end_s).abs();
        if skew > DURATION_TOL_S {
            return Check::fail(
                NAME,
                format!(
                    "audio [{label}] ends {:.3} ms from video (> {:.2} ms = 1 AAC frame)",
                    (end - video_end_s) * 1000.0,
                    DURATION_TOL_S * 1000.0
                ),
            );
        }
    }
    Check::pass(
        NAME,
        format!(
            "video end {:.3}s; {} audio track(s) within {:.2} ms",
            video_end_s,
            audio_ends.len(),
            DURATION_TOL_S * 1000.0
        ),
    )
}

/// The `§4` rebase origin: first video PTS is 0 (`§4.3`) and each audio track's
/// first PTS is within `[0, one AAC frame]` head-silence (`§4.4`).
pub fn check_rebase_origin(first_video_s: f64, first_audio: &[(String, f64)]) -> Check {
    const NAME: &str = "save rebase origin (video@0, audio head ≤ 1 AAC frame, §4.3/§4.4)";
    if first_video_s.abs() > ORIGIN_TOL_S {
        return Check::fail(
            NAME,
            format!(
                "first video PTS is {:.3} ms, expected 0 (§4.3)",
                first_video_s * 1000.0
            ),
        );
    }
    for (label, head) in first_audio {
        // A tiny negative is rounding noise; a real problem is a large head or a
        // negative beyond tolerance.
        if *head < -ORIGIN_TOL_S || *head > HEAD_SILENCE_MAX_S {
            return Check::fail(
                NAME,
                format!(
                    "audio [{label}] head-silence {:.3} ms outside [0, {:.2} ms] (§4.4)",
                    head * 1000.0,
                    HEAD_SILENCE_MAX_S * 1000.0
                ),
            );
        }
    }
    Check::pass(
        NAME,
        format!(
            "video@{:.3} ms; audio head(s) ≤ {:.2} ms",
            first_video_s * 1000.0,
            HEAD_SILENCE_MAX_S * 1000.0
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vid(codec: &str) -> StreamInfo {
        StreamInfo {
            codec_type: "video".into(),
            codec_name: codec.into(),
            sample_rate: None,
            channels: None,
        }
    }
    fn aud(codec: &str, rate: Option<u32>, ch: Option<u32>) -> StreamInfo {
        StreamInfo {
            codec_type: "audio".into(),
            codec_name: codec.into(),
            sample_rate: rate,
            channels: ch,
        }
    }

    #[test]
    fn stream_shape_accepts_one_video_two_aac() {
        let s = [
            vid("h264"),
            aud("aac", Some(48_000), Some(2)),
            aud("aac", Some(48_000), Some(2)),
        ];
        assert!(check_stream_shape(&s).pass);
    }

    #[test]
    fn stream_shape_rejects_missing_video() {
        let s = [aud("aac", Some(48_000), Some(2))];
        assert!(!check_stream_shape(&s).pass);
    }

    #[test]
    fn stream_shape_rejects_two_video() {
        let s = [vid("h264"), vid("h264"), aud("aac", Some(48_000), Some(2))];
        assert!(!check_stream_shape(&s).pass);
    }

    #[test]
    fn stream_shape_rejects_wrong_sample_rate() {
        let s = [vid("h264"), aud("aac", Some(44_100), Some(2))];
        assert!(!check_stream_shape(&s).pass);
    }

    #[test]
    fn stream_shape_rejects_non_aac_audio() {
        let s = [vid("h264"), aud("mp3", Some(48_000), Some(2))];
        assert!(!check_stream_shape(&s).pass);
    }

    #[test]
    fn stream_shape_rejects_no_audio() {
        let s = [vid("h264")];
        assert!(!check_stream_shape(&s).pass);
    }

    #[test]
    fn monotonic_accepts_increasing() {
        let pts = [0.0, 0.016667, 0.033333, 0.05];
        assert!(check_monotonic("v", &pts).pass);
    }

    #[test]
    fn monotonic_rejects_backward_step() {
        let pts = [0.0, 0.016667, 0.016667, 0.05]; // equal => violation (pts <= prev)
        assert!(!check_monotonic("v", &pts).pass);
        let pts2 = [0.0, 0.02, 0.01]; // decreasing
        assert!(!check_monotonic("v", &pts2).pass);
    }

    #[test]
    fn monotonic_rejects_empty() {
        assert!(!check_monotonic("v", &[]).pass);
    }

    #[test]
    fn cfr_accepts_constant_60fps() {
        let d = 1.0 / 60.0;
        let pts: Vec<f64> = (0..120).map(|k| k as f64 * d).collect();
        assert!(check_cfr(&pts, d).pass);
    }

    #[test]
    fn cfr_accepts_microsecond_rounding() {
        // pts_time printed to 6 decimals: deltas wobble by < 2 µs, well under tol.
        let d = 1.0 / 60.0;
        let pts: Vec<f64> = (0..120)
            .map(|k| ((k as f64 * d) * 1e6).round() / 1e6)
            .collect();
        assert!(check_cfr(&pts, d).pass);
    }

    #[test]
    fn cfr_rejects_dropped_frame() {
        let d = 1.0 / 60.0;
        let mut pts: Vec<f64> = (0..60).map(|k| k as f64 * d).collect();
        // Skip a slot: a 2×-delta gap.
        for p in pts.iter_mut().skip(30) {
            *p += d;
        }
        assert!(!check_cfr(&pts, d).pass);
    }

    #[test]
    fn cfr_rejects_duplicate_pts() {
        let d = 1.0 / 60.0;
        let pts = [0.0, d, d, 2.0 * d]; // a zero delta
        assert!(!check_cfr(&pts, d).pass);
    }

    #[test]
    fn end_alignment_accepts_within_one_frame() {
        // Audio ends 10 ms after video — inside the 21.33 ms tolerance.
        let a = vec![("desktop".to_string(), 30.010), ("mic".to_string(), 29.995)];
        assert!(check_end_alignment(30.0, &a).pass);
    }

    #[test]
    fn end_alignment_rejects_two_frames_off() {
        // 45 ms > one AAC frame.
        let a = vec![("desktop".to_string(), 30.045)];
        assert!(!check_end_alignment(30.0, &a).pass);
    }

    #[test]
    fn end_alignment_boundary_just_inside() {
        let a = vec![("desktop".to_string(), 30.0 + AAC_FRAME_S - 0.0005)];
        assert!(check_end_alignment(30.0, &a).pass);
    }

    #[test]
    fn rebase_origin_accepts_zero_video_and_small_head() {
        let a = vec![("desktop".to_string(), 0.005), ("mic".to_string(), 0.0)];
        assert!(check_rebase_origin(0.0, &a).pass);
    }

    #[test]
    fn rebase_origin_accepts_head_at_frame_boundary() {
        let a = vec![("desktop".to_string(), AAC_FRAME_S - 0.0005)];
        assert!(check_rebase_origin(0.0, &a).pass);
    }

    #[test]
    fn rebase_origin_rejects_nonzero_video() {
        let a = vec![("desktop".to_string(), 0.0)];
        assert!(!check_rebase_origin(0.5, &a).pass); // video should start at 0
    }

    #[test]
    fn rebase_origin_rejects_excessive_head_silence() {
        let a = vec![("desktop".to_string(), 0.050)]; // 50 ms > 1 AAC frame
        assert!(!check_rebase_origin(0.0, &a).pass);
    }
}
