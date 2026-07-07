//! `ui::recent` — the recent-clips list (M7 Slice A / A7).
//!
//! A scannable list of the last N clips saved into the engine's output dir, each with
//! **open / reveal-in-folder / copy-path** actions. No editor, no thumbnails-with-
//! scrubbing (explicit non-goals, `M7-M8-PLAN.md` §3). Lives on the settings-window
//! thread; the engine is never touched.
//!
//! The file-selection logic — filter to this app's clips (`{PRODUCT_NAME}_*.mp4`),
//! files only, newest-first, take N — is pure and unit-tested; only the directory
//! read and the Explorer shell-outs touch the OS. The list is re-scanned on each
//! window re-show (the window persists hidden across opens) and via a Refresh button;
//! it does not live-watch the filesystem.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use tracing::{info, warn};

use crate::spec_constants::PRODUCT_NAME;

/// How many recent clips to show (`M7-M8-PLAN.md` §3: "last 20").
const RECENT_LIMIT: usize = 20;

/// One clip file discovered in the output dir.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipFile {
    /// Full path (for the open / reveal / copy actions).
    path: PathBuf,
    /// File name (the display label).
    name: String,
    /// Last-modified time — the newest-first sort key. Not shown.
    modified: SystemTime,
}

/// The recent-clips panel. Caches the last scan; re-scans on construction (window
/// open) and when the Refresh button is clicked.
pub struct RecentClips {
    /// The engine's resolved output dir (where clips actually land).
    dir: PathBuf,
    /// The cached newest-first clip list.
    clips: Vec<ClipFile>,
}

impl RecentClips {
    /// Build for `dir` (the engine's resolved output dir) and do the first scan.
    pub fn new(dir: PathBuf) -> Self {
        let clips = scan_clips(&dir, RECENT_LIMIT);
        Self { dir, clips }
    }

    /// Re-scan the output dir. Called on each window re-show (the window persists
    /// hidden across opens, so clips saved while it was hidden would otherwise be
    /// missing until Refresh) and from the Refresh button.
    pub fn rescan(&mut self) {
        self.clips = scan_clips(&self.dir, RECENT_LIMIT);
    }

    /// Draw the list: a header with a Refresh button, then a row per clip with the
    /// three actions and the file name.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Recent clips");
            if ui.button("Refresh").clicked() {
                self.rescan();
            }
        });
        ui.add_space(4.0);

        if self.clips.is_empty() {
            ui.label(format!("No clips yet in {}", self.dir.display()));
            return;
        }

        for clip in &self.clips {
            ui.horizontal(|ui| {
                if ui.small_button("Open").clicked() {
                    open_path(&clip.path);
                }
                if ui.small_button("Folder").clicked() {
                    reveal_path(&clip.path);
                }
                if ui.small_button("Copy path").clicked() {
                    ui.ctx().copy_text(clip.path.display().to_string());
                }
                ui.monospace(&clip.name);
            });
        }
    }
}

/// Whether `name` is one of this app's clip files (`{PRODUCT_NAME}_*.mp4` — covers
/// both buffer saves `<product>_<ms>.mp4` and recordings `<product>_rec_<ms>.mp4`).
/// Pure.
fn is_clip_name(name: &str) -> bool {
    let mut prefix = String::from(PRODUCT_NAME);
    prefix.push('_');
    name.starts_with(&prefix) && name.to_ascii_lowercase().ends_with(".mp4")
}

/// Pick the most-recent `limit` clips: sort newest-first by mtime, take `limit`. Pure
/// over the scanned files, so the selection is unit-tested without a filesystem.
fn pick_recent(mut files: Vec<ClipFile>, limit: usize) -> Vec<ClipFile> {
    // Newest first; ties keep discovery order (`sort_by_key` is stable).
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files.truncate(limit);
    files
}

