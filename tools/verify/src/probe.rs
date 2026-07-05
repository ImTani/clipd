//! The thin `ffprobe`/`ffmpeg` shell — the only part that needs the tools on the
//! test box (7.x). It extracts the numbers [`crate::checks`] asserts on: stream
//! shape, per-track PTS series, and a full-decode pass for fragment validity.
//!
//! Output is parsed as `ffprobe -of default` key=value blocks (robust to fields
//! that don't exist for a given stream — unlike positional CSV, which misaligns
//! when e.g. a video stream has no `sample_rate`). Per-frame PTS come back as CSV
//! (one `pts_time` per line), the same lightweight path `avrig`'s `measure.rs` uses.

use std::process::Command;

use crate::checks::StreamInfo;

/// One probed stream: its identifying [`StreamInfo`] plus the video frame rate
/// (needed for the CFR check; `None` for audio / unparseable). Streams are kept in
/// file order, so the relative selectors `v:0`/`a:0`/`a:1` address them — the
/// absolute `index` is not retained.
#[derive(Debug, Clone)]
pub struct RawStream {
    /// Fields the shape check reads.
    pub info: StreamInfo,
    /// `avg_frame_rate` as a float (video); `None` when N/A ("0/0") or absent.
    pub avg_frame_rate: Option<f64>,
}

/// Probe every stream's shape fields via one `ffprobe` call.
pub fn probe_streams(clip: &str) -> Result<Vec<RawStream>, String> {
    let out = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "stream=index,codec_type,codec_name,sample_rate,channels,avg_frame_rate",
            // `default` prints `[STREAM] key=value … [/STREAM]` — keys absent for a
            // stream (e.g. sample_rate on video) are simply omitted, so parsing by
            // key never misaligns.
            "-of",
            "default",
            clip,
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut streams = Vec::new();
    for block in text.split("[STREAM]").skip(1) {
        let block = block.split("[/STREAM]").next().unwrap_or("");
        let mut codec_type = String::new();
        let mut codec_name = String::new();
        let mut sample_rate = None;
        let mut channels = None;
        let mut avg_frame_rate = None;
        for line in block.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "codec_type" => codec_type = v.to_string(),
                "codec_name" => codec_name = v.to_string(),
                "sample_rate" => sample_rate = v.parse::<u32>().ok(),
                "channels" => channels = v.parse::<u32>().ok(),
                "avg_frame_rate" => avg_frame_rate = parse_rational(v),
                _ => {}
            }
        }
        if codec_type.is_empty() {
            continue;
        }
        streams.push(RawStream {
            info: StreamInfo {
                codec_type,
                codec_name,
                sample_rate,
                channels,
            },
            avg_frame_rate,
        });
    }
    if streams.is_empty() {
        return Err(format!(
            "ffprobe found no streams in '{clip}' (is it a valid clip and ffprobe on PATH?)"
        ));
    }
    Ok(streams)
}

/// The `pts_time` series for one stream selector (`"v:0"`, `"a:0"`, `"a:1"`, …),
/// in file order. Lines that do not parse as a float are skipped.
pub fn probe_pts(clip: &str, selector: &str) -> Result<Vec<f64>, String> {
    let out = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            selector,
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
            clip,
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    // Take the FIRST comma-separated field per line, not the whole line: ffprobe
    // emits some frames (e.g. the leading keyframe) with a trailing empty field —
    // `0.000000,` — so parsing the raw line would silently drop that frame and
    // shift `first()` onto a later one. (Same defence as avrig's measure.rs.)
    let series: Vec<f64> = text
        .lines()
        .filter_map(|l| l.split(',').next()?.trim().parse::<f64>().ok())
        .collect();
    Ok(series)
}

/// Decode the whole file to null: proves every fragment parses and the clip plays
/// start-to-finish (`§4.6` crash-safe fragments). Any `-v error` output means a
/// decode problem.
pub fn full_decode_ok(clip: &str) -> Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", clip, "-f", "null", "-"])
        .output()
        .map_err(|e| format!("could not run `ffmpeg` (is it on PATH?): {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if !out.status.success() {
        return Err(format!("ffmpeg exited {}: {stderr}", out.status));
    }
    if !stderr.is_empty() {
        return Err(format!("decode errors: {stderr}"));
    }
    Ok(())
}

/// Parse an `ffprobe` rational like `"60/1"` into a float; `"0/0"` (N/A) → `None`.
fn parse_rational(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.trim().parse().ok()?;
    let d: f64 = d.trim().parse().ok()?;
    if d == 0.0 {
        return None;
    }
    Some(n / d)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_parses_and_handles_na() {
        assert_eq!(parse_rational("60/1"), Some(60.0));
        assert_eq!(parse_rational("60000/1001"), Some(60000.0 / 1001.0));
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("garbage"), None);
    }
}
