//! `ui::tray` — the tray shell (M5). Tray icon + native menu + a main-thread
//! message pump that translates menu clicks into [`EngineCommand`]s and reflects
//! [`ShellSignal`]s on the icon/tooltip. The "Settings…" item lazily opens the
//! satellite window (`ui::settings`).
//!
//! ## Satellite rule (`08-FEATURE-COMPLETE.md`, applied early)
//! This module depends on engine *types* and never the reverse; the engine runs
//! fully without a shell (the `record` subcommand and the hidden `--autosave` /
//! `--record-secs` hooks never build one). Everything here is **main-thread
//! only** — muda/`tray-icon` handles are `!Send` `Rc`s and the message pump must
//! run on the thread that owns the tray's hidden window. The settings window runs
//! on its OWN thread (spawned lazily by [`SettingsHandle`]) so it never shares
//! this pump.
//!
//! ## `unsafe` / threading
//! The only `unsafe` is the standard non-blocking Win32 message pump (confined
//! here per `CLAUDE.md`, with a `SAFETY:` note). The state/mapping logic is pure
//! and unit-tested.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tracing::{info, warn};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIcon, DispatchMessageW, PeekMessageW, TranslateMessage, HICON, MSG, PM_REMOVE, WM_QUIT,
};

use super::notify::TrayWindow;
use super::settings::SettingsHandle;
use super::theme;
use crate::audio::levels::AudioLevels;
use crate::audio::wasapi_stream::AudioTrackKind;
use crate::engine::{BufferEngine, EngineCommand, ShellSignal, TrayState};
use crate::hotkey::HotkeyControl;
use crate::spec_constants::PRODUCT_NAME;
use crate::status::EngineStatus;

/// Stable menu-item ids (compared against [`MenuEvent`]'s `id`). Kept as `&str`
/// constants so the mapping is pure and unit-testable.
const ID_SAVE: &str = "save";
const ID_PAUSE: &str = "pause";
const ID_RECORD: &str = "record";
const ID_SETTINGS: &str = "settings";
const ID_OPEN: &str = "open";
const ID_AUTOSTART: &str = "autostart";
const ID_QUIT: &str = "quit";

/// Shell loop cadence: pump messages + drain channels this often. 30 ms keeps the
/// tray responsive without busy-spinning the main thread.
const POLL_INTERVAL: Duration = Duration::from_millis(30);

/// Tray icon edge in pixels — a small solid-colour status square.
const ICON_SIZE: u32 = 32;

/// Errors from building the tray shell.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    /// Building a menu item failed (muda).
    #[error("building the tray menu: {0}")]
    Menu(#[from] muda::Error),
    /// Building the icon image (`CreateIcon` from our RGBA glyph) failed.
    #[error("building the tray icon image: {0}")]
    Icon(#[source] windows::core::Error),
    /// Creating the tray window + notification icon (`Shell_NotifyIcon(NIM_ADD)`) failed.
    #[error("creating the tray window / notification icon")]
    Window,
}

/// How the tray shell loop ended (U7). `Quit` = normal teardown (menu Quit / a worker
/// died); `Restart` = the settings window's auto-restart banner asked to relaunch. Lives
/// here in `ui` (not `engine`) so no engine→ui dependency is introduced — `main.rs`, which
/// owns process lifecycle, matches on it and spawns a fresh instance after teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOutcome {
    /// Quit the process (no relaunch).
    Quit,
    /// Relaunch the process to apply restart-required settings.
    Restart,
}

/// The action a menu-item id maps to. Pure (no side effects), so the click →
/// action mapping is unit-testable without a live tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    /// Save the last N seconds.
    Save,
    /// Toggle the paused state.
    TogglePause,
    /// Start/stop the timed recording.
    ToggleRecord,
    /// Open (or re-show) the settings window.
    OpenSettings,
    /// Open the clips folder in the file manager.
    OpenFolder,
    /// Toggle the start-with-Windows (HKCU Run key) entry.
    ToggleAutostart,
    /// Quit the app.
    Quit,
}

