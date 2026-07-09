//! Versioned TOML configuration (schema v2) with the `--check-config` load path
//! and the format-preserving rewrite path (M7 Slice A / A1).
//!
//! `01-PROJECT-PLAN.md §3` pitfall 30 and `§6`: *"Versioned schema
//! (`config_version`), unknown keys preserved on rewrite, file only rewritten on
//! explicit user change, and a `--check-config` flag that validates and prints
//! the effective config. Silent resets are the #1 incumbent trust-killer."*
//!
//! This module is pure logic (100% safe, unit-testable). **Reads** go through
//! `toml`/serde into the single typed [`Config`], which is then migrated forward
//! ([`Config::migrate`], v1→v2 in memory) and validated. **Writes** go through
//! [`Config::write_atomic`], which uses `toml_edit` to overlay the config onto
//! the on-disk document — overwriting only known keys, so user comments and
//! unknown/forward-compat keys survive (pitfall 30). `toml_edit` is only the
//! preserving serializer, not a second schema representation; the UI and any
//! editor write config exclusively through this path (same typed schema as
//! `--check-config`). See `DECISIONS.md` 2026-07-07 "A1".

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::spec_constants::{
    audio::{
        aac::{BITRATE_DEFAULT_BPS, BITRATE_MAX_BPS, BITRATE_MIN_BPS},
        SAMPLE_RATE_HZ,
    },
    encoder::{QUALITY_MULT_DEFAULT, QUALITY_MULT_EFFICIENT, QUALITY_MULT_HIGH, QUALITY_MULT_MAX},
    ring::{DEFAULT_BUFFER_SECONDS, MAX_BUFFER_SECONDS},
    video::{
        DEFAULT_FPS, DEFAULT_MAX_ENCODE_HEIGHT, MAX_ENCODE_HEIGHT_MAX, MAX_ENCODE_HEIGHT_MIN,
        RESOLUTION_TIER_1080, RESOLUTION_TIER_1440, RESOLUTION_TIER_720, SUPPORTED_FPS,
    },
    CONFIG_VERSION, MIN_SUPPORTED_CONFIG_VERSION, PRODUCT_NAME,
};

/// Errors from loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("reading config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The config file is not valid TOML for the schema.
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    /// The config's `config_version` is not one this build understands.
    #[error("unsupported config_version {found} (this build expects {expected})")]
    UnsupportedVersion { found: u32, expected: u32 },
    /// A value is out of the range the spec permits.
    #[error("invalid config: {0}")]
    Invalid(String),
    /// Serializing the effective config back to TOML failed (should not happen).
    #[error("serializing config: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// The existing file could not be parsed as an editable TOML document during
    /// the format-preserving rewrite.
    #[error("parsing config for rewrite: {0}")]
    Edit(#[from] toml_edit::TomlError),
}

/// Where to capture from. `01-PROJECT-PLAN.md §3` pitfall 31:
/// `monitor = "primary" | index | "focused-window"` — *never guess*.
///
/// Serialized untagged, so the TOML is either a string (`target = "primary"`)
/// or an integer monitor index (`target = 2`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum CaptureTarget {
    /// A named target: the primary monitor, or the focused window.
    Named(NamedTarget),
    /// A specific monitor by zero-based index.
    Monitor(u32),
}

/// The string-named capture targets.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NamedTarget {
    /// The primary monitor.
    Primary,
    /// The currently focused window (borderless/windowed; `§4` of the plan).
    FocusedWindow,
}

/// Video codec. `02-AV-SYNC-SPEC.md §6.1`: default h264 (universal edit/share
/// compatibility beats the HEVC size win).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// H.264 / AVC. Default.
    #[default]
    H264,
    /// H.265 / HEVC.
    Hevc,
}

/// `[capture]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct CaptureConfig {
    /// Capture target. Default: the primary monitor.
    pub target: CaptureTarget,
    /// Output frame rate. `§1.2` (30/60/120; 120 gated behind Milestone 6).
    pub fps: u32,
    /// Whether to composite the cursor. `01-PROJECT-PLAN.md §3` pitfall 10.
    pub cursor: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::Named(NamedTarget::Primary),
            fps: DEFAULT_FPS,
            cursor: true,
        }
    }
}

/// `encode.quality` — the named quality tier (schema v2 / A1, M7-M8-PLAN §3).
///
/// Maps to a **bitrate multiplier** over the T0-calibrated target, NOT a CQ
/// value: the T0 probe proved constant-QP is unreachable through Media
/// Foundation on the NVENC MFT (DECISIONS 2026-07-07), so the user-facing knob
/// scales `encoder::video_target_bitrate_bps` instead. `Default` reproduces the
/// T0 baseline (1080p60 = 16 Mbps) exactly.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Smaller files (0.6× the `Default` target ≈ 9.6 Mbps @ 1080p60).
    Efficient,
    /// The calibrated baseline (1.0×). Default.
    #[default]
    Default,
    /// Higher quality (1.5× ≈ 24 Mbps @ 1080p60).
    High,
    /// Maximum quality (2.0× ≈ 32 Mbps @ 1080p60).
    Max,
}

impl Quality {
    /// The bitrate multiplier for this tier — feeds
    /// `encoder::video_target_bitrate_bps` and, so the byte cap tracks it,
    /// `ring::est_bitrate_bps`.
    pub fn multiplier(&self) -> f64 {
        match self {
            Quality::Efficient => QUALITY_MULT_EFFICIENT,
            Quality::Default => QUALITY_MULT_DEFAULT,
            Quality::High => QUALITY_MULT_HIGH,
            Quality::Max => QUALITY_MULT_MAX,
        }
    }
}

/// `encode.resolution` — the named output-resolution tier (schema v2 / A1). The
/// canvas is the capture monitor's resolution scaled to fit within the resulting
/// height. `native` preserves the historical default cap (no downscale below
/// 2160; decision 2026-07-07); the lower tiers downscale via the existing
/// VideoProcessor canvas. Subsumes the v1 raw `max_height`, which survives as an
/// advanced override (see [`EncodeConfig::max_height`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resolution {
    /// Encode at the source resolution (capped at the historical 2160 default).
    #[default]
    #[serde(rename = "native")]
    Native,
    /// Downscale to 1440p.
    #[serde(rename = "1440")]
    P1440,
    /// Downscale to 1080p.
    #[serde(rename = "1080")]
    P1080,
    /// Downscale to 720p.
    #[serde(rename = "720")]
    P720,
}

