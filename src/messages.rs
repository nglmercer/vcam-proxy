//! User-facing log / stderr message templates.
//!
//! Keep long guidance strings here so call sites stay short and consistent.

use std::path::Path;

use crate::sink::DeviceUser;

/// Format the list of processes holding a loopback device open.
fn format_blockers(users: &[DeviceUser]) -> String {
    let mut s = String::new();
    for u in users {
        s.push_str(&format!("  - {} (pid {})\n", u.comm, u.pid));
    }
    s
}

/// Print the processes currently holding the virtual camera open (the reason a
/// module reload is blocked). Empty list → nothing printed.
pub fn note_blockers(users: &[DeviceUser]) {
    if users.is_empty() {
        return;
    }
    eprintln!(
        "The virtual camera is in use by {} process(es):\n{}",
        users.len(),
        format_blockers(users)
    );
}

/// Message shown while retrying a reload that is blocked by busy devices.
pub fn warn_multi_node_busy(users: &[DeviceUser], remaining: u64, module_params: &str) {
    eprintln!(
        "⚠ Multi-app mode needs a module reload, but the virtual camera is busy.\n\
         \n\
         Close these app(s) to continue (auto-retrying, ~{remaining}s left):\n\
         {}\
         \n\
         Or run manually:\n\
           sudo modprobe -r v4l2loopback\n\
           sudo modprobe v4l2loopback {module_params}",
        format_blockers(users)
    );
}

/// Default `modprobe` argument string when no ResolvedConfig is available yet.
///
/// NOTE: `timeout` is intentionally absent — it is not a v4l2loopback module
/// parameter on driver ≥ 0.14 (only a runtime control, which we set via ioctl).
pub const DEFAULT_MODULE_PARAMS: &str = "exclusive_caps=1 card_label=vcam-proxy devices=1 \
video_nr=10 max_buffers=4 max_openers=16";

pub const MODPROBE_HINT: &str =
    "sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1 video_nr=10";

pub const CONFIG_PATH_HINT: &str = "~/.config/vcam-proxy/config.toml";

// ---- short tracing message keys (stable, grep-friendly) ----

pub const LOG_CREATED_DEFAULT_CONFIG: &str =
    "created default config (edit this instead of using CLI flags)";
pub const LOG_CONFIG_CREATE_FAILED: &str = "could not create default config file";
pub const LOG_FIRST_RUN_SETTINGS: &str =
    "first run: opening settings window — save changes to config.toml";
pub const LOG_THREADS_STOPPED: &str = "all threads stopped; descriptors released";
pub const LOG_STARTING: &str = "starting vcam-proxy";
pub const LOG_DRY_RUN: &str = "dry-run mode: capture only, no virtual camera output";
pub const LOG_EXCLUSIVE_CAPS_OK: &str =
    "v4l2loopback exclusive_caps is active (browser-compatible)";
pub const LOG_AUTO_LOAD_ATTEMPT: &str =
    "attempting to auto-load v4l2loopback module (with auto-install fallback)";
pub const LOG_MODULE_LOADED: &str = "v4l2loopback module loaded; device should appear shortly";
pub const LOG_MULTI_NODE_RELOAD: &str = "multi-node mode: reloading module with more devices";
pub const LOG_MULTI_NODE_OK: &str = "multi-node module reload successful";
pub const LOG_MULTI_NODE_RELOAD_FAIL: &str = "failed to reload module for multi-node mode";
pub const LOG_MULTI_NODE_SINK: &str = "multi-node sink: feeding multiple loopback devices";
pub const LOG_MULTI_NODE_FALLBACK: &str =
    "multi-node sink: fewer than 2 loopback devices found, using single sink";
pub const LOG_MULTI_READER: &str =
    "multi-reader: one virtual camera, multiple concurrent readers (native v4l2loopback)";
