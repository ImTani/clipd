//! Milestone-0 spike #3 — **WASAPI loopback + mic capture, timestamp probe**.
//!
//! Tracker M0 #3: "dump both to WAV, inspect timestamps during silence and
//! device unplug." This is where "60% of the pain lives" (01-PROJECT-PLAN §3).
//! It de-risks the audio-clock story of 02-AV-SYNC-SPEC §2:
//!
//! - **Per-packet QPC timestamps.** `wasapi`'s `read_from_device_to_deque`
//!   returns `BufferInfo { index, timestamp, flags }`, where `timestamp` is the
//!   QPC-correlated position of the first frame **in 100 ns ticks** — the exact
//!   quantity §2.2 says to stamp audio with (never sample-count × nominal rate).
//! - **Silence gaps (pitfall 2).** Desktop loopback delivers *nothing* when the
//!   endpoint is silent. The probe surfaces this as event-wait timeouts and as
//!   `timestamp` jumps larger than the frames delivered — the "clips desync
//!   after the game goes quiet" bug, made visible.
//! - **Device changes (pitfall 3).** Unplugging the mic mid-capture must not
//!   crash; the stream ends / errors and we log it (full rebuild is §7 / M2).
//!
//! Two independent capture threads (desktop = default **Render** device opened
//! in loopback; mic = default **Capture** device), each writing a 48 kHz stereo
//! f32 WAV and a per-packet stats summary. Runs ~6 s.
//!
//! ## Not this spike's job
//! No resampling controller, no AAC, no mux, no silence *synthesis* or device
//! *rebuild* — those are Milestone 2. This only proves capture + that the
//! timestamps/flags carry the information §2 depends on.
//!
//! Throwaway, standalone crate; never linked into `clipd`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use tracing::{error, info, warn};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

const CAPTURE_SECONDS: u64 = 6;
const SAMPLE_RATE: u32 = 48_000; // 02-AV-SYNC-SPEC §2.1 canonical internal rate
const CHANNELS: u16 = 2;
const TICKS_PER_SECOND: u64 = 10_000_000; // 100 ns ticks (§0)
/// A timestamp jump this many ticks beyond the frames delivered flags a gap.
const GAP_THRESHOLD_TICKS: i64 = 20_000; // 2 ms — mirrors spec §2.3 jitter bound

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Two streams captured concurrently on their own threads (each owns its COM
    // objects; nothing crosses the thread boundary).
    let stop = Arc::new(AtomicBool::new(false));
    let desktop = spawn_capture("desktop-loopback", Direction::Render, stop.clone());
    let mic = spawn_capture("mic", Direction::Capture, stop.clone());

    info!(
        seconds = CAPTURE_SECONDS,
        "capturing — PLAY audio then let it go SILENT to see the loopback gap; \
         speak into the mic; optionally UNPLUG the mic to test pitfall 3"
    );
    std::thread::sleep(Duration::from_secs(CAPTURE_SECONDS));
    stop.store(true, Ordering::Relaxed);

    let mut ok = true;
    for handle in [desktop, mic] {
        match handle.join() {
            Ok(Ok(summary)) => summary.report(),
            Ok(Err(e)) => {
                error!(error = %e, "capture thread returned an error");
                ok = false;
            }
            Err(_) => {
                error!("capture thread panicked");
                ok = false;
            }
        }
    }
    if ok {
        info!("spike OK — inspect the two WAVs and the per-stream summaries above");
    } else {
        std::process::exit(1);
    }
}

/// Per-stream capture statistics gathered over the run.
struct Summary {
    label: String,
    device_name: String,
    wav_path: PathBuf,
    packets: u64,
    frames: u64,
    silent_packets: u64,
    discontinuities: u64,
    timestamp_errors: u64,
    event_timeouts: u64,
    max_gap_ticks: i64,
    non_monotonic: u64,
    first_timestamp: Option<u64>,
    last_timestamp: u64,
    device_lost: bool,
}

impl Summary {
    fn report(&self) {
        let duration_s = self.frames as f64 / SAMPLE_RATE as f64;
        let span_s = self
            .first_timestamp
            .map(|f| (self.last_timestamp.saturating_sub(f)) as f64 / TICKS_PER_SECOND as f64)
            .unwrap_or(0.0);
        info!(
            stream = %self.label,
            device = %self.device_name,
            wav = %self.wav_path.display(),
            packets = self.packets,
            frames = self.frames,
            captured_s = format!("{duration_s:.2}"),
            qpc_span_s = format!("{span_s:.2}"),
            silent_packets = self.silent_packets,
            discontinuities = self.discontinuities,
            timestamp_errors = self.timestamp_errors,
            event_timeouts = self.event_timeouts,
            max_gap_ms = format!("{:.1}", self.max_gap_ticks as f64 / 10_000.0),
            non_monotonic = self.non_monotonic,
            device_lost = self.device_lost,
            "stream summary"
        );
        if self.frames == 0 {
            warn!(stream = %self.label, "no audio captured (silent endpoint / no mic?)");
        }
        if self.device_lost {
            info!(
                stream = %self.label,
                "device was lost mid-capture (unplug / invalidation) and the stream \
                 ended cleanly — pitfall 3. M2 rebuilds the stream + stamps silence (§7)."
            );
        }
        if self.max_gap_ticks > GAP_THRESHOLD_TICKS {
            info!(
                stream = %self.label,
                "timestamp gap > frames delivered — this is the loopback-silence \
                 behaviour M2 must fill with synthesized silence (pitfall 2)"
            );
        }
    }
}

