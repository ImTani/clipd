//! `ui::settings` — the egui/eframe settings window (M7 Slice A / A2, skeleton).
//!
//! ## Satellite law (`CLAUDE.md` "UI rules")
//! Lazily created from the tray. Runs [`eframe::run_native`] on its OWN thread
//! (spawned on the first open) so the tray's main-thread Win32 pump is untouched
//! — message queues are per-thread on Windows, and winit is told to accept a
//! non-main-thread event loop via `with_any_thread(true)`. The only engine
//! coupling is a clone of [`EngineCommand`]'s sender; the engine never links
//! against or blocks on this module, and the tray runs fully if the window is
//! never opened. Dependency direction is `ui → engine`, never the reverse.
//!
//! ## Why a persistent hidden window instead of open/close
//! winit permits exactly ONE `EventLoop` per process, so re-running
//! [`eframe::run_native`] after a close would panic. Instead the window's close
//! request (the `X`) is intercepted — cancelled, then the viewport is hidden
//! ([`egui::ViewportCommand::Visible`]) — and the tray "Settings…" item re-shows
//! it. The event loop and its thread live until the process quits, when the tray
//! calls [`SettingsHandle::shutdown`], which sets the quit flag so the next frame
//! lets the close through and then joins the thread.
//!
//! The window shows the live status strip (A4) + VU meters (A3), the settings
//! *editor* (A5) with press-to-bind hotkeys (A6), and a recent-clips list (A7). The
//! editor covers quality tier, resolution, fps, buffer length, output folder,
//! clear-after-save, desktop audio, and mic policy. The
//! editor loads the current config on open and writes edits exclusively through the
//! A1 `Config::write_atomic` path (the single config representation, same as
//! `--check-config`); the one field safe to hot-apply (clear-after-save) is pushed
//! over [`EngineCommand`], the rest — including hotkeys, which re-register at
//! startup — are reported as restart-required (DECISIONS "A5"/"A6").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use eframe::egui;
use tracing::{info, warn};

use super::recent::RecentClips;
use super::theme;
use super::window_state::{self, WindowState};
use crate::audio::devices::{enumerate_capture_devices, AudioDevice, DeviceSelection};
use crate::audio::levels::{self, AudioLevels, StreamMeter};
use crate::audio::wasapi_stream::AudioTrackKind;
use crate::config::{self, Config, Quality, Resolution};
use crate::engine::{EngineCommand, TrayState};
use crate::hotkey::{parse_hotkey, Availability, HotkeyControl, HotkeyRole, RebindOutcome};
use crate::spec_constants::encoder::video_target_bitrate_bps;
use crate::spec_constants::ring::{
    byte_cap_bytes, est_bitrate_bps, IDR_INTERVAL_SECONDS, MAX_BUFFER_SECONDS,
};
use crate::spec_constants::PRODUCT_NAME;
use crate::status::{self, EngineStatus, SaveOutcome, StatusSnapshot};

/// The window's inner size at first open (logical points). A comfortable size for
/// the A5 editor to grow into without being cramped in the A2 skeleton.
const WINDOW_SIZE: [f32; 2] = [560.0, 440.0];

/// The window's **minimum** inner size (U6 / D-U5). The floor is set by the widest fixed
/// row — the hotkey row (a 150 px combo field + the Rebind button + the longest
/// availability note "⚠ in use by another app") comes to ≈ 400 px of content plus the
/// card/panel margins, so 440 wide renders it in full without a horizontal clip. Height
/// 340 shows the header + the first card without feeling cramped. Reversible: drop the
/// `with_min_inner_size` call → today's clip-on-shrink behaviour.
const MIN_WINDOW_SIZE: [f32; 2] = [440.0, 340.0];

/// Bounded wait for the settings-window thread to close on shutdown before
/// detaching. The window normally closes within a frame or two; a longer stall
/// means it is wedged in a native modal loop (e.g. mid drag/resize), and since the
/// process is exiting we detach rather than hang app exit on a second thread.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Repaint cadence while the window is visible: the VU meters animate, so we drive
/// ~30 fps repaints (atomic level writes do not wake egui on their own). Gated on
/// visibility so a hidden (closed-to-tray) window idles at zero CPU.
const METER_REFRESH: Duration = Duration::from_millis(33);

/// Horizontal room a section card leaves for the group frame's margin + stroke so its
/// right edge lines up with its left (see [`card`]).
const CARD_INSET: f32 = 14.0;

/// How long the editor waits on the pump thread for a live hotkey rebind reply (T2b)
/// before treating it as [`RebindOutcome::Unknown`]. A rebind is a couple of quick OS
/// calls on the (woken) pump thread; this bounds a wedged/absent pump. Runs on the
/// settings-UI thread only (a user-initiated, infrequent commit — never the engine).
const HOTKEY_REBIND_TIMEOUT: Duration = Duration::from_millis(300);

/// One VU meter row's bar height in logical points.
const METER_HEIGHT: f32 = 18.0;
/// Bar corner radius.
const METER_RADIUS: f32 = 3.0;
/// Fixed width of a meter row's right-aligned label column (T3): constant so every bar
/// starts at the same x regardless of the label text ("Other system" is the longest).
const METER_LABEL_WIDTH: f32 = 104.0;
/// Readout refresh period (T3): the numeric dB updates at ~3 Hz (decoupled from the bar's
/// display-rate ballistics) so the digits are legible, showing the interval peak.
const METER_READOUT_INTERVAL: f32 = 1.0 / 3.0;

/// State shared between the tray thread and the settings-window thread. The tray
/// thread stashes/reads the window's [`egui::Context`] (to drive show/close from
/// outside the event loop) and sets [`Shared::quit`] to bring the window down.
struct Shared {
    /// A clone of the window's context, published when the window thread starts
    /// (from the `CreationContext`, before the first frame). `None` only in the
    /// microscopic window before the app-creator runs; the tray reads it to send
    /// viewport commands cross-thread.
    ctx: Mutex<Option<egui::Context>>,
    /// Set by the tray to permit the next close request to actually close (the
    /// window otherwise hides on close). Drives a clean process-quit teardown.
    quit: AtomicBool,
    /// Whether the window is currently shown. The single source of truth for
    /// whether the app schedules meter-animation repaints: the app clears it when
    /// it intercepts a close (hide-to-tray), the tray sets it on re-show. Gating
    /// the animation on this (not on an inferred per-frame heuristic) means a stale
    /// repaint that fires just after a hide sees `false` and lets the loop idle,
    /// rather than resurrecting a hidden window into a 30 fps spin.
    visible: AtomicBool,
    /// Set by the tray on each re-show so the app re-scans the recent-clips list
    /// (A7): the window persists hidden across opens, so clips saved while it was
    /// hidden would otherwise be missing until the user hit Refresh. The app swaps it
    /// back to `false` when it consumes it.
    rescan_recent: AtomicBool,
    /// Set by the tray on each re-show so the app re-enumerates the mic device list
    /// (B3.5): a mic plugged/unplugged while the window was hidden should appear on
    /// the next open. Same swap-to-consume pattern as [`Shared::rescan_recent`].
    rescan_devices: AtomicBool,
    /// Set by the auto-restart banner's **Restart now** button (U7). The tray polls it
    /// via [`SettingsHandle::restart_requested`] and, when set, tears down and returns
    /// [`super::ShellOutcome::Restart`] so `main.rs` relaunches the process. UI signals
    /// *intent* over shared state; the process spawn stays in `main` (satellite law).
    restart: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            ctx: Mutex::new(None),
            quit: AtomicBool::new(false),
            // The window opens visible on creation.
            visible: AtomicBool::new(true),
            // The first scan happens in `RecentClips::new`; only re-shows re-scan.
            rescan_recent: AtomicBool::new(false),
            // The first enumeration happens in `Editor::load`; only re-shows re-scan.
            rescan_devices: AtomicBool::new(false),
            // Only the banner's Restart-now button sets this.
            restart: AtomicBool::new(false),
        }
    }

    /// Read the published context, tolerating a poisoned lock (never panics — this
    /// runs off the main thread; `CLAUDE.md` bars `unwrap` in worker paths).
    fn context(&self) -> Option<egui::Context> {
        self.ctx.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn publish_context(&self, ctx: egui::Context) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }
}

/// Handle to the lazily-spawned settings window. Owned by the tray [`super::Shell`].
/// The window is created on the first [`open`](Self::open); later opens just
/// re-show the existing window.
#[derive(Default)]
pub struct SettingsHandle {
    running: Option<Running>,
    /// Set once the window thread has exited unexpectedly: winit permits only one
    /// event loop per process, so we never respawn — Settings stays disabled for
    /// the rest of the session (logged when it happens).
    disabled: bool,
}

/// The live settings-window thread + the state shared with it.
struct Running {
    thread: JoinHandle<()>,
    shared: Arc<Shared>,
}

impl SettingsHandle {
    /// Open the settings window, or re-show it if already open. The first call
    /// spawns the UI thread and its eframe event loop; subsequent calls just make
    /// the (hidden-on-close) window visible and focused again. Cheap and
    /// non-blocking either way — the engine is never touched.
    pub fn open(
        &mut self,
        cmd_tx: &Sender<EngineCommand>,
        levels: &Arc<AudioLevels>,
        streams: &[AudioTrackKind],
        status: &Arc<EngineStatus>,
        output_dir: &Path,
        hotkey_ctl: &HotkeyControl,
    ) {
        if self.disabled {
            return;
        }
        if let Some(running) = &self.running {
            if running.thread.is_finished() {
                // The window thread exited unexpectedly (e.g. `run_native` failed to
                // create the window / GL context on a VM, RDP session, or restrictive
                // driver). winit permits only one event loop per process, so we do NOT
                // respawn — disable Settings for this session and say why (the trust
                // model depends on the log answering "why didn't my window open").
                warn!("settings-window thread exited; disabling Settings for this session");
                self.running = None;
                self.disabled = true;
                return;
            }
            // Already open: re-show via the published context. Set the shared
            // visibility flag first so the woken frame resumes meter animation, and
            // ask the app to re-scan the recent-clips list (A7 — clips may have been
            // saved while the window was hidden).
            running.shared.visible.store(true, Ordering::Relaxed);
            running.shared.rescan_recent.store(true, Ordering::Relaxed);
            running.shared.rescan_devices.store(true, Ordering::Relaxed);
            match running.shared.context() {
                Some(ctx) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                    info!("settings window re-shown");
                }
                // The app-creator has not run yet (a click in the first instant after
                // spawn) — the window opens visible on creation, so it is on its way.
                None => info!("settings window opening"),
            }
            return;
        }

