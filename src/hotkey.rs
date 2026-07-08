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
use tracing::{error, info, warn};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_APP, WM_QUIT,
};

/// A private thread message posted to wake the pump's blocking `GetMessageW` loop so
/// it drains the control channel (live availability probes from the settings editor).
/// `WM_APP` is the Windows-sanctioned base for app-private messages, well clear of
/// `WM_HOTKEY`.
const WM_HOTKEY_CONTROL: u32 = WM_APP;

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

/// The result of a live hotkey-availability probe, surfaced in the settings editor so
/// a rebind can warn "already in use by another app" without waiting for a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The combo registered cleanly (or is already held by this app) — bindable.
    Available,
    /// Another application already owns the combo — it would not fire for us.
    Taken,
    /// The probe could not be completed (pump gone / unparseable) — check skipped.
    Unknown,
}

/// One live availability probe sent to the pump thread: a candidate combo plus a
/// one-shot reply channel the caller polls.
struct ControlRequest {
    combo: String,
    reply: crossbeam_channel::Sender<Availability>,
}

/// A cheap, cloneable handle to the running pump's control channel. The settings
/// editor uses it to test whether a freshly-bound combo is free or already owned by
/// another app (live "combo already taken" detection). Independent of the
/// [`HotkeyPump`]'s lifetime — it only holds the channel sender + the pump's thread id.
#[derive(Clone)]
pub struct HotkeyControl {
    thread_id: u32,
    tx: crossbeam_channel::Sender<ControlRequest>,
}

impl HotkeyControl {
    /// Ask the pump to probe `combo`'s availability. Returns immediately with a
    /// receiver the caller polls — non-blocking (`try_send`, never `send`), so the UI
    /// thread never stalls on the pump even if its control queue is momentarily full.
    /// A disconnected/never-filled receiver (pump gone or queue full) ⇒ the caller
    /// maps it to [`Availability::Unknown`].
    #[must_use]
    pub fn check(&self, combo: &str) -> crossbeam_channel::Receiver<Availability> {
        let (reply, reply_rx) = crossbeam_channel::bounded(1);
        if self
            .tx
            .try_send(ControlRequest {
                combo: combo.to_string(),
                reply,
            })
            .is_ok()
        {
            self.wake();
        }
        // On a send failure (pump gone, or queue full) the reply sender is dropped with
        // the request, so `reply_rx` disconnects → the caller reads it as Unknown.
        reply_rx
    }

    /// Wake the pump thread (blocked in `GetMessageW`) so it drains the control channel
    /// this iteration.
    fn wake(&self) {
        // SAFETY: PostThreadMessageW is designed for cross-thread posting; the target
        // is our own pump thread's id (captured after it started). A stale id (pump
        // already exited) simply fails and is ignored — the caller's receiver then
        // reports Disconnected → Unknown.
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_HOTKEY_CONTROL,
                Default::default(),
                Default::default(),
            );
        }
    }
}

/// A running hotkey pump: the message-loop thread plus the ids needed to match its
/// events (one per registered hotkey, in registration order) and the Win32 thread id
/// used to stop it.
pub struct HotkeyPump {
    handle: JoinHandle<()>,
    thread_id: u32,
    hotkey_ids: Vec<u32>,
    /// Sender for live availability probes; cloned into each [`HotkeyControl`]. The
    /// pump thread holds the matching receiver and drains it when woken.
    control_tx: crossbeam_channel::Sender<ControlRequest>,
}

