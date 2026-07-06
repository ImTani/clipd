//! `autostart` — the optional "start with Windows" HKCU Run-key toggle (M5).
//!
//! `05-MILESTONE-TRACKER.md` M5: *"Start-with-Windows (registry Run key, off by
//! default)."* `06-SAFETY-AND-VMS.md` / `CLAUDE.md` constraint 5: the HKCU Run key
//! is the **one** registry write this project makes — no other keys, no HKLM.
//!
//! Enabled ⇔ a `HKCU\…\Run` value named after the product exists, holding
//! `"<exe>" buffer` so a logon launches straight into the replay buffer.
//!
//! ## `unsafe` / threading
//! The registry calls are confined here (an OS-wrapper module per `CLAUDE.md`),
//! each `unsafe` block carrying a `SAFETY:` note. The value-string builder is pure
//! and unit-tested; the registry I/O is validated by the `04-TEST-MACHINE.md`
//! reg-query procedure (it mutates real per-user state, so it is not unit-tested).

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

use crate::spec_constants::PRODUCT_NAME;

/// The Run subkey (relative to `HKEY_CURRENT_USER`).
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Errors from toggling the autostart entry.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// The current executable path could not be resolved.
    #[error("resolving the current exe: {0}")]
    Exe(#[source] std::io::Error),
    /// A registry operation failed (with its `WIN32_ERROR` code).
    #[error("registry {op} failed (win32 error {code})")]
    Registry {
        /// Which operation (open/set/delete).
        op: &'static str,
        /// The raw `WIN32_ERROR` value.
        code: u32,
    },
}

/// The Run-key value: the quoted exe path followed by the `buffer` subcommand, so
/// a logon starts the replay buffer. Pure — unit-tested.
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\" buffer", exe.display())
}

/// UTF-16LE bytes of `s` with a trailing NUL — the `REG_SZ` payload for
/// [`RegSetValueExW`]. Pure.
fn reg_sz_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// A NUL-terminated wide string for a `PCWSTR` argument. The returned `Vec` must
/// outlive the pointer. Pure.
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open the Run key with the requested access, running `f` with the handle and
/// closing it afterward. Centralizes the open/close so each caller's `unsafe` is
/// just the one operation.
fn with_run_key<T>(
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
    op: &'static str,
    f: impl FnOnce(HKEY) -> Result<T, AutostartError>,
) -> Result<T, AutostartError> {
    let subkey = wide_nul(RUN_SUBKEY);
    let mut hkey = HKEY::default();
    // SAFETY: RegOpenKeyExW with a valid predefined root (HKCU), a NUL-terminated
    // wide subkey that outlives the call, and a valid out-param. The Run key always
    // exists for the current user.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            access,
            &mut hkey,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(AutostartError::Registry { op, code: status.0 });
    }
    let result = f(hkey);
    // SAFETY: hkey was successfully opened above and is not used after this.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    result
}

/// Whether the autostart entry is currently present. Any failure (key/value
/// missing, access denied) reads as "not enabled".
pub fn is_enabled() -> bool {
    let name = wide_nul(PRODUCT_NAME);
    with_run_key(KEY_QUERY_VALUE, "open", |hkey| {
        // SAFETY: querying existence only — all optional out-params are None, so the
        // call just reports whether the value exists. `name` outlives the call.
        let status =
            unsafe { RegQueryValueExW(hkey, PCWSTR(name.as_ptr()), None, None, None, None) };
        Ok(status == ERROR_SUCCESS)
    })
    .unwrap_or(false)
}

/// Enable or disable the autostart entry. Enabling writes `"<current exe>" buffer`;
/// disabling deletes the value (an already-absent value is treated as success).
pub fn set_enabled(enable: bool) -> Result<(), AutostartError> {
    let name = wide_nul(PRODUCT_NAME);
    if enable {
        let exe = std::env::current_exe().map_err(AutostartError::Exe)?;
        let data = reg_sz_bytes(&run_value(&exe));
        with_run_key(KEY_SET_VALUE, "open", |hkey| {
            // SAFETY: writing a REG_SZ value — `name` and `data` outlive the call;
            // `data` is UTF-16LE with a NUL terminator as REG_SZ requires.
            let status =
                unsafe { RegSetValueExW(hkey, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data)) };
            if status == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(AutostartError::Registry {
                    op: "set",
                    code: status.0,
                })
            }
        })
    } else {
        with_run_key(KEY_SET_VALUE, "open", |hkey| {
            // SAFETY: deleting the named value; `name` outlives the call.
            let status = unsafe { RegDeleteValueW(hkey, PCWSTR(name.as_ptr())) };
            // Already-absent is success (idempotent disable).
            if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(AutostartError::Registry {
                    op: "delete",
                    code: status.0,
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_quotes_the_exe_and_appends_buffer() {
        let v = run_value(Path::new(r"C:\Program Files\clipd\clipd.exe"));
        assert_eq!(v, "\"C:\\Program Files\\clipd\\clipd.exe\" buffer");
    }

    #[test]
    fn reg_sz_bytes_are_utf16le_nul_terminated() {
        // "Hi" → 'H','i',NUL in UTF-16LE = 6 bytes.
        let b = reg_sz_bytes("Hi");
        assert_eq!(b, vec![b'H', 0, b'i', 0, 0, 0]);
    }

    #[test]
    fn wide_nul_terminates() {
        assert_eq!(wide_nul("Ab"), vec![0x41, 0x62, 0x00]);
    }
}
