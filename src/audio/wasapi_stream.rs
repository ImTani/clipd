//! `audio::wasapi_stream` — WASAPI capture for the desktop-loopback and mic
//! streams (`02-AV-SYNC-SPEC.md §2.1`/`§2.2`, `01-PROJECT-PLAN.md §2`).
//!
//! Cannibalized from Milestone-0 spike #3 (`spikes/wasapi_audio_spike/`), which
//! proved per-packet QPC stamping and the loopback-silence / device-unplug
//! behaviour. This promotes the spike into a real worker: each stream runs on
//! its own thread and emits [`AudioPacket`]s over a channel instead of writing a
//! WAV.
//!
//! ## Format and rate (`§2.1`)
//! The stream is opened **shared, event-driven**, requesting f32 **stereo** at
//! the device's **native sample rate** with WASAPI autoconvert on. Autoconvert
//! therefore handles only the integer→float and channel mapping; the sample rate
//! is left native **on purpose** — `§2.4` requires our own micro-resampler
//! (`rubato`, Task 3) to convert native→48 kHz while applying the drift
//! correction. Letting WASAPI resample the rate would hide exactly the
//! device-crystal drift this spec exists to measure. The native rate and frame
//! count travel on every packet so the resampler and the drift window can do
//! their honest work.
//!
//! ## Timestamps (`§2.2`)
//! `IAudioCaptureClient::GetBuffer` reports the QPC position of the first sample
//! as a `u64` already in 100 ns ticks — the master domain (`§0`), no `qpf`
//! conversion needed. That is the packet PTS, full stop. Buggy drivers that set
//! `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR` or report a zero position fall back, for
//! that packet only, to `prev_pts + prev_frames·ticks/rate`; if that happens more
//! than 100×/minute the stream switches permanently (this session) to sample
//! counting anchored at the last good QPC, and the drift controller (`§2.4`)
//! does the honest work. All of that per-packet decision logic lives in the pure,
//! unit-tested [`PtsDeriver`]; the WASAPI event loop around it needs hardware and
//! is exercised by the `audio-probe` diagnostic.
//!
//! ## `unsafe`
//! None. The `wasapi` crate is the COM wrapper (CLAUDE.md confines `unsafe` to
//! such wrappers); everything here is safe Rust over its API. An [`AudioPacket`]
//! carries only owned PCM + primitives, so it crosses the channel with no
//! `unsafe impl Send`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use tracing::{info, warn};
use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, Handle,
    SampleType, StreamMode, WaveFormat,
};

use super::devices::{Debouncer, DefaultChangeWatcher, DeviceSelection};
use crate::clock::{Clock, MonotonicGuard};
use crate::spec_constants::audio::{BAD_QPC_PER_MINUTE_THRESHOLD, BUFFER_PERIODS, CHANNELS};
use crate::spec_constants::units::TICKS_PER_SECOND;

/// Which capture stream this is. Track order in the container is fixed by
/// `§2.5` (desktop first, mic second); this enum names the source, not the
/// track index (see [`crate::spec_constants::audio::TRACK_DESKTOP`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStreamKind {
    /// Desktop audio via loopback on the default render endpoint.
    Desktop,
    /// Microphone via the default (or pinned) capture endpoint.
    Mic,
}

impl AudioStreamKind {
    /// A short lower-case label for logs / probe output.
    pub fn label(self) -> &'static str {
        match self {
            AudioStreamKind::Desktop => "desktop",
            AudioStreamKind::Mic => "mic",
        }
    }
}

/// One captured audio packet: interleaved f32 stereo samples at the device's
/// **native** rate, stamped with the QPC PTS of its first sample (`§2.2`).
#[derive(Debug, Clone)]
pub struct AudioPacket {
    /// Which stream produced this packet.
    pub stream: AudioStreamKind,
    /// PTS of the first sample, ticks in the master domain (`§0`).
    pub pts: i64,
    /// Frame count (samples per channel) in this packet, at [`Self::sample_rate`].
    pub frames: u32,
    /// The device's native sample rate (Hz) these samples are at. The resampler
    /// (Task 3) converts this to 48 kHz.
    pub sample_rate: u32,
    /// Interleaved stereo f32 samples (`frames × 2` values).
    pub samples: Vec<f32>,
    /// Whether the driver flagged this buffer as silence.
    pub silent: bool,
    /// Whether the driver flagged a data discontinuity (glitch) before it.
    pub discontinuity: bool,
}

