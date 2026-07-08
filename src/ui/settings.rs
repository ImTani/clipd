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
use crate::audio::levels::{self, AudioLevels, StreamMeter};
use crate::audio::wasapi_stream::AudioTrackKind;
use crate::config::{self, Config, Quality, Resolution};
use crate::engine::{EngineCommand, TrayState};
use crate::hotkey::{parse_hotkey, Availability, HotkeyControl};
use crate::spec_constants::encoder::video_target_bitrate_bps;
use crate::spec_constants::ring::{
    byte_cap_bytes, est_bitrate_bps, IDR_INTERVAL_SECONDS, MAX_BUFFER_SECONDS,
};
use crate::spec_constants::PRODUCT_NAME;
use crate::status::{self, EngineStatus, SaveOutcome, StatusSnapshot};

/// The window's inner size at first open (logical points). A comfortable size for
/// the A5 editor to grow into without being cramped in the A2 skeleton.
const WINDOW_SIZE: [f32; 2] = [560.0, 440.0];

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

/// One VU meter row's bar height in logical points.
const METER_HEIGHT: f32 = 18.0;
/// Bar corner radius.
const METER_RADIUS: f32 = 3.0;

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
    let mut native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{PRODUCT_NAME} settings"))
            .with_inner_size(WINDOW_SIZE),
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
}

impl MeterState {
    fn new(kind: AudioTrackKind) -> Self {
        Self {
            kind,
            display_rms: 0.0,
            display_peak: 0.0,
        }
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
        let meters = streams.into_iter().map(MeterState::new).collect();
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
        }
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

        // Re-scan the recent-clips list if the tray flagged a re-show (A7). Swap so we
        // consume the request exactly once.
        if self.shared.rescan_recent.swap(false, Ordering::Relaxed) {
            self.recent.rescan();
        }

        // Pick up any completed live hotkey-availability probe (A6 fast-follow). Cheap
        // when nothing is in flight; while visible the meter cadence already repaints,
        // so the result shows within a frame.
        self.editor.poll_availability();

        // Close handling. The tray's quit flag is authoritative: when set, close the
        // window for real (ending the event loop and this thread). Otherwise a
        // user-initiated close (the `X`) is intercepted — cancelled, then hidden — so
        // the window can be re-shown, since winit permits only one event loop per
        // process and we never recreate it. See the module docs.
        if self.shared.quit.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
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

    /// Draw the window contents. eframe hands a root [`egui::Ui`] with no margin or
    /// background, so wrap it in a central-panel frame for padding + fill.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let dt = ui.input(|i| i.stable_dt);
        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading(format!("{PRODUCT_NAME} settings"));
                ui.label(format!("version {VERSION}"));
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);

                draw_status(ui, &self.status.snapshot());

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);

                ui.heading("Audio levels");
                ui.add_space(6.0);
                if self.meters.is_empty() {
                    ui.label("No audio streams are enabled.");
                } else {
                    for meter in &mut self.meters {
                        let StreamMeter { peak, rms } = self.levels.level(meter.kind);
                        let rms_target = levels::linear_to_fraction(rms);
                        let peak_target = levels::linear_to_fraction(peak);
                        meter.display_rms =
                            levels::release_toward(meter.display_rms, rms_target, dt);
                        meter.display_peak =
                            levels::release_toward(meter.display_peak, peak_target, dt);
                        draw_meter(
                            ui,
                            meter.kind.title(),
                            meter.display_rms,
                            meter.display_peak,
                            peak,
                        );
                        ui.add_space(6.0);
                    }
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);

                self.editor.draw(ui, &self.cmd_tx);

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);

                self.recent.draw(ui);
            });
        });
    }
}

