//! `ui::notify` — the save-complete / save-failed tray balloon (T1 / A2): our no-overlay
//! analogue of the incumbents' corner "Clip Saved" toast (overlays are a permanent
//! non-goal). It owns a **hidden top-level window** (never shown, `WS_EX_TOOLWINDOW` so no
//! taskbar presence) that carries a `NIS_HIDDEN` notification-area entry, and raises
//! balloons via `Shell_NotifyIcon(NIM_MODIFY, NIF_INFO)`.
//!
//! ## Why our own window (not the tray-icon crate's, not `HWND_MESSAGE`)
//! Click-to-open needs `NIN_BALLOONUSERCLICK`, which the shell delivers to the icon's
//! callback window — and `tray-icon`'s window procedure is not ours, so we cannot
//! piggyback on it for clicks. We register our own class + WNDPROC. It is a real hidden
//! **top-level** window rather than a message-only (`HWND_MESSAGE`) window because
//! `Shell_NotifyIcon` callback delivery to message-only windows is historically
//! unreliable (DECISIONS "T1 save-toast mechanism").
//!
//! The window lives on the tray's main thread, so the tray's existing message pump
//! (`PeekMessageW`/`DispatchMessageW`) dispatches its messages to [`wndproc`]. Unsafe is
//! confined to this module, each block with a `// SAFETY:` note; no new dependency.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIF_MESSAGE, NIF_STATE, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NIS_HIDDEN, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED,
};

use crate::spec_constants::PRODUCT_NAME;

/// Our notification-area entry id (arbitrary, our own — no coupling to tray-icon).
const NOTIFY_UID: u32 = 0xC1D0;
/// Our private callback message for the notification entry.
const NOTIFY_CALLBACK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x21;
/// The window-class name (registered once per process).
const CLASS_NAME: PCWSTR = w!("clipd_notify_window");

thread_local! {
    /// The folder a balloon click should open. Set by [`Notifier::balloon`] before each
    /// balloon and read by [`wndproc`] on click — both on the main (tray) thread, so a
    /// plain thread-local is sound. **Latest-wins:** a newer save overwrites it; a click
    /// after a balloon times out unclicked simply opens the last target (harmless).
    static CLICK_TARGET: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
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

/// The save-complete/-failed balloon. A failed `new()` (window/icon creation) disables
/// toasts (logged) rather than blocking or panicking — the tray + log stay authoritative.
pub struct Notifier {
    hwnd: HWND,
    /// Whether the window + hidden entry registered — balloons no-op when `false`.
    active: bool,
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier {
    /// Create the hidden window + register its hidden notification entry.
    pub fn new() -> Self {
        // SAFETY: standard class registration + hidden top-level window creation +
        // `Shell_NotifyIcon(NIM_ADD)`. Every fallible call is checked; on any failure we
        // return `active = false` and all later calls no-op. The window/icon are torn down
        // in `Drop`. `wndproc` is a valid `extern "system"` fn pointer for this class.
        let (hwnd, active) = unsafe { create_window_and_icon() };
        if !active {
            warn!("could not create the notification window; save toasts disabled");
        }
        Self { hwnd, active }
    }

    /// Raise the balloon for a save outcome. `click_dir` is opened if the user clicks it
    /// (the clip's folder on success, the log folder on failure — chosen by the caller).
    pub fn saved(&self, ok: bool, seconds: f32, reason: &str, click_dir: &Path) {
        let (title, body) = save_toast(ok, seconds, reason);
        self.balloon(&title, &body, !ok, click_dir);
    }

    fn balloon(&self, title: &str, body: &str, error: bool, click_dir: &Path) {
        if !self.active {
            return;
        }
        CLICK_TARGET.with(|t| *t.borrow_mut() = Some(click_dir.to_path_buf()));
        // SAFETY: `NIM_MODIFY` with `NIF_INFO` on our own registered `(hWnd, uID)`.
        // `szInfoTitle`/`szInfo` are fixed-size inline wide buffers we fill + NUL-terminate;
        // no borrowed pointer escapes the call.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = if error { NIIF_WARNING } else { NIIF_INFO };
            fill_wide(&mut nid.szInfoTitle, title);
            fill_wide(&mut nid.szInfo, body);
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                warn!("could not show the save balloon");
            }
        }
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // SAFETY: remove our own `(hWnd, uID)` entry, then destroy our own window. A failed
        // call during teardown is harmless.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = NOTIFY_UID;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The window procedure: on our callback message, a balloon click opens the stored target
/// folder. Everything else falls through to `DefWindowProcW`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == NOTIFY_CALLBACK {
        // Classic (no `NIM_SETVERSION`): the notification event is the low word of lParam.
        let event = (lparam.0 as u32) & 0xFFFF;
        if event == NIN_BALLOONUSERCLICK {
            CLICK_TARGET.with(|t| {
                if let Some(dir) = t.borrow().clone() {
                    open_folder(&dir);
                }
            });
        }
        return LRESULT(0);
    }
    // SAFETY: forwarding unhandled messages to the default handler is the documented
    // contract for a WNDPROC.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Register the class + create the hidden window + add the hidden notification entry.
///
/// # Safety
/// Calls raw Win32; returns `(hwnd, false)` on any failure so the caller degrades.
unsafe fn create_window_and_icon() -> (HWND, bool) {
    let null = HWND(std::ptr::null_mut());
    let hinstance: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return (null, false),
    };

    // Register the class (idempotent — a second process-wide registration just returns 0
    // with ERROR_CLASS_ALREADY_EXISTS; we create exactly one `Notifier` per process).
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // A real top-level window, never shown (no `ShowWindow`), tool-window so it can never
    // appear in the taskbar.
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
        Err(_) => return (null, false),
    };

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = NOTIFY_UID;
    nid.uFlags = NIF_MESSAGE | NIF_STATE;
    nid.uCallbackMessage = NOTIFY_CALLBACK;
    nid.dwState = NIS_HIDDEN; // never a second visible clipd tray icon
    nid.dwStateMask = NIS_HIDDEN;
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    if !ok {
        let _ = DestroyWindow(hwnd);
        return (null, false);
    }
    (hwnd, true)
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
