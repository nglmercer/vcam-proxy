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
    /// Kernel driver name (e.g. `v4l2loopback`).
    pub driver: String,
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
        let mut driver = String::new();

        if let Ok(name) = fs::read_to_string(entry.path().join("name")) {
            card = name.trim().to_string();
        }

        if let Ok(driver_link) = fs::read_link(entry.path().join("device/driver")) {
            if let Some(driver_name) = driver_link.file_name() {
                driver = driver_name.to_string_lossy().into_owned();
            }
        }

        devices.push(DeviceInfo {
            path: device_path,
            card,
            driver,
        });
    }

    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

/// Check whether a device is driven by the v4l2loopback kernel module.
pub fn is_loopback_driver(driver: &str) -> bool {
    // Kernel reports "v4l2 loopback" (with space) on most builds, but we
    // normalize to "v4l2loopback" for matching.
    driver == "v4l2loopback" || driver == "v4l2 loopback"
}

/// Find a specific loopback device by path, or detect one automatically.
pub fn find_loopback_device(preferred: &Path) -> Result<PathBuf, LoopbackError> {
    // 1. If a preferred device was requested, validate it
    if preferred != Path::new("") && preferred.exists() {
        // Verify it's actually a loopback device
        let sysfs_name = preferred.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let sysfs_path = format!("/sys/class/video4linux/{sysfs_name}/device/driver");

        if let Ok(driver_link) = fs::read_link(&sysfs_path) {
            if let Some(driver_name) = driver_link.file_name() {
                let driver = driver_name.to_string_lossy();
                if is_loopback_driver(&driver) {
                    return Ok(preferred.to_path_buf());
                }
            }
        }

        warn!(
            path = %preferred.display(),
            "preferred device is not a loopback/output node; scanning for alternatives"
        );
    }

    // 2. Scan all /dev/video* for loopback devices
    let all_devices = discover_loopback_devices()?;

    // 3. Prefer v4l2loopback devices (driver name may include a space).
    let loopback_devices: Vec<_> = all_devices
        .iter()
        .filter(|d| is_loopback_driver(&d.driver))
        .cloned()
        .collect();

    if let Some(dev) = loopback_devices.first() {
        info!(device = %dev.path.display(), card = %dev.card, "auto-detected v4l2loopback device");
        return Ok(dev.path.clone());
    }

    // 4. Fall back to any output-capable device
    if let Some(dev) = all_devices.first() {
        warn!(
            device = %dev.path.display(), driver = %dev.driver,
            "no v4l2loopback device found; using alternative (may not work with all apps)"
        );
        return Ok(dev.path.clone());
    }

    // 5. Nothing available
    Err(LoopbackError::NoDeviceFound)
}

/// Error type for loopback device discovery operations.
#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("no video output device found on this system")]
    NoDeviceFound,
    #[error("error scanning /dev/video*: {source}")]
    ScanFailed { source: io::Error },
}