/// Draw the engine status strip (A4): state, capture target + format, buffer fill,
/// stage/dropped counters, and the last-save result. Values come from a one-shot
/// [`StatusSnapshot`]; the derived text/fraction mappings are pure (`crate::status`)
/// and unit-tested there.
fn draw_status(ui: &mut egui::Ui, s: &StatusSnapshot) {
    ui.heading("Status");
    ui.add_space(6.0);

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

    // Pipeline stage counters (the §6.3 watchdog signal) + dropped frames.
    ui.label(format!(
        "Frames: captured {} · encoded {} · muxed {} · dropped {}",
        s.captured, s.encoded, s.muxed, s.dropped,
    ));

    // Last save result, relative to now.
    ui.label(last_save_line(s));
}

/// A state's label + dot colour, matching the tray's `state_color` palette.
fn state_display(state: TrayState) -> (&'static str, egui::Color32) {
    match state {
        TrayState::Buffering => ("buffering", egui::Color32::from_rgb(0x3f, 0xb9, 0x50)),
        TrayState::Paused => ("paused", egui::Color32::from_rgb(0xc9, 0x9a, 0x24)),
        TrayState::Warning => ("warning", egui::Color32::from_rgb(0xe6, 0x8a, 0x00)),
        TrayState::Error => ("error", egui::Color32::from_rgb(0xd0, 0x3b, 0x2f)),
    }
}

/// A thin filled progress bar for the buffer fill, with the VU meter's theme-adaptive
/// recessed track.
fn draw_status_bar(ui: &mut egui::Ui, fraction: f32) {
    let width = ui.available_width().clamp(80.0, 320.0);
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let track_bg = ui.visuals().extreme_bg_color;
    let painter = ui.painter();
    painter.rect_filled(rect, METER_RADIUS, track_bg);
    let f = fraction.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * f);
        painter.rect_filled(
            fill,
            METER_RADIUS,
            egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
        );
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

/// Green / red used for the editor's save-result line (matches the tray palette).
const OK_GREEN: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);
const ERR_RED: egui::Color32 = egui::Color32::from_rgb(0xd0, 0x3b, 0x2f);

/// The mic-device selection, decoded from/encoded to the `audio.mic` config string
/// (`"default-follow"` / `"off"` / a pinned endpoint id). A5 offers the two policies
/// plus an advanced pinned-id field; a full enumerated device list is a fast-follow
/// once the WASAPI enumeration wrapper is added + HW-validated (DECISIONS "A5").
#[derive(Debug, Clone, PartialEq, Eq)]
enum MicChoice {
    /// Chase the Windows default capture device.
    Follow,
    /// No microphone track.
    Off,
    /// A specific endpoint id (advanced).
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

