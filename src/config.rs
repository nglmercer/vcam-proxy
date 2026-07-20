//! Command-line surface. Every knob has a sane default so the binary works
//! with zero arguments: camera 0 -> /dev/video10 (Linux) at 720p30.

use clap::{Parser, ValueEnum};

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
    #[arg(short, long, default_value_t = 0)]
    pub camera: u32,

    /// Sink backend. `auto` = v4l2 on Linux, pipe on Windows.
    #[arg(long, value_enum, default_value_t = SinkKind::Auto)]
    pub sink: SinkKind,

    /// Loopback device node (Linux, created by v4l2loopback).
    #[arg(short, long, default_value = "/dev/video10")]
    pub device: String,

    /// Named-pipe name (Windows): frames go to \\.\pipe\<name>.
    #[arg(long, default_value = "vcam_proxy_0")]
    pub pipe_name: String,

    /// Requested capture width.
    #[arg(long, default_value_t = 1280)]
    pub width: u32,

    /// Requested capture height.
    #[arg(long, default_value_t = 720)]
    pub height: u32,

    /// Requested capture frame rate.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,

    /// Wire format policy. `auto` passes YUYV through untouched (zero
    /// conversion) and decodes MJPEG/NV12 sources to RGB24.
    #[arg(long, value_enum, default_value_t = FormatPref::Auto)]
    pub format: FormatPref,

    /// Ring depth: number of reusable frame buffers in circulation.
    #[arg(long, default_value_t = 4)]
    pub buffers: usize,

    /// Backoff between camera re-open attempts after a failure, ms.
    #[arg(long, default_value_t = 1000)]
    pub retry_ms: u64,

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
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkKind {
    Auto,
    V4l2,
    Pipe,
    /// Discard frames; useful for benchmarking the capture side alone.
    Null,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatPref {
    Auto,
    Yuy2,
    Rgb24,
    Nv12,
    /// Compressed passthrough (Linux sink only).
    Mjpeg,
}
