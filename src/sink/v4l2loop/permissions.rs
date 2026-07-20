//! Permission validation for v4l2loopback device nodes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Check if we can read/write the given video device.
/// Returns Ok(()) or an Err with an actionable suggestion.
pub fn check_device_access(path: &Path) -> Result<(), AccessError> {
    if !path.exists() {
        return Err(AccessError::NotFound {
            path: path.to_path_buf(),
        });
    }

    // Try opening for read+write (loopback output needs write access)
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // Check if user is in 'video' group
            let in_video_group = std::process::Command::new("groups")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("video"))
                .unwrap_or(false);

            if in_video_group {
                Err(AccessError::PermissionDenied {
                    path: path.to_path_buf(),
                    suggestion: "User is in 'video' group but device is not writable. \
                                 Try: sudo chmod 0660 {} OR relogin"
                        .into(),
                })
            } else {
                Err(AccessError::PermissionDenied {
                    path: path.to_path_buf(),
                    suggestion: format!(
                        "Run: sudo usermod -aG video $USER\n\
                         Then LOG OUT and log back in for the group change to take effect.\n\
                         (Temporary: sudo chmod 0660 {})",
                        path.display()
                    ),
                })
            }
        }
        Err(e) => Err(AccessError::Other {
            source: e,
            path: path.to_path_buf(),
        }),
    }
}

/// Whether the current process has exclusive_caps enabled on any loopback device.
/// Browsers refuse devices that advertise both CAPTURE and OUTPUT; exclusive
/// caps is what makes the virtual node look like a real webcam.
pub fn exclusive_caps_active() -> Option<bool> {
    let raw = fs::read_to_string("/sys/module/v4l2loopback/parameters/exclusive_caps").ok()?;
    // Format is "Y,N,N,..." or "1,0,0,..." depending on kernel/module version.
    let first = raw.split(',').next()?.trim();
    Some(matches!(first, "Y" | "y" | "1"))
}

/// Read the current v4l2loopback `max_openers` module parameter (first device).
///
/// `max_openers` controls how many file descriptors can open the loopback
/// device simultaneously. With `max_openers < 2` only one app can read the
/// virtual camera at a time — the writer (vcam-proxy) counts as one opener,
/// so at least 2 are needed for a single reader, and more for multi-reader.
///
/// Returns `None` if the module isn't loaded or the parameter can't be read.
pub fn max_openers() -> Option<u32> {
    let raw = fs::read_to_string("/sys/module/v4l2loopback/parameters/max_openers").ok()?;
    // Array parameter: one value per device, comma-separated (e.g. "16,16,16").
    let first = raw.split(',').next()?.trim();
    first.parse().ok()
}

/// Error type for device access validation.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error("device {} does not exist", path.display())]
    NotFound { path: PathBuf },
    #[error("permission denied on {}. {}", path.display(), suggestion)]
    PermissionDenied { path: PathBuf, suggestion: String },
    #[error("cannot access device {path}: {source}")]
    Other { source: io::Error, path: PathBuf },
}