impl Resolution {
    /// The encode-height ceiling this tier maps to. `native` →
    /// [`DEFAULT_MAX_ENCODE_HEIGHT`]; the others are the explicit downscale caps.
    pub fn to_max_height(&self) -> u32 {
        match self {
            Resolution::Native => DEFAULT_MAX_ENCODE_HEIGHT,
            Resolution::P1440 => RESOLUTION_TIER_1440,
            Resolution::P1080 => RESOLUTION_TIER_1080,
            Resolution::P720 => RESOLUTION_TIER_720,
        }
    }
}

/// `[encode]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct EncodeConfig {
    /// Video codec. Default h264.
    pub codec: Codec,
    /// Named quality tier (schema v2 / A1). Drives the bitrate multiplier via
    /// [`Quality::multiplier`]. Default [`Quality::Default`].
    pub quality: Quality,
    /// Named output-resolution tier (schema v2 / A1). Default [`Resolution::Native`].
    pub resolution: Resolution,
    /// Advanced encode-height override for the fixed output canvas (M4-2 /
    /// pitfall 11), TOML-only escape hatch. When `Some`, it wins over
    /// [`Self::resolution`]; `None` (default) uses the resolution tier. The v1
    /// `max_height` integer migrates into this field losslessly. The canvas is the
    /// capture monitor's resolution scaled to fit within the effective height,
    /// evened; a resized window is letterboxed so a clip spans resizes at one res.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
}

impl EncodeConfig {
    /// The effective encode-height ceiling: the advanced [`Self::max_height`]
    /// override if set, else the [`Self::resolution`] tier's height. This is the
    /// single value the capture canvas is built from.
    pub fn effective_max_height(&self) -> u32 {
        self.max_height
            .unwrap_or_else(|| self.resolution.to_max_height())
    }
}

/// `[audio.tracks]` — the Slice-B (M8′) multi-track topology toggles.
///
/// Since B1 these gate the **planned** track set (`planned_kinds`) when
/// `[audio].separate_tracks` is on; the per-source tracks are actually *captured*
/// from B2 (process-loopback) / B4 (mixer). The `mix` track (always on) and the mic
/// track (`[audio].mic`) are not toggles here; these three are the optional per-source
/// tracks emitted only when `separate_tracks` is set. Defaults per M7-M8-PLAN §2 (all on).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct AudioTracks {
    /// Emit the game-audio track (Slice B). Default on.
    pub game: bool,
    /// Emit the voice-chat track (Slice B, process-detected per [`AudioConfig::vc_apps`]).
    /// Default on.
    pub voice_chat: bool,
    /// Emit the "other system" track (Slice B). Default on.
    pub other_system: bool,
}

impl Default for AudioTracks {
    fn default() -> Self {
        Self {
            game: true,
            voice_chat: true,
            other_system: true,
        }
    }
}

/// One `[[audio.vc_apps]]` entry — a voice-chat app the Slice-B detector scans
/// for. Detected by **process image name, never by window** (a tray-minimized
/// Discord has no window; M7-M8-PLAN §2 / §5). Ships as TOML **data**, not code.
///
/// **Schema v2 / A1: parsed, validated, round-tripped; the scanner consumes it in
/// Slice B / M8′.** A1 seeds only the P0 default (Discord family); the full
/// P1/P2 table (Vesktop/Legcord/TeamSpeak/Mumble/Steam/Game Bar) lands with the
/// scanner in Slice B.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct VcApp {
    /// Friendly name (e.g. `"Discord"`), for logs and the settings UI.
    pub name: String,
    /// Process image names to match (e.g. `["Discord.exe", "DiscordPTB.exe"]`).
    pub process_names: Vec<String>,
    /// Capture the whole process tree (Electron/helper children carry the audio)
    /// rather than just the matched PID. Discord needs this.
    pub include_tree: bool,
    /// Whether this entry is active in the scan.
    pub enabled: bool,
}

impl Default for VcApp {
    fn default() -> Self {
        Self {
            name: String::new(),
            process_names: Vec::new(),
            include_tree: true,
            enabled: true,
        }
    }
}

impl VcApp {
    /// The P0 default: the Discord family (stable, PTB, Canary), include-tree so
    /// the audio in the Electron child process is captured (M7-M8-PLAN §2).
    fn discord_default() -> Self {
        Self {
            name: "Discord".to_string(),
            process_names: vec![
                "Discord.exe".to_string(),
                "DiscordPTB.exe".to_string(),
                "DiscordCanary.exe".to_string(),
            ],
            include_tree: true,
            enabled: true,
        }
    }
}

/// `[audio]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct AudioConfig {
    /// Capture desktop loopback. `§2.5` track 1.
    pub desktop: bool,
    /// Mic device policy: `"default-follow"` (chase the Windows default),
    /// `"off"`, or a pinned endpoint id. `02-AV-SYNC-SPEC.md §7`.
    pub mic: String,
    /// Audio-track topology (Slice B / D1). **`false` (default) = Mix + Mic** — the
    /// upload-safe pair (mix is always track 1). **`true` = the full per-source set**
    /// (Mix / Game / Voice chat / Other system / Mic per [`AudioTracks`]). Wired by the
    /// engine's track-set builder (`planned_kinds`). Renamed semantics + default flip
    /// from Slice A, where `true`/{desktop, mic} was the shipped default (DECISIONS D1).
    pub separate_tracks: bool,
    /// AAC bitrate per track. `§2.6` (default 160 kbps, tunable 96–256).
    pub bitrate_bps: u32,
    /// Slice-B per-source track toggles, gating the planned track set when
    /// `separate_tracks` is on (read by `planned_kinds` since B1; the tracks
    /// themselves are captured from B2/B4). Must stay after the scalar fields above for
    /// TOML serialization.
    pub tracks: AudioTracks,
    /// Voice-chat apps the Slice-B detector scans for (schema v2 / A1, seeded with
    /// the Discord default). Array-of-tables — must be the last field.
    pub vc_apps: Vec<VcApp>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            mic: "default-follow".to_string(),
            // D1: default = Mix + Mic (was `true`/{desktop,mic} through Slice A).
            separate_tracks: false,
            bitrate_bps: BITRATE_DEFAULT_BPS,
            tracks: AudioTracks::default(),
            vc_apps: vec![VcApp::discord_default()],
        }
    }
}