/// Map a menu id to its [`MenuAction`] (pure). `None` for an unrecognized id.
fn menu_action(id: &MenuId) -> Option<MenuAction> {
    if id == ID_SAVE {
        Some(MenuAction::Save)
    } else if id == ID_PAUSE {
        Some(MenuAction::TogglePause)
    } else if id == ID_RECORD {
        Some(MenuAction::ToggleRecord)
    } else if id == ID_SETTINGS {
        Some(MenuAction::OpenSettings)
    } else if id == ID_OPEN {
        Some(MenuAction::OpenFolder)
    } else if id == ID_AUTOSTART {
        Some(MenuAction::ToggleAutostart)
    } else if id == ID_QUIT {
        Some(MenuAction::Quit)
    } else {
        None
    }
}

/// The RGBA chip colour for each tray state — the single place to re-theme the tray,
/// drawn from the value-harmonised semantic palette (`theme`). Brand-forward (D-U4):
/// healthy/buffering is the lavender accent ("here, quietly working"); the warm colours
/// are reserved for the attention states. `01-PROJECT-PLAN.md §5.5`.
fn state_color(state: TrayState) -> [u8; 4] {
    match state {
        TrayState::Buffering => theme::ACCENT.to_array(), // lavender — healthy
        TrayState::Paused => theme::AMBER.to_array(),
        TrayState::Warning => theme::WARN.to_array(),
        TrayState::Error => theme::BAD.to_array(),
    }
}

/// The raw RGBA pixels for a state's icon — the procedural "last-slice" glyph tinted
/// with the state colour (`theme::glyph_rgba`), replacing the old solid fill (U3). Pure,
/// so it is unit-testable without touching Win32/GDI.
fn icon_rgba(state: TrayState) -> Vec<u8> {
    theme::glyph_rgba(state_color(state), ICON_SIZE)
}

/// Build a Win32 [`HICON`] for `state` from the procedural glyph (P1a: we own the icon now,
/// so we build the `HICON` directly instead of via the dropped `tray-icon` crate).
///
/// The pixel producer ([`theme::glyph_rgba`]) is the ONLY thing that changes when the
/// designed SVG + embedded `.ico` art lands at M10 — this seam and its `icon_for(state)`
/// entry point stay, so there is no call-site churn (DECISIONS.md 2026-07-06 "M5 plan").
/// The caller owns the returned `HICON` and must `DestroyIcon` it ([`TrayWindow`] does).
fn icon_for(state: TrayState) -> Result<HICON, windows::core::Error> {
    hicon_from_rgba(&icon_rgba(state), ICON_SIZE)
}

/// Convert 32bpp RGBA pixels into an `HICON` via `CreateIcon`, using the exact conversion
/// the (now-dropped) `tray-icon`/`muda` path used — winit's `into_windows_icon`: the AND
/// mask is the inverted alpha, the XOR bits are BGRA. So the M5-verified glyph renders
/// identically. Confined-unsafe (a single FFI call over buffers we own).
fn hicon_from_rgba(rgba: &[u8], size: u32) -> Result<HICON, windows::core::Error> {
    let pixel_count = rgba.len() / 4;
    let mut bgra = rgba.to_vec();
    let mut and_mask = Vec::with_capacity(pixel_count);
    for px in bgra.chunks_exact_mut(4) {
        and_mask.push(px[3].wrapping_sub(u8::MAX)); // invert alpha into the AND mask
        px.swap(0, 2); // RGBA -> BGRA
    }
    // SAFETY: `CreateIcon` reads `size*size` pixels from each buffer; `and_mask` holds one
    // byte per pixel and `bgra` four, both sized from `rgba`. No pointer escapes the call;
    // the returned `HICON` is owned by the caller.
    unsafe {
        CreateIcon(
            None,
            size as i32,
            size as i32,
            1,
            32,
            and_mask.as_ptr(),
            bgra.as_ptr(),
        )
    }
}