    /// The combo's selected-text label.
    fn label(&self) -> String {
        match self {
            MicChoice::Follow => "Default (follow)".to_string(),
            MicChoice::Off => "Off (no mic)".to_string(),
            MicChoice::Pinned(id) if id.trim().is_empty() => "Specific device id…".to_string(),
            MicChoice::Pinned(id) => format!("Device: {id}"),
        }
    }
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
    /// The working copy the widgets edit.
    draft: Config,
    /// Mic selection, decoded from `draft.audio.mic` for the picker; re-encoded into
    /// the draft on Save.
    mic: MicChoice,
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
    /// The result of the last Save: `Ok(status line)` or `Err(the validate/IO error)`.
    last_result: Option<Result<String, String>>,
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
        Self {
            draft: base.clone(),
            base,
            mic,
            capturing: None,
            hotkey_ctl,
            hotkey_check: None,
            hotkey_avail: [None, None],
            path,
            last_result: None,
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

    /// Draw the editor and handle a Save click.
    fn draw(&mut self, ui: &mut egui::Ui, cmd_tx: &Sender<EngineCommand>) {
        ui.heading("Settings");
        ui.add_space(6.0);

        self.draw_fields(ui);

        ui.add_space(10.0);
        self.draw_hotkeys(ui);
        ui.add_space(10.0);

        // Derived feedback: the encoder's target Mbps at the chosen res/quality/fps,
        // and the ring RAM the buffer length will reserve (both pure + unit-tested).
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
        let res_note = if matches!(self.draft.encode.resolution, Resolution::Native) {
            " (native ≈ 1080p est.)"
        } else {
            ""
        };
        ui.label(format!(
            "≈ {mbps:.0} Mbps video{res_note} · buffer ≈ {} s / {:.0} MiB RAM",
            self.draft.buffer.seconds, ram
        ));

        ui.add_space(10.0);

        if ui.button("Save settings").clicked() {
            self.save(cmd_tx);
        }
        if let Some(result) = &self.last_result {
            match result {
                Ok(msg) => ui.colored_label(OK_GREEN, msg),
                Err(err) => ui.colored_label(ERR_RED, format!("Invalid: {err}")),
            };
        }
    }

    /// The two-column label/widget grid. Straight-line egui widget binding — each row
    /// edits one `draft` field in place; the mic row reveals a pinned-id text field
    /// when "Specific device id…" is chosen.
    fn draw_fields(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings_editor")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("Quality");
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(quality_label(self.draft.encode.quality))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.draft.encode.quality,
                            Quality::Efficient,
                            "Efficient",
                        );
                        ui.selectable_value(
                            &mut self.draft.encode.quality,
                            Quality::Default,
                            "Default",
                        );
                        ui.selectable_value(&mut self.draft.encode.quality, Quality::High, "High");
                        ui.selectable_value(&mut self.draft.encode.quality, Quality::Max, "Max");
                    });
                ui.end_row();

                ui.label("Resolution");
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(resolution_label(self.draft.encode.resolution))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.draft.encode.resolution,
                            Resolution::Native,
                            "Source (native)",
                        );
                        ui.selectable_value(
                            &mut self.draft.encode.resolution,
                            Resolution::P1440,
                            "1440p",
                        );
                        ui.selectable_value(
                            &mut self.draft.encode.resolution,
                            Resolution::P1080,
                            "1080p",
                        );
                        ui.selectable_value(
                            &mut self.draft.encode.resolution,
                            Resolution::P720,
                            "720p",
                        );
                    });
                ui.end_row();

                ui.label("Frame rate");
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(format!("{} fps", self.draft.capture.fps))
                    .show_ui(ui, |ui| {
                        // 30/60 only; 120 stays gated behind M6 (M7-M8-PLAN §3 / §1.2).
                        ui.selectable_value(&mut self.draft.capture.fps, 30, "30 fps");
                        ui.selectable_value(&mut self.draft.capture.fps, 60, "60 fps");
                    });
                ui.end_row();

                ui.label("Buffer length");
                ui.add(
                    egui::DragValue::new(&mut self.draft.buffer.seconds)
                        .range(1..=MAX_BUFFER_SECONDS)
                        .suffix(" s"),
                );
                ui.end_row();

                ui.label("Output folder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.output.dir)
                        .hint_text("OS Videos folder"),
                );
                ui.end_row();

                ui.label("Clear buffer after save");
                ui.checkbox(&mut self.draft.buffer.clear_after_save, "");
                ui.end_row();

                ui.label("Desktop audio");
                ui.checkbox(&mut self.draft.audio.desktop, "");
                ui.end_row();

                ui.label("Microphone");
                egui::ComboBox::from_id_salt("mic")
                    .selected_text(self.mic.label())
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                matches!(self.mic, MicChoice::Follow),
                                "Default (follow)",
                            )
                            .clicked()
                        {
                            self.mic = MicChoice::Follow;
                        }
                        if ui
                            .selectable_label(matches!(self.mic, MicChoice::Off), "Off (no mic)")
                            .clicked()
                        {
                            self.mic = MicChoice::Off;
                        }
                        if ui
                            .selectable_label(
                                matches!(self.mic, MicChoice::Pinned(_)),
                                "Specific device id…",
                            )
                            .clicked()
                            && !matches!(self.mic, MicChoice::Pinned(_))
                        {
                            self.mic = MicChoice::Pinned(String::new());
                        }
                    });
                ui.end_row();

                if let MicChoice::Pinned(id) = &mut self.mic {
                    ui.label("Device id");
                    ui.text_edit_singleline(id);
                    ui.end_row();
                }
            });
    }

    /// The hotkey press-to-bind rows (A6). While a row is capturing, the next valid
    /// combo pressed is written into the draft (Esc cancels); otherwise the row shows
    /// the current binding + a Rebind button. Persisted like the other fields (via
    /// `write_atomic`); re-registration happens on restart (restart-noted).
    fn draw_hotkeys(&mut self, ui: &mut egui::Ui) {
        // If a capture is active, consume this frame's key events first. Note: the
        // OS-global hotkey stays registered while capturing, so pressing the CURRENT
        // save/record combo here still fires the real global action (a save/record) —
        // an accepted v0 limitation of rebinding system-wide hotkeys (DECISIONS "A6").
        if let Some(target) = self.capturing {
            match ui.input_mut(capture_combo) {
                Some(CaptureResult::Cancel) => self.capturing = None,
                Some(CaptureResult::Bound(combo)) => {
                    match target {
                        HotkeyTarget::Save => self.draft.hotkeys.save_clip = combo.clone(),
                        HotkeyTarget::Record => self.draft.hotkeys.record_toggle = combo.clone(),
                    }
                    // Live-check whether another app already owns the new combo (A6
                    // fast-follow); the reply lands via `poll_availability`.
                    self.start_availability_check(target, &combo);
                    self.capturing = None;
                }
                None => {}
            }
        }

        ui.label(egui::RichText::new("Hotkeys").strong());
        ui.add_space(2.0);
        egui::Grid::new("hotkeys_grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                self.hotkey_row(ui, HotkeyTarget::Save, "Save clip");
                self.hotkey_row(ui, HotkeyTarget::Record, "Record toggle");
            });
        // Keep processing key events while capturing (the meter refresh already drives
        // repaints, but request one so capture is responsive even if meters are idle).
        if self.capturing.is_some() {
            ui.ctx().request_repaint();
        }
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
        ui.horizontal(|ui| {
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
            if ui.button("Rebind").clicked() {
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

    /// Validate + write the draft, hot-apply the one safe field, and record the
    /// result. On a validation failure nothing is written and the exact
    /// `Config::validate` error (same text `--check-config` prints) is shown.
    fn save(&mut self, cmd_tx: &Sender<EngineCommand>) {
        self.draft.audio.mic = self.mic.to_cfg();

        // Hotkeys are registered by the pump at startup, not checked by
        // `Config::validate`; guard them here so a Save can never brick the config
        // with an unparseable combo (fatal to buffer mode at next start) or a
        // self-conflict (the second registration silently loses).
        if let Err(e) = self.validate_hotkeys() {
            warn!(error = %e, "settings not saved — invalid hotkey");
            self.last_result = Some(Err(e));
            return;
        }

        if let Err(e) = self.draft.validate() {
            warn!(error = %e, "settings not saved — invalid");
            self.last_result = Some(Err(e.to_string()));
            return;
        }
        // Verify the output folder is usable BEFORE writing config, so a mistyped path
        // is rejected here instead of turning every later clip save into a silent I/O
        // failure. Per the orchestrator's call (2026-07-08): create it if missing,
        // reject only if uncreatable. An empty field resolves to the OS Videos default,
        // which is created here too.
        if let Err(e) = self.validate_output_dir() {
            warn!(error = %e, "settings not saved — output folder unusable");
            self.last_result = Some(Err(e));
            return;
        }
        if let Err(e) = self.draft.write_atomic(&self.path) {
            warn!(error = %e, "settings write failed");
            self.last_result = Some(Err(e.to_string()));
            return;
        }

        // Hot-apply the one field safe to change live; the rest need a restart.
        if self.draft.buffer.clear_after_save != self.base.buffer.clear_after_save {
            let _ = cmd_tx.send(EngineCommand::SetClearAfterSave(
                self.draft.buffer.clear_after_save,
            ));
        }
        let restart = self.restart_required_fields();
        self.base = self.draft.clone();
        self.mic = MicChoice::from_cfg(&self.base.audio.mic);
        let msg = if restart.is_empty() {
            format!("Saved to {}.", self.path.display())
        } else {
            format!("Saved. Restart clipd to apply: {}.", restart.join(", "))
        };
        info!(path = %self.path.display(), restart = ?restart, "settings saved");
        self.last_result = Some(Ok(msg));
    }

    /// The human names of the fields that changed between `base` and `draft` and
    /// need a restart to take effect (everything except the hot-applied
    /// clear-after-save).
    fn restart_required_fields(&self) -> Vec<&'static str> {
        let (a, b) = (&self.base, &self.draft);
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
        if a.buffer.seconds != b.buffer.seconds {
            v.push("buffer length");
        }
        if a.output.dir != b.output.dir {
            v.push("output folder");
        }
        if a.audio.desktop != b.audio.desktop {
            v.push("desktop audio");
        }
        if a.audio.mic != b.audio.mic {
            v.push("microphone");
        }
        if a.hotkeys.save_clip != b.hotkeys.save_clip
            || a.hotkeys.record_toggle != b.hotkeys.record_toggle
        {
            v.push("hotkeys");
        }
        v
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
    fn validate_output_dir(&self) -> Result<(), String> {
        let dir = config::resolve_output_dir(&self.draft.output.dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("output folder: {} — {e}", dir.display()))
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
        egui::Color32::from_rgb(0xd0, 0x3b, 0x2f) // red — near/at clip
    } else if fraction >= 0.8 {
        egui::Color32::from_rgb(0xc9, 0x9a, 0x24) // amber — hot
    } else {
        egui::Color32::from_rgb(0x3f, 0xb9, 0x50) // green — nominal
    }
}

/// Draw one VU meter row: `title` label, a track with an RMS body fill and a peak
/// marker, and a compact peak-dBFS readout. `rms_frac`/`peak_frac` are the smoothed
/// 0..=1 bar fractions; `peak_amp` is the raw linear peak for the dB readout.
fn draw_meter(ui: &mut egui::Ui, title: &str, rms_frac: f32, peak_frac: f32, peak_amp: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([90.0, METER_HEIGHT], egui::Label::new(title));

        // Reserve room for the dB readout on the right; the bar takes the rest.
        let bar_w = (ui.available_width() - 64.0).max(80.0);
        let (rect, _resp) =
            ui.allocate_exact_size(egui::vec2(bar_w, METER_HEIGHT), egui::Sense::hover());
        // Theme-adaptive chrome (eframe may follow a system light theme): a recessed
        // well for the track, the strong text colour for the peak tick.
        let track_bg = ui.visuals().extreme_bg_color;
        let marker_col = ui.visuals().strong_text_color();
        let painter = ui.painter();
        // Track background.
        painter.rect_filled(rect, METER_RADIUS, track_bg);
        // RMS body.
        if rms_frac > 0.0 {
            let mut fill = rect;
            fill.set_width(rect.width() * rms_frac.min(1.0));
            painter.rect_filled(fill, METER_RADIUS, meter_color(rms_frac));
        }
        // Peak marker: a thin bright bar at the peak position.
        if peak_frac > 0.0 {
            let x = rect.left() + rect.width() * peak_frac.min(1.0);
            let marker = egui::Rect::from_min_max(
                egui::pos2((x - 1.5).max(rect.left()), rect.top()),
                egui::pos2((x + 1.5).min(rect.right()), rect.bottom()),
            );
            painter.rect_filled(marker, 0.0, marker_col);
        }

        // Peak dBFS readout. At/below the floor, show the floor symbolically.
        let db = levels::linear_to_dbfs(peak_amp);
        let text = if db <= levels::METER_FLOOR_DBFS {
            "  −∞ dB".to_string()
        } else {
            format!("{db:>5.1} dB")
        };
        ui.monospace(text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
            draft: Config::default(),
            mic: MicChoice::Follow,
            capturing: None,
            hotkey_ctl: None,
            hotkey_check: None,
            hotkey_avail: [None, None],
            path,
            last_result: None,
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
    fn restart_fields_includes_hotkeys_when_changed() {
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        ed.draft.hotkeys.save_clip = "Ctrl+Alt+KeyP".to_string();
        assert!(ed.restart_required_fields().contains(&"hotkeys"));
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
        // Change all eight restart-required fields; each must appear, in order.
        let mut ed = test_editor(PathBuf::from("unused.toml"));
        ed.draft.encode.quality = Quality::Max;
        ed.draft.encode.resolution = Resolution::P720;
        ed.draft.capture.fps = 30;
        ed.draft.buffer.seconds = ed.base.buffer.seconds + 5;
        ed.draft.output.dir = "D:/clips".to_string();
        ed.draft.audio.desktop = !ed.base.audio.desktop;
        ed.draft.audio.mic = "off".to_string();
        ed.draft.hotkeys.save_clip = "Ctrl+Alt+KeyP".to_string();
        assert_eq!(
            ed.restart_required_fields(),
            vec![
                "quality",
                "resolution",
                "frame rate",
                "buffer length",
                "output folder",
                "desktop audio",
                "microphone",
                "hotkeys",
            ]
        );
    }

    #[test]
    fn save_valid_writes_file_hot_applies_and_syncs_base() {
        let dir = std::env::temp_dir().join(format!("clipd_a5_save_ok_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let (tx, rx) = crossbeam_channel::bounded::<EngineCommand>(4);
        let mut ed = test_editor(path.clone());
        // Point the output folder at the temp dir so the Save's create_dir_all check
        // doesn't materialise the real %USERPROFILE%\Videos\clipd default as a side
        // effect. (This also exercises the changed "output folder" restart field.)
        ed.draft.output.dir = dir.to_string_lossy().into_owned();
        // A restart field + the hot-swap field both change.
        ed.draft.encode.quality = Quality::High;
        let new_clear = !ed.base.buffer.clear_after_save;
        ed.draft.buffer.clear_after_save = new_clear;

        ed.save(&tx);

        // A valid, reloadable file was written.
        assert!(path.exists());
        assert!(Config::load(&path).is_ok());
        // The result reports success (and names the restart-required change).
        match &ed.last_result {
            Some(Ok(msg)) => assert!(msg.contains("quality"), "msg = {msg}"),
            other => panic!("expected Ok(_), got {other:?}"),
        }
        // The hot-swap command was pushed for the changed clear-after-save.
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineCommand::SetClearAfterSave(v)) if v == new_clear
        ));
        // base is now in sync with the saved draft.
        assert_eq!(ed.base, ed.draft);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn save_invalid_writes_nothing_and_reports_the_error() {
        let path =
            std::env::temp_dir().join(format!("clipd_a5_save_bad_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let (tx, rx) = crossbeam_channel::bounded::<EngineCommand>(4);
        let mut ed = test_editor(path.clone());
        // An empty pinned id → audio.mic = "" → Config::validate rejects it.
        ed.mic = MicChoice::Pinned(String::new());

        ed.save(&tx);

        assert!(matches!(ed.last_result, Some(Err(_))));
        assert!(!path.exists(), "an invalid save must not write the file");
        assert!(rx.try_recv().is_err(), "no hot-swap on a rejected save");
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