/// `[buffer]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct BufferConfig {
    /// Retained buffer duration in seconds. `§3` (default 120, max 600).
    pub seconds: u32,
    /// Clear the buffer after a successful save. `01-PROJECT-PLAN.md §3`
    /// pitfall 23 (default true).
    pub clear_after_save: bool,
    /// Tighten the GOP to 1 s for tighter clip starts. `§3` (default off).
    pub precise_mode: bool,
    /// Raise QP by 1 under sustained byte-cap eviction. `§6.2` (default on).
    pub auto_qp_relief: bool,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_BUFFER_SECONDS,
            clear_after_save: true,
            precise_mode: false,
            auto_qp_relief: true,
        }
    }
}

/// `[output]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct OutputConfig {
    /// Output directory. Empty = the OS default Videos folder
    /// (`%USERPROFILE%\Videos\{PRODUCT_NAME}`, resolved at runtime by
    /// [`resolve_output_dir`], not baked into the file). `01-PROJECT-PLAN.md §3`
    /// pitfall 24.
    pub dir: String,
    /// Filename template. `08-FEATURE-COMPLETE.md` M10 expands the token set;
    /// v1 ships a fixed default.
    pub filename_template: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            filename_template: "{product}_{date}_{time}".to_string(),
        }
    }
}

/// `[hotkeys]` section. Strings are parsed by the `global-hotkey` layer later;
/// v1 only stores and validates that they are present.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Save-last-N-seconds hotkey.
    pub save_clip: String,
    /// Toggle timed recording hotkey.
    pub record_toggle: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            save_clip: "Ctrl+Alt+S".to_string(),
            // Ctrl+Alt+F9 (not a letter): Ctrl+Alt+<letter> combos are commonly taken
            // by other apps; a function-key combo rarely conflicts. Registration is
            // tolerant if it IS taken (warns, buffer keeps running).
            record_toggle: "Ctrl+Alt+F9".to_string(),
        }
    }
}

/// `[feedback]` section — how a completed save is CONFIRMED to the user. Read only by
/// the shell's save-outcome sinks (tray balloon, save sound, overlay pill); the engine
/// never reads it. Additive over schema v2 (all keys default), so a v2 file without this
/// section loads unchanged. `01-PROJECT-PLAN.md §3` / DECISIONS 2026-07-09 (P1b/P1c).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Play a short sound when a clip is saved (default on). Pulled forward from M10
    /// (P1b): Win11's "when playing a game" DND suppresses the save toast during the core
    /// use case, and audio is the only in-the-moment channel Windows doesn't gate (it also
    /// covers exclusive fullscreen). Plays on SUCCESS only.
    pub save_sound: bool,
    /// Optional path to a custom save sound (`.wav`). Empty = the bundled default tone.
    pub save_sound_path: String,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            save_sound: true,
            save_sound_path: String::new(),
        }
    }
}

/// The whole configuration, schema v2.
///
/// `#[serde(default)]` means a missing section or key falls back to its spec
/// default rather than erroring — so a minimal (or empty) file is valid and the
/// effective config is always complete. Unknown keys are ignored on read (not
/// denied) and preserved on rewrite; see the module docs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Schema version. Must equal [`CONFIG_VERSION`].
    pub config_version: u32,
    #[serde(rename = "capture")]
    pub capture: CaptureConfig,
    #[serde(rename = "encode")]
    pub encode: EncodeConfig,
    #[serde(rename = "audio")]
    pub audio: AudioConfig,
    #[serde(rename = "buffer")]
    pub buffer: BufferConfig,
    #[serde(rename = "output")]
    pub output: OutputConfig,
    #[serde(rename = "hotkeys")]
    pub hotkeys: HotkeyConfig,
    #[serde(rename = "feedback")]
    pub feedback: FeedbackConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            capture: CaptureConfig::default(),
            encode: EncodeConfig::default(),
            audio: AudioConfig::default(),
            buffer: BufferConfig::default(),
            output: OutputConfig::default(),
            hotkeys: HotkeyConfig::default(),
            feedback: FeedbackConfig::default(),
        }
    }
}

impl Config {
    /// Parse a config from a TOML string, migrate it forward to the current
    /// schema (in memory), and validate it.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let mut cfg: Config = toml::from_str(s)?;
        cfg.migrate()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Bring a just-parsed config up to [`CONFIG_VERSION`] **in memory**. A file
    /// older than the current schema is migrated (never rewritten here — the disk
    /// file is only rewritten on an explicit user change, `§6`/pitfall-30); a file
    /// outside [`MIN_SUPPORTED_CONFIG_VERSION`]..=[`CONFIG_VERSION`] is rejected,
    /// never silently reset.
    fn migrate(&mut self) -> Result<(), ConfigError> {
        if !(MIN_SUPPORTED_CONFIG_VERSION..=CONFIG_VERSION).contains(&self.config_version) {
            return Err(ConfigError::UnsupportedVersion {
                found: self.config_version,
                expected: CONFIG_VERSION,
            });
        }
        if self.config_version == 1 {
            self.migrate_v1_to_v2();
        }
        Ok(())
    }

    /// v1 → v2 migration (schema v2 / A1). New keys (`encode.quality`,
    /// `encode.resolution`, `[audio.tracks]`, `[[audio.vc_apps]]`) already hold
    /// their serde defaults from the parse; the only carried value is the v1
    /// `encode.max_height` integer, preserved as the advanced override so behavior
    /// is unchanged — unless it equals the historical default cap, in which case
    /// we drop it for a clean `resolution = "native"`.
    fn migrate_v1_to_v2(&mut self) {
        if self.encode.max_height == Some(DEFAULT_MAX_ENCODE_HEIGHT) {
            self.encode.max_height = None;
        }
        self.encode.resolution = Resolution::Native;
        self.config_version = CONFIG_VERSION;
    }

