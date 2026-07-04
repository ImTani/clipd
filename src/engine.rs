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
use crate::capture::convert::Converter;
use crate::capture::pacing::{PacingGrid, SlotAction};
use crate::capture::wgc::{CapturedFrame, WgcCapture};
use crate::clock::Clock;
use crate::com::ComMta;
use crate::encode::mft_aac::{f32_to_i16, AacEncoder, EncodedAudioPacket};
use crate::encode::mft_h264::{EncodedPacket, EncoderConfig, H264Encoder, InputFrame};
use crate::gpu::GpuContext;
use crate::mux::fmp4::{AudioTrackConfig, Fmp4Writer};
use crate::mux::SendMediaType;
use crate::ring::{Ring, RingCaps};
use crate::save::{self, select_window, SaveWindow};
use crate::spec_constants::audio::{CHANNELS, SAMPLE_RATE_HZ};
use crate::spec_constants::ring::{byte_cap_bytes, est_bitrate_bps};
use crate::spec_constants::units::TICKS_PER_SECOND;
use crate::spec_constants::video::nominal_frame_duration_ticks;
use crate::spec_constants::watchdog::SAVE_DURATION_WARN_MS;
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

/// One item on the merged mux channel: a video packet, or an AAC access unit
/// tagged with its track index (`§2.5`: 0 = desktop, 1 = mic — the position in
/// the `AudioTrackConfig` slice passed to [`Fmp4Writer::create`]). A single
/// merged channel avoids `select!` over a variable number of audio channels; the
/// mux thread dispatches on the variant. Both payloads own their bytes, so the
/// enum is `Send` with no `unsafe`.
enum MuxItem {
    /// An encoded H.264 packet for the video track.
    Video(EncodedPacket),
    /// An encoded AAC access unit for audio track `.0`.
    Audio(usize, EncodedAudioPacket),
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

/// How a recording epoch ended.
#[derive(Debug)]
pub enum RecordOutcome {
    /// Stopped normally (duration elapsed / user request); the segment is final.
    Completed(RecordStats),
    /// The device was lost mid-recording; the segment was finalized up to the
    /// loss and the caller should rebuild for the next epoch (a clip must not
    /// span epochs — `02-AV-SYNC-SPEC.md §0`).
    DeviceLost(RecordStats),
}

/// Parameters for a recording session.
#[derive(Debug, Clone)]
pub struct RecordParams {
    /// Final `.mp4` path (the muxer writes `…​.part` then renames).
    pub output_path: PathBuf,
    /// Output frame rate (the CFR grid rate).
    pub fps: u32,
    /// Whether to composite the cursor (`config.capture.cursor`).
    pub cursor: bool,
    /// Constant quality / QP (spec §6.1).
    pub cq: u32,
    /// Closed-GOP IDR interval in frames (spec §3).
    pub gop_frames: u32,
    /// Capture the desktop-loopback stream as audio track 0 (`config.audio.desktop`,
    /// `§2.5`).
    pub desktop_audio: bool,
    /// Capture the microphone as the next audio track (`config.audio.mic != "off"`,
    /// `§2.5`).
    pub mic_audio: bool,
    /// Mic endpoint policy (`config.audio.mic`: `default-follow` or a pinned id,
    /// `§7`). Ignored when `mic_audio` is false.
    pub mic_selection: DeviceSelection,
    /// AAC bitrate per audio track (`config.audio.bitrate_bps`, `§2.6`).
    pub audio_bitrate_bps: u32,
    /// Test-only: inject a synthetic device loss after this many seconds, to
    /// exercise the epoch-restart path without an actual sleep/resume. `None` in
    /// normal operation.
    pub simulate_loss_after: Option<u64>,
}

/// Final counts from a completed recording.
#[derive(Debug, Clone)]
pub struct RecordStats {
    /// Grid slots captured.
    pub captured: u64,
    /// Packets encoded.
    pub encoded: u64,
    /// Packets muxed.
    pub muxed: u64,
    /// The finalized output path.
    pub output_path: PathBuf,
}

/// A running recording: three worker threads plus the shared stop flag and
/// counters. Drive it from the main thread (wait, then [`Self::stop_and_join`]).
pub struct RecordingEngine {
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
    output_path: PathBuf,
    capture: JoinHandle<Result<(), EngineError>>,
    encode: JoinHandle<Result<(), EngineError>>,
    mux: JoinHandle<Result<PathBuf, EngineError>>,
    /// Audio worker pairs (capture + process) per enabled stream. Empty when
    /// audio is disabled, keeping the M1 video-only path intact.
    audio: Vec<JoinHandle<Result<(), EngineError>>>,
}

impl RecordingEngine {
    /// Spawn the pipeline. Returns immediately. The engine owns its OWN internal
    /// stop flag (distinct from the caller's user-stop): [`Self::stop_and_join`]
    /// sets it to wind down this epoch's workers, without disturbing the record
    /// loop's user-stop (so a device-loss epoch restart is not mistaken for a
    /// user request to end the recording).
    pub fn start(gpu: GpuContext, params: RecordParams) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = PipelineStats::new();