        let shared = Arc::new(Shared::new());
        let opened_at = Instant::now();
        let thread = {
            let shared = shared.clone();
            let cmd_tx = cmd_tx.clone();
            let levels = levels.clone();
            let streams = streams.to_vec();
            let status = status.clone();
            let output_dir = output_dir.to_path_buf();
            let hotkey_ctl = hotkey_ctl.clone();
            std::thread::Builder::new()
                .name("settings-ui".to_string())
                .spawn(move || {
                    run_window(
                        shared, cmd_tx, levels, streams, status, output_dir, hotkey_ctl, opened_at,
                    )
                })
                .ok()
        };
        match thread {
            Some(thread) => {
                self.running = Some(Running { thread, shared });
                info!("settings window opening");
            }
            None => warn!("could not spawn the settings-window thread"),
        }
    }

    /// Whether the settings window's auto-restart banner asked to relaunch (U7). Read
    /// each poll by the tray loop; `false` if the window was never opened. Cheap atomic
    /// load — no lock.
    pub fn restart_requested(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.shared.restart.load(Ordering::Relaxed))
    }

    /// Tear the window down (tray Quit / session end): set the quit flag, wake the
    /// event loop, and join the thread within a bound. A no-op if the window was
    /// never opened.
    ///
    /// The `quit` flag is authoritative: the app polls it every frame and closes
    /// itself (see [`SettingsApp::logic`]); the `Close` command + `request_repaint`
    /// here just wake an idle/hidden loop so it acts this frame. The join is bounded
    /// by [`SHUTDOWN_JOIN_TIMEOUT`] so a window wedged in a native modal loop (mid
    /// drag/resize) cannot stall process exit — on timeout we detach.
    pub fn shutdown(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let Running { thread, shared } = running;
        shared.quit.store(true, Ordering::Relaxed);
        if let Some(ctx) = shared.context() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        while Instant::now() < deadline {
            if thread.is_finished() {
                if thread.join().is_err() {
                    warn!("settings-window thread panicked on shutdown");
                } else {
                    info!("settings window closed");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Detach: the process is exiting, so the OS reclaims the thread.
        warn!(timeout = ?SHUTDOWN_JOIN_TIMEOUT, "settings window did not close in time; detaching");
    }
}

impl Drop for SettingsHandle {
    fn drop(&mut self) {
        // Ensure the UI thread never outlives the tray (e.g. an early return path
        // that forgot to call `shutdown`).
        self.shutdown();
    }
}

/// Run the eframe event loop on the current (settings-ui) thread until the window
/// is closed for real (tray quit). Any eframe error is logged, not propagated —
/// the tray and engine keep running regardless (satellite law).
#[allow(clippy::too_many_arguments)]
fn run_window(
    shared: Arc<Shared>,
    cmd_tx: Sender<EngineCommand>,
    levels: Arc<AudioLevels>,
    streams: Vec<AudioTrackKind>,
    status: Arc<EngineStatus>,
    output_dir: PathBuf,
    hotkey_ctl: HotkeyControl,
    opened_at: Instant,
) {
    // Restore the saved geometry (A4/T7), clamped to the CURRENT virtual screen so a window
    // last placed on a now-unplugged monitor can't reopen off-screen. Falls back to the
    // default size on first run / a missing file. See [`window_state`] for why this is a
    // ui-state file rather than eframe's built-in persistence.
    window_state::log_location();
    let saved = window_state::load();
    let init_size = saved
        .map(|s| {
            [
                s.width.max(MIN_WINDOW_SIZE[0]),
                s.height.max(MIN_WINDOW_SIZE[1]),
            ]
        })
        .unwrap_or(WINDOW_SIZE);
    let init_pos = saved.and_then(|s| match (s.x, s.y) {
        (Some(x), Some(y)) => Some(window_state::clamp_to_virtual_screen(
            x,
            y,
            init_size[0],
            init_size[1],
        )),
        _ => None,
    });
    let maximized = saved.map(|s| s.maximized).unwrap_or(false);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("{PRODUCT_NAME} settings"))
        .with_inner_size(init_size)
        // A minimum size so the window can't be dragged smaller than its widest row
        // (U6): the page scrolls vertically only, so horizontal overflow would just
        // clip. See [`MIN_WINDOW_SIZE`].
        .with_min_inner_size(MIN_WINDOW_SIZE)
        // Identify the window in the taskbar / Alt-Tab / title bar with the same
        // procedural glyph the tray uses (U1); zero new dep — reuses the rasteriser.
        .with_icon(theme::window_icon());
    if let Some((x, y)) = init_pos {
        viewport = viewport.with_position(egui::pos2(x, y));
    }
    if maximized {
        viewport = viewport.with_maximized(true);
    }
    let mut native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    // Windows: allow the winit event loop to run off the main thread (the tray owns
    // the main thread's message pump). Per-thread message queues keep the two apart.
    native_options.event_loop_builder = Some(Box::new(|builder| {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }));

    let app_shared = shared.clone();
    let result = eframe::run_native(
        PRODUCT_NAME,
        native_options,
        Box::new(move |cc| {
            // Install the fonts, type scale, control sizing, and forced-dark accented
            // visuals once at creation (U1 / D-U1 + the research redesign). The palette is
            // calculated against egui's dark surfaces, so this must win over a light theme.
            theme::install(&cc.egui_ctx);
            // Publish the context synchronously (it exists on the `CreationContext`
            // before the first frame) so the tray can drive show/close without
            // racing on a first render.
            app_shared.publish_context(cc.egui_ctx.clone());
            Ok(Box::new(SettingsApp::new(
                app_shared, cmd_tx, levels, streams, status, output_dir, hotkey_ctl, opened_at,
            )) as Box<dyn eframe::App>)
        }),
    );
    if let Err(e) = result {
        warn!(error = %e, "settings window event loop ended with an error");
    }
}

/// The smoothed display state for one stream's VU meter. The bar snaps up to a new
/// level instantly (attack) and decays toward it between frames (release), so the
/// 30 fps redraw reads smoothly against the ~100 Hz level publish.
struct MeterState {
    kind: AudioTrackKind,
    /// Displayed RMS bar fill (0..=1), decayed.
    display_rms: f32,
    /// Displayed peak marker position (0..=1), decayed.
    display_peak: f32,
    /// The peak amplitude seen since the last readout publish — the numeric dB readout
    /// is DECOUPLED from the fast bar and shows this interval peak, refreshed at ~3 Hz
    /// so the digits are readable rather than a blur (T3).
    interval_peak: f32,
    /// The peak amplitude currently shown in the readout (held between publishes).
    readout_peak: f32,
    /// Seconds accumulated toward the next ~3 Hz readout publish.
    since_readout: f32,
}

impl MeterState {
    fn new(kind: AudioTrackKind) -> Self {
        Self {
            kind,
            display_rms: 0.0,
            display_peak: 0.0,
            interval_peak: 0.0,
            readout_peak: 0.0,
            since_readout: 0.0,
        }
    }

    /// Whether this is a secondary (per-app) track — Game / Voice chat / Other system —
    /// shown with a muted label under the primary Microphone + Mix (T3).
    fn is_secondary(&self) -> bool {
        matches!(
            self.kind,
            AudioTrackKind::Game | AudioTrackKind::VoiceChat | AudioTrackKind::OtherSystem
        )
    }
}

/// The Audio-levels row order (T3): the microphone first ("is my mic live?" is the
/// highest-value answer), then the Mix, then the per-app tracks. Lower sorts earlier.
fn meter_display_order(kind: AudioTrackKind) -> u8 {
    match kind {
        AudioTrackKind::Mic => 0,
        AudioTrackKind::Mix => 1,
        AudioTrackKind::Game => 2,
        AudioTrackKind::VoiceChat => 3,
        AudioTrackKind::OtherSystem => 4,
    }
}

/// The egui application backing the settings window. A3 adds the VU meters over
/// the A2 skeleton; the editor (A5) still writes only via `Config::write_atomic`.
struct SettingsApp {
    /// Shared with the tray thread (context handoff + quit + visibility flags).
    shared: Arc<Shared>,
    /// Engine command channel — the editor (A5) uses it to hot-apply the one safe
    /// live field (clear-after-save); also held for A6's hotkey rebinds.
    cmd_tx: Sender<EngineCommand>,
    /// Lock-free audio levels published by the engine's audio threads (A3). Read
    /// only; never written here (engine → UI).
    levels: Arc<AudioLevels>,
    /// Lock-free engine status published by the engine's ring/capture/mux threads
    /// (A4). Read only; never written here (engine → UI).
    status: Arc<EngineStatus>,
    /// The settings editor (A5): a draft config edited in place, written via
    /// `Config::write_atomic`.
    editor: Editor,
    /// The recent-clips list (A7): last ~20 saved clips + open/reveal/copy actions.
    recent: RecentClips,
    /// One animated meter per enabled audio stream, in engine order.
    meters: Vec<MeterState>,
    /// When the tray requested the open — used once to log the cold-open latency
    /// against the M7 < 300 ms budget.
    opened_at: Instant,
    /// Whether the first-frame one-time work (the cold-open log) has run.
    started: bool,
    /// The pending-restart set the user last dismissed with the banner's **Later**
    /// button (U7). The banner re-appears when the set changes (i.e. a further
    /// restart-bearing save), because a new save makes `pending` differ from this.
    /// `None` = never dismissed.
    restart_banner_dismissed: Option<Vec<&'static str>>,
    /// The latest observed window geometry (A4/T7), captured each frame and persisted when
    /// the window is hidden-to-tray or the app quits. Holds the last NON-maximized restore
    /// rect (so re-opening a maximized-then-closed window restores to a sane size), plus
    /// the maximized flag. `None` until the first frame reports a rect.
    geometry: Option<WindowState>,
}

impl SettingsApp {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shared: Arc<Shared>,
        cmd_tx: Sender<EngineCommand>,
        levels: Arc<AudioLevels>,
        streams: Vec<AudioTrackKind>,
        status: Arc<EngineStatus>,
        output_dir: PathBuf,
        hotkey_ctl: HotkeyControl,
        opened_at: Instant,
    ) -> Self {
        // Build one meter per stream, ordered for display (Microphone first — T3), not in
        // the engine's container order (Mix 0 … Mic 4).
        let mut meters: Vec<MeterState> = streams.into_iter().map(MeterState::new).collect();
        meters.sort_by_key(|m| meter_display_order(m.kind));
        let recent = RecentClips::new(output_dir);
        // Load the current config to seed the editor (A5). Reads the same
        // `%APPDATA%\clipd\config.toml` the engine started from; a missing/invalid
        // file falls back to defaults, so the form is always populated.
        let editor = Editor::load(config::default_config_path(), Some(hotkey_ctl));
        Self {
            shared,
            cmd_tx,
            levels,
            status,
            editor,
            recent,
            meters,
            opened_at,
            started: false,
            restart_banner_dismissed: None,
            geometry: None,
        }
    }

    /// The engine's EFFECTIVE clips directory (T5): the committed config's output folder,
    /// resolved the same way the engine resolves it. The recent-clips list scans this — not
    /// a possibly-stale value threaded at startup — so the list + empty state always name
    /// the folder the user's setting actually points at (fixes the reported D:\Clips vs.
    /// the configured folder mismatch).
    fn effective_output_dir(&self) -> PathBuf {
        config::resolve_output_dir(&self.editor.base.output.dir)
    }

    /// Capture the current window geometry into [`Self::geometry`] (A4/T7). When the window
    /// is maximized we keep the last NON-maximized restore rect and only set the flag, so a
    /// maximized-then-closed window reopens at a sane size rather than as a full-screen
    /// rectangle (the "sane post-maximized-close" guard). A frame that has no rect yet
    /// (before the window is placed) is ignored.
    fn capture_geometry(&mut self, ctx: &egui::Context) {
        let Some((width, height, x, y, maximized)) = ctx.input(|i| {
            let vp = i.viewport();
            let inner = vp.inner_rect?;
            let maximized = vp.maximized.unwrap_or(false);
            let (x, y) = match vp.outer_rect {
                Some(r) => (Some(r.min.x), Some(r.min.y)),
                None => (None, None),
            };
            Some((inner.width(), inner.height(), x, y, maximized))
        }) else {
            return;
        };
        if maximized {
            match &mut self.geometry {
                // Keep the prior restore rect; only remember that we closed maximized.
                Some(prev) => prev.maximized = true,
                // Never seen un-maximized (opened maximized): store what we have.
                None => {
                    self.geometry = Some(WindowState {
                        width,
                        height,
                        x,
                        y,
                        maximized: true,
                    })
                }
            }
        } else {
            self.geometry = Some(WindowState {
                width,
                height,
                x,
                y,
                maximized: false,
            });
        }
    }

    /// Persist the last-captured geometry to the ui-state file (A4/T7). A no-op if no rect
    /// has been observed yet.
    fn persist_geometry(&self) {
        if let Some(g) = &self.geometry {
            window_state::save(g);
        }
    }

    /// The pinned auto-restart banner (U7): names the accumulated pending restart-bearing
    /// changes and offers a one-click **Restart now** (signals the tray to relaunch) plus
    /// a quiet **Later** (dismiss until the set changes). Accent-filled. Drawn outside the
    /// scroll so it stays visible.
    fn draw_restart_banner(&mut self, ui: &mut egui::Ui, pending: &[&'static str]) {
        ui.add_space(6.0);
        // Line 1: what needs a restart. Line 2: the honest cost — a restart discards the
        // replay buffer you have right now, so the moment you were about to clip is lost
        // (T2b). "Later" is a first-class choice: everything else already applied live, so
        // deferring the restart keeps you buffering with the current quality.
        ui.label(
            egui::RichText::new(format!("⟳ Restart to apply {}.", pending.join(", ")))
                .color(theme::ACCENT),
        );
        ui.label(
            egui::RichText::new(
                "Restarting clears the replay you have buffered right now — save any clip \
                 you want to keep first.",
            )
            .weak(),
        );
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let restart =
                egui::Button::new(egui::RichText::new("Restart now").color(theme::ON_FILL))
                    .fill(theme::ACCENT_FILL);
            if ui.add(restart).clicked() {
                // Signal intent only; the tray tears down and `main.rs` spawns the fresh
                // instance after the hotkeys/devices are released (satellite law).
                self.shared.restart.store(true, Ordering::Relaxed);
                ui.ctx().request_repaint();
            }
            if ui
                .button("Later")
                .on_hover_text(
                    "Keep buffering with your current settings. Your other changes already \
                     applied; only the ones listed wait for a restart.",
                )
                .clicked()
            {
                self.restart_banner_dismissed = Some(pending.to_vec());
            }
        });
        ui.add_space(4.0);
    }
}

