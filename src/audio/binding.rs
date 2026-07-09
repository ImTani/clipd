//! `audio::binding` — live game / voice-chat **PID binding** for the per-source
//! system tracks (Slice B / B3). Decides *which process* feeds the
//! [`AudioTrackKind::Game`] and [`AudioTrackKind::VoiceChat`] tracks at any moment,
//! so [`crate::audio::process_loopback::run_process_capture`] (B2) can capture just
//! that app's audio.
//!
//! ## The split (why this is a separate module)
//! B2 gave us "capture one PID's tree". B3 answers "**which** PID?" — and the answer
//! changes over a session:
//!
//! - **Voice chat** is detected by scanning running processes for a configured image
//!   name ([`crate::config::VcApp`]), **never by window** — a tray-minimized Discord
//!   has no window (`M7-M8-PLAN §5`). Discord's audio lives in an Electron *child*, so
//!   the bound PID is the **top-most same-name** process (the one whose parent is not
//!   also Discord) captured **include-tree**.
//! - **Game** is detected by foreground state, with **no title database** (a hard
//!   non-goal): in *monitor* capture mode the game is whatever app is foreground **and
//!   borderless-fullscreen**; in *window* capture mode it is simply the captured
//!   window's process. The binding sticks while that process lives; a different
//!   fullscreen app retargets it (`SLICE-B-PLAN §3`).
//!
//! ## Structure: pure decision + thin OS probe
//! Everything that *decides* is a pure function over an injected snapshot
//! ([`ProcessInfo`] list, [`ForegroundWindow`]) and is exhaustively unit-tested with
//! no hardware — [`select_vc_pid`], [`classify_game`], [`is_borderless_fullscreen`],
//! and the [`BindingTracker`] retarget state machine. The only things that touch the
//! OS are the two thin snapshot providers ([`enumerate_processes`],
//! [`foreground_window`]) and [`window_pid`]; their `unsafe` is confined here with
//! `// SAFETY:` notes (CLAUDE.md), and they are exercised on hardware at B7 (the
//! `binding-probe` tool), never claimed to work until the machine says so.

use tracing::debug;

use crate::config::VcApp;

// ── Pure model ────────────────────────────────────────────────────────────────

/// One running process, as read by [`enumerate_processes`] — the injected snapshot
/// the pure selectors work over. `image_name` is the bare executable name
/// (`"Discord.exe"`), matched case-insensitively against [`VcApp::process_names`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process id.
    pub pid: u32,
    /// The parent process id (from the Toolhelp snapshot).
    pub parent_pid: u32,
    /// The bare image (executable) name, e.g. `"Discord.exe"`.
    pub image_name: String,
}

/// A plain integer rectangle (screen coordinates), decoupled from the Win32 `RECT`
/// so the fullscreen test stays pure and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The foreground window's owning PID plus its bounds and the bounds of the monitor
/// it sits on — the injected snapshot [`classify_game`] reads in monitor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForegroundWindow {
    /// The PID that owns the foreground window.
    pub pid: u32,
    /// The window rectangle (`GetWindowRect`).
    pub window_rect: Rect,
    /// The rectangle of the monitor the window is on (`GetMonitorInfoW.rcMonitor`).
    pub monitor_rect: Rect,
}

/// How the game track should be bound, derived from the video capture mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameDetect {
    /// Monitor capture: bind whatever app is foreground **and** borderless-fullscreen
    /// (no title database — a pure foreground+fullscreen heuristic).
    ForegroundFullscreen,
    /// Window capture: bind exactly this (the captured window's) process tree.
    Window(u32),
    /// No game binding (e.g. window capture whose PID could not be resolved).
    Off,
}

/// A resolved binding: the PID to capture and whether to include its whole tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub pid: u32,
    pub include_tree: bool,
}

/// Whether `image_name` matches any of `patterns`, case-insensitively (Windows image
/// names are not case-sensitive). Pure.
fn image_matches(image_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| p.eq_ignore_ascii_case(image_name))
}