/// Errors from opening or running a capture stream.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// A `wasapi`/COM call failed. The crate returns `Box<dyn Error>`; we keep
    /// the message. In the RUNNING loop, any such error (the classic being
    /// `AUDCLNT_E_DEVICE_INVALIDATED` on unplug) triggers a `§7` rebuild rather
    /// than surfacing — the response is the same for every device error, so no
    /// finer classification is needed. This variant only escapes `run_capture` if
    /// the master clock itself fails.
    #[error("WASAPI: {0}")]
    Wasapi(String),
    /// The master clock could not be established.
    #[error("audio clock: {0}")]
    Clock(#[from] crate::clock::ClockError),
}

/// Wrap a `wasapi` boxed error as an [`AudioError`].
fn wa<E: std::fmt::Display>(e: E) -> AudioError {
    AudioError::Wasapi(e.to_string())
}

// ── Pure PTS derivation (`§2.2`) ──────────────────────────────────────────────

/// Rolling 60 s window in ticks, for the `bad_qpc`/minute threshold (`§6.3`).
const ONE_MINUTE_TICKS: i64 = 60 * TICKS_PER_SECOND;

/// Per-stream PTS derivation with the `§2.2` bad-QPC fallback. Pure and
/// unit-tested; the capture loop feeds it each packet's raw QPC position, frame
/// count, and timestamp-error flag, and gets back the PTS to stamp.
///
/// Behaviour:
/// - Good QPC: PTS is the reported position (already ticks).
/// - Bad QPC (`timestamp_error` or a zero position) or permanent sample-counting
///   mode: PTS = `prev_pts + prev_frames·ticks/rate` (anchored at the last good
///   value), and a `bad_qpc` event is recorded.
/// - More than [`BAD_QPC_PER_MINUTE_THRESHOLD`] bad events in any trailing minute
///   flips the stream permanently (this session) to sample counting.
/// - The result always passes the `§0` monotonicity guard (strictly increasing;
///   a violation bumps to `prev + 1` and is counted).
#[derive(Debug)]
pub struct PtsDeriver {
    rate: u32,
    guard: MonotonicGuard,
    /// The last admitted PTS and the frame count of that packet, for the
    /// fallback anchor.
    prev: Option<(i64, u32)>,
    /// PTS of recent bad-QPC packets, for the per-minute threshold.
    bad_events: VecDeque<i64>,
    bad_qpc_total: u64,
    sample_counting: bool,
}

