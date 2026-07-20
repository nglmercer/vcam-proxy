//! Linux sink: streams frames into a v4l2loopback device node.
//!
//! Kernel interface used (via the `v4l` crate):
//! - `VIDIOC_QUERYCAP` to detect loopback output devices and validate capabilities
//! - `VIDIOC_S_FMT` to negotiate width/height/pixelformat
//! - `VIDIOC_REQBUFS` + `mmap` for kernel-allocated, userspace-mapped buffers
//! - `VIDIOC_QBUF`/`VIDIOC_DQBUF` to cycle frames (`V4L2_BUF_TYPE_VIDEO_OUTPUT`)
//! - `VIDIOC_STREAMON`/`STREAMOFF` on start/stop (handled by the stream impl)
//!
//! Frame data lands directly in the kernel-mapped buffer -- the only copy is
//! `memcpy` from our pooled slot into the mmap region; nothing crosses a
//! syscall boundary per frame beyond QBUF/DQBUF ioctls.
//!
//! # Architecture
//!
//! This module is split into submodules by functional area:
//!
//! - [`distro`]: Distribution detection & auto-install commands
//! - [`discovery`]: Device enumeration & validation
//! - [`permissions`]: Permission checks for device access
//! - [`module`]: Module management types & operations
//! - [`scaling`]: Resolution scaling with pre-computed LUTs
//! - [`active`]: Internal streaming state for a device
//! - [`sink`]: Single-device sink implementation
//! - [`multi_sink`]: Multi-device sink for multi-reader mode

pub mod active;
pub mod distro;
pub mod discovery;
pub mod module;
pub mod module_ops;
pub mod multi_sink;
pub mod permissions;
pub mod scaling;
pub mod sink;

// Re-export main types for convenience
pub use module::count_loopback_devices;
pub use discovery::{DeviceInfo, discover_loopback_devices, find_loopback_device, is_loopback_driver, LoopbackError};
pub use module::{ensure_module_loaded_with_install, is_module_loaded, ModuleError};
pub use module_ops::{load_module_with_params, load_module_with_params_force, unload_module};
pub use permissions::{check_device_access, exclusive_caps_active, AccessError};
pub use sink::V4l2LoopSink;
pub use multi_sink::V4l2LoopMultiSink;
