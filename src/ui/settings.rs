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
//! A2 renders a deliberately minimal placeholder; the settings *editor* (quality
//! tier, resolution, devices, …) lands in A5 and writes exclusively through the
//! A1 `Config::write_atomic` path. No config is read or written here yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use eframe::egui;
use tracing::{info, warn};

use crate::audio::levels::{self, AudioLevels, StreamMeter};
use crate::audio::wasapi_stream::AudioStreamKind;
use crate::engine::EngineCommand;
use crate::spec_constants::PRODUCT_NAME;

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
}

impl Shared {
    fn new() -> Self {
        Self {
            ctx: Mutex::new(None),
            quit: AtomicBool::new(false),
            // The window opens visible on creation.
            visible: AtomicBool::new(true),
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
        streams: &[AudioStreamKind],
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
            // visibility flag first so the woken frame resumes meter animation.
            running.shared.visible.store(true, Ordering::Relaxed);
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
            std::thread::Builder::new()
                .name("settings-ui".to_string())
                .spawn(move || run_window(shared, cmd_tx, levels, streams, opened_at))
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
fn run_window(
    shared: Arc<Shared>,
    cmd_tx: Sender<EngineCommand>,
    levels: Arc<AudioLevels>,
    streams: Vec<AudioStreamKind>,
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
                app_shared, cmd_tx, levels, streams, opened_at,
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
    kind: AudioStreamKind,
    /// Displayed RMS bar fill (0..=1), decayed.
    display_rms: f32,
    /// Displayed peak marker position (0..=1), decayed.
    display_peak: f32,
}

impl MeterState {
    fn new(kind: AudioStreamKind) -> Self {
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
    /// Engine command channel — unused for now, held for A5's editor and A6's
    /// hotkey rebinds so the wiring is already in place.
    _cmd_tx: Sender<EngineCommand>,
    /// Lock-free audio levels published by the engine's audio threads (A3). Read
    /// only; never written here (engine → UI).
    levels: Arc<AudioLevels>,
    /// One animated meter per enabled audio stream, in engine order.
    meters: Vec<MeterState>,
    /// When the tray requested the open — used once to log the cold-open latency
    /// against the M7 < 300 ms budget.
    opened_at: Instant,
    /// Whether the first-frame one-time work (the cold-open log) has run.
    started: bool,
}

impl SettingsApp {
    fn new(
        shared: Arc<Shared>,
        cmd_tx: Sender<EngineCommand>,
        levels: Arc<AudioLevels>,
        streams: Vec<AudioStreamKind>,
        opened_at: Instant,
    ) -> Self {
        let meters = streams.into_iter().map(MeterState::new).collect();
        Self {
            shared,
            _cmd_tx: cmd_tx,
            levels,
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
            ui.heading(format!("{PRODUCT_NAME} settings"));
            ui.label(format!("version {VERSION}"));
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
                    meter.display_rms = levels::release_toward(meter.display_rms, rms_target, dt);
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
            ui.add_space(6.0);
            ui.label("Status, and the settings editor arrive in A4–A5.");
        });
    }
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
