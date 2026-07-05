//! `engine` — the Milestone-1 recorder: three worker threads wired over bounded
//! channels, driven by the CFR pacing grid.
//!
//! ```text
//! capture ──InputFrame(bounded)──▶ encode ──EncodedPacket(bounded)──▶ mux ──▶ .mp4
//!    │  (WGC → VideoProcessor →         │  (async H.264 MFT, CQP)      │  (fMP4, crash-safe)
//!    │   pacing grid → NV12)            └─ output type ─(bounded 1)────┘
//! ```
//!
//! - **capture** owns WGC + the video processor + the pacing grid; it emits one
//!   NV12 frame per slot (fresh or a resubmit of the last, `02-AV-SYNC-SPEC §1`).
//! - **encode** runs the async MFT; it first hands the muxer the negotiated
//!   output media type (for the `avcC` box), then pumps packets.
//! - **mux** writes the crash-safe fMP4 on its own thread so a slow disk doesn't
//!   block the encode loop directly (plan pitfall 24 / data-flow rule 4). M1 has
//!   no ring buffer, so a sustained stall still back-pressures capture within the
//!   channel depth — full decoupling lands with the M3 ring.
//!
//! Shutdown propagates by channel disconnection: the main thread sets `stop`, the
//! capture loop breaks and drops its senders, the encoder drains, and the muxer
//! finalizes. Each worker body is wrapped in `catch_unwind` so a panic becomes an
//! error at the thread boundary instead of a silently dead thread under a live
//! process (the incumbent failure mode this project exists to kill).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, select, Receiver, Sender};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::{error, info, warn};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};

use crate::audio::devices::DeviceSelection;
use crate::audio::resample::StreamResampler;
use crate::audio::wasapi_stream::{run_capture, AudioPacket, AudioStreamKind};
use crate::capture::canvas::canvas_size;
use crate::capture::convert::Converter;
use crate::capture::pacing::{PacingGrid, SlotAction};
use crate::capture::resize::{ResizeTracker, DEFAULT_SETTLE_TICKS};
use crate::capture::wgc::{
    is_window, window_monitor_size, CaptureSource, CapturedFrame, WgcCapture,
};
use crate::clock::Clock;
use crate::com::ComMta;
use crate::encode::mft_aac::{f32_to_i16, AacEncoder, EncodedAudioPacket};
use crate::encode::mft_h264::{EncodedPacket, EncoderConfig, H264Encoder, InputFrame};
use crate::gpu::{AdapterSelection, GpuContext, GpuError};
use crate::mux::fmp4::{AudioTrackConfig, Fmp4Writer};
use crate::mux::SendMediaType;
use crate::ring::{Ring, RingCaps};
use crate::save::{self, select_window, SaveWindow};
use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::ring::{byte_cap_bytes, est_bitrate_bps};
use crate::spec_constants::units::{ms_to_ticks, TICKS_PER_SECOND};
use crate::spec_constants::video::nominal_frame_duration_ticks;
use crate::spec_constants::watchdog::{NO_WGC_FRAME_RESTART_MS, SAVE_DURATION_WARN_MS};
use crate::spec_constants::PRODUCT_NAME;
use crate::watchdog::PipelineStats;

/// Save-hotkey debounce: coalesce presses closer than this so a double-tap yields
/// one clip (`01-PROJECT-PLAN.md §3` pitfall 22, re-entrant/debounced saves). Not a
/// spec constant — matches the `§7` 250 ms debounce idiom for burst suppression.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Depth of the ring→save-worker job queue. Saves are rare and processed serially;
/// a tiny queue absorbs a burst without unbounded growth.
const SAVE_JOB_CHANNEL_CAP: usize = 4;

/// Input queue depth (capture → encode). Kept below the NV12 pool depth so an
/// in-flight frame never has its pool texture recycled under it.
const INPUT_CHANNEL_CAP: usize = 4;
/// Merged mux-item queue depth (video encode + audio process → mux). Carries both
/// video packets and AAC AUs; sized for ~1 s of the combined burst.
const MUX_CHANNEL_CAP: usize = 64;
/// Per-stream raw audio-packet queue depth (audio capture → audio process).
const AUDIO_PACKET_CHANNEL_CAP: usize = 16;

/// A window that has delivered no first frame for longer than this is treated as
/// uncapturable (exclusive-fullscreen) and capture falls back to the primary monitor
/// — the `02-AV-SYNC-SPEC §6.3` "No WGC frame … > 1 s" threshold, in ticks. Window
/// source only.
const NO_FRAME_TIMEOUT_TICKS: i64 = ms_to_ticks(NO_WGC_FRAME_RESTART_MS);
/// How often the capture thread polls `IsWindow` for a window close (cheap, but no
/// need every sub-slot nap).
const WINDOW_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// One item on the merged mux channel: a video packet, or an AAC access unit
/// tagged with its track index (`§2.5`: 0 = desktop, 1 = mic — the position in
/// the `AudioTrackConfig` slice passed to [`Fmp4Writer::create`]). A single
/// merged channel avoids `select!` over a variable number of audio channels; the
/// mux thread dispatches on the variant. Both payloads own their bytes (`Arc<[u8]>`),
/// so the enum is `Send` and cheap to `Clone` (an `Arc` bump — used to tee items to
/// the live timed recording, M4-3).
#[derive(Clone)]
enum MuxItem {
    /// An encoded H.264 packet for the video track.
    Video(EncodedPacket),
    /// An encoded AAC access unit for audio track `.0`.
    Audio(usize, EncodedAudioPacket),
}

/// Control for the live timed recording (M4-3), from the ring thread to the mux
/// worker. `Start` opens a new recording (which begins at the next teed video IDR);
/// `Stop` finalizes it.
enum RecordCtrl {
    /// Begin a recording written to this path.
    Start(PathBuf),
    /// Finalize the current recording.
    Stop,
}

