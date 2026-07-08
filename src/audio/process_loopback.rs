//! `audio::process_loopback` — per-application (process-tree) loopback capture
//! via WASAPI `ActivateAudioInterfaceAsync` + PROCESS_LOOPBACK (Slice B / B2).
//!
//! The endpoint path ([`crate::audio::wasapi_stream::run_capture`]) binds a
//! *device* (default render for loopback, a capture endpoint for the mic). This
//! module binds a **process id + include/exclude tree** instead, so the
//! per-source system tracks (`AudioTrackKind::{Game,VoiceChat,OtherSystem}`,
//! amended `§2.5`) can carry just one app's audio. It emits the same
//! [`AudioPacket`] stream over the same channel, so everything downstream
//! (resample → gap → drift → AAC → ring → mux) is unchanged.
//!
//! ## What is different from endpoint capture (why this is its own module)
//! A process-loopback [`AudioClient`] is deliberately crippled by Windows (the
//! `wasapi` crate documents it): `get_mixformat`, `get_device_period`,
//! `get_current_padding`, `get_buffer_size`, `get_audioclock`, … all return
//! `E_NOTIMPL` or garbage. Consequences, all handled here:
//!
//! - **Fixed format, not queried.** We cannot ask the client its native format,
//!   so we *request* a fixed 48 kHz f32 stereo format (autoconvert on). The
//!   packet therefore carries `sample_rate = 48 kHz`; the downstream resampler
//!   runs at an identity ratio but the `§2.4` drift controller still corrects,
//!   because it works off the QPC PTS vs. accumulated sample count, and the QPC
//!   is the *real* device clock (below), not the 48 kHz nominal.
//! - **QPCPosition IS the master domain** (amended `§2.2`, DECISIONS 2026-07-07
//!   §2.2). `IAudioCaptureClient::GetBuffer`'s `QPCPosition` is valid on this
//!   client even though the clock/position queries are not (OBS 28+ trusts it in
//!   production). We pass it straight into the shared [`PtsDeriver`], identical
//!   to the endpoint path.
//! - **No device-change state machine.** A PID does not follow an endpoint
//!   default, so there is no [`crate::audio::devices::DefaultChangeWatcher`] and
//!   no rebuild-in-place. Instead:
//! - **Process exit is silent, not an error.** When the target process dies the
//!   client simply delivers silence forever with no WASAPI error (`§5`
//!   research), so we run our own PID-liveness watchdog
//!   ([`ProcessHandle`]/[`is_dead`]) and end the capture when the process exits;
//!   the track then goes silent (or is rebound by B3), and the `§2.3`
//!   synthesizer fills the hole downstream.
//! - **Activations are serialized** ([`ACTIVATION_LOCK`]): parallel
//!   `ActivateAudioInterfaceAsync` calls are a known field hazard (they froze
//!   OBS), so at most one activation runs at a time.
//! - **Runtime floor** ([`process_loopback_supported`]): PROCESS_LOOPBACK needs
//!   Windows 10 2004 (build 19041). Below it, capture is refused (track silent)
//!   and B3's spawn gate hides the per-app tracks entirely.
//!
//! ## `unsafe`
//! The WASAPI/COM activation is wrapped by the `wasapi` crate (CLAUDE.md confines
//! `unsafe` to such wrappers). This module's own `unsafe` is confined to two tiny
//! OS calls, each with a `// SAFETY:` note: the PID-liveness handle
//! (`OpenProcess`/`WaitForSingleObject`/`CloseHandle`) and the OS-build probe
//! (`RtlGetVersion`). The decision logic around both ([`is_dead`],
//! [`build_supports_process_loopback`]) is pure and unit-tested. No COM object or
//! handle crosses the thread boundary.

use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use tracing::{info, warn};
use wasapi::{initialize_mta, Direction, SampleType, StreamMode, WaveFormat};

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use super::wasapi_stream::{drain_f32, wa, AudioError, AudioPacket, AudioTrackKind, PtsDeriver};
use crate::spec_constants::audio::{BUFFER_PERIODS, CHANNELS, PERIOD_MS, SAMPLE_RATE_HZ};
use crate::spec_constants::units::TICKS_PER_MILLISECOND;