/// The tooltip text for a state.
fn tooltip(state: TrayState, recording: bool) -> String {
    let s = match state {
        TrayState::Buffering => "buffering",
        TrayState::Paused => "paused",
        TrayState::Warning => "warning — check the log",
        TrayState::Error => "error — capture stopped",
    };
    // Recording is orthogonal to the four states (U8): append it rather than making it a
    // fifth state, so buffering/paused/warning/error keep meaning what they mean.
    let rec = if recording { " · recording" } else { "" };
    format!("{PRODUCT_NAME} — {s}{rec}")
}

/// The tray shell: owns the tray icon + menu handles and drives the pump loop.
/// Main-thread only (its `tray-icon`/muda members are `!Send`).
pub struct Shell {
    /// clipd's ONE tray window + visible notification icon (P1a): state glyph, tooltip,
    /// menu-on-click, and the save-complete/-failed balloon (with click routing) — all on a
    /// WNDPROC we own. Replaces the old `tray-icon` icon + separate hidden `Notifier`.
    window: TrayWindow,
    /// The muda context menu shown on an icon click. Held so it (and its items) stay alive;
    /// [`TrayWindow`] holds a cheap clone for its WNDPROC to pop.
    _menu: Menu,
    /// Held so its checkmark can be toggled on pause; also keeps the item alive.
    pause_item: CheckMenuItem,
    /// Held so its label flips "Start recording" ⇄ "Stop recording" with the engine's
    /// live recording state (U8); also keeps the item alive.
    record_item: MenuItem,
    /// Held so its checkmark reflects the HKCU Run-key state.
    autostart_item: CheckMenuItem,
    /// Command channel to the engine (tray → ring thread). Cloned to the settings
    /// window so it injects the same [`EngineCommand`]s.
    cmd_tx: Sender<EngineCommand>,
    /// The lazily-spawned satellite settings window (A2).
    settings: SettingsHandle,
    /// Lock-free audio levels for the settings window's VU meters (A3), handed to
    /// the window on open. Read-only here (engine → UI).
    levels: Arc<AudioLevels>,
    /// The audio tracks to draw meters for (the engine's spawnable set — Mix/Mic in
    /// B1), from the engine.
    audio_streams: Vec<AudioTrackKind>,
    /// Lock-free engine status for the settings window's status strip (A4), handed to
    /// the window on open. Read-only here (engine → UI).
    status: Arc<EngineStatus>,
    /// Where saved clips land — for "Open clips folder".
    output_dir: PathBuf,
    /// Control handle for the settings editor's live hotkey-availability check (A6
    /// fast-follow), handed to the window on open. Cheap clone of a channel sender.
    hotkey_ctl: HotkeyControl,
    /// The current tray state (to skip redundant icon updates).
    state: TrayState,
    /// Whether the user has paused buffering.
    paused: bool,
    /// The last-reflected recording state (U8), to skip redundant menu/tooltip updates.
    recording: bool,
    /// Whether start-with-Windows is currently enabled (mirrors the Run key).
    autostart_enabled: bool,
    /// Set by [`Self::open_settings_on_start`] (T2): auto-open the settings window on the
    /// first loop iteration after an auto-restart, so it doesn't appear to have vanished.
    open_on_start: bool,
}