        // The enabled audio streams in fixed §2.5 order (desktop first, mic
        // second), each with its §7 endpoint policy. A stream's index in this
        // list is its track index in the container — track 0 if desktop-only, so
        // the mux slice is contiguous. Desktop loopback always follows the
        // default render endpoint.
        let mut audio_streams: Vec<(AudioStreamKind, DeviceSelection)> = Vec::new();
        if params.desktop_audio {
            audio_streams.push((AudioStreamKind::Desktop, DeviceSelection::DefaultFollow));
        }
        if params.mic_audio {
            audio_streams.push((AudioStreamKind::Mic, params.mic_selection.clone()));
        }
        let num_audio = audio_streams.len();

        let (size_tx, size_rx) = bounded::<(u32, u32)>(1);
        let (input_tx, input_rx) = bounded::<InputFrame>(INPUT_CHANNEL_CAP);
        let (mt_tx, mt_rx) = bounded::<SendMediaType>(1);
        // The merged video+audio channel and the ASC setup channel. The mux
        // thread collects the video type plus every track's ASC before it can
        // build the moov, so the ASC channel is separate from the data channel
        // (each audio-process thread sends its ASC first, then data): the mux
        // can finish setup even if the data channel has already back-filled.
        let (item_tx, item_rx) = bounded::<MuxItem>(MUX_CHANNEL_CAP);
        let (asc_tx, asc_rx) = bounded::<(usize, AudioTrackConfig)>(num_audio.max(1));

        let capture = {
            let gpu = gpu.clone();
            let stop = stop.clone();
            let captured = stats.captured.clone();
            let (cursor, fps, sim) = (params.cursor, params.fps, params.simulate_loss_after);
            spawn("capture", move || {
                capture_thread(gpu, cursor, fps, sim, size_tx, input_tx, stop, captured)
            })
        };
        let encode = {
            let gpu = gpu.clone();
            let encoded = stats.encoded.clone();
            let (fps, cq, gop) = (params.fps, params.cq, params.gop_frames);
            let item_tx = item_tx.clone();
            spawn("encode", move || {
                encode_thread(
                    gpu, fps, cq, gop, size_rx, input_rx, mt_tx, item_tx, encoded,
                )
            })
        };

