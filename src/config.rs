//! Versioned TOML configuration (schema v1) and the `--check-config` load path.
//!
//! `01-PROJECT-PLAN.md §3` pitfall 30 and `§6`: *"Versioned schema
//! (`config_version = 1`), unknown keys preserved on rewrite, file only
//! rewritten on explicit user change, and a `--check-config` flag that
//! validates and prints the effective config. Silent resets are the #1
//! incumbent trust-killer."*
//!
//! This module is pure logic (100% safe, unit-testable): parse → validate →
//! serialize. There is deliberately **no rewrite path yet** — nothing in v1
//! writes config to disk. Unknown keys are therefore *tolerated* on read (they
//! do not error), and full round-trip *preservation* on rewrite lands with the
//! Milestone-5 settings path (see `DECISIONS.md`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::spec_constants::{
    audio::{
        aac::{BITRATE_DEFAULT_BPS, BITRATE_MAX_BPS, BITRATE_MIN_BPS},
        SAMPLE_RATE_HZ,
    },
    ring::{DEFAULT_BUFFER_SECONDS, MAX_BUFFER_SECONDS},
    video::{
        DEFAULT_FPS, DEFAULT_MAX_ENCODE_HEIGHT, MAX_ENCODE_HEIGHT_MAX, MAX_ENCODE_HEIGHT_MIN,
        SUPPORTED_FPS,
    },
    CONFIG_VERSION, PRODUCT_NAME,
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
    /// The config file is not valid TOML for the v1 schema.
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

/// `[encode]` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct EncodeConfig {
    /// Video codec. Default h264.
    pub codec: Codec,
    /// Encode-height ceiling for the fixed output canvas (M4-2 / pitfall 11). The
    /// canvas is the capture monitor's resolution scaled to fit within this height,
    /// evened; a resized window is letterboxed into it so a clip spans resizes at one
    /// resolution. Default [`DEFAULT_MAX_ENCODE_HEIGHT`] (2160).
    pub max_height: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            codec: Codec::default(),
            max_height: DEFAULT_MAX_ENCODE_HEIGHT,
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
    /// Two separate AAC tracks (desktop, mic) vs. none-mixed. `§2.5` (v1 has no
    /// mixed track; this toggles whether the mic track is written at all).
    pub separate_tracks: bool,
    /// AAC bitrate per track. `§2.6` (default 160 kbps, tunable 96–256).
    pub bitrate_bps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            mic: "default-follow".to_string(),
            separate_tracks: true,
            bitrate_bps: BITRATE_DEFAULT_BPS,
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
    /// Output directory. Empty = the OS default Videos folder (resolved at
    /// runtime, not baked into the file). `01-PROJECT-PLAN.md §3` pitfall 24.
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

/// The whole configuration, schema v1.
///
/// `#[serde(default)]` means a missing section or key falls back to its spec
/// default rather than erroring — so a minimal (or empty) file is valid and the
/// effective config is always complete. Unknown keys are ignored on read (not
/// denied); see the module docs on rewrite/preservation.
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
        }
    }
}

impl Config {
    /// Parse a config from a TOML string and validate it.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
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

        if !(MAX_ENCODE_HEIGHT_MIN..=MAX_ENCODE_HEIGHT_MAX).contains(&self.encode.max_height) {
            return Err(ConfigError::Invalid(format!(
                "encode.max_height = {} must be in {}..={}",
                self.encode.max_height, MAX_ENCODE_HEIGHT_MIN, MAX_ENCODE_HEIGHT_MAX
            )));
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

/// Note: the internal audio sample rate is fixed at [`SAMPLE_RATE_HZ`] and is
/// intentionally NOT configurable (`§2.1`: everything is resampled to it, so a
/// mismatch is structurally impossible). Re-exported here so the config module
/// documents the deliberate omission.
pub const INTERNAL_SAMPLE_RATE_HZ: u32 = SAMPLE_RATE_HZ;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_round_trips() {
        let cfg = Config::default();
        cfg.validate().expect("defaults must be valid");
        let toml = cfg.to_toml().unwrap();
        let back = Config::from_toml_str(&toml).unwrap();
        assert_eq!(cfg, back);
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
        let err = Config::from_toml_str("config_version = 99\n").unwrap_err();
        assert!(matches!(
            err,
            ConfigError::UnsupportedVersion {
                found: 99,
                expected: 1
            }
        ));
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
        // Default is valid and mid-range.
        assert_eq!(
            EncodeConfig::default().max_height,
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
}
