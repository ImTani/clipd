//! Extract the video-luma and audio-energy series from a recorded clip via
//! `ffmpeg`/`ffprobe` (7.x on the test box), then run them through the tested
//! [`crate::analysis`] detectors and print the `§5` offset report.
//!
//! The click is emitted through the default **render** endpoint, so `clipd`
//! records it on the **desktop-loopback** track (audio track 0, `§2.5`) — that is
//! the track measured here. Requires `[audio].desktop = true` in the clip.

use std::process::Command;

use crate::analysis::{pair_events, rising_edges, summarize};

/// Luma threshold (0–255): a flash is full white (~255) over black (~0).
const LUMA_THRESHOLD: f64 = 128.0;
/// Audio peak threshold (0–1 full scale): the click is a loud burst.
const CLICK_THRESHOLD: f64 = 0.30;
/// Minimum spacing between two distinct events (both signals). Comfortably below
/// the rig's default 2 s interval, above any single-event ringing.
const REFRACTORY_S: f64 = 0.30;
/// Max flash↔click skew to accept as the same event (≈ a few buffer periods).
const MAX_SKEW_S: f64 = 0.10;
/// Audio energy window (seconds) — 5 ms ≈ the click width.
const AUDIO_WINDOW_S: f64 = 0.005;

/// Run the full measurement on `clip` and print the report. Returns `Err` with a
/// human message if ffmpeg is missing or produced nothing usable.
pub fn run_measure(clip: &str) -> Result<(), String> {
    let luma = luma_series(clip)?;
    let audio = click_series(clip)?;
    if luma.is_empty() {
        return Err("no video luma samples — is ffprobe on PATH and the clip valid?".into());
    }
    if audio.is_empty() {
        return Err(
            "no audio samples on track 0 — is desktop loopback enabled in the clip?".into(),
        );
    }

    let flashes = rising_edges(&luma, LUMA_THRESHOLD, REFRACTORY_S);
    let clicks = rising_edges(&audio, CLICK_THRESHOLD, REFRACTORY_S);
    println!(
        "detected {} flashes (video) and {} clicks (audio track 0)",
        flashes.len(),
        clicks.len()
    );
    let pairs = pair_events(&flashes, &clicks, MAX_SKEW_S);
    let Some(r) = summarize(&pairs) else {
        return Err(format!(
            "no flash↔click pairs within {:.0} ms — flashes={}, clicks={} (check the recording captured both)",
            MAX_SKEW_S * 1000.0,
            flashes.len(),
            clicks.len()
        ));
    };

    println!("\n── A/V sync report ({} paired events) ──", r.n);
    println!(
        "  offset  mean {:+.2} ms   min {:+.2}   max {:+.2}   sd {:.2}",
        r.mean_ms, r.min_ms, r.max_ms, r.std_ms
    );
    match r.drift_endpoint_ms {
        Some(ep) => println!(
            "  drift   {:+.2} ms (minute-1 vs minute-10, §5 AV-2)   |   {:+.2} ms least-squares",
            ep, r.drift_lsq_ms
        ),
        None => println!(
            "  drift   {:+.2} ms least-squares (clip too short for the §5 minute-1/10 metric)",
            r.drift_lsq_ms
        ),
    }
    println!(
        "  AV-1 (|offset| ≤ 16.7 ms):  {}",
        if r.av1_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "  AV-2 (|drift|  ≤ 5.0 ms):   {}",
        if r.av2_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "\ninterpretation (§5): a constant mean offset → AAC-delay constant; a\n\
         linear drift → drift-controller bug; large jitter/sd → grid or queueing."
    );
    Ok(())
}

/// Per-frame average luma `(pts_time, YAVG)` via `ffprobe` + the `signalstats`
/// filter. Field order follows the `-show_entries` spec (`pts_time` then
/// `YAVG`); lines that do not parse as two floats are skipped.
fn luma_series(clip: &str) -> Result<Vec<(f64, f64)>, String> {
    let out = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("movie={},signalstats", ff_escape(clip)),
            "-show_entries",
            // `pts_time`, NOT the old `pkt_pts_time` — ffmpeg 7.x dropped the
            // latter (it emits an empty field, collapsing the CSV to just YAVG
            // and yielding zero usable samples: "no video luma samples").
            "frame=pts_time:frame_tags=lavfi.signalstats.YAVG",
            "-of",
            "csv=p=0",
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut series = Vec::new();
    for line in text.lines() {
        let mut it = line.split(',').filter_map(|f| f.trim().parse::<f64>().ok());
        if let (Some(t), Some(y)) = (it.next(), it.next()) {
            series.push((t, y));
        }
    }
    Ok(series)
}

/// Per-window peak amplitude `(time, 0..1)` of audio track 0, by decoding it to
/// mono s16 @ 48 kHz through `ffmpeg` and reducing each [`AUDIO_WINDOW_S`] window
/// to its peak |sample|.
fn click_series(clip: &str) -> Result<Vec<(f64, f64)>, String> {
    let rate = 48_000usize;
    let raw = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            clip,
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            &rate.to_string(),
            "-f",
            "s16le",
            "-",
        ],
    )?;
    let win = (AUDIO_WINDOW_S * rate as f64) as usize;
    let win = win.max(1);
    let mut series = Vec::with_capacity(raw.len() / (2 * win) + 1);
    let mut i = 0usize;
    let mut sample_idx = 0usize;
    while i + 1 < raw.len() {
        let mut peak = 0.0f64;
        let mut n = 0usize;
        while n < win && i + 1 < raw.len() {
            let s = i16::from_le_bytes([raw[i], raw[i + 1]]);
            let a = (s as f64).abs() / 32768.0;
            if a > peak {
                peak = a;
            }
            i += 2;
            n += 1;
        }
        let t = sample_idx as f64 / rate as f64;
        series.push((t, peak));
        sample_idx += n;
    }
    Ok(series)
}

/// `movie=` needs `\`, `:`, and `'` escaped inside the lavfi graph. Backslash the
/// Windows path separators and colons so a `C:\...` path survives.
fn ff_escape(path: &str) -> String {
    path.replace('\\', "/").replace(':', "\\:")
}

/// Run a command, returning stdout bytes or a message (including stderr) on
/// failure or a missing binary.
fn run(bin: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("could not run `{bin}` (is it on PATH?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{bin}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}
