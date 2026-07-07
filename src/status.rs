//! `status` — lock-free engine status publishing for the settings-window status
//! strip (M7 Slice A / A4).
//!
//! ## Satellite law direction (`CLAUDE.md` "UI rules")
//! Exactly the A3 shape (`audio::levels`): the engine PUBLISHES, the settings
//! window READS. Direction is strictly engine → UI — this type lives engine-side
//! and the `ui` module only holds a clone of the `Arc` and reads a
//! [`StatusSnapshot`] once per frame. No UI code is referenced from here, and the
//! engine runs fully whether or not the window ever opens.
//!
//! ## Why lock-free (atomics, not a channel or a mutex)
//! Status is a *latest-value* display signal, not a stream of events: the panel
//! only ever wants the most recent value and tolerates one a frame stale. Each
//! scalar is an atomic ([`Ordering::Relaxed`] — the fields are independent display
//! values with no cross-field invariant and gate no other memory), so the engine
//! threads store without ever blocking and the UI loads without ever blocking. It
//! deliberately does NOT route through `ShellSignal` (the tray's single, state-only
//! consumer). The producers span three threads — the ring thread (state, buffer
//! fill, stage counters), the capture thread (resolution, target, dropped frames),
//! and the mux worker (last-save result) — so a shared `Arc<EngineStatus>` cloned
//! into each is the natural fit; nothing needs to become `Sync`.
//!
//! ## Immutable header vs live cells
//! Three fields ([`EngineStatus::adapter`], `fps`, `configured_buffer_seconds`) are
//! known at [`crate::engine::BufferEngine::start`] and never change, so they are
//! plain fields set at construction and read without atomics. Everything else is a
//! live atomic cell published by one of the engine threads.
//!
//! ## Pure math
//! The tick/byte → human mappings and the elapsed-time formatting are pure
//! functions, unit-tested here with the boundary numbers like the other logic
//! modules (`clock`, `ring`, `audio::levels`).

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use crate::engine::TrayState;
use crate::spec_constants::units::TICKS_PER_SECOND;

/// One mebibyte in bytes — the unit the buffer-size readout uses.
const BYTES_PER_MIB: f32 = 1024.0 * 1024.0;

/// What the engine is currently capturing, for the status readout. Distinct from
/// [`crate::capture::wgc::CaptureSource`] (the *requested* source): this is the
/// *resolved* target, which can change mid-session without an epoch (a captured
/// window closing falls the source back to the primary monitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    /// A monitor (the primary monitor, or a window source that fell back to it).
    Monitor,
    /// A specific window.
    Window,
}

impl CaptureTarget {
    /// Stable atomic encoding.
    const fn code(self) -> u8 {
        match self {
            CaptureTarget::Monitor => 0,
            CaptureTarget::Window => 1,
        }
    }

    /// Decode; any unknown value reads as [`CaptureTarget::Monitor`] (the safe
    /// default — never panics on a torn/legacy value).
    const fn from_code(code: u8) -> Self {
        match code {
            1 => CaptureTarget::Window,
            _ => CaptureTarget::Monitor,
        }
    }

    /// A short human label for the panel.
    pub const fn label(self) -> &'static str {
        match self {
            CaptureTarget::Monitor => "Monitor",
            CaptureTarget::Window => "Window",
        }
    }
}

/// The outcome of the most recent save attempt this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// No save has been attempted yet this session.
    None,
    /// The last save wrote a clip successfully.
    Ok,
    /// The last save failed (see the log for why).
    Failed,
}

impl SaveOutcome {
    const fn code(self) -> u8 {
        match self {
            SaveOutcome::None => 0,
            SaveOutcome::Ok => 1,
            SaveOutcome::Failed => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => SaveOutcome::Ok,
            2 => SaveOutcome::Failed,
            _ => SaveOutcome::None,
        }
    }
}

