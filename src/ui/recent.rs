//! `ui::recent` — the recent-clips list (M7 Slice A / A7).
//!
//! A scannable list of the last N clips saved into the engine's output dir, **grouped by
//! the per-app folder** (T5/T6). Each row is identity-first (relative time · duration ·
//! size, filename in the tooltip) and opens the clip on click; reveal-in-folder + copy-
//! path live in a per-row `⋯` menu. No editor, no thumbnails (explicit non-goals,
//! `M7-M8-PLAN.md` §3). Lives on the settings-window thread; the engine is never touched.
//!
//! The file-selection logic — filter to this app's clips (`{PRODUCT_NAME}_*.mp4`),
//! files only, newest-first, take N — is pure and unit-tested; only the directory
//! read and the Explorer shell-outs touch the OS. The list is re-scanned on each
//! window re-show (the window persists hidden across opens) and via a Refresh button;
//! it does not live-watch the filesystem.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use tracing::{info, warn};

use super::theme;
use crate::spec_constants::PRODUCT_NAME;

/// How many recent clips to show (`M7-M8-PLAN.md` §3: "last 20").
const RECENT_LIMIT: usize = 20;

/// One clip file discovered in the output dir (or one of its per-app subfolders — T5).
#[derive(Debug, Clone, PartialEq)]
struct ClipFile {
    /// Full path (for the open / reveal / copy actions).
    path: PathBuf,
    /// File name (demoted to the row tooltip / secondary by T6).
    name: String,
    /// The per-app subfolder this clip lives in (`""` = the base dir, uncategorised) —
    /// the T6 grouping key (T5).
    app: String,
    /// File size in bytes (T6 identity column).
    size: u64,
    /// Playable duration in seconds, read from the MP4 `mvhd` at scan time (T6). `None`
    /// when it can't be parsed (an un-finalised/partial file) → shown as "—".
    duration_secs: Option<f32>,
    /// Last-modified time — the newest-first sort key + the relative-time label.
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

    /// Draw the list: a header with a Refresh button, then the clips grouped by app folder
    /// (newest group first — T6), each an identity-first clickable row. `effective_dir` is
    /// the engine's live clips dir (T5): if it differs from what the list last scanned (a
    /// live output-folder edit, or a startup mismatch with the setting), re-point and
    /// re-scan so the list + empty state always name the location the setting points at.
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

        // Grouped by app folder, newest group first (T6). Within a group, newest first.
        for (app, clips) in group_by_app(&self.clips) {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(app).strong());
            for clip in clips {
                draw_clip_row(ui, clip);
            }
            ui.add_space(4.0);
        }
    }
}

/// Draw one clip row (T6): identity-first — relative time · duration · size — with the
/// raw filename demoted to the hover tooltip. The whole row is clickable to OPEN the clip
/// (the primary action); Show-in-folder + Copy-path live in a per-row overflow (`⋮`) menu
/// (replacing the old three-button strip).
///
/// P3 hardening: the overflow trigger is a PAINTED three-dot glyph, not a font character —
/// the Unicode `⋯`/`⋮` are absent from egui's bundled atlas and rendered as a blank box on
/// the test machine. The row also gets a hover highlight + a pointing-hand cursor so the
/// click-to-open affordance is discoverable (it looked inert before).
fn draw_clip_row(ui: &mut egui::Ui, clip: &ClipFile) {
    let row_h = ui.spacing().interact_size.y;

    // Reserve a shape slot for the row's hover background FIRST. Shapes render in insertion
    // order, so this placeholder — filled in below once we know the row rect + hover state —
    // sits behind everything the row draws afterwards. (egui's canonical "background" idiom.)
    let bg_idx = ui.painter().add(egui::Shape::Noop);

    let egui::InnerResponse {
        inner: id_resp,
        response: row_resp,
    } = ui.horizontal(|ui| {
        ui.set_min_height(row_h);
        // Right-to-left so the overflow button pins to the right edge and the identity label
        // takes the remaining width.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let dots = overflow_button(ui, row_h);
            egui::Popup::menu(&dots).show(|ui| {
                if ui.button("Show in folder").clicked() {
                    reveal_path(&clip.path);
                    ui.close();
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(clip.path.display().to_string());
                    ui.close();
                }
            });

            // The clickable identity, filling the rest of the row (left-aligned).
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let identity = format!(
                    "{}   ·   {}   ·   {}",
                    relative_time(clip.modified),
                    format_duration(clip.duration_secs),
                    format_size(clip.size),
                );
                let w = ui.available_width();
                ui.add_sized(
                    [w, row_h],
                    egui::Label::new(identity)
                        .selectable(false)
                        .sense(egui::Sense::click()),
                )
                .on_hover_text(format!("{}\n(click to open)", clip.name))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
            })
            .inner
        })
        .inner
    });

    // Row-wide hover highlight (P3 discoverability): fill the reserved slot whenever the
    // pointer is anywhere over the row — signalling the whole row is a click target.
    if row_resp.contains_pointer() {
        let fill = ui.visuals().widgets.hovered.weak_bg_fill;
        ui.painter().set(
            bg_idx,
            egui::Shape::rect_filled(row_resp.rect.expand2(egui::vec2(2.0, 1.0)), 4.0, fill),
        );
    }

    // Open on click / double-click of the identity.
    if id_resp.clicked() || id_resp.double_clicked() {
        open_path(&clip.path);
    }
}