impl PtsDeriver {
    /// A fresh deriver for a stream captured at `rate` Hz.
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            guard: MonotonicGuard::new(),
            prev: None,
            bad_events: VecDeque::new(),
            bad_qpc_total: 0,
            sample_counting: false,
        }
    }

    /// Derive the PTS for a packet: `raw_qpc_ticks` is the driver-reported
    /// position (100 ns units), `frames` its frame count, `timestamp_error` the
    /// driver's flag. Returns the master-domain PTS to stamp.
    pub fn derive(&mut self, raw_qpc_ticks: u64, frames: u32, timestamp_error: bool) -> i64 {
        let bad = timestamp_error || raw_qpc_ticks == 0;

        // Candidate PTS before the monotonicity guard: sample-count fallback when
        // in permanent mode or this packet's QPC is bad AND we have an anchor.
        let candidate = match self.prev {
            Some((pp, pf)) if self.sample_counting || bad => pp + frames_to_ticks(pf, self.rate),
            _ => raw_qpc_ticks as i64,
        };

        let pts = self.guard.admit(candidate);

        if bad {
            self.bad_qpc_total += 1;
            self.bad_events.push_back(pts);
            let cutoff = pts - ONE_MINUTE_TICKS;
            while self.bad_events.front().is_some_and(|&t| t <= cutoff) {
                self.bad_events.pop_front();
            }
            if !self.sample_counting && self.bad_events.len() as u32 > BAD_QPC_PER_MINUTE_THRESHOLD
            {
                self.sample_counting = true;
                warn!(
                    rate = self.rate,
                    "bad QPC > {BAD_QPC_PER_MINUTE_THRESHOLD}/min — switching stream to sample counting (§2.2)"
                );
            }
        }

        self.prev = Some((pts, frames));
        pts
    }

    /// Total bad-QPC packets seen (diagnostic).
    #[inline]
    pub fn bad_qpc_total(&self) -> u64 {
        self.bad_qpc_total
    }

    /// Whether the stream has switched permanently to sample counting (`§2.2`).
    #[inline]
    pub fn sample_counting(&self) -> bool {
        self.sample_counting
    }

    /// Monotonicity violations observed (`§0`; steady state is 0).
    #[inline]
    pub fn ts_violations(&self) -> u64 {
        self.guard.violations()
    }
}

/// Ticks spanned by `frames` at `rate` Hz, floored — the `§2.2` fallback's
/// `prev_frames · 10_000_000 / rate` generalized to the native rate.
#[inline]
fn frames_to_ticks(frames: u32, rate: u32) -> i64 {
    (frames as i128 * TICKS_PER_SECOND as i128 / rate.max(1) as i128) as i64
}

// ── WASAPI capture loop (hardware) ────────────────────────────────────────────

