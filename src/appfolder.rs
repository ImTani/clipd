//! `appfolder` — resolve the foreground application's clip subfolder name (A3 / T5).
//!
//! When a clip is saved, it lands in `<clips_dir>/<AppName>/` so a user's clips are
//! grouped by the game/app that was in the foreground at the moment they pressed save.
//! This is **folders-only categorisation, NOT a game database** (`CLAUDE.md` scope): it
//! must never fail or delay a save, so every step degrades gracefully to the next and
//! the whole thing is cheap enough to run inline on the save trigger.
//!
//! ## Resolution chain (`foreground_app_folder`)
//! 1. the foreground window's owning process (reusing the B3 [`crate::audio::binding`]
//!    foreground provider — HW-exercised at B7),
//! 2. → the process's executable path ([`exe_path_for_pid`], a couple of cheap kernel
//!    queries — **no file open**, so the save latency budget is untouched),
//! 3. → the exe **file stem** (`Discord.exe` → `Discord`), sanitised for the filesystem,
//! 4. → `"Other"` when the app can't be identified.
//!
//! ## Deferred (flagged): the version-resource "pretty" name
//! `pick_folder_name` already accepts a version-resource product/description name
//! (`GTA5.exe` → "Grand Theft Auto V") ahead of the exe stem, but reading it is a **file
//! open** (`GetFileVersionInfo`) — the one step the A3 "never delay a save" rule warns
//! about. It is deferred (DECISIONS "T5"): to add it, resolve the name **off** the ring
//! thread (in the mux worker, or via a cached foreground watcher), never inline here.
//! Today `folder_for_exe` passes `None` for the version name, so only the exe stem is used.

use std::path::Path;

/// The folder clips land in when the foreground app can't be identified.
pub const FALLBACK_FOLDER: &str = "Other";

/// Cap a folder name to a sane length (well under any single-component path limit).
const MAX_FOLDER_LEN: usize = 64;

/// The resolved clip subfolder for the CURRENT foreground app (T5). Cheap enough to call
/// inline on the save trigger: a window/PID query + a process-image-path query (no file
/// open) + pure string work. Never fails — an unidentifiable app maps to [`FALLBACK_FOLDER`].
pub fn foreground_app_folder() -> String {
    let Some(fw) = crate::audio::binding::foreground_window() else {
        return FALLBACK_FOLDER.to_string();
    };
    let exe = exe_path_for_pid(fw.pid);
    folder_for_exe(exe.as_deref())
}

/// Turn a foreground exe path into a clip folder name via the [`pick_folder_name`] chain.
/// The version-resource name is deferred (see the module docs), so only the exe stem +
/// the `"Other"` fallback are used today. Pure given the path.
pub fn folder_for_exe(exe: Option<&Path>) -> String {
    let stem = exe.and_then(|p| p.file_stem()).and_then(|s| s.to_str());
    // `None` version name for now (the file-open step is deferred — DECISIONS "T5").
    pick_folder_name(None, stem)
}

/// Assemble the folder name from the resolved candidates in preference order — a
/// version-resource name, then the exe stem — sanitised for the filesystem, falling back
/// to [`FALLBACK_FOLDER`] when nothing usable remains. Pure + unit-tested.
pub fn pick_folder_name(version_name: Option<&str>, exe_stem: Option<&str>) -> String {
    let raw = version_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| exe_stem.map(str::trim).filter(|s| !s.is_empty()));
    match raw {
        Some(s) => {
            let clean = sanitize_folder_name(s);
            if clean.is_empty() {
                FALLBACK_FOLDER.to_string()
            } else {
                clean
            }
        }
        None => FALLBACK_FOLDER.to_string(),
    }
}

/// Make `name` safe as a SINGLE Windows folder component: replace the reserved characters
/// (`<>:"/\|?*` and control chars) with spaces, collapse runs of whitespace, strip the
/// trailing dots/spaces Windows forbids, cap the length, and refuse the reserved DOS
/// device names (`CON`, `NUL`, `COM1`, …). Returns `""` when nothing usable remains, so
/// the caller falls back to [`FALLBACK_FOLDER`]. Pure + unit-tested.
pub fn sanitize_folder_name(name: &str) -> String {
    const RESERVED: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_control() || RESERVED.contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Collapse whitespace, then trim the trailing dots/spaces Windows disallows.
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches(['.', ' ']).trim();
    let capped: String = trimmed.chars().take(MAX_FOLDER_LEN).collect();
    let capped = capped.trim_end_matches(['.', ' ']).trim().to_string();
    if is_reserved_device_name(&capped) {
        return String::new();
    }
    capped
}