/// Stable atomic encoding for [`TrayState`], so the UI can read the engine state
/// without a channel. Kept here (not on `TrayState`) so the state enum stays a
/// plain UI-neutral type.
const fn state_code(state: TrayState) -> u8 {
    match state {
        TrayState::Buffering => 0,
        TrayState::Paused => 1,
        TrayState::Warning => 2,
        TrayState::Error => 3,
    }
}

/// Decode a [`TrayState`]; any unknown value reads as [`TrayState::Buffering`] (the
/// nominal state — never panics on a torn/legacy value).
const fn state_from_code(code: u8) -> TrayState {
    match code {
        1 => TrayState::Paused,
        2 => TrayState::Warning,
        3 => TrayState::Error,
        _ => TrayState::Buffering,
    }
}

/// A one-shot read of the whole engine status, decoded for the UI to render. Taken
/// with [`EngineStatus::snapshot`] once per frame so the panel draws from a single
/// consistent-enough read rather than re-loading each atomic at its use site.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    /// The current engine state.
    pub state: TrayState,
    /// Buffered footage currently held in the ring, in seconds.
    pub held_seconds: f32,
    /// The configured retention target, in seconds (the ring holds one extra GOP of
    /// margin above this — see the engine's `retained_seconds`).
    pub configured_seconds: u32,
    /// Bytes currently held in the ring.
    pub held_bytes: u64,
    /// The output canvas width in pixels; `0` until the first frame is captured.
    pub width: u32,
    /// The output canvas height in pixels; `0` until the first frame is captured.
    pub height: u32,
    /// The capture frame rate (fps).
    pub fps: u32,
    /// What is being captured (monitor / window).
    pub target: CaptureTarget,
    /// The GPU adapter description (our best runtime vendor/encoder signal — the
    /// H.264 MFT friendly name is not surfaced, and this is the device NVENC runs
    /// on). Empty only if the adapter could not be described. `Arc<str>` so the
    /// per-frame snapshot clones a pointer, not the string.
    pub adapter: Arc<str>,
    /// Grid slots produced by the capture stage.
    pub captured: u64,
    /// Packets emitted by the encoder.
    pub encoded: u64,
    /// Packets written into the ring by the muxer stage.
    pub muxed: u64,
    /// Frames dropped by the pacing grid (superseded arrivals — keep-latest),
    /// cumulative across the whole session (each capture thread accumulates its own
    /// drops into the shared total, so a §7 device-loss respawn keeps the history).
    pub dropped: u64,
    /// The outcome of the most recent save this session.
    pub last_save: SaveOutcome,
    /// When the last save completed, as a Unix-epoch millisecond timestamp (`0`
    /// when [`Self::last_save`] is [`SaveOutcome::None`]). Formatted relative to the
    /// current time by the UI ([`format_elapsed`]) so no timezone handling is
    /// needed.
    pub last_save_unix_ms: u64,
    /// How long the last save took to write, in milliseconds.
    pub last_save_duration_ms: u64,
}

/// Lock-free engine status. Written by the engine's ring / capture / mux-worker
/// threads, read by the settings window. Shared behind an `Arc`; the immutable
/// header ([`adapter`](Self::adapter) / [`fps`](Self::fps) /
/// [`configured_buffer_seconds`](Self::configured_buffer_seconds)) is set once at
/// construction, the atomics are published live.
#[derive(Debug)]
pub struct EngineStatus {
    // ---- immutable header (set at construction, read without atomics) ----
    /// The GPU adapter description (see [`StatusSnapshot::adapter`]).
    pub adapter: Arc<str>,
    /// The capture frame rate.
    pub fps: u32,
    /// The configured retention target in seconds.
    pub configured_buffer_seconds: u32,

    // ---- live cells (published by the engine threads) ----
    state: AtomicU8,
    held_ticks: AtomicU64,
    held_bytes: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    target: AtomicU8,
    captured: AtomicU64,
    encoded: AtomicU64,
    muxed: AtomicU64,
    dropped: AtomicU64,
    last_save_result: AtomicU8,
    last_save_unix_ms: AtomicU64,
    last_save_duration_ms: AtomicU64,
}