impl eframe::App for SettingsApp {
    /// Non-drawing per-frame logic (eframe 0.35 splits this from [`Self::ui`]):
    /// the one-time cold-open log, close interception, and scheduling the next
    /// animation repaint while visible.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            // Cold-open latency (M7 acceptance: < 300 ms) — tray click to first
            // rendered frame. Measured on hardware.
            let ms = self.opened_at.elapsed().as_secs_f64() * 1000.0;
            info!(cold_open_ms = ms, "settings window first frame");
        }

        // Re-scan the recent-clips list if the tray flagged a re-show (A7). Point it at the
        // engine's EFFECTIVE clips dir (T5) so clips saved while hidden — and any live
        // output-folder change — are reflected. Swap so we consume the request once.
        if self.shared.rescan_recent.swap(false, Ordering::Relaxed) {
            let dir = self.effective_output_dir();
            self.recent.rescan_in(dir);
        }

        // Re-enumerate the mic device list on a re-show (B3.5), so a mic hot-plugged
        // while the window was hidden shows up. Same swap-to-consume as above.
        if self.shared.rescan_devices.swap(false, Ordering::Relaxed) {
            self.editor.refresh_devices();
        }

        // Pick up any completed live hotkey-availability probe (A6 fast-follow). Cheap
        // when nothing is in flight; while visible the meter cadence already repaints,
        // so the result shows within a frame.
        self.editor.poll_availability();

        // Track the window geometry each frame so it can be persisted on hide/quit (A4/T7).
        self.capture_geometry(ctx);

        // Close handling. The tray's quit flag is authoritative: when set, close the
        // window for real (ending the event loop and this thread). Otherwise a
        // user-initiated close (the `X`) is intercepted — cancelled, then hidden — so
        // the window can be re-shown, since winit permits only one event loop per
        // process and we never recreate it. See the module docs.
        if self.shared.quit.load(Ordering::Relaxed) {
            self.persist_geometry(); // remember size/pos for next launch
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            self.persist_geometry(); // save on hide-to-tray, not just on quit
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            // Stop animating: a hidden window must idle at zero CPU. The tray sets
            // this back on re-show.
            self.shared.visible.store(false, Ordering::Relaxed);
            return;
        }

        // Drive the meter animation while visible. Gating on the shared flag (not on
        // an inferred heuristic) keeps a stale post-hide repaint from resurrecting
        // the loop — it sees `false` here and simply lets egui idle.
        if self.shared.visible.load(Ordering::Relaxed) {
            ctx.request_repaint_after(METER_REFRESH);
        }
    }

    /// Draw the window contents. eframe hands a root [`egui::Ui`]; the restart banner is
    /// a pinned bottom panel (outside the scroll, so it never scrolls away — U7 §7.2) and
    /// the rest is a central scroll area.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let dt = ui.input(|i| i.stable_dt);
        // One status read per frame — feeds the recording pill + the Debug expander.
        let snap = self.status.snapshot();

        // Auto-restart banner (U7): shown once there is a committed-but-not-applied
        // restart-bearing change, and not dismissed for exactly this set.
        let pending = self.editor.pending_restart_fields();
        // `show_collapsible` takes `&mut bool` (it drives the open/close animation); we
        // recompute the visibility from state each frame, so any panel-side change is
        // harmlessly overwritten next frame.
        let mut banner_visible = !pending.is_empty()
            && self.restart_banner_dismissed.as_deref() != Some(pending.as_slice());
        egui::Panel::bottom("restart_banner")
            .show_separator_line(true)
            .show_collapsible(ui, &mut banner_visible, |ui| {
                self.draw_restart_banner(ui, &pending);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            // `auto_shrink([false, false])` so the scroll area always reserves the FULL
            // panel width (after its own gutter) — every card then flexes to a stable
            // width, so a meter row's fixed label + readout columns can't be squeezed into
            // clipping the dB text (T3: the dB-clip bug is impossible by construction).
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.heading(format!("{PRODUCT_NAME} settings"));
                    ui.label(egui::RichText::new(format!("version {VERSION}")).weak());

                    // First-run orientation (U-P2b): what the app is doing + how to save.
                    // Read from the DRAFT so it live-updates the moment the instant-replay
                    // length or the save hotkey changes — before the edit even commits (T4).
                    ui.add_space(4.0);
                    ui.label(first_run_line(
                        &self.editor.draft.hotkeys.save_clip,
                        self.editor.draft.buffer.seconds,
                    ));
                    ui.add_space(10.0);

                    // A live recording pill (U8) — the one piece of "status" that IS
                    // user-facing, kept visible while a timed recording runs (the rest of the
                    // telemetry moved to the Debug expander below, UI-RESEARCH F3).
                    if snap.recording {
                        ui.horizontal(|ui| {
                            ui.colored_label(theme::BAD, "●");
                            ui.label(format!(
                                "Recording — {}",
                                record_elapsed_mmss(snap.record_started_unix_ms)
                            ));
                        });
                        ui.add_space(10.0);
                    }

                    // VU meters FIRST (U-P1a): "is my mic recording?" is the highest-value
                    // answer, so it sits directly under the header, above Status.
                    section(ui, "Audio levels", |ui| {
                        if self.meters.is_empty() {
                            ui.label("No audio streams are enabled.");
                        } else {
                            for meter in &mut self.meters {
                                let StreamMeter { peak, rms } = self.levels.level(meter.kind);
                                // Bar: fast display-rate ballistics (unchanged).
                                let rms_target = levels::linear_to_fraction(rms);
                                let peak_target = levels::linear_to_fraction(peak);
                                meter.display_rms =
                                    levels::smooth_toward(meter.display_rms, rms_target, dt);
                                meter.display_peak =
                                    levels::smooth_toward(meter.display_peak, peak_target, dt);
                                // Readout: decoupled from the bar — accumulate the interval
                                // peak and publish it at ~3 Hz so the digits are legible (T3).
                                meter.interval_peak = meter.interval_peak.max(peak);
                                meter.since_readout += dt;
                                if meter.since_readout >= METER_READOUT_INTERVAL {
                                    meter.readout_peak = meter.interval_peak;
                                    meter.interval_peak = 0.0;
                                    meter.since_readout = 0.0;
                                }
                                draw_meter(
                                    ui,
                                    meter.kind.title(),
                                    meter.is_secondary(),
                                    meter.display_rms,
                                    meter.display_peak,
                                    meter.readout_peak,
                                );
                                ui.add_space(6.0);
                            }
                        }
                    });

                    section(ui, "Settings", |ui| self.editor.draw(ui, &self.cmd_tx));

                    // Recent clips draws its own heading + Refresh button, so use a plain
                    // card (no section title) to avoid a doubled heading.
                    let effective_dir = self.effective_output_dir();
                    card(ui, |ui| self.recent.draw(ui, &effective_dir));

                    // Live telemetry (engine state, capture target, GPU, buffer fill, frame
                    // counters, last save) lives behind a collapsed "Debug information"
                    // disclosure — kept for power users but out of the average user's view
                    // (UI-RESEARCH F3: status does not belong in the Settings body).
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Debug information")
                        .default_open(false)
                        .show(ui, |ui| draw_status(ui, &snap));
                    ui.add_space(4.0);
                });
        });
    }
}

/// Wrap a section body in a quiet group frame that spans the available width (U-P2a):
/// framing, not chrome — a subtle boundary per section instead of a bare heading +
/// separator. Full-width so the cards flex with the window (U6).
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        // Leave room for the group's own margin + stroke so the card's RIGHT edge lines
        // up with its left (#1 — `set_width(available_width())` alone overflows the frame
        // margin on the right, giving the asymmetric "margin on the left, none on the
        // right" look).
        ui.set_width((ui.available_width() - CARD_INSET).max(0.0));
        add(ui);
    });
    ui.add_space(8.0);
}

/// A [`card`] that leads with a section `title` heading (Audio / Status / Settings).
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    card(ui, |ui| {
        ui.heading(title);
        ui.add_space(6.0);
        add(ui);
    });
}

/// The first-run orientation line (U-P2b): what the app is doing + the save hotkey +
/// the buffer length. Pure over the two config values, so it is unit-testable.
fn first_run_line(save_hotkey: &str, buffer_seconds: u32) -> String {
    format!(
        "{PRODUCT_NAME} keeps your last {} ready. Press {} to save a clip.",
        format_buffer_len(buffer_seconds),
        save_hotkey.trim(),
    )
}

/// The recording elapsed as `M:SS` from a start Unix-ms stamp, relative to now (U8).
/// Reads the wall clock here (UI thread); the pure `M:SS` formatting is [`format_mmss`].
fn record_elapsed_mmss(started_unix_ms: u64) -> String {
    if started_unix_ms == 0 {
        return format_mmss(0);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format_mmss(now.saturating_sub(started_unix_ms) / 1000)
}

/// Format an elapsed second count as `M:SS` (minutes uncapped). Pure + unit-tested.
fn format_mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A human buffer length: seconds under a minute, else whole/fractional minutes. Pure.
fn format_buffer_len(seconds: u32) -> String {
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

/// Draw the engine status strip (A4): state, capture target + format, buffer fill,
/// stage/dropped counters, and the last-save result. Values come from a one-shot
/// [`StatusSnapshot`]; the derived text/fraction mappings are pure (`crate::status`)
/// and unit-tested there. The section heading is drawn by the enclosing [`section`].
fn draw_status(ui: &mut egui::Ui, s: &StatusSnapshot) {
    // Engine state, with a colour dot matching the tray palette.
    ui.horizontal(|ui| {
        let (label, color) = state_display(s.state);
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(12.0, METER_HEIGHT), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, color);
        ui.label(format!("State: {label}"));
    });

    // Capture target + output format. Before the first frame the canvas is unknown.
    if s.width == 0 {
        ui.label("Capture: starting…");
    } else {
        ui.label(format!(
            "Capture: {} · {}×{} @ {} fps · H.264",
            s.target.label(),
            s.width,
            s.height,
            s.fps,
        ));
    }
    if !s.adapter.is_empty() {
        ui.label(format!("Encoder GPU: {}", s.adapter));
    }

    ui.add_space(4.0);

    // Buffer fill: seconds held vs configured, plus current RAM, with a bar.
    ui.label(format!(
        "Buffer: {:.1} s / {} s held · {:.1} MiB",
        s.held_seconds,
        s.configured_seconds,
        status::bytes_to_mib(s.held_bytes),
    ));
    draw_status_bar(
        ui,
        status::fill_fraction(s.held_seconds, s.configured_seconds),
    );

    ui.add_space(4.0);

    // Pipeline stage counters (the §6.3 watchdog signal) + dropped frames. De-emphasised
    // (U-P2e): a developer trust signal, not a first-look element for a non-technical
    // tester — kept available but visually quiet.
    ui.label(
        egui::RichText::new(format!(
            "Frames: captured {} · encoded {} · muxed {}",
            s.captured, s.encoded, s.muxed,
        ))
        .weak(),
    )
    .on_hover_text(
        "captured = pacing-grid slots produced · encoded = encoder outputs · muxed = \
         packets written to the replay buffer.",
    );
    // Split the old single "dropped" into the honest two (T8): pacing skips are expected on
    // a high-refresh panel; only late drops (encoder behind) indicate a real problem.
    ui.label(
        egui::RichText::new(format!(
            "skipped (pacing) {} · dropped (late) {}",
            s.skipped, s.dropped,
        ))
        .weak(),
    )
    .on_hover_text(
        "skipped (pacing): frames coalesced because your display refreshes faster than the \
         capture rate — normal, nothing is lost from clips. dropped (late): frames lost \
         because the encoder fell behind — if this climbs, the machine can't keep up.",
    );

    // Last save result, relative to now.
    ui.label(last_save_line(s));
}

/// A state's label + dot colour, from the value-harmonised semantic palette (`theme`).
/// Stays semantic (green/amber/orange/red) — the lavender brand accent is reserved for
/// the tray glyph + the buffer-fill bar; the status dot still means *state*.
fn state_display(state: TrayState) -> (&'static str, egui::Color32) {
    match state {
        // Healthy/buffering carries the brand accent (not green — #4); the warm colours
        // are reserved for the states that actually want your eye.
        TrayState::Buffering => ("buffering", theme::ACCENT),
        TrayState::Paused => ("paused", theme::AMBER),
        TrayState::Warning => ("warning", theme::WARN),
        TrayState::Error => ("error", theme::BAD),
    }
}

/// A thin filled progress bar for the buffer fill, with the VU meter's theme-adaptive
/// recessed track.
fn draw_status_bar(ui: &mut egui::Ui, fraction: f32) {
    // Flex with the window (U6): grow up to a comfortable max, never exceed the available
    // width, and the min-window floor keeps it above the 80 px minimum.
    let width = ui.available_width().clamp(80.0, 640.0);
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let track_bg = ui.visuals().extreme_bg_color;
    let painter = ui.painter();
    painter.rect_filled(rect, METER_RADIUS, track_bg);
    let f = fraction.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * f);
        // The one hand-painted accent (U2): the buffer-fill bar is lavender, not green.
        painter.rect_filled(fill, METER_RADIUS, theme::ACCENT);
    }
}

/// The "Last save: …" line: outcome + a relative time (and the write duration on
/// success). "none this session" until the first save is attempted.
fn last_save_line(s: &StatusSnapshot) -> String {
    match s.last_save {
        SaveOutcome::None => "Last save: none this session".to_string(),
        SaveOutcome::Ok => format!(
            "Last save: OK {} ({} ms)",
            elapsed_label(s.last_save_unix_ms),
            s.last_save_duration_ms,
        ),
        SaveOutcome::Failed => format!("Last save: failed {}", elapsed_label(s.last_save_unix_ms)),
    }
}

/// Format a stored Unix-ms save time relative to now ("12 s ago"). Reads the wall
/// clock here (the UI thread) and defers the pure bucketing to `crate::status`. A
/// future timestamp (clock skew) saturates to zero → "just now".
fn elapsed_label(unix_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    status::format_elapsed(now.saturating_sub(unix_ms))
}

/// Green / red used for the editor's save-result line + the hotkey availability badges.
/// Aliased to the value-harmonised semantic palette (`theme`) so they are defined once.
const OK_GREEN: egui::Color32 = theme::GOOD;
const ERR_RED: egui::Color32 = theme::BAD;