/// Errors from any pipeline stage.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Capture (WGC) stage failure.
    #[error("capture stage: {0}")]
    Capture(#[from] crate::capture::wgc::CaptureError),
    /// Colour-conversion stage failure.
    #[error("convert stage: {0}")]
    Convert(#[from] crate::capture::convert::ConvertError),
    /// Encode stage failure.
    #[error("encode stage: {0}")]
    Encode(#[from] crate::encode::mft_h264::EncodeError),
    /// Audio capture (WASAPI) stage failure.
    #[error("audio capture stage: {0}")]
    AudioCapture(#[from] crate::audio::wasapi_stream::AudioError),
    /// Audio resample stage failure.
    #[error("audio resample stage: {0}")]
    Resample(#[from] crate::audio::resample::ResampleError),
    /// AAC encode stage failure.
    #[error("aac encode stage: {0}")]
    Aac(#[from] crate::encode::mft_aac::AacError),
    /// Mux stage failure.
    #[error("mux stage: {0}")]
    Mux(#[from] crate::mux::MuxError),
    /// Clock initialization failure.
    #[error("clock: {0}")]
    Clock(#[from] crate::clock::ClockError),
    /// Rebuilding the shared D3D11 device after a loss failed (`§7` restart).
    #[error("gpu rebuild: {0}")]
    Gpu(#[from] GpuError),
    /// A worker thread panicked (caught at the thread boundary).
    #[error("worker thread '{0}' panicked")]
    Panicked(&'static str),
    /// A setup channel closed before handing off its value.
    #[error("pipeline setup channel closed unexpectedly")]
    ChannelClosed,
}

impl EngineError {
    /// Whether this error is a D3D device-loss (`DXGI_ERROR_DEVICE_REMOVED` /
    /// `_RESET`) — the trigger for an epoch restart (`02-AV-SYNC-SPEC.md §7`,
    /// plan pitfalls 25/26: sleep/resume, driver reset, TDR all funnel here).
    pub fn is_device_lost(&self) -> bool {
        use crate::capture::convert::ConvertError;
        use crate::capture::wgc::CaptureError;
        use crate::encode::mft_h264::EncodeError;
        let code = match self {
            EngineError::Capture(CaptureError::Windows(e)) => e.code(),
            EngineError::Convert(ConvertError::Windows(e)) => e.code(),
            EngineError::Encode(EncodeError::Windows(e)) => e.code(),
            _ => return false,
        };
        code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET
    }
}

/// Spawn a named worker whose body is panic-isolated at the thread boundary.
fn spawn<T, F>(name: &'static str, body: F) -> JoinHandle<Result<T, EngineError>>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, EngineError> + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(
            move || match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
                Ok(result) => {
                    if let Err(e) = &result {
                        error!(thread = name, error = %e, "worker failed");
                    }
                    result
                }
                Err(_) => {
                    error!(thread = name, "worker panicked");
                    Err(EngineError::Panicked(name))
                }
            },
        )
        .expect("thread spawn should not fail")
}

/// Join a worker, converting a panic-on-join into a typed error.
fn join<T>(
    handle: JoinHandle<Result<T, EngineError>>,
    name: &'static str,
) -> Result<T, EngineError> {
    handle.join().map_err(|_| EngineError::Panicked(name))?
}

/// The capture thread: WGC → video processor → pacing grid → NV12 `InputFrame`s.
#[allow(clippy::too_many_arguments)]
fn capture_thread(
    gpu: GpuContext,
    source: CaptureSource,
    start_epoch: u32,
    max_encode_height: u32,
    cursor: bool,
    fps: u32,
    simulate_loss_after: Option<u64>,
    size_tx: Sender<(u32, u32)>,
    input_tx: Sender<InputFrame>,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    triggers_enabled: bool,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();
    let mut capture = WgcCapture::start(&gpu, source, cursor)?;
    let input_size = (capture.width(), capture.height());
    let mut window_hwnd = capture.window_hwnd();

    // Fixed output canvas (M4-2 / pitfall 11): the capture monitor's resolution capped
    // at `max_encode_height`, evened. A window is captured at its native (changing)
    // size and rescaled-to-fit (letterboxed) into this fixed canvas, so a resize
    // rebuilds only the input side (pool + converter) — the encoder/epoch are untouched
    // and a clip spans resizes at one resolution. For a monitor source the canvas is the
    // evened monitor (no scaling).
    let monitor_res = match window_hwnd {
        Some(h) => window_monitor_size(h).unwrap_or(input_size),
        None => input_size, // monitor source: the item size IS the monitor resolution
    };
    let canvas = canvas_size(monitor_res, max_encode_height);

    let mut converter = Converter::new(&gpu, input_size, canvas, fps)?;
    // Hand the encode thread the CANVAS (fixed for this epoch); ignore a closed
    // receiver (the engine is tearing down).
    let _ = size_tx.send(canvas);
    // Each capture thread serves ONE epoch (the engine spawns a fresh one per epoch on
    // a device-loss / capture-target-change restart); the grid carries this epoch's id
    // so ring packets are tagged for the §4.2 per-epoch save selection. A window RESIZE
    // is NOT an epoch — it is handled in-thread below (fixed canvas).
    let mut grid = PacingGrid::with_default_grace_at_epoch(fps, start_epoch);
    let clock = Clock::from_system()?;
    let duration = nominal_frame_duration_ticks(fps);

    // Test hook: after this instant, return a synthetic device loss so the
    // epoch-restart path can be exercised without an actual sleep/resume.
    let loss_deadline =
        simulate_loss_after.map(|s| std::time::Instant::now() + Duration::from_secs(s));

    // M4-2 triggers (buffer mode only — `triggers_enabled`). ALL are handled in-thread
    // on the fixed canvas, so a clip spans them at one resolution and NO epoch starts
    // (only a device loss restarts the epoch, via the supervisor): a RESIZE rescales the
    // window into the canvas; a window CLOSE / exclusive-fullscreen NO-FRAME switches the
    // source to the primary monitor scaled into the same canvas. The record path passes
    // `false` (a size change ends the segment, pitfall 11).
    let mut resize = ResizeTracker::new(input_size, DEFAULT_SETTLE_TICKS);
    let session_start = clock.now_ticks();
    let mut last_window_check = std::time::Instant::now();

    // The newest captured (BGRA) frame not yet converted, and the last NV12 we
    // produced (for resubmits on a static screen).
    let mut latest_frame: Option<CapturedFrame> = None;
    let mut last_nv12 = None;

    while !stop.load(Ordering::Relaxed) {
        let now = clock.now_ticks();
        // Drain WGC arrivals into the grid, keeping only the newest (keep-latest);
        // feed each frame's ContentSize to the resize debouncer (buffer mode).
        while let Some(frame) = capture.take_latest() {
            grid.on_arrival(frame.system_relative_time);
            if triggers_enabled {
                if let Ok(cs) = frame.content_size() {
                    resize.observe(cs, now);
                }
            }
            latest_frame = Some(frame);
        }

        // M4-2 triggers (buffer mode). Checked every iteration so they fire even when
        // no slot is due and on a static screen where no frame drives the loop. ALL are
        // in-thread on the fixed canvas → the clip spans them, no epoch.
        if triggers_enabled {
            // RESIZE settled → rebuild the input side (pool + converter) to the fixed
            // canvas and keep going in the SAME epoch (no encoder change, clip spans).
            if let Some(new_input) = resize.poll(now) {
                info!(
                    w = new_input.0,
                    h = new_input.1,
                    "window resized — rescaling into the fixed canvas (no epoch)"
                );
                capture.recreate_pool(new_input)?;
                converter = Converter::new(&gpu, new_input, canvas, fps)?;
                // Discard any frame captured at the OLD size still in the cell, so the
                // new (new-size) converter never gets a mismatched texture. The stale
                // last NV12 is dropped too; the §1.2 resubmit rule fills the brief gap
                // until the first frame at the new size arrives.
                while capture.take_latest().is_some() {}
                latest_frame = None;
                last_nv12 = None;
                continue;
            }
            // CLOSE / NO-FRAME → switch the source to the primary monitor, scaled into
            // the SAME canvas (no epoch). The clip keeps the pre-close window footage,
            // then continues on the monitor.
            if window_hwnd.is_some()
                && should_fall_back_to_monitor(
                    window_hwnd,
                    &capture,
                    grid.base(),
                    session_start,
                    now,
                    &mut last_window_check,
                )
            {
                info!("switching capture to the primary monitor (same canvas, no epoch)");
                capture = WgcCapture::start(&gpu, CaptureSource::PrimaryMonitor, cursor)?;
                let new_input = (capture.width(), capture.height());
                converter = Converter::new(&gpu, new_input, canvas, fps)?;
                window_hwnd = None; // now a monitor — no more window triggers
                resize = ResizeTracker::new(new_input, DEFAULT_SETTLE_TICKS);
                latest_frame = None;
                last_nv12 = None;
                continue;
            }
        }

        let Some(action) = grid.poll(now) else {
            // Not yet time for the next slot; nap sub-slot and re-check.
            std::thread::sleep(Duration::from_micros(500));
            continue;
        };

        let nv12 = match action {
            SlotAction::Fresh { .. } => match latest_frame.take() {
                Some(frame) => {
                    let bgra = frame.texture()?;
                    let converted = converter.convert(&bgra)?;
                    last_nv12 = Some(converted.clone());
                    converted
                }
                // Grid said fresh but the frame was already consumed — fall back.
                None => match last_nv12.clone() {
                    Some(n) => n,
                    None => continue,
                },
            },
            SlotAction::Resubmit { .. } => match last_nv12.clone() {
                Some(n) => n,
                None => continue,
            },
        };

        let frame = InputFrame {
            texture: nv12,
            pts: action.pts(),
            duration,
            epoch_id: grid.epoch_id(),
        };
        // A closed receiver means the encoder stopped; end the loop.
        if input_tx.send(frame).is_err() {
            break;
        }
        captured.fetch_add(1, Ordering::Relaxed);

        // Test hook: fire the synthetic device loss once the deadline passes (after
        // some frames have been sent, so the segment has content).
        if let Some(deadline) = loss_deadline {
            if std::time::Instant::now() >= deadline {
                warn!("simulated device loss (--simulate-device-loss) — triggering epoch restart");
                return Err(EngineError::Convert(
                    crate::capture::convert::ConvertError::Windows(windows::core::Error::from(
                        DXGI_ERROR_DEVICE_REMOVED,
                    )),
                ));
            }
        }
    }
    Ok(())
}

/// Whether a captured **window** has become uncapturable and the source should switch
/// to the primary monitor (M4-2): a window **close** (`IsWindow` false — the `Closed`
/// event does not fire on Win11) or an exclusive-fullscreen **no-frame** timeout. The
/// caller does the switch in-thread (same canvas, no epoch). Window-source only; a
/// *resize* is handled separately.
fn should_fall_back_to_monitor(
    window_hwnd: Option<isize>,
    capture: &WgcCapture,
    grid_base: Option<i64>,
    session_start: i64,
    now: i64,
    last_window_check: &mut Instant,
) -> bool {
    let Some(h) = window_hwnd else {
        return false;
    };
    // Throttle the `IsWindow` poll; it need not run every sub-slot nap.
    if last_window_check.elapsed() >= WINDOW_CHECK_INTERVAL {
        *last_window_check = Instant::now();
        if !is_window(h) || capture.is_closed() {
            warn!("captured window closed");
            return true;
        }
    }
    // Exclusive-fullscreen: a window that never delivered a first frame.
    if grid_base.is_none() && now.saturating_sub(session_start) > NO_FRAME_TIMEOUT_TICKS {
        warn!("no frames from the window (exclusive-fullscreen?) — §6.3");
        return true;
    }
    false
}

/// The encode thread: async H.264 MFT with CQP; hands the muxer the output type,
/// then pumps encoded packets onto the merged mux channel.
#[allow(clippy::too_many_arguments)]
fn encode_thread(
    gpu: GpuContext,
    epoch: u32,
    fps: u32,
    cq: u32,
    gop_frames: u32,
    size_rx: Receiver<(u32, u32)>,
    input_rx: Receiver<InputFrame>,
    mt_tx: Sender<(u32, SendMediaType)>,
    item_tx: Sender<MuxItem>,
    encoded: Arc<AtomicU64>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();
    let (width, height) = size_rx.recv().map_err(|_| EngineError::ChannelClosed)?;

    let mut encoder = H264Encoder::new(
        &gpu,
        EncoderConfig {
            width,
            height,
            fps,
            cq,
            gop_frames,
        },
    )?;
    encoder.begin()?;
    // Hand the negotiated output type (with SPS/PPS) to the sink, tagged with this
    // epoch's id: a resolution change starts a new epoch with new SPS/PPS, and a
    // save must mux with the type matching the clip's epoch (`§0`/`§4.2`).
    let output_type = encoder.output_media_type()?;
    let _ = mt_tx.send((epoch, SendMediaType(output_type)));

    encoder.pump(
        || input_rx.recv().ok(),
        |packet| {
            // A closed muxer just means we drop the tail; keep draining.
            let _ = item_tx.send(MuxItem::Video(packet));
            encoded.fetch_add(1, Ordering::Relaxed);
        },
    )?;
    Ok(())
}

/// The audio-process thread for one stream: owns the native→48 kHz resampler and
/// the AAC-LC encoder on this MTA thread (COM objects are never moved from
/// another thread — `CLAUDE.md` COM rule). It hands the muxer this track's
/// `AudioSpecificConfig` *before* any data (so the moov can be built), then per
/// capture packet: resample → f32→i16 → AAC → merged channel.
fn audio_process_thread(
    kind: AudioStreamKind,
    track_index: usize,
    bitrate_bps: u32,
    pkt_rx: Receiver<AudioPacket>,
    asc_tx: Sender<(usize, AudioTrackConfig)>,
    item_tx: Sender<MuxItem>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();

    // The AAC encoder produces the ASC at construction — it needs no sample rate,
    // so hand it to the muxer immediately, before the first capture packet.
    let mut encoder = AacEncoder::new(kind, bitrate_bps)?;
    // A silence template lets the muxer fill leading silence for a late-starting
    // track (e.g. a mic that delivers its first AU ~30–60 ms after capture on an
    // early save) so every track begins at the clip origin within ≤ 1 AAC frame
    // (`§4.4`). Best-effort: on failure the muxer falls back to the plain head slack.
    let silent_au = match AacEncoder::silent_au(bitrate_bps) {
        Ok(au) => au,
        Err(e) => {
            warn!(error = %e, "no AAC silence template — save head-silence fill disabled for this track");
            Vec::new()
        }
    };
    let cfg = AudioTrackConfig {
        asc: encoder.audio_specific_config().to_vec(),
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE_HZ,
        silent_au,
    };
    if asc_tx.send((track_index, cfg)).is_err() {
        return Ok(()); // muxer gone during setup — nothing to produce
    }
    drop(asc_tx); // ASC is the only setup message; release so the mux can proceed

    // The resampler needs the device's native rate, which only arrives on the
    // first packet (`AudioPacket::sample_rate`), so it is built lazily.
    let mut resampler: Option<StreamResampler> = None;

    while let Ok(pkt) = pkt_rx.recv() {
        match resampler.as_mut() {
            // A §7 rebuild that landed on a different-rate device: switch the
            // resampler's input rate while keeping the output timeline continuous.
            Some(rs) if rs.native_rate() != pkt.sample_rate => {
                rs.switch_native_rate(pkt.sample_rate)?
            }
            Some(_) => {}
            None => resampler = Some(StreamResampler::new(pkt.sample_rate)?),
        }
        let rs = resampler.as_mut().expect("resampler built above");
        for chunk in rs.process(&pkt)? {
            if !push_aac(
                &mut encoder,
                track_index,
                chunk.pts,
                &chunk.samples,
                &item_tx,
            )? {
                return Ok(()); // muxer gone — stop cleanly
            }
        }
    }

    // Stop requested (capture dropped its sender): flush the resampler delay line
    // and the encoder tail so the track ends within one AAC frame of the audio.
    if let Some(mut rs) = resampler {
        for chunk in rs.finish()? {
            if !push_aac(
                &mut encoder,
                track_index,
                chunk.pts,
                &chunk.samples,
                &item_tx,
            )? {
                return Ok(());
            }
        }
    }
    for au in encoder.finish()? {
        if item_tx.send(MuxItem::Audio(track_index, au)).is_err() {
            break;
        }
    }
    Ok(())
}

/// Encode one 48 kHz chunk to AAC and forward every access unit to the muxer.
/// Returns `false` if the mux channel has closed (the caller should stop).
fn push_aac(
    encoder: &mut AacEncoder,
    track_index: usize,
    pts: i64,
    samples: &[f32],
    item_tx: &Sender<MuxItem>,
) -> Result<bool, EngineError> {
    let pcm = f32_to_i16(samples);
    for au in encoder.encode(&pcm, pts)? {
        if item_tx.send(MuxItem::Audio(track_index, au)).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// Buffer mode (M3): continuous capture into the ring; a hotkey saves the last N s.
// ─────────────────────────────────────────────────────────────────────────────
//
// The same capture/encode/audio producers as record mode, but the sink is the
// ring (02-AV-SYNC-SPEC §3) instead of a duration-bound muxer — the ring is the
// pipeline spine (01-PROJECT-PLAN §2; DECISIONS 2026-07-04). Two new threads:
//
//   producers ──MuxItem──▶ RING THREAD ──(hotkey)──SaveJob──▶ SAVE WORKER ──▶ .mp4
//                          (Ring + §4 select)                 (reused Fmp4Writer)
//
// The ring thread owns the Ring and select!s over the merged MuxItem channel and
// the global hotkey receiver; on a save press it runs the pure §4 `select_window`
// (cheap Arc-handle clones) and hands the SAVE WORKER an owned window, so muxing
// happens entirely off the ring — the RAM-budget discipline the Arc<[u8]> packet
// bytes exist for. The save worker holds the encoder output type + track ASCs
// (like the record mux thread) and builds a fresh crash-safe fMP4 per save.
//
// Epoch restart (§7, M4-2): the ring thread + save worker are the PERSISTENT CORE
// (spawned once, live the whole session); the capture/encode/audio PRODUCERS are a
// rebuildable set. A `buffer_supervisor` thread owns the epoch loop: it spawns the
// core, then loops spawning a producer set per epoch feeding the SAME ring (the ring
// spans epochs — §7 "older epochs remain saveable"). On a device loss the producers
// exit, the supervisor bumps the epoch and rebuilds them into the same ring/save;
// the ring is never torn down, so a save right after a restart still finds the last
// pre-loss GOPs. The save worker holds an output type PER EPOCH (a resolution change
// = new SPS/PPS) and a save selects the type matching the clip's epoch (§4.2). This
// turn wires the DEVICE-LOSS trigger (self-verified via `--simulate-device-loss`,
// like the record path); the window resize/close + no-frame triggers ride the same
// machinery and land next. Auto-QP-relief (§6.2) is still deferred (needs live-encoder
// QP tuning on hardware).

/// Parameters for a buffer (replay) session.
#[derive(Debug, Clone)]
pub struct BufferParams {
    /// What to capture (`config.capture.target`, pitfall 31).
    pub capture_source: CaptureSource,
    /// How to (re)create the shared D3D11 device — needed to rebuild it on a
    /// device-loss epoch restart (`§7`).
    pub adapter: AdapterSelection,
    /// Encode-height ceiling for the fixed output canvas (`config.encode.max_height`,
    /// M4-2 / pitfall 11).
    pub max_encode_height: u32,
    /// Output frame rate (the CFR grid rate).
    pub fps: u32,
    /// Whether to composite the cursor.
    pub cursor: bool,
    /// Constant quality / QP (spec §6.1).
    pub cq: u32,
    /// Closed-GOP IDR interval in frames (spec §3).
    pub gop_frames: u32,
    /// Capture the desktop-loopback stream as audio track 0 (`§2.5`).
    pub desktop_audio: bool,
    /// Capture the microphone as the next audio track (`§2.5`).
    pub mic_audio: bool,
    /// Mic endpoint policy (`§7`). Ignored when `mic_audio` is false.
    pub mic_selection: DeviceSelection,
    /// AAC bitrate per audio track (`§2.6`).
    pub audio_bitrate_bps: u32,
    /// Retained buffer duration in seconds (`§3`, `config.buffer.seconds`).
    pub buffer_seconds: u32,
    /// Clear the ring after a successful save (`config.buffer.clear_after_save`).
    pub clear_after_save: bool,
    /// Directory saved clips are written to (already resolved to an absolute path).
    pub output_dir: PathBuf,
    /// The `global-hotkey` event id of the save hotkey (from [`crate::hotkey::HotkeyPump`]).
    pub save_hotkey_id: u32,
    /// The `global-hotkey` event id of the record-toggle hotkey (M4-3/M4-4). A press
    /// starts a timed recording; a second press stops it.
    pub record_hotkey_id: u32,
    /// Start a timed recording at buffer start and stop it (with the `§4`-clean
    /// tail-drain) after this long. Drives the `--record-secs N` test hook AND the
    /// `record --seconds N` subcommand (which runs on this converged ring+disk path
    /// since `RecordingEngine` was retired). `None` in normal buffer operation (the
    /// hotkey drives recording).
    pub record_auto: Option<Duration>,
    /// Destination for a `record_auto` recording when the caller wants a specific
    /// file (the `record` subcommand's `--out`). `None` → the default
    /// `<product>_rec_<ms>.mp4` in `output_dir` (buffer-mode hotkey / `--record-secs`).
    pub record_out: Option<PathBuf>,
    /// Start a timed recording at buffer start (the `record` subcommand, or the
    /// `--record-secs` hook). With `record_auto = Some(d)` it also auto-stops after
    /// `d` (`§4`-clean tail-drain); with `None` it records until the session stops
    /// (`record` without `--seconds`). `false` in normal buffer mode (hotkey-driven).
    pub record_autostart: bool,
    /// Test-only (`--autosave N`): fire a save on this interval, in addition to the
    /// hotkey, so the 50-consecutive-saves and 24-hour-soak acceptance tests run
    /// unattended. `None` in normal operation. Exercises the same `§4` save path as
    /// the hotkey (a hidden hook, like `--simulate-device-loss`).
    pub autosave: Option<Duration>,
    /// Test-only (`--simulate-device-loss N`): inject a synthetic device loss in the
    /// first epoch's capture thread after this many seconds, to exercise the
    /// buffer-mode epoch restart (§7) without an actual sleep/resume. `None` in
    /// normal operation. Only the first epoch simulates, so the rebuild doesn't loop.
    pub simulate_loss_after: Option<u64>,
}

/// A save job handed from the ring thread to the save worker: an owned `§4` window
/// plus the destination path. The window owns cloned (`Arc`) packets, so the ring
/// keeps running (and may `clear`) while the worker muxes.
struct SaveJob {
    window: SaveWindow,
    path: PathBuf,
}

/// A running buffer session: a `supervisor` thread owning the persistent ring +
/// save worker and a rebuildable producer set (capture/encode/audio) across epochs.
/// Drive it from the main thread (wait, then [`Self::stop_and_join`]).
pub struct BufferEngine {
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
    supervisor: JoinHandle<Result<(), EngineError>>,
}

impl BufferEngine {
    /// Spawn the buffer pipeline. Returns immediately; capture flows into the ring
    /// and a save-hotkey press writes the last `buffer_seconds` to `output_dir`. A
    /// mid-buffer device loss triggers an epoch restart without ending the session
    /// (`§7`) — the ring survives, so a save right after the restart still finds the
    /// last pre-loss GOPs.
    pub fn start(gpu: GpuContext, params: BufferParams) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = PipelineStats::new();
        let supervisor = {
            let stop = stop.clone();
            let stats = stats.clone();
            spawn("supervisor", move || {
                buffer_supervisor(gpu, params, stop, stats)
            })
        };
        Self {
            stop,
            stats,
            supervisor,
        }
    }

    /// Whether the session has ended on its own (a fatal error, or a clean wind-down).
    /// A device loss does NOT end it — the supervisor rebuilds the producers.
    pub fn any_worker_finished(&self) -> bool {
        self.supervisor.is_finished()
    }

    /// A live snapshot of the stage counters.
    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Signal stop and join. The supervisor tears down the current producers and the
    /// persistent core (ring + save) and surfaces the first fatal error, if any.
    pub fn stop_and_join(self) -> Result<(), EngineError> {
        self.stop.store(true, Ordering::Relaxed);
        join(self.supervisor, "supervisor")
    }
}

/// Epoch-restart budget (`§7`): pause before rebuilding producers so a lost device
/// (sleep/resume, TDR) has time to come back. Mirrors the record path's 500 ms.
const EPOCH_REBUILD_SLEEP: Duration = Duration::from_millis(500);
/// Depth of the per-epoch media-type channel (encode → save worker). One message per
/// epoch (rare — device loss); a small buffer absorbs a burst of restarts while the
/// save worker is busy muxing.
const EPOCH_TYPE_CHANNEL_CAP: usize = 8;
/// Depth of the record-control channel (ring thread → mux worker). Toggles are rare.
const RECORD_CTRL_CHANNEL_CAP: usize = 4;
/// Depth of the teed record-item channel (ring thread → mux worker). Sized for ~2 s of
/// video+audio so a one-shot save briefly muxing does not stall the tee.
const RECORD_ITEM_CHANNEL_CAP: usize = 256;
/// Max time to drain audio to the video tail when stopping a recording (§5 AV-3). The
/// audio lags video by ~90 ms, so it catches up quickly; this is a safety net.
const RECORD_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// The buffer supervisor: owns the persistent ring + save worker and an epoch loop
/// that (re)spawns the producer set. The ring/save channels' tx ends are retained
/// here so a producer set exiting (device loss) does not disconnect and tear down the
/// core; each producer set gets fresh clones. See the module comment above.
fn buffer_supervisor(
    gpu: GpuContext,
    params: BufferParams,
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
) -> Result<(), EngineError> {
    // Enabled audio streams in §2.5 order (desktop first, mic second).
    let mut audio_streams: Vec<(AudioStreamKind, DeviceSelection)> = Vec::new();
    if params.desktop_audio {
        audio_streams.push((AudioStreamKind::Desktop, DeviceSelection::DefaultFollow));
    }
    if params.mic_audio {
        audio_streams.push((AudioStreamKind::Mic, params.mic_selection.clone()));
    }
    let num_audio = audio_streams.len();

    // Persistent channels — the ring + mux worker recv these for the whole session.
    let (item_tx, item_rx) = bounded::<MuxItem>(MUX_CHANNEL_CAP);
    let (mt_tx, mt_rx) = bounded::<(u32, SendMediaType)>(EPOCH_TYPE_CHANNEL_CAP);
    let (asc_tx, asc_rx) =
        bounded::<(usize, AudioTrackConfig)>(num_audio.max(1) * EPOCH_TYPE_CHANNEL_CAP);
    let (save_job_tx, save_job_rx) = bounded::<SaveJob>(SAVE_JOB_CHANNEL_CAP);
    // Timed recording (M4-3): the ring thread controls (`rec_ctrl`) and tees each
    // MuxItem (`rec_item`) to the mux worker while recording. `rec_item` is generously
    // buffered so a one-shot save briefly muxing does not stall the tee.
    let (rec_ctrl_tx, rec_ctrl_rx) = bounded::<RecordCtrl>(RECORD_CTRL_CHANNEL_CAP);
    let (rec_item_tx, rec_item_rx) = bounded::<MuxItem>(RECORD_ITEM_CHANNEL_CAP);

    // Ring caps (§3/§6.2): retain buffer_seconds + one GOP of pre-roll margin so a
    // full-length save finds an IDR at/before the target rather than clamping at the
    // whole-GOP eviction boundary (the §4.2 clamp then fires only for genuine
    // shortfalls: buffer not full yet, or an epoch boundary within the window).
    // DECISIONS. The byte cap uses a nominal 1080p tier (the real frame size isn't
    // known until the first frame; it only shifts the byte cap, duration is primary).
    let gop_seconds = (params.gop_frames / params.fps.max(1)).max(1);
    let retained_seconds = params.buffer_seconds + gop_seconds;
    let est = est_bitrate_bps(1920, 1080, params.fps);
    let ring_caps = RingCaps {
        max_duration_ticks: retained_seconds as i64 * TICKS_PER_SECOND,
        max_bytes: byte_cap_bytes(retained_seconds, est),
        num_audio_tracks: num_audio,
    };

    // Persistent core: ring thread + mux worker (spawned once; survive restarts).
    let ring = {
        let stop = stop.clone();
        let consumed = stats.muxed.clone();
        let cfg = RingThreadConfig {
            buffer_seconds: params.buffer_seconds,
            clear_after_save: params.clear_after_save,
            output_dir: params.output_dir.clone(),
            save_hotkey_id: params.save_hotkey_id,
            record_hotkey_id: params.record_hotkey_id,
            autosave: params.autosave,
            record_auto: params.record_auto,
            record_out: params.record_out.clone(),
            record_autostart: params.record_autostart,
        };
        spawn("ring", move || {
            ring_thread(
                ring_caps,
                cfg,
                item_rx,
                save_job_tx,
                rec_ctrl_tx,
                rec_item_tx,
                consumed,
                stop,
            )
        })
    };
    let save = spawn("save", move || {
        mux_worker_thread(
            num_audio,
            mt_rx,
            asc_rx,
            save_job_rx,
            rec_ctrl_rx,
            rec_item_rx,
        )
    });

    // Epoch loop: (re)spawn the producer set feeding the persistent core. Only a
    // DEVICE LOSS bumps the epoch and rebuilds (rebuilding the D3D device too — a real
    // loss killed the old one); resize/close/no-frame are handled in-thread by the
    // capture thread (fixed canvas, same epoch). A stop or a fatal error ends the loop.
    let mut gpu = gpu;
    let mut epoch: u32 = 0;
    let outcome: Result<(), EngineError> = loop {
        // Only the first epoch honours the simulate hook, so the rebuild doesn't loop.
        let simulate = if epoch == 0 {
            params.simulate_loss_after
        } else {
            None
        };
        let producers = spawn_buffer_producers(
            gpu.clone(),
            &params,
            params.capture_source,
            epoch,
            simulate,
            &audio_streams,
            item_tx.clone(),
            mt_tx.clone(),
            asc_tx.clone(),
            &stats,
        );

        // Wait until stop is requested or a producer exits (device loss / error).
        let mut ticks = 0u32;
        while !stop.load(Ordering::Relaxed) && !producers.any_finished() {
            std::thread::sleep(Duration::from_millis(100));
            ticks += 1;
            if ticks.is_multiple_of(10) {
                stats.check_divergence();
            }
        }

        // Bring the whole set down (a device loss only exits capture; the independent
        // audio threads must be told too), then classify why it ended.
        producers.stop();
        match producers.join_and_classify() {
            ProducerOutcome::DeviceLost if !stop.load(Ordering::Relaxed) => {
                epoch += 1;
                warn!(
                    epoch,
                    "device lost mid-buffer — rebuilding into a new epoch (§7)"
                );
                std::thread::sleep(EPOCH_REBUILD_SLEEP);
                // Rebuild the device: a real loss invalidated it (a simulated loss did
                // not, but a fresh device is harmless). Retry within the §7 budget.
                gpu = match rebuild_gpu(params.adapter) {
                    Ok(g) => g,
                    Err(e) => break Err(EngineError::from(e)),
                };
                continue;
            }
            ProducerOutcome::DeviceLost | ProducerOutcome::Completed => break Ok(()),
            ProducerOutcome::Failed(e) => break Err(e),
        }
    };

    // Shutdown: drop the ring's input so it disconnects and exits (dropping its
    // save-job sender), which lets the save worker's save-job channel disconnect →
    // it drains and exits. mt_tx/asc_tx stay alive until after the save join so the
    // save worker's select never busy-spins on a disconnected type/ASC channel.
    drop(item_tx);
    let ring_res = join(ring, "ring");
    let save_res = join(save, "save");
    drop(mt_tx);
    drop(asc_tx);

    outcome?;
    ring_res?;
    save_res?;
    Ok(())
}

/// Rebuild the shared D3D11 device after a loss, retrying within the `§7` restart
/// budget (~2 s) while the device comes back (sleep/resume, TDR).
fn rebuild_gpu(adapter: AdapterSelection) -> Result<GpuContext, GpuError> {
    let mut last_err = None;
    for _ in 0..20 {
        match GpuContext::new(adapter) {
            Ok(gpu) => return Ok(gpu),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last_err.expect("at least one attempt was made"))
}

/// Why a producer set ended.
enum ProducerOutcome {
    /// Clean stop (session stop requested).
    Completed,
    /// A D3D device loss in capture/encode — rebuild the same source into a new epoch
    /// (`§7`), rebuilding the device. (M4-2 resize/close/no-frame do NOT reach here —
    /// they are handled in-thread by the capture thread on the fixed canvas.)
    DeviceLost,
    /// A fatal (non-device-loss) error — end the session and surface it.
    Failed(EngineError),
}

/// The rebuildable producers for one buffer epoch (capture + encode + audio),
/// feeding the persistent ring/save via cloned channel senders. Owns a per-epoch
/// stop flag distinct from the session stop (so a device-loss rebuild is not
/// mistaken for a user request to end the session — mirrors [`RecordingEngine`]).
struct ProducerSet {
    epoch_stop: Arc<AtomicBool>,
    capture: JoinHandle<Result<(), EngineError>>,
    encode: JoinHandle<Result<(), EngineError>>,
    audio: Vec<JoinHandle<Result<(), EngineError>>>,
}

impl ProducerSet {
    /// Whether any producer has exited (device loss / error).
    fn any_finished(&self) -> bool {
        self.capture.is_finished()
            || self.encode.is_finished()
            || self.audio.iter().any(JoinHandle::is_finished)
    }

    /// Signal this epoch's producers to stop. A device loss only exits capture/encode
    /// on its own; the independent audio threads keep running until told.
    fn stop(&self) {
        self.epoch_stop.store(true, Ordering::Relaxed);
    }

    /// Join every producer and classify why the set ended (device loss vs clean stop
    /// vs fatal error). Mirrors [`RecordingEngine::stop_and_join`]'s classification.
    fn join_and_classify(self) -> ProducerOutcome {
        let capture = join(self.capture, "capture");
        let encode = join(self.encode, "encode");
        let audio: Vec<Result<(), EngineError>> =
            self.audio.into_iter().map(|h| join(h, "audio")).collect();

        let audio_failures = audio.iter().filter(|r| r.is_err()).count();
        if audio_failures > 0 {
            warn!(audio_failures, "audio worker(s) ended in error");
        }
        let device_lost = [&capture, &encode]
            .iter()
            .filter_map(|r| r.as_ref().err())
            .any(EngineError::is_device_lost);
        if device_lost {
            return ProducerOutcome::DeviceLost;
        }
        if let Err(e) = capture {
            return ProducerOutcome::Failed(e);
        }
        if let Err(e) = encode {
            return ProducerOutcome::Failed(e);
        }
        ProducerOutcome::Completed
    }
}

/// Spawn the capture/encode/audio producers for `epoch`, feeding `item_tx` (→ ring)
/// and `mt_tx`/`asc_tx` (→ save worker) via clones the producers own (so their exit
/// drops those clones without disconnecting the persistent core).
#[allow(clippy::too_many_arguments)]
fn spawn_buffer_producers(
    gpu: GpuContext,
    params: &BufferParams,
    source: CaptureSource,
    epoch: u32,
    simulate_loss_after: Option<u64>,
    audio_streams: &[(AudioStreamKind, DeviceSelection)],
    item_tx: Sender<MuxItem>,
    mt_tx: Sender<(u32, SendMediaType)>,
    asc_tx: Sender<(usize, AudioTrackConfig)>,
    stats: &PipelineStats,
) -> ProducerSet {
    let epoch_stop = Arc::new(AtomicBool::new(false));

    let (size_tx, size_rx) = bounded::<(u32, u32)>(1);
    let (input_tx, input_rx) = bounded::<InputFrame>(INPUT_CHANNEL_CAP);

    let capture = {
        let gpu = gpu.clone();
        let stop = epoch_stop.clone();
        let captured = stats.captured.clone();
        let (cursor, fps, max_h) = (params.cursor, params.fps, params.max_encode_height);
        spawn("capture", move || {
            capture_thread(
                gpu,
                source,
                epoch,
                max_h,
                cursor,
                fps,
                simulate_loss_after,
                size_tx,
                input_tx,
                stop,
                captured,
                true, // buffer mode: M4-2 triggers enabled
            )
        })
    };
    let encode = {
        let gpu = gpu.clone();
        let encoded = stats.encoded.clone();
        let (fps, cq, gop) = (params.fps, params.cq, params.gop_frames);
        let item_tx = item_tx.clone();
        spawn("encode", move || {
            encode_thread(
                gpu, epoch, fps, cq, gop, size_rx, input_rx, mt_tx, item_tx, encoded,
            )
        })
    };

    let mut audio: Vec<JoinHandle<Result<(), EngineError>>> = Vec::new();
    for (track_index, (kind, selection)) in audio_streams.iter().cloned().enumerate() {
        let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
        let cap_stop = epoch_stop.clone();
        audio.push(spawn("audio-capture", move || {
            Ok(run_capture(kind, selection, apkt_tx, cap_stop)?)
        }));
        let asc_tx = asc_tx.clone();
        let item_tx = item_tx.clone();
        let bitrate = params.audio_bitrate_bps;
        audio.push(spawn("audio-process", move || {
            audio_process_thread(kind, track_index, bitrate, apkt_rx, asc_tx, item_tx)
        }));
    }
    // The passed-in item_tx/asc_tx clones drop here; the producers hold their own.

    ProducerSet {
        epoch_stop,
        capture,
        encode,
        audio,
    }
}

/// Settings for the [`ring_thread`] (grouped to keep its arg list sane).
struct RingThreadConfig {
    buffer_seconds: u32,
    clear_after_save: bool,
    output_dir: PathBuf,
    save_hotkey_id: u32,
    record_hotkey_id: u32,
    autosave: Option<Duration>,
    record_auto: Option<Duration>,
    record_out: Option<PathBuf>,
    record_autostart: bool,
}

/// The ring thread: push producers' packets into the [`Ring`]; on a save-hotkey press
/// run the pure `§4` [`select_window`] and dispatch a window to the mux worker; on the
/// record-toggle hotkey start/stop a live timed recording, teeing each `MuxItem` to the
/// mux worker while active (M4-3). Owns the `Ring`; needs no COM.
#[allow(clippy::too_many_arguments)]
fn ring_thread(
    ring_caps: RingCaps,
    cfg: RingThreadConfig,
    item_rx: Receiver<MuxItem>,
    save_job_tx: Sender<SaveJob>,
    rec_ctrl_tx: Sender<RecordCtrl>,
    rec_item_tx: Sender<MuxItem>,
    consumed: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Result<(), EngineError> {
    let clock = Clock::from_system()?;
    let buffer_ticks = cfg.buffer_seconds as i64 * TICKS_PER_SECOND;
    let hotkey_rx = GlobalHotKeyEvent::receiver();
    // The `--autosave N` test hook fires on a tick; `never()` when disabled.
    let autosave_rx = match cfg.autosave {
        Some(d) => crossbeam_channel::tick(d),
        None => crossbeam_channel::never::<Instant>(),
    };
    // `--record-secs N` test hook: fire once after N to stop the auto-started recording.
    let record_stop_rx = match cfg.record_auto {
        Some(d) => crossbeam_channel::tick(d),
        None => crossbeam_channel::never::<Instant>(),
    };
    let mut ring = Ring::new(ring_caps);
    let mut last_save: Option<Instant> = None;
    let mut rec = RingRec::Off;
    // The newest video PTS teed to the recording — the target the audio drains to at stop.
    let mut last_video_pts: i64 = 0;

    // Auto-start the timed recording (`record` subcommand / `--record-secs`) — it begins
    // at the first IDR downstream. Honor an explicit `--out` path, else the default name.
    // `record_auto` (if set) later drives the timed auto-stop; without it the recording
    // runs until the session stops (`record` without `--seconds`).
    if cfg.record_autostart {
        let path = cfg
            .record_out
            .clone()
            .unwrap_or_else(|| record_path(&cfg.output_dir));
        if rec_ctrl_tx.send(RecordCtrl::Start(path)).is_ok() {
            rec = RingRec::On;
        }
    }

    loop {
        select! {
            recv(item_rx) -> msg => match msg {
                Ok(item) => {
                    // Tee to the recording (cheap Arc clone) BEFORE the packet moves into
                    // the ring.
                    match &rec {
                        RingRec::On => {
                            if let MuxItem::Video(pkt) = &item {
                                last_video_pts = last_video_pts.max(pkt.pts);
                            }
                            // try_send: if the mux worker falls behind the disk, stop the
                            // recording rather than stall the replay buffer.
                            if tee_record(&rec_item_tx, &item).is_err() {
                                warn!("recording can't keep up with the disk — stopping it \
                                       to protect the buffer");
                                let _ = rec_ctrl_tx.send(RecordCtrl::Stop);
                                rec = RingRec::Off;
                            }
                        }
                        RingRec::Draining { until_pts, since } => {
                            // Tee only AUDIO until it reaches the video tail (or a timeout),
                            // so the recording's audio ends with its video (§5 AV-3).
                            let caught_up = matches!(&item, MuxItem::Audio(_, p) if p.pts >= *until_pts)
                                || since.elapsed() >= RECORD_DRAIN_TIMEOUT;
                            if matches!(&item, MuxItem::Audio(..)) {
                                let _ = tee_record(&rec_item_tx, &item);
                            }
                            if caught_up {
                                let _ = rec_ctrl_tx.send(RecordCtrl::Stop);
                                info!("timed recording finalized (audio drained to the video tail)");
                                rec = RingRec::Off;
                            }
                        }
                        RingRec::Off => {}
                    }
                    match item {
                        MuxItem::Video(packet) => {
                            ring.push_video(packet);
                            consumed.fetch_add(1, Ordering::Relaxed);
                        }
                        MuxItem::Audio(track, packet) => {
                            ring.push_audio(track, packet);
                        }
                    }
                }
                Err(_) => break, // producers gone → shutdown
            },
            recv(hotkey_rx) -> ev => {
                if let Ok(e) = &ev {
                    if matches!(e.state, HotKeyState::Pressed) {
                        if e.id == cfg.save_hotkey_id {
                            if !trigger_save(
                                &mut ring, &clock, buffer_ticks, &cfg.output_dir,
                                &save_job_tx, cfg.clear_after_save, &mut last_save,
                            ) {
                                break; // mux worker gone
                            }
                        } else if e.id == cfg.record_hotkey_id {
                            rec = match rec {
                                RingRec::Off => start_recording(&cfg.output_dir, &rec_ctrl_tx),
                                RingRec::On => {
                                    info!("timed recording stopping — draining audio to the tail");
                                    RingRec::Draining {
                                        until_pts: last_video_pts,
                                        since: Instant::now(),
                                    }
                                }
                                draining => draining, // already stopping
                            };
                        }
                    }
                }
            },
            recv(autosave_rx) -> _ => {
                if !trigger_save(
                    &mut ring, &clock, buffer_ticks, &cfg.output_dir, &save_job_tx,
                    cfg.clear_after_save, &mut last_save,
                ) {
                    break;
                }
            },
            recv(record_stop_rx) -> _ => {
                if let RingRec::On = rec {
                    info!("--record-secs elapsed — draining audio, then finalizing");
                    rec = RingRec::Draining {
                        until_pts: last_video_pts,
                        since: Instant::now(),
                    };
                }
            },
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    // On session stop, finalize any recording (the mux worker also finalizes on its
    // channel disconnect, but be explicit so the file lands promptly).
    if !matches!(rec, RingRec::Off) {
        let _ = rec_ctrl_tx.send(RecordCtrl::Stop);
    }
    Ok(())
}

/// The ring thread's live-recording state (M4-3).
enum RingRec {
    /// Not recording.
    Off,
    /// Recording: tee every `MuxItem` to the mux worker.
    On,
    /// Stopping: tee only audio until it reaches `until_pts` (the last teed video PTS)
    /// or `since` exceeds the drain timeout, so the recording's audio tail matches its
    /// video tail (`§5` AV-3, within one AAC frame).
    Draining { until_pts: i64, since: Instant },
}

/// Start a timed recording (send `Start`), returning the new ring-record state.
fn start_recording(output_dir: &Path, rec_ctrl_tx: &Sender<RecordCtrl>) -> RingRec {
    let path = record_path(output_dir);
    if rec_ctrl_tx.send(RecordCtrl::Start(path)).is_ok() {
        info!("timed recording started (hotkey)");
        RingRec::On
    } else {
        RingRec::Off
    }
}

/// Tee one item to the recording (`try_send`, cheap `Arc` clone). `Err(())` if the
/// channel is full (mux worker behind the disk) or gone — the caller stops recording.
fn tee_record(rec_item_tx: &Sender<MuxItem>, item: &MuxItem) -> Result<(), ()> {
    rec_item_tx.try_send(item.clone()).map_err(|_| ())
}

/// A timed-recording destination path: `<product>_rec_<unix_ms>.mp4` under `dir`
/// (distinct prefix from the `_<ms>` clip saves).
fn record_path(dir: &Path) -> PathBuf {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{PRODUCT_NAME}_rec_{ms}.mp4"))
}

/// Run one save from the ring thread (hotkey press or the `--autosave` tick):
/// debounce, run the pure `§4` [`select_window`], and dispatch an owned window to
/// the save worker (then optionally clear the ring). Returns `false` if the
/// save-job channel has closed (the ring thread should stop).
#[allow(clippy::too_many_arguments)]
fn trigger_save(
    ring: &mut Ring,
    clock: &Clock,
    buffer_ticks: i64,
    output_dir: &Path,
    save_job_tx: &Sender<SaveJob>,
    clear_after_save: bool,
    last_save: &mut Option<Instant>,
) -> bool {
    let now = Instant::now();
    if last_save.is_some_and(|t| now.duration_since(t) < SAVE_DEBOUNCE) {
        info!("save coalesced (debounce)");
        return true;
    }
    *last_save = Some(now);
    let target = clock.now_ticks() - buffer_ticks;
    match select_window(ring, target) {
        Ok(window) => {
            if window.clamped {
                warn!(
                    "clip shorter than requested — target predates the current epoch's \
                     first IDR (§4.2)"
                );
            }
            info!(
                packets = window.packet_count(),
                origin = window.origin,
                last_pts = window.last_video_pts,
                "save triggered"
            );
            let path = buffer_clip_path(output_dir);
            if save_job_tx.send(SaveJob { window, path }).is_err() {
                return false; // save worker gone
            }
            if clear_after_save {
                ring.clear();
            }
        }
        Err(e) => warn!(error = %e, "save skipped"),
    }
    true
}

/// The mux worker: holds an output type PER EPOCH (a resolution change starts a new
/// epoch with new SPS/PPS) plus the (epoch-invariant) track ASCs, and drives the
/// reused [`Fmp4Writer`] for BOTH one-shot saves (`SaveJob`, via [`save::save_clip`]
/// with the type matching the clip's epoch — `§4.2`) AND the live timed recording
/// (M4-3: `RecordCtrl` + teed `MuxItem`s). A `select!` loop so new-epoch types/ASCs
/// from a restart are absorbed; it exits when the ring drops its channels (session
/// end), finalizing any recording in flight. Runs in the MTA (COM/MF).
#[allow(clippy::too_many_arguments)]
fn mux_worker_thread(
    num_audio: usize,
    mt_rx: Receiver<(u32, SendMediaType)>,
    asc_rx: Receiver<(usize, AudioTrackConfig)>,
    save_job_rx: Receiver<SaveJob>,
    rec_ctrl_rx: Receiver<RecordCtrl>,
    rec_item_rx: Receiver<MuxItem>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();
    // One output type per epoch seen. Kept for the whole session (a save may target
    // an older, still-retained epoch); restarts are rare so this stays tiny.
    let mut types: Vec<(u32, SendMediaType)> = Vec::new();
    let mut asc_slots: Vec<Option<AudioTrackConfig>> = (0..num_audio).map(|_| None).collect();
    let mut ready_logged = false;
    let mut rec = Rec::Idle;

    loop {
        select! {
            recv(mt_rx) -> msg => if let Ok((epoch, ty)) = msg {
                // An epoch's type is sent once; replace defensively.
                match epoch_index(types.iter().map(|(e, _)| *e), epoch) {
                    Some(i) => types[i].1 = ty,
                    None => types.push((epoch, ty)),
                }
            },
            recv(asc_rx) -> msg => if let Ok((idx, cfg)) = msg {
                if let Some(slot) = asc_slots.get_mut(idx) {
                    *slot = Some(cfg);
                }
            },
            recv(save_job_rx) -> msg => match msg {
                Ok(job) => {
                    if !ready_logged {
                        info!(tracks = num_audio, "mux worker ready");
                        ready_logged = true;
                    }
                    process_save_job(&types, &asc_slots, num_audio, job);
                }
                Err(_) => break, // ring gone → session ending
            },
            recv(rec_ctrl_rx) -> msg => match msg {
                Ok(RecordCtrl::Start(path)) => {
                    finalize_recording(&mut rec); // in case one is already running
                    rec = Rec::Pending { path, audio: Vec::new() };
                    info!("recording armed — starts at the next keyframe (M4-3)");
                }
                Ok(RecordCtrl::Stop) => finalize_recording(&mut rec),
                Err(_) => break,
            },
            recv(rec_item_rx) -> msg => match msg {
                Ok(item) => record_item(&mut rec, item, &types, &asc_slots, num_audio),
                Err(_) => break,
            },
        }
    }
    // Finalize any recording in flight so a running session-stop still yields a file.
    finalize_recording(&mut rec);
    Ok(())
}

/// Cap on the audio AUs buffered while a recording waits for its first IDR — bounds
/// memory if a keyframe is slow (≤ 1 GOP normally). ~256 AUs ≈ 5.5 s of audio/track.
const RECORD_PREBUFFER_MAX: usize = 256;

/// State of the live timed recording (M4-3).
enum Rec {
    /// Not recording.
    Idle,
    /// `RecordCtrl::Start` received; the recording begins at the next teed video IDR
    /// (so the file opens on a keyframe — no force-IDR needed, ≤ 1 GOP delay). Audio
    /// AUs are buffered meanwhile and replayed into the writer when it opens, so the
    /// writer's prebuffer aligns them to the origin IDR (`§4.4` ≤ 1-frame head).
    Pending {
        path: PathBuf,
        audio: Vec<(usize, EncodedAudioPacket)>,
    },
    /// Writing to a live [`Fmp4Writer`] opened in `epoch` (a device-loss epoch change
    /// finalizes it — `§0`, a recording must not span epochs).
    Active { writer: Fmp4Writer, epoch: u32 },
}

/// Feed one teed [`MuxItem`] to the timed recording.
fn record_item(
    rec: &mut Rec,
    item: MuxItem,
    types: &[(u32, SendMediaType)],
    asc_slots: &[Option<AudioTrackConfig>],
    num_audio: usize,
) {
    match rec {
        Rec::Pending { .. } => record_pending(rec, item, types, asc_slots, num_audio),
        Rec::Active { .. } => record_active(rec, item),
        Rec::Idle => {}
    }
}

/// Pending: buffer audio (for the writer's prebuffer) and start on the first IDR.
fn record_pending(
    rec: &mut Rec,
    item: MuxItem,
    types: &[(u32, SendMediaType)],
    asc_slots: &[Option<AudioTrackConfig>],
    num_audio: usize,
) {
    match &item {
        MuxItem::Video(pkt) if pkt.is_keyframe => {
            // Take the buffered audio + path out, then open the writer for the IDR's
            // epoch, replay the buffered audio (prebuffered by the writer), and write
            // the IDR (sets origin, admits the audio with the §4.4 head slack).
            let (path, buffered) = match std::mem::replace(rec, Rec::Idle) {
                Rec::Pending { path, audio } => (path, audio),
                other => {
                    *rec = other;
                    return;
                }
            };
            let Some(mut writer) = open_recording(&path, pkt.epoch_id, types, asc_slots, num_audio)
            else {
                warn!("recording not started — encoder type / ASCs not ready");
                return; // stays Idle
            };
            let write = buffered
                .iter()
                .try_for_each(|(track, apkt)| writer.write_audio_packet(*track, apkt))
                .and_then(|()| writer.write_video_packet(pkt));
            match write {
                Ok(()) => {
                    info!(
                        epoch = pkt.epoch_id,
                        prebuffered = buffered.len(),
                        "recording started"
                    );
                    *rec = Rec::Active {
                        writer,
                        epoch: pkt.epoch_id,
                    };
                }
                Err(e) => {
                    error!(error = %e, "recording write failed at start");
                    let _ = writer.finish();
                }
            }
        }
        MuxItem::Video(_) => {} // non-IDR before the start keyframe — drop
        MuxItem::Audio(track, pkt) => {
            if let Rec::Pending { audio, .. } = rec {
                if audio.len() < RECORD_PREBUFFER_MAX {
                    audio.push((*track, pkt.clone()));
                }
            }
        }
    }
}

/// Active: write the item; finalize on a device-loss epoch boundary or a write error.
fn record_active(rec: &mut Rec, item: MuxItem) {
    let epoch_changed = matches!((&*rec, &item),
        (Rec::Active { epoch, .. }, MuxItem::Video(pkt)) if pkt.epoch_id != *epoch);
    if epoch_changed {
        info!("recording stopped at an epoch boundary (device loss, §0)");
        finalize_recording(rec);
        return;
    }
    let write_err = if let Rec::Active { writer, .. } = rec {
        match &item {
            MuxItem::Video(pkt) => writer.write_video_packet(pkt).err(),
            MuxItem::Audio(track, pkt) => writer.write_audio_packet(*track, pkt).err(),
        }
    } else {
        None
    };
    if let Some(e) = write_err {
        error!(error = %e, "recording write failed — finalizing");
        finalize_recording(rec);
    }
}

/// Open a live recording writer for `epoch` (needs the epoch's output type and all
/// ASCs — present by the time video flows). `None` if not yet ready or on error.
fn open_recording(
    path: &Path,
    epoch: u32,
    types: &[(u32, SendMediaType)],
    asc_slots: &[Option<AudioTrackConfig>],
    num_audio: usize,
) -> Option<Fmp4Writer> {
    let idx = epoch_index(types.iter().map(|(e, _)| *e), epoch)?;
    let audio_tracks: Vec<AudioTrackConfig> = asc_slots.iter().cloned().collect::<Option<_>>()?;
    if audio_tracks.len() != num_audio {
        return None;
    }
    match Fmp4Writer::create(&types[idx].1 .0, &audio_tracks, path) {
        Ok(w) => Some(w),
        Err(e) => {
            error!(error = %e, "recording create failed");
            None
        }
    }
}

/// Finalize the recording if one is active (atomic `.part`→rename), logging outcome.
fn finalize_recording(rec: &mut Rec) {
    if let Rec::Active { writer, .. } = std::mem::replace(rec, Rec::Idle) {
        match writer.finish() {
            Ok(path) => info!(path = %path.display(), "recording finalized"),
            Err(e) => error!(error = %e, "recording finalize failed"),
        }
    }
}

/// Mux one save job with the output type matching the clip's epoch (`§4.2`) and log
/// the outcome (WARN on a slow write, `§6.3`). Skips (WARN) if the epoch's type or a
/// track's ASC isn't known yet — this should not happen, since the encoder sends its
/// type and the audio threads their ASCs before any packet of the epoch reaches the
/// ring.
fn process_save_job(
    types: &[(u32, SendMediaType)],
    asc_slots: &[Option<AudioTrackConfig>],
    num_audio: usize,
    job: SaveJob,
) {
    let epoch = job.window.epoch_id;
    let Some(idx) = epoch_index(types.iter().map(|(e, _)| *e), epoch) else {
        warn!(
            epoch,
            "save skipped — no encoder output type for the clip's epoch yet"
        );
        return;
    };
    let output_type = &types[idx].1;
    let audio_tracks: Vec<AudioTrackConfig> =
        match asc_slots.iter().cloned().collect::<Option<Vec<_>>>() {
            Some(v) if v.len() == num_audio => v,
            _ => {
                warn!("save skipped — audio track config(s) not yet known");
                return;
            }
        };

    let start = Instant::now();
    match save::save_clip(&job.window, &output_type.0, &audio_tracks, &job.path) {
        Ok(path) => {
            let ms = start.elapsed().as_millis() as i64;
            if ms > SAVE_DURATION_WARN_MS {
                warn!(path = %path.display(), ms, "clip saved (slow write — disk suspect, §6.3)");
            } else {
                info!(path = %path.display(), ms, "clip saved");
            }
        }
        Err(e) => error!(error = %e, "clip save FAILED"),
    }
}

/// Index of the entry tagged `epoch` (exact match, first occurrence) — the `§4.2`
/// per-epoch output-type selection. Pure, so it is unit-tested directly.
fn epoch_index(epochs: impl Iterator<Item = u32>, epoch: u32) -> Option<usize> {
    epochs
        .enumerate()
        .find(|(_, e)| *e == epoch)
        .map(|(i, _)| i)
}

/// Build a save destination path under `dir`. v1 filename: `<product>_<unix_ms>.mp4`
/// (the full `filename_template` token set is M10). Millisecond granularity avoids
/// collisions on rapid saves.
fn buffer_clip_path(dir: &Path) -> PathBuf {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{PRODUCT_NAME}_{ms}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `§4.2` per-epoch output-type selection: a save picks the type tagged with the
    /// clip's own epoch (exact match), so a clip from an older still-retained epoch
    /// muxes with its matching SPS/PPS, never a newer epoch's.
    #[test]
    fn epoch_index_selects_exact_epoch() {
        // The save worker accumulates types in arrival (epoch) order after restarts.
        let epochs = [0u32, 1, 2];
        assert_eq!(epoch_index(epochs.iter().copied(), 0), Some(0));
        assert_eq!(epoch_index(epochs.iter().copied(), 1), Some(1));
        assert_eq!(epoch_index(epochs.iter().copied(), 2), Some(2));
        // An epoch with no type yet (should not happen in practice) → None → skip.
        assert_eq!(epoch_index(epochs.iter().copied(), 3), None);
        assert_eq!(epoch_index([].iter().copied(), 0), None);
    }

    /// A save after a device-loss restart targets the current (newest) epoch, but an
    /// older epoch's type is still present and distinct.
    #[test]
    fn epoch_index_after_restart_keeps_old_epochs_addressable() {
        // Non-contiguous is fine (defensive), and first-occurrence wins on any dup.
        let epochs = [0u32, 2, 2];
        assert_eq!(epoch_index(epochs.iter().copied(), 2), Some(1));
        assert_eq!(epoch_index(epochs.iter().copied(), 0), Some(0));
        assert_eq!(epoch_index(epochs.iter().copied(), 1), None);
    }
}
