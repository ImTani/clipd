//! `ui::sound` — the optional save-confirmation sound (P1b, pulled forward from M10).
//!
//! Win11's "when playing a game" DND auto-rule suppresses the save toast during the core
//! use case (DECISIONS 2026-07-09 toast-test matrix), and it also covers exclusive
//! fullscreen where no window can draw. Audio is the only in-the-moment confirmation channel
//! Windows does not gate — so a short, quiet tone plays on a successful save.
//!
//! Scope (exactly M10's): one toggle (`config.feedback.save_sound`, default on), one bundled
//! `.wav` (replaceable by `config.feedback.save_sound_path`), played on **success only**.
//! Playback is fire-and-forget on a detached thread via `PlaySoundW`, so nothing on the
//! caller (tray) side blocks on audio. `unsafe` is the single confined `winmm` FFI call.
//!
//! **LIMITATIONS:** the sound plays out the default render device, so it is captured into the
//! desktop-audio track of subsequently-buffered footage. The bundled tone is short + quiet by
//! design to keep that mark negligible.

use tracing::warn;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_FILENAME, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

/// The bundled default confirmation tone — a short, quiet rising two-note blip
/// (48 kHz mono 16-bit PCM, ~160 ms). Embedded so the single binary has no external asset.
const BUNDLED_WAV: &[u8] = include_bytes!("../../assets/save.wav");

/// Play the save-confirmation sound: the file at `custom_path` if it is a non-empty,
/// existing path, else the [`BUNDLED_WAV`]. Fire-and-forget — spawns a detached thread and
/// returns immediately, so the caller (the tray's save-outcome handler) never blocks on
/// audio. Call only for a SUCCESSFUL save, and only when `config.feedback.save_sound` is on.
pub fn play_save(custom_path: &str) {
    // Resolve on the caller side (cheap), then hand an owned choice to the thread.
    let custom = {
        let p = custom_path.trim();
        (!p.is_empty() && std::path::Path::new(p).is_file()).then(|| p.to_string())
    };
    // A detached thread: even a slow custom-file load happens off the tray thread. The
    // thread lives only as long as the ~160 ms blip (SND_SYNC), then exits.
    let spawned = std::thread::Builder::new()
        .name("save-sound".to_string())
        .spawn(move || match custom {
            Some(path) => play_file(&path),
            None => play_bundled(),
        });
    if let Err(e) = spawned {
        warn!(error = %e, "could not spawn the save-sound thread");
    }
}

/// Play a custom `.wav` from disk (blocking this thread until it finishes). `SND_NODEFAULT`
/// so a bad/decodable-less file is silent rather than firing the system default ding.
fn play_file(path: &str) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `PlaySoundW` reads a NUL-terminated wide string that outlives the (blocking,
    // SND_SYNC) call; `hmod` is null; no pointer escapes. A `false` return is logged.
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(wide.as_ptr()),
            None,
            SND_FILENAME | SND_SYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        warn!(path, "PlaySoundW failed for the custom save sound");
    }
}

/// Play the embedded default tone from memory (blocking this thread until it finishes).
fn play_bundled() {
    // SAFETY: `SND_MEMORY` reads the in-memory WAV image at `pszSound`; `BUNDLED_WAV` is a
    // `'static` byte slice that outlives the (blocking) call. `hmod` is null.
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(BUNDLED_WAV.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_SYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        warn!("PlaySoundW failed for the bundled save sound");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_wav_is_a_riff_wave() {
        // Sanity: the embedded asset is a real RIFF/WAVE container (so PlaySound can play it).
        assert!(
            BUNDLED_WAV.len() > 44,
            "wav too small: {}",
            BUNDLED_WAV.len()
        );
        assert_eq!(&BUNDLED_WAV[0..4], b"RIFF");
        assert_eq!(&BUNDLED_WAV[8..12], b"WAVE");
    }
}