/// The voice-chat PID to bind, or `None` if no configured app is running. Scans
/// `procs` for the first **enabled** [`VcApp`] (config order — Discord first) that has
/// a running match, and within that app binds the **top-most same-name** process: the
/// matched process whose parent is *not* itself a match for the same app. That is the
/// Electron main process, not a renderer/helper child — capturing it **include-tree**
/// (per the app's [`VcApp::include_tree`]) catches the audio that actually plays in a
/// child (`M7-M8-PLAN §2`). Ties (several top-most matches) break to the lowest PID for
/// determinism. Pure.
pub fn select_vc_pid(procs: &[ProcessInfo], apps: &[VcApp]) -> Option<Binding> {
    for app in apps.iter().filter(|a| a.enabled) {
        // The set of PIDs matching THIS app — used to test "is my parent also a match".
        let matched: Vec<&ProcessInfo> = procs
            .iter()
            .filter(|p| image_matches(&p.image_name, &app.process_names))
            .collect();
        if matched.is_empty() {
            continue;
        }
        let match_pids: std::collections::HashSet<u32> = matched.iter().map(|p| p.pid).collect();
        // Top-most = a match whose parent is not also a match of the same app.
        let top = matched
            .iter()
            .filter(|p| !match_pids.contains(&p.parent_pid))
            .map(|p| p.pid)
            .min();
        // Fall back to the lowest matched PID if every match's parent is (improbably)
        // also a match — still deterministic, still this app.
        let pid = top.or_else(|| matched.iter().map(|p| p.pid).min())?;
        return Some(Binding {
            pid,
            include_tree: app.include_tree,
        });
    }
    None
}

/// Whether `window` covers `monitor` entirely — the borderless-/exclusive-fullscreen
/// test. A borderless-fullscreen game sets its window to the full monitor bounds
/// (`rcMonitor`); a *maximized* window instead covers the work area (`rcWork`, minus
/// the taskbar) so its bottom edge falls short — which is exactly why comparing
/// against `rcMonitor` distinguishes the two. Pure. A zero-area monitor never matches.
pub fn is_borderless_fullscreen(window: Rect, monitor: Rect) -> bool {
    if monitor.right <= monitor.left || monitor.bottom <= monitor.top {
        return false;
    }
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

/// The lowest PID a real foreground app can have. 0 (System Idle) and 4 (System) are
/// never a game; treating them as unbindable keeps a transient empty/`Program Manager`
/// foreground from binding the kernel.
const MIN_REAL_PID: u32 = 8;

/// The game PID to bind for `mode`, or `None`. In [`GameDetect::Window`] it is the
/// captured window's PID (include-tree). In [`GameDetect::ForegroundFullscreen`] it is
/// the foreground PID **iff** that window is borderless-fullscreen ([`is_borderless_fullscreen`])
/// and owned by a real process. Pure. This is the RAW per-poll candidate — [`GameStickiness`]
/// turns it into the actually-bound target (sticky-hold + edge-debounce, F8), so the game
/// track is not unbound the instant the game loses foreground.
pub fn classify_game(mode: GameDetect, foreground: Option<ForegroundWindow>) -> Option<Binding> {
    match mode {
        GameDetect::Off => None,
        GameDetect::Window(pid) if pid >= MIN_REAL_PID => Some(Binding {
            pid,
            include_tree: true,
        }),
        GameDetect::Window(_) => None,
        GameDetect::ForegroundFullscreen => {
            let fg = foreground?;
            if fg.pid >= MIN_REAL_PID && is_borderless_fullscreen(fg.window_rect, fg.monitor_rect) {
                Some(Binding {
                    pid: fg.pid,
                    include_tree: true,
                })
            } else {
                None
            }
        }
    }
}

/// Tracks the currently-bound target for one role and reports when it must retarget.
/// Pure; the capture loop drives it each poll tick and, on a [`Retarget`], tears down
/// the running [`crate::audio::process_loopback::run_process_capture`] and starts a new
/// one for the new PID — the `§2.3` synthesizer fills the silence gap downstream, so
/// the retarget needs no explicit gap plumbing, only a bumped generation for the loop.
#[derive(Debug, Default)]
pub struct BindingTracker {
    current: Option<Binding>,
    /// Increments on every retarget so the capture loop can tell "same target" from
    /// "rebound to a same-looking PID after a gap".
    generation: u64,
    retargets: u64,
}

/// The outcome of a [`BindingTracker::update`] — what (if anything) changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retarget {
    /// The target that was bound before (for logging the gap).
    pub from: Option<Binding>,
    /// The new target (`None` = the app went away; the track goes silent).
    pub to: Option<Binding>,
    /// The generation after this retarget.
    pub generation: u64,
}