/// Lowest Windows 10 build that supports application (process) loopback capture.
/// 2004 / 19041 — the Microsoft docs *claim* 20348, but 19041 is the true floor
/// (`M7-M8-PLAN §5`, `HANDOVER §4`); probe at runtime, do not trust the doc.
const MIN_PROCESS_LOOPBACK_BUILD: u32 = 19041;

/// The `initialize_client` buffer duration for a loopback client, in 100 ns units.
/// `get_device_period` is `E_NOTIMPL` on this client (the requested period is
/// irrelevant anyway per the `wasapi` docs), so we pass the nominal `§2.1`
/// 4 × 10 ms of headroom directly instead of querying it.
const LOOPBACK_BUFFER_HNS: i64 = PERIOD_MS * TICKS_PER_MILLISECOND * BUFFER_PERIODS as i64;

/// Serializes `ActivateAudioInterfaceAsync` activations across all process-loopback
/// capture threads: parallel activation spam is a documented field hazard (it froze
/// OBS, `§5`). Held only across the activation call itself; init/start run unlocked.
static ACTIVATION_LOCK: Mutex<()> = Mutex::new(());

// ── Pure decision logic (unit-tested; no COM, no hardware) ────────────────────

/// Whether Windows build `build` supports process-loopback capture. Pure; the
/// runtime probe [`process_loopback_supported`] feeds it the live build number.
pub const fn build_supports_process_loopback(build: u32) -> bool {
    build >= MIN_PROCESS_LOOPBACK_BUILD
}

/// The fixed capture format requested from a process-loopback client: 48 kHz f32
/// stereo (`§2.1` rate/channels). The client cannot report its native format, so
/// this is *requested*, not queried, and WASAPI autoconvert honours it.
fn fixed_capture_format() -> WaveFormat {
    WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE_HZ as usize,
        CHANNELS as usize,
        None,
    )
}

/// Outcome of one zero-timeout `WaitForSingleObject` liveness poll on the target
/// process handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// The process is still running (wait timed out).
    StillAlive,
    /// The process object was signalled — the process has exited.
    Exited,
    /// The wait call itself failed (a bad/closed handle).
    WaitFailed,
}

/// Pure liveness latch: once the target is known dead it stays dead. A
/// `WaitFailed` is treated as dead — a valid `SYNCHRONIZE` handle does not fail a
/// zero-timeout wait unless the underlying process object is gone, so ending the
/// capture (track → silence) is the safe response.
fn is_dead(prev_dead: bool, outcome: WaitOutcome) -> bool {
    prev_dead || matches!(outcome, WaitOutcome::Exited | WaitOutcome::WaitFailed)
}

// ── OS build probe (confined unsafe) ──────────────────────────────────────────

/// The live Windows build number via `RtlGetVersion` (the manifest-independent
/// source — `GetVersionEx` lies without an app manifest we do not ship).
fn os_build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `RtlGetVersion` fills the caller-owned `OSVERSIONINFOW` whose
    // `dwOSVersionInfoSize` we set per its contract; it always returns
    // STATUS_SUCCESS. No pointer escapes `info`.
    unsafe {
        let _ = RtlGetVersion(&mut info);
    }
    info.dwBuildNumber
}

/// Whether this machine supports process-loopback capture (Windows 10 2004+).
/// B3's spawn gate calls this to hide the per-app tracks below the floor; the
/// capture path ([`run_process_capture`]) also refuses defensively.
pub fn process_loopback_supported() -> bool {
    build_supports_process_loopback(os_build())
}

// ── PID-liveness handle (confined unsafe) ─────────────────────────────────────

