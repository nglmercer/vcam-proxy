//! Module operations: load, unload, and reload for v4l2loopback.

use std::io;
use std::process::Command;
use std::time::Duration;

use tracing::info;

use super::module::{count_loopback_devices, is_module_loaded, ModuleError};

/// Unload the v4l2loopback kernel module via pkexec.
pub fn unload_module() -> Result<(), ModuleError> {
    if !is_module_loaded() {
        return Ok(());
    }

    info!("unloading v4l2loopback module via pkexec");

    let result = Command::new("pkexec")
        .arg("modprobe")
        .arg("-r")
        .arg("v4l2loopback")
        .output();

    match result {
        Ok(output) if output.status.success() => {
            // Wait for module to actually unload
            for _ in 0..50 {
                if !is_module_loaded() {
                    info!("v4l2loopback module unloaded successfully");
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(ModuleError::LoadFailed {
                reason: "module still loaded after modprobe -r".into(),
            })
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ModuleError::LoadFailed {
                reason: format!(
                    "pkexec modprobe -r failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ModuleError::PkexecNotAvailable),
        Err(e) => Err(ModuleError::LoadFailed {
            reason: e.to_string(),
        }),
    }
}

/// Load the v4l2loopback kernel module with custom parameters via pkexec.
/// The `params` string is split on whitespace and passed as arguments to modprobe.
pub fn load_module_with_params(params: &str) -> Result<(), ModuleError> {
    load_module_with_params_internal(params, false)
}

/// Load the module, optionally forcing a reload if already loaded.
/// This is needed when the user changes the number of devices (e.g., enabling multi-reader mode).
pub fn load_module_with_params_force(params: &str) -> Result<(), ModuleError> {
    load_module_with_params_internal(params, true)
}

fn load_module_with_params_internal(params: &str, force_reload: bool) -> Result<(), ModuleError> {
    let already_loaded = is_module_loaded();

    if already_loaded && !force_reload {
        return Ok(());
    }

    if already_loaded && force_reload {
        info!("v4l2loopback module already loaded; reloading with new params: {params}");
        unload_module()?;
        std::thread::sleep(Duration::from_millis(200));
    }

    info!("v4l2loopback module not loaded; attempting auto-load via pkexec with params: {params}");

    let args: Vec<&str> = params.split_whitespace().collect();
    let result = Command::new("pkexec")
        .arg("modprobe")
        .arg("v4l2loopback")
        .args(&args)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            for _ in 0..50 {
                if is_module_loaded() && count_loopback_devices() > 0 {
                    info!("v4l2loopback module loaded successfully");
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if is_module_loaded() {
                info!("v4l2loopback module loaded (devices may still be creating)");
                Ok(())
            } else {
                Err(ModuleError::LoadFailed {
                    reason: "modprobe reported success but module not visible in /proc/modules"
                        .into(),
                })
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ModuleError::LoadFailed {
                reason: format!(
                    "pkexec modprobe failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ModuleError::PkexecNotAvailable),
        Err(e) => Err(ModuleError::LoadFailed {
            reason: e.to_string(),
        }),
    }
}