impl EngineStatus {
    /// A fresh status with the immutable header filled and every live cell at its
    /// zero/nominal value (state Buffering, no frame yet, no save yet).
    pub fn new(adapter: String, fps: u32, configured_buffer_seconds: u32) -> Self {
        Self {
            adapter: adapter.into(),
            fps,
            configured_buffer_seconds,
            state: AtomicU8::new(state_code(TrayState::Buffering)),
            held_ticks: AtomicU64::new(0),
            held_bytes: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            target: AtomicU8::new(CaptureTarget::Monitor.code()),
            captured: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            muxed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_save_result: AtomicU8::new(SaveOutcome::None.code()),
            last_save_unix_ms: AtomicU64::new(0),
            last_save_duration_ms: AtomicU64::new(0),
        }
    }

    /// Publish the engine state (ring thread — at each transition it already signals
    /// the tray; the supervisor publishes `Error` on a fatal teardown).
    pub fn set_state(&self, state: TrayState) {
        self.state.store(state_code(state), Ordering::Relaxed);
    }

    /// Publish the current ring fill (ring thread — polled on the watchdog tick).
    /// A negative duration (never expected) clamps to zero.
    pub fn set_fill(&self, held_ticks: i64, held_bytes: u64) {
        self.held_ticks
            .store(held_ticks.max(0) as u64, Ordering::Relaxed);
        self.held_bytes.store(held_bytes, Ordering::Relaxed);
    }

    /// Publish the stage counters (ring thread — polled on the watchdog tick, from
    /// the shared [`crate::watchdog::PipelineStats`]).
    pub fn set_stage_counts(&self, captured: u64, encoded: u64, muxed: u64) {
        self.captured.store(captured, Ordering::Relaxed);
        self.encoded.store(encoded, Ordering::Relaxed);
        self.muxed.store(muxed, Ordering::Relaxed);
    }

    /// Publish the output canvas size (capture thread — once the canvas is known,
    /// and again after a monitor fall-back rebuild).
    pub fn set_resolution(&self, width: u32, height: u32) {
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
    }

    /// Publish what is being captured (capture thread — at start, and on a
    /// window→monitor fall-back).
    pub fn set_target(&self, target: CaptureTarget) {
        self.target.store(target.code(), Ordering::Relaxed);
    }

    /// Add `delta` newly-dropped frames to the shared session total (capture thread).
    /// A *delta*, not a set, because each epoch's capture thread owns a fresh pacing
    /// grid whose drop count restarts at zero on a §7 device-loss respawn — a `store`
    /// of the new grid's (smaller) absolute count would silently erase the prior
    /// epochs' drops. Accumulating each thread's own increments keeps the total
    /// genuinely session-cumulative across rebuilds.
    pub fn add_dropped(&self, delta: u64) {
        if delta > 0 {
            self.dropped.fetch_add(delta, Ordering::Relaxed);
        }
    }

    /// Publish the outcome of a completed save (mux worker). `unix_ms` is the
    /// completion wall-clock time; `duration_ms` is how long the write took.
    pub fn set_last_save(&self, outcome: SaveOutcome, unix_ms: u64, duration_ms: u64) {
        // Store the timestamp/duration before the result so a reader that observes
        // the new result tends to see its matching time. `Relaxed` gives no portable
        // cross-field ordering guarantee — this pairing holds in practice on the
        // project's sole target (x86-64 TSO does not reorder these stores between
        // cores), and a display anyway tolerates the one-frame stale pairing that a
        // weaker model could produce, self-correcting on the next snapshot.
        self.last_save_unix_ms.store(unix_ms, Ordering::Relaxed);
        self.last_save_duration_ms
            .store(duration_ms, Ordering::Relaxed);
        self.last_save_result
            .store(outcome.code(), Ordering::Relaxed);
    }