/// The mic-device selection, decoded from/encoded to the `audio.mic` config string
/// (`"default-follow"` / `"off"` / a pinned endpoint id). The picker (B3.5) offers the
/// two policies plus one entry per enumerated capture device; the config *encoding* is
/// unchanged from A5 (a device is still stored as its endpoint id), so this is a
/// presentation-only change — no schema bump.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MicChoice {
    /// Chase the Windows default capture device.
    Follow,
    /// No microphone track.
    Off,
    /// A specific endpoint id.
    Pinned(String),
}

impl MicChoice {
    fn from_cfg(mic: &str) -> Self {
        match mic.trim() {
            "default-follow" => MicChoice::Follow,
            "off" => MicChoice::Off,
            other => MicChoice::Pinned(other.to_string()),
        }
    }

    /// The `audio.mic` string for this choice. A pinned id is trimmed; an empty
    /// pinned id round-trips to `""`, which `Config::validate` rejects with the
    /// `audio.mic must be …` error the editor surfaces.
    fn to_cfg(&self) -> String {
        match self {
            MicChoice::Follow => "default-follow".to_string(),
            MicChoice::Off => "off".to_string(),
            MicChoice::Pinned(id) => id.trim().to_string(),
        }
    }

    /// The combo's selected-text label. A pinned id is resolved to its friendly name
    /// when the device is present in `devices`; a pin that is not enumerable right now
    /// (unplugged, or hand-set in TOML) is shown as `Unavailable: <id>` rather than
    /// silently masked — the user must see that their pinned device is missing (`§7`:
    /// never pretend a gone device is fine).
    fn label(&self, devices: &[AudioDevice]) -> String {
        match self {
            MicChoice::Follow => "Default (follow)".to_string(),
            MicChoice::Off => "Off (no mic)".to_string(),
            MicChoice::Pinned(id) if id.trim().is_empty() => "Select a device…".to_string(),
            MicChoice::Pinned(id) => match devices.iter().find(|d| d.id == id.trim()) {
                Some(d) => d.name.clone(),
                None => format!("Unavailable: {}", id.trim()),
            },
        }
    }
}

/// One entry in the mic picker dropdown: the [`MicChoice`] it selects + its display
/// label. Built by [`mic_options`].
struct MicOption {
    choice: MicChoice,
    label: String,
}

/// Build the mic picker's dropdown options from the enumerated capture devices and the
/// current selection. Always leads with **Default (follow)** and **Off**; then one
/// entry per live device (label = friendly name); then, if `current` pins an id that is
/// **not** among the live devices (unplugged, or hand-set in TOML), a trailing
/// `Unavailable: <id>` entry so opening Settings never silently drops or substitutes a
/// saved pin (`§7`). Pure + unit-tested; `devices` is the only HW-sourced input.
fn mic_options(devices: &[AudioDevice], current: &MicChoice) -> Vec<MicOption> {
    let mut out = vec![
        MicOption {
            choice: MicChoice::Follow,
            label: "Default (follow)".to_string(),
        },
        MicOption {
            choice: MicChoice::Off,
            label: "Off (no mic)".to_string(),
        },
    ];
    for d in devices {
        out.push(MicOption {
            choice: MicChoice::Pinned(d.id.clone()),
            label: d.name.clone(),
        });
    }
    if let MicChoice::Pinned(id) = current {
        let id = id.trim();
        if !id.is_empty() && !devices.iter().any(|d| d.id == id) {
            out.push(MicOption {
                choice: MicChoice::Pinned(id.to_string()),
                label: format!("Unavailable: {id}"),
            });
        }
    }
    out
}

/// Whether an `audio.mic` config string selects an active mic (any value other than
/// `"off"`). A change that flips this is a track-topology change (the Mic track is
/// added/removed) and stays restart-required (T2b); a change between two active
/// selections is a live device swap.
fn mic_is_on(mic: &str) -> bool {
    mic.trim() != "off"
}

/// Whether a mic change from `old` to `new` is a live **device swap** — both sides
/// active (`Default-follow` ↔ pinned ↔ another pinned), so it rides the `§7` in-stream
/// rebuild with no restart (T2b). An on↔off flip returns `false` (restart-required).
fn mic_is_device_swap(old: &str, new: &str) -> bool {
    mic_is_on(old) && mic_is_on(new)
}

/// The settings editor (A5). Holds a draft [`Config`] the widgets edit in place; on
/// Save it validates and writes through [`Config::write_atomic`] — the single config
/// representation, same typed path as `--check-config`. The one field safe to
/// hot-apply (`clear_after_save`) is pushed to the engine over [`EngineCommand`];
/// the rest need an epoch/encoder rebuild and are reported as restart-required
/// (DECISIONS "A5").
struct Editor {
    /// The config as last loaded/saved — the baseline for naming which fields
    /// changed (to list the restart-required ones) and the previous hot-swap value.
    base: Config,
    /// The config the **running engine** started from (U5/U7). Seeded at window creation
    /// and only advanced by an actual restart — so it is the anchor for both the inline
    /// per-field "needs restart" chips (draft vs `applied`) and the auto-restart banner's
    /// accumulated pending set (committed `base` vs `applied`). See DECISIONS "D-U7" for
    /// the one accepted limitation (a prior-session save without a restart under-reports).
    applied: Config,
    /// The working copy the widgets edit.
    draft: Config,
    /// Mic selection, decoded from `draft.audio.mic` for the picker; re-encoded into
    /// the draft on Save.
    mic: MicChoice,
    /// The enumerated capture (microphone) endpoints backing the mic dropdown (B3.5).
    /// Filled by [`enumerate_capture_devices`] on load and re-filled on a window
    /// re-show (via [`Editor::refresh_devices`]); empty in unit tests / on COM failure.
    mic_devices: Vec<AudioDevice>,
    /// Which hotkey (if any) is currently in press-to-bind capture mode (A6). The
    /// next valid combo pressed is written into the draft; Esc cancels.
    capturing: Option<HotkeyTarget>,
    /// Control handle for the live "combo already taken" check (A6 fast-follow).
    /// `None` when no pump is running (unit tests) — the check is simply skipped.
    hotkey_ctl: Option<HotkeyControl>,
    /// The in-flight availability probe: which row it is for + the reply receiver,
    /// polled each frame in [`Editor::poll_availability`]. At most one at a time.
    hotkey_check: Option<(HotkeyTarget, Receiver<Availability>)>,
    /// The last availability result per row (indexed by [`HotkeyTarget::idx`]), shown
    /// beside the binding. `None` = not yet checked this session.
    hotkey_avail: [Option<Availability>; 2],
    /// Where config is read from / written to (`%APPDATA%\clipd\config.toml`).
    path: PathBuf,
    /// The last **error** from an apply-on-change commit (T2), shown inline; `None` when
    /// the last write succeeded. There is no success message — settings just apply.
    last_result: Option<Result<String, String>>,
    /// Set by any field that completed an edit this frame (T2); consumed at the end of
    /// [`Editor::draw`] to write-through the config. Apply-on-change replaces the Save button.
    dirty: bool,
    /// The live edit buffer for the output-folder text field (T2). Kept separate from the
    /// committed `draft.output.dir` so a partial path being typed never triggers a write /
    /// folder creation — the field commits into `draft.output.dir` only on focus loss.
    folder_text: String,
}

/// Which hotkey a press-to-bind capture targets (A6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    /// The save-clip hotkey (`[hotkeys].save_clip`).
    Save,
    /// The record-toggle hotkey (`[hotkeys].record_toggle`).
    Record,
}

impl HotkeyTarget {
    /// This target's slot in [`Editor::hotkey_avail`].
    fn idx(self) -> usize {
        match self {
            HotkeyTarget::Save => 0,
            HotkeyTarget::Record => 1,
        }
    }

    /// The other row (Save ↔ Record) — used to detect a cross-row duplicate binding.
    fn other(self) -> HotkeyTarget {
        match self {
            HotkeyTarget::Save => HotkeyTarget::Record,
            HotkeyTarget::Record => HotkeyTarget::Save,
        }
    }

    /// The pump-side [`HotkeyRole`] this row rebinds (T2b live-apply).
    fn role(self) -> HotkeyRole {
        match self {
            HotkeyTarget::Save => HotkeyRole::Save,
            HotkeyTarget::Record => HotkeyRole::Record,
        }
    }

    /// The human label of this row (matches the labels drawn in `draw_hotkeys`).
    fn label(self) -> &'static str {
        match self {
            HotkeyTarget::Save => "Save clip",
            HotkeyTarget::Record => "Record toggle",
        }
    }
}

impl Editor {
    /// Load the config at `path` to seed the form; a missing or invalid file falls
    /// back to defaults so the form is always populated (an invalid file surfaces its
    /// error only when the user next Saves — we never silently overwrite on open).
    fn load(path: PathBuf, hotkey_ctl: Option<HotkeyControl>) -> Self {
        let base = if path.exists() {
            Config::load(&path).unwrap_or_default()
        } else {
            Config::default()
        };
        let mic = MicChoice::from_cfg(&base.audio.mic);
        let folder_text = base.output.dir.clone();
        Self {
            draft: base.clone(),
            // The engine started from this same on-disk config, so `applied` == `base`
            // at load; it diverges only as the user saves without restarting.
            applied: base.clone(),
            base,
            mic,
            // Enumerate the capture devices once on open; a re-show re-enumerates
            // (`refresh_devices`) so hot-plugged mics appear. HW-sourced, so it is
            // empty in the `test_editor` path.
            mic_devices: enumerate_capture_devices(),
            capturing: None,
            hotkey_ctl,
            hotkey_check: None,
            hotkey_avail: [None, None],
            folder_text,
            path,
            last_result: None,
            dirty: false,
        }
    }

    /// Kick off a live availability probe for `combo` on `target` (A6 fast-follow):
    /// clear the stale result and, if a pump handle exists, send the request. The
    /// reply is picked up later by [`Self::poll_availability`] — never blocks here.
    fn start_availability_check(&mut self, target: HotkeyTarget, combo: &str) {
        self.hotkey_avail[target.idx()] = None;
        if let Some(ctl) = &self.hotkey_ctl {
            self.hotkey_check = Some((target, ctl.check(combo)));
        }
    }

