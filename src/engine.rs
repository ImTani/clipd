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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, select, Receiver, Sender};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::{error, info, warn};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};

use crate::audio::binding::{self, Binding, BindingTracker, GameDetect};
use crate::audio::devices::DeviceSelection;
use crate::audio::levels::{peak_rms, AudioLevels, StreamMeter};
use crate::audio::mixer::TwoSourceMixer;
use crate::audio::process_loopback::{process_loopback_supported, run_process_capture};
use crate::audio::resample::{ResampledChunk, StreamResampler};
use crate::audio::wasapi_stream::{
    run_capture, AudioPacket, AudioSource, AudioTrackKind, MicControl,
};
use crate::capture::canvas::canvas_size;
use crate::capture::convert::Converter;
use crate::capture::pacing::{PacingGrid, SlotAction};
use crate::capture::resize::{ResizeTracker, DEFAULT_SETTLE_TICKS};
use crate::capture::wgc::{
    is_window, window_monitor_size, CaptureSource, CapturedFrame, WgcCapture,
};
use crate::clock::Clock;
use crate::com::ComMta;
use crate::config::VcApp;
use crate::encode::mft_aac::{f32_to_i16, AacEncoder, EncodedAudioPacket};
use crate::encode::mft_h264::{
    EncodedPacket, EncoderConfig, EncoderOverrides, H264Encoder, InputFrame,
};
use crate::gpu::{AdapterSelection, GpuContext, GpuError};
use crate::mux::fmp4::{AudioTrackConfig, Fmp4Writer};
use crate::mux::SendMediaType;
use crate::ring::{Ring, RingCaps};
use crate::save::{self, select_window, SaveWindow};
use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::encoder::{video_peak_bitrate_bps, video_target_bitrate_bps};
use crate::spec_constants::ring::{byte_cap_bytes, est_bitrate_bps};
use crate::spec_constants::units::{ms_to_ticks, TICKS_PER_SECOND};
use crate::spec_constants::video::nominal_frame_duration_ticks;
use crate::spec_constants::watchdog::{NO_WGC_FRAME_RESTART_MS, SAVE_DURATION_WARN_MS};
use crate::spec_constants::PRODUCT_NAME;
use crate::status::{CaptureTarget, EngineStatus, SaveOutcome};
use crate::watchdog::{PipelineStats, Watchdog, WatchdogState};

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

/// A command from the shell (tray menu — `ui.rs`) to the running engine (M5). The
/// tray injects the SAME actions as the global hotkeys, over an explicit channel,
/// so the engine stays fully functional headless (the `record` subcommand and the
/// `--autosave`/`--record-secs` hooks never create a shell). Read by the ring thread
/// in its `select!` alongside the hotkey receiver.
///
/// Not `Copy`: a live-apply command (A5) may carry an owned value (e.g. a future
/// output-dir `PathBuf`), and the variants are only ever sent or matched by value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    /// Save the last N seconds (same path as the save hotkey).
    SaveClip,
    /// Start/stop the timed recording (same path as the record hotkey).
    ToggleRecord,
    /// Pause/resume buffering. Pause stops NEW footage entering the ring while
    /// keeping the existing buffer and the pipeline alive (`DECISIONS.md`
    /// 2026-07-06 "M5 plan"); the ingest gating itself lands in the pause task.
    SetPaused(bool),
    /// Live-apply the `clear-after-save` toggle from the settings editor (A5). Safe
    /// to hot-swap: it only changes what a *future* save does (whether it clears the
    /// ring), with no effect on the running pipeline, so no epoch restart is needed.
    /// The remaining editable fields (quality/resolution/fps/buffer/devices) need an
    /// epoch or encoder rebuild and are applied on restart (DECISIONS "A5"/"T2").
    SetClearAfterSave(bool),
    /// Live-apply the output folder from the settings editor (T2). The save/record path
    /// is resolved per-save from the ring thread's `output_dir`, so the folder can change
    /// with no restart. The editor sends the already-resolved + created directory.
    SetOutputDir(std::path::PathBuf),
    /// Live-apply the instant-replay length in seconds (T2b). The ring thread resizes the
    /// ring's duration + byte caps (a grow just retains more before the next eviction; a
    /// shrink evicts the now-excess GOPs at once) and the save window — no restart, since
    /// the length is a ring bound with no pipeline side effect.
    SetDurationCap(u32),
    /// Live-apply a rebound global hotkey id (T2b). The settings editor re-registers the
    /// combo on the pump thread (unregister old → register new) and, on success, sends
    /// the new `GlobalHotKeyEvent` id so the ring thread's event filter matches the new
    /// binding without a restart. `Save`/`Record` name which id to swap.
    SetSaveHotkeyId(u32),
    /// See [`EngineCommand::SetSaveHotkeyId`] — the record-toggle counterpart.
    SetRecordHotkeyId(u32),
    /// Live-apply a mic **device** swap (T2b): Default-follow ↔ pinned ↔ another pinned.
    /// The ring thread pushes the new selection into the shared [`MicControl`], and the
    /// running Mic capture thread reopens on it via the `§7` rebuild path. Mic off↔on is
    /// a topology change and stays restart-required (never sent here — DECISIONS "T2b").
    SetMicSelection(DeviceSelection),
    /// Wind the session down cleanly (the Quit menu item).
    Shutdown,
}

/// The tray's visual state (`01-PROJECT-PLAN.md §5.5`). Buffering (green), Paused
/// (amber/idle), Warning (a `§6.3` threshold crossed — divergence, slow save,
/// encoder-open retry), Error (a worker died / the session ended abnormally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// Actively buffering.
    Buffering,
    /// Paused by the user — not ingesting new footage.
    Paused,
    /// A watchdog threshold crossed; degraded but running.
    Warning,
    /// A fatal condition — the session is no longer trustworthy.
    Error,
}

/// A signal from the engine to the shell (`ui.rs`), driving the tray icon/tooltip.
/// Sent with `try_send` so a slow or absent shell never blocks an engine thread.
#[derive(Debug, Clone)]
pub enum ShellSignal {
    /// The tray state changed.
    State(TrayState),
    /// A save completed (T1): the tray raises the save-complete/-failed balloon. `seconds`
    /// is the clip length; `folder` is the clip's containing directory (opened on a
    /// success click); `reason` is the failure text (empty on success). Emitted from the
    /// save worker AFTER the write, so it never touches the save latency budget.
    Saved {
        ok: bool,
        seconds: f32,
        folder: std::path::PathBuf,
        reason: String,
    },
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
    status: Arc<EngineStatus>,
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
    // Publish the resolved capture target + output canvas for the status strip (A4).
    // `window_hwnd` is `Some` for a window source, `None` for a monitor.
    status.set_target(if window_hwnd.is_some() {
        CaptureTarget::Window
    } else {
        CaptureTarget::Monitor
    });
    status.set_resolution(canvas.0, canvas.1);
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
    // Last dropped-frame count we published, so we forward only the delta into the
    // shared session total (A4). This grid's count is monotonic within the thread;
    // a new epoch gets a fresh thread + grid starting at 0, and its deltas keep
    // accumulating into the same shared total (see `EngineStatus::add_dropped`).
    let mut published_drops: u64 = 0;

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
                status.set_target(CaptureTarget::Monitor); // A4: target changed, no epoch
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
        // Sample encoder backpressure BEFORE the (blocking) send: a full input channel
        // means the encoder is behind, so this send will stall and the keep-latest drops
        // accumulating this iteration are LATE (frames genuinely lost because the pipeline
        // can't keep up) rather than benign high-refresh pacing skips (T8). Read-only —
        // does not change the blocking-send backpressure the pipeline relies on.
        let encoder_behind = input_tx.is_full();
        // A closed receiver means the encoder stopped; end the loop.
        if input_tx.send(frame).is_err() {
            break;
        }
        captured.fetch_add(1, Ordering::Relaxed);
        // Forward any newly-dropped frames into the shared session total for the Debug
        // panel (A4/T8), split by whether the encoder was behind. A *delta*, not the
        // absolute count, so a §7 device-loss respawn (fresh grid, counters back to 0)
        // accumulates onto the prior epochs' totals instead of overwriting them.
        let drops = grid.counters().drops;
        let new_drops = drops.saturating_sub(published_drops);
        if encoder_behind {
            status.add_dropped(new_drops); // dropped (late) — encoder couldn't keep up
        } else {
            status.add_skipped(new_drops); // skipped (pacing) — expected on high-Hz panels
        }
        published_drops = drops;

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
    quality_mult: f64,
    gop_frames: u32,
    overrides: EncoderOverrides,
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
            // §6.1 amendment (DECISIONS 2026-07-07): the encoder targets a bitrate
            // (CQP is unreachable on the NVENC MFT). Same number the byte cap uses.
            target_bitrate_bps: video_target_bitrate_bps(width, height, fps, quality_mult),
            peak_bitrate_bps: video_peak_bitrate_bps(width, height, fps, quality_mult),
            overrides,
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
///
/// When `chunk_fanout` is `Some` (the Mic track under a Mix, B4/D3), each resampled
/// 48 kHz chunk is *also* forwarded to the mixer before being encoded here — the mic
/// is captured and resampled once, its output feeding both its own Mic track and the
/// Mix. The forward is a non-blocking [`Sender::try_send`] ([`forward_to_mixer`]): if
/// the mixer is behind, the chunk is dropped rather than stalling this track's capture,
/// and the mixer silence-fills that position with no drift (it places by frame index).
#[allow(clippy::too_many_arguments)]
fn audio_process_thread(
    kind: AudioTrackKind,
    track_index: usize,
    bitrate_bps: u32,
    pkt_rx: Receiver<AudioPacket>,
    asc_tx: Sender<(usize, AudioTrackConfig)>,
    item_tx: Sender<MuxItem>,
    levels: Arc<AudioLevels>,
    mut chunk_fanout: Option<Sender<ResampledChunk>>,
) -> Result<(), EngineError> {
    let mut mix_drops: u64 = 0;
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
        name: kind.title().to_string(),
    };
    if asc_tx.send((track_index, cfg)).is_err() {
        return Ok(()); // muxer gone during setup — nothing to produce
    }
    drop(asc_tx); // ASC is the only setup message; release so the mux can proceed

    // The resampler needs the device's native rate, which only arrives on the
    // first packet (`AudioPacket::sample_rate`), so it is built lazily.
    let mut resampler: Option<StreamResampler> = None;