pub const LOG_TRAY_DISABLED: &str = "tray icon disabled via --no-tray";
pub const LOG_SHUTDOWN_DRAIN: &str = "shutdown requested; draining pipeline";
pub const LOG_QUIT_GUI: &str = "quit requested from GUI; exiting";
pub const LOG_RESTART: &str = "restarting pipeline with new settings";
pub const LOG_DRY_RUN_STOP: &str = "shutdown requested; stopping dry-run";
pub const LOG_DRY_RUN_DONE: &str = "dry-run complete";
pub const LOG_DRY_RUN_FRAME: &str = "dry-run: capturing (frames discarded)";
pub const LOG_WIRE_FORMAT_WARN: &str =
    "wire format is often rejected by browsers; prefer format=auto (YUYV) for Chrome/Firefox/Zoom";
pub const LOG_JOIN_TIMEOUT: &str =
    "thread did not exit within timeout; will be killed on process exit";

// ---- stderr helpers (multi-line guidance) ----

pub fn warn_exclusive_caps_zero(module_params: &str) {
    eprintln!(
        "WARNING: v4l2loopback is loaded with exclusive_caps=0.\n\
         Chrome, Firefox, Zoom, and Teams will NOT list the virtual camera.\n\
         OBS may still work. Reload the module:\n\
           sudo modprobe -r v4l2loopback\n\
           sudo modprobe v4l2loopback {module_params}"
    );
}

pub fn note_pkexec_missing(module_params: &str) {
    eprintln!(
        "Note: pkexec not available. Run manually:\n  sudo modprobe v4l2loopback {module_params}"
    );
}

pub fn note_distro_unsupported(module_params: &str) {
    eprintln!("Auto-install not supported on this distribution.");
    eprintln!(
        "Install v4l2loopback-dkms manually, then run:\n  sudo modprobe v4l2loopback {module_params}"
    );
}

pub fn note_install_failed(code: i32) {
    eprintln!("Package installation failed (exit code {code}).");
    eprintln!("Check your network connection and try again, or install manually.");
}

pub fn note_auto_load_failed(err: &impl std::fmt::Display) {
    eprintln!("Failed to auto-load v4l2loopback module: {err}");
}

pub fn note_manual_modprobe(module_params: &str) {
    eprintln!("\nYou can also load it manually:\n  sudo modprobe v4l2loopback {module_params}");
}

pub fn note_multi_node_reload(desired: u32, current: usize) {
    eprintln!(
        "Multi-app mode needs {desired} virtual cameras but only {current} exist.\n\
         Reloading v4l2loopback (devices={desired})...\n\
         NOTE: the reload only succeeds while NO app is using the virtual camera\n\
         (modprobe -r fails on a busy device). Close camera apps if it fails.\n\
         (A polkit authentication dialog may appear)"
    );
}

pub fn note_multi_node_ready(desired: u32) {
    eprintln!("✓ Multi-node mode ready: {desired} virtual cameras available");
}

pub fn warn_multi_node_partial(found: usize, module_params: &str) {
    eprintln!(
        "WARNING: Module reloaded but only {found} device(s) found.\n\
         If the extra nodes do not appear shortly, try:\n\
         \n\
         sudo modprobe -r v4l2loopback\n\
         sudo modprobe v4l2loopback {module_params}"
    );
}

pub fn error_multi_node_reload(err: &impl std::fmt::Display, module_params: &str) {
    eprintln!(
        "ERROR: Could not reload v4l2loopback for multi-app mode: {err}\n\
         \n\
         Continuing with the existing single node. Note: on v4l2loopback ≥ 0.14\n\
         only ONE app can stream from it at a time — the second app gets\n\
         \"Device or resource busy\". To enable multi-app support, CLOSE every\n\
         app using the virtual camera, then either restart vcam-proxy or run:\n\
         \n\
           sudo modprobe -r v4l2loopback\n\
           sudo modprobe v4l2loopback {module_params}\n\
         \n\
         Then assign each app its own camera ('vcam-proxy', 'vcam-proxy-2', …)."
    );
}

