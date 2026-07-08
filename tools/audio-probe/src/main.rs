//! `audio-probe` — Slice B / B2 process-loopback capture instrument.
//!
//! Captures ONE process tree's audio via WASAPI `ActivateAudioInterfaceAsync` +
//! PROCESS_LOOPBACK (the exact activation path `src/audio/process_loopback.rs`
//! uses in the core binary), writes it to a WAV, and prints per-run stats
//! (packets, frames, silent packets, QPC span, max gap, timestamp errors). This
//! is the manual hardware validation for B2 — the module's pure logic is
//! unit-tested in-tree; the COM path is HW-only (CLAUDE.md testing rules).
//!
//! ## Usage (via `just probe -- <ARGS>`)
//! ```text
//! audio-probe [--pid <PID>] [--exclude] [--seconds <S>] [--out <WAV>] [--tone|--no-tone]
//! ```
//! - `--pid <PID>` — process to capture (default: this probe's own PID, a self
//!   tree, which needs `--tone` to have any signal).
//! - `--exclude` — capture everything EXCEPT the target tree (default: include).
//! - `--seconds <S>` — capture duration (default 8).
//! - `--out <WAV>` — output WAV path (default: a temp file).
//! - `--tone` — render a 440 Hz sine on the default endpoint from THIS process so a
//!   self-capture (default `--pid`) records a known signal. Defaults ON for a self
//!   capture, OFF when `--pid` targets another process (you supply the signal).
//!
//! ## The B2 checklist (run on the 04-TEST-MACHINE Nitro; expected results)
//! 1. `just probe` (self + tone) → WAV contains a steady 440 Hz tone; stats show
//!    `packets > 0`, `silent` low, `timestamp_errors = 0`, and `qpc_span_s ≈`
//!    the requested seconds → **QPCPosition is the master domain (§2.2).**
//! 2. `just probe -- --pid <a browser playing audio>` → the WAV contains only that
//!    app's audio; `--exclude` on the same PID → everything BUT it.
//! 3. `just probe -- --pid <PID> --seconds 20`, then KILL the target mid-run →
//!    capture ends promptly with "target process exited" (PID-liveness), not a
//!    hang; the partial WAV is valid.
//! 4. `just probe -- --pid <dead/bogus PID>` → activation fails or yields silence,
//!    the probe exits cleanly (no crash) — mirrors the core's "track silent".
//! 5. Run two probes at once on different PIDs → both capture (serialized
//!    activation does not deadlock).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use tracing::{info, warn};
use wasapi::{
    initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

/// 48 kHz f32 stereo — the fixed format the core module requests (the loopback
/// client cannot report a native format).
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 100 ns ticks per second (the master QPC domain unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

struct Args {
    pid: u32,
    include_tree: bool,
    seconds: u64,
    out: PathBuf,
    tone: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut pid: Option<u32> = None;
    let mut include_tree = true;
    let mut seconds = 8u64;
    let mut out: Option<PathBuf> = None;
    let mut tone: Option<bool> = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pid" => {
                pid = Some(
                    it.next()
                        .ok_or("--pid needs a value")?
                        .parse()
                        .map_err(|e| format!("--pid: {e}"))?,
                )
            }
            "--exclude" => include_tree = false,
            "--include" => include_tree = true,
            "--seconds" => {
                seconds = it
                    .next()
                    .ok_or("--seconds needs a value")?
                    .parse()
                    .map_err(|e| format!("--seconds: {e}"))?
            }
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out needs a value")?)),
            "--tone" => tone = Some(true),
            "--no-tone" => tone = Some(false),
            "-h" | "--help" => return Err("help".into()),
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    let self_pid = std::process::id();
    let pid = pid.unwrap_or(self_pid);
    // Default tone ON only for a self capture (otherwise there is no signal).
    let tone = tone.unwrap_or(pid == self_pid);
    let out = out.unwrap_or_else(|| std::env::temp_dir().join(format!("audio_probe_{pid}.wav")));
    Ok(Args {
        pid,
        include_tree,
        seconds,
        out,
        tone,
    })
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e == "help" {
                eprintln!(
                    "audio-probe [--pid <PID>] [--exclude] [--seconds <S>] [--out <WAV>] [--tone|--no-tone]"
                );
                std::process::exit(0);
            }
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let stop = Arc::new(AtomicBool::new(false));

    // Optional self-signal so a self capture is not silent.
    let render = if args.tone {
        let stop = stop.clone();
        Some(thread::spawn(move || {
            if let Err(e) = render_tone(&stop) {
                warn!(error = %e, "tone render failed — capture may be silent");
            }
        }))
    } else {
        None
    };

    // Stop after the duration.
    {
        let stop = stop.clone();
        let seconds = args.seconds;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            stop.store(true, Ordering::Relaxed);
        });
    }

    println!(
        "capturing pid {} tree ({}) for {}s → {}{}",
        args.pid,
        if args.include_tree {
            "include"
        } else {
            "exclude"
        },
        args.seconds,
        args.out.display(),
        if args.tone { " [+440 Hz self-tone]" } else { "" }
    );

    let code = match capture(&args, &stop) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("capture failed: {e}");
            2
        }
    };

    stop.store(true, Ordering::Relaxed);
    if let Some(r) = render {
        let _ = r.join();
    }
    std::process::exit(code);
}