    /// Poll the in-flight availability probe (called once per frame). Stores the
    /// result when it arrives; a disconnected channel (pump gone) resolves to
    /// [`Availability::Unknown`]. A no-op when nothing is in flight.
    fn poll_availability(&mut self) {
        let Some((target, rx)) = &self.hotkey_check else {
            return;
        };
        let target = *target;
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Availability::Unknown,
        };
        self.hotkey_avail[target.idx()] = Some(result);
        self.hotkey_check = None;
    }

    /// Re-enumerate the capture devices for the mic dropdown (B3.5), called when the
    /// window is re-shown so a mic plugged/unplugged since the last open is reflected.
    /// The current [`MicChoice`] is untouched — only the option list + how a pinned id
    /// renders (name vs `Unavailable: …`) change.
    fn refresh_devices(&mut self) {
        self.mic_devices = enumerate_capture_devices();
    }

    /// Draw the editor. Two-tier progressive disclosure (UI-RESEARCH F4): a short
    /// **Essentials** set + a collapsed **Advanced** section. Settings **apply on change**
    /// (T2 / A1) — there is no Save button; each field write-through when its edit
    /// completes, and restart-bearing changes surface only in the bottom banner.
    fn draw(&mut self, ui: &mut egui::Ui, cmd_tx: &Sender<EngineCommand>) {
        // Consume an in-flight press-to-bind capture once, before either hotkey row.
        self.process_capture(ui);

        self.draw_essentials(ui);

        ui.add_space(8.0);

        // Advanced — everything a first-timer never needs, collapsed by default.
        egui::CollapsingHeader::new("Advanced")
            .default_open(false)
            .show(ui, |ui| self.draw_advanced(ui));

        // Apply-on-change: write through whenever a field completed an edit this frame.
        if self.dirty {
            self.autocommit(cmd_tx);
            self.dirty = false;
        }
        // Inline validation error only — a successful apply is silent.
        if let Some(Err(err)) = &self.last_result {
            ui.add_space(4.0);
            ui.colored_label(ERR_RED, format!("Couldn't apply — {err}"));
        }
    }

    /// Consume an in-flight press-to-bind hotkey capture (A6): the next valid combo
    /// pressed is written into the draft; Esc cancels. Called once per frame before the
    /// hotkey rows. Note: the OS-global hotkey stays registered while capturing, so
    /// pressing the CURRENT save/record combo still fires the real action — an accepted
    /// v0 limitation (DECISIONS "A6").
    fn process_capture(&mut self, ui: &mut egui::Ui) {
        if let Some(target) = self.capturing {
            match ui.input_mut(capture_combo) {
                Some(CaptureResult::Cancel) => self.capturing = None,
                Some(CaptureResult::Bound(combo)) => {
                    match target {
                        HotkeyTarget::Save => self.draft.hotkeys.save_clip = combo.clone(),
                        HotkeyTarget::Record => self.draft.hotkeys.record_toggle = combo.clone(),
                    }
                    self.start_availability_check(target, &combo);
                    self.capturing = None;
                    self.dirty = true; // apply-on-change (T2)
                }
                None => {}
            }
        }
        // Keep repainting while a capture is armed so key events are processed promptly.
        if self.capturing.is_some() {
            ui.ctx().request_repaint();
        }
    }

    /// The **Essentials** grid (UI-RESEARCH F4): the four things an average user actually
    /// sets — instant-replay length, quality, microphone, save folder — plus the save-clip
    /// hotkey. Terminology is plain-language (F1/F2): "instant replay length" not "buffer",
    /// a quality preset with no Mbps, "save clips to" not "output folder".
    fn draw_essentials(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings_essentials")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                // Instant replay length. Commit on drag end / edit end, not per-increment.
                ui.label("Instant replay length").on_hover_text(
                    "How many seconds of play are kept ready to clip. Press your save \
                     hotkey to keep the last this-many seconds. Longer needs more memory.",
                );
                let r = ui.add(
                    egui::DragValue::new(&mut self.draft.buffer.seconds)
                        .range(1..=MAX_BUFFER_SECONDS)
                        .suffix(" s"),
                );
                if r.drag_stopped() || r.lost_focus() {
                    self.dirty = true;
                }
                ui.end_row();

                // Quality (a preset — bitrate stays hidden; UI-RESEARCH F2).
                ui.label("Quality").on_hover_text(
                    "Higher quality looks sharper and makes bigger files. Default suits most.",
                );
                let mut changed = false;
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(quality_label(self.draft.encode.quality))
                    .show_ui(ui, |ui| {
                        let q = &mut self.draft.encode.quality;
                        changed |= ui
                            .selectable_value(q, Quality::Efficient, "Efficient")
                            .changed();
                        changed |= ui
                            .selectable_value(q, Quality::Default, "Default")
                            .changed();
                        changed |= ui.selectable_value(q, Quality::High, "High").changed();
                        changed |= ui.selectable_value(q, Quality::Max, "Max").changed();
                    });
                self.dirty |= changed;
                ui.end_row();

                // Microphone.
                ui.label("Microphone").on_hover_text(
                    "Which mic to record. \"Default\" follows Windows; \"Off\" records no \
                     mic. A pinned device shows \"Unavailable\" if it's unplugged.",
                );
                // Build the option list + selected label before `show_ui` so the closure's
                // only borrow of `self` is the `self.mic` write on click.
                let current_label = self.mic.label(&self.mic_devices);
                let options = mic_options(&self.mic_devices, &self.mic);
                let mut mic_changed = false;
                egui::ComboBox::from_id_salt("mic")
                    .selected_text(current_label)
                    .show_ui(ui, |ui| {
                        for opt in options {
                            if ui
                                .selectable_label(self.mic == opt.choice, opt.label.as_str())
                                .clicked()
                            {
                                self.mic = opt.choice;
                                mic_changed = true;
                            }
                        }
                    });
                self.dirty |= mic_changed;
                ui.end_row();

                // Save clips to. The text field edits `folder_text` and commits into the
                // draft only on focus loss, so a partial path being typed never triggers a
                // write / folder creation (T2).
                ui.label("Save clips to").on_hover_text(
                    "Where clips are written. Leave blank for your Videos\\clipd folder; \
                     it's created when it's first used.",
                );
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.folder_text)
                            .hint_text("Videos\\clipd"),
                    );
                    if r.lost_focus() && self.folder_text != self.draft.output.dir {
                        self.draft.output.dir = self.folder_text.clone();
                        self.dirty = true;
                    }
                    if ui.button("Browse…").clicked() {
                        if let Some(dir) = super::folder_dialog::pick_folder() {
                            let s = dir.to_string_lossy().into_owned();
                            self.folder_text = s.clone();
                            self.draft.output.dir = s;
                            self.dirty = true;
                        }
                    }
                });
                ui.end_row();

                // The Hotkeys pair (T4): both the save and the record on/off hotkey live in
                // Essentials — the record hotkey is no longer buried in Advanced. Both
                // rebind live (T2b), so a change never raises the restart banner.
                self.hotkey_row(ui, HotkeyTarget::Save, "Save-clip hotkey");
                self.hotkey_row(ui, HotkeyTarget::Record, "Record on/off hotkey");
            });
    }

    /// The collapsed **Advanced** section: resolution, frame rate, game/app audio, clear-
    /// after-save, and the (quiet) resource estimate. Rarely touched, so deferred behind a
    /// disclosure (UI-RESEARCH F4). The record hotkey moved up to the Essentials Hotkeys
    /// pair (T4).
    fn draw_advanced(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings_advanced")
            .num_columns(3)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Resolution").on_hover_text(
                    "Video sharpness. \"Source\" matches your screen; lower downscales to \
                     save space.",
                );
                let mut changed = false;
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(resolution_label(self.draft.encode.resolution))
                    .show_ui(ui, |ui| {
                        let res = &mut self.draft.encode.resolution;
                        changed |= ui
                            .selectable_value(res, Resolution::Native, "Source (native)")
                            .changed();
                        changed |= ui
                            .selectable_value(res, Resolution::P1440, "1440p")
                            .changed();
                        changed |= ui
                            .selectable_value(res, Resolution::P1080, "1080p")
                            .changed();
                        changed |= ui.selectable_value(res, Resolution::P720, "720p").changed();
                    });
                self.dirty |= changed;
                ui.end_row();

                ui.label("Frame rate")
                    .on_hover_text("Motion smoothness. 60 is smoother; 30 saves space.");
                let mut changed = false;
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(format!("{} fps", self.draft.capture.fps))
                    .show_ui(ui, |ui| {
                        // 30/60 only; 120 stays gated behind M6 (M7-M8-PLAN §3 / §1.2).
                        let fps = &mut self.draft.capture.fps;
                        changed |= ui.selectable_value(fps, 30, "30 fps").changed();
                        changed |= ui.selectable_value(fps, 60, "60 fps").changed();
                    });
                self.dirty |= changed;
                ui.end_row();

                ui.label("Record game & app sound").on_hover_text(
                    "Include system/game audio (your default playback device) in clips.",
                );
                if ui.checkbox(&mut self.draft.audio.desktop, "").changed() {
                    self.dirty = true;
                }
                ui.end_row();

                ui.label("Start fresh after each clip").on_hover_text(
                    "After saving, clear the replay so the next clip starts clean. Applies \
                     immediately.",
                );
                if ui
                    .checkbox(&mut self.draft.buffer.clear_after_save, "")
                    .changed()
                {
                    self.dirty = true;
                }
                ui.end_row();
                // (The record on/off hotkey moved to the Essentials Hotkeys pair — T4.)
            });

        // A quiet resource estimate — kept out of the essentials view. Mbps lives here
        // (advanced), never on the default screen (UI-RESEARCH F2).
        let mbps = estimate_video_mbps(
            self.draft.encode.resolution,
            self.draft.capture.fps,
            self.draft.encode.quality,
        );
        let ram = estimate_ram_mib(
            self.draft.buffer.seconds,
            self.draft.capture.fps,
            self.draft.encode.quality,
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
                "≈ {mbps:.0} Mbps video · replay uses ≈ {ram:.0} MiB RAM"
            ))
            .weak(),
        );
    }

    /// One hotkey row: label + either the "press a combo…" prompt (capturing) or an
    /// editable combo field + a Rebind button + the live availability note.
    ///
    /// Two ways to set a binding: **Rebind** (press-to-bind) is the quick path for free
    /// combos, but a combo already claimed as a global hotkey by ANOTHER app is
    /// swallowed by Windows and never reaches this window — press-to-bind can't catch
    /// it. So the current binding is also an editable text field: the user can type
    /// such a combo directly, and the same live check then reports it as taken. Bad
    /// input is caught on Save by `validate_hotkeys`.
    fn hotkey_row(&mut self, ui: &mut egui::Ui, target: HotkeyTarget, label: &str) {
        ui.label(label);
        // Wrapped so the availability note ("⚠ in use by another app") drops below the
        // field + Rebind button on a narrow window instead of clipping (U6).
        ui.horizontal_wrapped(|ui| {
            if self.capturing == Some(target) {
                ui.label(egui::RichText::new("press a combo…  (Esc cancels)").italics());
                ui.label(
                    egui::RichText::new(
                        "— a combo another app owns can't be caught; type it below",
                    )
                    .weak(),
                );
                return;
            }

            // Editable combo field. Rebind fills it for free combos; typing covers the
            // OS-claimed ones press-to-bind can't capture.
            let field = match target {
                HotkeyTarget::Save => &mut self.draft.hotkeys.save_clip,
                HotkeyTarget::Record => &mut self.draft.hotkeys.record_toggle,
            };
            let resp = ui.add(
                egui::TextEdit::singleline(field)
                    .desired_width(150.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("Ctrl+Alt+K"),
            );
            if ui
                .button("Rebind")
                .on_hover_text(
                    "Click, then press the new combo (Esc cancels). A combo another app \
                     already owns can't be captured this way — type it in the field instead.",
                )
                .clicked()
            {
                self.capturing = Some(target);
            }
            // Re-run the availability probe when the user edits the field to a parseable
            // combo; while it is incomplete/invalid, clear the stale note (Save still
            // surfaces the exact error).
            if resp.changed() {
                let combo = match target {
                    HotkeyTarget::Save => self.draft.hotkeys.save_clip.clone(),
                    HotkeyTarget::Record => self.draft.hotkeys.record_toggle.clone(),
                };
                if parse_hotkey(combo.trim()).is_ok() {
                    self.start_availability_check(target, combo.trim());
                } else {
                    self.hotkey_avail[target.idx()] = None;
                    self.hotkey_check = None;
                }
            }
            // Commit a typed hotkey on focus loss (T2 apply-on-change).
            if resp.lost_focus() {
                self.dirty = true;
            }
            // A cross-row duplicate (this combo == the other row's) takes precedence over
            // the pump's probe, which reports our own already-registered combos as
            // `Available` and so can't see it (A6 fast-follow HW finding). Otherwise show
            // the live "combo already taken" feedback set by the last probe for this row.
            if let Some(note) = self.cross_conflict_note(target) {
                ui.colored_label(ERR_RED, note);
            } else {
                match self.hotkey_avail[target.idx()] {
                    Some(Availability::Taken) => {
                        ui.colored_label(ERR_RED, "⚠ in use by another app");
                    }
                    Some(Availability::Available) => {
                        ui.colored_label(OK_GREEN, "✓ available");
                    }
                    Some(Availability::Unknown) => {
                        ui.weak("(couldn't check)");
                    }
                    None => {}
                }
            }
        });
        ui.end_row();
    }

    /// Apply-on-change write-through (T2): validate the draft, write it (preserving the
    /// file's comments + unknown keys), and hot-apply the live fields. Called whenever a
    /// field completes an edit ([`Editor::dirty`]). On a validation failure nothing is
    /// written and the exact error is shown inline; a change with no net effect is a no-op.
    /// The restart-bearing fields are surfaced only by the banner (no per-field chip, no
    /// success line).
    fn autocommit(&mut self, cmd_tx: &Sender<EngineCommand>) {
        self.draft.audio.mic = self.mic.to_cfg();

        // Validate EVERYTHING first — all fallible checks are pure / side-effect-free — so
        // no hard-to-undo OS change (the live hotkey rebind below) can happen before a
        // later check aborts the commit. Ordering the rebind before `draft.validate` /
        // `validate_output_dir` (as an earlier version did) could unregister the old combo
        // yet skip sending the matching engine id → a genuinely dead hotkey (rust-review
        // HIGH). Hotkeys are checked here (not in `Config::validate`) so one bad combo
        // can't make `load(..).unwrap_or_default()` silently discard the whole config.
        if let Err(e) = self.validate_hotkeys() {
            self.last_result = Some(Err(e));
            return;
        }
        if let Err(e) = self.draft.validate() {
            self.last_result = Some(Err(e.to_string()));
            return;
        }
        // Resolve + create the output folder (empty → OS Videos default); reject only if
        // uncreatable. The resolved path is what the engine hot-applies.
        let resolved = match self.validate_output_dir() {
            Ok(d) => d,
            Err(e) => {
                self.last_result = Some(Err(e));
                return;
            }
        };

        // All validation passed. Now live-rebind any changed hotkey on the pump thread —
        // the one hard-to-undo OS change, done LAST before the write. On success it sends
        // the new engine event id IMMEDIATELY (so the OS registration and the engine's id
        // filter are updated together, never split by a later fallible step — even a write
        // failure then leaves a working, if unpersisted, binding rather than a dead one); a
        // combo another app owns is reverted here so the write persists the working combo.
        let hotkey_conflict = self.rebind_changed_hotkeys(cmd_tx);

        // Nothing left to write (e.g. the sole change was a reverted hotkey conflict) →
        // surface any conflict, else clear a stale error.
        if self.draft == self.base {
            self.last_result = hotkey_conflict.map(Err);
            return;
        }
        if let Err(e) = self.draft.write_atomic(&self.path) {
            warn!(error = %e, "settings write failed");
            self.last_result = Some(Err(e.to_string()));
            return;
        }

        // Hot-apply the remaining live fields (no restart, T2/T2b): clear-after-save, the
        // output folder (save path resolves it per-save), the instant-replay length (ring
        // caps), and a mic DEVICE swap (§7 rebuild). The rebound hotkey ids already went
        // out with the rebind above. What remains — quality/resolution/fps, a mic on↔off
        // flip, the game/app-sound toggle — surfaces in the restart banner.
        if self.draft.buffer.clear_after_save != self.base.buffer.clear_after_save {
            let _ = cmd_tx.send(EngineCommand::SetClearAfterSave(
                self.draft.buffer.clear_after_save,
            ));
        }
        if self.draft.output.dir != self.base.output.dir {
            let _ = cmd_tx.send(EngineCommand::SetOutputDir(resolved));
        }
        if self.draft.buffer.seconds != self.base.buffer.seconds {
            let _ = cmd_tx.send(EngineCommand::SetDurationCap(self.draft.buffer.seconds));
        }
        // A mic device swap (both sides "on") is live; a mic on↔off flip is a topology
        // change routed through the restart banner instead.
        if self.draft.audio.mic != self.base.audio.mic
            && mic_is_device_swap(&self.base.audio.mic, &self.draft.audio.mic)
        {
            let _ = cmd_tx.send(EngineCommand::SetMicSelection(DeviceSelection::for_mic(
                self.draft.audio.mic.trim(),
            )));
        }

        let restart = self.restart_required_fields();
        self.base = self.draft.clone();
        self.mic = MicChoice::from_cfg(&self.base.audio.mic);
        info!(path = %self.path.display(), restart = ?restart, "settings applied (write-through)");
        self.last_result = hotkey_conflict.map(Err);
    }

    /// Live-rebind each hotkey that changed from `base` (T2b) on the pump thread. On a
    /// clean rebind the new engine event id is sent through `cmd_tx` IMMEDIATELY — so the
    /// OS registration and the engine's id filter always move together (never a dead
    /// hotkey, even if a later step in `autocommit` fails). A combo another app owns is
    /// reverted in the draft (keeping the working binding) and its message returned.
    /// Blocks briefly on the pump reply — a user-initiated, infrequent commit on the
    /// settings-UI thread (satellite law: never the engine).
    fn rebind_changed_hotkeys(&mut self, cmd_tx: &Sender<EngineCommand>) -> Option<String> {
        let mut conflict = None;
        for target in [HotkeyTarget::Save, HotkeyTarget::Record] {
            let draft = self.combo_for(target).trim().to_string();
            let base = match target {
                HotkeyTarget::Save => self.base.hotkeys.save_clip.trim().to_string(),
                HotkeyTarget::Record => self.base.hotkeys.record_toggle.trim().to_string(),
            };
            if draft == base {
                continue; // unchanged
            }
            match self.rebind_blocking(target.role(), &draft) {
                RebindOutcome::Applied(id) => {
                    // Send the new id in lockstep with the successful OS rebind.
                    let _ = cmd_tx.send(match target {
                        HotkeyTarget::Save => EngineCommand::SetSaveHotkeyId(id),
                        HotkeyTarget::Record => EngineCommand::SetRecordHotkeyId(id),
                    });
                }
                RebindOutcome::Conflict => {
                    // Keep the working binding: revert the draft + surface the error.
                    match target {
                        HotkeyTarget::Save => self.draft.hotkeys.save_clip = base,
                        HotkeyTarget::Record => self.draft.hotkeys.record_toggle = base,
                    }
                    conflict.get_or_insert_with(|| {
                        format!(
                            "{} is in use by another app — kept your previous binding.",
                            target.label()
                        )
                    });
                }
                // Pump gone / unparseable-after-validate: write it (it takes effect on the
                // next restart), but there is no live id to apply.
                RebindOutcome::Unknown => {}
            }
        }
        conflict
    }

    /// Wait briefly for the pump's live-rebind reply (T2b). No pump (unit tests / a
    /// failed spawn) → [`RebindOutcome::Unknown`], so the config write still happens and
    /// the binding applies on the next restart.
    fn rebind_blocking(&self, role: HotkeyRole, combo: &str) -> RebindOutcome {
        match &self.hotkey_ctl {
            Some(ctl) => ctl
                .rebind(role, combo)
                .recv_timeout(HOTKEY_REBIND_TIMEOUT)
                .unwrap_or(RebindOutcome::Unknown),
            None => RebindOutcome::Unknown,
        }
    }

    /// The human names of the restart-bearing fields that differ between configs `a`
    /// and `b` (everything except the hot-applied clear-after-save). Pure + the single
    /// place the field→name mapping lives, so the save note, the inline chips (U5), and
    /// the banner (U7) can't drift.
    fn restart_fields(a: &Config, b: &Config) -> Vec<&'static str> {
        let mut v = Vec::new();
        if a.encode.quality != b.encode.quality {
            v.push("quality");
        }
        if a.encode.resolution != b.encode.resolution {
            v.push("resolution");
        }
        if a.capture.fps != b.capture.fps {
            v.push("frame rate");
        }
        // Hot-applied live (T2/T2b), so NOT here: output folder (SetOutputDir), instant-
        // replay length (SetDurationCap), hotkeys (pump rebind), and mic DEVICE swaps
        // (SetMicSelection / §7). What remains are the epoch-topology changes:
        //   • a mic on↔off flip adds/removes the Mic track,
        //   • the game/app-sound toggle adds/removes the desktop source,
        // both decided at epoch start (DECISIONS "T2b" — the accepted residual).
        if mic_is_on(&a.audio.mic) != mic_is_on(&b.audio.mic) {
            v.push("microphone on/off");
        }
        if a.audio.desktop != b.audio.desktop {
            v.push("game & app sound");
        }
        v
    }

    /// The fields changed in the pending save (`base` → `draft`) — the "Restart to
    /// apply: …" note shown right after Save.
    fn restart_required_fields(&self) -> Vec<&'static str> {
        Self::restart_fields(&self.base, &self.draft)
    }

    /// The accumulated set of committed-but-not-yet-applied restart-bearing changes
    /// (`applied` → committed `base`) — what the auto-restart banner names (U7). Empty
    /// when the running engine already matches the saved config.
    fn pending_restart_fields(&self) -> Vec<&'static str> {
        Self::restart_fields(&self.applied, &self.base)
    }

    /// Check both hotkeys parse and differ from each other. Returns the message to
    /// show on failure. Pure over the draft (no I/O), so it is unit-tested.
    ///
    /// Not checked by `Config::validate` on purpose: making a bad hotkey fail the
    /// load would turn `Config::load(..).unwrap_or_default()` (main.rs + the editor
    /// open) into a silent "discard the whole user config" — worse than the pump's
    /// clear fatal-at-startup parse error. So we guard the UI *write* path here and
    /// leave read-side enforcement to the pump (DECISIONS "A6").
    fn validate_hotkeys(&self) -> Result<(), String> {
        let save = parse_hotkey(self.draft.hotkeys.save_clip.trim())
            .map_err(|e| format!("save-clip hotkey: {e}"))?;
        let record = parse_hotkey(self.draft.hotkeys.record_toggle.trim())
            .map_err(|e| format!("record hotkey: {e}"))?;
        // Compare the PARSED hotkeys, not the strings, so aliases / different modifier
        // order ("Alt+Ctrl+S" vs "Ctrl+Alt+S") are still caught as the same binding.
        if save == record {
            return Err("save-clip and record hotkeys must differ".to_string());
        }
        Ok(())
    }

    /// This row's current draft combo string.
    fn combo_for(&self, target: HotkeyTarget) -> &str {
        match target {
            HotkeyTarget::Save => &self.draft.hotkeys.save_clip,
            HotkeyTarget::Record => &self.draft.hotkeys.record_toggle,
        }
    }

    /// A live "same as the other hotkey" note for `target`, or `None` if there is no
    /// cross-row conflict. This is checked UI-side and takes precedence over the pump's
    /// availability probe, which structurally CANNOT catch it: the probe reports any
    /// combo already registered by us (i.e. the *other* row's current binding) as
    /// `Available` (`hotkey.rs` `check_availability` short-circuits on our own ids), so
    /// typing Save's combo into the Record field would otherwise read a false
    /// `✓ available`. Compares the PARSED hotkeys so alias/modifier-order forms still
    /// match. `validate_hotkeys` still blocks the same conflict on Save; this only stops
    /// the badge from lying before then.
    fn cross_conflict_note(&self, target: HotkeyTarget) -> Option<String> {
        let this = parse_hotkey(self.combo_for(target).trim()).ok()?;
        let other = parse_hotkey(self.combo_for(target.other()).trim()).ok()?;
        (this == other).then(|| format!("⚠ same as {}", target.other().label()))
    }

    /// Ensure the draft's output folder exists (creating it if missing), mirroring the
    /// engine's `prepare_output_dir`. Returns the exact I/O error on failure so Save can
    /// surface it in red and write nothing. Kept out of `Config::validate` on purpose:
    /// a "dir must exist" check there would make `Config::load(..).unwrap_or_default()`
    /// silently discard a whole config when a saved drive is unplugged (the same trap
    /// hotkeys avoid — DECISIONS "A6" / "2026-07-08").
    fn validate_output_dir(&self) -> Result<PathBuf, String> {
        let dir = config::resolve_output_dir(&self.draft.output.dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("output folder: {} — {e}", dir.display()))?;
        Ok(dir)
    }
}

