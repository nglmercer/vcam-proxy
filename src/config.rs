//! Command-line surface. Every knob has a sane default so the binary works
//! with zero arguments: camera 0 -> /dev/video10 (Linux) at 720p30.
//!
//! Settings are loaded from `~/.config/vcam-proxy/config.toml` and overridden
//! by any CLI arguments the user provides.

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug, Clone)]
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

    /// Wire format policy for the virtual camera.
    /// `auto` always outputs YUYV — the only format accepted by every app
    /// (Chrome, Firefox, Zoom, Teams, OBS). NV12 is rejected by Zoom and some
    /// Firefox builds; RGB24 is rejected by Chrome/Firefox; MJPEG loopback
    /// fails in most apps. Use explicit values only if you know your consumer.
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

    /// Run headless without the settings GUI (args/config file only).
    /// By default vcam-proxy opens a settings window on first run and keeps a
    /// tray icon afterwards; pass this to skip all of that.
    #[arg(long)]
    pub no_gui: bool,

    /// Force the settings window to open on startup (overrides first-run logic).
    #[arg(long)]
    pub settings: bool,

    /// Auto-load the v4l2loopback kernel module via pkexec if not present.
    #[arg(long)]
    pub auto_load_module: bool,

    /// Auto-setup mode: check system, load module, fix permissions, validate.
    /// Exits after setup — does not start the proxy.
    #[arg(long)]
    pub setup: bool,

    /// Allow multiple apps to read the virtual camera at the same time
    /// (native v4l2loopback multi-reader on a single device node).
    /// Accepts an optional value: `--multi-reader`, `--multi-reader true|false`.
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub multi_reader: Option<bool>,

    /// Number of v4l2loopback device nodes to feed (multi-node mode).
    /// 1 (default) = a single virtual camera; multiple apps can still open it
    /// concurrently. >=2 = write the feed to N isolated nodes, for apps that
    /// grab a device exclusively. May require a module reload (pkexec prompt)
    /// and apps using the virtual camera must re-open it afterwards.
    #[arg(long)]
    pub devices: Option<u32>,

    /// v4l2loopback exclusive_caps value (0 or 1).
    /// 1 = required for Chrome/Firefox/Zoom to list the device as a camera
    ///     (hides OUTPUT caps from consumers). Keep this at 1.
    /// 0 = device advertises both CAPTURE+OUTPUT; browsers usually ignore it.
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
    /// Number of v4l2loopback device nodes to feed (1 = single node).
    pub devices: u32,
    pub exclusive_caps: u32,
    pub timeout: u32,
    /// When true, the capture thread ignores `width`/`height`/`fps` and instead
    /// negotiates the camera's highest-resolution supported mode (fps as
    /// tiebreak). Enabled automatically when the user does not pass an explicit
    /// `--width`/`--height` on the CLI. `width`/`height`/`fps` still hold the
    /// fallback geometry used if the capability query fails.
    pub auto_resolution: bool,
}

impl ResolvedConfig {
    /// Build a ResolvedConfig by merging CLI args over settings file values.
    pub fn from_cli_and_settings(cli: &Config, settings: &crate::settings::Settings) -> Self {
        Self {
            camera: cli.camera.unwrap_or(settings.camera),
            sink: cli.sink.unwrap_or(SinkKind::Auto),
            device: cli
                .device
                .clone()
                .unwrap_or_else(|| settings.device.clone()),
            pipe_name: cli
                .pipe_name
                .clone()
                .unwrap_or_else(|| "vcam_proxy_0".to_string()),
            width: cli.width.unwrap_or(settings.width),
            height: cli.height.unwrap_or(settings.height),
            fps: cli.fps.unwrap_or(settings.fps),
            format: cli.format.unwrap_or(settings.format),
            buffers: cli.buffers.unwrap_or(settings.buffers),
            retry_ms: cli.retry_ms.unwrap_or(settings.retry_ms),
            multi_reader: cli.multi_reader.unwrap_or(settings.multi_reader),
            devices: cli.devices.unwrap_or(settings.devices),
            exclusive_caps: cli.exclusive_caps.unwrap_or(settings.exclusive_caps),
            timeout: cli.timeout.unwrap_or(settings.timeout),
            // Auto-pick the camera's max resolution unless the user pinned a
            // specific geometry via the CLI.
            auto_resolution: cli.width.is_none() && cli.height.is_none(),
        }
    }

    /// Clamp obviously-invalid values to safe fallbacks so a stale or partial
    /// config can never zero out the buffer pool / frame size (which silently
    /// drops every frame). Belt-and-suspenders on top of the settings defaults.
    pub fn sanitized(mut self) -> Self {
        if self.width == 0 {
            self.width = 1280;
        }
        if self.height == 0 {
            self.height = 720;
        }
        if self.fps == 0 {
            self.fps = 30;
        }
        if self.buffers == 0 {
            self.buffers = 4;
        }
        self
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
            devices: self.devices,
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