    /// Load and validate a config file from disk.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Serialize the effective (defaults-filled) config back to TOML — what
    /// `--check-config` prints.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Format-preserving serialization: overlay this config onto `existing` TOML
    /// text, overwriting only known keys and leaving comments and unknown keys
    /// untouched (`01-PROJECT-PLAN §3` pitfall 30). An empty `existing` yields a
    /// fresh, fully-populated v2 document. Always writes `config_version =`
    /// [`CONFIG_VERSION`] — a rewrite migrates the file to the current schema.
    ///
    /// This is the write half of the single config representation: reads go
    /// through `toml`/serde into the typed [`Config`]; this uses `toml_edit` only
    /// as the preserving serializer, not as a second schema.
    pub fn to_preserving_toml(&self, existing: &str) -> Result<String, ConfigError> {
        let mut doc: DocumentMut = existing.parse()?;
        self.apply_to_document(&mut doc);
        Ok(doc.to_string())
    }

    /// Atomically rewrite the config file at `path`, preserving the existing
    /// file's comments and unknown keys. Writes `path.part`, flushes it to disk
    /// (`FlushFileBuffers`, `§4.7`), then renames over `path`. Creates the parent
    /// directory if needed. The UI and any future editor write config **only**
    /// through here (same typed path as `--check-config`).
    pub fn write_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let rendered = self.to_preserving_toml(&existing)?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }

        let mut part_os = path.as_os_str().to_owned();
        part_os.push(crate::spec_constants::mux::PART_SUFFIX);
        let part = PathBuf::from(part_os);

        let io_err = |p: &Path| {
            let p = p.to_path_buf();
            move |source| ConfigError::Io {
                path: p.clone(),
                source,
            }
        };

        {
            let mut f = std::fs::File::create(&part).map_err(io_err(&part))?;
            f.write_all(rendered.as_bytes()).map_err(io_err(&part))?;
            f.sync_all().map_err(io_err(&part))?; // FlushFileBuffers (§4.7)
        }
        std::fs::rename(&part, path).map_err(io_err(path))?;
        Ok(())
    }

    /// Overlay every known key of `self` onto `doc`. Scalars are updated in place
    /// (preserving their surrounding decor/comments via [`set_val`]); the
    /// `[[audio.vc_apps]]` array-of-tables is rebuilt wholesale (it is data, not
    /// comment-bearing). Keys not written here — unknown/forward-compat keys —
    /// are never touched.
    fn apply_to_document(&self, doc: &mut DocumentMut) {
        let root = doc.as_table_mut();
        set_val(root, "config_version", Value::from(CONFIG_VERSION as i64));

        // [capture]
        let capture = ensure_table(root, "capture");
        let target = match &self.capture.target {
            CaptureTarget::Named(NamedTarget::Primary) => Value::from("primary"),
            CaptureTarget::Named(NamedTarget::FocusedWindow) => Value::from("focused-window"),
            CaptureTarget::Monitor(n) => Value::from(*n as i64),
        };
        set_val(capture, "target", target);
        set_val(capture, "fps", Value::from(self.capture.fps as i64));
        set_val(capture, "cursor", Value::from(self.capture.cursor));

        // [encode]
        let encode = ensure_table(root, "encode");
        set_val(
            encode,
            "codec",
            Value::from(codec_toml_str(&self.encode.codec)),
        );
        set_val(
            encode,
            "quality",
            Value::from(quality_toml_str(&self.encode.quality)),
        );
        set_val(
            encode,
            "resolution",
            Value::from(resolution_toml_str(&self.encode.resolution)),
        );
        match self.encode.max_height {
            Some(h) => set_val(encode, "max_height", Value::from(h as i64)),
            None => {
                encode.remove("max_height");
            }
        }

        // [audio]
        let audio = ensure_table(root, "audio");
        set_val(audio, "desktop", Value::from(self.audio.desktop));
        set_val(audio, "mic", Value::from(self.audio.mic.as_str()));
        set_val(
            audio,
            "separate_tracks",
            Value::from(self.audio.separate_tracks),
        );
        set_val(
            audio,
            "bitrate_bps",
            Value::from(self.audio.bitrate_bps as i64),
        );
        {
            let tracks = ensure_table(audio, "tracks");
            set_val(tracks, "game", Value::from(self.audio.tracks.game));
            set_val(
                tracks,
                "voice_chat",
                Value::from(self.audio.tracks.voice_chat),
            );
            set_val(
                tracks,
                "other_system",
                Value::from(self.audio.tracks.other_system),
            );
        }
        let mut apps = ArrayOfTables::new();
        for app in &self.audio.vc_apps {
            let mut t = Table::new();
            t["name"] = Item::Value(Value::from(app.name.as_str()));
            let mut names = Array::new();
            for n in &app.process_names {
                names.push(n.as_str());
            }
            t["process_names"] = Item::Value(Value::Array(names));
            t["include_tree"] = Item::Value(Value::from(app.include_tree));
            t["enabled"] = Item::Value(Value::from(app.enabled));
            apps.push(t);
        }
        audio.insert("vc_apps", Item::ArrayOfTables(apps));

        // [buffer]
        let buffer = ensure_table(root, "buffer");
        set_val(buffer, "seconds", Value::from(self.buffer.seconds as i64));
        set_val(
            buffer,
            "clear_after_save",
            Value::from(self.buffer.clear_after_save),
        );
        set_val(
            buffer,
            "precise_mode",
            Value::from(self.buffer.precise_mode),
        );
        set_val(
            buffer,
            "auto_qp_relief",
            Value::from(self.buffer.auto_qp_relief),
        );

        // [output]
        let output = ensure_table(root, "output");
        set_val(output, "dir", Value::from(self.output.dir.as_str()));
        set_val(
            output,
            "filename_template",
            Value::from(self.output.filename_template.as_str()),
        );

        // [hotkeys]
        let hotkeys = ensure_table(root, "hotkeys");
        set_val(
            hotkeys,
            "save_clip",
            Value::from(self.hotkeys.save_clip.as_str()),
        );
        set_val(
            hotkeys,
            "record_toggle",
            Value::from(self.hotkeys.record_toggle.as_str()),
        );
    }

    /// Validate all invariants the spec dictates. Called after every parse.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.config_version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: self.config_version,
                expected: CONFIG_VERSION,
            });
        }

        if !SUPPORTED_FPS.contains(&self.capture.fps) {
            return Err(ConfigError::Invalid(format!(
                "capture.fps = {} is not one of {:?}",
                self.capture.fps, SUPPORTED_FPS
            )));
        }

        if self.buffer.seconds == 0 || self.buffer.seconds > MAX_BUFFER_SECONDS {
            return Err(ConfigError::Invalid(format!(
                "buffer.seconds = {} must be in 1..={}",
                self.buffer.seconds, MAX_BUFFER_SECONDS
            )));
        }

        if let Some(h) = self.encode.max_height {
            if !(MAX_ENCODE_HEIGHT_MIN..=MAX_ENCODE_HEIGHT_MAX).contains(&h) {
                return Err(ConfigError::Invalid(format!(
                    "encode.max_height = {h} must be in {MAX_ENCODE_HEIGHT_MIN}..={MAX_ENCODE_HEIGHT_MAX}"
                )));
            }
        }

        if !(BITRATE_MIN_BPS..=BITRATE_MAX_BPS).contains(&self.audio.bitrate_bps) {
            return Err(ConfigError::Invalid(format!(
                "audio.bitrate_bps = {} must be in {}..={}",
                self.audio.bitrate_bps, BITRATE_MIN_BPS, BITRATE_MAX_BPS
            )));
        }

        if self.audio.mic.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "audio.mic must be \"default-follow\", \"off\", or a device id".to_string(),
            ));
        }

        Ok(())
    }
}

