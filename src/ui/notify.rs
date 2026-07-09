//! `ui::notify` — the single Win32 tray window (M5 + P1a): clipd's ONE notification-area
//! icon, on a window procedure **we own**. It carries the state glyph, tooltip, native
//! menu (shown via muda's `show_context_menu_for_hwnd`), and the save-complete/-failed
//! balloon — our no-overlay analogue of the incumbents' corner "Clip Saved" toast (a true
//! overlay is a permanent non-goal; the P1c pill is a separate topmost window, not this).
//!
//! ## Why our own window (P1a)
//! The Win11 toast-test matrix (DECISIONS 2026-07-09) showed two things: `NIS_HIDDEN`
//! balloons are suppressed outright, and balloon click-through needs `NIN_BALLOONUSERCLICK`,
//! which the shell delivers only to the icon's callback window. The `tray-icon` crate owns
//! its own window/WNDPROC, so it could deliver neither. So clipd now registers ONE class +
//! WNDPROC, adds ONE **visible** `Shell_NotifyIcon` entry, and drives everything through it:
//! left/right-click shows the muda menu on this HWND; balloons hang off the same visible
//! icon; `NIN_BALLOONUSERCLICK` opens the stored target folder. Exactly one clipd icon.
//!
//! The window lives on the tray's main thread, so the tray's message pump
//! (`PeekMessageW`/`DispatchMessageW`) dispatches its messages to [`wndproc`]. Unsafe is
//! confined to this module, each block with a `// SAFETY:` note; no new dependency (muda
//! was already in the tree via `tray-icon`, now a direct dep — DECISIONS 2026-07-09).

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use muda::{ContextMenu, Menu};
use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_STATE, NIF_TIP, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NIS_HIDDEN,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, LoadIconW,
    PeekMessageW, RegisterClassW, TranslateMessage, HICON, IDI_APPLICATION, MSG, PM_REMOVE,
    WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

use crate::spec_constants::PRODUCT_NAME;

/// Our notification-area entry id (arbitrary, our own).
const NOTIFY_UID: u32 = 0xC1D0;
/// Our private callback message for the notification entry.
const NOTIFY_CALLBACK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x21;
/// The window-class name (registered once per process).
const CLASS_NAME: PCWSTR = w!("clipd_notify_window");

thread_local! {
    /// The folder a balloon click should open. Set by [`TrayWindow::balloon`] before each
    /// balloon and read by [`wndproc`] on click — both on the main (tray) thread, so a plain
    /// thread-local is sound. **Latest-wins:** a newer save overwrites it; a click after a
    /// balloon times out unclicked simply opens the last target (harmless).
    static CLICK_TARGET: RefCell<Option<PathBuf>> = const { RefCell::new(None) };

    /// A clone of the tray's muda [`Menu`] (a cheap `Rc` handle sharing the live menu), so
    /// the free-function [`wndproc`] can pop the context menu on an icon click. Set by
    /// [`TrayWindow::new`], cleared on [`TrayWindow`] drop. Cloned out before the modal
    /// `TrackPopupMenu` call so a re-entrant click can't double-borrow the `RefCell`.
    static TRAY_MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
}

/// Build the balloon (title, body) for a save outcome. Pure, so the toast text is
/// unit-tested — and the caller feeds the SAME `seconds`/`reason` into the log line, so
/// the toast and the log can never disagree (T1).
pub fn save_toast(ok: bool, seconds: f32, reason: &str) -> (String, String) {
    if ok {
        (
            PRODUCT_NAME.to_string(),
            format!("Clip saved · {seconds:.0} s"),
        )
    } else {
        // Distinct + loud + the reason.
        (
            format!("{PRODUCT_NAME} — clip NOT saved"),
            format!("Clip NOT saved — {reason}"),
        )
    }
}

/// clipd's single visible tray window + notification icon (P1a). Owns the current [`HICON`]
/// (destroyed on replace + on drop) and drives the icon glyph, tooltip, menu, and balloons.
pub struct TrayWindow {
    hwnd: HWND,
    /// The icon currently installed on the notification entry — kept alive while set,
    /// destroyed when replaced by [`Self::set_icon`] or on drop.
    icon: HICON,
}

impl TrayWindow {
    /// Create the window, register the class, install the single **visible** notification
    /// icon (`NIF_ICON | NIF_MESSAGE | NIF_TIP`), and stash a clone of `menu` so [`wndproc`]
    /// can show it on click. `None` on any failure (window/icon creation), so the caller can
    /// degrade — a live app with no tray still runs the engine (the satellite rule).
    pub fn new(icon: HICON, tooltip: &str, menu: Menu) -> Option<Self> {
        // SAFETY: standard class registration + hidden-but-real top-level window creation +
        // `Shell_NotifyIcon(NIM_ADD)` with a visible icon. Every fallible call is checked; on
        // any failure we destroy what we made and return `None`. `wndproc` is a valid
        // `extern "system"` fn pointer for this class; the window is torn down in `Drop`.
        let hwnd = unsafe { create_window(icon, tooltip)? };
        TRAY_MENU.with(|m| *m.borrow_mut() = Some(menu));
        Some(Self { hwnd, icon })
    }