    /// A one-shot decoded read of the whole status for the UI.
    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            state: state_from_code(self.state.load(Ordering::Relaxed)),
            held_seconds: ticks_to_seconds(self.held_ticks.load(Ordering::Relaxed)),
            configured_seconds: self.configured_buffer_seconds,
            held_bytes: self.held_bytes.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
            fps: self.fps,
            target: CaptureTarget::from_code(self.target.load(Ordering::Relaxed)),
            adapter: self.adapter.clone(),
            captured: self.captured.load(Ordering::Relaxed),
            encoded: self.encoded.load(Ordering::Relaxed),
            muxed: self.muxed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            last_save: SaveOutcome::from_code(self.last_save_result.load(Ordering::Relaxed)),
            last_save_unix_ms: self.last_save_unix_ms.load(Ordering::Relaxed),
            last_save_duration_ms: self.last_save_duration_ms.load(Ordering::Relaxed),
        }
    }
}

/// Convert a ring duration in 100 ns ticks to seconds. Pure.
pub fn ticks_to_seconds(ticks: u64) -> f32 {
    ticks as f32 / TICKS_PER_SECOND as f32
}

/// Convert a byte count to mebibytes. Pure.
pub fn bytes_to_mib(bytes: u64) -> f32 {
    bytes as f32 / BYTES_PER_MIB
}

/// The fill fraction (`0.0..=1.0`) of `held_seconds` against a `configured` target.
/// A zero (or absent) target reads empty rather than dividing by zero. Pure.
pub fn fill_fraction(held_seconds: f32, configured: u32) -> f32 {
    if configured == 0 {
        return 0.0;
    }
    (held_seconds / configured as f32).clamp(0.0, 1.0)
}