impl HotkeyPump {
    /// Spawn the pump thread, register every hotkey in `hotkey_strs`, and block until
    /// it reports success or failure. On success the message loop is running.
    /// [`Self::hotkey_id`] returns the event id for each, by registration index.
    pub fn spawn(hotkey_strs: &[&str]) -> Result<Self, HotkeyError> {
        // Validate + parse all up front so a bad string surfaces without spawning.
        let hotkeys: Vec<HotKey> = hotkey_strs
            .iter()
            .map(|s| parse_hotkey(s))
            .collect::<Result<_, _>>()?;
        let hotkey_ids: Vec<u32> = hotkeys.iter().map(|h| h.id()).collect();
        let labels: Vec<String> = hotkey_strs.iter().map(|s| s.to_string()).collect();

        // Bounded to match project convention (no unbounded channels): availability
        // probes are user-interaction-paced and drained fast, so a small cap is ample;
        // a full queue (pathological mashing while the pump is briefly busy) drops the
        // newest probe to `Unknown` rather than blocking the UI thread. See `check`.
        let (control_tx, control_rx) = crossbeam_channel::bounded::<ControlRequest>(8);
        let (setup_tx, setup_rx) = mpsc::channel::<Result<u32, HotkeyError>>();
        let handle = std::thread::Builder::new()
            .name("hotkey".to_string())
            .spawn(move || pump_body(hotkeys, labels, control_rx, setup_tx))
            .expect("thread spawn should not fail");

        match setup_rx.recv() {
            Ok(Ok(thread_id)) => Ok(HotkeyPump {
                handle,
                thread_id,
                hotkey_ids,
                control_tx,
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

    /// The [`global_hotkey::GlobalHotKeyEvent`] id for the hotkey registered at
    /// `index` (the order passed to [`Self::spawn`]).
    pub fn hotkey_id(&self, index: usize) -> u32 {
        self.hotkey_ids.get(index).copied().unwrap_or(0)
    }

    /// A cloneable handle for live availability probes (the settings editor's
    /// "combo already taken" check). Cheap — clones the channel sender.
    pub fn control(&self) -> HotkeyControl {
        HotkeyControl {
            thread_id: self.thread_id,
            tx: self.control_tx.clone(),
        }
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

/// The pump thread body: create the manager on THIS thread, register every hotkey,
/// report the thread id, then run the message loop until `WM_QUIT`.
fn pump_body(
    hotkeys: Vec<HotKey>,
    labels: Vec<String>,
    control_rx: crossbeam_channel::Receiver<ControlRequest>,
    setup_tx: mpsc::Sender<Result<u32, HotkeyError>>,
) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            let label = labels.first().cloned().unwrap_or_default();
            let _ = setup_tx.send(Err(HotkeyError::Register(label, e.to_string())));
            return;
        }
    };
    // Register each hotkey; a failure (e.g. the combo is already owned by another app)
    // is NON-fatal — warn and carry on so the other hotkeys (and the rest of buffer
    // mode) still work. The unregistered hotkey's id simply never fires. The ids we
    // DID register are kept so a live availability probe recognizes a combo this app
    // already holds (else re-probing our own combo would look "taken").
    let mut registered = Vec::new();
    let mut registered_ids = Vec::new();
    for (hotkey, label) in hotkeys.iter().zip(&labels) {
        match manager.register(*hotkey) {
            Ok(()) => {
                registered.push(label.clone());
                registered_ids.push(hotkey.id());
            }
            Err(e) => warn!(
                hotkey = %label, error = %e,
                "could not register hotkey (already in use by another app?) — it will not work"
            ),
        }
    }

    // SAFETY: GetCurrentThreadId has no preconditions and returns this thread's id.
    let thread_id = unsafe { GetCurrentThreadId() };
    if setup_tx.send(Ok(thread_id)).is_err() {
        return; // caller gone during setup — nothing to pump for
    }
    info!(hotkeys = ?registered, "global hotkeys registered");

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
            // A wake from `HotkeyControl` — drain and answer the queued availability
            // probes on this thread (register/unregister must run where the manager
            // lives). Thread messages carry no window, so they never Dispatch.
            if msg.message == WM_HOTKEY_CONTROL {
                drain_control(&manager, &registered_ids, &control_rx);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // `manager` drops here on the pump thread → DestroyWindow + unregister.
    drop(manager);
    info!("hotkey pump stopped");
}

/// Drain and answer every queued availability probe (called when the pump thread is
/// woken by a [`WM_HOTKEY_CONTROL`] post).
fn drain_control(
    manager: &GlobalHotKeyManager,
    registered_ids: &[u32],
    control_rx: &crossbeam_channel::Receiver<ControlRequest>,
) {
    while let Ok(req) = control_rx.try_recv() {
        let _ = req
            .reply
            .send(check_availability(manager, registered_ids, &req.combo));
    }
}

/// Test whether `combo` can be registered right now. A combo this app already holds
/// counts as available; otherwise we momentarily register it — releasing it at once,
/// since the real binding is applied from config at restart — and report success
/// versus a conflict with another application. Runs only on the pump thread (the
/// manager is `!Send`).
///
/// The register→unregister probe makes the candidate a live OS hotkey for a moment, so
/// Windows could deliver a `WM_HOTKEY` for it (e.g. key auto-repeat right after a
/// press-to-bind). That is harmless ONLY because the buffer engine filters strictly on
/// the `save`/`record` ids captured at startup, and a probed combo is by construction
/// never one of those (a combo equal to a currently-held one short-circuits above
/// without ever registering). Do not weaken that engine-side id filter without
/// revisiting this.
fn check_availability(
    manager: &GlobalHotKeyManager,
    registered_ids: &[u32],
    combo: &str,
) -> Availability {
    let hotkey = match parse_hotkey(combo) {
        Ok(h) => h,
        Err(_) => return Availability::Unknown,
    };
    if registered_ids.contains(&hotkey.id()) {
        return Availability::Available;
    }
    match manager.register(hotkey) {
        Ok(()) => {
            // Release the probe immediately. If unregister fails (rare), the combo stays
            // registered to this process for the session — log it: it would then be
            // misreported as `Taken` on a re-probe (it is not in `registered_ids`) and
            // leaks a RegisterHotKey slot.
            if let Err(e) = manager.unregister(hotkey) {
                warn!(
                    hotkey = %combo, error = %e,
                    "could not release a probed hotkey; it may stay registered to this process"
                );
            }
            Availability::Available
        }
        Err(_) => Availability::Taken,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_hotkeys_parse() {
        // Both `[hotkeys]` defaults must be valid global-hotkey strings, or the
        // shipped default hotkey silently would not work. Sourced from the config
        // default so a future change stays covered.
        let cfg = crate::config::HotkeyConfig::default();
        assert!(
            parse_hotkey(&cfg.save_clip).is_ok(),
            "save_clip: {}",
            cfg.save_clip
        );
        assert!(
            parse_hotkey(&cfg.record_toggle).is_ok(),
            "record_toggle: {}",
            cfg.record_toggle
        );
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
