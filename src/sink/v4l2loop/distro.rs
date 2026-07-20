//! Distribution detection & auto-install for v4l2loopback.

use std::fs;
use std::path::Path;
use std::process::Command;

use tracing::info;

use super::ModuleError;

/// Known Linux package-manager families we can auto-install for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Ubuntu,
    Fedora,
    Arch,
    OpenSuse,
    Unknown,
}

/// Read `/etc/os-release` and map the `ID` (and fallback `ID_LIKE`) to a
/// [`Distro`] variant. Returns [`Distro::Unknown`] if the file is missing or
/// contains an unrecognised ID.
pub fn detect_distro() -> Distro {
    let Ok(contents) = fs::read_to_string("/etc/os-release") else {
        return Distro::Unknown;
    };

    let mut id = "";
    let mut id_like = "";
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"');
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = v.trim_matches('"');
        }
    }

    match id {
        "debian" => Distro::Debian,
        "ubuntu" => Distro::Ubuntu,
        "fedora" => Distro::Fedora,
        "arch" => Distro::Arch,
        "opensuse" | "opensuse-tumbleweed" | "opensuse-leap" => Distro::OpenSuse,
        _ => {
            // Fall back to ID_LIKE (e.g. "ubuntu" for Pop!_OS, "debian" for Mint).
            if id_like.contains("debian") || id_like.contains("ubuntu") {
                Distro::Debian
            } else if id_like.contains("fedora") || id_like.contains("rhel") {
                Distro::Fedora
            } else if id_like.contains("arch") {
                Distro::Arch
            } else {
                Distro::Unknown
            }
        }
    }
}

/// Build the `pkexec <pm> install …` command for the detected distro.
/// Returns `None` when we don't recognise the distro.
pub fn install_command(distro: Distro) -> Option<(String, Vec<String>)> {
    match distro {
        Distro::Debian | Distro::Ubuntu => Some((
            "apt-get".to_string(),
            vec![
                "install".into(),
                "-y".into(),
                "v4l2loopback-dkms".into(),
                "v4l-utils".into(),
            ],
        )),
        Distro::Fedora => Some((
            "dnf".to_string(),
            vec!["install".into(), "-y".into(), "v4l2loopback".into()],
        )),
        Distro::Arch => Some((
            "pacman".into(),
            vec![
                "-S".into(),
                "--noconfirm".into(),
                "v4l2loopback-dkms".into(),
                "v4l-utils".into(),
            ],
        )),
        Distro::OpenSuse => Some((
            "zypper".into(),
            vec!["install".into(), "-y".into(), "v4l2loopback".into()],
        )),
        Distro::Unknown => None,
    }
}

/// Attempt to install the v4l2loopback DKMS package through the native package
/// manager, escalating via `pkexec`. This covers the case where `modprobe`
/// fails because the module was never built/installed, not just unloaded.
///
/// Returns:
/// - `Ok(())` if the package manager reported success.
/// - `Err(ModuleError::InstallFailed)` on a non-zero exit.
/// - `Err(ModuleError::PkexecNotAvailable)` if `pkexec` is missing.
/// - `Err(ModuleError::DistroNotSupported)` when we cannot map the distro.
pub fn install_v4l2loopback() -> Result<(), ModuleError> {
    let distro = detect_distro();
    let (pm, args) = install_command(distro).ok_or(ModuleError::DistroNotSupported)?;

    info!(?distro, package_manager = %pm, "attempting v4l2loopback package install");

    // pkexec gives us a GUI polkit prompt; fall back to sudo if pkexec is
    // unavailable (headless server, minimal desktop).
    let escalator = if Path::new("/usr/bin/pkexec").exists() {
        "pkexec"
    } else if Path::new("/usr/bin/sudo").exists() {
        "sudo"
    } else {
        return Err(ModuleError::PkexecNotAvailable);
    };

    let status = Command::new(escalator)
        .arg(&pm)
        .args(&args)
        .status()
        .map_err(|e| ModuleError::LoadFailed {
            reason: format!("failed to invoke {escalator} {pm}: {e}"),
        })?;

    if status.success() {
        info!("v4l2loopback package installed successfully");
        Ok(())
    } else {
        Err(ModuleError::InstallFailed(status.code().unwrap_or(-1)))
    }
}
