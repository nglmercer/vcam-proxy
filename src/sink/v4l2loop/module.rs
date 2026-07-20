//! Kernel module management for v4l2loopback.

use std::fs;

use tracing::warn;

use super::discovery::discover_loopback_devices;
use super::distro::install_v4l2loopback;
use super::is_loopback_driver;
use super::module_ops::load_module_with_params;

/// Check if the v4l2loopback kernel module is currently loaded.
pub fn is_module_loaded() -> bool {
    fs::read_to_string("/proc/modules")
        .map(|c| c.lines().any(|line| line.starts_with("v4l2loopback ")))
        .unwrap_or(false)
}

/// Parse a v4l2loopback version string ("0.15.3", "0.15.3-dirty") into a tuple.
fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let mut parts = raw.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // Patch may carry a suffix ("3-dirty", "3rc1") — take leading digits only.
    let patch_raw = parts.next()?;
    let patch_digits: String = patch_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

/// Version of the currently loaded v4l2loopback module, if readable.
pub fn module_version() -> Option<(u32, u32, u32)> {
    let raw = fs::read_to_string("/sys/module/v4l2loopback/version").ok()?;
    parse_version(&raw)
}

/// First driver release with the exclusive stream-token model.
pub const SINGLE_STREAMER_SINCE: (u32, u32, u32) = (0, 14, 0);

/// Whether the loaded driver allows only ONE streaming reader per device node.
///
/// v4l2loopback ≥ 0.14 grants the CAPTURE stream token to a single opener:
/// the first app to stream from a node owns it, and every additional reader
/// fails with EBUSY ("Device or resource busy") from VIDIOC_REQBUFS/read().
/// Releases ≤ 0.13 broadcast frames to any number of concurrent readers on
/// one node. Returns `None` when the version can't be determined.
pub fn capture_single_streamer() -> Option<bool> {
    module_version().map(|v| v >= SINGLE_STREAMER_SINCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_versions() {
        assert_eq!(parse_version("0.15.3"), Some((0, 15, 3)));
        assert_eq!(parse_version("0.12.7\n"), Some((0, 12, 7)));
        assert_eq!(parse_version("0.14.0"), Some((0, 14, 0)));
    }

    #[test]
    fn parses_suffixed_patch() {
        assert_eq!(parse_version("0.15.3-dirty"), Some((0, 15, 3)));
        assert_eq!(parse_version("1.0.0rc1"), Some((1, 0, 0)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("0.15"), None);
        assert_eq!(parse_version("a.b.c"), None);
    }

    #[test]
    fn single_streamer_threshold() {
        assert!((0, 15, 3) >= SINGLE_STREAMER_SINCE);
        assert!((0, 14, 0) >= SINGLE_STREAMER_SINCE);
        assert!((0, 13, 99) < SINGLE_STREAMER_SINCE);
        assert!((0, 12, 7) < SINGLE_STREAMER_SINCE);
    }
}

/// Count how many v4l2loopback devices currently exist in /dev.
pub fn count_loopback_devices() -> usize {
    match discover_loopback_devices() {
        Ok(devices) => devices
            .iter()
            .filter(|d| is_loopback_driver(&d.driver))
            .count(),
        Err(_) => 0,
    }
}

/// Error type for module management operations.
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("module load failed: {reason}")]
    LoadFailed { reason: String },
    #[error("pkexec not available; run manually: sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1")]
    PkexecNotAvailable,
    #[error(
        "cannot auto-install: unsupported Linux distribution. Install v4l2loopback-dkms manually"
    )]
    DistroNotSupported,
    #[error("package install failed (exit code {0}); check network and try installing v4l2loopback-dkms manually")]
    InstallFailed(i32),
}

/// Ensure the v4l2loopback module is loaded, installing the package first if
/// it cannot be found by `modprobe`. Flow:
/// 1. Check `/proc/modules` — if already loaded, return immediately.
/// 2. Try `modprobe v4l2loopback …` via pkexec.
/// 3. If modprobe failed because the module isn't installed, attempt
///    [`install_v4l2loopback`] and retry modprobe once.
pub fn ensure_module_loaded_with_install(params: &str) -> Result<(), ModuleError> {
    if is_module_loaded() {
        return Ok(());
    }

    // First attempt: modprobe (module may already be built but unloaded).
    match load_module_with_params(params) {
        Ok(()) => return Ok(()),
        Err(_) => {
            // Module likely not installed at all → try to install it.
            warn!("modprobe failed; v4l2loopback may not be installed");
        }
    }

    // Auto-install and retry.
    install_v4l2loopback()?;
    load_module_with_params(params)
}
