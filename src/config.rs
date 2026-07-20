//! Command-line surface. Every knob has a sane default so the binary works
//! with zero arguments: camera 0 -> /dev/video10 (Linux) at 720p30.
//!
//! Settings are loaded from `~/.config/vcam-proxy/config.toml` and overridden
//! by any CLI arguments the user provides.

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "vcam-proxy",
    version,
    about = "Physical camera -> virtual loopback proxy (v4l2loopback / Win32 named pipe)"
)]
pub struct Config {
    /// Enumerate capture devices and exit.
    #[arg(long)]
    pub list: bool,

    /// Camera index as reported by `--list`.
    #[arg(short, long)]
    pub camera: Option<u32>,

    /// Sink backend. `auto` = v4l2 on Linux, pipe on Windows.
    #[arg(long, value_enum)]
    pub sink: Option<SinkKind>,

    /// Loopback device node (Linux, created by v4l2loopback).
    #[arg(short, long)]
    pub device: Option<String>,

    /// Named-pipe name (Windows): frames go to \\.\pipe\<name>.
    #[arg(long)]
    pub pipe_name: Option<String>,

    /// Requested capture width.
    #[arg(long)]
    pub width: Option<u32>,

    /// Requested capture height.
    #[arg(long)]
    pub height: Option<u32>,

    /// Requested capture frame rate.
    #[arg(long)]
    pub fps: Option<u32>,

    /// Wire format policy. `auto` passes YUYV through untouched (zero
    /// conversion) and decodes MJPEG/NV12 sources to RGB24.
    #[arg(long, value_enum)]
    pub format: Option<FormatPref>,

    /// Ring depth: number of reusable frame buffers in circulation.
    #[arg(long)]
    pub buffers: Option<usize>,

    /// Backoff between camera re-open attempts after a failure, ms.
    #[arg(long)]
    pub retry_ms: Option<u64>,

    /// List available v4l2loopback output devices and exit.
    #[arg(long)]
    pub list_loopback: bool,

    /// Test capture without writing to the loopback device (dry run).
    #[arg(long)]
    pub dry_run: bool,

    /// Disable the system-tray icon.
    #[arg(long)]
    pub no_tray: bool,

    /// Auto-load the v4l2loopback kernel module via pkexec if not present.
    #[arg(long)]
    pub auto_load_module: bool,

    /// Auto-setup mode: check system, load module, fix permissions, validate.
    /// Exits after setup — does not start the proxy.
    #[arg(long)]
    pub setup: bool,

    /// Enable multi-reader mode (multiple apps can use virtual camera).
    #[arg(long)]
    pub multi_reader: Option<bool>,

    /// v4l2loopback exclusive_caps value (0 or 1).
    /// 1 = UVC-compatible (apps recognize as camera, but some may get exclusive access).
    /// 0 = broader compatibility (allows multiple simultaneous readers).
    #[arg(long)]
    pub exclusive_caps: Option<u32>,

    /// v4l2loopback timeout in ms (how long frames persist without a reader).
    #[arg(long)]
    pub timeout: Option<u32>,

    /// Save current settings to config file for persistence.
    #[arg(long)]
    pub save_config: bool,

    /// Open the config file in the default editor.
    #[arg(long)]
    pub edit_config: bool,

    /// Show the current settings and their source (CLI / config file / default).
    #[arg(long)]
    pub show_config: bool,
}

/// Resolved configuration with all values populated from CLI + settings file.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub camera: u32,
    pub sink: SinkKind,
    pub device: String,
    #[allow(dead_code)] // only used on Windows builds
    pub pipe_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: FormatPref,
    pub buffers: usize,
    pub retry_ms: u64,
    pub multi_reader: bool,
    pub exclusive_caps: u32,
    pub timeout: u32,
}

impl ResolvedConfig {
    /// Build a ResolvedConfig by merging CLI args over settings file values.
    pub fn from_cli_and_settings(cli: &Config, settings: &crate::settings::Settings) -> Self {
        Self {
            camera: cli.camera.unwrap_or(settings.camera),
            sink: cli.sink.unwrap_or(SinkKind::Auto),
            device: cli.device.clone().unwrap_or_else(|| settings.device.clone()),
            pipe_name: cli.pipe_name.clone().unwrap_or_else(|| "vcam_proxy_0".to_string()),
            width: cli.width.unwrap_or(settings.width),
            height: cli.height.unwrap_or(settings.height),
            fps: cli.fps.unwrap_or(settings.fps),
            format: cli.format.unwrap_or(settings.format),
            buffers: cli.buffers.unwrap_or(settings.buffers),
            retry_ms: cli.retry_ms.unwrap_or(settings.retry_ms),
            multi_reader: cli.multi_reader.unwrap_or(settings.multi_reader),
            exclusive_caps: cli.exclusive_caps.unwrap_or(settings.exclusive_caps),
            timeout: cli.timeout.unwrap_or(settings.timeout),
        }
    }

    /// Convert back to settings for saving.
    pub fn to_settings(&self) -> crate::settings::Settings {
        crate::settings::Settings {
            camera: self.camera,
            device: self.device.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            buffers: self.buffers,
            format: self.format,
            retry_ms: self.retry_ms,
            multi_reader: self.multi_reader,
            exclusive_caps: self.exclusive_caps,
            timeout: self.timeout,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkKind {
    Auto,
    V4l2,
    Pipe,
    /// Discard frames; useful for benchmarking the capture side alone.
    Null,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FormatPref {
    #[default]
    Auto,
    Yuy2,
    Rgb24,
    Nv12,
    /// Compressed passthrough (Linux sink only).
    Mjpeg,
}