impl BindingTracker {
    /// A fresh tracker with nothing bound.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the freshly-computed `desired` target. Returns `Some(Retarget)` iff it
    /// differs from the current binding (a bind, an unbind, or a PID change), having
    /// adopted it and bumped the generation; `None` when nothing changed. Pure.
    pub fn update(&mut self, desired: Option<Binding>) -> Option<Retarget> {
        if desired == self.current {
            return None;
        }
        let from = self.current;
        self.current = desired;
        self.generation += 1;
        self.retargets += 1;
        Some(Retarget {
            from,
            to: desired,
            generation: self.generation,
        })
    }

    /// The currently-bound target.
    pub fn current(&self) -> Option<Binding> {
        self.current
    }

    /// The current generation (bumped on each retarget).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Total retargets observed (diagnostic).
    pub fn retargets(&self) -> u64 {
        self.retargets
    }
}

/// Consecutive polls a NEW foreground-fullscreen candidate must hold before the game
/// binding retargets to it ([`GameStickiness`], F8). At the 600 ms
/// [`crate::engine::BINDING_SCAN_INTERVAL`] this is ~1.2–1.8 s of stability (phase-
/// dependent). It guards the failure mode that stickiness *creates*: a spurious retarget is
/// now expensive (held-wrong until the next candidate), so a fullscreen *flash* — a game's
/// non-fullscreen loading frame, an alt-tab overlay — must not steal the binding. The cost
/// lands on a genuine game switch (a loading screen), where no gameplay audio is lost.
const NEW_CANDIDATE_DEBOUNCE_POLLS: u32 = 3;

/// **Sticky game binding with new-candidate edge-debounce** (F8, DECISIONS 2026-07-09). The
/// game track is *"the last foreground-fullscreen game, held while alive"* — NOT "whatever is
/// foreground-fullscreen right now". [`classify_game`] gives the raw foreground candidate each
/// poll; this pure state machine turns that into the *desired* binding:
///
/// - **Sticky:** an alive bound game is HELD when the foreground leaves it (alt-tab to a
///   window, or another app that is not fullscreen) — its audio keeps playing and is captured.
/// - **Edge-debounce:** a *different* foreground-fullscreen PID (or the first bind from
///   nothing) must persist for [`NEW_CANDIDATE_DEBOUNCE_POLLS`] consecutive polls before it
///   retargets; a flash resets the count.
/// - **Liveness is the unbind-of-last-resort:** a bound PID that has died clears **immediately**
///   (no debounce — the process is gone), on the next poll.
///
/// Feeds [`BindingTracker::update`], which owns the retarget/generation bookkeeping and the
/// one-`desired`-drives-both dual-publish (game-include + other-system-exclude).
#[derive(Debug, Default)]
pub struct GameStickiness {
    /// A candidate accumulating consecutive-poll confirmations toward a retarget: the
    /// binding and how many consecutive polls it has held foreground-fullscreen.
    pending: Option<(Binding, u32)>,
}

