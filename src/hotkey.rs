//! `hotkey` — the global save/record hotkey pump (`01-PROJECT-PLAN.md §2`;
//! `CLAUDE.md` hard-constraint 5: `RegisterHotKey` via `global-hotkey` **only**, no
//! low-level keyboard hooks).
//!
//! `global-hotkey`'s Windows backend creates a hidden message window and
//! `RegisterHotKey`s to it, so `WM_HOTKEY` reaches that window's proc **only while
//! the creating thread pumps its message queue** (`GetMessage`/`DispatchMessage`).
//! This module owns that thread: it creates the [`GlobalHotKeyManager`], registers
//! the configured hotkey, and runs the message loop until asked to quit (a
//! cross-thread `WM_QUIT` posted by [`HotkeyPump::request_quit`]). Pressed/released
//! events flow to the process-global `GlobalHotKeyEvent::receiver()`, which the
//! buffer engine reads.
//!
//! ## `unsafe` / threading
//! This is a Win32 syscall wrapper (message loop + `PostThreadMessageW`), so the
//! `unsafe` here is consistent with `CLAUDE.md` (unsafe confined to OS-wrapper
//! modules, each block carrying a `SAFETY:` note). The manager holds a raw `HWND`
//! and is not `Send`; it is created, used, and dropped entirely on the pump thread.
//! No COM apartment is needed — `RegisterHotKey` and the message loop are not COM.

use std::str::FromStr;
use std::sync::mpsc;
use std::thread::JoinHandle;

use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;
use tracing::{error, info};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
};

/// Errors from setting up or driving the hotkey pump.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// The hotkey string did not parse (`global-hotkey` accepts e.g. `Ctrl+Alt+S`).
    #[error("could not parse hotkey '{0}': {1}")]
    Parse(String, String),
    /// Creating the manager or registering the hotkey failed (e.g. the combo is
    /// already owned by another app — `ERROR_HOTKEY_ALREADY_REGISTERED`).
    #[error("could not register hotkey '{0}': {1}")]
    Register(String, String),
    /// The pump thread ended before reporting setup.
    #[error("hotkey pump thread failed during setup")]
    SetupChannelClosed,
}

/// Parse a config hotkey string (`[hotkeys]`) into a [`HotKey`]. `global-hotkey`
/// accepts `+`-separated modifiers (`Ctrl`/`Control`/`Alt`/`Shift`/`Super`,
/// case-insensitive) and a key token (`S` or `KeyS`, `F8`, `Space`, …). Kept
/// separate from the pump so config validation can check hotkeys without a window.
pub fn parse_hotkey(s: &str) -> Result<HotKey, HotkeyError> {
    HotKey::from_str(s).map_err(|e| HotkeyError::Parse(s.to_string(), e.to_string()))
}

/// A running hotkey pump: the message-loop thread plus the id needed to match its
/// events and the Win32 thread id used to stop it.
pub struct HotkeyPump {
    handle: JoinHandle<()>,
    thread_id: u32,
    save_hotkey_id: u32,
}

impl HotkeyPump {
    /// Spawn the pump thread, register `hotkey_str`, and block until it reports
    /// success or failure. On success the message loop is running.
    pub fn spawn(hotkey_str: &str) -> Result<Self, HotkeyError> {
        // Validate the string up front so a parse error surfaces without spawning.
        let hotkey = parse_hotkey(hotkey_str)?;
        let save_hotkey_id = hotkey.id();
        let label = hotkey_str.to_string();

        let (setup_tx, setup_rx) = mpsc::channel::<Result<u32, HotkeyError>>();
        let handle = std::thread::Builder::new()
            .name("hotkey".to_string())
            .spawn(move || pump_body(hotkey, label, setup_tx))
            .expect("thread spawn should not fail");

        match setup_rx.recv() {
            Ok(Ok(thread_id)) => Ok(HotkeyPump {
                handle,
                thread_id,
                save_hotkey_id,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(HotkeyError::SetupChannelClosed)
            }
        }
    }

    /// The id carried by [`global_hotkey::GlobalHotKeyEvent`]s for the save hotkey.
    pub fn save_hotkey_id(&self) -> u32 {
        self.save_hotkey_id
    }

    /// Ask the pump to exit: post `WM_QUIT` to its thread so `GetMessageW` returns
    /// 0 and the loop breaks. Safe to call from any thread.
    pub fn request_quit(&self) {
        // SAFETY: PostThreadMessageW is designed for cross-thread posting; the
        // target is our own pump thread's id (captured after it started). A stale
        // id (thread already gone) simply fails and is ignored.
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT,
                Default::default(),
                Default::default(),
            );
        }
    }

    /// Join the pump thread (call after [`Self::request_quit`]).
    pub fn join(self) {
        let _ = self.handle.join();
    }
}

/// The pump thread body: create the manager on THIS thread, register the hotkey,
/// report the thread id, then run the message loop until `WM_QUIT`.
fn pump_body(hotkey: HotKey, label: String, setup_tx: mpsc::Sender<Result<u32, HotkeyError>>) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            let _ = setup_tx.send(Err(HotkeyError::Register(label, e.to_string())));
            return;
        }
    };
    if let Err(e) = manager.register(hotkey) {
        let _ = setup_tx.send(Err(HotkeyError::Register(label, e.to_string())));
        return;
    }

    // SAFETY: GetCurrentThreadId has no preconditions and returns this thread's id.
    let thread_id = unsafe { GetCurrentThreadId() };
    if setup_tx.send(Ok(thread_id)).is_err() {
        return; // caller gone during setup — nothing to pump for
    }
    info!(hotkey = %label, "global save hotkey registered");

    // SAFETY: a standard Win32 message loop on the thread that owns the hotkey
    // window. GetMessageW blocks until a message arrives; it returns >0 for a
    // normal message, 0 on WM_QUIT (→ break), and -1 on error (→ break + log).
    // DispatchMessageW routes WM_HOTKEY to global-hotkey's window proc, which
    // pushes onto GlobalHotKeyEvent::receiver().
    unsafe {
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0).0;
            if ret == 0 {
                break; // WM_QUIT
            }
            if ret == -1 {
                error!("hotkey message loop GetMessageW failed; ending pump");
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // `manager` drops here on the pump thread → DestroyWindow + unregister.
    drop(manager);
    info!("hotkey pump stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_save_hotkey_parses() {
        // The `[hotkeys].save_clip` default must be a valid global-hotkey string.
        assert!(parse_hotkey("Ctrl+Alt+S").is_ok());
    }

    #[test]
    fn parses_letter_and_key_forms_and_fn_keys() {
        assert!(parse_hotkey("Ctrl+Shift+D").is_ok());
        assert!(parse_hotkey("Ctrl+Shift+KeyD").is_ok());
        assert!(parse_hotkey("F8").is_ok());
    }

    #[test]
    fn rejects_garbage_and_modifier_only() {
        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("Ctrl+Shift").is_err()); // no key
        assert!(parse_hotkey("Ctrl+Alt+NopeKey").is_err());
    }
}