/// Format a wall-clock elapsed span (`now_unix_ms − event_unix_ms`) as a compact
/// relative label: "just now", "N s ago", "N m ago", "N h ago". Pure, so the UI's
/// "last save …" line is unit-tested without a clock. A future timestamp (clock
/// skew) reads as "just now" rather than a negative span (the caller passes a
/// saturating difference).
pub fn format_elapsed(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs} s ago")
    } else if secs < 3600 {
        format!("{} m ago", secs / 60)
    } else {
        format!("{} h ago", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn ticks_to_seconds_uses_the_spec_tick_rate() {
        assert!(close(ticks_to_seconds(TICKS_PER_SECOND as u64), 1.0, 1e-6));
        assert!(close(ticks_to_seconds(0), 0.0, 1e-6));
        // 30 s of buffer.
        assert!(close(
            ticks_to_seconds(30 * TICKS_PER_SECOND as u64),
            30.0,
            1e-4
        ));
    }

    #[test]
    fn bytes_to_mib_boundaries() {
        assert!(close(bytes_to_mib(0), 0.0, 1e-6));
        assert!(close(bytes_to_mib(1024 * 1024), 1.0, 1e-6));
        assert!(close(bytes_to_mib(3 * 1024 * 1024 / 2), 1.5, 1e-6));
    }

    #[test]
    fn fill_fraction_clamps_and_guards_zero() {
        assert!(close(fill_fraction(15.0, 30), 0.5, 1e-6));
        assert!(close(fill_fraction(0.0, 30), 0.0, 1e-6));
        // Full and over-full both clamp to 1.0 (the ring holds a GOP of margin
        // above the configured target, so held can briefly exceed it).
        assert!(close(fill_fraction(30.0, 30), 1.0, 1e-6));
        assert!(close(fill_fraction(31.0, 30), 1.0, 1e-6));
        // A zero target never divides by zero.
        assert_eq!(fill_fraction(10.0, 0), 0.0);
    }

    #[test]
    fn format_elapsed_thresholds() {
        assert_eq!(format_elapsed(0), "just now");
        assert_eq!(format_elapsed(4_999), "just now");
        assert_eq!(format_elapsed(5_000), "5 s ago");
        assert_eq!(format_elapsed(59_000), "59 s ago");
        assert_eq!(format_elapsed(60_000), "1 m ago");
        assert_eq!(format_elapsed(3_599_000), "59 m ago");
        assert_eq!(format_elapsed(3_600_000), "1 h ago");
        assert_eq!(format_elapsed(7_200_000), "2 h ago");
    }

    #[test]
    fn state_code_roundtrips_every_variant() {
        for s in [
            TrayState::Buffering,
            TrayState::Paused,
            TrayState::Warning,
            TrayState::Error,
        ] {
            assert_eq!(state_from_code(state_code(s)), s);
        }
        // An out-of-range code reads as the nominal state, never panics.
        assert_eq!(state_from_code(200), TrayState::Buffering);
    }

    #[test]
    fn target_code_roundtrips() {
        assert_eq!(
            CaptureTarget::from_code(CaptureTarget::Monitor.code()),
            CaptureTarget::Monitor
        );
        assert_eq!(
            CaptureTarget::from_code(CaptureTarget::Window.code()),
            CaptureTarget::Window
        );
        assert_eq!(CaptureTarget::from_code(200), CaptureTarget::Monitor);
    }

    #[test]
    fn save_outcome_code_roundtrips() {
        for o in [SaveOutcome::None, SaveOutcome::Ok, SaveOutcome::Failed] {
            assert_eq!(SaveOutcome::from_code(o.code()), o);
        }
        assert_eq!(SaveOutcome::from_code(200), SaveOutcome::None);
    }

    #[test]
    fn publish_then_snapshot_roundtrips() {
        let st = EngineStatus::new("Test Adapter".to_string(), 60, 30);
        // Fresh: nominal.
        let s = st.snapshot();
        assert_eq!(s.state, TrayState::Buffering);
        assert_eq!(s.width, 0);
        assert_eq!(s.last_save, SaveOutcome::None);
        assert_eq!(s.fps, 60);
        assert_eq!(s.configured_seconds, 30);
        assert_eq!(&*s.adapter, "Test Adapter");

        st.set_state(TrayState::Paused);
        st.set_fill(15 * TICKS_PER_SECOND, 4 * 1024 * 1024);
        st.set_stage_counts(100, 98, 97);
        st.set_resolution(1920, 1080);
        st.set_target(CaptureTarget::Window);
        st.add_dropped(3);
        st.set_last_save(SaveOutcome::Ok, 1_700_000_000_000, 85);

        let s = st.snapshot();
        assert_eq!(s.state, TrayState::Paused);
        assert!(close(s.held_seconds, 15.0, 1e-3));
        assert!(close(bytes_to_mib(s.held_bytes), 4.0, 1e-6));
        assert_eq!((s.captured, s.encoded, s.muxed), (100, 98, 97));
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.target, CaptureTarget::Window);
        assert_eq!(s.dropped, 3);
        assert_eq!(s.last_save, SaveOutcome::Ok);
        assert_eq!(s.last_save_unix_ms, 1_700_000_000_000);
        assert_eq!(s.last_save_duration_ms, 85);
    }

    #[test]
    fn add_dropped_accumulates_across_epochs() {
        // Two capture threads (an epoch rebuild between them) each publish their own
        // grid's incremental drops; the shared total must be the sum, not the last
        // grid's absolute count. Epoch 1 drops 5 (deltas 2, then 3); a device loss
        // respawns a fresh grid (drops restart at 0) that then drops 4 (delta 4).
        let st = EngineStatus::new(String::new(), 60, 30);
        st.add_dropped(2);
        st.add_dropped(3);
        assert_eq!(st.snapshot().dropped, 5);
        // New epoch's fresh grid: its first published delta is 4 (0 → 4), which must
        // ADD to the running total, not reset it.
        st.add_dropped(4);
        assert_eq!(st.snapshot().dropped, 9);
        // A zero delta (the common per-frame case) is a no-op.
        st.add_dropped(0);
        assert_eq!(st.snapshot().dropped, 9);
    }

    #[test]
    fn set_fill_clamps_negative_ticks() {
        let st = EngineStatus::new(String::new(), 30, 10);
        st.set_fill(-5, 0);
        assert!(close(st.snapshot().held_seconds, 0.0, 1e-6));
    }

    #[test]
    fn engine_status_is_send_and_sync() {
        // The Arc is shared between the engine threads and the UI thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EngineStatus>();
    }
}