    /// Replace the notification icon (state change). Destroys the previous [`HICON`] once the
    /// shell has adopted the new one.
    pub fn set_icon(&mut self, icon: HICON) {
        // SAFETY: `NIM_MODIFY` with `NIF_ICON` on our own registered `(hWnd, uID)`; `hIcon`
        // is a live icon we own. The old icon is destroyed only after the modify call.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            nid.uFlags = NIF_ICON;
            nid.hIcon = icon;
            if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                let old = std::mem::replace(&mut self.icon, icon);
                let _ = DestroyIcon(old);
            } else {
                warn!("Shell_NotifyIcon(NIM_MODIFY, NIF_ICON) failed — tray glyph unchanged");
                // Keep the old icon installed; destroy the unused new one so it doesn't leak.
                let _ = DestroyIcon(icon);
            }
        }
    }

    /// Update the icon tooltip (state / recording text).
    pub fn set_tooltip(&self, tooltip: &str) {
        // SAFETY: `NIM_MODIFY` with `NIF_TIP`; `szTip` is a fixed inline wide buffer we fill
        // + NUL-terminate.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            nid.uFlags = NIF_TIP;
            fill_wide(&mut nid.szTip, tooltip);
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                warn!("Shell_NotifyIcon(NIM_MODIFY, NIF_TIP) failed — tooltip unchanged");
            }
        }
    }

    /// Raise the balloon for a save outcome. `click_dir` is opened if the user clicks it
    /// (the clip's folder on success, the log folder on failure — chosen by the caller).
    pub fn saved(&self, ok: bool, seconds: f32, reason: &str, click_dir: &Path) {
        let (title, body) = save_toast(ok, seconds, reason);
        self.balloon(&title, &body, !ok, click_dir);
    }

    /// Raise a plain informational balloon (e.g. the post-restart confirmation, T2).
    pub fn info(&self, title: &str, body: &str, click_dir: &Path) {
        self.balloon(title, body, false, click_dir);
    }

    /// Raise a balloon on the visible icon. **Latest-wins:** a second save before the first
    /// balloon dismisses just re-modifies the same entry (the shell replaces the balloon) and
    /// overwrites the click target — no stuck icon, no queue.
    fn balloon(&self, title: &str, body: &str, error: bool, click_dir: &Path) {
        CLICK_TARGET.with(|t| *t.borrow_mut() = Some(click_dir.to_path_buf()));
        // SAFETY: `NIM_MODIFY` with `NIF_INFO` on our own registered `(hWnd, uID)`.
        // `szInfoTitle`/`szInfo` are fixed inline wide buffers we fill + NUL-terminate.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = if error { NIIF_WARNING } else { NIIF_INFO };
            fill_wide(&mut nid.szInfoTitle, title);
            fill_wide(&mut nid.szInfo, body);
            let ok = Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
            let err = GetLastError();
            // A `true` here means the shell ACCEPTED the balloon; whether it DISPLAYS is a
            // separate policy question (DND / gaming-DND / fullscreen). The P1c pill + P1b
            // sound are the in-game backstops; this balloon still lands in Action Center.
            if ok {
                info!(
                    last_error = err.0,
                    "Shell_NotifyIcon(NIM_MODIFY, NIF_INFO) ok (balloon queued)"
                );
            } else {
                warn!(
                    last_error = err.0,
                    "Shell_NotifyIcon(NIM_MODIFY, NIF_INFO) FAILED — save balloon not shown"
                );
            }
        }
    }
}

impl Drop for TrayWindow {
    fn drop(&mut self) {
        TRAY_MENU.with(|m| *m.borrow_mut() = None);
        // SAFETY: remove our own `(hWnd, uID)` entry, destroy our window, then free the icon.
        // A failed call during teardown is harmless.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.icon);
        }
    }
}