/// Variant of [`error_multi_node_reload`] used when the reload was blocked by
/// busy devices; `users` names exactly which processes to close.
pub fn error_multi_node_busy(users: &[DeviceUser], module_params: &str) {
    eprintln!(
        "ERROR: Could not reload v4l2loopback for multi-app mode — the virtual\n\
         camera is in use by {} process(es):\n\
         {}\
         \n\
         Close these app(s), then either restart vcam-proxy or run:\n\
         \n\
           sudo modprobe -r v4l2loopback\n\
           sudo modprobe v4l2loopback {module_params}\n\
         \n\
         Until then, only ONE app can use the single virtual camera at a time.",
        users.len(),
        format_blockers(users)
    );
}

pub fn error_no_loopback_device() {
    eprintln!(
        "Error: No v4l2loopback virtual camera device found.\n\
         \n\
         Set auto_load_module = true in {CONFIG_PATH_HINT}\n\
         (default) and restart — a polkit prompt may appear.\n\
         \n\
         Or load manually:\n\
           {MODPROBE_HINT}\n\
         \n\
         Verify with: vcam-proxy --list-loopback"
    );
}

pub fn error_scan_failed(source: &impl std::fmt::Display) {
    eprintln!("Error scanning for video devices: {source}");
}

pub fn error_device_not_found(path: &Path) {
    eprintln!(
        "Error: {} not found. Is v4l2loopback loaded?\n  sudo modprobe v4l2loopback exclusive_caps=1",
        path.display()
    );
}

pub fn error_permission(err: &impl std::fmt::Display, suggestion: &str) {
    eprintln!("Error: {err}\n\nSuggestion:\n  {suggestion}");
}

pub fn error_device_access(path: &Path, err: &impl std::fmt::Display) {
    eprintln!("Error accessing {}: {err}", path.display());
}

pub fn note_multi_node_active(count: usize, paths: &[impl AsRef<Path>]) {
    eprintln!("✓ Multi-app mode active: feeding {count} virtual cameras (assign one per app):");
    for (i, p) in paths.iter().enumerate() {
        eprintln!(
            "  App {} → '{}' ({})",
            i + 1,
            card_label(i as u32),
            p.as_ref().display()
        );
    }
}

pub fn warn_multi_node_fallback(desired: u32, module_params: &str) {
    eprintln!(
        "\n⚠ Multi-app mode requested ({desired} nodes) but only 1 virtual camera exists.\n\
           On v4l2loopback ≥ 0.14 a single node can only serve ONE app at a time\n\
           (the second app gets \"Device or resource busy\"). To create more nodes,\n\
           CLOSE every app using the virtual camera, then reload the module:\n\
         \n\
             sudo modprobe -r v4l2loopback\n\
             sudo modprobe v4l2loopback {module_params}\n"
    );
}

pub fn print_no_loopback_list_hint() {
    println!("No video output devices found.");
    println!("\nTo create a virtual camera, run:");
    println!("  {MODPROBE_HINT}");
}

pub fn print_permissions_help() {
    println!("\n        → Fix permissions with:");
    println!("          sudo usermod -aG video $USER");
    println!("        → Then LOG OUT and log back in for the group change to take effect.");
    println!("        → Verify with: groups | grep video");
}

pub fn print_setup_install_hints() {
    println!("\n        → Try installing v4l2loopback-dkms for your distro:");
    println!("          Debian/Ubuntu: sudo apt install v4l2loopback-dkms v4l-utils");
    println!("          Fedora:       sudo dnf install v4l2loopback");
    println!("          Arch:         sudo pacman -S v4l2loopback-dkms v4l-utils");
    println!("          openSUSE:     sudo zypper install v4l2loopback");
    println!("\n        → Then load with:");
    println!("          {MODPROBE_HINT}");
    println!("\n        → To load at boot, create /etc/modules-load.d/v4l2loopback.conf with:");
    println!("          v4l2loopback");
    println!("\n        → And /etc/modprobe.d/v4l2loopback.conf with:");
    println!(
        "          options v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1 video_nr=10"
    );
}

