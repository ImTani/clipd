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
use tracing::{info, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

use super::settings::SettingsHandle;
use crate::audio::levels::AudioLevels;
use crate::audio::wasapi_stream::AudioStreamKind;
use crate::engine::{BufferEngine, EngineCommand, ShellSignal, TrayState};
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
    /// Creating the tray icon failed (`Shell_NotifyIcon`).
    #[error("building the tray icon: {0}")]
    Tray(#[from] tray_icon::Error),
    /// Building a menu item failed.
    #[error("building the tray menu: {0}")]
    Menu(#[from] tray_icon::menu::Error),
    /// Building the icon image from RGBA failed.
    #[error("building the tray icon image: {0}")]
    Icon(#[from] tray_icon::BadIcon),
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

/// The RGBA colour for each tray state — the single place to re-theme the tray.
/// `01-PROJECT-PLAN.md §5.5`: buffering / paused / warning / error.
fn state_color(state: TrayState) -> [u8; 4] {
    match state {
        TrayState::Buffering => [0x3f, 0xb9, 0x50, 0xff], // green
        TrayState::Paused => [0xc9, 0x9a, 0x24, 0xff],    // amber
        TrayState::Warning => [0xe6, 0x8a, 0x00, 0xff],   // orange
        TrayState::Error => [0xd0, 0x3b, 0x2f, 0xff],     // red
    }
}

/// The raw RGBA pixels for a state's icon (a solid `ICON_SIZE`² fill). Pure, so it
/// is unit-testable without touching Win32/GDI.
fn icon_rgba(state: TrayState) -> Vec<u8> {
    let color = state_color(state);
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for _ in 0..(ICON_SIZE * ICON_SIZE) {
        rgba.extend_from_slice(&color);
    }
    rgba
}

/// Build a solid-colour tray icon for `state`.
///
/// Programmatic on purpose (no image decoder linked, no asset files, no binary
/// bloat) and isolated behind this one function: switching to designed art later
/// is a one-function change — `include_bytes!` a PNG per state and decode it here,
/// with **no** call-site churn (DECISIONS.md 2026-07-06 "M5 plan").
fn icon_for(state: TrayState) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(icon_rgba(state), ICON_SIZE, ICON_SIZE)
}

/// The tooltip text for a state.
fn tooltip(state: TrayState) -> String {
    let s = match state {
        TrayState::Buffering => "buffering",
        TrayState::Paused => "paused",
        TrayState::Warning => "warning — check the log",
        TrayState::Error => "error — capture stopped",
    };
    format!("{PRODUCT_NAME} — {s}")
}

/// The tray shell: owns the tray icon + menu handles and drives the pump loop.
/// Main-thread only (its `tray-icon`/muda members are `!Send`).
pub struct Shell {
    tray: TrayIcon,
    /// Held so its checkmark can be toggled on pause; also keeps the item alive.
    pause_item: CheckMenuItem,
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
    /// The audio streams to draw meters for (desktop / mic), from the engine.
    audio_streams: Vec<AudioStreamKind>,
    /// Lock-free engine status for the settings window's status strip (A4), handed to
    /// the window on open. Read-only here (engine → UI).
    status: Arc<EngineStatus>,
    /// Where saved clips land — for "Open clips folder".
    output_dir: PathBuf,
    /// The current tray state (to skip redundant icon updates).
    state: TrayState,
    /// Whether the user has paused buffering.
    paused: bool,
    /// Whether start-with-Windows is currently enabled (mirrors the Run key).
    autostart_enabled: bool,
}

impl Shell {
    /// Build the tray icon + menu. `cmd_tx` comes from
    /// [`BufferEngine::command_sender`]; `output_dir` is the clips directory;
    /// `levels`/`audio_streams` come from the engine and feed the settings-window
    /// VU meters (A3); `status` feeds its status strip (A4).
    pub fn new(
        cmd_tx: Sender<EngineCommand>,
        output_dir: PathBuf,
        levels: Arc<AudioLevels>,
        audio_streams: Vec<AudioStreamKind>,
        status: Arc<EngineStatus>,
    ) -> Result<Self, ShellError> {
        // Reflect the current HKCU Run-key state on the checkbox at build time.
        let autostart_enabled = crate::autostart::is_enabled();

        let menu = Menu::new();
        let save = MenuItem::with_id(ID_SAVE, "Save clip", true, None);
        let pause_item = CheckMenuItem::with_id(ID_PAUSE, "Pause buffering", true, false, None);
        let record = MenuItem::with_id(ID_RECORD, "Start / stop recording", true, None);
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
        menu.append(&record)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&settings)?;
        menu.append(&open)?;
        menu.append(&autostart_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let state = TrayState::Buffering;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(state))
            .with_icon(icon_for(state)?)
            .build()?;

        Ok(Self {
            tray,
            pause_item,
            autostart_item,
            cmd_tx,
            settings: SettingsHandle::default(),
            levels,
            audio_streams,
            status,
            output_dir,
            state,
            paused: false,
            autostart_enabled,
        })
    }

    /// Run the shell loop on the calling (main) thread until the user picks Quit or
    /// the engine session ends. Pumps Win32 messages (so menu clicks arrive), maps
    /// them to [`EngineCommand`]s, and reflects [`ShellSignal`]s on the tray.
    pub fn run(&mut self, engine: &BufferEngine) {
        loop {
            pump_messages();

            // Menu clicks → engine commands. muda posts to a global receiver.
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if self.handle_menu(&event.id) {
                    // Quit: close the settings window, ask the engine to wind down,
                    // then leave the loop.
                    self.settings.shutdown();
                    let _ = self.cmd_tx.send(EngineCommand::Shutdown);
                    return;
                }
            }

            // Engine → tray state.
            while let Ok(signal) = engine.signals().try_recv() {
                match signal {
                    ShellSignal::State(state) => self.set_state(state),
                }
            }

            // The session ended on its own (fatal error / worker died): show Error,
            // tear down the settings window, and hand back — `run_buffer` will join
            // and surface the error.
            if engine.any_worker_finished() {
                self.set_state(TrayState::Error);
                self.settings.shutdown();
                return;
            }

            std::thread::sleep(POLL_INTERVAL);
        }
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
            Some(MenuAction::OpenSettings) => self.settings.open(
                &self.cmd_tx,
                &self.levels,
                &self.audio_streams,
                &self.status,
            ),
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
            Ok(icon) => {
                if let Err(e) = self.tray.set_icon(Some(icon)) {
                    warn!(error = %e, "could not update the tray icon");
                }
            }
            Err(e) => warn!(error = %e, "could not build the tray icon image"),
        }
        if let Err(e) = self.tray.set_tooltip(Some(tooltip(state))) {
            warn!(error = %e, "could not update the tray tooltip");
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
    fn icon_rgba_is_a_full_solid_fill() {
        let px = icon_rgba(TrayState::Buffering);
        assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        // First pixel is the state colour and the buffer is a uniform fill.
        let color = state_color(TrayState::Buffering);
        assert_eq!(&px[0..4], &color);
        assert!(px.chunks_exact(4).all(|p| p == color));
    }
}