/// Get-or-create a child `[name]` table of `parent`, replacing a non-table value
/// of the same name (a malformed file would already have failed the typed load,
/// so this only fires defensively). Used by the format-preserving rewrite.
fn ensure_table<'a>(parent: &'a mut Table, name: &str) -> &'a mut Table {
    let item = parent
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()));
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("just ensured a table")
}

/// Set `table[key] = v`, preserving the existing value's surrounding decor
/// (whitespace + inline `# comment`) when the key is already present. This is
/// what keeps a user's comment on a known key alive across a rewrite; unknown
/// keys are never passed here, so they are untouched entirely.
fn set_val(table: &mut Table, key: &str, v: Value) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = v;
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(key, Item::Value(v));
        }
    }
}

/// The TOML string for a [`Codec`] — must match its `#[serde]` representation
/// (guarded by `enum_toml_strings_match_serde`).
fn codec_toml_str(codec: &Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
    }
}

/// The TOML string for a [`Quality`] tier — see [`codec_toml_str`].
fn quality_toml_str(quality: &Quality) -> &'static str {
    match quality {
        Quality::Efficient => "efficient",
        Quality::Default => "default",
        Quality::High => "high",
        Quality::Max => "max",
    }
}

/// The TOML string for a [`Resolution`] tier — see [`codec_toml_str`].
fn resolution_toml_str(resolution: &Resolution) -> &'static str {
    match resolution {
        Resolution::Native => "native",
        Resolution::P1440 => "1440",
        Resolution::P1080 => "1080",
        Resolution::P720 => "720",
    }
}

/// The default config file path: `%APPDATA%\{PRODUCT_NAME}\config.toml`. Falls
/// back to `config.toml` in the working directory if `%APPDATA%` is unset.
pub fn default_config_path() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata)
            .join(PRODUCT_NAME)
            .join("config.toml"),
        None => PathBuf::from("config.toml"),
    }
}

/// The default clips directory when `[output].dir` is empty: the OS Videos folder,
/// `%USERPROFILE%\Videos\{PRODUCT_NAME}`. Mirrors the env-var convention used by
/// [`default_config_path`] (`%APPDATA%`) and the log dir (`%LOCALAPPDATA%`) rather
/// than adding a `windows`/Shell known-folder call for one path — the env-var form
/// resolves the Videos library correctly in the normal case, stays pure + testable,
/// and honours the tie-break rule (simpler + reversible; DECISIONS 2026-07-08). Falls
/// back to the working directory if `%USERPROFILE%` is unset.
pub fn default_output_dir() -> PathBuf {
    match std::env::var_os("USERPROFILE") {
        Some(home) => PathBuf::from(home).join("Videos").join(PRODUCT_NAME),
        None => PathBuf::from("."),
    }
}

/// Resolve the configured `[output].dir` to a concrete path: an empty/whitespace
/// value means "follow the OS Videos folder" ([`default_output_dir`]); anything else
/// is taken verbatim. Pure — creating the directory (and any fallback) is the caller's
/// job so this stays unit-testable.
pub fn resolve_output_dir(dir: &str) -> PathBuf {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        default_output_dir()
    } else {
        PathBuf::from(trimmed)
    }
}