/// Spawn a capture thread. `device_dir` selects the endpoint (Render → opened in
/// loopback to capture desktop audio; Capture → the mic). Returns a handle whose
/// join value is the stream summary.
fn spawn_capture(
    label: &str,
    device_dir: Direction,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Result<Summary, String>> {
    let label = label.to_string();
    std::thread::spawn(move || capture_stream(label, device_dir, stop).map_err(|e| e.to_string()))
}

fn capture_stream(
    label: String,
    device_dir: Direction,
    stop: Arc<AtomicBool>,
) -> Result<Summary, Box<dyn std::error::Error>> {
    // Each capture thread runs its own MTA apartment (CLAUDE.md COM rule).
    initialize_mta().ok()?;

    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&device_dir)?;
    let device_name = device
        .get_friendlyname()
        .unwrap_or_else(|_| "<unknown>".into());
    let mut audio_client = device.get_iaudioclient()?;

    // Ask for the canonical internal format; the engine converts (autoconvert)
    // from whatever the device runs natively — §2.1 (resample-always).
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS as usize,
        None,
    );
    let (def_period, _min_period) = audio_client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_period,
    };
    // Direction::Capture on a Render device makes `wasapi` set the LOOPBACK flag.
    audio_client.initialize_client(&format, &Direction::Capture, &mode)?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let bytes_per_frame = format.get_blockalign() as usize;

    let wav_path = std::env::temp_dir().join(format!("clipd_spike_audio_{label}.wav"));
    let mut wav = WavWriter::create(
        &wav_path,
        WavSpec {
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        },
    )?;

    let mut deque: VecDeque<u8> = VecDeque::with_capacity(bytes_per_frame * SAMPLE_RATE as usize);
    let mut summary = Summary {
        label: label.clone(),
        device_name,
        wav_path: wav_path.clone(),
        packets: 0,
        frames: 0,
        silent_packets: 0,
        discontinuities: 0,
        timestamp_errors: 0,
        event_timeouts: 0,
        max_gap_ticks: 0,
        non_monotonic: 0,
        first_timestamp: None,
        last_timestamp: 0,
        device_lost: false,
    };
    let mut prev_ts: Option<u64> = None;
    let mut logged_samples = 0u32;

    audio_client.start_stream()?;

    'capture: while !stop.load(Ordering::Relaxed) {
        // A timeout during silence is expected for loopback — count it, don't die.
        if h_event.wait_for_event(1000).is_err() {
            summary.event_timeouts += 1;
            continue;
        }
        // Drain every packet currently queued.
        loop {
            // A device unplug/invalidation (pitfall 3) surfaces as an error here.
            // End the stream cleanly and keep the summary — do NOT crash.
            let n = match capture_client.get_next_packet_size() {
                Ok(v) => v.unwrap_or(0),
                Err(e) => {
                    warn!(stream = %label, error = %e, "device lost (unplug/invalidation) — ending stream");
                    summary.device_lost = true;
                    break 'capture;
                }
            };
            if n == 0 {
                break;
            }
            let before = deque.len();
            let info = match capture_client.read_from_device_to_deque(&mut deque) {
                Ok(i) => i,
                Err(e) => {
                    warn!(stream = %label, error = %e, "device lost (unplug/invalidation) — ending stream");
                    summary.device_lost = true;
                    break 'capture;
                }
            };
            let frames = (deque.len().saturating_sub(before) / bytes_per_frame) as u64;

            summary.packets += 1;
            summary.frames += frames;
            if info.flags.silent {
                summary.silent_packets += 1;
            }
            if info.flags.data_discontinuity {
                summary.discontinuities += 1;
            }
            if info.flags.timestamp_error {
                summary.timestamp_errors += 1;
            }
            summary.first_timestamp.get_or_insert(info.timestamp);
            // Gap = actual QPC advance minus the frames actually delivered. A
            // device change can hand back a non-monotonic or garbage timestamp
            // (this is what crashed the first cut on mic-unplug), so guard it:
            // i128 math can't overflow, and a backward jump is a device event
            // (§0 monotonicity), not a silence gap — count it, don't measure it.
            if let Some(pt) = prev_ts {
                if info.flags.timestamp_error || info.timestamp < pt {
                    summary.non_monotonic += 1;
                } else {
                    let expected =
                        (frames as i128 * TICKS_PER_SECOND as i128) / SAMPLE_RATE as i128;
                    let actual = info.timestamp as i128 - pt as i128;
                    let gap = (actual - expected).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                    if gap > summary.max_gap_ticks {
                        summary.max_gap_ticks = gap;
                    }
                }
            }
            prev_ts = Some(info.timestamp);
            summary.last_timestamp = info.timestamp;

            // Log the first few packets in detail so the QPC stamps are visible.
            if logged_samples < 5 {
                logged_samples += 1;
                info!(
                    stream = %label,
                    packet = summary.packets,
                    frames,
                    index = info.index,
                    qpc_ticks = info.timestamp,
                    silent = info.flags.silent,
                    discontinuity = info.flags.data_discontinuity,
                    "packet"
                );
            }

            write_f32_frames(&mut wav, &mut deque)?;
        }
    }

    // Best-effort: a lost/invalidated device errors on stop — ignore it, the
    // WAV captured up to the unplug is still valid and worth finalizing.
    let _ = audio_client.stop_stream();
    wav.finalize()?;
    Ok(summary)
}

/// Drain whole f32 samples from the byte deque into the WAV writer.
fn write_f32_frames(
    wav: &mut WavWriter<std::io::BufWriter<std::fs::File>>,
    deque: &mut VecDeque<u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    while deque.len() >= 4 {
        let b = [
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
        ];
        wav.write_sample(f32::from_le_bytes(b))?;
    }
    Ok(())
}