/// The combo label for a [`Quality`] tier.
fn quality_label(q: Quality) -> &'static str {
    match q {
        Quality::Efficient => "Efficient",
        Quality::Default => "Default",
        Quality::High => "High",
        Quality::Max => "Max",
    }
}

/// The combo label for a [`Resolution`] tier.
fn resolution_label(r: Resolution) -> &'static str {
    match r {
        Resolution::Native => "Source (native)",
        Resolution::P1440 => "1440p",
        Resolution::P1080 => "1080p",
        Resolution::P720 => "720p",
    }
}

/// The outcome of polling for a press-to-bind capture (A6).
enum CaptureResult {
    /// Esc pressed — cancel the capture, leave the binding unchanged.
    Cancel,
    /// A valid combo was captured (already `parse_hotkey`-validated).
    Bound(String),
}

/// Scan this frame's key events for a press-to-bind result: Esc cancels; the first
/// key press that forms a valid accelerator (a bindable key with a Ctrl/Alt modifier)
/// binds. Modifier-only presses and unbindable keys are ignored. The matched event is
/// *consumed* (removed from the queue) so no other focused widget also reacts to the
/// same keystroke.
fn capture_combo(i: &mut egui::InputState) -> Option<CaptureResult> {
    let found = i.events.iter().enumerate().find_map(|(idx, ev)| {
        if let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = ev
        {
            if *key == egui::Key::Escape {
                return Some((idx, CaptureResult::Cancel));
            }
            if let Some(combo) = accelerator_from(*modifiers, *key) {
                return Some((idx, CaptureResult::Bound(combo)));
            }
        }
        None
    });
    found.map(|(idx, result)| {
        i.events.remove(idx);
        result
    })
}

/// Build a `global-hotkey` accelerator string (e.g. `"Ctrl+Alt+S"`) from a modifier
/// set + key, or `None` if it is not a valid global hotkey: a primary modifier (Ctrl
/// or Alt) is required so the combo can't fire on a bare keypress, the key must be
/// bindable ([`key_to_token`]), and the result must actually [`parse_hotkey`]. Pure +
/// unit-tested.
fn accelerator_from(mods: egui::Modifiers, key: egui::Key) -> Option<String> {
    // Windows target: ignore mac_cmd/command; Ctrl/Alt/Shift are the usable modifiers.
    // Ctrl or Alt is REQUIRED (stricter than global-hotkey, which would accept a bare
    // "F9"): press-to-bind refuses bare-key / Shift-only combos so a global hotkey
    // can't hijack an ordinary keystroke. A bare function key must be hand-set in TOML.
    if !(mods.ctrl || mods.alt) {
        return None;
    }
    let token = key_to_token(key)?;
    let mut parts: Vec<&str> = Vec::new();
    if mods.ctrl {
        parts.push("Ctrl");
    }
    if mods.alt {
        parts.push("Alt");
    }
    if mods.shift {
        parts.push("Shift");
    }
    let combo = format!("{}+{token}", parts.join("+"));
    // Only accept a combo `global-hotkey` can actually parse (guards odd keys).
    parse_hotkey(&combo).ok().map(|_| combo)
}

/// Map an [`egui::Key`] to the human key token used in accelerator strings (`A` → `A`,
/// `Num1` → `1`, `F9` → `F9`). `global-hotkey`'s parser accepts these short forms
/// identically to the long `KeyA`/`Digit1` codes (same resulting `HotKey`), so we store
/// and show the readable form — matching the shipped `Ctrl+Alt+S` defaults. Returns
/// `None` for keys that are not sensible global-hotkey targets (arrows, Escape,
/// punctuation, …). Pure.
fn key_to_token(key: egui::Key) -> Option<String> {
    let n = key.name(); // letters "A".."Z", digits "0".."9", "F1".., others like "Escape"
    let first = n.chars().next()?;
    if n.len() == 1 && first.is_ascii_alphabetic() {
        Some(first.to_ascii_uppercase().to_string())
    } else if n.len() == 1 && first.is_ascii_digit() {
        Some(first.to_string())
    } else if first == 'F' && n.len() >= 2 && n[1..].chars().all(|c| c.is_ascii_digit()) {
        Some(n.to_string())
    } else {
        None
    }
}

/// A representative 16:9 canvas for a resolution tier's bitrate estimate. `native`
/// is estimated at 1080p (the common beta display); explicit tiers use their height.
fn estimate_canvas(res: Resolution) -> (u32, u32) {
    let h = match res {
        Resolution::Native => 1080,
        other => other.to_max_height(),
    };
    (h * 16 / 9, h)
}