pub fn print_setup_ok() {
    println!("        ✓ Everything looks good! You can now run:");
    println!("          cargo run");
    println!("          # or: vcam-proxy");
    println!("        Settings live in {CONFIG_PATH_HINT}");
}

pub fn print_setup_fail() {
    println!("        ✗ Some issues need fixing. See above for guidance.");
    println!("        After fixing, run this command again to verify.");
}

/// Base card label for the first node. Extra nodes get `-2`, `-3`, … suffixes
/// so users can assign each app its own virtual camera by name.
pub const CARD_LABEL_BASE: &str = "vcam-proxy";

/// Card label for node `i` (0-based). The first node keeps the historical
/// plain "vcam-proxy" name so existing app camera selections keep working.
pub fn card_label(i: u32) -> String {
    if i == 0 {
        CARD_LABEL_BASE.to_string()
    } else {
        format!("{CARD_LABEL_BASE}-{}", i + 1)
    }
}

/// Build the modprobe parameter string from runtime config.
///
/// Correctness notes for v4l2loopback ≥ 0.14:
/// - `exclusive_caps` and `card_label` are **per-device arrays**: passing a
///   scalar only configures the first node, leaving extra nodes invisible to
///   browsers (`exclusive_caps=N`) and indistinguishable (duplicate labels).
/// - `timeout` is deliberately NOT passed: it is not a module parameter on
///   modern v4l2loopback (passing unknown parameters can fail the whole
///   `modprobe`). The frame timeout is applied at runtime via the `timeout`
///   ioctl control instead (see `apply_loopback_controls`).
pub fn module_params(exclusive_caps: u32, devices: u32, _timeout: u32) -> String {
    let devices = devices.clamp(1, 8);
    let caps: Vec<String> = (0..devices).map(|_| exclusive_caps.to_string()).collect();
    let labels: Vec<String> = (0..devices).map(card_label).collect();
    let video_nr: Vec<String> = (0..devices).map(|i| (10 + i).to_string()).collect();
    format!(
        "exclusive_caps={} card_label={} devices={devices} \
         video_nr={} max_buffers=4 max_openers=16",
        caps.join(","),
        labels.join(","),
        video_nr.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_params_single_device() {
        let p = module_params(1, 1, 0);
        assert!(
            p.contains("exclusive_caps=1 "),
            "scalar caps for one node: {p}"
        );
        assert!(p.contains("card_label=vcam-proxy "), "plain label: {p}");
        assert!(p.contains("devices=1"));
        assert!(p.contains("video_nr=10 "));
        assert!(p.contains("max_openers=16"));
    }

    #[test]
    fn module_params_multi_device_arrays() {
        let p = module_params(1, 3, 0);
        // Per-node exclusive_caps, otherwise nodes 2+ are invisible to browsers.
        assert!(p.contains("exclusive_caps=1,1,1 "), "caps array: {p}");
        // Distinct labels so each app can be assigned its own node by name.
        assert!(
            p.contains("card_label=vcam-proxy,vcam-proxy-2,vcam-proxy-3 "),
            "label array: {p}"
        );
        assert!(p.contains("devices=3"));
        assert!(p.contains("video_nr=10,11,12 "));
    }

    #[test]
    fn module_params_never_emits_timeout_param() {
        // `timeout` is not a module parameter on v4l2loopback ≥ 0.14; passing
        // it can fail the whole modprobe on strict kernels.
        for d in [1, 2, 8] {
            let p = module_params(1, d, 1000);
            assert!(!p.contains("timeout"), "no timeout param: {p}");
        }
    }

    #[test]
    fn module_params_clamps_devices() {
        let p = module_params(1, 99, 0);
        assert!(p.contains("devices=8"));
    }

    #[test]
    fn card_labels_numbered_from_two() {
        assert_eq!(card_label(0), "vcam-proxy");
        assert_eq!(card_label(1), "vcam-proxy-2");
        assert_eq!(card_label(7), "vcam-proxy-8");
    }
}