/// Run one capture stream until `stop` is set, sending [`AudioPacket`]s to `tx`,
/// **rebuilding the WASAPI client in place** across device changes (`§7`). Opens
/// the endpoint chosen by `selection` (Render in loopback for
/// [`AudioStreamKind::Desktop`], Capture for [`AudioStreamKind::Mic`]),
/// requesting f32 stereo at the device's native rate.
///
/// Rebuild triggers (`§7`): any WASAPI call error (unplug / invalidation —
/// immediate) or a debounced default-endpoint switch (`default-follow`). Only the
/// WASAPI client is recreated; the [`PtsDeriver`] survives (so PTS stays
/// monotonic across the hole) and the downstream resampler + AAC encoder are
/// untouched, so a rebuild never re-anchors the audio timeline — the QPC PTS just
/// jumps forward by the hole and the `§2.3` synthesizer fills it with silence.
///
/// Runs its own MTA apartment (CLAUDE.md COM rule); owns all its `wasapi` and
/// COM objects, none of which cross the thread boundary.
pub fn run_capture(
    kind: AudioStreamKind,
    selection: DeviceSelection,
    tx: Sender<AudioPacket>,
    stop: Arc<AtomicBool>,
) -> Result<(), AudioError> {
    initialize_mta().ok().map_err(wa)?;
    let enumerator = DeviceEnumerator::new().map_err(wa)?;
    let clock = Clock::from_system()?;

    // Default-follow: watch for a default-endpoint switch and debounce the burst
    // (`§7`). A registration failure is non-fatal — we lose default-switch
    // chasing but keep the invalidation-driven rebuild (the AV-4 path).
    let default_changed = Arc::new(AtomicBool::new(false));
    let _watcher = match DefaultChangeWatcher::register(kind, default_changed.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(stream = kind.label(), error = %e, "device-change watcher unavailable — default-follow disabled (invalidation rebuild still active)");
            None
        }
    };
    let mut debouncer = Debouncer::new();

    // The deriver outlives individual sessions (monotonic across rebuilds); it is
    // recreated only when the native rate changes (§7 rebuild to a different device).
    let mut deriver: Option<PtsDeriver> = None;

    while !stop.load(Ordering::Relaxed) {
        // REBUILDING: open + start the session, retrying (a pinned device that is
        // gone, or no default yet). A failed open leaves `tx` intact, so the
        // downstream chain survives and the hole is silence-filled on recovery.
        let session = match open_session(&enumerator, kind, &selection) {
            Ok(s) => s,
            Err(e) => {
                warn!(stream = kind.label(), error = %e, "audio device open failed — retrying (§7)");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        // Recreate the deriver only on a native-rate change, so a same-rate
        // rebuild preserves the monotonic guard across the hole.
        let rate_changed = deriver
            .as_ref()
            .is_none_or(|d| d.rate != session.native_rate);
        if rate_changed {
            if deriver.is_some() {
                warn!(
                    stream = kind.label(),
                    new_rate = session.native_rate,
                    "device native rate changed on rebuild — re-deriving PTS (resampler switches rate downstream)"
                );
            }
            deriver = Some(PtsDeriver::new(session.native_rate));
        }
        let deriver = deriver.as_mut().expect("deriver set above");

        let outcome = run_stream(
            &session,
            kind,
            deriver,
            &tx,
            &stop,
            &default_changed,
            &mut debouncer,
            &clock,
        );
        // DRAINING: stop + release this session's client before the next open.
        let _ = session.audio_client.stop_stream();
        match outcome {
            StreamOutcome::Stopped => break,
            StreamOutcome::Rebuild => continue,
        }
    }

    if let Some(d) = &deriver {
        info!(
            stream = kind.label(),
            bad_qpc = d.bad_qpc_total(),
            sample_counting = d.sample_counting(),
            ts_violations = d.ts_violations(),
            "audio capture stopped"
        );
    }
    Ok(())
}

/// A live WASAPI capture session — the client objects for ONE device generation.
/// Recreated on each `§7` rebuild; everything downstream (deriver, resampler, AAC
/// encoder) outlives it.
struct CaptureSession {
    audio_client: AudioClient,
    capture_client: AudioCaptureClient,
    h_event: Handle,
    native_rate: u32,
    bytes_per_frame: usize,
}

/// Open, initialize, and start a capture session on the endpoint chosen by
/// `selection` (`§2.1` format; `§7` selection policy).
fn open_session(
    enumerator: &DeviceEnumerator,
    kind: AudioStreamKind,
    selection: &DeviceSelection,
) -> Result<CaptureSession, AudioError> {
    // Desktop loopback = the default *render* endpoint opened for capture; mic =
    // the *capture* endpoint (default-follow) or a pinned id (spike #3 finding).
    let device_dir = match kind {
        AudioStreamKind::Desktop => Direction::Render,
        AudioStreamKind::Mic => Direction::Capture,
    };
    let device = match selection {
        DeviceSelection::DefaultFollow => enumerator.get_default_device(&device_dir).map_err(wa)?,
        // Pinned: bind exactly this endpoint id; if it is gone the call fails and
        // the caller retries (record silence, never substitute — §7).
        DeviceSelection::Pinned(id) => enumerator.get_device(id).map_err(wa)?,
    };
    let device_name = device
        .get_friendlyname()
        .unwrap_or_else(|_| "<unknown>".into());
    let mut audio_client = device.get_iaudioclient().map_err(wa)?;

    // Native rate/channels from the device mix format; we request f32 stereo at
    // that SAME rate so autoconvert only touches format+channels (§2.1).
    let mix = audio_client.get_mixformat().map_err(wa)?;
    let native_rate = mix.get_samplespersec();
    let native_channels = mix.get_nchannels();
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        native_rate as usize,
        CHANNELS as usize,
        None,
    );

    let (def_period, _min_period) = audio_client.get_device_period().map_err(wa)?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        // §2.1: 4 × period of overrun headroom.
        buffer_duration_hns: def_period * BUFFER_PERIODS as i64,
    };
    // Always Direction::Capture at initialize; on a render device the crate sets
    // the loopback flag.
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(wa)?;

    let h_event = audio_client.set_get_eventhandle().map_err(wa)?;
    let capture_client = audio_client.get_audiocaptureclient().map_err(wa)?;
    let bytes_per_frame = format.get_blockalign() as usize; // 2ch × 4 bytes = 8

    audio_client.start_stream().map_err(wa)?;

    info!(
        stream = kind.label(),
        device = %device_name,
        native_rate,
        native_channels,
        "audio capture started (f32 stereo @ native rate; rubato → 48 kHz downstream)"
    );

    Ok(CaptureSession {
        audio_client,
        capture_client,
        h_event,
        native_rate,
        bytes_per_frame,
    })
}

