//! `ui::window_state` — persist the settings window's size/position across sessions (A4 / T7).
//!
//! ## Why a ui-state file, not eframe's built-in persistence
//! eframe *can* save the window geometry itself (the `persistence` feature + a RON blob in
//! a platform data dir), but it restores the raw saved rectangle with **no clamp to the
//! current virtual-screen bounds** — so a window last placed on a monitor that is now
//! unplugged reopens off-screen and unreachable. It also has no seam to record whether the
//! window was maximized when it closed. We need both guards, so we keep eframe's
//! persistence OFF (as configured) and own a tiny TOML file instead (no new dependency —
//! `serde` + `toml` are already in the tree, the same versioned path config uses).
//!
//! The file lives next to the logs (`%LOCALAPPDATA%/{PRODUCT_NAME}/ui-state.toml`), NOT in
//! `config.toml` — window geometry is UI state, not user configuration (A4). A missing,
//! unreadable, or malformed file is a silent "use the default size" — never an error.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::logging::log_dir;
use crate::spec_constants::PRODUCT_NAME;

/// The persisted window geometry. Sizes/positions are logical points (what eframe's
/// `ViewportBuilder`/`ViewportInfo` use).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Inner (content) size.
    pub width: f32,
    pub height: f32,
    /// Outer (window) top-left position. `None` before the window has ever been placed.
    pub x: Option<f32>,
    pub y: Option<f32>,
    /// Whether the window was maximized when it closed (restored maximized if so).
    pub maximized: bool,
}

/// The ui-state file path: `<log_dir>/../ui-state.toml` — a sibling of the `logs/` folder
/// in the app's local-data dir (falls back to inside the log dir if it has no parent).
pub fn state_path() -> PathBuf {
    let dir = log_dir();
    let base = dir.parent().map(|p| p.to_path_buf()).unwrap_or(dir);
    base.join("ui-state.toml")
}

/// Load the saved window geometry, or `None` if there is no valid file (first run,
/// unreadable, or malformed → the caller uses the default size). Never errors.
pub fn load() -> Option<WindowState> {
    let path = state_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<WindowState>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring malformed window-state file");
            None
        }
    }
}

/// Persist the window geometry (best-effort — a failure is logged, never surfaced). Called
/// when the window is hidden-to-tray or the app quits.
pub fn save(state: &WindowState) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string(state) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                warn!(path = %path.display(), error = %e, "could not write window-state file");
            }
        }
        Err(e) => warn!(error = %e, "could not serialize window state"),
    }
}

/// Clamp a saved top-left position so the window lands inside the CURRENT virtual screen
/// (guard against a monitor that has since disappeared — A4/T7). If the whole window fits,
/// it is nudged fully on-screen; if it is wider/taller than the virtual screen, it pins to
/// the virtual screen's top-left. With no monitor info, the saved position is used as-is.
pub fn clamp_to_virtual_screen(x: f32, y: f32, width: f32, height: f32) -> (f32, f32) {
    let Some((vx, vy, vw, vh)) = virtual_screen_rect() else {
        return (x, y);
    };
    let (vx, vy, vw, vh) = (vx as f32, vy as f32, vw as f32, vh as f32);
    // Keep at least the whole window on-screen when it fits; else pin to the top-left.
    let max_x = (vx + vw - width).max(vx);
    let max_y = (vy + vh - height).max(vy);
    (x.clamp(vx, max_x), y.clamp(vy, max_y))
}

/// The virtual-screen bounding rectangle `(x, y, width, height)` covering all monitors,
/// via `GetSystemMetrics`. `None` if the metrics are unavailable (width/height 0). Cheap —
/// no event loop needed, so it can run before the window is built.
#[cfg(windows)]
fn virtual_screen_rect() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    // SAFETY: GetSystemMetrics has no preconditions and returns a plain integer metric.
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((x, y, w, h))
}

/// Non-Windows stub (keeps the module buildable off-target; the crate ships Windows-only).
#[cfg(not(windows))]
fn virtual_screen_rect() -> Option<(i32, i32, i32, i32)> {
    None
}

/// Log the resolved ui-state path once at startup (helps answer "where is my window size
/// remembered"). Cheap; call from the window-thread setup.
pub fn log_location() {
    info!(path = %state_path().display(), product = PRODUCT_NAME, "window geometry persisted here (A4)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_a_fitting_window_and_pins_an_offscreen_one() {
        match virtual_screen_rect() {
            // No monitor info (a non-Windows test host): the position is passed through.
            None => assert_eq!(
                clamp_to_virtual_screen(10.0, 20.0, 400.0, 300.0),
                (10.0, 20.0)
            ),
            // With real bounds: a wildly off-screen position (a disappeared monitor) is
            // pulled back so the whole window fits inside the virtual screen.
            Some((vx, vy, vw, vh)) => {
                let (w, h) = (400.0f32, 300.0f32);
                let (cx, cy) = clamp_to_virtual_screen(999_999.0, 999_999.0, w, h);
                assert!(
                    cx >= vx as f32 && cx + w <= (vx + vw) as f32,
                    "x off-screen: {cx}"
                );
                assert!(
                    cy >= vy as f32 && cy + h <= (vy + vh) as f32,
                    "y off-screen: {cy}"
                );
                // An already-on-screen position near the origin is left alone.
                let (ox, oy) = clamp_to_virtual_screen(vx as f32, vy as f32, w, h);
                assert_eq!((ox, oy), (vx as f32, vy as f32));
            }
        }
    }

    #[test]
    fn window_state_round_trips_through_toml() {
        let s = WindowState {
            width: 560.0,
            height: 440.0,
            x: Some(120.0),
            y: Some(80.0),
            maximized: false,
        };
        let text = toml::to_string(&s).expect("serialize");
        let back: WindowState = toml::from_str(&text).expect("deserialize");
        assert_eq!(s, back);
    }
}