/// A `SYNCHRONIZE`-access handle to the target process, polled with a zero-timeout
/// wait to detect exit. Owned and dropped on the capture thread; never crosses a
/// thread boundary.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// Open `pid` for synchronization, or `None` if it cannot be opened (already
    /// gone, or insufficient rights) — capture then proceeds without exit
    /// detection (best-effort; the `stop` flag still ends it).
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: `OpenProcess` returns an owned handle we close in `Drop`; we
        // request only `SYNCHRONIZE` (no memory/thread access). A failure is
        // returned as `Err` and mapped to `None`.
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(h) if !h.is_invalid() => Some(Self(h)),
            _ => None,
        }
    }

    /// Poll liveness once (non-blocking).
    fn poll(&self) -> WaitOutcome {
        // SAFETY: `self.0` is a valid handle owned by this struct; a zero timeout
        // makes the wait non-blocking.
        let r = unsafe { WaitForSingleObject(self.0, 0) };
        if r == WAIT_OBJECT_0 {
            WaitOutcome::Exited
        } else if r == WAIT_TIMEOUT {
            WaitOutcome::StillAlive
        } else {
            WaitOutcome::WaitFailed
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by `OpenProcess` and not closed elsewhere.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// ── Capture (hardware) ────────────────────────────────────────────────────────

/// One live process-loopback session — the client objects for one activation.
struct ProcessSession {
    audio_client: wasapi::AudioClient,
    capture_client: wasapi::AudioCaptureClient,
    h_event: wasapi::Handle,
    bytes_per_frame: usize,
}

/// Activate + initialize + start a process-loopback capture session for `pid`.
/// The [`ACTIVATION_LOCK`] is held only across the activation call.
fn open_process_session(pid: u32, include_tree: bool) -> Result<ProcessSession, AudioError> {
    // Serialize the activation only (init/start are per-client and cheap).
    let mut audio_client = {
        let _guard = ACTIVATION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        wasapi::AudioClient::new_application_loopback_client(pid, include_tree).map_err(wa)?
    };

    let format = fixed_capture_format();
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: LOOPBACK_BUFFER_HNS,
    };
    // The loopback client MUST be initialized Capture + Shared (wasapi docs).
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(wa)?;

    let h_event = audio_client.set_get_eventhandle().map_err(wa)?;
    let capture_client = audio_client.get_audiocaptureclient().map_err(wa)?;
    let bytes_per_frame = format.get_blockalign() as usize; // 2ch × 4 bytes = 8
    audio_client.start_stream().map_err(wa)?;

    Ok(ProcessSession {
        audio_client,
        capture_client,
        h_event,
        bytes_per_frame,
    })
}

/// Capture the audio of process `pid`'s tree (`include_tree = true`) or everything
/// *except* that tree (`false`) until `stop` is set or the process exits, emitting
/// [`AudioPacket`]s stamped `kind` to `tx`. Same contract as
/// [`crate::audio::wasapi_stream::run_capture`]'s endpoint path.
///
/// Returns `Ok(())` on every non-fatal end (stop, process exit, unsupported OS, or
/// an activation/read failure): the audio thread then drops its `tx` clone and the
/// track goes silent downstream — never surfaced as an engine error, because the
/// response to "this app's audio is gone" is always the same (silence + a possible
/// B3 rebind), exactly like the endpoint path's device-loss rebuild.
///
/// Runs its own MTA apartment (CLAUDE.md COM rule); owns all its COM objects and
/// the liveness handle, none of which cross the thread boundary.
pub fn run_process_capture(
    kind: AudioTrackKind,
    pid: u32,
    include_tree: bool,
    tx: Sender<AudioPacket>,
    stop: Arc<AtomicBool>,
) -> Result<(), AudioError> {
    initialize_mta().ok().map_err(wa)?;

    if !process_loopback_supported() {
        warn!(
            track = kind.label(),
            pid,
            min_build = MIN_PROCESS_LOOPBACK_BUILD,
            "process loopback unsupported on this Windows build (< 2004) — track will be silent"
        );
        return Ok(());
    }

    // Best-effort PID liveness: without a handle we capture until `stop`, relying
    // on nothing else to notice the process leaving.
    let liveness = ProcessHandle::open(pid);
    if liveness.is_none() {
        warn!(
            track = kind.label(),
            pid, "could not open process for liveness — capturing without exit detection"
        );
    }

    let session = match open_process_session(pid, include_tree) {
        Ok(s) => s,
        Err(e) => {
            // A dead PID or a failed activation: refuse this capture (track silent),
            // do not surface — B3 may rebind to a live PID later.
            warn!(track = kind.label(), pid, error = %e, "process-loopback activation failed — track will be silent");
            return Ok(());
        }
    };

    // Requested 48 kHz (autoconvert), so the deriver runs at the nominal rate; the
    // QPC PTS still reflects the real device clock (amended §2.2).
    let mut deriver = PtsDeriver::new(SAMPLE_RATE_HZ);
    let mut deque: VecDeque<u8> =
        VecDeque::with_capacity(session.bytes_per_frame * SAMPLE_RATE_HZ as usize);
    let mut dead = false;

    info!(
        track = kind.label(),
        pid,
        include_tree,
        rate = SAMPLE_RATE_HZ,
        "process-loopback capture started (f32 stereo @ 48 kHz; QPCPosition master domain §2.2)"
    );

    'capture: while !stop.load(Ordering::Relaxed) {
        // PID-liveness (§5): process exit ⇒ silence forever, no WASAPI error, so
        // detect it ourselves and end the capture (downstream silence-fills).
        if let Some(h) = &liveness {
            dead = is_dead(dead, h.poll());
            if dead {
                info!(
                    track = kind.label(),
                    pid, "target process exited — ending process-loopback capture (§2.2)"
                );
                break 'capture;
            }
        }

        if session.h_event.wait_for_event(200).is_err() {
            continue;
        }
        loop {
            let n = match session.capture_client.get_next_packet_size() {
                Ok(v) => v.unwrap_or(0),
                Err(e) => {
                    warn!(track = kind.label(), pid, error = %e, "process-loopback read error — ending capture");
                    break 'capture;
                }
            };
            if n == 0 {
                break;
            }
            let before = deque.len();
            let info = match session.capture_client.read_from_device_to_deque(&mut deque) {
                Ok(i) => i,
                Err(e) => {
                    warn!(track = kind.label(), pid, error = %e, "process-loopback read error — ending capture");
                    break 'capture;
                }
            };
            let frames = ((deque.len() - before) / session.bytes_per_frame) as u32;
            if frames == 0 {
                continue;
            }

            let pts = deriver.derive(info.timestamp, frames, info.flags.timestamp_error);
            let samples = drain_f32(&mut deque, frames as usize * CHANNELS as usize);
            let packet = AudioPacket {
                stream: kind,
                pts,
                frames,
                sample_rate: SAMPLE_RATE_HZ,
                samples,
                silent: info.flags.silent,
                discontinuity: info.flags.data_discontinuity,
            };
            if tx.send(packet).is_err() {
                break 'capture; // consumer gone
            }
        }
    }

    let _ = session.audio_client.stop_stream();
    info!(
        track = kind.label(),
        pid,
        bad_qpc = deriver.bad_qpc_total(),
        sample_counting = deriver.sample_counting(),
        ts_violations = deriver.ts_violations(),
        "process-loopback capture stopped"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_floor_is_win10_2004() {
        assert!(!build_supports_process_loopback(0));
        assert!(!build_supports_process_loopback(18363)); // 1909, below the floor
        assert!(!build_supports_process_loopback(
            MIN_PROCESS_LOOPBACK_BUILD - 1
        ));
        assert!(build_supports_process_loopback(MIN_PROCESS_LOOPBACK_BUILD)); // 2004
        assert!(build_supports_process_loopback(19045)); // 22H2
        assert!(build_supports_process_loopback(22631)); // Win11 23H2
    }

    #[test]
    fn fixed_format_is_48k_f32_stereo() {
        let f = fixed_capture_format();
        assert_eq!(f.get_samplespersec(), SAMPLE_RATE_HZ);
        assert_eq!(f.get_nchannels(), CHANNELS);
        assert_eq!(f.get_bitspersample(), 32);
        // 2ch × 4 bytes.
        assert_eq!(f.get_blockalign(), CHANNELS as u32 * 4);
    }

    #[test]
    fn liveness_latches_dead_on_exit() {
        // Alive stays alive.
        assert!(!is_dead(false, WaitOutcome::StillAlive));
        // Exit flips to dead.
        assert!(is_dead(false, WaitOutcome::Exited));
        // Once dead, stays dead even if a later poll spuriously reports alive.
        assert!(is_dead(true, WaitOutcome::StillAlive));
    }

    #[test]
    fn liveness_treats_wait_failure_as_dead() {
        // A failed wait (bad handle) is treated as the process being gone.
        assert!(is_dead(false, WaitOutcome::WaitFailed));
        assert!(is_dead(true, WaitOutcome::WaitFailed));
    }

    #[test]
    fn loopback_buffer_is_four_periods() {
        // 4 × 10 ms = 40 ms = 400_000 ticks.
        assert_eq!(LOOPBACK_BUFFER_HNS, 400_000);
    }
}