/// Why a [`run_stream`] session ended.
enum StreamOutcome {
    /// `stop` was set or the consumer disconnected — end `run_capture`.
    Stopped,
    /// A device change (invalidation or debounced default switch) — rebuild.
    Rebuild,
}

/// The inner RUNNING loop for one session: drain packets until `stop`, a device
/// error, or a debounced default switch. Never drops `tx`, so the downstream
/// chain survives a `Rebuild`.
#[allow(clippy::too_many_arguments)]
fn run_stream(
    session: &CaptureSession,
    kind: AudioStreamKind,
    deriver: &mut PtsDeriver,
    tx: &Sender<AudioPacket>,
    stop: &AtomicBool,
    default_changed: &AtomicBool,
    debouncer: &mut Debouncer,
    clock: &Clock,
) -> StreamOutcome {
    let bytes_per_frame = session.bytes_per_frame;
    let mut deque: VecDeque<u8> =
        VecDeque::with_capacity(bytes_per_frame * session.native_rate as usize);

    while !stop.load(Ordering::Relaxed) {
        // Debounced default-endpoint switch (§7 default-follow): coalesce the
        // burst, rebuild ~250 ms after the first event.
        if default_changed.swap(false, Ordering::Relaxed) {
            debouncer.signal(clock.now_ticks());
        }
        if debouncer.pending() && debouncer.due(clock.now_ticks()) {
            info!(
                stream = kind.label(),
                "default endpoint changed — rebuilding stream (§7)"
            );
            return StreamOutcome::Rebuild;
        }

        // Poll faster while a debounce is armed so the 250 ms window is honoured;
        // otherwise the 200 ms idle timeout keeps a silent loopback cheap (§2.3).
        let timeout_ms = if debouncer.pending() { 20 } else { 200 };
        if session.h_event.wait_for_event(timeout_ms).is_err() {
            continue;
        }
        loop {
            // A device unplug/invalidation surfaces as an error here → immediate
            // rebuild (skip debounce, §7).
            let n = match session.capture_client.get_next_packet_size() {
                Ok(v) => v.unwrap_or(0),
                Err(e) => {
                    warn!(stream = kind.label(), error = %e, "audio device error — rebuilding stream (§7)");
                    return StreamOutcome::Rebuild;
                }
            };
            if n == 0 {
                break;
            }
            let before = deque.len();
            let info = match session.capture_client.read_from_device_to_deque(&mut deque) {
                Ok(i) => i,
                Err(e) => {
                    warn!(stream = kind.label(), error = %e, "audio device error — rebuilding stream (§7)");
                    return StreamOutcome::Rebuild;
                }
            };
            let frames = ((deque.len() - before) / bytes_per_frame) as u32;
            if frames == 0 {
                continue;
            }

            let pts = deriver.derive(info.timestamp, frames, info.flags.timestamp_error);
            let samples = drain_f32(&mut deque, frames as usize * CHANNELS as usize);

            let packet = AudioPacket {
                stream: kind,
                pts,
                frames,
                sample_rate: session.native_rate,
                samples,
                silent: info.flags.silent,
                discontinuity: info.flags.data_discontinuity,
            };
            if tx.send(packet).is_err() {
                return StreamOutcome::Stopped; // consumer gone
            }
        }
    }
    StreamOutcome::Stopped
}

