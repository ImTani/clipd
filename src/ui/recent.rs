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

/// One clip file discovered in the output dir (or one of its per-app subfolders — T5).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipFile {
    /// Full path (for the open / reveal / copy actions).
    path: PathBuf,
    /// File name (the raw label; demoted to secondary by T6).
    name: String,
    /// The per-app subfolder this clip lives in (`""` = the base dir, uncategorised) —
    /// the T6 grouping key + the identity label (T5).
    app: String,
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

    /// Re-scan the CURRENT dir. Used by the Refresh button.
    pub fn rescan(&mut self) {
        self.clips = scan_clips(&self.dir, RECENT_LIMIT);
    }

    /// Re-point at `dir` (the engine's EFFECTIVE clips dir — T5) and re-scan. Called on a
    /// window re-show (the window persists hidden across opens, so clips saved meanwhile
    /// would otherwise be missing until Refresh) and whenever the effective dir changes
    /// (e.g. a live output-folder edit) so the list never reports a stale location.
    pub fn rescan_in(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.rescan();
    }

    /// Draw the list: a header with a Refresh button, then a row per clip with the
    /// three actions and the file name. `effective_dir` is the engine's live clips dir
    /// (T5): if it differs from what the list last scanned (a live output-folder edit, or
    /// a startup mismatch with the setting), re-point and re-scan so the list + empty
    /// state always name the location the user's setting actually points at.
    pub fn draw(&mut self, ui: &mut egui::Ui, effective_dir: &Path) {
        if self.dir != effective_dir {
            self.rescan_in(effective_dir.to_path_buf());
        }
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
                // A friendly relative time is the primary label (U-P2d); the raw
                // epoch-ms filename is kept as weak secondary text (and still copyable
                // via the button above / its hover tooltip).
                ui.label(relative_time(clip.modified))
                    .on_hover_text(&clip.name);
                ui.label(egui::RichText::new(&clip.name).weak());
            });
        }
    }
}

/// A friendly "N ago" label for a clip's mtime (U-P2d), reusing the status strip's pure
/// bucketing (`crate::status::format_elapsed`). A future mtime (clock skew) saturates to
/// "just now".
fn relative_time(modified: SystemTime) -> String {
    let elapsed_ms = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    crate::status::format_elapsed(elapsed_ms)
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

/// Scan `dir` (and ONE level of per-app subfolders — T5) for this app's clips and return
/// the newest `limit`. Base-dir clips are tagged `app = ""` (uncategorised / legacy); a
/// clip in `<dir>/<AppName>/` is tagged with that folder name. The only impure part —
/// `read_dir` + per-file `metadata`; filter/sort/take are pure helpers.
fn scan_clips(dir: &Path, limit: usize) -> Vec<ClipFile> {
    let mut files = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // A missing/inaccessible dir yields an empty list (logged here, not per frame —
        // the scan runs only on open / Refresh / a dir change). `warn!` since it silently
        // empties a user-facing list (matches the open/reveal failure severity).
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "recent-clips: output dir not readable");
            return files;
        }
    };
    for entry in rd.flatten() {
        match entry.file_type() {
            // A per-app subfolder (T5): scan its immediate clip files, one level only.
            Ok(ft) if ft.is_dir() => {
                let path = entry.path();
                let app = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Ok(sub) = std::fs::read_dir(&path) {
                    for sub_entry in sub.flatten() {
                        push_if_clip(&sub_entry, app, &mut files);
                    }
                }
            }
            // A clip directly in the base dir (legacy / uncategorised) → app = "".
            Ok(_) => push_if_clip(&entry, "", &mut files),
            Err(_) => {}
        }
    }
    pick_recent(files, limit)
}

/// Push `entry` onto `out` as a [`ClipFile`] tagged `app` if it is one of this app's clip
/// FILES (a directory named `clipd_*.mp4` must not masquerade as a clip). Impure
/// (`metadata`); the name filter is the pure [`is_clip_name`].
fn push_if_clip(entry: &std::fs::DirEntry, app: &str, out: &mut Vec<ClipFile>) {
    let path = entry.path();
    let Some(name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
        return;
    };
    if !is_clip_name(&name) {
        return;
    }
    let Ok(meta) = entry.metadata() else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    out.push(ClipFile {
        path,
        name,
        app: app.to_string(),
        modified,
    });
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
            app: String::new(),
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
        // A directory directly named like a clip: it is NOT a clip file, and (T5) it is
        // now descended into as a per-app folder — but it holds no clip, so nothing added.
        std::fs::create_dir(base.join("clipd_200.mp4")).expect("mkdir clip-named");
        // A non-clip file must be excluded.
        std::fs::write(base.join("notes.txt"), b"x").expect("write note");

        let got = scan_clips(&base, 20);
        assert_eq!(got.len(), 1, "only the real clip file, got {got:?}");
        assert_eq!(got[0].name, "clipd_100.mp4");
        assert_eq!(got[0].app, "", "a base-dir clip is uncategorised");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_clips_descends_one_level_of_app_folders() {
        let base = std::env::temp_dir().join(format!("clipd_t5_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Discord")).expect("app dir");
        std::fs::create_dir_all(base.join("GTA5").join("nested")).expect("nested dir");
        // A clip inside a per-app folder is found and tagged with the folder name.
        std::fs::write(base.join("Discord").join("clipd_10.mp4"), b"x").expect("clip");
        // A base-dir clip is still found (uncategorised).
        std::fs::write(base.join("clipd_20.mp4"), b"x").expect("clip");
        // A clip TWO levels deep is NOT scanned (one level only).
        std::fs::write(base.join("GTA5").join("nested").join("clipd_30.mp4"), b"x")
            .expect("deep clip");

        let got = scan_clips(&base, 20);
        let names: std::collections::HashSet<&str> = got.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains("clipd_10.mp4"),
            "app-folder clip found: {got:?}"
        );
        assert!(
            names.contains("clipd_20.mp4"),
            "base-dir clip found: {got:?}"
        );
        assert!(
            !names.contains("clipd_30.mp4"),
            "two-levels-deep clip skipped"
        );
        // The Discord clip carries its app tag.
        let discord = got.iter().find(|c| c.name == "clipd_10.mp4").unwrap();
        assert_eq!(discord.app, "Discord");

        let _ = std::fs::remove_dir_all(&base);
    }
}
