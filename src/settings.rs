//! Persistent configuration file support.
//!
//! Settings are stored in TOML format at `~/.config/vcam-proxy/config.toml`.
//! CLI arguments always take precedence over file settings.
//!
//! Example config file:
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
    /// Requested capture width.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Requested capture height.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Requested frame rate.
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
    /// Allow multiple apps to read the virtual camera at the same time.
    /// A single v4l2loopback device node natively supports many concurrent
    /// readers; no extra nodes are needed for Chrome + Zoom + OBS at once.
    #[serde(default = "default_multi_reader")]
    pub multi_reader: bool,
    /// Number of v4l2loopback device nodes to feed (multi-node mode).
    /// 1 = a single virtual camera (default). >=2 writes the feed to N
    /// isolated nodes for apps that grab a device exclusively.
    #[serde(default = "default_devices")]
    pub devices: u32,
    /// v4l2loopback exclusive_caps parameter (0 or 1).
    /// Controls capability advertisement only (CAPTURE-only while a producer
    /// is attached, so browsers list the node as a camera). It does NOT limit
    /// the number of concurrent readers. Keep at 1.
    #[serde(default = "default_exclusive_caps")]
    pub exclusive_caps: u32,
    /// v4l2loopback timeout in ms (how long frames persist without a reader).
    #[serde(default = "default_timeout")]
    pub timeout: u32,
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
fn default_multi_reader() -> bool {
    true
}
fn default_devices() -> u32 {
    1
}
fn default_exclusive_caps() -> u32 {
    1
}
fn default_timeout() -> u32 {
    1000
}

// NOTE: `#[serde(default = "...")]` only applies when *deserializing* a
// (possibly partial) TOML file. It does NOT feed into `Default::default()`.
// Deriving `Default` would therefore zero every numeric field, which in turn
// yields a 0-slot buffer pool and a 0-byte frame size — silently dropping
// every captured frame. We implement `Default` by hand so the no-config-file
// path uses the same sane values as the serde defaults.
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
            multi_reader: default_multi_reader(),
            devices: default_devices(),
            exclusive_caps: default_exclusive_caps(),
            timeout: default_timeout(),
        }
    }
}

impl Settings {
    /// Get the default config file path.
    /// Uses XDG_CONFIG_HOME or falls back to ~/.config/vcam-proxy/config.toml
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("vcam-proxy").join("config.toml")
    }

    /// Ensure the config directory exists.
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

        let content = toml::to_string_pretty(self)
            .map_err(std::io::Error::other)?;

        fs::write(&path, content)?;
        info!(path = %path.display(), "saved settings to config file");
        Ok(())
    }

    /// Create a settings file with current values (for first-time setup).
    pub fn create_default_file() -> std::io::Result<PathBuf> {
        let path = Self::config_path();
        Self::ensure_dir(&path)?;

        if !path.exists() {
            let default = Self::default();
            default.save()?;
        }

        Ok(path)
    }
}
