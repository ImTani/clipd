//! `engine` — the Milestone-1 recorder: three worker threads wired over bounded
//! channels, driven by the CFR pacing grid.
//!
//! ```text
//! capture ──InputFrame(bounded)──▶ encode ──EncodedPacket(bounded)──▶ mux ──▶ .mp4
//!    │  (WGC → VideoProcessor →         │  (async H.264 MFT, CQP)      │  (Sink Writer)
//!    │   pacing grid → NV12)            └─ output type ─(bounded 1)────┘
//! ```
//!
//! - **capture** owns WGC + the video processor + the pacing grid; it emits one
//!   NV12 frame per slot (fresh or a resubmit of the last, `02-AV-SYNC-SPEC §1`).
//! - **encode** runs the async MFT; it first hands the muxer the negotiated
//!   output media type (for the `avcC` box), then pumps packets.
//! - **mux** writes the MP4 on its own thread so a slow disk never stalls capture
//!   (plan pitfall 24 / data-flow rule 4).
//!
//! Shutdown propagates by channel disconnection: the main thread sets `stop`, the
//! capture loop breaks and drops its senders, the encoder drains, and the muxer
//! finalizes. Each worker body is wrapped in `catch_unwind` so a panic becomes an
//! error at the thread boundary instead of a silently dead thread under a live
//! process (the incumbent failure mode this project exists to kill).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use tracing::{error, info};

use crate::capture::convert::Converter;
use crate::capture::pacing::{PacingGrid, SlotAction};
use crate::capture::wgc::{CapturedFrame, WgcCapture};
use crate::clock::Clock;
use crate::com::ComMta;
use crate::encode::mft_h264::{EncodedPacket, EncoderConfig, H264Encoder, InputFrame};
use crate::gpu::GpuContext;
use crate::mux::sinkwriter::SinkWriterMux;
use crate::mux::SendMediaType;
use crate::spec_constants::video::nominal_frame_duration_ticks;
use crate::watchdog::PipelineStats;

/// Input queue depth (capture → encode). Kept below the NV12 pool depth so an
/// in-flight frame never has its pool texture recycled under it.
const INPUT_CHANNEL_CAP: usize = 4;
/// Encoded-packet queue depth (encode → mux).
const PACKET_CHANNEL_CAP: usize = 8;

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
    /// Mux stage failure.
    #[error("mux stage: {0}")]
    Mux(#[from] crate::mux::sinkwriter::MuxError),
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
    capture: JoinHandle<Result<(), EngineError>>,
    encode: JoinHandle<Result<(), EngineError>>,
    mux: JoinHandle<Result<PathBuf, EngineError>>,
}

impl RecordingEngine {
    /// Spawn the pipeline. Returns immediately; the main thread controls the
    /// `stop` flag and later joins via [`Self::stop_and_join`].
    pub fn start(gpu: GpuContext, params: RecordParams, stop: Arc<AtomicBool>) -> Self {
        let stats = PipelineStats::new();

        let (size_tx, size_rx) = bounded::<(u32, u32)>(1);
        let (input_tx, input_rx) = bounded::<InputFrame>(INPUT_CHANNEL_CAP);
        let (mt_tx, mt_rx) = bounded::<SendMediaType>(1);
        let (pkt_tx, pkt_rx) = bounded::<EncodedPacket>(PACKET_CHANNEL_CAP);

        let capture = {
            let gpu = gpu.clone();
            let stop = stop.clone();
            let captured = stats.captured.clone();
            let (cursor, fps) = (params.cursor, params.fps);
            spawn("capture", move || {
                capture_thread(gpu, cursor, fps, size_tx, input_tx, stop, captured)
            })
        };
        let encode = {
            let gpu = gpu.clone();
            let encoded = stats.encoded.clone();
            let (fps, cq, gop) = (params.fps, params.cq, params.gop_frames);
            spawn("encode", move || {
                encode_thread(gpu, fps, cq, gop, size_rx, input_rx, mt_tx, pkt_tx, encoded)
            })
        };
        let mux = {
            let muxed = stats.muxed.clone();
            let out = params.output_path.clone();
            spawn("mux", move || mux_thread(out, mt_rx, pkt_rx, muxed))
        };

        Self {
            stop,
            stats,
            capture,
            encode,
            mux,
        }
    }

    /// A live snapshot of the stage counters (for the watchdog / progress).
    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Signal stop, join all workers, and surface the first stage error.
    pub fn stop_and_join(self) -> Result<RecordStats, EngineError> {
        self.stop.store(true, Ordering::Relaxed);
        let capture = join(self.capture, "capture");
        let encode = join(self.encode, "encode");
        let mux = join(self.mux, "mux");

        // Surface capture/encode errors first, then the mux result (which carries
        // the final path).
        capture?;
        encode?;
        let output_path = mux?;

        let (captured, encoded, muxed) = self.stats.snapshot();
        Ok(RecordStats {
            captured,
            encoded,
            muxed,
            output_path,
        })
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

    let mut converter = Converter::new(&gpu, width, height, fps)?;
    let mut grid = PacingGrid::with_default_grace(fps);
    let clock = Clock::from_system()?;
    let duration = nominal_frame_duration_ticks(fps);

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
    }
    Ok(())
}

/// The encode thread: async H.264 MFT with CQP; hands the muxer the output type,
/// then pumps encoded packets.
#[allow(clippy::too_many_arguments)]
fn encode_thread(
    gpu: GpuContext,
    fps: u32,
    cq: u32,
    gop_frames: u32,
    size_rx: Receiver<(u32, u32)>,
    input_rx: Receiver<InputFrame>,
    mt_tx: Sender<SendMediaType>,
    pkt_tx: Sender<EncodedPacket>,
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
            let _ = pkt_tx.send(packet);
            encoded.fetch_add(1, Ordering::Relaxed);
        },
    )?;
    Ok(())
}

/// The mux thread: Sink Writer MP4 (M1 first cut). Waits for the output type,
/// then writes packets until the encoder disconnects, then finalizes.
fn mux_thread(
    output_path: PathBuf,
    mt_rx: Receiver<SendMediaType>,
    pkt_rx: Receiver<EncodedPacket>,
    muxed: Arc<AtomicU64>,
) -> Result<PathBuf, EngineError> {
    let _com = ComMta::initialize();
    let output_type = mt_rx.recv().map_err(|_| EngineError::ChannelClosed)?;
    let mux = SinkWriterMux::create(&output_type.0, &output_path)?;

    while let Ok(packet) = pkt_rx.recv() {
        mux.write_packet(&packet)?;
        muxed.fetch_add(1, Ordering::Relaxed);
    }

    let final_path = mux.finish()?;
    info!(path = %final_path.display(), "recording finalized");
    Ok(final_path)
}