/// A painter-drawn "kebab" (`⋮`, three vertical dots) overflow-menu trigger. egui's bundled
/// font atlas ships no `⋯`/`⋮` glyph — it renders as an empty box (P3) — so the dots are
/// stroked directly. Returns the click response for callers to hang an [`egui::Popup::menu`]
/// on. Highlights (subtle fill + accent dots) on hover, with a pointing-hand cursor.
fn overflow_button(ui: &mut egui::Ui, row_h: f32) -> egui::Response {
    let size = egui::vec2(22.0, row_h);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let active = resp.hovered() || resp.is_pointer_button_down_on();
    if active {
        let fill = ui.visuals().widgets.hovered.weak_bg_fill;
        ui.painter().rect_filled(rect, 4.0, fill);
    }
    let color = if active {
        theme::ACCENT_HOVER
    } else {
        ui.visuals().weak_text_color()
    };
    let c = rect.center();
    let dot_r = 1.7;
    let gap = 4.5;
    for dy in [-gap, 0.0, gap] {
        ui.painter()
            .circle_filled(egui::pos2(c.x, c.y + dy), dot_r, color);
    }
    resp
}

/// Group clips by their app folder, preserving the newest-first order the input carries:
/// each app's group appears at the position of its NEWEST clip (so groups are ordered
/// newest-group-first), and clips within a group stay newest-first. `""` (uncategorised /
/// legacy) is shown under the [`crate::appfolder::FALLBACK_FOLDER`] heading. Pure + tested.
fn group_by_app(clips: &[ClipFile]) -> Vec<(&str, Vec<&ClipFile>)> {
    let mut groups: Vec<(&str, Vec<&ClipFile>)> = Vec::new();
    for c in clips {
        let app = if c.app.is_empty() {
            crate::appfolder::FALLBACK_FOLDER
        } else {
            c.app.as_str()
        };
        match groups.iter_mut().find(|(a, _)| *a == app) {
            Some((_, v)) => v.push(c),
            None => groups.push((app, vec![c])),
        }
    }
    groups
}

/// Format a clip duration as `M:SS` (`—` when unknown / un-finalised). Pure.
fn format_duration(secs: Option<f32>) -> String {
    match secs {
        Some(s) if s >= 0.0 => {
            let total = s.round() as u64;
            format!("{}:{:02}", total / 60, total % 60)
        }
        _ => "—".to_string(),
    }
}

/// Format a byte count as a compact human size (`KB`/`MB`/`GB`, 1 decimal). Pure.
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
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
    // Sort + truncate on the CHEAP metadata (mtime) first, THEN read the MP4 `mvhd`
    // duration for the surviving ≤ `limit` clips only. This bounds the per-open file I/O to
    // what's actually shown, not to how many clips have ever accumulated (R-M3). Best-effort:
    // a partial / un-finalised file yields `None` → the row shows "—".
    let mut recent = pick_recent(files, limit);
    for clip in &mut recent {
        clip.duration_secs = read_mp4_duration_secs(&clip.path);
    }
    recent
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
    // Duration is deliberately NOT read here — that MP4 `mvhd` parse (a file open + seeks)
    // would run for EVERY clip on disk, before we truncate to the newest `limit`. It is
    // filled in `scan_clips` for the survivors only (R-M3, DECISIONS 2026-07-10).
    out.push(ClipFile {
        path,
        name,
        app: app.to_string(),
        size: meta.len(),
        duration_secs: None,
        modified,
    });
}