/// Note: the internal audio sample rate is fixed at [`SAMPLE_RATE_HZ`] and is
/// intentionally NOT configurable (`§2.1`: everything is resampled to it, so a
/// mismatch is structurally impossible). Re-exported here so the config module
/// documents the deliberate omission.
pub const INTERNAL_SAMPLE_RATE_HZ: u32 = SAMPLE_RATE_HZ;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_preserves_comments_and_unknown_keys() {
        // T2 / A1: the UI now rewrites config.toml on every change, so a hand-edited
        // file's comments and unknown (forward-compat) keys MUST survive a rewrite.
        let dir = std::env::temp_dir().join(format!("clipd_cfg_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let hand = "\
# my clipd config — keep this comment!
config_version = 2

[buffer]
seconds = 45  # I like a long replay
future_flux_capacitor = true  # unknown key from a newer build
";
        std::fs::write(&path, hand).unwrap();

        // Load (serde ignores the unknown key), change a known field, write back.
        let mut cfg = Config::load(&path).expect("hand-edited config loads");
        assert_eq!(cfg.buffer.seconds, 45, "known key read through");
        cfg.encode.quality = Quality::High;
        cfg.write_atomic(&path).expect("write_atomic");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# my clipd config — keep this comment!"),
            "leading comment lost:\n{after}"
        );
        assert!(
            after.contains("# I like a long replay"),
            "inline comment lost:\n{after}"
        );
        assert!(
            after.contains("future_flux_capacitor = true"),
            "unknown forward-compat key lost:\n{after}"
        );
        // The reloaded doc still parses and reflects the change.
        let reloaded = Config::load(&path).expect("rewritten config still loads");
        assert_eq!(reloaded.encode.quality, Quality::High);
        assert_eq!(reloaded.buffer.seconds, 45);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_config_is_valid_and_round_trips() {
        let cfg = Config::default();
        cfg.validate().expect("defaults must be valid");
        let toml = cfg.to_toml().unwrap();
        let back = Config::from_toml_str(&toml).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn resolve_output_dir_empty_follows_videos_default() {
        // An empty or whitespace-only dir resolves to the OS Videos default, which
        // ends with `Videos/{PRODUCT_NAME}` (or the CWD fallback when USERPROFILE is
        // unset — then it just matches default_output_dir()).
        for empty in ["", "   ", "\t"] {
            assert_eq!(resolve_output_dir(empty), default_output_dir());
        }
        if std::env::var_os("USERPROFILE").is_some() {
            let d = default_output_dir();
            assert!(
                d.ends_with(PathBuf::from("Videos").join(PRODUCT_NAME)),
                "expected …/Videos/{PRODUCT_NAME}, got {}",
                d.display()
            );
        }
    }

    #[test]
    fn resolve_output_dir_explicit_is_verbatim() {
        assert_eq!(
            resolve_output_dir("D:/clips"),
            PathBuf::from("D:/clips"),
            "an explicit dir is taken as-is"
        );
        // Surrounding whitespace is trimmed (a stray space must not create a phantom
        // sibling directory).
        assert_eq!(
            resolve_output_dir("  D:/clips  "),
            PathBuf::from("D:/clips")
        );
    }

    #[test]
    fn empty_file_yields_defaults() {
        // A minimal/empty file must produce a complete effective config.
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_file_fills_missing_with_defaults() {
        let cfg = Config::from_toml_str("[buffer]\nseconds = 300\n").unwrap();
        assert_eq!(cfg.buffer.seconds, 300);
        assert_eq!(cfg.capture.fps, DEFAULT_FPS); // untouched → default
        assert!(cfg.buffer.clear_after_save);
    }

    #[test]
    fn unknown_keys_are_tolerated_not_rejected() {
        // §3/pitfall-30: reading must not choke on unknown keys (forward-compat).
        let cfg = Config::from_toml_str("mystery_key = 42\n[capture]\nfuture = true\nfps = 30\n")
            .unwrap();
        assert_eq!(cfg.capture.fps, 30);
    }

    #[test]
    fn capture_target_string_and_int_forms_parse() {
        let a = Config::from_toml_str("[capture]\ntarget = \"primary\"\n").unwrap();
        assert_eq!(a.capture.target, CaptureTarget::Named(NamedTarget::Primary));

        let b = Config::from_toml_str("[capture]\ntarget = \"focused-window\"\n").unwrap();
        assert_eq!(
            b.capture.target,
            CaptureTarget::Named(NamedTarget::FocusedWindow)
        );

        let c = Config::from_toml_str("[capture]\ntarget = 2\n").unwrap();
        assert_eq!(c.capture.target, CaptureTarget::Monitor(2));
    }

    #[test]
    fn codec_parses_lowercase() {
        let cfg = Config::from_toml_str("[encode]\ncodec = \"hevc\"\n").unwrap();
        assert_eq!(cfg.encode.codec, Codec::Hevc);
    }

    #[test]
    fn rejects_wrong_version() {
        // Newer-than-this-build → rejected, never silently reset.
        let err = Config::from_toml_str("config_version = 99\n").unwrap_err();
        assert!(matches!(
            err,
            ConfigError::UnsupportedVersion {
                found: 99,
                expected: CONFIG_VERSION
            }
        ));
        // Older-than-the-migration-floor (0) → also rejected.
        let err0 = Config::from_toml_str("config_version = 0\n").unwrap_err();
        assert!(matches!(
            err0,
            ConfigError::UnsupportedVersion { found: 0, .. }
        ));
    }

    #[test]
    fn migrates_v1_file_to_current_schema() {
        // A v1 file (with the v1 `max_height` integer) loads, is migrated in
        // memory to the current version, and gains the v2 keys' defaults.
        let v1 = "config_version = 1\n[encode]\nmax_height = 1440\n[capture]\nfps = 30\n";
        let cfg = Config::from_toml_str(v1).unwrap();
        assert_eq!(cfg.config_version, CONFIG_VERSION);
        // v1 max_height is preserved losslessly as the advanced override →
        // effective cap unchanged.
        assert_eq!(cfg.encode.max_height, Some(1440));
        assert_eq!(cfg.encode.effective_max_height(), 1440);
        // New v2 keys take their defaults.
        assert_eq!(cfg.encode.quality, Quality::Default);
        assert_eq!(cfg.encode.resolution, Resolution::Native);
        assert_eq!(cfg.audio.tracks, AudioTracks::default());
        assert_eq!(cfg.audio.vc_apps, vec![VcApp::discord_default()]);
        // Unrelated carried value survives.
        assert_eq!(cfg.capture.fps, 30);
    }

    #[test]
    fn v1_default_max_height_migrates_to_clean_native() {
        // A v1 file whose max_height IS the historical default cap drops the
        // override in favor of a clean `resolution = native` (same effective cap).
        let v1 =
            format!("config_version = 1\n[encode]\nmax_height = {DEFAULT_MAX_ENCODE_HEIGHT}\n");
        let cfg = Config::from_toml_str(&v1).unwrap();
        assert_eq!(cfg.encode.max_height, None);
        assert_eq!(cfg.encode.resolution, Resolution::Native);
        assert_eq!(cfg.encode.effective_max_height(), DEFAULT_MAX_ENCODE_HEIGHT);
    }

    #[test]
    fn quality_tiers_parse_and_map_to_multipliers() {
        for (s, tier, mult) in [
            ("efficient", Quality::Efficient, QUALITY_MULT_EFFICIENT),
            ("default", Quality::Default, QUALITY_MULT_DEFAULT),
            ("high", Quality::High, QUALITY_MULT_HIGH),
            ("max", Quality::Max, QUALITY_MULT_MAX),
        ] {
            let cfg = Config::from_toml_str(&format!("[encode]\nquality = \"{s}\"\n")).unwrap();
            assert_eq!(cfg.encode.quality, tier);
            assert_eq!(cfg.encode.quality.multiplier(), mult);
        }
        // Default when the key is absent.
        assert_eq!(EncodeConfig::default().quality, Quality::Default);
    }

    #[test]
    fn resolution_tiers_parse_and_map_to_effective_height() {
        for (s, tier, height) in [
            ("native", Resolution::Native, DEFAULT_MAX_ENCODE_HEIGHT),
            ("1440", Resolution::P1440, RESOLUTION_TIER_1440),
            ("1080", Resolution::P1080, RESOLUTION_TIER_1080),
            ("720", Resolution::P720, RESOLUTION_TIER_720),
        ] {
            let cfg = Config::from_toml_str(&format!("[encode]\nresolution = \"{s}\"\n")).unwrap();
            assert_eq!(cfg.encode.resolution, tier);
            // No override → effective height is the tier's height.
            assert_eq!(cfg.encode.effective_max_height(), height);
        }
        // The advanced override wins over the tier when set.
        let cfg =
            Config::from_toml_str("[encode]\nresolution = \"1080\"\nmax_height = 1440\n").unwrap();
        assert_eq!(cfg.encode.effective_max_height(), 1440);
    }

    #[test]
    fn vc_apps_default_is_the_discord_family() {
        let cfg = Config::default();
        assert_eq!(cfg.audio.vc_apps.len(), 1);
        let discord = &cfg.audio.vc_apps[0];
        assert_eq!(discord.name, "Discord");
        assert!(discord.process_names.contains(&"Discord.exe".to_string()));
        assert!(discord.include_tree);
        assert!(discord.enabled);
    }

    #[test]
    fn rejects_unsupported_fps() {
        let err = Config::from_toml_str("[capture]\nfps = 45\n").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn accepts_all_supported_fps() {
        for fps in SUPPORTED_FPS {
            let cfg = Config::from_toml_str(&format!("[capture]\nfps = {fps}\n")).unwrap();
            assert_eq!(cfg.capture.fps, fps);
        }
    }

    #[test]
    fn rejects_buffer_seconds_out_of_range() {
        assert!(Config::from_toml_str("[buffer]\nseconds = 0\n").is_err());
        assert!(Config::from_toml_str("[buffer]\nseconds = 601\n").is_err());
        // Boundary: exactly the max is allowed.
        assert!(
            Config::from_toml_str(&format!("[buffer]\nseconds = {MAX_BUFFER_SECONDS}\n")).is_ok()
        );
    }

    #[test]
    fn rejects_max_height_out_of_range() {
        assert!(Config::from_toml_str("[encode]\nmax_height = 240\n").is_err());
        assert!(Config::from_toml_str("[encode]\nmax_height = 5000\n").is_err());
        // Boundaries inclusive.
        assert!(Config::from_toml_str(&format!(
            "[encode]\nmax_height = {MAX_ENCODE_HEIGHT_MIN}\n"
        ))
        .is_ok());
        assert!(Config::from_toml_str(&format!(
            "[encode]\nmax_height = {MAX_ENCODE_HEIGHT_MAX}\n"
        ))
        .is_ok());
        // Default has no override; the effective cap is the native tier's height.
        assert_eq!(EncodeConfig::default().max_height, None);
        assert_eq!(
            EncodeConfig::default().effective_max_height(),
            DEFAULT_MAX_ENCODE_HEIGHT
        );
    }

    #[test]
    fn rejects_bitrate_out_of_range() {
        assert!(Config::from_toml_str("[audio]\nbitrate_bps = 64000\n").is_err());
        assert!(Config::from_toml_str("[audio]\nbitrate_bps = 320000\n").is_err());
        // Boundaries inclusive.
        assert!(
            Config::from_toml_str(&format!("[audio]\nbitrate_bps = {BITRATE_MIN_BPS}\n")).is_ok()
        );
        assert!(
            Config::from_toml_str(&format!("[audio]\nbitrate_bps = {BITRATE_MAX_BPS}\n")).is_ok()
        );
    }

    #[test]
    fn rejects_empty_mic() {
        assert!(Config::from_toml_str("[audio]\nmic = \"\"\n").is_err());
        assert!(Config::from_toml_str("[audio]\nmic = \"   \"\n").is_err());
    }

    #[test]
    fn default_path_uses_product_name() {
        // Whatever APPDATA is, the path ends with <product>/config.toml.
        let p = default_config_path();
        assert!(p.ends_with(format!("{PRODUCT_NAME}/config.toml")) || p.ends_with("config.toml"));
    }

    #[test]
    fn enum_toml_strings_match_serde() {
        // The hand-written rewrite strings MUST equal the serde representation,
        // else a rewrite would emit a value the reader can't parse.
        for c in [Codec::H264, Codec::Hevc] {
            let s = toml::Value::try_from(&c).unwrap();
            assert_eq!(s.as_str().unwrap(), codec_toml_str(&c));
        }
        for q in [
            Quality::Efficient,
            Quality::Default,
            Quality::High,
            Quality::Max,
        ] {
            let s = toml::Value::try_from(q).unwrap();
            assert_eq!(s.as_str().unwrap(), quality_toml_str(&q));
        }
        for r in [
            Resolution::Native,
            Resolution::P1440,
            Resolution::P1080,
            Resolution::P720,
        ] {
            let s = toml::Value::try_from(r).unwrap();
            assert_eq!(s.as_str().unwrap(), resolution_toml_str(&r));
        }
    }

    #[test]
    fn fresh_rewrite_from_empty_is_complete_and_valid() {
        // Overlaying defaults onto an empty file yields a full v2 document that
        // round-trips back to the defaults.
        let cfg = Config::default();
        let rendered = cfg.to_preserving_toml("").unwrap();
        assert!(rendered.contains("config_version = 2"));
        let back = Config::from_toml_str(&rendered).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn rewrite_preserves_comments_and_unknown_keys_and_bumps_version() {
        // pitfall 30: a rewrite must keep the user's comments and any unknown /
        // forward-compat keys, update the changed value, and migrate the version.
        let original = concat!(
            "# top-of-file comment\n",
            "config_version = 1\n",
            "mystery_key = \"keep me\"\n",
            "\n",
            "[capture]\n",
            "# a comment above fps\n",
            "fps = 30 # inline comment\n",
            "future_key = 123\n",
        );
        let mut cfg = Config::from_toml_str(original).unwrap();
        assert_eq!(cfg.capture.fps, 30);
        cfg.buffer.seconds = 240; // the user's change

        let out = cfg.to_preserving_toml(original).unwrap();

        // Comments survive.
        assert!(out.contains("# top-of-file comment"), "{out}");
        assert!(out.contains("# a comment above fps"), "{out}");
        assert!(out.contains("# inline comment"), "{out}");
        // Unknown keys survive.
        assert!(out.contains("mystery_key = \"keep me\""), "{out}");
        assert!(out.contains("future_key = 123"), "{out}");
        // Version migrated and the change applied.
        assert!(out.contains("config_version = 2"), "{out}");
        assert!(out.contains("seconds = 240"), "{out}");

        // Re-parse: valid, our change present, untouched values intact.
        let back = Config::from_toml_str(&out).unwrap();
        assert_eq!(back.buffer.seconds, 240);
        assert_eq!(back.capture.fps, 30);
        assert_eq!(back.config_version, CONFIG_VERSION);
    }

    #[test]
    fn rewrite_v1_audio_section_with_scalars_stays_valid() {
        // A v1 file that HAS an [audio] section with scalars (no subtables yet):
        // the rewrite must add [audio.tracks] / [[audio.vc_apps]] AFTER those
        // scalars and stay valid + preserve the user's comment.
        let v1 = concat!(
            "config_version = 1\n",
            "[audio]\n",
            "# my audio note\n",
            "desktop = true\n",
            "bitrate_bps = 128000\n",
        );
        let cfg = Config::from_toml_str(v1).unwrap();
        let out = cfg.to_preserving_toml(v1).unwrap();
        // Re-parses (would fail if a scalar landed under a subtable header).
        let back = Config::from_toml_str(&out).unwrap();
        assert_eq!(back.audio.bitrate_bps, 128000);
        assert!(out.contains("# my audio note"), "{out}");
        assert!(out.contains("config_version = 2"), "{out}");
    }

    #[test]
    fn rewrite_partial_v2_with_subtable_before_missing_scalars_stays_valid() {
        // The sharp edge: a hand-authored v2 file whose [audio.tracks] subtable
        // exists BUT the [audio] scalar keys (mic/desktop/…) are absent. Naively
        // inserting a missing scalar would append it AFTER the subtable header and
        // produce invalid TOML — a config-trust catastrophe (pitfall 30). The
        // rewrite output MUST still re-parse.
        let partial = concat!("config_version = 2\n", "[audio.tracks]\n", "game = false\n",);
        let cfg = Config::from_toml_str(partial).unwrap();
        let out = cfg.to_preserving_toml(partial).unwrap();
        let back = Config::from_toml_str(&out).unwrap();
        // The subtable value the user set is preserved; the missing scalars are
        // filled from defaults; the document is valid.
        assert!(!back.audio.tracks.game);
        assert_eq!(back.audio.mic, "default-follow");
        // And it is stable on a second pass.
        let out2 = cfg.to_preserving_toml(&out).unwrap();
        Config::from_toml_str(&out2).unwrap();
    }

    #[test]
    fn rewrite_is_idempotent_and_stays_valid() {
        // Writing twice must be stable (guards against appending a scalar after a
        // subtable and producing invalid TOML on the second pass).
        let cfg = Config::default();
        let first = cfg.to_preserving_toml("").unwrap();
        let second = cfg.to_preserving_toml(&first).unwrap();
        assert_eq!(first, second);
        assert_eq!(Config::from_toml_str(&second).unwrap(), cfg);
    }

    #[test]
    fn write_atomic_creates_dirs_reads_back_and_replaces() {
        let dir = std::env::temp_dir().join(format!(
            "clipd_cfg_test_{}_{}",
            std::process::id(),
            "write_atomic"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("config.toml"); // parent must be created

        let mut cfg = Config::default();
        cfg.buffer.seconds = 200;
        cfg.encode.quality = Quality::High;
        cfg.write_atomic(&path).unwrap();

        // No leftover .part; the real file parses back to what we wrote.
        assert!(!path.with_extension("toml.part").exists());
        let back = Config::load(&path).unwrap();
        assert_eq!(back.buffer.seconds, 200);
        assert_eq!(back.encode.quality, Quality::High);

        // A second write over the existing file preserves any added comment.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n# user note\n");
        std::fs::write(&path, &text).unwrap();
        let mut cfg2 = Config::load(&path).unwrap();
        cfg2.buffer.seconds = 111;
        cfg2.write_atomic(&path).unwrap();
        let reread = std::fs::read_to_string(&path).unwrap();
        assert!(reread.contains("# user note"), "{reread}");
        assert_eq!(Config::load(&path).unwrap().buffer.seconds, 111);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_config_template_matches_defaults() {
        // The friends-beta template (A8, `just dist`) is a hand-commented mirror of
        // the schema defaults. Loading it must parse + validate, and must equal
        // `Config::default()` — so this test fails the moment the template drifts from
        // the schema (a changed default, a bad value, or a typo'd key value).
        let template = include_str!("../dist/config.template.toml");
        let cfg = Config::from_toml_str(template)
            .expect("dist/config.template.toml must parse and validate");
        assert_eq!(
            cfg,
            Config::default(),
            "dist/config.template.toml drifted from Config::default()"
        );
    }

    /// D1 (Slice B): the default audio topology is Mix + Mic, i.e. `separate_tracks`
    /// defaults to `false` (it was `true` through Slice A). A written `true` still
    /// round-trips (selecting the full per-source set).
    #[test]
    fn separate_tracks_defaults_to_false() {
        assert!(
            !AudioConfig::default().separate_tracks,
            "D1: separate_tracks must default to false (Mix+Mic)"
        );
        let cfg = Config::from_toml_str("[audio]\nseparate_tracks = true\n")
            .expect("explicit separate_tracks = true parses");
        assert!(cfg.audio.separate_tracks);
    }
}
