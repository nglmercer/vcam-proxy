//! Persistent configuration file support.
//!
//! Primary configuration surface: `~/.config/vcam-proxy/config.toml`.
//! Prefer editing this file (or the Settings GUI) over CLI flags.
//!
//! Example:
//! ```toml
//! camera = 0
//! device = "/dev/video10"
//! width = 1280
//! height = 720
//! fps = 30
//! buffers = 4
//! format = "auto"
//! retry_ms = 1000
//! multi_reader = true
//! devices = 1
//! exclusive_caps = 1
//! timeout = 0
//! auto_load_module = true
//! auto_resolution = true
//! image = "/path/to/logo.png"   # optional: stream a still image instead of a webcam
//! multi_app_timeout = 30         # seconds to wait for a busy camera to free up before falling back
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::FormatPref;

/// Persistent settings that can be saved to and loaded from a config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Physical camera index.
    #[serde(default)]
    pub camera: u32,
    /// Virtual device node path.
    #[serde(default = "default_device")]
    pub device: String,
    /// Requested capture width (used when `auto_resolution = false`).
    #[serde(default = "default_width")]
    pub width: u32,
    /// Requested capture height (used when `auto_resolution = false`).
    #[serde(default = "default_height")]
    pub height: u32,
    /// Requested frame rate (tie-break when `auto_resolution = true`).
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Number of frame buffers in circulation.
    #[serde(default = "default_buffers")]
    pub buffers: usize,
    /// Wire format preference.
    #[serde(default)]
    pub format: FormatPref,
    /// Backoff between camera re-open attempts (ms).
    #[serde(default = "default_retry_ms")]
    pub retry_ms: u64,
    /// Allow multiple apps to use the virtual camera at the same time.
    /// v4l2loopback ≥ 0.14 grants only ONE streaming reader per device node,
    /// so multi-app support feeds one node per app (see `devices`).
    #[serde(default = "default_true")]
    pub multi_reader: bool,
    /// Number of v4l2loopback device nodes to feed (1 = single node).
    /// With `multi_reader = true` at least 2 nodes are created automatically
    /// ('vcam-proxy', 'vcam-proxy-2', …) — assign each app its own camera.
    #[serde(default = "default_devices")]
    pub devices: u32,
    /// v4l2loopback exclusive_caps (1 = browser-compatible).
    #[serde(default = "default_exclusive_caps")]
    pub exclusive_caps: u32,
    /// v4l2loopback frame timeout in ms. `0` keeps the last frame forever
    /// (avoids green flashes when a reader reconnects).
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    /// Auto-install/load v4l2loopback via pkexec when missing (default on).
    #[serde(default = "default_true")]
    pub auto_load_module: bool,
    /// Pick the camera's highest supported mode instead of `width`/`height`.
    #[serde(default = "default_true")]
    pub auto_resolution: bool,
    /// Optional still image to stream instead of a webcam (demos / tests).
    /// When set, `auto_resolution` is ignored and the image's size is used.
    #[serde(default)]
    pub image: Option<String>,
    /// Seconds to keep retrying a module reload when the virtual camera is
    /// busy (another app holds it open). `0` = give up immediately and fall
    /// back to single-node. Only used when the device count must change
    /// (multi-app mode or editing `devices` via the manager).
    #[serde(default = "default_multi_app_timeout")]
    pub multi_app_timeout: u32,
}

fn default_device() -> String {
    "/dev/video10".to_string()
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    720
}
fn default_fps() -> u32 {
    30
}
fn default_buffers() -> usize {
    4
}
fn default_retry_ms() -> u64 {
    1000
}
fn default_true() -> bool {
    true
}
fn default_devices() -> u32 {
    1
}
fn default_exclusive_caps() -> u32 {
    1
}
fn default_timeout() -> u32 {
    0
}
fn default_multi_app_timeout() -> u32 {
    30
}

// NOTE: `#[serde(default = "...")]` only applies when *deserializing* a
// (possibly partial) TOML file. It does NOT feed into `Default::default()`.
impl Default for Settings {
    fn default() -> Self {
        Self {
            camera: 0,
            device: default_device(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            buffers: default_buffers(),
            format: FormatPref::default(),
            retry_ms: default_retry_ms(),
            multi_reader: true,
            devices: default_devices(),
            exclusive_caps: default_exclusive_caps(),
            timeout: default_timeout(),
            auto_load_module: true,
            auto_resolution: true,
            image: None,
            multi_app_timeout: default_multi_app_timeout(),
        }
    }
}

impl Settings {
    /// Get the default config file path.
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("vcam-proxy").join("config.toml")
    }

    fn ensure_dir(path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Load settings from the config file.
    /// Returns default settings if the file doesn't exist or is invalid.
    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.exists() {
            debug!(path = %path.display(), "no config file found, using defaults");
            return Self::default();
        }

        match fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(settings) => {
                    info!(path = %path.display(), "loaded settings from config file");
                    settings
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to parse config file, using defaults");
                    Self::default()
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read config file, using defaults");
                Self::default()
            }
        }
    }

    /// Save settings to the config file.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        Self::ensure_dir(&path)?;

        let content = toml::to_string_pretty(self).map_err(std::io::Error::other)?;

        fs::write(&path, content)?;
        info!(path = %path.display(), "saved settings to config file");
        Ok(())
    }

    /// Create a settings file with defaults if missing (first-run bootstrap).
    pub fn create_default_file() -> std::io::Result<PathBuf> {
        let path = Self::config_path();
        Self::ensure_dir(&path)?;

        if !path.exists() {
            Self::default().save()?;
        }

        Ok(path)
    }
}