/// Estimated video bitrate (Mbps) the encoder targets at the chosen resolution tier,
/// fps, and quality — the same `video_target_bitrate_bps` the encoder uses.
fn estimate_video_mbps(res: Resolution, fps: u32, quality: Quality) -> f32 {
    let (w, h) = estimate_canvas(res);
    video_target_bitrate_bps(w, h, fps, quality.multiplier()) as f32 / 1_000_000.0
}

/// Estimated ring RAM (MiB) for the buffer length + fps + quality. Mirrors the
/// engine's byte cap (`buffer_supervisor`): computed at a nominal 1080p regardless
/// of the output resolution, and over `buffer_seconds + one GOP` of retention (the
/// engine keeps a GOP of pre-roll margin above the configured length). Assumes the
/// default GOP (precise mode, a TOML-only advanced toggle, tightens it slightly).
fn estimate_ram_mib(buffer_seconds: u32, fps: u32, quality: Quality) -> f32 {
    let est = est_bitrate_bps(1920, 1080, fps, quality.multiplier());
    let retained = buffer_seconds + IDR_INTERVAL_SECONDS as u32;
    status::bytes_to_mib(byte_cap_bytes(retained, est))
}

/// The bar fill colour for a level fraction: green through most of the range,
/// amber approaching clip, red at the very top. Mirrors the tray's state palette.
fn meter_color(fraction: f32) -> egui::Color32 {
    if fraction >= 0.95 {
        theme::BAD // red — near/at clip
    } else if fraction >= 0.8 {
        theme::AMBER // amber — hot
    } else {
        theme::ACCENT // lavender — nominal (green is too prominent; accent carries "normal")
    }
}