    while let Ok(pkt) = pkt_rx.recv() {
        // Publish this stream's level for the settings-window VU meter (A3), once
        // per captured packet on the raw f32 (resampling barely moves amplitude, and
        // this needs no copy). Silence-flagged packets skip the scan. Engine → UI
        // only; a no-op when the window is closed (nobody reads).
        let meter = if pkt.silent {
            StreamMeter::default()
        } else {
            peak_rms(&pkt.samples)
        };
        levels.publish(kind, meter);

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
            forward_to_mixer(&mut chunk_fanout, &mut mix_drops, &chunk); // D3 mic → mixer
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

    // The stream stopped (capture dropped its sender: device loss / epoch rebuild /
    // shutdown). Drop the meter to silence so it decays rather than freezing at the
    // last level for the ~500 ms epoch gap — a stuck bar on a dead stream is exactly
    // the "live indicator, dead thread" lie this project exists to kill.
    levels.publish(kind, StreamMeter::default());

    // Flush the resampler delay line and the encoder tail so the track ends within
    // one AAC frame of the audio.
    if let Some(mut rs) = resampler {
        for chunk in rs.finish()? {
            forward_to_mixer(&mut chunk_fanout, &mut mix_drops, &chunk);
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
    if mix_drops > 0 {
        warn!(
            track = kind.label(),
            mix_drops,
            "resampled chunks dropped feeding the mixer under backpressure (mix silence-filled at those positions; this track unaffected)"
        );
    }
    // Dropping `chunk_fanout` here closes the mixer's mic channel → the mixer sees the
    // mic ended and plays the desktop alone through teardown.
    drop(chunk_fanout);
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

/// Forward one resampled chunk to the mixer (B4/D3 fan-out), non-blocking. If the mixer
/// is behind (channel full) the chunk is **dropped** rather than blocking — the mic
/// capture path must not stall on a slow mixer, and because the mixer places chunks by
/// absolute frame index a dropped chunk becomes silence at that position with no
/// cumulative drift; the mic's own track still encodes every chunk. A disconnected mixer
/// (its thread gone) stops the forwarding for good.
fn forward_to_mixer(
    fanout: &mut Option<Sender<ResampledChunk>>,
    drops: &mut u64,
    chunk: &ResampledChunk,
) {
    if let Some(tx) = fanout.as_ref() {
        match tx.try_send(chunk.clone()) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => *drops += 1,
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => *fanout = None,
        }
    }
}

/// How long the [`mix_process_thread`] waits for a not-yet-anchored expected source
/// (the mic) before proceeding with whoever is present. Bounds both the startup
/// latency of a correctly-mixed clip and the mixer's buffer growth (~this × 48 kHz)
/// when a mic device never opens. Well under a save's `buffer_seconds`.
const MIX_WARMUP_GRACE: Duration = Duration::from_millis(500);

/// The Mix-track (track 0) process thread (B4): resamples the desktop loopback here,
/// sums it with the mic's fanned-in resampled chunks via the pure [`TwoSourceMixer`],
/// and AAC-encodes the result. Owns the desktop resampler + the Mix AAC encoder on this
/// MTA thread (COM rule). Sends the Mix ASC eagerly *before* any data, exactly like
/// [`audio_process_thread`], so the save gate (`§4.4` / D4) is satisfied whether or not
/// the mic ever delivers a frame.
///
/// `desktop_rx` carries the desktop-endpoint capture packets; `mic_rx` is `Some` when
/// the mic is on (its resampled chunks fanned from the Mic track's thread), `None` for
/// a desktop-only mix. The thread ends when the desktop capture channel closes (epoch
/// stop) and the mic channel is closed or absent; it flushes the desktop resampler and
/// the mixer, then the AAC tail.
fn mix_process_thread(
    track_index: usize,
    bitrate_bps: u32,
    desktop_rx: Receiver<AudioPacket>,
    mic_rx: Option<Receiver<ResampledChunk>>,
    asc_tx: Sender<(usize, AudioTrackConfig)>,
    item_tx: Sender<MuxItem>,
    levels: Arc<AudioLevels>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();

    let mut encoder = AacEncoder::new(AudioTrackKind::Mix, bitrate_bps)?;
    let silent_au = match AacEncoder::silent_au(bitrate_bps) {
        Ok(au) => au,
        Err(e) => {
            warn!(error = %e, "no AAC silence template — save head-silence fill disabled for the mix track");
            Vec::new()
        }
    };
    let cfg = AudioTrackConfig {
        asc: encoder.audio_specific_config().to_vec(),
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE_HZ,
        silent_au,
        name: AudioTrackKind::Mix.title().to_string(),
    };
    if asc_tx.send((track_index, cfg)).is_err() {
        return Ok(()); // muxer gone during setup
    }
    drop(asc_tx);

    let mic_present = mic_rx.is_some();
    let mut mixer = TwoSourceMixer::new(mic_present);
    let mut desk_rs: Option<StreamResampler> = None;

    if let Some(mic_rx) = mic_rx {
        // Dual-source: select over the desktop packets, the mic chunks, and a one-shot
        // warm-up timer. A disconnected input is swapped to `never()` so its arm stops
        // firing (a disconnected receiver is otherwise always "ready" in select!).
        let mut desk = desktop_rx;
        let mut mic = mic_rx;
        let mut warm = crossbeam_channel::after(MIX_WARMUP_GRACE);
        let mut desk_open = true;
        let mut mic_open = true;
        while desk_open || mic_open {
            select! {
                recv(desk) -> msg => match msg {
                    Ok(pkt) => feed_desktop(&mut mixer, &mut desk_rs, &pkt)?,
                    Err(_) => {
                        flush_desktop(&mut mixer, &mut desk_rs)?;
                        mixer.desktop_ended();
                        desk = crossbeam_channel::never();
                        desk_open = false;
                    }
                },
                recv(mic) -> msg => match msg {
                    Ok(chunk) => mixer.push_mic(&chunk),
                    Err(_) => {
                        mixer.mic_ended();
                        mic = crossbeam_channel::never();
                        mic_open = false;
                    }
                },
                recv(warm) -> _ => {
                    mixer.release_warmup();
                    warm = crossbeam_channel::never();
                }
            }
            if !drain_mix(&mut mixer, &mut encoder, track_index, &item_tx, &levels)? {
                return Ok(()); // muxer gone
            }
        }
    } else {
        // Desktop-only mix (mic off): no alignment needed, just resample → gain/clip.
        while let Ok(pkt) = desktop_rx.recv() {
            feed_desktop(&mut mixer, &mut desk_rs, &pkt)?;
            if !drain_mix(&mut mixer, &mut encoder, track_index, &item_tx, &levels)? {
                return Ok(());
            }
        }
        flush_desktop(&mut mixer, &mut desk_rs)?;
        mixer.desktop_ended();
    }

    // Final flush: emit any tail the mixer still holds, then drop the meter to silence
    // and drain the AAC encoder.
    if !drain_mix(&mut mixer, &mut encoder, track_index, &item_tx, &levels)? {
        return Ok(());
    }
    levels.publish(AudioTrackKind::Mix, StreamMeter::default());
    for au in encoder.finish()? {
        if item_tx.send(MuxItem::Audio(track_index, au)).is_err() {
            break;
        }
    }
    Ok(())
}

/// Resample one desktop-loopback packet into the mixer, building the resampler lazily
/// (its rate is the first packet's) and switching it on a `§7` rebuild to a
/// different-rate device — mirroring [`audio_process_thread`]'s handling.
fn feed_desktop(
    mixer: &mut TwoSourceMixer,
    rs: &mut Option<StreamResampler>,
    pkt: &AudioPacket,
) -> Result<(), EngineError> {
    match rs.as_mut() {
        Some(r) if r.native_rate() != pkt.sample_rate => r.switch_native_rate(pkt.sample_rate)?,
        Some(_) => {}
        None => *rs = Some(StreamResampler::new(pkt.sample_rate)?),
    }
    let r = rs.as_mut().expect("resampler built above");
    for chunk in r.process(pkt)? {
        mixer.push_desktop(&chunk);
    }
    Ok(())
}

/// Flush the desktop resampler's delay line into the mixer at end of stream.
fn flush_desktop(
    mixer: &mut TwoSourceMixer,
    rs: &mut Option<StreamResampler>,
) -> Result<(), EngineError> {
    if let Some(r) = rs.as_mut() {
        for chunk in r.finish()? {
            mixer.push_desktop(&chunk);
        }
    }
    Ok(())
}

/// Drain every mixable chunk the mixer can currently produce, publishing the Mix VU
/// level on the mixed output and AAC-encoding each chunk. Returns `false` if the muxer
/// channel closed (the thread should stop).
fn drain_mix(
    mixer: &mut TwoSourceMixer,
    encoder: &mut AacEncoder,
    track_index: usize,
    item_tx: &Sender<MuxItem>,
    levels: &Arc<AudioLevels>,
) -> Result<bool, EngineError> {
    while let Some(mc) = mixer.drain() {
        levels.publish(AudioTrackKind::Mix, peak_rms(&mc.samples));
        if !push_aac(encoder, track_index, mc.pts, &mc.samples, item_tx)? {
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
    /// Constant quality / QP (spec §6.1). Vestigial since the T0 §6.1 amendment
    /// (CQP unreachable on the NVENC MFT); [`Self::quality_mult`] is the live knob.
    pub cq: u32,
    /// Named quality-tier multiplier over the T0-calibrated bitrate target
    /// (`config.encode.quality`, A1). Scales BOTH the encoder target and the ring
    /// byte cap so a higher tier is not evicted by a cap sized for `Default`.
    pub quality_mult: f64,
    /// Closed-GOP IDR interval in frames (spec §3).
    pub gop_frames: u32,
    /// T0 calibration-probe encoder overrides (M7-M8-PLAN §1, hidden `--encode-*`
    /// hooks). `Default` (all-`None`) in normal operation.
    pub overrides: EncoderOverrides,
    /// Capture the default-endpoint loopback (`config.audio.desktop`). Feeds the Mix
    /// track (track 0) and, in the full topology, the per-source system tracks.
    pub desktop_audio: bool,
    /// Capture the microphone as the Mic track (`§2.5`). `config.audio.mic != "off"`.
    pub mic_audio: bool,
    /// Mic endpoint policy (`§7`). Ignored when `mic_audio` is false.
    pub mic_selection: DeviceSelection,
    /// Emit the full per-source track topology (Game/VoiceChat/OtherSystem in addition
    /// to Mix+Mic) vs. the default Mix+Mic pair (`config.audio.separate_tracks`, D1).
    /// The extra tracks are *planned* by [`planned_kinds`] but not *fed* until their
    /// sources land (process-loopback B2, mixer fan-out B4); B1 spawns Mix+Mic only.
    pub separate_tracks: bool,
    /// Per-source system-track toggles under `separate_tracks` (`config.audio.tracks`).
    pub track_game: bool,
    pub track_voice_chat: bool,
    pub track_other_system: bool,
    /// Voice-chat apps the B3 detector scans for by process image name
    /// (`config.audio.vc_apps`). Consumed by the binding watcher only when the
    /// VoiceChat track is spawnable (`separate_tracks` + `track_voice_chat` + OS floor).
    pub vc_apps: Vec<VcApp>,
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

/// A save job handed from the ring thread to the save worker: an owned `§4` window plus
/// the destination. The window owns cloned (`Arc`) packets, so the ring keeps running
/// (and may `clear`) while the worker muxes. The per-app subfolder join, its
/// `create_dir_all`, and the final filename all happen in the WORKER (off the save latency
/// budget — T5); the ring thread only resolves the cheap `app_folder` (no file open) at
/// the save moment.
struct SaveJob {
    window: SaveWindow,
    /// The base clips directory (the engine's live `output_dir`).
    dir: PathBuf,
    /// The foreground app's subfolder name at save time (`""` → save straight into
    /// `dir`). Resolved cheaply on the ring thread; the folder is created in the worker.
    app_folder: String,
}

/// A running buffer session: a `supervisor` thread owning the persistent ring +
/// save worker and a rebuildable producer set (capture/encode/audio) across epochs.
/// Drive it from the main thread (wait, then [`Self::stop_and_join`]).
pub struct BufferEngine {
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
    supervisor: JoinHandle<Result<(), EngineError>>,
    /// Command channel to the ring thread (tray → engine). Cloned for the shell.
    cmd_tx: Sender<EngineCommand>,
    /// State/toast signals from the engine (engine → tray).
    signal_rx: Receiver<ShellSignal>,
    /// Lock-free audio levels the audio-process threads publish and the settings
    /// window's VU meters read (A3). Cloned into each producer set (survives epoch
    /// rebuilds) and handed to the shell. Engine → UI only.
    levels: Arc<AudioLevels>,
    /// The enabled audio streams in `§2.5` order (desktop, then mic) — the set of
    /// VU meters the settings window draws. Handed to the shell.
    audio_streams: Vec<AudioTrackKind>,
    /// Lock-free engine status the ring/capture/mux threads publish and the settings
    /// window's status strip reads (A4). Cloned into the supervisor (which fans it
    /// out to those threads, surviving epoch rebuilds) and handed to the shell.
    /// Engine → UI only.
    status: Arc<EngineStatus>,
}

/// Depth of the shell command channel (tray → ring thread). Menu clicks are rare.
const ENGINE_CMD_CHANNEL_CAP: usize = 16;
/// Depth of the shell signal channel (engine → tray). State changes are rare; a
/// small buffer absorbs a burst without ever blocking an engine thread.
const SHELL_SIGNAL_CHANNEL_CAP: usize = 16;

impl BufferEngine {
    /// Spawn the buffer pipeline. Returns immediately; capture flows into the ring
    /// and a save-hotkey press writes the last `buffer_seconds` to `output_dir`. A
    /// mid-buffer device loss triggers an epoch restart without ending the session
    /// (`§7`) — the ring survives, so a save right after the restart still finds the
    /// last pre-loss GOPs.
    pub fn start(gpu: GpuContext, params: BufferParams) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = PipelineStats::new();
        let (cmd_tx, cmd_rx) = bounded::<EngineCommand>(ENGINE_CMD_CHANNEL_CAP);
        let (signal_tx, signal_rx) = bounded::<ShellSignal>(SHELL_SIGNAL_CHANNEL_CAP);
        // The spawnable audio tracks (Mix first … Mic last), the same set (and order)
        // `buffer_supervisor` builds its capture list from — so the UI's meter list can
        // never drift from what is actually captured. Levels are keyed by track *kind*,
        // so the order matters only for the meter list.
        let audio_streams = spawnable_kinds(&params);
        // Created on the main thread (before the supervisor spawns) so the shell can
        // clone it synchronously right after `start` returns. Shared with every
        // producer set the supervisor spawns, so it survives epoch rebuilds.
        let levels = Arc::new(AudioLevels::new());
        // Engine status (A4): the immutable header is known here (adapter from the
        // GPU, fps + buffer seconds from params); the live cells are published later
        // by the ring/capture/mux threads. Built before the supervisor moves `gpu`.
        let status = Arc::new(EngineStatus::new(
            gpu.adapter_description.clone(),
            params.fps,
            params.buffer_seconds,
        ));
        let supervisor = {
            let stop = stop.clone();
            let stats = stats.clone();
            let levels = levels.clone();
            let status = status.clone();
            spawn("supervisor", move || {
                buffer_supervisor(gpu, params, stop, stats, cmd_rx, signal_tx, levels, status)
            })
        };
        Self {
            stop,
            stats,
            supervisor,
            cmd_tx,
            signal_rx,
            levels,
            audio_streams,
            status,
        }
    }

    /// A cloneable sender the shell uses to inject [`EngineCommand`]s (tray menu).
    pub fn command_sender(&self) -> Sender<EngineCommand> {
        self.cmd_tx.clone()
    }

    /// A clone of the lock-free audio levels for the shell's settings-window VU
    /// meters (A3). Reading them never touches the engine (satellite law).
    pub fn audio_levels(&self) -> Arc<AudioLevels> {
        self.levels.clone()
    }

    /// The enabled audio streams (in `§2.5` order) the settings window should draw
    /// a meter for.
    pub fn audio_streams(&self) -> Vec<AudioTrackKind> {
        self.audio_streams.clone()
    }

    /// A clone of the lock-free engine status for the shell's settings-window status
    /// strip (A4). Reading it never touches the engine (satellite law).
    pub fn status(&self) -> Arc<EngineStatus> {
        self.status.clone()
    }

    /// The receiver of [`ShellSignal`]s (tray-state updates) for the shell to poll.
    pub fn signals(&self) -> &Receiver<ShellSignal> {
        &self.signal_rx
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
/// How often the ring thread polls the `§6.3` divergence flag to drive the tray
/// WARNING/OK state (M5). 500 ms is well under the 2 s divergence window.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The audio-topology inputs that decide the track set — the pure subset of
/// [`BufferParams`] the builder reads. Kept tiny and `Copy` so [`planned_kinds`] is
/// unit-testable over every combination without a full engine fixture.
#[derive(Debug, Clone, Copy)]
struct TrackModel {
    desktop: bool,
    mic: bool,
    separate_tracks: bool,
    game: bool,
    voice_chat: bool,
    other_system: bool,
}

impl TrackModel {
    fn from_params(p: &BufferParams) -> Self {
        Self {
            desktop: p.desktop_audio,
            mic: p.mic_audio,
            separate_tracks: p.separate_tracks,
            game: p.track_game,
            voice_chat: p.track_voice_chat,
            other_system: p.track_other_system,
        }
    }
}

/// The **full planned** track set, in the amended `§2.5` container order (Mix first,
/// then the per-source system tracks under `separate_tracks`, then Mic last). Pure and
/// exhaustively unit-tested. This is the end-state 5-track model; the runtime spawns a
/// subset of it until every source lands (see [`spawnable_streams`]).
///
/// All system tracks require desktop-audio capture (`desktop`); with desktop off there
/// is no system-audio source at all, so only Mic can appear — preserving the Slice-A
/// "desktop off, mic on → single mic track" behaviour.
fn planned_kinds(m: TrackModel) -> Vec<AudioTrackKind> {
    let mut kinds = Vec::new();
    if m.desktop {
        kinds.push(AudioTrackKind::Mix); // always index 0 when present
        if m.separate_tracks {
            if m.game {
                kinds.push(AudioTrackKind::Game);
            }
            if m.voice_chat {
                kinds.push(AudioTrackKind::VoiceChat);
            }
            if m.other_system {
                kinds.push(AudioTrackKind::OtherSystem);
            }
        }
    }
    if m.mic {
        kinds.push(AudioTrackKind::Mic); // always last when present
    }
    kinds
}

/// A live-bound per-app system-track role (Slice B / B3). The two tracks whose
/// capture PID is *discovered at runtime* by [`crate::audio::binding`]: [`Self::Game`]
/// (foreground-fullscreen in monitor mode / the captured window's process in window
/// mode) and [`Self::VoiceChat`] (the `vc_apps` process scan). `OtherSystem` is NOT a
/// bound role — it *consumes* the game binding (excluding it) rather than *including* a
/// PID's tree, so it runs on its own [`run_other_system_capture`] loop reading the
/// watcher's game publication (decision D5), not through [`run_bound_capture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundRole {
    Game,
    VoiceChat,
}

impl BoundRole {
    fn label(self) -> &'static str {
        match self {
            BoundRole::Game => "game",
            BoundRole::VoiceChat => "voice-chat",
        }
    }
}

/// How a spawnable track is fed — the Slice-B "sources ≠ tracks" split at the wiring
/// layer. A [`Self::Static`] track binds one endpoint [`AudioSource`] for its whole
/// life (Mic = the capture endpoint). A [`Self::Bound`] track's PID is rediscovered
/// live by the binding watcher and fed through [`run_bound_capture`] → B2's
/// [`run_process_capture`]. [`Self::Mix`] (track 0) is the B4 software mixer:
/// desktop-loopback + (when `mic_present`) the mic, summed with headroom + soft clip —
/// the mic's resampled chunks are *fanned* from the Mic track's thread, so nothing is
/// captured or resampled twice (decision D3). [`Self::OtherSystem`] (track 4) is the
/// "everything but the game" track: it reads the *same* live game binding the watcher
/// publishes for the Game track and captures the default-endpoint loopback with that
/// game tree **excluded** — or, when no game is bound, the plain endpoint loopback. The
/// endpoint↔exclude switch is a within-epoch source swap (decision D5), not an epoch.
#[derive(Debug, Clone)]
enum TrackFeed {
    Static(AudioSource),
    Bound(BoundRole),
    Mix { mic_present: bool },
    OtherSystem,
}

/// Whether `kind` has a runnable *bound* capture source and the OS supports process
/// loopback. Mix + Mic are always spawnable; Game + VoiceChat became spawnable in B3
/// (bound sources) but only above the Win10-2004 process-loopback floor
/// ([`process_loopback_supported`]). `supported` is passed in so the pure track-set
/// shape stays testable without an OS probe.
fn spawnable_feed(kind: AudioTrackKind, supported: bool) -> Option<BoundRole> {
    match kind {
        AudioTrackKind::Game if supported => Some(BoundRole::Game),
        AudioTrackKind::VoiceChat if supported => Some(BoundRole::VoiceChat),
        _ => None,
    }
}

/// The [`TrackFeed`] for `kind`, or `None` for a not-yet-spawnable track (a per-app
/// track — Game/VoiceChat/OtherSystem — below the OS floor). `mic` is `Some(selection)`
/// when the mic is on (feeding both the Mic track and the Mix), `None` when it is off.
/// Mix is the B4 software mixer (`mic_present` = `mic.is_some()`); OtherSystem needs the
/// process-loopback floor because its exclude-tree source (when a game is bound) is
/// process loopback. Pure given `supported`.
fn track_feed(
    kind: AudioTrackKind,
    mic: Option<&DeviceSelection>,
    supported: bool,
) -> Option<TrackFeed> {
    match kind {
        AudioTrackKind::Mix => Some(TrackFeed::Mix {
            mic_present: mic.is_some(),
        }),
        AudioTrackKind::Mic => {
            mic.map(|sel| TrackFeed::Static(AudioSource::MicEndpoint(sel.clone())))
        }
        AudioTrackKind::Game | AudioTrackKind::VoiceChat => {
            spawnable_feed(kind, supported).map(TrackFeed::Bound)
        }
        // OtherSystem: endpoint loopback (no game) ↔ process-exclude-tree(game). The
        // exclude branch is process loopback, so it needs the same OS floor as the
        // per-app tracks; below it the track is hidden.
        AudioTrackKind::OtherSystem => supported.then_some(TrackFeed::OtherSystem),
    }
}

/// The [`AudioSource`] the OtherSystem track captures given the current live game
/// binding: no game bound → the plain default-endpoint loopback; a game bound → that
/// game's process tree **excluded** (`include_tree = false`) so the track carries all
/// system audio *except* the game (decision D5). Pure — the impure switch lives in
/// [`run_other_system_capture`].
fn other_system_source(game: Option<Binding>) -> AudioSource {
    match game {
        Some(b) => AudioSource::ProcessLoopback {
            pid: b.pid,
            include_tree: false,
        },
        None => AudioSource::EndpointLoopback,
    }
}

/// The audio tracks the runtime can actually spawn — [`planned_kinds`] filtered to
/// those with a live feed, each paired with its [`TrackFeed`], in container order.
/// Pure given `supported` (no logging — it is called on two threads; see
/// [`warn_deferred_tracks`]). Single source of truth for BOTH the supervisor's capture
/// list and the shell's VU-meter set (via [`spawnable_kinds`]), so the two never drift.
fn spawnable_streams_with(
    params: &BufferParams,
    supported: bool,
) -> Vec<(AudioTrackKind, TrackFeed)> {
    let mic = params.mic_audio.then_some(&params.mic_selection);
    planned_kinds(TrackModel::from_params(params))
        .into_iter()
        .filter_map(|kind| track_feed(kind, mic, supported).map(|f| (kind, f)))
        .collect()
}

/// [`spawnable_streams_with`] using the live OS process-loopback support probe. The
/// impure entry point the supervisor/shell call.
fn spawnable_streams(params: &BufferParams) -> Vec<(AudioTrackKind, TrackFeed)> {
    spawnable_streams_with(params, process_loopback_supported())
}

/// Log each planned-but-not-spawnable track, so the deferral is visible in the log.
/// Call once per session start (the supervisor). The three per-app tracks
/// (Game/VoiceChat/OtherSystem) are deferred only below the Win10-2004 process-loopback
/// floor; above it all five spawn.
fn warn_deferred_tracks(params: &BufferParams) {
    let supported = process_loopback_supported();
    let mic = params.mic_audio.then_some(&params.mic_selection);
    for kind in planned_kinds(TrackModel::from_params(params)) {
        if track_feed(kind, mic, supported).is_none() {
            // Exhaustive on purpose (no wildcard): a future deferred variant must state its
            // own reason in the log, not inherit a misleading one (CLAUDE.md trust model).
            let reason = match kind {
                AudioTrackKind::Game | AudioTrackKind::VoiceChat | AudioTrackKind::OtherSystem => {
                    "process loopback unsupported on this Windows build (< 2004) — per-app track hidden"
                }
                // Mix/Mic are always spawnable, so they never reach this deferred branch.
                AudioTrackKind::Mix | AudioTrackKind::Mic => continue,
            };
            warn!(
                track = kind.label(),
                reason, "audio track planned but not captured"
            );
        }
    }
}

/// The kinds of the spawnable track set (the VU-meter set), in container order. Kept in
/// lock-step with [`spawnable_streams`] by deriving from it.
fn spawnable_kinds(params: &BufferParams) -> Vec<AudioTrackKind> {
    spawnable_streams(params)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

// ── Live game/VC binding (B3) ─────────────────────────────────────────────────
//
// The per-app tracks (Game, VoiceChat) are captured from a PID that is discovered at
// runtime and can change mid-session (a game alt-tabbed to another fullscreen title, a
// Discord opened after the buffer started). One `binding_watcher_thread` per epoch owns
// the detection (`audio::binding`, all pure over an injected OS snapshot) and publishes
// each role's current target into a shared [`BindingState`]; each role's
// [`run_bound_capture`] loop reads its target and runs B2's `run_process_capture` on it,
// tearing the run down and re-reading whenever the watcher retargets. The gap between a
// teardown and the next bind is silence-filled downstream by the `§2.3` synthesizer, so
// no explicit gap plumbing is needed (`SLICE-B-PLAN §3`).

/// How often the binding watcher re-enumerates processes / re-reads the foreground
/// window. Detection latency, not a hot path — a game/VC appearing is noticed within
/// this window. Cheap (~1 ms Toolhelp scan) so it can be frequent without cost.
const BINDING_SCAN_INTERVAL: Duration = Duration::from_millis(600);
/// How often the watcher and the idle bound-capture loops wake to check their stop flag
/// — bounds teardown latency (well under the `§7` 500 ms epoch budget) independently of
/// the coarser scan cadence.
const BINDING_STOP_POLL: Duration = Duration::from_millis(120);
/// Minimum spacing between successive `run_process_capture` attempts on the same target
/// — bounds any spin to a few Hz if a live-but-unbindable PID keeps failing activation
/// (a dead PID is filtered out by the watcher's liveness check, so this is a backstop).
const BOUND_RETRY_BACKOFF: Duration = Duration::from_millis(300);

/// One bound role's shared, watcher-published capture target plus a handle to the
/// stop flag of the run currently consuming it (so the watcher can interrupt a live
/// run on retarget or teardown). Poison is treated as the inner value (a lock holder
/// only ever does an infallible store/replace, so a poisoned lock still holds valid
/// data), matching [`crate::audio::process_loopback`].
struct RoleSlot {
    /// `(desired target, generation)`. `generation` bumps on every retarget so a
    /// bound-capture loop can tell "unchanged" from "rebound to the same-looking PID".
    target: Mutex<(Option<Binding>, u64)>,
    /// The stop flag of the in-flight `run_process_capture` for this role, if any. The
    /// watcher sets it to end that run so the loop re-reads the new target.
    run_stop: Mutex<Option<Arc<AtomicBool>>>,
}

impl RoleSlot {
    fn new() -> Self {
        Self {
            target: Mutex::new((None, 0)),
            run_stop: Mutex::new(None),
        }
    }

    fn read_target(&self) -> (Option<Binding>, u64) {
        *self.target.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn generation(&self) -> u64 {
        self.target.lock().unwrap_or_else(|p| p.into_inner()).1
    }

    /// Publish a new target (watcher side) and interrupt any in-flight run so it re-reads.
    fn retarget(&self, desired: Option<Binding>, generation: u64) {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = (desired, generation);
        self.interrupt();
    }

    /// End the in-flight run for this role, if any (watcher side; retarget + teardown).
    fn interrupt(&self) {
        if let Some(rs) = self
            .run_stop
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            rs.store(true, Ordering::Relaxed);
        }
    }

    /// Register (loop side) the stop flag of the run about to start, or clear it when the
    /// run ends. Returns whether the current generation still matches `gen` (the caller
    /// arms `stop = false` before this, so a racing retarget that happened in between is
    /// caught here and the caller re-reads instead of running a stale target).
    fn arm_run(&self, stop: Option<Arc<AtomicBool>>) {
        *self.run_stop.lock().unwrap_or_else(|p| p.into_inner()) = stop;
    }
}

/// The shared binding state for this epoch's live-bound roles. `game`/`voice_chat` feed
/// the two [`BoundRole`] tracks (include-tree). `other_system` mirrors the *game*
/// binding for the OtherSystem track, which reads it as an **exclude** target (decision
/// D5) — a separate slot so its own [`run_other_system_capture`] run-stop never clobbers
/// the Game track's (both may consume the game binding at once).
struct BindingState {
    game: RoleSlot,
    voice_chat: RoleSlot,
    other_system: RoleSlot,
}

impl BindingState {
    fn new() -> Self {
        Self {
            game: RoleSlot::new(),
            voice_chat: RoleSlot::new(),
            other_system: RoleSlot::new(),
        }
    }

    fn slot(&self, role: BoundRole) -> &RoleSlot {
        match role {
            BoundRole::Game => &self.game,
            BoundRole::VoiceChat => &self.voice_chat,
        }
    }
}

/// Derive the game-detection mode from the video capture source. Monitor capture →
/// bind whatever is foreground-fullscreen (live). Window capture → bind the captured
/// window's process (fixed for the session); an unresolvable HWND → no game track.
/// `FocusedWindow` resolves the current foreground window's PID at spawn (that is the
/// window about to be captured).
fn game_detect_for(source: &CaptureSource) -> GameDetect {
    match source {
        CaptureSource::PrimaryMonitor | CaptureSource::Monitor(_) => {
            GameDetect::ForegroundFullscreen
        }
        CaptureSource::Window(hwnd) => binding::window_pid(*hwnd)
            .map(GameDetect::Window)
            .unwrap_or(GameDetect::Off),
        CaptureSource::FocusedWindow => binding::foreground_window()
            .map(|f| GameDetect::Window(f.pid))
            .unwrap_or(GameDetect::ForegroundFullscreen),
    }
}

/// Sleep up to `total`, waking every [`BINDING_STOP_POLL`] to check `stop`. Returns
/// early (as soon as the current poll chunk elapses) when `stop` is set.
fn sleep_until_stop(stop: &AtomicBool, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let chunk = BINDING_STOP_POLL.min(total - slept);
        std::thread::sleep(chunk);
        slept += chunk;
    }
}

/// The binding watcher: one per epoch (spawned only if ≥1 bound role). Scans the OS on
/// [`BINDING_SCAN_INTERVAL`], computes each spawned role's desired target with the pure
/// `audio::binding` selectors, and publishes retargets into `state`. Panic-free (no
/// unwrap that can fail, no indexing) so it never dies mid-session and leaves a bound
/// capture blocked — its liveness is the teardown guarantee for the bound captures. On
/// `stop` it interrupts every bound role's in-flight run so they unblock promptly.
fn binding_watcher_thread(
    state: Arc<BindingState>,
    roles: Vec<BoundRole>,
    other_system: bool,
    vc_apps: Vec<VcApp>,
    game_detect: GameDetect,
    stop: Arc<AtomicBool>,
) -> Result<(), EngineError> {
    let game_track_on = roles.contains(&BoundRole::Game);
    let vc_on = roles.contains(&BoundRole::VoiceChat);
    // Game detection runs when *either* the Game track (include-tree) or the OtherSystem
    // track (exclude-tree, decision D5) needs the game PID — so `track_game = off` +
    // `track_other_system = on` still excludes the game from OtherSystem.
    let game_needed = game_track_on || other_system;
    let mut game = BindingTracker::new();
    // Sticky game binding + new-candidate edge-debounce (F8): the game track is the last
    // foreground-fullscreen game HELD WHILE ALIVE, not "whatever is fullscreen right now" —
    // so alt-tabbing to the settings window (or any non-fullscreen app) no longer unbinds it
    // and churns the process-loopback tracks. Voice chat keeps its own (process-scan) model.
    let mut game_sticky = binding::GameStickiness::new();
    let mut vc = BindingTracker::new();

    info!(
        game = game_track_on,
        voice_chat = vc_on,
        other_system,
        ?game_detect,
        vc_apps = vc_apps.iter().filter(|a| a.enabled).count(),
        "binding watcher started (B3)"
    );

    while !stop.load(Ordering::Relaxed) {
        let procs = binding::enumerate_processes();

        if game_needed {
            // Foreground-fullscreen (monitor) or the fixed captured PID (window). The sticky
            // policy (F8) then decides the DESIRED binding: it holds a live bound game across
            // a foreground change, edge-debounces a new fullscreen candidate, and — as the
            // unbind-of-last-resort — clears a dead bound PID immediately (the liveness check
            // it runs internally over the same process snapshot).
            let raw = binding::classify_game(game_detect, binding::foreground_window());
            let desired = game_sticky.decide(game.current(), raw, |pid| {
                procs.iter().any(|p| p.pid == pid)
            });
            if let Some(r) = game.update(desired) {
                binding::log_retarget(BoundRole::Game.label(), &r);
                // Publish to whichever consumers exist: the Game track (include-tree)
                // and/or OtherSystem (which will exclude this same PID). Same generation
                // — each consumer only compares against its own slot's last-read value.
                if game_track_on {
                    state.game.retarget(desired, r.generation);
                }
                if other_system {
                    state.other_system.retarget(desired, r.generation);
                }
            }
        }

        if vc_on {
            let desired = binding::select_vc_pid(&procs, &vc_apps);
            if let Some(r) = vc.update(desired) {
                binding::log_retarget(BoundRole::VoiceChat.label(), &r);
                state.voice_chat.retarget(desired, r.generation);
            }
        }

        sleep_until_stop(&stop, BINDING_SCAN_INTERVAL);
    }

    // Teardown: unblock every bound capture's in-flight run (the bound roles + the
    // OtherSystem run, which is armed on `state.other_system`).
    for role in &roles {
        state.slot(*role).interrupt();
    }
    if other_system {
        state.other_system.interrupt();
    }
    info!(
        game_retargets = game.retargets(),
        vc_retargets = vc.retargets(),
        "binding watcher stopped"
    );
    Ok(())
}

/// The capture side of a bound (Game/VoiceChat) track: run B2's `run_process_capture`
/// on the role's watcher-published PID, re-reading and rebinding whenever the watcher
/// retargets (a new PID, or the app going away). Emits the same [`AudioPacket`]/`stop`
/// contract as [`run_capture`], so the downstream `audio_process_thread` is identical.
///
/// Teardown: the watcher sets this run's `stop` flag on `cap_stop` (it polls the same
/// epoch stop). The generation re-check closes the race where the watcher retargets
/// between reading the target and arming the run.
fn run_bound_capture(
    kind: AudioTrackKind,
    role: BoundRole,
    state: Arc<BindingState>,
    tx: Sender<AudioPacket>,
    cap_stop: Arc<AtomicBool>,
) -> Result<(), EngineError> {
    while !cap_stop.load(Ordering::Relaxed) {
        let (target, gen) = state.slot(role).read_target();
        let Some(binding) = target else {
            // No app bound yet — idle until the watcher finds one (it interrupts via the
            // run_stop only while a run is active, so poll the target here).
            sleep_until_stop(&cap_stop, BINDING_SCAN_INTERVAL);
            continue;
        };

        // Arm a fresh stop for this run and register it so the watcher can interrupt it
        // on retarget/teardown; then re-check BOTH the generation (retarget) and cap_stop
        // (teardown) to catch a signal that landed between the read above and the arm.
        //
        // The cap_stop recheck closes a teardown TOCTOU: the watcher's teardown is a
        // one-shot interrupt sweep, so a run_stop armed *after* that sweep (which saw
        // `None`) would otherwise never be set and `run_process_capture` would block
        // forever, hanging the epoch-restart join. This recheck is sufficient: the sweep
        // happens only once cap_stop is set, so if our arm precedes the sweep the sweep
        // sees it, and if it follows the sweep this load observes cap_stop = true. Either
        // way the run never starts unkillable.
        let run_stop = Arc::new(AtomicBool::new(false));
        state.slot(role).arm_run(Some(run_stop.clone()));
        if cap_stop.load(Ordering::Relaxed) || state.slot(role).generation() != gen {
            state.slot(role).arm_run(None);
            continue; // torn down or retargeted mid-arm — re-evaluate the outer loop
        }

        let r = run_process_capture(
            kind,
            binding.pid,
            binding.include_tree,
            tx.clone(),
            run_stop,
        );
        state.slot(role).arm_run(None);
        if let Err(e) = r {
            // run_process_capture only errors if the master clock fails; treat like the
            // endpoint path and end the track (never surface — the track goes silent).
            warn!(track = kind.label(), pid = binding.pid, error = %e, "bound capture ended in error — track silent");
            return Ok(());
        }
        // The run ended (retarget / process exit / teardown). Space out retries so a
        // live-but-unbindable PID can't spin.
        sleep_until_stop(&cap_stop, BOUND_RETRY_BACKOFF);
    }
    Ok(())
}

/// The capture side of the OtherSystem track (decision D5): capture the default-endpoint
/// loopback with the bound game tree **excluded** — or, when no game is bound, the plain
/// endpoint loopback. It reads the *same* game binding the watcher publishes for the Game
/// track (via `state.other_system`) and switches source whenever that binding changes: a
/// game appearing swaps endpoint → process-exclude, a game leaving swaps back. Each swap
/// is a within-epoch source change (a fresh `run_capture` on the same QPC master domain,
/// so PTS stays absolute/monotonic); the gap between the two runs is silence-filled by the
/// `§2.3` synthesizer downstream — no epoch bump, no ring/encoder restart.
///
/// Teardown mirrors [`run_bound_capture`]: arm this run's stop on `state.other_system` so
/// the watcher can interrupt it, then re-check `cap_stop` + the generation to close the
/// same teardown/retarget TOCTOU. The watcher (spawned whenever OtherSystem is present)
/// interrupts `state.other_system` on teardown, so a run armed after its sweep still sees
/// `cap_stop` here and never starts unkillable.
fn run_other_system_capture(
    state: Arc<BindingState>,
    tx: Sender<AudioPacket>,
    cap_stop: Arc<AtomicBool>,
) -> Result<(), EngineError> {
    let kind = AudioTrackKind::OtherSystem;
    while !cap_stop.load(Ordering::Relaxed) {
        let (game, gen) = state.other_system.read_target();
        let source = other_system_source(game);

        let run_stop = Arc::new(AtomicBool::new(false));
        state.other_system.arm_run(Some(run_stop.clone()));
        if cap_stop.load(Ordering::Relaxed) || state.other_system.generation() != gen {
            state.other_system.arm_run(None);
            continue; // torn down or retargeted mid-arm — re-evaluate the outer loop
        }

        match &game {
            Some(b) => info!(
                track = kind.label(),
                excluded_game_pid = b.pid,
                "other-system: capturing all system audio EXCEPT the bound game tree (D5)"
            ),
            None => info!(
                track = kind.label(),
                "other-system: capturing the full default-endpoint loopback (no game bound, D5)"
            ),
        }

        let r = run_capture(kind, source, None, tx.clone(), run_stop);
        state.other_system.arm_run(None);
        if let Err(e) = r {
            // run_capture only errors if the master clock fails; treat like the endpoint
            // path and end the track (never surface — the track goes silent).
            warn!(track = kind.label(), error = %e, "other-system capture ended in error — track silent");
            return Ok(());
        }
        // The run ended (source switch / game exit / teardown). Space out retries so a
        // repeatedly-failing exclude activation (e.g. a game exiting mid-run before the
        // watcher clears the target) can't spin.
        sleep_until_stop(&cap_stop, BOUND_RETRY_BACKOFF);
    }
    Ok(())
}

/// The buffer supervisor: owns the persistent ring + save worker and an epoch loop
/// that (re)spawns the producer set. The ring/save channels' tx ends are retained
/// here so a producer set exiting (device loss) does not disconnect and tear down the
/// core; each producer set gets fresh clones. See the module comment above.
#[allow(clippy::too_many_arguments)]
fn buffer_supervisor(
    gpu: GpuContext,
    params: BufferParams,
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
    cmd_rx: Receiver<EngineCommand>,
    signal_tx: Sender<ShellSignal>,
    levels: Arc<AudioLevels>,
    status: Arc<EngineStatus>,
) -> Result<(), EngineError> {
    // The spawnable audio tracks (Mix first … Mic last), each paired with the
    // `TrackFeed` that feeds it (a static endpoint source, a live-bound Game/VoiceChat
    // role (B3), or the OtherSystem endpoint↔exclude switch (D5)). Same set the shell
    // draws meters for (`spawnable_kinds`). Planned-but-not-spawnable tracks (the three
    // per-app tracks below the Win10-2004 process-loopback floor) are logged + dropped.
    let audio_streams: Vec<(AudioTrackKind, TrackFeed)> = spawnable_streams(&params);
    warn_deferred_tracks(&params); // once per start — the deferral log (see the fn doc)
    let num_audio = audio_streams.len();

    // The live mic-device control (T2b): shared by the ring thread (which pushes a new
    // selection when the editor commits a mic device swap) and each epoch's Mic capture
    // thread (which reopens on it via §7). `Some` only when a Mic track exists; a mic
    // off↔on change is a topology change and takes a restart instead (DECISIONS "T2b").
    let mic_control: Option<Arc<MicControl>> = params
        .mic_audio
        .then(|| Arc::new(MicControl::new(params.mic_selection.clone())));

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
    let est = est_bitrate_bps(1920, 1080, params.fps, params.quality_mult);
    let ring_caps = RingCaps {
        max_duration_ticks: retained_seconds as i64 * TICKS_PER_SECOND,
        max_bytes: byte_cap_bytes(retained_seconds, est),
        num_audio_tracks: num_audio,
    };

    // Persistent core: ring thread + mux worker (spawned once; survive restarts).
    let ring = {
        let signal_tx = signal_tx.clone();
        let stop = stop.clone();
        let stats = stats.clone();
        let cfg = RingThreadConfig {
            buffer_seconds: params.buffer_seconds,
            gop_seconds,
            est_bitrate_bps: est,
            clear_after_save: params.clear_after_save,
            output_dir: params.output_dir.clone(),
            save_hotkey_id: params.save_hotkey_id,
            record_hotkey_id: params.record_hotkey_id,
            mic_control: mic_control.clone(),
            autosave: params.autosave,
            record_auto: params.record_auto,
            record_out: params.record_out.clone(),
            record_autostart: params.record_autostart,
        };
        let status = status.clone();
        spawn("ring", move || {
            ring_thread(
                ring_caps,
                cfg,
                item_rx,
                save_job_tx,
                rec_ctrl_tx,
                rec_item_tx,
                cmd_rx,
                signal_tx,
                stats,
                stop,
                status,
            )
        })
    };
    let save = {
        let status = status.clone();
        let signal_tx = signal_tx.clone();
        spawn("save", move || {
            mux_worker_thread(
                num_audio,
                mt_rx,
                asc_rx,
                save_job_rx,
                rec_ctrl_rx,
                rec_item_rx,
                status,
                signal_tx,
            )
        })
    };

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
            mic_control.clone(),
            item_tx.clone(),
            mt_tx.clone(),
            asc_tx.clone(),
            &stats,
            &levels,
            &status,
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

    // A fatal session end (not a clean stop) → publish Error for the status strip, so
    // the settings window reflects the same failure the tray shows via
    // `any_worker_finished`. A clean stop leaves the last live state as-is.
    if outcome.is_err() || ring_res.is_err() || save_res.is_err() {
        status.set_state(TrayState::Error);
    }

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
    audio_streams: &[(AudioTrackKind, TrackFeed)],
    mic_control: Option<Arc<MicControl>>,
    item_tx: Sender<MuxItem>,
    mt_tx: Sender<(u32, SendMediaType)>,
    asc_tx: Sender<(usize, AudioTrackConfig)>,
    stats: &PipelineStats,
    levels: &Arc<AudioLevels>,
    status: &Arc<EngineStatus>,
) -> ProducerSet {
    let epoch_stop = Arc::new(AtomicBool::new(false));

    let (size_tx, size_rx) = bounded::<(u32, u32)>(1);
    let (input_tx, input_rx) = bounded::<InputFrame>(INPUT_CHANNEL_CAP);

    let capture = {
        let gpu = gpu.clone();
        let stop = epoch_stop.clone();
        let captured = stats.captured.clone();
        let status = status.clone();
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
                status,
            )
        })
    };
    let encode = {
        let gpu = gpu.clone();
        let encoded = stats.encoded.clone();
        let (fps, cq, gop) = (params.fps, params.cq, params.gop_frames);
        let quality_mult = params.quality_mult;
        let overrides = params.overrides;
        let item_tx = item_tx.clone();
        spawn("encode", move || {
            encode_thread(
                gpu,
                epoch,
                fps,
                cq,
                quality_mult,
                gop,
                overrides,
                size_rx,
                input_rx,
                mt_tx,
                item_tx,
                encoded,
            )
        })
    };

    // Shared live-binding state for the Game/VoiceChat roles (B3) + OtherSystem (D5), and
    // the roles actually spawned this epoch — the watcher is spawned if ≥1 bound role OR
    // OtherSystem is present (which needs the live game PID to exclude it).
    let binding_state = Arc::new(BindingState::new());
    let mut bound_roles: Vec<BoundRole> = Vec::new();
    let mut other_system_present = false;

    // The mic → mixer fan-out (B4/D3): created when both a Mix and a Mic track spawn, so
    // the mic is captured and resampled ONCE and its chunks feed both its own Mic track
    // and the mix — no second WASAPI client, one drift domain.
    let mix_present = audio_streams.iter().any(|(k, _)| *k == AudioTrackKind::Mix);
    let mic_present = audio_streams.iter().any(|(k, _)| *k == AudioTrackKind::Mic);
    let (mut mic_mix_tx, mut mic_mix_rx) = if mix_present && mic_present {
        let (t, r) = bounded::<ResampledChunk>(AUDIO_PACKET_CHANNEL_CAP);
        (Some(t), Some(r))
    } else {
        (None, None)
    };

    let mut audio: Vec<JoinHandle<Result<(), EngineError>>> = Vec::new();
    for (track_index, (kind, feed)) in audio_streams.iter().cloned().enumerate() {
        let cap_stop = epoch_stop.clone();
        let asc_tx = asc_tx.clone();
        let item_tx = item_tx.clone();
        let bitrate = params.audio_bitrate_bps;
        let levels = levels.clone();
        match feed {
            // Track 0 mix (B4): a desktop-loopback capture feeds the mix thread, which
            // sums it with the mic's fanned resampled chunks (`mic_present`).
            TrackFeed::Mix { mic_present } => {
                let (desk_tx, desk_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
                audio.push(spawn("audio-capture", move || {
                    Ok(run_capture(
                        AudioTrackKind::Mix,
                        AudioSource::EndpointLoopback,
                        None, // Mix follows the default render endpoint; never user-swapped
                        desk_tx,
                        cap_stop,
                    )?)
                }));
                let mic_rx = if mic_present { mic_mix_rx.take() } else { None };
                audio.push(spawn("audio-mix", move || {
                    mix_process_thread(
                        track_index,
                        bitrate,
                        desk_rx,
                        mic_rx,
                        asc_tx,
                        item_tx,
                        levels,
                    )
                }));
            }
            // Static endpoint source (Mic = capture endpoint): `run_capture` rebuilds it
            // in place on device change (§7). The Mic track fans its resampled chunks to
            // the mixer when a Mix track is present (D3).
            TrackFeed::Static(source) => {
                let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
                // Only the Mic track gets the live device-swap control (T2b); other static
                // sources (none today) pass `None` and behave exactly as before.
                let mic_control = (kind == AudioTrackKind::Mic)
                    .then(|| mic_control.clone())
                    .flatten();
                audio.push(spawn("audio-capture", move || {
                    Ok(run_capture(kind, source, mic_control, apkt_tx, cap_stop)?)
                }));
                let fanout = if kind == AudioTrackKind::Mic {
                    mic_mix_tx.take()
                } else {
                    None
                };
                audio.push(spawn("audio-process", move || {
                    audio_process_thread(
                        kind,
                        track_index,
                        bitrate,
                        apkt_rx,
                        asc_tx,
                        item_tx,
                        levels,
                        fanout,
                    )
                }));
            }
            // Live-bound per-app source (Game/VoiceChat, B3): the watcher discovers the
            // PID and `run_bound_capture` runs B2's process loopback on it, rebinding on
            // retarget. `§2.3` silence-fills the gaps.
            TrackFeed::Bound(role) => {
                bound_roles.push(role);
                let state = binding_state.clone();
                let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
                audio.push(spawn("audio-capture", move || {
                    run_bound_capture(kind, role, state, apkt_tx, cap_stop)
                }));
                audio.push(spawn("audio-process", move || {
                    audio_process_thread(
                        kind,
                        track_index,
                        bitrate,
                        apkt_rx,
                        asc_tx,
                        item_tx,
                        levels,
                        None,
                    )
                }));
            }
            // OtherSystem (D5): reads the watcher's live game binding and captures the
            // endpoint loopback with that game excluded (or the full loopback when no
            // game is bound). `run_other_system_capture` owns the endpoint↔exclude swap.
            TrackFeed::OtherSystem => {
                other_system_present = true;
                let state = binding_state.clone();
                let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
                audio.push(spawn("audio-capture", move || {
                    run_other_system_capture(state, apkt_tx, cap_stop)
                }));
                audio.push(spawn("audio-process", move || {
                    audio_process_thread(
                        kind,
                        track_index,
                        bitrate,
                        apkt_rx,
                        asc_tx,
                        item_tx,
                        levels,
                        None,
                    )
                }));
            }
        }
    }

    // One binding watcher per epoch drives PID discovery for the bound roles (B3) and,
    // when OtherSystem is present, the game PID it excludes (D5). Spawned whenever either
    // needs it — OtherSystem alone (game/VC tracks off) still requires game detection.
    if !bound_roles.is_empty() || other_system_present {
        let state = binding_state.clone();
        let vc_apps = params.vc_apps.clone();
        let game_detect = game_detect_for(&source);
        let stop = epoch_stop.clone();
        audio.push(spawn("binding-watcher", move || {
            binding_watcher_thread(
                state,
                bound_roles,
                other_system_present,
                vc_apps,
                game_detect,
                stop,
            )
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
    /// GOP length in whole seconds — the pre-roll margin the ring retains above
    /// `buffer_seconds` (`§6.2`). Held so a live replay-length change (T2b) can recompute
    /// the ring caps with the same `buffer_seconds + one GOP` retention the engine used.
    gop_seconds: u32,
    /// Estimated encoded bitrate (bps) at nominal 1080p — the byte-cap input, so a live
    /// replay-length change (T2b) recomputes the byte cap exactly as the engine did.
    est_bitrate_bps: u64,
    clear_after_save: bool,
    output_dir: PathBuf,
    save_hotkey_id: u32,
    record_hotkey_id: u32,
    /// The live mic-device control (T2b) — `Some` when a Mic track exists. A commanded
    /// [`EngineCommand::SetMicSelection`] pushes the new selection here for the capture
    /// thread's §7 rebuild. `None` when the mic is off (off↔on takes a restart).
    mic_control: Option<Arc<MicControl>>,
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
    mut cfg: RingThreadConfig,
    item_rx: Receiver<MuxItem>,
    save_job_tx: Sender<SaveJob>,
    rec_ctrl_tx: Sender<RecordCtrl>,
    rec_item_tx: Sender<MuxItem>,
    cmd_rx: Receiver<EngineCommand>,
    signal_tx: Sender<ShellSignal>,
    stats: PipelineStats,
    stop: Arc<AtomicBool>,
    status: Arc<EngineStatus>,
) -> Result<(), EngineError> {
    let clock = Clock::from_system()?;
    // Mutable so a live replay-length change (T2b `SetDurationCap`) re-derives the save
    // window alongside the ring's caps.
    let mut buffer_ticks = cfg.buffer_seconds as i64 * TICKS_PER_SECOND;
    let hotkey_rx = GlobalHotKeyEvent::receiver();
    // Announce the initial state to the shell (no-op if there is no shell) and the
    // status strip (A4).
    let _ = signal_tx.try_send(ShellSignal::State(TrayState::Buffering));
    status.set_state(TrayState::Buffering);
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
    // Whether the recording indicator (U8) currently reads "on" — mirrors `rec == On`,
    // published once per transition after the select! below.
    let mut recording_published = false;
    // The newest video PTS teed to the recording — the target the audio drains to at stop.
    let mut last_video_pts: i64 = 0;
    // Paused (tray): stop retaining NEW footage but keep the existing buffer + the
    // pipeline alive, and refuse to record (DECISIONS 2026-07-06 "M5 plan"). A save of
    // already-buffered footage still works while paused.
    let mut paused = false;
    // Watchdog (§6.3): poll the divergence flag and flip the tray WARNING/OK on a
    // transition. Suppressed while paused (the user's Paused state must win).
    let mut watchdog = Watchdog::new();
    let watchdog_rx = crossbeam_channel::tick(WATCHDOG_POLL_INTERVAL);

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
                            ingest_video(&mut ring, packet, paused, &stats.muxed);
                        }
                        MuxItem::Audio(track, packet) => {
                            ingest_audio(&mut ring, track, packet, paused);
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
                            if paused {
                                info!("ignoring record hotkey — buffering is paused");
                            } else {
                                rec = toggle_record(
                                    rec,
                                    &cfg.output_dir,
                                    &rec_ctrl_tx,
                                    last_video_pts,
                                );
                            }
                        }
                    }
                }
            },
            recv(cmd_rx) -> msg => match msg {
                // The tray injects the same actions as the hotkeys (plus pause/quit).
                Ok(EngineCommand::SaveClip) => {
                    if !trigger_save(
                        &mut ring, &clock, buffer_ticks, &cfg.output_dir,
                        &save_job_tx, cfg.clear_after_save, &mut last_save,
                    ) {
                        break; // mux worker gone
                    }
                }
                Ok(EngineCommand::ToggleRecord) => {
                    if paused {
                        info!("ignoring record toggle — buffering is paused");
                    } else {
                        rec = toggle_record(rec, &cfg.output_dir, &rec_ctrl_tx, last_video_pts);
                    }
                }
                Ok(EngineCommand::SetPaused(p)) => {
                    paused = p;
                    if p {
                        // Privacy: no recording while paused — drain any active one to a
                        // clean §5 tail, then it finalizes.
                        if let RingRec::On = rec {
                            info!("pausing — draining the active recording to its tail");
                            rec = RingRec::Draining {
                                until_pts: last_video_pts,
                                since: Instant::now(),
                            };
                        }
                    }
                    info!(paused = p, "buffer {}", if p { "paused" } else { "resumed" });
                    let state = if p {
                        TrayState::Paused
                    } else {
                        TrayState::Buffering
                    };
                    let _ = signal_tx.try_send(ShellSignal::State(state));
                    status.set_state(state);
                }
                Ok(EngineCommand::SetClearAfterSave(clear)) => {
                    // Live-apply from the settings editor (A5). Only affects what the
                    // NEXT save does; the running pipeline is untouched.
                    cfg.clear_after_save = clear;
                    info!(clear_after_save = clear, "clear-after-save updated (live)");
                }
                Ok(EngineCommand::SetOutputDir(dir)) => {
                    // Live-apply from the settings editor (T2). The save/record PATH is
                    // resolved per-save from `cfg.output_dir`, so the NEXT clip lands in the
                    // new folder with no epoch/encoder rebuild — the folder no longer needs a
                    // restart. The editor sends the already-resolved+created directory.
                    info!(dir = %dir.display(), "output folder updated (live)");
                    cfg.output_dir = dir;
                }
                Ok(EngineCommand::SetDurationCap(seconds)) => {
                    // Live-apply the instant-replay length (T2b). Recompute both ring caps
                    // and the save window with the SAME `buffer_seconds + one GOP` retention
                    // and nominal-1080p byte cap the engine used at start, then resize the
                    // ring: a grow just lets more accumulate, a shrink evicts now.
                    cfg.buffer_seconds = seconds;
                    buffer_ticks = seconds as i64 * TICKS_PER_SECOND;
                    let retained = seconds + cfg.gop_seconds;
                    ring.set_caps(
                        retained as i64 * TICKS_PER_SECOND,
                        byte_cap_bytes(retained, cfg.est_bitrate_bps),
                    );
                    info!(seconds, "instant-replay length updated (live)");
                }
                Ok(EngineCommand::SetSaveHotkeyId(id)) => {
                    // The editor re-registered the save combo on the pump thread and sent the
                    // new event id (T2b); switch the filter so the rebound combo fires the
                    // save with no restart.
                    info!(id, "save hotkey id updated (live)");
                    cfg.save_hotkey_id = id;
                }
                Ok(EngineCommand::SetRecordHotkeyId(id)) => {
                    info!(id, "record hotkey id updated (live)");
                    cfg.record_hotkey_id = id;
                }
                Ok(EngineCommand::SetMicSelection(selection)) => {
                    // Live mic DEVICE swap (T2b): push the new selection into the shared
                    // control; the running Mic capture thread reopens on it via §7. A no-op
                    // if the mic is off (no control / no Mic track — off↔on takes a restart).
                    match &cfg.mic_control {
                        Some(ctl) => {
                            info!(selection = ?selection, "mic device changed (live) — §7 rebuild");
                            ctl.set_selection(selection);
                        }
                        None => warn!("SetMicSelection ignored — no live Mic track (off↔on needs a restart)"),
                    }
                }
                Ok(EngineCommand::Shutdown) => {
                    info!("shutdown requested (tray Quit)");
                    stop.store(true, Ordering::Relaxed);
                }
                // Shell gone: the session lifetime is governed by `stop`/producers, not
                // by the command channel — keep buffering.
                Err(_) => {}
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
            recv(watchdog_rx) -> _ => {
                // Publish the live ring fill + stage counts for the status strip (A4)
                // on every tick (500 ms — a status display, not a meter). Done first so
                // it happens regardless of pause/divergence.
                status.set_fill(ring.duration_ticks(), ring.total_bytes());
                let (captured, encoded, muxed) = stats.snapshot();
                status.set_stage_counts(captured, encoded, muxed);

                // Flip the tray on a §6.3 divergence transition. Suppressed while paused
                // so the user's Paused state is never clobbered by a WARNING/OK signal.
                if !paused {
                    if let Some(level) = watchdog.observe(stats.is_diverged()) {
                        let state = match level {
                            WatchdogState::Warning => {
                                warn!("§6.3 divergence — encoder/mux falling behind; tray WARNING");
                                TrayState::Warning
                            }
                            WatchdogState::Ok => {
                                info!("pipeline recovered — clearing tray WARNING");
                                TrayState::Buffering
                            }
                        };
                        let _ = signal_tx.try_send(ShellSignal::State(state));
                        status.set_state(state);
                    }
                }
            },
        }
        // Publish the recording on/off state for the tray + status strip (U8). One point
        // catches every `RingRec` transition above; the start timestamp drives the UI's
        // "● Recording — MM:SS". `Draining` counts as off (the user pressed stop).
        let recording_now = matches!(rec, RingRec::On);
        if recording_now != recording_published {
            status.set_recording(recording_now, if recording_now { now_unix_ms() } else { 0 });
            recording_published = recording_now;
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

/// Toggle the timed recording — the shared body for both the record hotkey and the
/// tray's `ToggleRecord` command. `Off` → start; `On` → begin the `§5` tail-drain;
/// already-`Draining` is left alone.
fn toggle_record(
    rec: RingRec,
    output_dir: &Path,
    rec_ctrl_tx: &Sender<RecordCtrl>,
    last_video_pts: i64,
) -> RingRec {
    match rec {
        RingRec::Off => start_recording(output_dir, rec_ctrl_tx),
        RingRec::On => {
            info!("timed recording stopping — draining audio to the tail");
            RingRec::Draining {
                until_pts: last_video_pts,
                since: Instant::now(),
            }
        }
        draining => draining, // already stopping
    }
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

/// Ingest one video packet into the ring under the paused state. When `paused` the
/// packet is DROPPED — no new footage is retained while paused (privacy; the existing
/// buffer stays intact) — but it is still counted as consumed from the channel so the
/// `§6.3` `frames_in − frames_out` divergence check stays honest across a pause
/// (DECISIONS 2026-07-06 "M5 plan"). Returns whether the packet was retained. Pure.
fn ingest_video(
    ring: &mut Ring,
    packet: EncodedPacket,
    paused: bool,
    consumed: &AtomicU64,
) -> bool {
    let retained = !paused;
    if retained {
        ring.push_video(packet);
    }
    consumed.fetch_add(1, Ordering::Relaxed);
    retained
}

/// Ingest one audio packet into the ring under the paused state. Paused drops it
/// (keeping the existing buffer); otherwise it is pushed. Returns whether it was
/// retained (a non-paused push can still be dropped by the ring for an unknown
/// track — that result is forwarded). Pure.
fn ingest_audio(ring: &mut Ring, track: usize, packet: EncodedAudioPacket, paused: bool) -> bool {
    if paused {
        return false;
    }
    ring.push_audio(track, packet)
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
            // Resolve the foreground app's subfolder now (T5) — cheap kernel queries, no
            // file open — so the categorisation reflects what was on screen at the save
            // moment. The subfolder join + create + filename happen in the worker.
            let app_folder = crate::appfolder::foreground_app_folder();
            let job = SaveJob {
                window,
                dir: output_dir.to_path_buf(),
                app_folder,
            };
            if save_job_tx.send(job).is_err() {
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
    status: Arc<EngineStatus>,
    signal_tx: Sender<ShellSignal>,
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
                    process_save_job(&types, &asc_slots, num_audio, job, &status, &signal_tx);
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
    /// finalizes it — `§0`, a recording must not span epochs). Boxed: the writer's
    /// finalize state (per-track sample indexes) makes it far larger than the other
    /// variants, so keep `Rec` small.
    Active { writer: Box<Fmp4Writer>, epoch: u32 },
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
                        writer: Box::new(writer),
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
    status: &Arc<EngineStatus>,
    signal_tx: &Sender<ShellSignal>,
) {
    // Build the per-app subfolder (T5) + the final clip path HERE, in the worker — off the
    // save latency budget. Create the folder now; if that fails, save straight into the
    // base dir rather than failing the save (A3: a save must never fail on categorisation).
    let clip_dir = prepare_clip_subdir(&job.dir, &job.app_folder);
    let path = buffer_clip_path(&clip_dir);
    // The clip's containing folder (opened on a success-toast click) + its length. Both are
    // cheap (no I/O) and computed so the toast and the log line use the SAME data.
    let folder = clip_dir;
    let seconds =
        (job.window.last_video_pts - job.window.origin).max(0) as f32 / TICKS_PER_SECOND as f32;
    // Emit the save-outcome signal for the T1 balloon (`try_send`, so a slow/absent shell
    // never blocks the worker). Runs AFTER the write — off the save latency budget.
    let emit = |ok: bool, seconds: f32, reason: &str| {
        let _ = signal_tx.try_send(ShellSignal::Saved {
            ok,
            seconds,
            folder: folder.clone(),
            reason: reason.to_string(),
        });
    };

    let epoch = job.window.epoch_id;
    let Some(idx) = epoch_index(types.iter().map(|(e, _)| *e), epoch) else {
        warn!(
            epoch,
            "save skipped — no encoder output type for the clip's epoch yet"
        );
        // The user requested a save and got no clip → surface it as a failure in the
        // status strip (A4) + a failure toast, rather than leaving a stale prior success.
        status.set_last_save(SaveOutcome::Failed, now_unix_ms(), 0);
        emit(false, 0.0, "no clip for this moment yet");
        return;
    };
    let output_type = &types[idx].1;
    let audio_tracks: Vec<AudioTrackConfig> =
        match asc_slots.iter().cloned().collect::<Option<Vec<_>>>() {
            Some(v) if v.len() == num_audio => v,
            _ => {
                warn!("save skipped — audio track config(s) not yet known");
                status.set_last_save(SaveOutcome::Failed, now_unix_ms(), 0);
                emit(false, 0.0, "audio not ready yet");
                return;
            }
        };

    let start = Instant::now();
    let result = save::save_clip(&job.window, &output_type.0, &audio_tracks, &path);
    let ms = start.elapsed().as_millis() as u64;
    match result {
        Ok(path) => {
            if ms as i64 > SAVE_DURATION_WARN_MS {
                warn!(path = %path.display(), ms, seconds, "clip saved (slow write — disk suspect, §6.3)");
            } else {
                info!(path = %path.display(), ms, seconds, "clip saved");
            }
            status.set_last_save(SaveOutcome::Ok, now_unix_ms(), ms);
            emit(true, seconds, "");
        }
        Err(e) => {
            let reason = save_fail_reason(&e);
            error!(error = %e, reason, "clip save FAILED");
            status.set_last_save(SaveOutcome::Failed, now_unix_ms(), ms);
            emit(false, 0.0, &reason);
        }
    }
}

/// A short, honest failure reason for the save toast (T1) — the full error stays in the
/// log. Pure, so the mapping is unit-tested.
fn save_fail_reason(e: &save::SaveError) -> String {
    let low = e.to_string().to_ascii_lowercase();
    if low.contains("space") || low.contains("disk full") {
        "disk full".to_string()
    } else if low.contains("denied") || low.contains("permission") {
        "folder not writable".to_string()
    } else if low.contains("empty") {
        "nothing to save yet".to_string()
    } else {
        "see the log".to_string()
    }
}

/// Current wall-clock time as a Unix-epoch millisecond count, for the status strip's
/// "last save" timestamp (formatted relative to now by the UI). Saturates to `0`
/// before the epoch (never happens in practice).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Index of the entry tagged `epoch` (exact match, first occurrence) — the `§4.2`
/// per-epoch output-type selection. Pure, so it is unit-tested directly.
fn epoch_index(epochs: impl Iterator<Item = u32>, epoch: u32) -> Option<usize> {
    epochs
        .enumerate()
        .find(|(_, e)| *e == epoch)
        .map(|(i, _)| i)
}

/// The per-app clip subdirectory (T5): `base/<app_folder>`, created if missing. An empty
/// `app_folder` (unidentified app was already mapped to "Other" upstream, so this is
/// mainly defensive) or a `create_dir_all` failure (permissions, read-only drive) falls
/// back to `base` — a save must NEVER fail on categorisation (A3). Runs in the mux worker,
/// off the save latency budget.
fn prepare_clip_subdir(base: &Path, app_folder: &str) -> PathBuf {
    if app_folder.is_empty() {
        return base.to_path_buf();
    }
    let dir = base.join(app_folder);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "could not create the per-app clip folder — saving into the base folder");
            base.to_path_buf()
        }
    }
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

    // --- M5 T3: pause = stop ingest, keep the buffer -------------------------------

    fn test_ring() -> Ring {
        Ring::new(RingCaps {
            max_duration_ticks: 60 * TICKS_PER_SECOND,
            max_bytes: 64 * 1024 * 1024,
            num_audio_tracks: 1,
        })
    }

    fn vpkt(pts: i64, keyframe: bool) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(vec![0u8; 32].into_boxed_slice()),
            pts,
            duration: TICKS_PER_SECOND / 60,
            is_keyframe: keyframe,
            epoch_id: 0,
        }
    }

    /// While paused, a video packet is DROPPED (not retained) — but still counted as
    /// consumed, so the `§6.3` divergence check does not falsely fire during a pause.
    #[test]
    fn paused_video_is_dropped_but_still_counted() {
        let mut ring = test_ring();
        let consumed = AtomicU64::new(0);
        let retained = ingest_video(&mut ring, vpkt(0, true), true, &consumed);
        assert!(!retained, "paused ingest must not retain");
        assert!(ring.is_empty(), "paused ingest must not touch the buffer");
        assert_eq!(
            consumed.load(Ordering::Relaxed),
            1,
            "a dropped packet is still consumed from the channel (honest divergence)"
        );
    }

    /// When not paused, a video packet is retained and counted.
    #[test]
    fn unpaused_video_is_retained_and_counted() {
        let mut ring = test_ring();
        let consumed = AtomicU64::new(0);
        let retained = ingest_video(&mut ring, vpkt(0, true), false, &consumed);
        assert!(retained);
        assert!(!ring.is_empty(), "unpaused ingest retains the packet");
        assert_eq!(consumed.load(Ordering::Relaxed), 1);
    }

    /// Pause keeps EXISTING footage: buffer contents from before the pause survive, and
    /// resuming ingests again — so a save while paused still finds the pre-pause GOP.
    #[test]
    fn pause_retains_existing_buffer_then_resumes() {
        let mut ring = test_ring();
        let consumed = AtomicU64::new(0);
        // Buffer a keyframe before pausing.
        ingest_video(&mut ring, vpkt(0, true), false, &consumed);
        assert!(!ring.is_empty());
        // Pause: a new packet is dropped, but the earlier one is retained.
        ingest_video(&mut ring, vpkt(1_000, false), true, &consumed);
        assert!(!ring.is_empty(), "pausing must not clear the buffer");
        // Resume: ingest works again.
        let retained = ingest_video(&mut ring, vpkt(2_000, true), false, &consumed);
        assert!(retained);
        assert_eq!(
            consumed.load(Ordering::Relaxed),
            3,
            "all three were consumed"
        );
    }

    /// Paused audio is dropped (the buffer is not extended while paused).
    #[test]
    fn paused_audio_is_dropped() {
        let mut ring = test_ring();
        let pkt = EncodedAudioPacket {
            stream: AudioTrackKind::Mix,
            data: Arc::from(vec![0u8; 16].into_boxed_slice()),
            pts: 0,
            duration: 1024,
        };
        assert!(!ingest_audio(&mut ring, 0, pkt, true), "paused audio drops");
    }

    // --- B1: the audio track-set builder (sources ≠ tracks) -----------------------

    fn model(
        desktop: bool,
        mic: bool,
        separate_tracks: bool,
        game: bool,
        voice_chat: bool,
        other_system: bool,
    ) -> TrackModel {
        TrackModel {
            desktop,
            mic,
            separate_tracks,
            game,
            voice_chat,
            other_system,
        }
    }

    /// The new default (D1): `separate_tracks = false` ⇒ exactly Mix + Mic, in order.
    #[test]
    fn planned_default_is_mix_then_mic() {
        assert_eq!(
            planned_kinds(model(true, true, false, true, true, true)),
            vec![AudioTrackKind::Mix, AudioTrackKind::Mic],
            "separate_tracks off must collapse to Mix+Mic regardless of the tracks.* toggles"
        );
    }

    /// The full topology (`separate_tracks = true`, all toggles on): the fixed
    /// container order Mix, Game, VoiceChat, OtherSystem, Mic.
    #[test]
    fn planned_full_topology_is_in_container_order() {
        assert_eq!(
            planned_kinds(model(true, true, true, true, true, true)),
            vec![
                AudioTrackKind::Mix,
                AudioTrackKind::Game,
                AudioTrackKind::VoiceChat,
                AudioTrackKind::OtherSystem,
                AudioTrackKind::Mic,
            ]
        );
    }

    /// Mix is always container index 0 whenever it is present; Mic is always last.
    #[test]
    fn planned_mix_is_first_and_mic_is_last() {
        // Over every combination that captures desktop audio, Mix leads and (when
        // present) Mic trails.
        for &sep in &[false, true] {
            for &g in &[false, true] {
                for &v in &[false, true] {
                    for &o in &[false, true] {
                        for &mic in &[false, true] {
                            let ks = planned_kinds(model(true, mic, sep, g, v, o));
                            assert_eq!(ks[0], AudioTrackKind::Mix, "Mix must be index 0");
                            if mic {
                                assert_eq!(
                                    *ks.last().unwrap(),
                                    AudioTrackKind::Mic,
                                    "Mic must be last when present"
                                );
                            } else {
                                assert!(!ks.contains(&AudioTrackKind::Mic));
                            }
                            // No duplicate track kinds, ever.
                            let mut sorted: Vec<usize> = ks.iter().map(|k| k.index()).collect();
                            sorted.sort_unstable();
                            sorted.dedup();
                            assert_eq!(sorted.len(), ks.len(), "no track kind may repeat");
                        }
                    }
                }
            }
        }
    }

    /// The per-source toggles gate their tracks independently (still in order).
    #[test]
    fn planned_track_toggles_gate_independently() {
        assert_eq!(
            planned_kinds(model(true, false, true, true, false, false)),
            vec![AudioTrackKind::Mix, AudioTrackKind::Game]
        );
        assert_eq!(
            planned_kinds(model(true, false, true, false, true, false)),
            vec![AudioTrackKind::Mix, AudioTrackKind::VoiceChat]
        );
        assert_eq!(
            planned_kinds(model(true, false, true, false, false, true)),
            vec![AudioTrackKind::Mix, AudioTrackKind::OtherSystem]
        );
    }

    /// Desktop off ⇒ no system-audio source at all, so only Mic can appear — even with
    /// `separate_tracks` and every toggle on (the Slice-A "mic only" behaviour).
    #[test]
    fn planned_desktop_off_yields_mic_only() {
        assert_eq!(
            planned_kinds(model(false, true, true, true, true, true)),
            vec![AudioTrackKind::Mic]
        );
        assert!(planned_kinds(model(false, false, true, true, true, true)).is_empty());
    }

    /// Which `track_feed` kinds are spawnable. Above the OS floor: Mix + Mic + all three
    /// per-app tracks (Game/VoiceChat bound, OtherSystem its endpoint↔exclude feed).
    /// Below the floor the three per-app tracks vanish (Mix/Mic only) — the process-
    /// loopback capability gate.
    #[test]
    fn track_feed_spawnable_set_depends_on_os_support() {
        let sel = DeviceSelection::DefaultFollow;
        let mic = Some(&sel);

        // Supported: Mix (mixer, mic present) + Mic static, Game + VoiceChat bound,
        // OtherSystem its own feed.
        assert!(matches!(
            track_feed(AudioTrackKind::Mix, mic, true),
            Some(TrackFeed::Mix { mic_present: true })
        ));
        assert!(matches!(
            track_feed(AudioTrackKind::Mic, mic, true),
            Some(TrackFeed::Static(AudioSource::MicEndpoint(_)))
        ));
        assert!(matches!(
            track_feed(AudioTrackKind::Game, mic, true),
            Some(TrackFeed::Bound(BoundRole::Game))
        ));
        assert!(matches!(
            track_feed(AudioTrackKind::VoiceChat, mic, true),
            Some(TrackFeed::Bound(BoundRole::VoiceChat))
        ));
        assert!(matches!(
            track_feed(AudioTrackKind::OtherSystem, mic, true),
            Some(TrackFeed::OtherSystem)
        ));

        // Below the floor: the per-app tracks are hidden; Mix + Mic still spawn.
        assert!(track_feed(AudioTrackKind::Mix, mic, false).is_some());
        assert!(track_feed(AudioTrackKind::Mic, mic, false).is_some());
        assert!(track_feed(AudioTrackKind::Game, mic, false).is_none());
        assert!(track_feed(AudioTrackKind::VoiceChat, mic, false).is_none());
        assert!(track_feed(AudioTrackKind::OtherSystem, mic, false).is_none());

        // Mic off: the Mix track exists but knows the mic is absent; no Mic track.
        assert!(matches!(
            track_feed(AudioTrackKind::Mix, None, true),
            Some(TrackFeed::Mix { mic_present: false })
        ));
        assert!(track_feed(AudioTrackKind::Mic, None, true).is_none());
    }

    /// OtherSystem's source (decision D5): no game bound → the full endpoint loopback;
    /// a game bound → that PID's tree EXCLUDED (never included — that would double the
    /// game into OtherSystem, the exact bug the split-out avoided).
    #[test]
    fn other_system_source_switches_on_the_game_binding() {
        assert_eq!(
            other_system_source(None),
            AudioSource::EndpointLoopback,
            "no game bound → the plain default-endpoint loopback"
        );
        // The Game track binds include-tree; OtherSystem must exclude that same PID.
        let game = Binding {
            pid: 4242,
            include_tree: true,
        };
        assert_eq!(
            other_system_source(Some(game)),
            AudioSource::ProcessLoopback {
                pid: 4242,
                include_tree: false,
            },
            "a game bound → exclude-tree(game PID), so OtherSystem carries everything but the game"
        );
    }

    /// The Mic feed carries the endpoint selection through unchanged.
    #[test]
    fn track_feed_mic_carries_selection() {
        let sel = DeviceSelection::Pinned("mic-42".into());
        match track_feed(AudioTrackKind::Mic, Some(&sel), true) {
            Some(TrackFeed::Static(AudioSource::MicEndpoint(got))) => {
                assert_eq!(got, DeviceSelection::Pinned("mic-42".into()));
            }
            other => panic!("expected a mic endpoint feed, got {other:?}"),
        }
    }

    /// The invariant `spawnable_streams`/`spawnable_kinds` rely on: the spawned (meter)
    /// set is exactly the planned set intersected with what can be fed — so the two never
    /// drift. Above the OS floor with `separate_tracks`, that is the full
    /// Mix/Game/VoiceChat/OtherSystem/Mic; below it, Mix/Mic. Order is always the planned
    /// order.
    #[test]
    fn spawnable_is_planned_intersect_feed() {
        for &supported in &[false, true] {
            for &desktop in &[false, true] {
                for &mic in &[false, true] {
                    for &sep in &[false, true] {
                        let m = model(desktop, mic, sep, true, true, true);
                        let planned = planned_kinds(m);
                        let sel = DeviceSelection::DefaultFollow;
                        let mic_opt = mic.then_some(&sel);
                        let spawn: Vec<AudioTrackKind> = planned
                            .iter()
                            .copied()
                            .filter(|k| track_feed(*k, mic_opt, supported).is_some())
                            .collect();
                        // Stays in the planned order.
                        assert!(spawn.windows(2).all(|w| {
                            planned.iter().position(|k| *k == w[0]).unwrap()
                                < planned.iter().position(|k| *k == w[1]).unwrap()
                        }));
                        // Expected: Mix (desktop) → Game/VoiceChat/OtherSystem
                        // (desktop+sep+supported) → Mic.
                        let mut expected = Vec::new();
                        if desktop {
                            expected.push(AudioTrackKind::Mix);
                            if sep && supported {
                                expected.push(AudioTrackKind::Game);
                                expected.push(AudioTrackKind::VoiceChat);
                                expected.push(AudioTrackKind::OtherSystem);
                            }
                        }
                        if mic {
                            expected.push(AudioTrackKind::Mic);
                        }
                        assert_eq!(spawn, expected);
                        // OtherSystem spawns iff desktop + separate_tracks + OS floor.
                        assert_eq!(
                            spawn.contains(&AudioTrackKind::OtherSystem),
                            desktop && sep && supported
                        );
                    }
                }
            }
        }
    }

    /// `game_detect_for` maps the capture source onto the detection mode: monitor →
    /// foreground-fullscreen; a resolvable window → that fixed PID. (The HWND/foreground
    /// OS reads are HW-exercised at B7; this pins the monitor arms, which take no OS call.)
    #[test]
    fn game_detect_monitor_modes_are_foreground_fullscreen() {
        assert_eq!(
            game_detect_for(&CaptureSource::PrimaryMonitor),
            GameDetect::ForegroundFullscreen
        );
        assert_eq!(
            game_detect_for(&CaptureSource::Monitor(1)),
            GameDetect::ForegroundFullscreen
        );
    }
}
