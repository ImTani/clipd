//! `avrig` — the A/V sync measurement rig for `02-AV-SYNC-SPEC.md §5` (Task 8).
//!
//! Two subcommands:
//!
//! - `avrig flash [--seconds N] [--interval-ms M] [--flash-ms F]` — the
//!   generator: a full-screen white flash + a simultaneous click, repeated, that
//!   `clipd record` captures (monitor + desktop loopback) so the saved clip
//!   encodes the A/V offset.
//! - `avrig measure <clip.mp4>` — the analyzer: extracts the video-luma and
//!   desktop-audio series via ffmpeg, detects flashes and clicks, and prints the
//!   offset + drift with AV-1 / AV-2 pass/fail.
//!
//! Usage for the acceptance tests (run on the test box — 04-TEST-MACHINE.md):
//!   1. `just rig flash --seconds 35`      (in one shell)
//!   2. `just run -- record --seconds 30`  (in another, capturing that monitor)
//!   3. `just rig measure <clip>.mp4`      (after the record finalizes)
//!
//! AV-2 uses `--seconds 620` + a 10-minute record; AV-3 pauses desktop audio
//! mid-run; AV-5 runs a GPU-saturating game alongside.

mod analysis;
mod generator;
mod measure;

use std::process::ExitCode;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("flash") => cmd_flash(&args[1..]),
        Some("measure") => cmd_measure(&args[1..]),
        _ => {
            usage();
            ExitCode::from(2)
        }
    }
}

/// `flash [--seconds N] [--interval-ms M] [--flash-ms F]`.
fn cmd_flash(args: &[String]) -> ExitCode {
    let mut seconds = 35u64;
    let mut interval_ms = 2000u64;
    let mut flash_ms = 16u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let val = || it.clone().next().and_then(|s| s.parse::<u64>().ok());
        match a.as_str() {
            "--seconds" => {
                if let Some(v) = val() {
                    seconds = v;
                }
                it.next();
            }
            "--interval-ms" => {
                if let Some(v) = val() {
                    interval_ms = v;
                }
                it.next();
            }
            "--flash-ms" => {
                if let Some(v) = val() {
                    flash_ms = v;
                }
                it.next();
            }
            other => {
                eprintln!("flash: unrecognized argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    match generator::run(seconds, interval_ms, flash_ms) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flash failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// `measure <clip.mp4>`.
fn cmd_measure(args: &[String]) -> ExitCode {
    let Some(clip) = args.first() else {
        eprintln!("measure: expected a clip path");
        return ExitCode::from(2);
    };
    match measure::run_measure(clip) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("measure failed: {e}");
            ExitCode::from(1)
        }
    }
}

fn usage() {
    eprintln!(
        "avrig — A/V sync rig (02-AV-SYNC-SPEC.md §5)\n\
         \n\
         USAGE:\n\
         \x20 avrig flash [--seconds N] [--interval-ms M] [--flash-ms F]\n\
         \x20 avrig measure <clip.mp4>\n\
         \n\
         Run `flash` while `clipd record` captures the monitor + desktop\n\
         loopback, then `measure` the saved clip."
    );
}