/// The window procedure: an icon left/right-click pops the tray menu on this HWND; a balloon
/// click opens the stored target folder. Everything else falls through to `DefWindowProcW`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == NOTIFY_CALLBACK {
        // Classic (no `NIM_SETVERSION`): the notification event is the low word of lParam —
        // either a mouse message (WM_L/RBUTTONUP) or a balloon event (NIN_BALLOONUSERCLICK).
        let event = (lparam.0 as u32) & 0xFFFF;
        if event == NIN_BALLOONUSERCLICK {
            let target = CLICK_TARGET.with(|t| t.borrow().clone());
            if let Some(dir) = target {
                open_folder(&dir);
            }
        } else if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
            // Clone the menu handle out FIRST so the borrow is released before the modal
            // `TrackPopupMenu` (which pumps messages and could re-enter this proc).
            let menu = TRAY_MENU.with(|m| m.borrow().clone());
            if let Some(menu) = menu {
                // SAFETY: `hwnd` is our live window; `menu` is our live muda menu. muda does
                // the `SetForegroundWindow` + `TrackPopupMenu(TPM_RETURNCMD)` dance and fires
                // the `MenuEvent` to the global receiver the tray already drains.
                unsafe {
                    let _ = menu.show_context_menu_for_hwnd(hwnd.0 as isize, None);
                }
            }
        }
        return LRESULT(0);
    }
    // SAFETY: forwarding unhandled messages to the default handler is the documented
    // contract for a WNDPROC.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Register the class + create the hidden-but-real window + add the single **visible**
/// notification entry (icon + tooltip + our callback message).
///
/// # Safety
/// Calls raw Win32; returns `None` on any failure so the caller degrades.
unsafe fn create_window(icon: HICON, tooltip: &str) -> Option<HWND> {
    let hinstance: HINSTANCE = GetModuleHandleW(None).ok()?.into();

    // Register the class (idempotent — a second process-wide registration just returns 0
    // with ERROR_CLASS_ALREADY_EXISTS; we create exactly one `TrayWindow` per process).
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // A real top-level window, never shown (no `ShowWindow`), tool-window so it can never
    // appear in the taskbar. It exists only to own the notification icon + receive its
    // callbacks; the icon is the app's entire visible surface.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!("clipd"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance),
        None,
    )
    .ok()?;

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = NOTIFY_UID;
    // Visible icon (no NIS_HIDDEN, P1a) + callback message + tooltip.
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = NOTIFY_CALLBACK;
    nid.hIcon = icon;
    fill_wide(&mut nid.szTip, tooltip);
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    let err = GetLastError();
    if ok {
        info!(
            last_error = err.0,
            "Shell_NotifyIcon(NIM_ADD) ok (single visible tray icon)"
        );
        Some(hwnd)
    } else {
        warn!(
            last_error = err.0,
            "Shell_NotifyIcon(NIM_ADD) FAILED for the tray window"
        );
        let _ = DestroyWindow(hwnd);
        None
    }
}

/// **P1 diagnostic** (`clipd toast-test`): fire a success-style and a failure-style balloon
/// from a bare console, printing each `Shell_NotifyIcon` BOOL + `GetLastError`, for BOTH a
/// HIDDEN entry (the pre-P1a production path) and a VISIBLE (`NIF_ICON`) entry — the tool
/// that produced the matrix behind the P1a migration. Kept for future notification-policy
/// debugging. Runs its own message pump so each balloon has time to render.
pub fn run_toast_diagnostic() {
    println!("clipd toast diagnostic (P1)\n");
    println!(
        "Watch the screen for balloons. Each Shell_NotifyIcon call prints BOOL + GetLastError."
    );
    println!("BOOL=true at the NIM_MODIFY site means the shell ACCEPTED the balloon; whether it");
    println!(
        "DISPLAYS then depends on DND / fullscreen / the per-app toggle / hidden-icon policy.\n"
    );

    run_toast_trial(
        "HIDDEN entry — NIS_HIDDEN, no icon (the pre-P1a production path)",
        true,
    );
    println!();
    run_toast_trial(
        "VISIBLE entry — NIF_ICON, not hidden (the P1a path — this one shows)",
        false,
    );

    println!("\n─────────────────────────────────────────────────────────────");
    println!("Interpretation:");
    println!("  • Both NIM_ADD/NIM_MODIFY return FALSE  → plumbing bug (not a policy suppressor).");
    println!("  • HIDDEN shows nothing but VISIBLE shows → Win11 suppresses balloons on hidden");
    println!("    icons → the P1a single-visible-icon migration (already applied).");
    println!(
        "  • Neither shows but both return TRUE     → a global suppressor (DND / gaming-DND /"
    );
    println!("    fullscreen) — the P1b sound + P1c pill are the in-game backstops.");
    println!("\nManual checks to pair with this run:");
    println!("  • Is 'clipd' listed under Settings > System > Notifications (and toggled ON)?");
    println!("  • Re-run with Do Not Disturb ON, and again with a fullscreen game focused.");
}

