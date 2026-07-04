//! `verify` — the ffprobe assertion script (`02-AV-SYNC-SPEC.md §4/§5`,
//! CLAUDE.md testing rules; Milestone-3 deliverable).
//!
//! Asserts that a saved `clipd` clip is correct by construction: stream shape
//! (`§2.5`/§2.6), monotonic PTS (`§0`), strict video CFR (`§1.3`/§4.5), the `§4`
//! save-rebase origin (`§4.3`/§4.4), track end-alignment within one AAC frame
//! (`§5 AV-3`), and full-decode fragment validity (`§4.6`).
//!
//! Usage (via the justfile): `just verify <clip.mp4> [<clip2.mp4> …]`. It accepts
//! one or more clips so the `§5` "green on 50 consecutive saves" gate is a single
//! invocation: `just verify (Get-ChildItem clips\*.mp4)`. Exit 0 iff every clip
//! passes every check; exit 1 otherwise (2 on a usage error).
//!
//! The assertion logic ([`checks`]) is pure and unit-tested; this file and
//! [`probe`] are the thin `ffprobe`/`ffmpeg` shell (7.x on the test box).

mod checks;
mod probe;

use std::process::ExitCode;

use checks::{
    check_cfr, check_end_alignment, check_monotonic, check_rebase_origin, check_stream_shape,
    Check, AAC_FRAME_S,
};

fn main() -> ExitCode {
    let clips: Vec<String> = std::env::args().skip(1).collect();
    if clips.is_empty() {
        eprintln!(
            "verify — ffprobe assertion script (02-AV-SYNC-SPEC.md §4/§5)\n\n\
             USAGE:\n\
             \x20 verify <clip.mp4> [<clip2.mp4> …]\n\n\
             Asserts stream shape, monotonic PTS, video CFR, the §4 save-rebase\n\
             origin, track end-alignment (≤ 1 AAC frame), and full-decode validity.\n\
             Exit 0 iff every clip passes every check."
        );
        return ExitCode::from(2);
    }

    let total = clips.len();
    let mut failed = 0usize;
    for clip in &clips {
        let checks = verify_clip(clip);
        let clip_ok = checks.iter().all(|c| c.pass);
        if !clip_ok {
            failed += 1;
        }
        print_report(clip, &checks, clip_ok);
    }

    println!("\n═══ {}/{} clip(s) passed ═══", total - failed, total);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Run every check against one clip, returning the ordered results. A probe
/// failure (missing/invalid file, no ffprobe) becomes a single failing check so
/// the report is uniform.
fn verify_clip(clip: &str) -> Vec<Check> {
    let streams = match probe::probe_streams(clip) {
        Ok(s) => s,
        Err(e) => return vec![probe_error(e)],
    };

    let mut results = Vec::new();

    // Stream shape (§2.5/§2.6).
    let infos: Vec<checks::StreamInfo> = streams.iter().map(|s| s.info.clone()).collect();
    results.push(check_stream_shape(&infos));

    // Video track: PTS series, CFR, and the rebase origin end/first.
    let video = streams.iter().find(|s| s.info.codec_type == "video");
    let (mut video_end, mut video_first) = (None, None);
    if video.is_some() {
        match probe::probe_pts(clip, "v:0") {
            Ok(vpts) => {
                results.push(check_monotonic("v:0", &vpts));
                let fps = video.and_then(|v| v.avg_frame_rate);
                let expected_delta = fps.map(|f| 1.0 / f).unwrap_or_else(|| median_delta(&vpts));
                results.push(check_cfr(&vpts, expected_delta));
                if let (Some(&first), Some(&last)) = (vpts.first(), vpts.last()) {
                    video_first = Some(first);
                    video_end = Some(last + expected_delta);
                }
            }
            Err(e) => results.push(probe_error(e)),
        }
    } else {
        results.push(probe_error("no video stream to probe PTS from".into()));
    }

    // Audio tracks: one selector per audio stream, in file order (a:0 = desktop,
    // a:1 = mic — §2.5).
    let n_audio = streams
        .iter()
        .filter(|s| s.info.codec_type == "audio")
        .count();
    let mut audio_ends: Vec<(String, f64)> = Vec::new();
    let mut audio_firsts: Vec<(String, f64)> = Vec::new();
    for i in 0..n_audio {
        let sel = format!("a:{i}");
        match probe::probe_pts(clip, &sel) {
            Ok(apts) => {
                results.push(check_monotonic(&sel, &apts));
                if let (Some(&first), Some(&last)) = (apts.first(), apts.last()) {
                    audio_firsts.push((sel.clone(), first));
                    audio_ends.push((sel.clone(), last + AAC_FRAME_S));
                }
            }
            Err(e) => results.push(probe_error(e)),
        }
    }

    // §4 rebase origin + track end-alignment (need the video timeline).
    if let Some(vend) = video_end {
        results.push(check_end_alignment(vend, &audio_ends));
    }
    if let Some(vfirst) = video_first {
        results.push(check_rebase_origin(vfirst, &audio_firsts));
    }

    // Fragment validity (§4.6): full decode to null.
    results.push(match probe::full_decode_ok(clip) {
        Ok(()) => Check {
            name: "fragment validity (full decode, §4.6)".into(),
            pass: true,
            detail: "decoded start-to-finish, no errors".into(),
        },
        Err(e) => Check {
            name: "fragment validity (full decode, §4.6)".into(),
            pass: false,
            detail: e,
        },
    });

    results
}

/// The median of consecutive deltas — the CFR fallback when `avg_frame_rate` is
/// N/A. (Median resists the odd dropped/duplicated frame better than the mean.)
fn median_delta(pts: &[f64]) -> f64 {
    if pts.len() < 2 {
        return 0.0;
    }
    let mut deltas: Vec<f64> = pts.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    deltas[deltas.len() / 2]
}

/// A probe/subprocess failure rendered as a failing check (uniform report shape).
fn probe_error(detail: String) -> Check {
    Check {
        name: "probe".into(),
        pass: false,
        detail,
    }
}

/// Print one clip's results with a PASS/FAIL line per check.
fn print_report(clip: &str, checks: &[Check], clip_ok: bool) {
    println!("\n── {clip} — {} ──", if clip_ok { "PASS" } else { "FAIL" });
    for c in checks {
        let mark = if c.pass { "PASS" } else { "FAIL" };
        println!("  [{mark}] {}", c.name);
        println!("         {}", c.detail);
    }
}
