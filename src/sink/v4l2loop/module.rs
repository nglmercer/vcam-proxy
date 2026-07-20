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