/// Whether `name` is a reserved DOS device name (case-insensitive, ignoring any
/// extension) — Windows forbids a file/folder so named. Pure.
fn is_reserved_device_name(name: &str) -> bool {
    // Compare the part before the first '.' (a component like "NUL.txt" is still reserved).
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const DEVICES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    DEVICES.contains(&base.as_str())
}

/// The full executable path for a live process id, via `OpenProcess` +
/// `QueryFullProcessImageNameW` — no file open, just a kernel query of the image path.
/// `None` if the process is gone or access is denied (→ the caller falls back). Confined
/// unsafe (`CLAUDE.md`): the handle is owned and closed on every path.
#[cfg(windows)]
pub fn exe_path_for_pid(pid: u32) -> Option<std::path::PathBuf> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return None;
    }
    // SAFETY: `OpenProcess` with QUERY_LIMITED_INFORMATION returns an owned handle (or an
    // error we map to `None`). It is closed on every return path below.
    let handle: HANDLE =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;

    let mut buf = vec![0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` is the live process handle from OpenProcess; `buf`/`len` describe a
    // valid writable u16 buffer, and on success `len` is set to the character count
    // written (excluding the NUL terminator).
    let query = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: `handle` was returned by OpenProcess and has not been closed elsewhere.
    unsafe {
        let _ = CloseHandle(handle);
    }
    query.ok()?;
    buf.truncate(len as usize);
    Some(std::path::PathBuf::from(String::from_utf16_lossy(&buf)))
}

/// Non-Windows stub (the crate only targets `x86_64-pc-windows-msvc`; this keeps the
/// pure logic + its tests buildable on other hosts). Always `None` → `"Other"`.
#[cfg(not(windows))]
pub fn exe_path_for_pid(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pick_prefers_version_then_stem_then_other() {
        // Version name wins when present.
        assert_eq!(
            pick_folder_name(Some("Grand Theft Auto V"), Some("GTA5")),
            "Grand Theft Auto V"
        );
        // Falls back to the exe stem when there is no version name.
        assert_eq!(pick_folder_name(None, Some("Discord")), "Discord");
        // Blank/whitespace candidates are skipped.
        assert_eq!(pick_folder_name(Some("   "), Some("cs2")), "cs2");
        // Nothing usable → the fallback folder.
        assert_eq!(pick_folder_name(None, None), FALLBACK_FOLDER);
        assert_eq!(pick_folder_name(Some(""), Some("  ")), FALLBACK_FOLDER);
    }

    #[test]
    fn folder_for_exe_uses_the_stem() {
        assert_eq!(
            folder_for_exe(Some(&PathBuf::from(r"C:\Games\Discord\Discord.exe"))),
            "Discord"
        );
        assert_eq!(
            folder_for_exe(Some(&PathBuf::from(r"D:\Steam\common\GTA V\GTA5.exe"))),
            "GTA5"
        );
        // No path → the fallback.
        assert_eq!(folder_for_exe(None), FALLBACK_FOLDER);
    }

    #[test]
    fn sanitize_strips_reserved_chars_and_trailing_dots() {
        assert_eq!(sanitize_folder_name("Half-Life 2"), "Half-Life 2");
        // Reserved path characters become spaces (collapsed).
        assert_eq!(sanitize_folder_name("Portal: Reloaded"), "Portal Reloaded");
        assert_eq!(sanitize_folder_name("a/b\\c|d"), "a b c d");
        // Windows forbids trailing dots/spaces on a component.
        assert_eq!(sanitize_folder_name("Trailing...  "), "Trailing");
        // Control chars are stripped.
        assert_eq!(sanitize_folder_name("Tab\tSeparated"), "Tab Separated");
    }

    #[test]
    fn sanitize_rejects_reserved_device_names() {
        assert_eq!(sanitize_folder_name("NUL"), "");
        assert_eq!(sanitize_folder_name("con"), "");
        assert_eq!(sanitize_folder_name("COM1"), "");
        assert_eq!(sanitize_folder_name("nul.txt"), "");
        // A normal name containing a device substring is fine.
        assert_eq!(sanitize_folder_name("Falcon"), "Falcon");
        // …and the folder picker maps a rejected name to the fallback.
        assert_eq!(pick_folder_name(None, Some("NUL")), FALLBACK_FOLDER);
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "A".repeat(200);
        assert_eq!(sanitize_folder_name(&long).chars().count(), MAX_FOLDER_LEN);
    }
}
