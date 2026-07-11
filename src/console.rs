//! `console` — hide the auto-allocated console window for GUI-style launches.
//!
//! `clipd` is a **console-subsystem** binary so its CLI / `*-probe` subcommands
//! print to a terminal synchronously (the orchestrator reads probe output as it
//! runs). But when a friend double-clicks `clipd.exe`, or Windows launches it from
//! the HKCU Run key at logon (`autostart.rs` writes `"<exe>" buffer`), Windows
//! allocates a **fresh** console for the process — a black window that sits behind
//! the tray for the whole session and, if the friend closes it, kills clipd.
//!
//! This module hides that window in exactly those "we own the console" cases while
//! leaving a real terminal launch (`clipd buffer`, `clipd capture-probe 3`, …)
//! untouched, so developer output still streams to the shell that started it.
//!
//! ## Detection
//! `GetConsoleProcessList` reports how many processes share our console. A console
//! we own alone (count == 1) was allocated for us — a double-click or a Run-key
//! logon launch — so we hide it. A console shared with a parent shell (count > 1)
//! was inherited from a terminal, so we leave it visible.
//!
//! ## `unsafe` / threading
//! The three Win32 calls are confined here (an OS-wrapper module per `CLAUDE.md`
//! constraint 5's spirit — logic modules stay 100% safe), each `unsafe` block
//! carrying a `SAFETY:` note. No pixels, no COM. Call once from `main`, before the
//! engine threads start; the whole thing is best-effort — any failure leaves the
//! console exactly as it was.

use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

/// Hide the console window **iff** this process allocated it itself (a double-click
/// or a Run-key logon launch), so those launches show only the tray. A console
/// inherited from a terminal (a parent shell is attached) is left visible so CLI /
/// probe output keeps streaming. Best-effort: any failure leaves the console as-is.
pub fn hide_if_owned() {
    // SAFETY: GetConsoleWindow takes no arguments and returns the console's HWND
    // (null if this process has no console). No invariants to uphold.
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.0.is_null() {
        return; // no console attached — nothing to hide
    }

    // SAFETY: GetConsoleProcessList fills the provided slice with the PIDs attached
    // to our console and returns the count (0 on failure). A 2-element buffer is
    // enough to distinguish "only us" (1) from "shared with a shell" (>= 2); a
    // larger real count still returns >= 2, which we treat as "shared".
    let mut pids = [0u32; 2];
    let count = unsafe { GetConsoleProcessList(&mut pids) };

    // count == 0 → the call failed (leave the console alone); count > 1 → launched
    // from a shell that shares the console (leave it visible). Only a console this
    // process owns by itself is hidden.
    if count == 1 {
        // SAFETY: `hwnd` is the valid console window returned above; ShowWindow only
        // toggles visibility (SW_HIDE) and cannot violate memory safety.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}
