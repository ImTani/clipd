//! `ui` — the tray shell + the satellite settings window.
//!
//! Submodules:
//! - [`tray`] — the M5 tray icon + native menu + a main-thread message pump that
//!   translates menu clicks into [`crate::engine::EngineCommand`]s and reflects
//!   [`crate::engine::ShellSignal`]s on the icon/tooltip.
//! - [`settings`] — the A2 egui/eframe settings window, lazily spawned from the
//!   tray onto its own thread (the satellite law, `CLAUDE.md` "UI rules").
//!
//! ## Satellite rule (`08-FEATURE-COMPLETE.md`)
//! This module depends on engine *types* and never the reverse; the engine runs
//! fully without a shell (the `record` subcommand and the hidden `--autosave` /
//! `--record-secs` hooks never build one). The settings window talks to the
//! engine only over the existing [`crate::engine::EngineCommand`] channel; no
//! engine code links against, depends on, or blocks on anything under `ui`.

mod folder_dialog;
mod recent;
mod settings;
mod theme;
mod tray;

pub use tray::{Shell, ShellError, ShellOutcome};