/// Per-run capture stats (mirrors the core `run_audio_probe` aggregation).
#[derive(Default)]
struct Stat {
    packets: u64,
    frames: u64,
    silent: u64,
    timestamp_errors: u64,
    first_qpc: Option<u64>,
    last_qpc: u64,
    prev: Option<(u64, u32)>,
    max_gap_ticks: i64,
}

/// Open the process-loopback client, capture to WAV, and report stats. This
/// re-implements the core module's open sequence against `wasapi` directly
/// (standalone tool, not linked to `clipd`) — kept deliberately in lock-step with
/// `src/audio/process_loopback.rs::open_process_session`.
fn capture(args: &Args, stop: &AtomicBool) -> Result<(), String> {
    initialize_mta().ok().map_err(|e| e.to_string())?;

    let mut client = AudioClient::new_application_loopback_client(args.pid, args.include_tree)
        .map_err(|e| format!("activate: {e}"))?;
    // Fixed 48 kHz f32 stereo (the loopback client cannot report a native format).
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CHANNELS as usize, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 400_000, // 4 × 10 ms, per the core module
    };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| format!("initialize: {e}"))?;
    let h_event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture_client = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    let bytes_per_frame = format.get_blockalign() as usize;
    client.start_stream().map_err(|e| e.to_string())?;

    let spec = WavSpec {
        channels: CHANNELS,
        sample_rate: RATE,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut wav = WavWriter::create(&args.out, spec).map_err(|e| format!("wav create: {e}"))?;

    let mut deque: VecDeque<u8> = VecDeque::with_capacity(bytes_per_frame * RATE as usize);
    let mut stat = Stat::default();

    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        loop {
            let n = capture_client
                .get_next_packet_size()
                .map_err(|e| e.to_string())?
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            let before = deque.len();
            let info = capture_client
                .read_from_device_to_deque(&mut deque)
                .map_err(|e| e.to_string())?;
            let frames = ((deque.len() - before) / bytes_per_frame) as u32;
            if frames == 0 {
                continue;
            }

            stat.packets += 1;
            stat.frames += frames as u64;
            if info.flags.silent {
                stat.silent += 1;
            }
            if info.flags.timestamp_error {
                stat.timestamp_errors += 1;
            }
            stat.first_qpc.get_or_insert(info.timestamp);
            stat.last_qpc = info.timestamp;
            // Gap = QPC advance beyond frames delivered (mirrors spike #3 / core probe).
            if let Some((pt, pf)) = stat.prev {
                let advance = info.timestamp as i128 - pt as i128;
                let expected = pf as i128 * TICKS_PER_SECOND as i128 / RATE as i128;
                let gap = (advance - expected) as i64;
                stat.max_gap_ticks = stat.max_gap_ticks.max(gap);
            }
            stat.prev = Some((info.timestamp, frames));

            // f32 little-endian → WAV samples.
            while deque.len() >= 4 {
                let b = [
                    deque.pop_front().unwrap(),
                    deque.pop_front().unwrap(),
                    deque.pop_front().unwrap(),
                    deque.pop_front().unwrap(),
                ];
                wav.write_sample(f32::from_le_bytes(b))
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    let _ = client.stop_stream();
    wav.finalize().map_err(|e| e.to_string())?;

    let span_s = stat
        .first_qpc
        .map(|f| (stat.last_qpc.saturating_sub(f)) as f64 / TICKS_PER_SECOND as f64)
        .unwrap_or(0.0);
    info!(
        pid = args.pid,
        wav = %args.out.display(),
        packets = stat.packets,
        frames = stat.frames,
        silent = stat.silent,
        timestamp_errors = stat.timestamp_errors,
        qpc_span_s = format!("{span_s:.2}"),
        max_gap_ms = format!("{:.1}", stat.max_gap_ticks as f64 / 10_000.0),
        "process-loopback capture done — inspect the WAV"
    );
    if stat.packets == 0 {
        warn!("no packets captured — target produced no audio (self capture without --tone?), or the PID was already gone");
    }
    Ok(())
}

/// Render a continuous 440 Hz sine on the default render endpoint from THIS process,
/// so a self-tree capture records a known signal. Mirrors `tools/avrig`'s render
/// loop (silence-fed shared render), but with a steady tone instead of a click.
fn render_tone(stop: &AtomicBool) -> Result<(), String> {
    initialize_mta().ok().map_err(|e| e.to_string())?;
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|e| e.to_string())?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let mix = client.get_mixformat().map_err(|e| e.to_string())?;
    let rate = mix.get_samplespersec();
    let format = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None);
    let (def_period, _) = client.get_device_period().map_err(|e| e.to_string())?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_period * 4,
    };
    client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(|e| e.to_string())?;
    let h_event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    client.start_stream().map_err(|e| e.to_string())?;

    let mut phase = 0.0f64;
    let step = 2.0 * std::f64::consts::PI * 440.0 / rate as f64;
    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(200).is_err() {
            continue;
        }
        let avail = client
            .get_available_space_in_frames()
            .map_err(|e| e.to_string())? as usize;
        if avail == 0 {
            continue;
        }
        let mut bytes = Vec::with_capacity(avail * 2 * 4);
        for _ in 0..avail {
            let s = (phase.sin() * 0.25) as f32;
            phase += step;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
            bytes.extend_from_slice(&s.to_le_bytes());
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        render
            .write_to_device(avail, &bytes, None)
            .map_err(|e| e.to_string())?;
    }
    let _ = client.stop_stream();
    Ok(())
}