impl GameStickiness {
    /// A fresh policy with no pending candidate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide the desired game binding for this poll (pure). `current` is what is bound now,
    /// `raw` the foreground-fullscreen candidate from [`classify_game`] this poll (`None` =
    /// nothing fullscreen), and `alive(pid)` reports process liveness (the caller's process
    /// snapshot). See the type docs for the sticky + debounce + liveness rules.
    pub fn decide(
        &mut self,
        current: Option<Binding>,
        raw: Option<Binding>,
        alive: impl Fn(u32) -> bool,
    ) -> Option<Binding> {
        // 1. Liveness is the unbind-of-last-resort: a dead held PID clears NOW, no debounce.
        if let Some(b) = current {
            if !alive(b.pid) {
                self.pending = None;
                return None;
            }
        }
        match (current, raw) {
            // The held game is still foreground-fullscreen → hold; drop any stale pending.
            (Some(b), Some(r)) if r.pid == b.pid => {
                self.pending = None;
                Some(b)
            }
            // Foreground left the game (alt-tab / a non-fullscreen app) → STICKY: keep the
            // live game bound and its audio captured; cancel any in-flight candidate.
            (Some(b), None) => {
                self.pending = None;
                Some(b)
            }
            // A DIFFERENT foreground-fullscreen candidate, or the first bind from nothing →
            // edge-debounce it; hold `current` meanwhile.
            (_, Some(r)) => self.debounce(current, r, &alive),
            // Nothing bound, nothing fullscreen → stay unbound.
            (None, None) => {
                self.pending = None;
                None
            }
        }
    }

    /// Accumulate consecutive-poll confirmations for the new candidate `cand`; retarget once
    /// it has held [`NEW_CANDIDATE_DEBOUNCE_POLLS`] polls. While it proves itself, keep
    /// `current` (sticky). A dead candidate is ignored (keeps `current`).
    fn debounce(
        &mut self,
        current: Option<Binding>,
        cand: Binding,
        alive: &impl Fn(u32) -> bool,
    ) -> Option<Binding> {
        if !alive(cand.pid) {
            self.pending = None;
            return current;
        }
        let count = match self.pending {
            Some((p, n)) if p.pid == cand.pid => n + 1,
            _ => 1, // a new / changed candidate restarts the counter
        };
        if count >= NEW_CANDIDATE_DEBOUNCE_POLLS {
            self.pending = None;
            Some(cand) // confirmed stable → retarget
        } else {
            self.pending = Some((cand, count));
            current // still holding the prior binding while the candidate proves itself
        }
    }
}

// ── OS snapshot providers (confined unsafe; HW-exercised at B7) ────────────────

#[cfg(windows)]
mod os {
    use super::{ForegroundWindow, ProcessInfo, Rect};

