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
pub mod discovery;
pub mod distro;
pub mod module;
pub mod module_ops;
pub mod multi_sink;
pub mod permissions;
pub mod scaling;
pub mod sink;
pub mod usage;

// Re-export main types for convenience
pub use discovery::{
    discover_loopback_devices, find_loopback_device, is_loopback_driver, LoopbackError,
};
pub use module::count_loopback_devices;
pub use module::{
    capture_single_streamer, ensure_module_loaded_with_install, is_module_loaded, module_version,
    ModuleError,
};
pub use usage::{all_loopback_users, all_loopback_user_pids, device_users, DeviceUser};
pub use module_ops::load_module_with_params_force;
pub use multi_sink::V4l2LoopMultiSink;
pub use permissions::{check_device_access, exclusive_caps_active, max_openers, AccessError};
pub use sink::V4l2LoopSink;
