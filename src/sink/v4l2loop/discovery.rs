//! Device discovery & validation for v4l2loopback devices.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// One discovered video device with its metadata.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Device node path (e.g. `/dev/video0`).
    pub path: PathBuf,
    /// Human-readable card label from the driver.
    pub card: String,
    /// Kernel driver name. For physical devices this comes from the sysfs
    /// `device/driver` symlink (e.g. `uvcvideo`); for virtual devices such as
    /// v4l2loopback — which register no sysfs driver link — it is queried
    /// directly from the driver via `VIDIOC_QUERYCAP` (`"v4l2 loopback"`).
    pub driver: String,
}

/// Extract the numeric suffix of a `/dev/videoN` node for natural ordering
/// (so `/dev/video2` sorts before `/dev/video10`).
fn device_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| s.strip_prefix("video"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Ask the driver itself for its name via `VIDIOC_QUERYCAP`.
///
/// v4l2loopback devices live under `/sys/devices/virtual/...` and have **no**
/// `device/driver` symlink, so sysfs-only detection sees them as `driver=""`
/// and misidentifies them. The kernel fills the querycap `driver` field with
/// `"v4l2 loopback"`, which is authoritative.
fn query_driver_via_ioctl(dev_path: &Path) -> Option<String> {
    let dev = v4l::device::Device::with_path(dev_path).ok()?;
    let caps = dev.query_caps().ok()?;
    Some(caps.driver)
}

/// Resolve the driver name for a device: sysfs symlink first (cheap, works
/// for physical devices), `VIDIOC_QUERYCAP` as the fallback for virtual ones.
fn driver_for(sysfs_dir: &Path, dev_path: &Path) -> String {
    if let Ok(driver_link) = fs::read_link(sysfs_dir.join("device/driver")) {
        if let Some(driver_name) = driver_link.file_name() {
            return driver_name.to_string_lossy().into_owned();
        }
    }
    query_driver_via_ioctl(dev_path).unwrap_or_default()
}

/// Whether `dev_path` is driven by the v4l2loopback kernel module.
pub fn device_is_loopback(dev_path: &Path) -> bool {
    let sysfs_name = dev_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let sysfs_dir = PathBuf::from(format!("/sys/class/video4linux/{sysfs_name}"));
    if sysfs_dir.exists() {
        is_loopback_driver(&driver_for(&sysfs_dir, dev_path))
    } else {
        // No sysfs entry (unusual): ask the driver directly.
        query_driver_via_ioctl(dev_path)
            .map(|d| is_loopback_driver(&d))
            .unwrap_or(false)
    }
}

/// Enumerate all video devices under `/sys/class/video4linux`.
/// Devices without a matching `/dev/video*` node are skipped.
pub fn discover_loopback_devices() -> Result<Vec<DeviceInfo>, LoopbackError> {
    let sysfs_path = Path::new("/sys/class/video4linux");
    let contents =
        fs::read_dir(sysfs_path).map_err(|source| LoopbackError::ScanFailed { source })?;

    let mut devices = Vec::new();
    for entry in contents {
        let entry = entry.map_err(|source| LoopbackError::ScanFailed { source })?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("video") {
            continue;
        }

        let device_path = PathBuf::from(format!("/dev/{name_str}"));
        if !device_path.exists() {
            continue;
        }

        let mut card = String::new();
        if let Ok(name) = fs::read_to_string(entry.path().join("name")) {
            card = name.trim().to_string();
        }

        let driver = driver_for(&entry.path(), &device_path);

        devices.push(DeviceInfo {
            path: device_path,
            card,
            driver,
        });
    }

    // Natural numeric order: video0, video1, video2, ..., video10.
    devices.sort_by_key(|d| device_number(&d.path));
    Ok(devices)
}

/// Check whether a device is driven by the v4l2loopback kernel module.
pub fn is_loopback_driver(driver: &str) -> bool {
    // The sysfs driver link reports "v4l2loopback" while VIDIOC_QUERYCAP
    // reports "v4l2 loopback" (with a space); accept both.
    driver == "v4l2loopback" || driver == "v4l2 loopback"
}

/// Find a specific loopback device by path, or detect one automatically.
///
/// **Never** falls back to a non-loopback node: opening a physical camera
/// (uvcvideo) as an output device makes every write fail (black virtual
/// camera) and can disturb the real camera — a silent failure mode that is
/// far worse than a clean "no device found" error.
pub fn find_loopback_device(preferred: &Path) -> Result<PathBuf, LoopbackError> {
    // 1. If a preferred device was requested and exists, validate it.
    if !preferred.as_os_str().is_empty() && preferred.exists() {
        if device_is_loopback(preferred) {
            return Ok(preferred.to_path_buf());
        }

        warn!(
            path = %preferred.display(),
            "preferred device is not a v4l2loopback node; scanning for alternatives"
        );
    }

    // 2. Scan all /dev/video* for a real loopback device.
    let all_devices = discover_loopback_devices()?;

    if let Some(dev) = all_devices
        .iter()
        .find(|d| is_loopback_driver(&d.driver))
    {
        info!(device = %dev.path.display(), card = %dev.card, "auto-detected v4l2loopback device");
        return Ok(dev.path.clone());
    }

    // 3. No loopback device anywhere — report cleanly so the caller can guide
    //    the user (modprobe / --auto-load-module) instead of writing frames
    //    into the wrong device.
    Err(LoopbackError::NoDeviceFound)
}

/// Error type for loopback device discovery operations.
#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("no v4l2loopback virtual camera device found on this system")]
    NoDeviceFound,
    #[error("error scanning /dev/video*: {source}")]
    ScanFailed { source: io::Error },
}