impl Shell {
    /// Build the tray icon + menu. `cmd_tx` comes from
    /// [`BufferEngine::command_sender`]; `output_dir` is the clips directory;
    /// `levels`/`audio_streams` come from the engine and feed the settings-window
    /// VU meters (A3); `status` feeds its status strip (A4); `hotkey_ctl` backs the
    /// editor's live "combo already taken" check (A6 fast-follow).
    pub fn new(
        cmd_tx: Sender<EngineCommand>,
        output_dir: PathBuf,
        levels: Arc<AudioLevels>,
        audio_streams: Vec<AudioTrackKind>,
        status: Arc<EngineStatus>,
        hotkey_ctl: HotkeyControl,
    ) -> Result<Self, ShellError> {
        // Reflect the current HKCU Run-key state on the checkbox at build time.
        let autostart_enabled = crate::autostart::is_enabled();

        let menu = Menu::new();
        let save = MenuItem::with_id(ID_SAVE, "Save clip", true, None);
        let pause_item = CheckMenuItem::with_id(ID_PAUSE, "Pause buffering", true, false, None);
        // Label starts at "Start recording"; U8 flips it to "Stop recording" while a
        // timed recording is running.
        let record_item = MenuItem::with_id(ID_RECORD, "Start recording", true, None);
        let settings = MenuItem::with_id(ID_SETTINGS, "Settings…", true, None);
        let open = MenuItem::with_id(ID_OPEN, "Open clips folder", true, None);
        let autostart_item = CheckMenuItem::with_id(
            ID_AUTOSTART,
            "Start with Windows",
            true,
            autostart_enabled,
            None,
        );
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        menu.append(&save)?;
        menu.append(&pause_item)?;
        menu.append(&record_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&settings)?;
        menu.append(&open)?;
        menu.append(&autostart_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let state = TrayState::Buffering;
        // Build our own HICON + the single visible tray window carrying it (P1a). The window
        // holds a clone of the muda menu so its WNDPROC can pop it on an icon click.
        let icon = icon_for(state).map_err(ShellError::Icon)?;
        let window = TrayWindow::new(icon, &tooltip(state, false), menu.clone())
            .ok_or(ShellError::Window)?;

        Ok(Self {
            window,
            _menu: menu,
            pause_item,
            record_item,
            autostart_item,
            cmd_tx,
            settings: SettingsHandle::default(),
            levels,
            audio_streams,
            status,
            output_dir,
            hotkey_ctl,
            state,
            paused: false,
            recording: false,
            autostart_enabled,
            open_on_start: false,
        })
    }

    /// Request that the settings window auto-open on the first loop iteration (T2 — after
    /// an auto-restart, so the window doesn't appear to have vanished).
    pub fn open_settings_on_start(&mut self) {
        self.open_on_start = true;
    }

    /// Open (or re-show) the settings window — the shared path for the tray menu item and
    /// the post-restart auto-open.
    fn open_settings(&mut self) {
        self.settings.open(
            &self.cmd_tx,
            &self.levels,
            &self.audio_streams,
            &self.status,
            &self.output_dir,
            &self.hotkey_ctl,
        );
    }

    /// Run the shell loop on the calling (main) thread until the user picks Quit, the
    /// settings window requests a restart, or the engine session ends. Pumps Win32
    /// messages (so menu clicks arrive), maps them to [`EngineCommand`]s, and reflects
    /// [`ShellSignal`]s on the tray. Returns how the loop ended so `run_buffer` can
    /// relaunch on [`ShellOutcome::Restart`] (after its own teardown releases the
    /// hotkeys/devices — see the U7 ordering in `main.rs`).
    pub fn run(&mut self, engine: &BufferEngine) -> ShellOutcome {
        // After an auto-restart, re-open the settings window and confirm — so the window
        // never appears to have vanished (which reads as a crash), T2.
        if self.open_on_start {
            self.open_on_start = false;
            self.open_settings();
            self.window.info(
                &format!("{PRODUCT_NAME} restarted"),
                "Your new settings are now active.",
                &self.output_dir,
            );
        }
        loop {
            pump_messages();

            // Menu clicks → engine commands. muda posts to a global receiver.
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if self.handle_menu(&event.id) {
                    // Quit: close the settings window, ask the engine to wind down,
                    // then leave the loop.
                    self.shutdown_settings_and_engine();
                    return ShellOutcome::Quit;
                }
            }

            // The settings window's auto-restart banner asked to relaunch (U7). Tear
            // down exactly as for Quit — but return `Restart` so `run_buffer` spawns a
            // fresh instance once the hotkeys + devices are released.
            if self.settings.restart_requested() {
                info!("restart requested from settings; tearing down to relaunch");
                self.shutdown_settings_and_engine();
                return ShellOutcome::Restart;
            }

            // Engine → tray.
            while let Ok(signal) = engine.signals().try_recv() {
                match signal {
                    ShellSignal::State(state) => self.set_state(state),
                    // Save outcome → the T1 balloon (survives the settings window being
                    // closed; renders over borderless-fullscreen with the LIMITATIONS
                    // caveat). Click opens the clip folder on success, the log folder on
                    // failure.
                    ShellSignal::Saved {
                        ok,
                        seconds,
                        folder,
                        reason,
                    } => {
                        let click_dir = if ok {
                            folder
                        } else {
                            crate::logging::log_dir()
                        };
                        self.window.saved(ok, seconds, &reason, &click_dir);
                    }
                }
            }

            // The session ended on its own (fatal error / worker died): show Error,
            // tear down the settings window, and hand back — `run_buffer` will join
            // and surface the error.
            if engine.any_worker_finished() {
                self.set_state(TrayState::Error);
                self.settings.shutdown();
                return ShellOutcome::Quit;
            }

            // Reflect the recording label + tooltip (U8); the save balloon is handled
            // above via ShellSignal::Saved (T1).
            self.poll_status();

            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Shared teardown for Quit and Restart: close the settings window and ask the
    /// engine to wind down.
    fn shutdown_settings_and_engine(&mut self) {
        self.settings.shutdown();
        let _ = self.cmd_tx.send(EngineCommand::Shutdown);
    }

    /// Handle one menu click. Returns `true` if it was Quit (caller should exit).
    fn handle_menu(&mut self, id: &MenuId) -> bool {
        match menu_action(id) {
            Some(MenuAction::Save) => {
                let _ = self.cmd_tx.send(EngineCommand::SaveClip);
            }
            Some(MenuAction::TogglePause) => {
                self.paused = !self.paused;
                self.pause_item.set_checked(self.paused);
                let _ = self.cmd_tx.send(EngineCommand::SetPaused(self.paused));
            }
            Some(MenuAction::ToggleRecord) => {
                let _ = self.cmd_tx.send(EngineCommand::ToggleRecord);
            }
            Some(MenuAction::OpenSettings) => self.open_settings(),
            Some(MenuAction::OpenFolder) => self.open_folder(),
            Some(MenuAction::ToggleAutostart) => self.toggle_autostart(),
            Some(MenuAction::Quit) => return true,
            None => {}
        }
        false
    }

    /// Toggle the start-with-Windows Run-key entry, then force the checkmark to
    /// match the resulting truth (so a failed write leaves the box unchecked).
    fn toggle_autostart(&mut self) {
        let want = !self.autostart_enabled;
        match crate::autostart::set_enabled(want) {
            Ok(()) => {
                self.autostart_enabled = want;
                info!(enabled = want, "start-with-Windows toggled");
            }
            Err(e) => warn!(error = %e, "could not change start-with-Windows"),
        }
        self.autostart_item.set_checked(self.autostart_enabled);
    }

    /// Update the tray icon + tooltip for a new state (skip if unchanged).
    fn set_state(&mut self, state: TrayState) {
        if state == self.state {
            return;
        }
        self.state = state;
        match icon_for(state) {
            Ok(icon) => self.window.set_icon(icon),
            Err(e) => warn!(error = %e, "could not build the tray icon image"),
        }
        self.refresh_tooltip();
    }

    /// Set the tray tooltip from the current state + recording (U8).
    fn refresh_tooltip(&self) {
        self.window
            .set_tooltip(&tooltip(self.state, self.recording));
    }

    /// Reflect the engine's live recording state on the tray (U8): flip the menu label
    /// and refresh the tooltip suffix. Skips if unchanged (called from [`Self::poll_status`]).
    fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
        self.record_item.set_text(if recording {
            "Stop recording"
        } else {
            "Start recording"
        });
        self.refresh_tooltip();
    }

    /// Poll the engine status the tray reflects itself: the recording label/tooltip (U8).
    /// The save balloon is now signal-driven (`ShellSignal::Saved`, T1), not polled.
    fn poll_status(&mut self) {
        let s = self.status.snapshot();
        if s.recording != self.recording {
            self.set_recording(s.recording);
        }
    }

    /// Open the clips folder in Explorer. `explorer` returns a non-zero exit code
    /// even on success, so we only care that the spawn launched.
    fn open_folder(&self) {
        match std::process::Command::new("explorer")
            .arg(&self.output_dir)
            .spawn()
        {
            Ok(_) => info!(dir = %self.output_dir.display(), "opened clips folder"),
            Err(e) => warn!(error = %e, "could not open the clips folder"),
        }
    }
}

/// Drain and dispatch all pending Win32 messages (non-blocking), so `tray-icon`
/// and muda's window procs run and post menu/tray events to their global receivers.
fn pump_messages() {
    // SAFETY: a standard non-blocking Win32 message pump on the thread that owns
    // the tray's hidden message window. `PeekMessageW(PM_REMOVE)` removes each
    // pending message; `DispatchMessageW` routes it to the owning window proc.
    // `MSG` is a plain POD we own; no message data escapes this function.
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(menu_action(&MenuId::new(ID_SAVE)), Some(MenuAction::Save));
        assert_eq!(
            menu_action(&MenuId::new(ID_PAUSE)),
            Some(MenuAction::TogglePause)
        );
        assert_eq!(
            menu_action(&MenuId::new(ID_RECORD)),
            Some(MenuAction::ToggleRecord)
        );
        assert_eq!(
            menu_action(&MenuId::new(ID_SETTINGS)),
            Some(MenuAction::OpenSettings)
        );
        assert_eq!(
            menu_action(&MenuId::new(ID_OPEN)),
            Some(MenuAction::OpenFolder)
        );
        assert_eq!(
            menu_action(&MenuId::new(ID_AUTOSTART)),
            Some(MenuAction::ToggleAutostart)
        );
        assert_eq!(menu_action(&MenuId::new(ID_QUIT)), Some(MenuAction::Quit));
    }