        // One capture + one process thread per enabled audio stream. The process
        // thread owns the (COM) AAC encoder and (pure) resampler on the same MTA
        // thread — never moved from another (`CLAUDE.md` COM rule).
        let mut audio: Vec<JoinHandle<Result<(), EngineError>>> = Vec::new();
        for (track_index, (kind, selection)) in audio_streams.into_iter().enumerate() {
            let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
            let cap_stop = stop.clone();
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
        // Drop the parent handles so the mux's recv loop ends once every worker
        // clone is gone (channel disconnection is the shutdown signal).
        drop(item_tx);
        drop(asc_tx);

        let mux = {
            let muxed = stats.muxed.clone();
            let out = params.output_path.clone();
            spawn("mux", move || {
                mux_thread(out, num_audio, mt_rx, asc_rx, item_rx, muxed)
            })
        };

        Self {
            stop,
            stats,
            output_path: params.output_path,
            capture,
            encode,
            mux,
            audio,
        }
    }

    /// Whether any worker thread has already exited — i.e. the pipeline ended on
    /// its own (device loss or a fatal error) before a stop was requested. The
    /// record loop polls this to react to a device loss without waiting out the
    /// full duration.
    pub fn any_worker_finished(&self) -> bool {
        self.capture.is_finished()
            || self.encode.is_finished()
            || self.mux.is_finished()
            || self.audio.iter().any(JoinHandle::is_finished)
    }