/// One trial of the P1 diagnostic: create a window, add the notification entry (hidden or
/// visible), fire a success then a failure balloon, pumping messages between so each renders.
fn run_toast_trial(label: &str, hidden: bool) {
    println!("== {label} ==");
    // SAFETY: standard class registration + top-level window + Shell_NotifyIcon calls, all
    // checked and torn down before return; `wndproc` is a valid WNDPROC for this class.
    unsafe {
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                println!("  GetModuleHandleW failed: {e} — skipping trial");
                return;
            }
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent across trials (ERROR_CLASS_ALREADY_EXISTS is fine)

        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("clipd"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                println!("  CreateWindowExW failed: {e} — skipping trial");
                return;
            }
        };

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = NOTIFY_UID;
        nid.uCallbackMessage = NOTIFY_CALLBACK;
        if hidden {
            nid.uFlags = NIF_MESSAGE | NIF_STATE;
            nid.dwState = NIS_HIDDEN;
            nid.dwStateMask = NIS_HIDDEN;
        } else {
            // A visible entry needs a real icon; use the stock application icon.
            nid.uFlags = NIF_MESSAGE | NIF_STATE | NIF_ICON;
            nid.dwStateMask = NIS_HIDDEN; // clear hidden explicitly (dwState left 0 = visible)
            if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
                nid.hIcon = icon;
            }
        }
        let add_ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        println!(
            "  NIM_ADD           -> {add_ok:<5}  GetLastError={}",
            GetLastError().0
        );
        if !add_ok {
            let _ = DestroyWindow(hwnd);
            println!("  (add failed — skipping the balloons for this trial)");
            return;
        }

        let (ok1, e1) = fire_test_balloon(hwnd, "clipd", "Clip saved · 12 s (toast-test)", false);
        println!("  NIM_MODIFY success-> {ok1:<5}  GetLastError={e1}   (watch for a balloon ~4 s)");
        pump_messages(4000);

        let (ok2, e2) = fire_test_balloon(
            hwnd,
            "clipd — clip NOT saved",
            "Clip NOT saved — toast-test (simulated disk full)",
            true,
        );
        println!("  NIM_MODIFY failure-> {ok2:<5}  GetLastError={e2}   (watch for a balloon ~4 s)");
        pump_messages(4000);

        let mut del: NOTIFYICONDATAW = std::mem::zeroed();
        del.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        del.hWnd = hwnd;
        del.uID = NOTIFY_UID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &del);
        let _ = DestroyWindow(hwnd);
    }
}

/// Fire one `NIM_MODIFY`/`NIF_INFO` balloon, returning `(accepted, GetLastError)`.
///
/// # Safety
/// `hwnd` must be a live window with a registered `(hWnd, NOTIFY_UID)` entry.
unsafe fn fire_test_balloon(hwnd: HWND, title: &str, body: &str, error: bool) -> (bool, u32) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = NOTIFY_UID;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if error { NIIF_WARNING } else { NIIF_INFO };
    fill_wide(&mut nid.szInfoTitle, title);
    fill_wide(&mut nid.szInfo, body);
    let ok = Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
    (ok, GetLastError().0)
}

/// Pump this thread's message queue for `ms` milliseconds so a balloon can render and a
/// click can be delivered to [`wndproc`].
///
/// # Safety
/// Standard `PeekMessageW`/`DispatchMessageW` loop on the calling thread.
unsafe fn pump_messages(ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    let mut msg = MSG::default();
    while std::time::Instant::now() < deadline {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Open `dir` in Explorer (the balloon-click action).
fn open_folder(dir: &Path) {
    match std::process::Command::new("explorer").arg(dir).spawn() {
        Ok(_) => info!(dir = %dir.display(), "opened folder from save toast"),
        Err(e) => warn!(dir = %dir.display(), error = %e, "could not open folder from save toast"),
    }
}

/// Copy `s` into a fixed-size wide buffer, truncated to fit, always NUL-terminated.
fn fill_wide(dst: &mut [u16], s: &str) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let n = wide.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&wide[..n]);
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_toast_success_shows_length() {
        let (title, body) = save_toast(true, 30.4, "");
        assert_eq!(title, PRODUCT_NAME);
        assert_eq!(body, "Clip saved · 30 s");
    }

    #[test]
    fn save_toast_failure_is_distinct_and_names_the_reason() {
        let (title, body) = save_toast(false, 0.0, "disk full");
        assert!(title.contains("NOT saved"), "title = {title}");
        assert!(body.contains("disk full"), "body = {body}");
    }

    #[test]
    fn fill_wide_truncates_and_nul_terminates() {
        let mut buf = [0xFFu16; 4];
        fill_wide(&mut buf, "abcd");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
        let mut buf = [0xFFu16; 6];
        fill_wide(&mut buf, "hi");
        assert_eq!(&buf[..3], &[b'h' as u16, b'i' as u16, 0]);
    }
}
