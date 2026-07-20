//! Virtual-camera sink backends.
//!
//! - Linux: `v4l2loop` — mmap streaming into a v4l2loopback device node.
//! - Windows: `winpipe` — overlapped named-pipe IPC to a virtual-camera
//!   driver/filter (wire protocol documented at the top of `winpipe.rs`).
//! - Anywhere: `null` — discards frames (benchmarking / dry runs).

use std::io;
use std::path::Path;

use tracing::warn;

use crate::config::{Config, SinkKind};
use crate::frame::Frame;

pub mod null;
#[cfg(target_os = "linux")]
pub mod v4l2loop;
#[cfg(target_os = "windows")]
pub mod winpipe;

// Re-export the Linux discovery utilities for use from main.
#[cfg(target_os = "linux")]
pub use v4l2loop::{
    check_device_access, discover_loopback_devices, find_loopback_device, is_module_loaded,
    load_module,
};
#[cfg(target_os = "linux")]
pub use v4l2loop::{AccessError, LoopbackError, ModuleError};

pub trait Sink: Send {
    /// Write one frame. `WouldBlock` signals "no consumer attached / reader
    /// behind" and is counted as a graceful drop by the caller; any other
    /// error triggers device re-initialization on the next frame.
    fn write(&mut self, frame: &Frame) -> io::Result<()>;
    fn describe(&self) -> String;
}

pub fn build_with_path(cfg: &Config, path: &Path) -> Box<dyn Sink> {
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
                Box::new(v4l2loop::V4l2LoopSink::new(path.to_path_buf()))
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