    /// A live snapshot of the stage counters (for the watchdog / progress).
    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Signal stop, join all workers, and classify the outcome. A device-loss in
    /// any stage yields [`RecordOutcome::DeviceLost`] (the segment is still
    /// finalized by the mux thread on channel disconnect); any other stage error
    /// is surfaced as `Err`.
    pub fn stop_and_join(self) -> Result<RecordOutcome, EngineError> {
        self.stop.store(true, Ordering::Relaxed);
        let capture = join(self.capture, "capture");
        let encode = join(self.encode, "encode");
        // Join the audio workers before the mux: they drop the merged-channel
        // senders on exit, which lets the mux's recv loop terminate. Collect
        // their results so an audio-stage error is still surfaced.
        let audio: Vec<Result<(), EngineError>> =
            self.audio.into_iter().map(|h| join(h, "audio")).collect();
        let mux = join(self.mux, "mux");

        let (captured, encoded, muxed) = self.stats.snapshot();
        let stats = RecordStats {
            captured,
            encoded,
            muxed,
            output_path: self.output_path,
        };

        // Audio-stage failures are non-fatal to the video clip: any AUs produced
        // before the failure are already muxed, and the thread boundary logged
        // the cause. Proper audio device-change recovery (`§7`) is Task 6; until
        // then a *setup-time* audio failure (before the ASC handoff) instead
        // surfaces as a mux `ChannelClosed` error below, failing the segment.
        let audio_failures = audio.iter().filter(|r| r.is_err()).count();
        if audio_failures > 0 {
            warn!(
                audio_failures,
                "audio worker(s) ended in error; video clip unaffected"
            );
        }

        // Device loss → rebuild (the segment was finalized on disconnect).
        let device_lost = [&capture, &encode]
            .iter()
            .filter_map(|r| r.as_ref().err())
            .any(EngineError::is_device_lost);
        if device_lost {
            return Ok(RecordOutcome::DeviceLost(stats));
        }

        // Otherwise surface any hard error, then report completion.
        capture?;
        encode?;
        mux?;
        Ok(RecordOutcome::Completed(stats))
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
    cursor: bool,
    fps: u32,
    simulate_loss_after: Option<u64>,
    size_tx: Sender<(u32, u32)>,
    input_tx: Sender<InputFrame>,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();
    let capture = WgcCapture::start_primary(&gpu, cursor)?;
    let (width, height) = (capture.width(), capture.height());
    // Hand the frame size to the encode thread; ignore a closed receiver (the
    // engine is tearing down).
    let _ = size_tx.send((width, height));

    // NOTE (plan pitfall 11, deferred to M4): the pool, converter, and encoder are
    // sized once here. A mid-recording resolution / display-mode change is NOT a
    // DXGI device loss, so it does not funnel into the epoch restart — it surfaces
    // as a stage error that ends the recording rather than segmenting at the
    // boundary. Fixed-resolution monitor capture is the M1 scope; frame-pool
    // `Recreate` on a size change lands with window mode in M4.
    let mut converter = Converter::new(&gpu, width, height, fps)?;
    let mut grid = PacingGrid::with_default_grace(fps);
    let clock = Clock::from_system()?;
    let duration = nominal_frame_duration_ticks(fps);

    // Test hook: after this instant, return a synthetic device loss so the
    // epoch-restart path can be exercised without an actual sleep/resume.
    let loss_deadline =
        simulate_loss_after.map(|s| std::time::Instant::now() + Duration::from_secs(s));

    // The newest captured (BGRA) frame not yet converted, and the last NV12 we
    // produced (for resubmits on a static screen).
    let mut latest_frame: Option<CapturedFrame> = None;
    let mut last_nv12 = None;

    while !stop.load(Ordering::Relaxed) {
        // Drain WGC arrivals into the grid, keeping only the newest (keep-latest).
        while let Some(frame) = capture.take_latest() {
            grid.on_arrival(frame.system_relative_time);
            latest_frame = Some(frame);
        }

        let now = clock.now_ticks();
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

/// The encode thread: async H.264 MFT with CQP; hands the muxer the output type,
/// then pumps encoded packets onto the merged mux channel.
#[allow(clippy::too_many_arguments)]
fn encode_thread(
    gpu: GpuContext,
    fps: u32,
    cq: u32,
    gop_frames: u32,
    size_rx: Receiver<(u32, u32)>,
    input_rx: Receiver<InputFrame>,
    mt_tx: Sender<SendMediaType>,
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
    // Hand the negotiated output type (with SPS/PPS) to the muxer.
    let output_type = encoder.output_media_type()?;
    let _ = mt_tx.send(SendMediaType(output_type));

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
    let cfg = AudioTrackConfig {
        asc: encoder.audio_specific_config().to_vec(),
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE_HZ,
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

/// The mux thread: crash-safe fragmented MP4 ([`Fmp4Writer`]). Collects the video
/// output type plus every audio track's ASC (the moov needs all of them), then
/// dispatches merged items until every producer disconnects, then finalizes.
fn mux_thread(
    output_path: PathBuf,
    num_audio: usize,
    mt_rx: Receiver<SendMediaType>,
    asc_rx: Receiver<(usize, AudioTrackConfig)>,
    item_rx: Receiver<MuxItem>,
    muxed: Arc<AtomicU64>,
) -> Result<PathBuf, EngineError> {
    let _com = ComMta::initialize();
    let output_type = mt_rx.recv().map_err(|_| EngineError::ChannelClosed)?;

    // Gather every track's ASC into its slot before building the container. Each
    // audio-process thread sends exactly one; a missing one (a process thread
    // that died before the handoff) closes the channel and fails the segment.
    let mut slots: Vec<Option<AudioTrackConfig>> = (0..num_audio).map(|_| None).collect();
    for _ in 0..num_audio {
        let (idx, cfg) = asc_rx.recv().map_err(|_| EngineError::ChannelClosed)?;
        if let Some(slot) = slots.get_mut(idx) {
            *slot = Some(cfg);
        }
    }
    let audio_tracks: Vec<AudioTrackConfig> = slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(EngineError::ChannelClosed)?;

    let mut mux = Fmp4Writer::create(&output_type.0, &audio_tracks, &output_path)?;

    while let Ok(item) = item_rx.recv() {
        match item {
            MuxItem::Video(packet) => {
                mux.write_video_packet(&packet)?;
                muxed.fetch_add(1, Ordering::Relaxed);
            }
            MuxItem::Audio(track_index, packet) => {
                mux.write_audio_packet(track_index, &packet)?;
            }
        }
    }

    let final_path = mux.finish()?;
    info!(path = %final_path.display(), "recording finalized");
    Ok(final_path)
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
// Device-loss epoch restart (§7) is NOT wired into buffer mode yet — a mid-buffer
// device loss ends the session rather than segmenting the ring across epochs. The
// record path has the restart; folding it into buffer mode (ring spanning epochs,
// save picking the newest per §4.2) is a follow-up. Auto-QP-relief (§6.2) is
// likewise deferred: the ring exposes the fill signal (`duration_ticks`/`caps`)
// but the QP bump needs on-hardware tuning of the live encoder.

/// Parameters for a buffer (replay) session.
#[derive(Debug, Clone)]
pub struct BufferParams {
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
    /// Test-only (`--autosave N`): fire a save on this interval, in addition to the
    /// hotkey, so the 50-consecutive-saves and 24-hour-soak acceptance tests run
    /// unattended. `None` in normal operation. Exercises the same `§4` save path as
    /// the hotkey (a hidden hook, like `--simulate-device-loss`).
    pub autosave: Option<Duration>,
}

/// A save job handed from the ring thread to the save worker: an owned `§4` window
/// plus the destination path. The window owns cloned (`Arc`) packets, so the ring
/// keeps running (and may `clear`) while the worker muxes.
struct SaveJob {
    window: SaveWindow,
    path: PathBuf,
}

/// A running buffer session: the capture/encode/audio producers plus the ring and
/// save-worker threads. Drive it from the main thread (wait, then
/// [`Self::stop_and_join`]).
pub struct BufferEngine {
    stop: Arc<AtomicBool>,
    stats: PipelineStats,
    capture: JoinHandle<Result<(), EngineError>>,
    encode: JoinHandle<Result<(), EngineError>>,
    audio: Vec<JoinHandle<Result<(), EngineError>>>,
    ring: JoinHandle<Result<(), EngineError>>,
    save: JoinHandle<Result<(), EngineError>>,
}

impl BufferEngine {
    /// Spawn the buffer pipeline. Returns immediately; capture flows into the ring
    /// and a save-hotkey press writes the last `buffer_seconds` to `output_dir`.
    pub fn start(gpu: GpuContext, params: BufferParams) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = PipelineStats::new();

        // Enabled audio streams in §2.5 order (desktop first, mic second).
        let mut audio_streams: Vec<(AudioStreamKind, DeviceSelection)> = Vec::new();
        if params.desktop_audio {
            audio_streams.push((AudioStreamKind::Desktop, DeviceSelection::DefaultFollow));
        }
        if params.mic_audio {
            audio_streams.push((AudioStreamKind::Mic, params.mic_selection.clone()));
        }
        let num_audio = audio_streams.len();

        let (size_tx, size_rx) = bounded::<(u32, u32)>(1);
        let (input_tx, input_rx) = bounded::<InputFrame>(INPUT_CHANNEL_CAP);
        let (mt_tx, mt_rx) = bounded::<SendMediaType>(1);
        let (item_tx, item_rx) = bounded::<MuxItem>(MUX_CHANNEL_CAP);
        let (asc_tx, asc_rx) = bounded::<(usize, AudioTrackConfig)>(num_audio.max(1));
        let (save_job_tx, save_job_rx) = bounded::<SaveJob>(SAVE_JOB_CHANNEL_CAP);

        let capture = {
            let gpu = gpu.clone();
            let stop = stop.clone();
            let captured = stats.captured.clone();
            let (cursor, fps) = (params.cursor, params.fps);
            spawn("capture", move || {
                capture_thread(gpu, cursor, fps, None, size_tx, input_tx, stop, captured)
            })
        };
        let encode = {
            let gpu = gpu.clone();
            let encoded = stats.encoded.clone();
            let (fps, cq, gop) = (params.fps, params.cq, params.gop_frames);
            let item_tx = item_tx.clone();
            spawn("encode", move || {
                encode_thread(
                    gpu, fps, cq, gop, size_rx, input_rx, mt_tx, item_tx, encoded,
                )
            })
        };

        let mut audio: Vec<JoinHandle<Result<(), EngineError>>> = Vec::new();
        for (track_index, (kind, selection)) in audio_streams.into_iter().enumerate() {
            let (apkt_tx, apkt_rx) = bounded::<AudioPacket>(AUDIO_PACKET_CHANNEL_CAP);
            let cap_stop = stop.clone();
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
        drop(item_tx);
        drop(asc_tx);

        // Retain ONE GOP of pre-roll margin beyond buffer_seconds so a full-length
        // save (target = now − buffer_seconds) reliably finds an IDR at/before the
        // target instead of clamping at the whole-GOP eviction boundary (a
        // buffer_seconds save otherwise lands on the ring's oldest edge, where the
        // oldest retained IDR is usually a hair newer than the target → §4.2 clamp
        // on every save). buffer_seconds stays the SAVEABLE length; the margin is
        // the difference between "hold N seconds" and "let me save N seconds ending
        // at any frame". The §4.2 clamp WARN then fires only for genuine shortfalls
        // (buffer not full yet, or an epoch boundary within the window). DECISIONS.
        let gop_seconds = (params.gop_frames / params.fps.max(1)).max(1);
        let retained_seconds = params.buffer_seconds + gop_seconds;
        // Byte cap from the §6.2 estimate at this config; frame size isn't known
        // here yet (it arrives with the first frame), so it uses a nominal 1080p
        // tier — the exact tier only shifts the byte cap, and the duration cap is
        // the primary bound. (Refined once size flows through — a follow-up.)
        let est = est_bitrate_bps(1920, 1080, params.fps);
        let ring_caps = RingCaps {
            max_duration_ticks: retained_seconds as i64 * TICKS_PER_SECOND,
            max_bytes: byte_cap_bytes(retained_seconds, est),
            num_audio_tracks: num_audio,
        };

        let ring = {
            let stop = stop.clone();
            // The ring is the buffer-mode sink, so count consumed video packets into
            // `muxed` — otherwise `check_divergence` (encoded − muxed) sees muxed=0
            // and spuriously warns "mux falling behind" every second.
            let consumed = stats.muxed.clone();
            let (buffer_seconds, clear_after_save) =
                (params.buffer_seconds, params.clear_after_save);
            let (output_dir, save_hotkey_id) = (params.output_dir.clone(), params.save_hotkey_id);
            let autosave = params.autosave;
            spawn("ring", move || {
                ring_thread(
                    ring_caps,
                    buffer_seconds,
                    clear_after_save,
                    output_dir,
                    save_hotkey_id,
                    autosave,
                    item_rx,
                    save_job_tx,
                    consumed,
                    stop,
                )
            })
        };
        let save = spawn("save", move || {
            save_worker_thread(num_audio, mt_rx, asc_rx, save_job_rx)
        });

        Self {
            stop,
            stats,
            capture,
            encode,
            audio,
            ring,
            save,
        }
    }

    /// Whether any worker has already exited (e.g. capture hit a device loss).
    pub fn any_worker_finished(&self) -> bool {
        self.capture.is_finished()
            || self.encode.is_finished()
            || self.ring.is_finished()
            || self.save.is_finished()
            || self.audio.iter().any(JoinHandle::is_finished)
    }

    /// A live snapshot of the stage counters.
    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Signal stop and join every worker. Shutdown propagates by channel
    /// disconnection: capture breaks → encode/audio drain → the merged channel
    /// closes → the ring thread breaks → the save-job channel closes → the save
    /// worker drains its queue and exits.
    pub fn stop_and_join(self) -> Result<(), EngineError> {
        self.stop.store(true, Ordering::Relaxed);
        let capture = join(self.capture, "capture");
        let encode = join(self.encode, "encode");
        let audio: Vec<Result<(), EngineError>> =
            self.audio.into_iter().map(|h| join(h, "audio")).collect();
        let ring = join(self.ring, "ring");
        let save = join(self.save, "save");

        let audio_failures = audio.iter().filter(|r| r.is_err()).count();
        if audio_failures > 0 {
            warn!(audio_failures, "audio worker(s) ended in error");
        }
        capture?;
        encode?;
        ring?;
        save?;
        Ok(())
    }
}

/// The ring thread: push producers' packets into the [`Ring`]; on a save-hotkey
/// press run the pure `§4` [`select_window`] and dispatch an owned window to the
/// save worker. Owns the `Ring`; needs no COM.
#[allow(clippy::too_many_arguments)]
fn ring_thread(
    ring_caps: RingCaps,
    buffer_seconds: u32,
    clear_after_save: bool,
    output_dir: PathBuf,
    save_hotkey_id: u32,
    autosave: Option<Duration>,
    item_rx: Receiver<MuxItem>,
    save_job_tx: Sender<SaveJob>,
    consumed: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Result<(), EngineError> {
    let clock = Clock::from_system()?;
    let buffer_ticks = buffer_seconds as i64 * TICKS_PER_SECOND;
    let hotkey_rx = GlobalHotKeyEvent::receiver();
    // The `--autosave N` test hook fires on a tick; `never()` when disabled.
    let autosave_rx = match autosave {
        Some(d) => crossbeam_channel::tick(d),
        None => crossbeam_channel::never::<Instant>(),
    };
    let mut ring = Ring::new(ring_caps);
    let mut last_save: Option<Instant> = None;

    loop {
        select! {
            recv(item_rx) -> msg => match msg {
                Ok(MuxItem::Video(packet)) => {
                    ring.push_video(packet);
                    consumed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(MuxItem::Audio(track, packet)) => { ring.push_audio(track, packet); }
                Err(_) => break, // producers gone → shutdown
            },
            recv(hotkey_rx) -> ev => {
                let is_save = matches!(&ev, Ok(e)
                    if e.id == save_hotkey_id && matches!(e.state, HotKeyState::Pressed));
                if is_save && !trigger_save(
                    &mut ring, &clock, buffer_ticks, &output_dir, &save_job_tx,
                    clear_after_save, &mut last_save,
                ) {
                    break; // save worker gone
                }
            },
            recv(autosave_rx) -> _ => {
                if !trigger_save(
                    &mut ring, &clock, buffer_ticks, &output_dir, &save_job_tx,
                    clear_after_save, &mut last_save,
                ) {
                    break;
                }
            },
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    Ok(())
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

/// The save worker: holds the encoder output type + track ASCs (like the record
/// mux thread) and, per [`SaveJob`], drives the reused [`Fmp4Writer`] via
/// [`save::save_clip`] and logs the outcome (WARN if the write exceeds the `§6.3`
/// save-duration threshold — disk suspect). Runs in the MTA (COM/MF).
fn save_worker_thread(
    num_audio: usize,
    mt_rx: Receiver<SendMediaType>,
    asc_rx: Receiver<(usize, AudioTrackConfig)>,
    save_job_rx: Receiver<SaveJob>,
) -> Result<(), EngineError> {
    let _com = ComMta::initialize();
    let output_type = mt_rx.recv().map_err(|_| EngineError::ChannelClosed)?;

    // Gather every track's ASC into its slot before any save (mirrors mux_thread).
    let mut slots: Vec<Option<AudioTrackConfig>> = (0..num_audio).map(|_| None).collect();
    for _ in 0..num_audio {
        let (idx, cfg) = asc_rx.recv().map_err(|_| EngineError::ChannelClosed)?;
        if let Some(slot) = slots.get_mut(idx) {
            *slot = Some(cfg);
        }
    }
    let audio_tracks: Vec<AudioTrackConfig> = slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(EngineError::ChannelClosed)?;
    info!(tracks = num_audio, "save worker ready");

    while let Ok(job) = save_job_rx.recv() {
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
    Ok(())
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