    use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    };

    /// Decode the `szExeFile` WCHAR array (NUL-terminated) to a `String`.
    fn exe_name(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// A snapshot of every running process (pid, parent pid, image name) via
    /// Toolhelp. Returns an empty vec if the snapshot cannot be taken (the pure
    /// selectors then simply find no match — the track stays silent).
    pub fn enumerate_processes() -> Vec<ProcessInfo> {
        let mut out = Vec::new();
        // SAFETY: `CreateToolhelp32Snapshot` returns an owned handle we close below;
        // `TH32CS_SNAPPROCESS` with pid 0 snapshots all processes. On failure the
        // call returns Err and we return the empty vec.
        let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            Ok(h) if !h.is_invalid() => h,
            _ => return out,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry.dwSize` is set as the API requires; `snapshot` is valid; we
        // iterate First → Next until it returns Err (no more entries). `szExeFile` is
        // a fixed WCHAR array owned by `entry`; no pointer escapes it.
        unsafe {
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    out.push(ProcessInfo {
                        pid: entry.th32ProcessID,
                        parent_pid: entry.th32ParentProcessID,
                        image_name: exe_name(&entry.szExeFile),
                    });
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            // SAFETY: `snapshot` was returned by `CreateToolhelp32Snapshot` and not
            // closed elsewhere.
            let _ = CloseHandle(snapshot);
        }
        out
    }

    fn rect_of(r: RECT) -> Rect {
        Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    /// The owning PID of `hwnd`, or `None` for an invalid handle. Used to resolve the
    /// captured window's process in window capture mode.
    pub fn window_pid(hwnd: isize) -> Option<u32> {
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        let mut pid: u32 = 0;
        // SAFETY: `GetWindowThreadProcessId` writes the owning PID into `pid`; a bad
        // handle yields a 0 thread id and leaves `pid` 0, which we reject.
        let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }

    /// The foreground window's PID, bounds, and monitor bounds — the monitor-mode
    /// game snapshot. `None` if there is no foreground window or its geometry can't be
    /// read (the game track then binds nothing this tick).
    pub fn foreground_window() -> Option<ForegroundWindow> {
        // SAFETY: `GetForegroundWindow` is a pure query; a null result means no
        // foreground window.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        // SAFETY: writes the owning PID into `pid`; `hwnd` is the just-queried valid
        // foreground window.
        let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            return None;
        }
        let mut wr = RECT::default();
        // SAFETY: `GetWindowRect` fills the caller-owned `wr` for the valid `hwnd`.
        if unsafe { GetWindowRect(hwnd, &mut wr) }.is_err() {
            return None;
        }
        // SAFETY: `MonitorFromWindow` / `GetMonitorInfoW` are pure queries; `mi` is a
        // caller-owned struct with `cbSize` set as the API requires.
        let monitor_rect = unsafe {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if hmon.is_invalid() {
                return None;
            }
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return None;
            }
            rect_of(mi.rcMonitor)
        };
        Some(ForegroundWindow {
            pid,
            window_rect: rect_of(wr),
            monitor_rect,
        })
    }
}

/// A snapshot of every running process via the OS (Toolhelp). Empty on failure — the
/// pure selectors then find no match and the track stays silent. See [`os`].
#[cfg(windows)]
pub fn enumerate_processes() -> Vec<ProcessInfo> {
    os::enumerate_processes()
}

/// The foreground window's PID + geometry, or `None`. Monitor-mode game snapshot.
#[cfg(windows)]
pub fn foreground_window() -> Option<ForegroundWindow> {
    os::foreground_window()
}

/// The owning PID of an HWND (window-mode game), or `None` for an invalid handle.
#[cfg(windows)]
pub fn window_pid(hwnd: isize) -> Option<u32> {
    os::window_pid(hwnd)
}

/// Log a retarget for `role_label` at a stable, greppable target so "why did my game
/// track go silent / rebind" is answerable from the log (CLAUDE.md trust model).
pub fn log_retarget(role_label: &str, r: &Retarget) {
    match (r.from, r.to) {
        (_, Some(to)) => debug!(
            role = role_label,
            pid = to.pid,
            include_tree = to.include_tree,
            generation = r.generation,
            "audio binding retargeted (§2.3 fills the gap)"
        ),
        (Some(from), None) => debug!(
            role = role_label,
            prev_pid = from.pid,
            generation = r.generation,
            "audio binding cleared — target gone; track goes silent (§2.3)"
        ),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            image_name: name.to_string(),
        }
    }

    fn discord_app() -> VcApp {
        VcApp {
            name: "Discord".into(),
            process_names: vec!["Discord.exe".into(), "DiscordPTB.exe".into()],
            include_tree: true,
            enabled: true,
        }
    }

    // ── VC selection ──────────────────────────────────────────────────────────

    #[test]
    fn vc_none_running_binds_nothing() {
        let procs = vec![proc(100, 4, "explorer.exe"), proc(200, 100, "chrome.exe")];
        assert_eq!(select_vc_pid(&procs, &[discord_app()]), None);
    }

    #[test]
    fn vc_picks_top_most_same_name_over_electron_child() {
        // The Electron main (pid 500, parent = explorer) spawns a same-name child
        // (pid 600, parent = 500) that actually renders audio. We must bind the MAIN
        // (500) include-tree, not the child.
        let procs = vec![
            proc(300, 4, "explorer.exe"),
            proc(600, 500, "Discord.exe"), // child helper
            proc(500, 300, "Discord.exe"), // main (parent is not Discord)
        ];
        assert_eq!(
            select_vc_pid(&procs, &[discord_app()]),
            Some(Binding {
                pid: 500,
                include_tree: true,
            })
        );
    }

    #[test]
    fn vc_case_insensitive_image_match() {
        let procs = vec![proc(500, 300, "discord.EXE")];
        assert_eq!(
            select_vc_pid(&procs, &[discord_app()]).map(|b| b.pid),
            Some(500)
        );
    }

    #[test]
    fn vc_honours_config_order_first_running_app_wins() {
        let teamspeak = VcApp {
            name: "TeamSpeak".into(),
            process_names: vec!["ts3client_win64.exe".into()],
            include_tree: false,
            enabled: true,
        };
        let procs = vec![
            proc(700, 4, "ts3client_win64.exe"),
            proc(500, 300, "Discord.exe"),
        ];
        // Discord is listed first → it wins even though TeamSpeak is also running.
        let apps = vec![discord_app(), teamspeak];
        assert_eq!(select_vc_pid(&procs, &apps).map(|b| b.pid), Some(500));
    }

    #[test]
    fn vc_skips_disabled_app() {
        let mut app = discord_app();
        app.enabled = false;
        let procs = vec![proc(500, 300, "Discord.exe")];
        assert_eq!(select_vc_pid(&procs, &[app]), None);
    }

    #[test]
    fn vc_include_tree_follows_the_app_config() {
        let mut app = discord_app();
        app.include_tree = false;
        let procs = vec![proc(500, 300, "Discord.exe")];
        assert_eq!(
            select_vc_pid(&procs, &[app]),
            Some(Binding {
                pid: 500,
                include_tree: false,
            })
        );
    }

    #[test]
    fn vc_ties_break_to_lowest_pid() {
        // Two independent top-most Discords (two accounts) → deterministic lowest pid.
        let procs = vec![proc(900, 300, "Discord.exe"), proc(500, 300, "Discord.exe")];
        assert_eq!(
            select_vc_pid(&procs, &[discord_app()]).map(|b| b.pid),
            Some(500)
        );
    }

    // ── fullscreen test ───────────────────────────────────────────────────────

    fn mon() -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    }

    #[test]
    fn fullscreen_exact_cover_is_fullscreen() {
        assert!(is_borderless_fullscreen(mon(), mon()));
    }

    #[test]
    fn fullscreen_window_larger_than_monitor_still_counts() {
        // Exclusive-fullscreen can report a window slightly larger than the monitor.
        let win = Rect {
            left: -1,
            top: -1,
            right: 1921,
            bottom: 1081,
        };
        assert!(is_borderless_fullscreen(win, mon()));
    }

    #[test]
    fn fullscreen_maximized_window_short_of_taskbar_is_not_fullscreen() {
        // Maximized covers the work area — 40 px short of the monitor bottom (taskbar).
        let win = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert!(!is_borderless_fullscreen(win, mon()));
    }

    #[test]
    fn fullscreen_windowed_is_not_fullscreen() {
        let win = Rect {
            left: 100,
            top: 100,
            right: 900,
            bottom: 700,
        };
        assert!(!is_borderless_fullscreen(win, mon()));
    }

    #[test]
    fn fullscreen_zero_area_monitor_never_matches() {
        let zero = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        assert!(!is_borderless_fullscreen(zero, zero));
    }

    // ── game classification ───────────────────────────────────────────────────

    #[test]
    fn game_window_mode_binds_captured_pid_include_tree() {
        assert_eq!(
            classify_game(GameDetect::Window(4242), None),
            Some(Binding {
                pid: 4242,
                include_tree: true,
            })
        );
    }

    #[test]
    fn game_window_mode_rejects_system_pid() {
        assert_eq!(classify_game(GameDetect::Window(4), None), None);
    }

    #[test]
    fn game_off_binds_nothing() {
        assert_eq!(classify_game(GameDetect::Off, None), None);
    }

    #[test]
    fn game_monitor_mode_binds_foreground_fullscreen() {
        let fg = ForegroundWindow {
            pid: 4242,
            window_rect: mon(),
            monitor_rect: mon(),
        };
        assert_eq!(
            classify_game(GameDetect::ForegroundFullscreen, Some(fg)),
            Some(Binding {
                pid: 4242,
                include_tree: true,
            })
        );
    }

    #[test]
    fn game_monitor_mode_ignores_windowed_foreground() {
        let fg = ForegroundWindow {
            pid: 4242,
            window_rect: Rect {
                left: 100,
                top: 100,
                right: 900,
                bottom: 700,
            },
            monitor_rect: mon(),
        };
        assert_eq!(
            classify_game(GameDetect::ForegroundFullscreen, Some(fg)),
            None
        );
    }

    #[test]
    fn game_monitor_mode_no_foreground_binds_nothing() {
        assert_eq!(classify_game(GameDetect::ForegroundFullscreen, None), None);
    }

    #[test]
    fn game_monitor_mode_rejects_fullscreen_system_pid() {
        // The desktop (Program Manager / a low system pid) covering the screen must
        // not bind as a game.
        let fg = ForegroundWindow {
            pid: 4,
            window_rect: mon(),
            monitor_rect: mon(),
        };
        assert_eq!(
            classify_game(GameDetect::ForegroundFullscreen, Some(fg)),
            None
        );
    }

    // ── retarget state machine ────────────────────────────────────────────────

    #[test]
    fn tracker_first_bind_is_a_retarget() {
        let mut t = BindingTracker::new();
        let b = Binding {
            pid: 500,
            include_tree: true,
        };
        let r = t.update(Some(b)).expect("first bind retargets");
        assert_eq!(r.from, None);
        assert_eq!(r.to, Some(b));
        assert_eq!(r.generation, 1);
        assert_eq!(t.current(), Some(b));
    }

    #[test]
    fn tracker_same_target_is_no_op() {
        let mut t = BindingTracker::new();
        let b = Binding {
            pid: 500,
            include_tree: true,
        };
        assert!(t.update(Some(b)).is_some());
        assert!(t.update(Some(b)).is_none()); // unchanged
        assert_eq!(t.generation(), 1);
        assert_eq!(t.retargets(), 1);
    }

    #[test]
    fn tracker_pid_change_retargets_and_bumps_generation() {
        let mut t = BindingTracker::new();
        let a = Binding {
            pid: 500,
            include_tree: true,
        };
        let b = Binding {
            pid: 900,
            include_tree: true,
        };
        t.update(Some(a));
        let r = t.update(Some(b)).expect("pid change retargets");
        assert_eq!(r.from, Some(a));
        assert_eq!(r.to, Some(b));
        assert_eq!(r.generation, 2);
    }

    #[test]
    fn tracker_unbind_is_a_retarget_to_none() {
        let mut t = BindingTracker::new();
        let a = Binding {
            pid: 500,
            include_tree: true,
        };
        t.update(Some(a));
        let r = t.update(None).expect("losing the target retargets");
        assert_eq!(r.from, Some(a));
        assert_eq!(r.to, None);
        assert_eq!(t.current(), None);
        assert_eq!(t.retargets(), 2);
    }

    #[test]
    fn tracker_stays_unbound_without_events() {
        let mut t = BindingTracker::new();
        assert!(t.update(None).is_none());
        assert_eq!(t.generation(), 0);
    }

    // ───────────────────────── F8: sticky game binding + debounce ─────────────────────────

    fn gb(pid: u32) -> Option<Binding> {
        Some(Binding {
            pid,
            include_tree: true,
        })
    }

    /// Bind `pid` by feeding it as the raw candidate for the full debounce, threading the
    /// decided value as `current` (as the engine's `game.update(desired)` does).
    fn bind(p: &mut GameStickiness, pid: u32) -> Option<Binding> {
        let mut cur = None;
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS {
            cur = p.decide(cur, gb(pid), |_| true);
        }
        assert_eq!(cur, gb(pid), "should be bound after the debounce");
        cur
    }

    #[test]
    fn sticky_holds_the_live_game_across_a_foreground_change() {
        let mut p = GameStickiness::new();
        let mut cur = bind(&mut p, 1);
        // Alt-tab away (nothing fullscreen) for many polls — the live game stays bound.
        for _ in 0..5 {
            cur = p.decide(cur, None, |_| true);
            assert_eq!(cur, gb(1), "an alt-tabbed but live game must stay bound");
        }
    }

    #[test]
    fn retargets_to_a_new_candidate_only_after_the_debounce() {
        let mut p = GameStickiness::new();
        let mut cur = bind(&mut p, 1);
        // A different fullscreen candidate (pid 2) must hold DEBOUNCE polls; pid 1 held until.
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS - 1 {
            cur = p.decide(cur, gb(2), |_| true);
            assert_eq!(
                cur,
                gb(1),
                "holds the old game while the candidate proves itself"
            );
        }
        cur = p.decide(cur, gb(2), |_| true);
        assert_eq!(
            cur,
            gb(2),
            "retargets once the candidate is confirmed stable"
        );
    }

    #[test]
    fn a_sub_debounce_fullscreen_flash_does_not_retarget() {
        let mut p = GameStickiness::new();
        let mut cur = bind(&mut p, 1);
        // pid 2 flashes fullscreen for fewer than the debounce polls, then vanishes.
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS - 1 {
            cur = p.decide(cur, gb(2), |_| true);
        }
        assert_eq!(cur, gb(1));
        cur = p.decide(cur, None, |_| true); // flash gone → pending cleared, game held
        assert_eq!(cur, gb(1));
        // pid 2 reappearing must start the count over (one poll is not enough).
        cur = p.decide(cur, gb(2), |_| true);
        assert_eq!(cur, gb(1), "a flash must not accumulate toward a retarget");
    }

    #[test]
    fn flip_back_to_the_held_game_cancels_a_pending_candidate() {
        let mut p = GameStickiness::new();
        let mut cur = bind(&mut p, 1);
        cur = p.decide(cur, gb(2), |_| true); // pid 2 pending (1)
        assert_eq!(cur, gb(1));
        cur = p.decide(cur, gb(1), |_| true); // back to the held game → pending cleared
        assert_eq!(cur, gb(1));
        // pid 2 must now earn the FULL debounce again, not resume from 1.
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS - 1 {
            cur = p.decide(cur, gb(2), |_| true);
            assert_eq!(cur, gb(1));
        }
        cur = p.decide(cur, gb(2), |_| true);
        assert_eq!(cur, gb(2));
    }

    #[test]
    fn a_dead_bound_pid_unbinds_immediately_without_debounce() {
        let mut p = GameStickiness::new();
        let cur = bind(&mut p, 1);
        // The bound PID is gone → unbind on the very next poll, whatever the raw candidate.
        assert_eq!(p.decide(cur, gb(1), |_| false), None);
        assert_eq!(p.decide(cur, None, |_| false), None);
    }

    #[test]
    fn first_bind_from_nothing_is_debounced() {
        let mut p = GameStickiness::new();
        let mut cur = None;
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS - 1 {
            cur = p.decide(cur, gb(7), |_| true);
            assert_eq!(cur, None, "the first bind is debounced too");
        }
        cur = p.decide(cur, gb(7), |_| true);
        assert_eq!(cur, gb(7));
    }

    #[test]
    fn a_dead_new_candidate_never_binds() {
        let mut p = GameStickiness::new();
        // A fullscreen candidate whose process is already gone must not bind.
        for _ in 0..NEW_CANDIDATE_DEBOUNCE_POLLS + 2 {
            assert_eq!(p.decide(None, gb(9), |_| false), None);
        }
    }
}
