//! User-facing log / stderr message templates.
//!
//! Keep long guidance strings here so call sites stay short and consistent.

use std::path::Path;

/// Default `modprobe` argument string when no ResolvedConfig is available yet.
pub const DEFAULT_MODULE_PARAMS: &str = "exclusive_caps=1 card_label=vcam-proxy devices=1 \
video_nr=10 max_buffers=4 max_openers=16 timeout=0";

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
    eprintln!("Note: pkexec not available. Run manually:\n  sudo modprobe v4l2loopback {module_params}");
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
        "Multi-node mode needs {desired} virtual cameras but only {current} exist.\n\
         Reloading v4l2loopback (devices={desired})...\n\
         Apps using the virtual camera must re-open it afterwards.\n\
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
        "ERROR: Could not reload v4l2loopback for multi-node mode: {err}\n\
         \n\
         Continuing with the existing node(s) — multiple apps can still\n\
         share ONE node (native multi-reader). For manual setup:\n\
         \n\
           sudo modprobe -r v4l2loopback\n\
           sudo modprobe v4l2loopback {module_params}\n\
         \n\
         Then restart vcam-proxy."
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
    eprintln!("✓ Multi-node mode active: writing to {count} virtual cameras");
    for (i, p) in paths.iter().enumerate() {
        eprintln!("  App {} can open: {}", i + 1, p.as_ref().display());
    }
}

pub fn warn_multi_node_fallback(desired: u32, module_params: &str) {
    eprintln!(
        "\n⚠ Multi-node mode requested ({desired}) but only 1 virtual camera found.\n\
           Falling back to a single node — multiple apps can still share it.\n\
           To create more nodes:\n\
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

/// Build the modprobe parameter string from runtime config.
pub fn module_params(exclusive_caps: u32, devices: u32, timeout: u32) -> String {
    let devices = devices.clamp(1, 8);
    let video_nr: Vec<String> = (0..devices).map(|i| (10 + i).to_string()).collect();
    format!(
        "exclusive_caps={exclusive_caps} card_label=vcam-proxy devices={devices} \
         video_nr={} max_buffers=4 max_openers=16 timeout={timeout}",
        video_nr.join(",")
    )
}