/// Draw one VU meter row as three columns partitioned at EXPLICIT rects (P2): a
/// right-aligned constant-width label · the bar · a fixed-width monospace dB readout. The
/// readout column is reserved at a width MEASURED from the monospace face (widest value
/// `-00.0 dB`) BEFORE the bar takes the remainder — so across every row the bars share an
/// identical start AND end x and the readouts right-align to one column (a dimmed dash is
/// centred in the same reservation). `secondary` mutes the label for the per-app tracks;
/// `rms_frac`/`peak_frac` are the smoothed 0..=1 bar fractions; `readout_amp` is the ~3 Hz
/// interval-peak amplitude for the DECOUPLED numeric readout.
fn draw_meter(
    ui: &mut egui::Ui,
    title: &str,
    secondary: bool,
    rms_frac: f32,
    peak_frac: f32,
    readout_amp: f32,
) {
    let body_font = egui::TextStyle::Body.resolve(ui.style());
    let mono_font = egui::TextStyle::Monospace.resolve(ui.style());

    // Reserve the whole row up front so measuring + drawing share one painter.
    let full_w = ui.available_width();
    let (row, _resp) =
        ui.allocate_exact_size(egui::vec2(full_w, METER_HEIGHT), egui::Sense::hover());

    let visuals = ui.visuals();
    let text_col = visuals.text_color();
    let weak_col = visuals.weak_text_color();
    let track_bg = visuals.extreme_bg_color;
    let marker_col = theme::ACCENT_HOVER;
    let painter = ui.painter();

    // Reserve the readout column at the width of the widest value in the monospace face
    // ("-00.0 dB"), plus a little breathing room — measured via the painter (whose
    // `layout_no_wrap` is `&self`, unlike `Fonts`) so it is never clipped and never floats.
    let readout_w = painter
        .layout_no_wrap("-00.0 dB".to_owned(), mono_font.clone(), text_col)
        .rect
        .width()
        .ceil()
        + 8.0;

    // Partition the row into fixed rects so all bars share an identical start AND end x.
    let gap = 8.0;
    let label_rect =
        egui::Rect::from_min_size(row.min, egui::vec2(METER_LABEL_WIDTH, row.height()));
    let readout_rect = egui::Rect::from_min_size(
        egui::pos2(row.right() - readout_w, row.top()),
        egui::vec2(readout_w, row.height()),
    );
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(label_rect.right() + gap, row.top()),
        egui::pos2(readout_rect.left() - gap, row.bottom()),
    );

    // Column 1 — the label, right-aligned against the bar's start (muted if secondary).
    painter.text(
        egui::pos2(label_rect.right(), label_rect.center().y),
        egui::Align2::RIGHT_CENTER,
        title,
        body_font,
        if secondary { weak_col } else { text_col },
    );

    // Column 2 — the bar (recessed well + RMS body + a bright peak tick).
    if bar_rect.width() > 1.0 {
        painter.rect_filled(bar_rect, METER_RADIUS, track_bg);
        if rms_frac > 0.0 {
            let mut fill = bar_rect;
            fill.set_width(bar_rect.width() * rms_frac.min(1.0));
            painter.rect_filled(fill, METER_RADIUS, meter_color(rms_frac));
        }
        if peak_frac > 0.0 {
            let x = bar_rect.left() + bar_rect.width() * peak_frac.min(1.0);
            let marker = egui::Rect::from_min_max(
                egui::pos2((x - 1.5).max(bar_rect.left()), bar_rect.top()),
                egui::pos2((x + 1.5).min(bar_rect.right()), bar_rect.bottom()),
            );
            painter.rect_filled(marker, 0.0, marker_col);
        }
    }

    // Column 3 — the dB readout. At/below the floor a dimmed em dash CENTRED in the
    // reservation; otherwise the value RIGHT-aligned to the same column edge.
    let db = levels::linear_to_dbfs(readout_amp);
    if db <= levels::METER_FLOOR_DBFS {
        painter.text(
            readout_rect.center(),
            egui::Align2::CENTER_CENTER,
            "—",
            mono_font,
            weak_col,
        );
    } else {
        painter.text(
            egui::pos2(readout_rect.right(), readout_rect.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("{db:.1} dB"),
            mono_font,
            text_col,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mmss_pads_seconds() {
        assert_eq!(format_mmss(0), "0:00");
        assert_eq!(format_mmss(5), "0:05");
        assert_eq!(format_mmss(65), "1:05");
        assert_eq!(format_mmss(600), "10:00");
        assert_eq!(format_mmss(3599), "59:59");
        assert_eq!(format_mmss(3600), "60:00");
    }

    #[test]
    fn meter_display_order_puts_mic_first_and_marks_secondary() {
        use AudioTrackKind::*;
        // Engine container order in, display order out: Mic first, then Mix, then per-app.
        let mut kinds = vec![Mix, Game, VoiceChat, OtherSystem, Mic];
        kinds.sort_by_key(|k| meter_display_order(*k));
        assert_eq!(kinds, vec![Mic, Mix, Game, VoiceChat, OtherSystem]);
        // Only the per-app tracks are muted (secondary).
        assert!(MeterState::new(Game).is_secondary());
        assert!(MeterState::new(VoiceChat).is_secondary());
        assert!(MeterState::new(OtherSystem).is_secondary());
        assert!(!MeterState::new(Mic).is_secondary());
        assert!(!MeterState::new(Mix).is_secondary());
    }

    #[test]
    fn format_buffer_len_reads_naturally() {
        assert_eq!(format_buffer_len(30), "30 s");
        assert_eq!(format_buffer_len(59), "59 s");
        assert_eq!(format_buffer_len(60), "1 min");
        assert_eq!(format_buffer_len(120), "2 min");
        assert_eq!(format_buffer_len(90), "1 min 30 s");
    }

    #[test]
    fn first_run_line_names_the_hotkey_and_length() {
        let line = first_run_line("Ctrl+Alt+S", 45);
        assert!(line.contains("Ctrl+Alt+S"), "line = {line}");
        assert!(line.contains("45 s"), "line = {line}");
        assert!(line.contains(PRODUCT_NAME), "line = {line}");
    }

    #[test]
    fn mic_choice_roundtrips() {
        assert_eq!(MicChoice::from_cfg("default-follow"), MicChoice::Follow);
        assert_eq!(MicChoice::from_cfg("  default-follow "), MicChoice::Follow);
        assert_eq!(MicChoice::from_cfg("off"), MicChoice::Off);
        assert_eq!(
            MicChoice::from_cfg("{0.0.1.00000000}.{abc}"),
            MicChoice::Pinned("{0.0.1.00000000}.{abc}".to_string())
        );
        assert_eq!(MicChoice::Follow.to_cfg(), "default-follow");
        assert_eq!(MicChoice::Off.to_cfg(), "off");
        // A pinned id is trimmed; an empty pinned id round-trips to "" (which
        // Config::validate then rejects — the editor surfaces that error).
        assert_eq!(
            MicChoice::Pinned("  dev-id ".to_string()).to_cfg(),
            "dev-id"
        );
        assert_eq!(MicChoice::Pinned(String::new()).to_cfg(), "");
    }

    fn dev(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn mic_label_resolves_name_or_marks_unavailable() {
        let devices = vec![
            dev("id-fifine", "FIFINE Microphone"),
            dev("id-rt", "Realtek"),
        ];
        // Policies are fixed labels regardless of the device list.
        assert_eq!(MicChoice::Follow.label(&devices), "Default (follow)");
        assert_eq!(MicChoice::Off.label(&devices), "Off (no mic)");
        // A pin present in the list resolves to its friendly name (trimmed match).
        assert_eq!(
            MicChoice::Pinned("id-fifine".to_string()).label(&devices),
            "FIFINE Microphone"
        );
        assert_eq!(
            MicChoice::Pinned("  id-rt ".to_string()).label(&devices),
            "Realtek"
        );
        // A pin NOT in the list is surfaced as unavailable, never silently masked.
        assert_eq!(
            MicChoice::Pinned("id-gone".to_string()).label(&devices),
            "Unavailable: id-gone"
        );
        // An empty pin (defensive; the dropdown can't produce it) prompts a selection.
        assert_eq!(
            MicChoice::Pinned(String::new()).label(&devices),
            "Select a device…"
        );
    }

    #[test]
    fn mic_options_lists_policies_devices_and_preserves_unavailable_pin() {
        let devices = vec![dev("id-a", "Mic A"), dev("id-b", "Mic B")];

        // With a policy selected: Default + Off + one entry per live device, in order,
        // and NO trailing unavailable entry.
        let opts = mic_options(&devices, &MicChoice::Follow);
        let labels: Vec<&str> = opts.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Default (follow)", "Off (no mic)", "Mic A", "Mic B"]
        );
        let choices: Vec<MicChoice> = opts.into_iter().map(|o| o.choice).collect();
        assert_eq!(
            choices,
            vec![
                MicChoice::Follow,
                MicChoice::Off,
                MicChoice::Pinned("id-a".to_string()),
                MicChoice::Pinned("id-b".to_string()),
            ]
        );

        // A pin that IS a live device does not add a duplicate/unavailable entry.
        let opts = mic_options(&devices, &MicChoice::Pinned("id-a".to_string()));
        assert_eq!(opts.len(), 4, "live pin must not add an entry");

        // A pin that is NOT among the live devices is preserved as a trailing entry so
        // opening Settings never silently drops it.
        let opts = mic_options(&devices, &MicChoice::Pinned("id-gone".to_string()));
        assert_eq!(opts.len(), 5);
        let last = opts.last().unwrap();
        assert_eq!(last.label, "Unavailable: id-gone");
        assert_eq!(last.choice, MicChoice::Pinned("id-gone".to_string()));

        // No live devices at all (COM failure / none present): still Default + Off, and
        // a hand-set pin is still preserved.
        let opts = mic_options(&[], &MicChoice::Pinned("id-manual".to_string()));
        let labels: Vec<&str> = opts.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Default (follow)", "Off (no mic)", "Unavailable: id-manual"]
        );

        // An empty pin is NOT surfaced as an unavailable entry (nothing to preserve).
        let opts = mic_options(&devices, &MicChoice::Pinned("  ".to_string()));
        assert_eq!(opts.len(), 4);
    }

    #[test]
    fn estimate_canvas_is_16x9() {
        assert_eq!(estimate_canvas(Resolution::Native), (1920, 1080));
        assert_eq!(estimate_canvas(Resolution::P1080), (1920, 1080));
        assert_eq!(estimate_canvas(Resolution::P720), (1280, 720));
        assert_eq!(estimate_canvas(Resolution::P1440), (2560, 1440));
    }

    #[test]
    fn default_1080p_estimate_matches_t0_baseline() {
        // T0 calibration (DECISIONS 2026-07-07): 1080p60 Default ≈ 16 Mbps.
        let mbps = estimate_video_mbps(Resolution::P1080, 60, Quality::Default);
        assert!(
            (14.0..=18.0).contains(&mbps),
            "1080p60 default estimate = {mbps} Mbps, expected ~16"
        );
    }

    #[test]
    fn quality_scales_bitrate_and_ram() {
        let eff = estimate_video_mbps(Resolution::P1080, 60, Quality::Efficient);
        let def = estimate_video_mbps(Resolution::P1080, 60, Quality::Default);
        let max = estimate_video_mbps(Resolution::P1080, 60, Quality::Max);
        assert!(eff < def && def < max, "{eff} < {def} < {max}");

        // RAM grows with both buffer length and quality (higher bitrate → bigger cap).
        assert!(
            estimate_ram_mib(30, 60, Quality::Default)
                < estimate_ram_mib(120, 60, Quality::Default)
        );
        assert!(
            estimate_ram_mib(120, 60, Quality::Efficient) < estimate_ram_mib(120, 60, Quality::Max)
        );
    }

    fn test_editor(path: PathBuf) -> Editor {
        Editor {
            base: Config::default(),
            applied: Config::default(),
            draft: Config::default(),
            mic: MicChoice::Follow,
            mic_devices: Vec::new(),
            capturing: None,
            hotkey_ctl: None,
            hotkey_check: None,
            hotkey_avail: [None, None],
            folder_text: String::new(),
            path,
            last_result: None,
            dirty: false,
        }
    }

    #[test]
    fn key_to_token_maps_bindable_keys_only() {
        assert_eq!(key_to_token(egui::Key::A).as_deref(), Some("A"));
        assert_eq!(key_to_token(egui::Key::S).as_deref(), Some("S"));
        assert_eq!(key_to_token(egui::Key::Num1).as_deref(), Some("1"));
        assert_eq!(key_to_token(egui::Key::F9).as_deref(), Some("F9"));
        // Non-bindable keys (no sensible global-hotkey target).
        assert_eq!(key_to_token(egui::Key::Escape), None);
        assert_eq!(key_to_token(egui::Key::ArrowUp), None);
        assert_eq!(key_to_token(egui::Key::Space), None);
    }

    #[test]
    fn accelerator_from_requires_primary_modifier_and_parses() {
        use egui::{Key, Modifiers};
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let combo = accelerator_from(ctrl_alt, Key::S).expect("ctrl+alt+S is valid");
        // Human token, not the long `KeyS` code — matches the shipped defaults' style.
        assert_eq!(combo, "Ctrl+Alt+S");
        assert!(parse_hotkey(&combo).is_ok());

        // Ctrl+F9 (single modifier + function key).
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(accelerator_from(ctrl, Key::F9).as_deref(), Some("Ctrl+F9"));

        // Shift alone is not a primary modifier → rejected.
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(accelerator_from(shift, Key::S), None);
        // No modifier at all → rejected (would fire on a bare keypress).
        assert_eq!(accelerator_from(Modifiers::default(), Key::S), None);
        // A modifier but an unbindable key → rejected.
        assert_eq!(accelerator_from(ctrl_alt, Key::Escape), None);

        // Full three-modifier combo, ordered Ctrl+Alt+Shift.
        let all = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            accelerator_from(all, Key::D).as_deref(),
            Some("Ctrl+Alt+Shift+D")
        );
    }

    #[test]
    fn pretty_and_code_forms_are_the_same_hotkey() {
        // The human token we now emit registers to the identical HotKey as the long
        // code form, so switching the stored/displayed style is purely cosmetic.
        assert_eq!(
            parse_hotkey("Ctrl+Alt+K").unwrap(),
            parse_hotkey("Ctrl+Alt+KeyK").unwrap()
        );
        assert_eq!(
            parse_hotkey("Ctrl+Alt+1").unwrap(),
            parse_hotkey("Ctrl+Alt+Digit1").unwrap()
        );
    }

    #[test]
    fn validate_hotkeys_rejects_unparseable_and_self_conflict() {
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        // Defaults are valid and distinct.
        assert!(ed.validate_hotkeys().is_ok());
        // Identical bindings conflict.
        ed.draft.hotkeys.record_toggle = ed.draft.hotkeys.save_clip.clone();
        assert!(ed.validate_hotkeys().is_err());
        // Unparseable combo.
        ed.draft.hotkeys.record_toggle = "Ctrl+Alt+Nope".to_string();
        assert!(ed.validate_hotkeys().is_err());
    }

    #[test]
    fn cross_conflict_note_catches_duplicate_both_ways() {
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        // Distinct defaults → no cross-row note on either row.
        assert!(ed.cross_conflict_note(HotkeyTarget::Save).is_none());
        assert!(ed.cross_conflict_note(HotkeyTarget::Record).is_none());

        // Make them equal (via a modifier-order alias to prove it compares PARSED keys,
        // not strings) → both rows report the conflict, naming the other row.
        ed.draft.hotkeys.save_clip = "Ctrl+Alt+S".to_string();
        ed.draft.hotkeys.record_toggle = "Alt+Ctrl+S".to_string();
        assert_eq!(
            ed.cross_conflict_note(HotkeyTarget::Save).as_deref(),
            Some("⚠ same as Record toggle")
        );
        assert_eq!(
            ed.cross_conflict_note(HotkeyTarget::Record).as_deref(),
            Some("⚠ same as Save clip")
        );

        // An unparseable row yields no note (Save surfaces the parse error instead).
        ed.draft.hotkeys.record_toggle = "Ctrl+Alt+Nope".to_string();
        assert!(ed.cross_conflict_note(HotkeyTarget::Save).is_none());
    }

    #[test]
    fn hotkey_target_idx_is_distinct() {
        assert_ne!(HotkeyTarget::Save.idx(), HotkeyTarget::Record.idx());
        assert!(HotkeyTarget::Save.idx() < 2 && HotkeyTarget::Record.idx() < 2);
    }

    #[test]
    fn availability_check_is_a_noop_without_a_pump() {
        // No pump handle (as in every headless test / no-tray path): starting a check
        // clears the stale result and enqueues nothing to poll, so the UI simply shows
        // no availability note rather than blocking or panicking.
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        ed.hotkey_avail[HotkeyTarget::Save.idx()] = Some(Availability::Taken);
        ed.start_availability_check(HotkeyTarget::Save, "Ctrl+Alt+KeyK");
        assert!(ed.hotkey_check.is_none());
        assert_eq!(ed.hotkey_avail[HotkeyTarget::Save.idx()], None);
        // Polling with nothing in flight is a harmless no-op.
        ed.poll_availability();
        assert!(ed.hotkey_check.is_none());
    }

    #[test]
    fn pending_restart_fields_tracks_committed_vs_applied() {
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        // Fresh: the engine's `applied` config matches the committed `base` → banner empty.
        assert!(ed.pending_restart_fields().is_empty());
        // A committed restart-bearing change (base advanced past applied) is reported.
        ed.base.encode.quality = Quality::Max;
        ed.base.capture.fps = 30;
        assert_eq!(ed.pending_restart_fields(), vec!["quality", "frame rate"]);
        // clear_after_save hot-applies, so it never enters the pending set.
        ed.base = ed.applied.clone();
        ed.base.buffer.clear_after_save = !ed.applied.buffer.clear_after_save;
        assert!(ed.pending_restart_fields().is_empty());
    }

    #[test]
    fn restart_fields_excludes_the_t2b_live_fields() {
        // T2b: hotkeys, instant-replay length, and a mic DEVICE swap hot-apply live, so
        // none of them is a restart field anymore.
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        ed.draft.hotkeys.save_clip = "Ctrl+Alt+KeyP".to_string();
        ed.draft.hotkeys.record_toggle = "Ctrl+Alt+KeyR".to_string();
        ed.draft.buffer.seconds = ed.base.buffer.seconds + 5;
        // A device swap: default-follow → a pinned id (both "on").
        ed.draft.audio.mic = "{0.0.1.0}.{some-mic}".to_string();
        assert!(
            ed.restart_required_fields().is_empty(),
            "T2b-live fields must not require a restart: {:?}",
            ed.restart_required_fields()
        );
    }

    #[test]
    fn restart_fields_empty_when_unchanged_or_only_hotswap() {
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        // No change → nothing needs a restart.
        assert!(ed.restart_required_fields().is_empty());
        // clear_after_save is hot-applied, so it is NOT a restart field.
        ed.draft.buffer.clear_after_save = !ed.base.buffer.clear_after_save;
        assert!(ed.restart_required_fields().is_empty());
    }

    #[test]
    fn restart_fields_covers_every_restart_only_field() {
        // Change every restart-required field; each must appear, in order. Live fields
        // (output folder, replay length, hotkeys, mic device swaps — T2/T2b) must NOT
        // appear. The two accepted topology residuals are the mic on↔off flip and the
        // game/app-sound toggle (DECISIONS "T2b").
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        ed.draft.encode.quality = Quality::Max;
        ed.draft.encode.resolution = Resolution::P720;
        ed.draft.capture.fps = 30;
        ed.draft.buffer.seconds = ed.base.buffer.seconds + 5; // live — must NOT appear
        ed.draft.output.dir = "D:/clips".to_string(); // live — must NOT appear
        ed.draft.hotkeys.save_clip = "Ctrl+Alt+KeyP".to_string(); // live — must NOT appear
        ed.draft.audio.desktop = !ed.base.audio.desktop; // topology → restart
        ed.draft.audio.mic = "off".to_string(); // on↔off topology → restart
        assert_eq!(
            ed.restart_required_fields(),
            vec![
                "quality",
                "resolution",
                "frame rate",
                "microphone on/off",
                "game & app sound",
            ]
        );
    }

    #[test]
    fn mic_swap_helpers_classify_on_off_vs_device_swap() {
        // "off" ↔ anything is a topology flip (restart); two active selections swap live.
        assert!(mic_is_on("default-follow"));
        assert!(mic_is_on("{0.0.1.0}.{x}"));
        assert!(!mic_is_on("off"));
        assert!(!mic_is_on("  off "));
        assert!(mic_is_device_swap("default-follow", "{0.0.1.0}.{x}"));
        assert!(mic_is_device_swap("{a}", "default-follow"));
        assert!(!mic_is_device_swap("off", "default-follow"));
        assert!(!mic_is_device_swap("default-follow", "off"));
    }

    #[test]
    fn autocommit_writes_file_hot_applies_and_syncs_base() {
        let dir = std::env::temp_dir().join(format!("clipd_a5_save_ok_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let (tx, rx) = crossbeam_channel::bounded::<EngineCommand>(4);
        let mut ed = test_editor(path.clone());
        // Point the output folder at the temp dir so the create_dir_all check doesn't
        // materialise the real %USERPROFILE%\Videos\clipd default as a side effect. The
        // output folder now hot-applies live (T2), so this also queues SetOutputDir.
        ed.draft.output.dir = dir.to_string_lossy().into_owned();
        // A restart field + the two hot-swap fields all change.
        ed.draft.encode.quality = Quality::High;
        let new_clear = !ed.base.buffer.clear_after_save;
        ed.draft.buffer.clear_after_save = new_clear;

        ed.autocommit(&tx);

        // A valid, reloadable file was written; a successful apply is silent (no error).
        assert!(path.exists());
        assert!(Config::load(&path).is_ok());
        assert!(ed.last_result.is_none(), "a successful apply is silent");
        // The hot-swap commands were pushed: clear-after-save, then the output folder.
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineCommand::SetClearAfterSave(v)) if v == new_clear
        ));
        assert!(matches!(rx.try_recv(), Ok(EngineCommand::SetOutputDir(_))));
        // base is now in sync with the applied draft.
        assert_eq!(ed.base, ed.draft);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn autocommit_hot_applies_replay_length() {
        // T2b: an instant-replay length change hot-applies as SetDurationCap (no restart)
        // and does not enter the restart set.
        let dir = std::env::temp_dir().join(format!("clipd_t2b_len_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let (tx, rx) = crossbeam_channel::bounded::<EngineCommand>(8);
        let mut ed = test_editor(path.clone());
        ed.draft.output.dir = dir.to_string_lossy().into_owned();
        let new_len = ed.base.buffer.seconds + 7;
        ed.draft.buffer.seconds = new_len;

        ed.autocommit(&tx);

        assert!(ed.last_result.is_none(), "a successful apply is silent");
        let mut saw_duration = false;
        while let Ok(cmd) = rx.try_recv() {
            if let EngineCommand::SetDurationCap(s) = cmd {
                assert_eq!(s, new_len);
                saw_duration = true;
            }
        }
        assert!(
            saw_duration,
            "a replay-length change must queue SetDurationCap"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn autocommit_invalid_writes_nothing_and_reports_the_error() {
        let path =
            std::env::temp_dir().join(format!("clipd_a5_save_bad_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let (tx, rx) = crossbeam_channel::bounded::<EngineCommand>(4);
        let mut ed = test_editor(path.clone());
        // An empty pinned id → audio.mic = "" → Config::validate rejects it.
        ed.mic = MicChoice::Pinned(String::new());

        ed.autocommit(&tx);

        assert!(matches!(ed.last_result, Some(Err(_))));
        assert!(!path.exists(), "an invalid apply must not write the file");
        assert!(rx.try_recv().is_err(), "no hot-swap on a rejected apply");
    }

    #[test]
    fn validate_output_dir_creates_missing_and_rejects_uncreatable() {
        let base = std::env::temp_dir().join(format!("clipd_a5_outdir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let mut ed = test_editor(PathBuf::from("unused.toml"));

        // A not-yet-existing, creatable folder is accepted AND created.
        let good = base.join("clips");
        ed.draft.output.dir = good.to_string_lossy().into_owned();
        assert!(ed.validate_output_dir().is_ok());
        assert!(good.is_dir(), "the output folder should have been created");

        // A path *under a file* can't be made into a directory → rejected, error surfaced.
        let file = base.join("a_file");
        std::fs::write(&file, b"x").unwrap();
        ed.draft.output.dir = file.join("nope").to_string_lossy().into_owned();
        let err = ed.validate_output_dir().unwrap_err();
        assert!(err.starts_with("output folder:"), "err = {err}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