/// Read the playable duration (seconds) from an MP4's movie header (`moov` → `mvhd`),
/// best-effort (T6). Walks the top-level boxes to locate `moov` (handling the 64-bit
/// `largesize` `mdat` the B5 muxer writes), reads a bounded prefix of it, finds `mvhd`,
/// and computes `duration / timescale`. Returns `None` on any parse issue or an
/// un-finalised file (a fragmented `moov` has a zero `mvhd` duration) → the row shows "—".
/// A handful of small reads + seeks; no full-file read.
fn read_mp4_duration_secs(path: &Path) -> Option<f32> {
    let mut f = File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();
    let (moov_off, moov_len) = find_top_level_box(&mut f, file_len, b"moov")?;

    // Read a bounded prefix of `moov` — `mvhd` is its first child, well within a few KiB.
    let read_len = moov_len.min(8192) as usize;
    let mut buf = vec![0u8; read_len];
    f.seek(SeekFrom::Start(moov_off)).ok()?;
    f.read_exact(&mut buf).ok()?;

    // Find `mvhd` and parse timescale + duration (v0 = 32-bit, v1 = 64-bit fields).
    let m = find_subslice(&buf, b"mvhd")?;
    let version = *buf.get(m + 4)?; // the byte after the "mvhd" fourcc
    let (timescale, duration) = if version == 1 {
        // version(1)+flags(3)+ctime(8)+mtime(8)+timescale(4)+duration(8)
        let ts = read_u32(&buf, m + 4 + 4 + 8 + 8)?;
        let du = read_u64(&buf, m + 4 + 4 + 8 + 8 + 4)?;
        (ts, du)
    } else {
        // version(1)+flags(3)+ctime(4)+mtime(4)+timescale(4)+duration(4)
        let ts = read_u32(&buf, m + 4 + 4 + 4 + 4)?;
        let du = read_u32(&buf, m + 4 + 4 + 4 + 4 + 4)? as u64;
        (ts, du)
    };
    if timescale == 0 || duration == 0 {
        return None;
    }
    Some(duration as f32 / timescale as f32)
}

/// Locate a top-level box by its 4-byte `fourcc`, returning `(payload_offset, payload_len)`.
/// Walks `size`/`type` headers from the start, handling `size == 1` (64-bit `largesize`)
/// and `size == 0` (extends to EOF). `None` if not found or the boxes are malformed.
fn find_top_level_box(f: &mut File, file_len: u64, fourcc: &[u8; 4]) -> Option<(u64, u64)> {
    let mut pos = 0u64;
    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr).ok()?;
        let size32 = u32::from_be_bytes(hdr[0..4].try_into().ok()?);
        let typ = &hdr[4..8];
        let (box_size, header_len) = match size32 {
            1 => {
                let mut ext = [0u8; 8];
                f.read_exact(&mut ext).ok()?;
                (u64::from_be_bytes(ext), 16u64)
            }
            0 => (file_len - pos, 8u64), // extends to EOF
            n => (n as u64, 8u64),
        };
        if typ == fourcc {
            return Some((pos + header_len, box_size.saturating_sub(header_len)));
        }
        if box_size < header_len {
            return None; // malformed — avoid a zero/negative advance
        }
        // Advance with a checked add: a corrupt/garbage 64-bit `largesize` near u64::MAX
        // must not overflow `pos` (which panics under dev/test overflow-checks and wraps in
        // release) — a malformed clip yields `None`, never a panic.
        match pos.checked_add(box_size) {
            Some(next) if next <= file_len => pos = next,
            _ => return None,
        }
    }
    None
}

/// The index of the first occurrence of `needle` in `hay`, or `None`. Small linear scan.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read a big-endian u32 at `off`, or `None` if out of bounds.
fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