/// Drain exactly `count` little-endian f32 samples from the byte deque.
fn drain_f32(deque: &mut VecDeque<u8>, count: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if deque.len() < 4 {
            break;
        }
        let b = [
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
            deque.pop_front().unwrap(),
        ];
        out.push(f32::from_le_bytes(b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_qpc_is_used_verbatim() {
        let mut d = PtsDeriver::new(48_000);
        assert_eq!(d.derive(1_000_000, 480, false), 1_000_000);
        assert_eq!(d.derive(1_100_000, 480, false), 1_100_000);
        assert_eq!(d.bad_qpc_total(), 0);
        assert_eq!(d.ts_violations(), 0);
    }

    #[test]
    fn timestamp_error_falls_back_to_sample_count() {
        let mut d = PtsDeriver::new(48_000);
        let p0 = d.derive(1_000_000, 480, false);
        // 480 frames @ 48 kHz = 100_000 ticks; the bad packet is anchored there.
        let p1 = d.derive(9_999_999, 480, true);
        assert_eq!(p1, p0 + 100_000);
        assert_eq!(d.bad_qpc_total(), 1);
    }

    #[test]
    fn zero_position_is_treated_as_bad() {
        let mut d = PtsDeriver::new(48_000);
        let p0 = d.derive(500_000, 480, false);
        let p1 = d.derive(0, 480, false);
        assert_eq!(p1, p0 + 100_000);
        assert_eq!(d.bad_qpc_total(), 1);
    }

    #[test]
    fn first_packet_bad_qpc_uses_raw_value() {
        // Nothing to anchor to: fall back to the raw value (even if 0) and let the
        // monotonic guard keep things increasing thereafter.
        let mut d = PtsDeriver::new(48_000);
        assert_eq!(d.derive(0, 480, true), 0);
        assert_eq!(d.bad_qpc_total(), 1);
    }

    #[test]
    fn monotonicity_guard_bumps_backward_qpc() {
        let mut d = PtsDeriver::new(48_000);
        assert_eq!(d.derive(1_000_000, 480, false), 1_000_000);
        // A backward (but "good") QPC still gets bumped to prev+1 (§0).
        assert_eq!(d.derive(999_000, 480, false), 1_000_001);
        assert_eq!(d.ts_violations(), 1);
    }

    #[test]
    fn native_rate_fallback_uses_that_rate() {
        // At 44.1 kHz, 441 frames = 100_000 ticks (floored 99_999... check exact).
        let mut d = PtsDeriver::new(44_100);
        let p0 = d.derive(2_000_000, 441, false);
        let p1 = d.derive(0, 441, true);
        assert_eq!(p1, p0 + frames_to_ticks(441, 44_100));
    }

    #[test]
    fn exceeding_bad_qpc_threshold_latches_sample_counting() {
        let mut d = PtsDeriver::new(48_000);
        d.derive(1_000_000, 480, false); // one good anchor
                                         // Feed > 100 bad packets within a minute (each advances ~100_000 ticks,
                                         // 101 packets ≈ 10.1 ms << 60 s, so all stay in the window).
        for _ in 0..(BAD_QPC_PER_MINUTE_THRESHOLD + 1) {
            d.derive(0, 480, true);
        }
        assert!(d.sample_counting());
    }

    #[test]
    fn sample_counting_is_permanent_once_latched() {
        let mut d = PtsDeriver::new(48_000);
        d.derive(1_000_000, 480, false);
        for _ in 0..(BAD_QPC_PER_MINUTE_THRESHOLD + 1) {
            d.derive(0, 480, true);
        }
        assert!(d.sample_counting());
        // Even a subsequent GOOD QPC is ignored in favour of sample counting.
        let (pp, pf) = d.prev.unwrap();
        let expected = pp + frames_to_ticks(pf, 48_000);
        assert_eq!(d.derive(50_000_000, 480, false), expected);
    }

    #[test]
    fn drain_f32_reads_little_endian() {
        let mut dq: VecDeque<u8> = VecDeque::new();
        dq.extend(1.0f32.to_le_bytes());
        dq.extend((-2.0f32).to_le_bytes());
        let out = drain_f32(&mut dq, 2);
        assert_eq!(out, vec![1.0, -2.0]);
    }
}
