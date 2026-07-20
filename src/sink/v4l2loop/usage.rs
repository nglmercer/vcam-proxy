//! Detect which processes are holding a loopback device node open.
//!
//! A v4l2loopback module reload (`modprobe -r`) fails with EBUSY while any
//! process keeps the `/dev/videoN` node open. To turn the generic
//! "Module is in use" error into an actionable message ("OBS (pid 1234) is
//! using the camera — close it"), we walk `/proc/<pid>/fd` and resolve each
//! symlink, matching it against the loopback device paths.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::debug;

/// One process currently holding a device node open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceUser {
    pub pid: i32,
    pub comm: String,
}

/// Resolve the target of a `/proc/<pid>/fd/<n>` symlink.
fn fd_target(pid: i32, fd: &str) -> Option<PathBuf> {
    let link = format!("/proc/{pid}/fd/{fd}");
    fs::read_link(&link).ok()
}

/// Read `/proc/<pid>/comm` for a human-readable process name.
fn comm_of(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// Find the processes that currently have `device` open.
///
/// `device` is compared against the symlink target of every `/proc/<pid>/fd/*`.
/// Returns an empty `Vec` when nothing matches or `/proc` is unavailable.
pub fn device_users(device: &Path) -> Vec<DeviceUser> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    let dev_str = device.to_string_lossy().into_owned();
    let mut users = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for proc in entries.flatten() {
        let pid_str = proc.file_name();
        let pid_str = pid_str.to_string_lossy();
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };

        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };

        for fd in fds.flatten() {
            let name = fd.file_name();
            let name = name.to_string_lossy();
            let Some(target) = fd_target(pid, &name) else {
                continue;
            };
            if target.to_string_lossy() == dev_str {
                if seen.insert(pid) {
                    users.push(DeviceUser {
                        pid,
                        comm: comm_of(pid),
                    });
                }
                break;
            }
        }
    }

    users
}

/// Return the current loopback devices together with the processes using each.
///
/// Used by the in-app device *manager* to show which virtual cameras are in use
/// and by the reload path to name the blockers that keep `modprobe -r` busy.
pub fn all_loopback_users() -> Vec<(crate::sink::v4l2loop::discovery::DeviceInfo, Vec<DeviceUser>)> {
    match crate::sink::v4l2loop::discovery::discover_loopback_devices() {
        Ok(devices) => devices
            .into_iter()
            .filter(|d| crate::sink::v4l2loop::is_loopback_driver(&d.driver))
            .map(|d| {
                let users = device_users(&d.path);
                if !users.is_empty() {
                    debug!(device = %d.path.display(), count = users.len(), "device in use");
                }
                (d, users)
            })
            .collect(),
        Err(e) => {
            debug!(error = %e, "could not enumerate loopback devices for usage scan");
            Vec::new()
        }
    }
}

/// Flattened list of every process using any loopback device.
pub fn all_loopback_user_pids() -> Vec<DeviceUser> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_dev, users) in all_loopback_users() {
        for u in users {
            if seen.insert(u.pid) {
                out.push(u);
            }
        }
    }
    out
}