/// Read a big-endian u64 at `off`, or `None` if out of bounds.
fn read_u64(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)
        .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
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
///
/// `explorer.exe` uses a NON-standard command-line parser: the correct form is
/// `/select,` unquoted with only the PATH quoted (`explorer /select,"C:\a b\f.mp4"`).
/// `Command::arg` would instead wrap the whole `/select,<path>` token in quotes as soon as
/// the path contains a space (e.g. a T5 per-app folder like `Antigravity IDE`), which
/// explorer mis-parses — it drops the selection and opens its default location (Documents)
/// instead. So we build the command line verbatim with `raw_arg`, quoting only the path.
fn reveal_path(path: &Path) {
    use std::os::windows::process::CommandExt;
    let arg = format!("/select,\"{}\"", path.display());
    match std::process::Command::new("explorer").raw_arg(arg).spawn() {
        Ok(_) => info!(path = %path.display(), "revealed clip in folder"),
        Err(e) => warn!(path = %path.display(), error = %e, "could not reveal clip"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn clip(name: &str, secs: u64) -> ClipFile {
        clip_app(name, secs, "")
    }

    fn clip_app(name: &str, secs: u64, app: &str) -> ClipFile {
        ClipFile {
            path: PathBuf::from(name),
            name: name.to_string(),
            app: app.to_string(),
            size: 0,
            duration_secs: None,
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
    fn group_by_app_orders_newest_group_first_and_maps_empty_to_other() {
        // Newest-first input (as pick_recent produces): Mix-in of two apps + an
        // uncategorised clip. Groups appear at their newest clip's position.
        let clips = vec![
            clip_app("clipd_5.mp4", 50, "Discord"), // newest overall
            clip_app("clipd_4.mp4", 40, "GTA5"),
            clip_app("clipd_3.mp4", 30, "Discord"),
            clip("clipd_2.mp4", 20), // uncategorised → "Other"
        ];
        let groups = group_by_app(&clips);
        let names: Vec<&str> = groups.iter().map(|(a, _)| *a).collect();
        assert_eq!(names, vec!["Discord", "GTA5", "Other"]);
        // Discord holds its two clips, newest-first.
        let discord = &groups[0].1;
        assert_eq!(discord.len(), 2);
        assert_eq!(discord[0].name, "clipd_5.mp4");
        assert_eq!(discord[1].name, "clipd_3.mp4");
    }

    #[test]
    fn format_duration_reads_mmss_or_dash() {
        assert_eq!(format_duration(None), "—");
        assert_eq!(format_duration(Some(0.0)), "0:00");
        assert_eq!(format_duration(Some(5.4)), "0:05");
        assert_eq!(format_duration(Some(65.0)), "1:05");
        assert_eq!(format_duration(Some(605.0)), "10:05");
        // A negative (nonsense) duration reads as unknown.
        assert_eq!(format_duration(Some(-1.0)), "—");
    }

    #[test]
    fn format_size_is_compact() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn read_mp4_duration_parses_a_minimal_moov() {
        // A minimal `ftyp` + `moov`/`mvhd` (v0) file: timescale 1000, duration 2500 →
        // 2.5 s. No `mdat` needed — the box walker just needs to reach `moov`.
        let base = std::env::temp_dir().join(format!("clipd_t6_dur_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("dir");
        let path = base.join("clipd_dur.mp4");

        let mut file = Vec::new();
        // ftyp box (size 16): 'ftyp' + 'isom' + minor version.
        file.extend_from_slice(&16u32.to_be_bytes());
        file.extend_from_slice(b"ftyp");
        file.extend_from_slice(b"isom");
        file.extend_from_slice(&0u32.to_be_bytes());
        // mvhd payload (v0): version+flags(4) + ctime(4)+mtime(4)+timescale(4)+duration(4).
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&(8 + 24u32).to_be_bytes()); // box size = header + fields we write
        mvhd.extend_from_slice(b"mvhd");
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // version 0 + flags
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // ctime
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // mtime
        mvhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mvhd.extend_from_slice(&2500u32.to_be_bytes()); // duration
                                                        // moov box wrapping the mvhd.
        let moov_size = 8 + mvhd.len();
        file.extend_from_slice(&(moov_size as u32).to_be_bytes());
        file.extend_from_slice(b"moov");
        file.extend_from_slice(&mvhd);
        std::fs::write(&path, &file).expect("write mp4");

        let d = read_mp4_duration_secs(&path).expect("duration parsed");
        assert!((d - 2.5).abs() < 1e-3, "duration = {d}");

        // A non-MP4 file yields None, not a panic.
        let junk = base.join("clipd_junk.mp4");
        std::fs::write(&junk, b"not an mp4 at all").expect("write junk");
        assert_eq!(read_mp4_duration_secs(&junk), None);

        // A corrupt 64-bit `largesize` (u64::MAX) must NOT overflow the box walk — it
        // yields None, never a panic (dev/test overflow-checks would otherwise trip).
        let mut bad = Vec::new();
        bad.extend_from_slice(&16u32.to_be_bytes()); // ftyp box
        bad.extend_from_slice(b"ftyp");
        bad.extend_from_slice(b"isom");
        bad.extend_from_slice(&0u32.to_be_bytes());
        bad.extend_from_slice(&1u32.to_be_bytes()); // size == 1 → 64-bit largesize follows
        bad.extend_from_slice(b"mdat");
        bad.extend_from_slice(&u64::MAX.to_be_bytes()); // garbage largesize
        let bad_path = base.join("clipd_bad.mp4");
        std::fs::write(&bad_path, &bad).expect("write bad mp4");
        assert_eq!(read_mp4_duration_secs(&bad_path), None);

        let _ = std::fs::remove_dir_all(&base);
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