    #[test]
    fn unknown_menu_id_maps_to_nothing() {
        assert_eq!(menu_action(&MenuId::new("bogus")), None);
    }

    #[test]
    fn each_state_has_a_distinct_colour() {
        let states = [
            TrayState::Buffering,
            TrayState::Paused,
            TrayState::Warning,
            TrayState::Error,
        ];
        for (i, a) in states.iter().enumerate() {
            for b in &states[i + 1..] {
                assert_ne!(
                    state_color(*a),
                    state_color(*b),
                    "states {a:?} and {b:?} share a colour"
                );
            }
        }
    }

    #[test]
    fn icon_rgba_is_the_glyph_not_a_solid_fill() {
        let px = icon_rgba(TrayState::Buffering);
        assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        let chip = state_color(TrayState::Buffering);
        let at = |x: u32, y: u32| {
            let o = ((y * ICON_SIZE + x) * 4) as usize;
            [px[o], px[o + 1], px[o + 2], px[o + 3]]
        };
        // The chip body (above the carved track) is the solid state colour…
        assert_eq!(at(ICON_SIZE / 2, ICON_SIZE / 4), chip);
        // …and the carved (elapsed) portion of the track differs from it — so the icon
        // is the glyph, not a uniform fill.
        let carved = at((ICON_SIZE as f32 * 0.30) as u32, ICON_SIZE / 2);
        assert_ne!(carved, chip);
    }
}