/// Scan `dir` for this app's clips and return the newest `limit`. The only impure
/// part — `read_dir` + per-file `metadata`; filter/sort/take are pure helpers.
fn scan_clips(dir: &Path, limit: usize) -> Vec<ClipFile> {
    let mut files = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                    continue;
                };
                if !is_clip_name(&name) {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                // Files only — a directory (or symlink to one) named `clipd_*.mp4`
                // must not masquerade as a clip.
                if !meta.is_file() {
                    continue;
                }
                let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                files.push(ClipFile {
                    path,
                    name,
                    modified,
                });
            }
        }
        // A missing/inaccessible dir yields an empty list (logged here, not per frame —
        // the scan runs only on open / Refresh). `warn!` since it silently empties a
        // user-facing list (matches the open/reveal failure severity).
        Err(e) => warn!(dir = %dir.display(), error = %e, "recent-clips: output dir not readable"),
    }
    pick_recent(files, limit)
}

/// Open a clip with its default handler (Explorer's shell-open of a file path).
/// `explorer` returns a non-zero exit even on success, so we only check the spawn.
fn open_path(path: &Path) {
    match std::process::Command::new("explorer").arg(path).spawn() {
        Ok(_) => info!(path = %path.display(), "opened clip"),
        Err(e) => warn!(path = %path.display(), error = %e, "could not open clip"),
    }
}

/// Reveal a clip in Explorer with the file selected (`/select,<path>`).
fn reveal_path(path: &Path) {
    let arg = format!("/select,{}", path.display());
    match std::process::Command::new("explorer").arg(arg).spawn() {
        Ok(_) => info!(path = %path.display(), "revealed clip in folder"),
        Err(e) => warn!(path = %path.display(), error = %e, "could not reveal clip"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn clip(name: &str, secs: u64) -> ClipFile {
        ClipFile {
            path: PathBuf::from(name),
            name: name.to_string(),
            modified: UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    #[test]
    fn is_clip_name_matches_only_this_apps_mp4s() {
        assert!(is_clip_name("clipd_1700000000000.mp4"));
        assert!(is_clip_name("clipd_rec_1700000000000.mp4"));
        assert!(is_clip_name("clipd_1700.MP4")); // extension is case-insensitive
        assert!(!is_clip_name("clipd_1700.mkv")); // wrong extension
        assert!(!is_clip_name("someothervideo.mp4")); // not our prefix
        assert!(!is_clip_name("clipd.mp4")); // no `_` separator after the product name
    }

    #[test]
    fn pick_recent_sorts_newest_first_and_truncates() {
        let files = vec![
            clip("clipd_a.mp4", 10),
            clip("clipd_b.mp4", 30),
            clip("clipd_c.mp4", 20),
        ];
        let got = pick_recent(files, 2);
        assert_eq!(
            got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["clipd_b.mp4", "clipd_c.mp4"] // newest (30) then (20); (10) dropped
        );
    }

    #[test]
    fn pick_recent_handles_zero_and_over_limit() {
        assert!(pick_recent(vec![clip("clipd_a.mp4", 1)], 0).is_empty());
        let two = vec![clip("clipd_a.mp4", 1), clip("clipd_b.mp4", 2)];
        assert_eq!(pick_recent(two, 20).len(), 2); // limit > count keeps all
    }

    #[test]
    fn scan_clips_lists_files_only_not_dirs() {
        let base = std::env::temp_dir().join(format!("clipd_a7_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp dir");
        std::fs::write(base.join("clipd_100.mp4"), b"x").expect("write clip");
        // A directory named like a clip must be excluded.
        std::fs::create_dir(base.join("clipd_200.mp4")).expect("mkdir clip-named");
        // A non-clip file must be excluded.
        std::fs::write(base.join("notes.txt"), b"x").expect("write note");

        let got = scan_clips(&base, 20);
        assert_eq!(got.len(), 1, "only the real clip file, got {got:?}");
        assert_eq!(got[0].name, "clipd_100.mp4");

        let _ = std::fs::remove_dir_all(&base);
    }
}
