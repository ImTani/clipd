//! `ui::folder_dialog` — a native folder picker for the Output-folder **Browse…** button
//! (U10 / D-U10).
//!
//! A confined-`unsafe` COM wrapper over the `windows` crate's `IFileOpenDialog`
//! (`FOS_PICKFOLDERS`) — **no** new crate (no `rfd`), just one `Win32_UI_Shell` +
//! `Win32_System_Com` feature gate. It runs on the settings-UI thread, which `winit` has
//! already initialised into an STA (so no `CoInitialize` here). It returns the chosen
//! folder, or `None` on cancel / any failure — never panics. The Save-time
//! `validate_output_dir` stays the backstop for hand-typed / TOML-set paths (this is the
//! friendly front door, not a replacement for validation).

use std::path::PathBuf;

use tracing::warn;
use windows::core::w;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Open the native folder chooser (modal to the current foreground window — the settings
/// window when Browse… is clicked). Returns the selected folder, or `None` if the user
/// cancelled or the dialog could not be created.
pub fn pick_folder() -> Option<PathBuf> {
    // SAFETY: the standard `IFileOpenDialog` folder-pick sequence on the settings-UI
    // thread, which `winit` has put in an STA. Every COM interface
    // (`IFileOpenDialog`/`IShellItem`) is a reference-counted RAII handle released on
    // drop; the single raw allocation — the display-name `PWSTR` from `GetDisplayName`
    // — is freed with `CoTaskMemFree`. Every fallible call is matched / `?`-guarded, so
    // any `HRESULT` failure (including a user cancel, which `Show` reports as an error)
    // returns `None` rather than panicking. No borrowed pointer outlives a call.
    unsafe {
        let dialog: IFileOpenDialog =
            match CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) {
                Ok(d) => d,
                Err(e) => {
                    warn!(error = %e, "folder picker unavailable (COM not initialised?)");
                    return None;
                }
            };
        // Restrict to real filesystem folders. A failure here is very unlikely; log it
        // (consistent with the other error paths) rather than silently showing a file picker.
        let opts = dialog.GetOptions().unwrap_or_default();
        if let Err(e) = dialog.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM) {
            warn!(error = %e, "folder picker: could not set folder-pick options");
        }
        // `Show` returns `Err` on cancel (`ERROR_CANCELLED`) or if it could not display —
        // either way there is nothing to return.
        if dialog.Show(Some(GetForegroundWindow())).is_err() {
            return None;
        }
        let item = dialog.GetResult().ok()?;
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = pwstr.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}

/// Open the native FILE chooser filtered to `.wav`, for the custom save-sound path (F7).
/// Returns the selected file, or `None` on cancel / failure. Same confined-COM contract as
/// [`pick_folder`] — a file pick this time (no `FOS_PICKFOLDERS`), with a wav filter.
pub fn pick_wav() -> Option<PathBuf> {
    // SAFETY: as [`pick_folder`] — the standard `IFileOpenDialog` sequence on the settings-UI
    // STA thread; every interface is RAII, the one `PWSTR` is `CoTaskMemFree`d, and every
    // fallible call returns `None` rather than panicking. The `COMDLG_FILTERSPEC` `w!` strings
    // are `'static` wide literals that outlive the `SetFileTypes` call.
    unsafe {
        let dialog: IFileOpenDialog =
            match CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) {
                Ok(d) => d,
                Err(e) => {
                    warn!(error = %e, "file picker unavailable (COM not initialised?)");
                    return None;
                }
            };
        let opts = dialog.GetOptions().unwrap_or_default();
        let _ = dialog.SetOptions(opts | FOS_FORCEFILESYSTEM);
        let filters = [
            COMDLG_FILTERSPEC {
                pszName: w!("Sound files (*.wav)"),
                pszSpec: w!("*.wav"),
            },
            COMDLG_FILTERSPEC {
                pszName: w!("All files (*.*)"),
                pszSpec: w!("*.*"),
            },
        ];
        let _ = dialog.SetFileTypes(&filters);
        if dialog.Show(Some(GetForegroundWindow())).is_err() {
            return None;
        }
        let item = dialog.GetResult().ok()?;
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = pwstr.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}
