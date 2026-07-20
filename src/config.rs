//! Command-line surface — one-shot utilities only.
//!
//! All runtime configuration lives in `~/.config/vcam-proxy/config.toml` (or the
//! Settings GUI). There are intentionally **no** override flags: editing the
//! config file (or the in-app manager) is the only way to change behaviour, so
//! the active configuration is always reproducible and persisted.
//!
//! A plain `vcam-proxy` / `cargo run` loads the config and starts. The flags
//! below are diagnostic/utility actions that exit immediately.

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "vcam-proxy",
    version,
    about = "Physical camera -> virtual loopback proxy (configure via ~/.config/vcam-proxy/config.toml)",
    after_help = "All options live in ~/.config/vcam-proxy/config.toml or the tray Settings window.\n\
                  Just run:  cargo run   /   vcam-proxy"
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

    /// Save current settings to the config file and exit.
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
    /// Seconds to keep retrying a module reload when the virtual camera is
    /// busy (another app is using it). `0` disables the wait and falls back
    /// to single-node immediately. Only relevant when the device count must
    /// change (multi-app mode / editing devices via the manager).
    pub multi_app_timeout: u32,
}

impl ResolvedConfig {
    /// Build a resolved config purely from the persisted settings file.
    ///
    /// There are no CLI overrides by design: the config file (or the in-app
    /// manager) is the single source of truth, so the active configuration is
    /// always reproducible.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        // An explicit `image` pins geometry; otherwise honor auto_resolution.
        let auto_resolution = if settings.image.is_some() {
            false
        } else {
            settings.auto_resolution
        };

        Self {
            camera: settings.camera,
            sink: SinkKind::Auto,
            device: settings.device.clone(),
            pipe_name: "vcam_proxy_0".to_string(),
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            format: settings.format,
            buffers: settings.buffers,
            retry_ms: settings.retry_ms,
            multi_reader: settings.multi_reader,
            devices: settings.devices,
            exclusive_caps: settings.exclusive_caps,
            timeout: settings.timeout,
            auto_load_module: settings.auto_load_module,
            auto_resolution,
            image: settings.image.clone(),
            multi_app_timeout: settings.multi_app_timeout,
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
            image: self.image.clone(),
            multi_app_timeout: self.multi_app_timeout,
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
