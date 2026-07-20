//! Command-line surface — optional overrides only.
//!
//! Prefer `~/.config/vcam-proxy/config.toml` (or the Settings GUI). CLI flags
//! exist for one-shot utilities (`--list`, `--setup`) and rare overrides.
//! A plain `vcam-proxy` / `cargo run` uses config defaults with every feature on.

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "vcam-proxy",
    version,
    about = "Physical camera -> virtual loopback proxy (configure via ~/.config/vcam-proxy/config.toml)",
    after_help = "Tip: edit ~/.config/vcam-proxy/config.toml or use the tray Settings window.\n\
                  Most runs need no flags:  cargo run   /   vcam-proxy"
)]
pub struct Config {
    /// Enumerate capture devices and exit.
    #[arg(long)]
    pub list: bool,

    /// List available v4l2loopback output devices and exit.
    #[arg(long)]
    pub list_loopback: bool,

    /// Auto-setup: check system, load module, fix permissions, then exit.
    #[arg(long)]
    pub setup: bool,

    /// Save current effective settings to the config file and exit.
    #[arg(long)]
    pub save_config: bool,

    /// Open the config file in the default editor.
    #[arg(long)]
    pub edit_config: bool,

    /// Show the current settings and their source, then exit.
    #[arg(long)]
    pub show_config: bool,

    /// Test capture without writing to the loopback device.
    #[arg(long)]
    pub dry_run: bool,

    /// Force the settings window open on startup.
    #[arg(long)]
    pub settings: bool,

    /// Run headless (no settings GUI).
    #[arg(long)]
    pub no_gui: bool,

    /// Disable the system-tray icon.
    #[arg(long)]
    pub no_tray: bool,

    /// Feed a still image instead of a webcam (demos / tests).
    #[arg(long, value_name = "PATH")]
    pub image: Option<String>,

    // ---- Optional overrides (prefer config.toml) ----

    /// Override: camera index.
    #[arg(short, long)]
    pub camera: Option<u32>,

    /// Override: sink backend.
    #[arg(long, value_enum)]
    pub sink: Option<SinkKind>,

    /// Override: loopback device node.
    #[arg(short, long)]
    pub device: Option<String>,

    /// Override: named-pipe name (Windows).
    #[arg(long)]
    pub pipe_name: Option<String>,

    /// Override: capture width (also disables auto_resolution).
    #[arg(long)]
    pub width: Option<u32>,

    /// Override: capture height (also disables auto_resolution).
    #[arg(long)]
    pub height: Option<u32>,

    /// Override: capture frame rate.
    #[arg(long)]
    pub fps: Option<u32>,

    /// Override: wire format policy.
    #[arg(long, value_enum)]
    pub format: Option<FormatPref>,

    /// Override: ring buffer depth.
    #[arg(long)]
    pub buffers: Option<usize>,

    /// Override: camera re-open backoff (ms).
    #[arg(long)]
    pub retry_ms: Option<u64>,

    /// Override: auto-load v4l2loopback when missing (`true`/`false`).
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub auto_load_module: Option<bool>,

    /// Override: multi-reader mode (`true`/`false`).
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub multi_reader: Option<bool>,

    /// Override: number of v4l2loopback nodes.
    #[arg(long)]
    pub devices: Option<u32>,

    /// Override: exclusive_caps (0 or 1).
    #[arg(long)]
    pub exclusive_caps: Option<u32>,

    /// Override: v4l2loopback timeout ms (`0` = keep last frame).
    #[arg(long)]
    pub timeout: Option<u32>,
}

/// Resolved configuration: config file + optional CLI overrides.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub camera: u32,
    pub sink: SinkKind,
    pub device: String,
    #[allow(dead_code)]
    pub pipe_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: FormatPref,
    pub buffers: usize,
    pub retry_ms: u64,
    pub multi_reader: bool,
    pub devices: u32,
    pub exclusive_caps: u32,
    pub timeout: u32,
    pub auto_load_module: bool,
    /// When true, capture negotiates the camera's highest supported mode.
    pub auto_resolution: bool,
    pub image: Option<String>,
}

impl ResolvedConfig {
    /// Merge CLI overrides over settings-file values.
    pub fn from_cli_and_settings(cli: &Config, settings: &crate::settings::Settings) -> Self {
        let image = cli.image.clone();
        // Explicit --width/--height or --image pins geometry; otherwise honor
        // the config's auto_resolution flag (default true).
        let auto_resolution = if image.is_some() || cli.width.is_some() || cli.height.is_some() {
            false
        } else {
            settings.auto_resolution
        };

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
            auto_load_module: cli.auto_load_module.unwrap_or(settings.auto_load_module),
            auto_resolution,
            image,
        }
    }

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
            auto_load_module: self.auto_load_module,
            auto_resolution: self.auto_resolution,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkKind {
    Auto,
    V4l2,
    Pipe,
    Null,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FormatPref {
    #[default]
    Auto,
    Yuy2,
    Rgb24,
    Nv12,
    Mjpeg,
}
