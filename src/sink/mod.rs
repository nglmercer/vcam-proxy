//! Virtual-camera sink backends.
//!
//! - Linux: `v4l2loop` — mmap streaming into a v4l2loopback device node.
//! - Windows: `winpipe` — overlapped named-pipe IPC to a virtual-camera
//!   driver/filter (wire protocol documented at the top of `winpipe.rs`).
//! - Anywhere: `null` — discards frames (benchmarking / dry runs).

use std::io;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::config::{ResolvedConfig, SinkKind};
use crate::frame::Frame;

pub mod null;
#[cfg(target_os = "linux")]
pub mod v4l2loop;
#[cfg(target_os = "windows")]
pub mod winpipe;

// Re-export the Linux discovery utilities for use from main.
#[cfg(target_os = "linux")]
pub use v4l2loop::{
    all_loopback_users, capture_single_streamer, check_device_access,
    count_loopback_devices, device_users, discover_loopback_devices,
    ensure_module_loaded_with_install, exclusive_caps_active, find_loopback_device,
    is_loopback_driver, is_module_loaded, load_module_with_params_force, max_openers,
    module_version, DeviceUser, AccessError, LoopbackError, ModuleError,
};

pub trait Sink: Send {
    /// Write one frame. `WouldBlock` / `TimedOut` may still surface when the
    /// kernel output queue cannot accept another buffer (rare with
    /// v4l2loopback); any other error triggers device re-initialization on
    /// the next frame.
    fn write(&mut self, frame: &Frame) -> io::Result<()>;
    fn describe(&self) -> String;
}

pub fn build_with_path(cfg: &ResolvedConfig, path: &Path) -> Box<dyn Sink> {
    let kind = match cfg.sink {
        SinkKind::Auto => {
            #[cfg(target_os = "linux")]
            {
                SinkKind::V4l2
            }
            #[cfg(target_os = "windows")]
            {
                SinkKind::Pipe
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                SinkKind::Null
            }
        }
        k => k,
    };

    match kind {
        SinkKind::Null => Box::new(null::NullSink::default()),
        SinkKind::V4l2 => {
            #[cfg(target_os = "linux")]
            {
                Box::new(v4l2loop::V4l2LoopSink::with_timeout(
                    path.to_path_buf(),
                    cfg.timeout,
                ))
            }
            #[cfg(not(target_os = "linux"))]
            {
                warn!("v4l2 sink requested on non-Linux; using null sink");
                Box::new(null::NullSink::default())
            }
        }
        SinkKind::Pipe => {
            #[cfg(target_os = "windows")]
            {
                Box::new(winpipe::PipeSink::new(&cfg.pipe_name))
            }
            #[cfg(not(target_os = "windows"))]
            {
                warn!("pipe sink requested on non-Windows; using null sink");
                Box::new(null::NullSink::default())
            }
        }
        SinkKind::Auto => unreachable!("auto resolved above"),
    }
}

/// Build a sink that writes to *all* provided loopback device paths. This is the
/// multi-node path: with `devices >= 2` under v4l2loopback, each node must
/// receive the same frames or the extras appear as dead/black cameras.
pub fn build_multi_with_paths(paths: Vec<PathBuf>, timeout_ms: u32) -> Box<dyn Sink> {
    #[cfg(target_os = "linux")]
    {
        Box::new(v4l2loop::V4l2LoopMultiSink::with_timeout(paths, timeout_ms))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = timeout_ms;
        warn!("multi-device v4l2 sink requested on non-Linux; using null sink");
        Box::new(null::NullSink::default())
    }
}
