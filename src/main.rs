//! vcam-proxy: physical camera -> virtual loopback proxy.
//!
//! Thread topology:
//! - `main`    : setup, signal handling, join & teardown
//! - `capture` : owns the camera, fills pooled frames, drops when behind
//! - `sink`    : owns the virtual device, writes frames, recycles buffers
//!
//! Frames flow capture -> sink through a bounded channel; free buffer slots
//! flow back through the pool. No allocation happens per frame in steady
//! state.

mod capture;
mod config;
mod convert;
mod frame;
mod pipeline;
mod settings;
mod shutdown;
mod sink;
mod tray;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use config::{Config, ResolvedConfig};
use frame::BufferPool;
use pipeline::Stats;
use shutdown::Shutdown;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Install shutdown handler ONCE at startup. All modes (normal, dry-run,
    // etc.) share this same flag so Ctrl+C works consistently.
    let shutdown = Shutdown::install();

    let cli = Config::parse();
    let settings = settings::Settings::load();

    // Handle --edit-config: open config file in editor, then exit.
    if cli.edit_config {
        let path = settings::Settings::config_path();
        if let Err(e) = settings::Settings::create_default_file() {
            eprintln!("Failed to create config file: {e}");
            return;
        }
        println!("Opening config file: {}", path.display());
        let _ = open::that(&path);
        return;
    }

    // Handle --show-config: display current settings and exit.
    if cli.show_config {
        let resolved = ResolvedConfig::from_cli_and_settings(&cli, &settings);
        print_settings_table(&resolved, &settings);
        return;
    }

    // Handle --save-config: persist current settings, then exit.
    if cli.save_config {
        let resolved = ResolvedConfig::from_cli_and_settings(&cli, &settings);
        match resolved.to_settings().save() {
            Ok(()) => {
                let path = settings::Settings::config_path();
                println!("Settings saved to: {}", path.display());
            }
            Err(e) => eprintln!("Failed to save settings: {e}"),
        }
        return;
    }

    // Resolve final config: CLI args override settings file.
    let cfg = Arc::new(ResolvedConfig::from_cli_and_settings(&cli, &settings));

    if cli.list {
        capture::list_cameras();
        return;
    }

    // Handle --list-loopback: enumerate output devices, print, exit.
    if cli.list_loopback {
        list_loopback_devices();
        return;
    }

    // Handle --setup: auto-configure system, then exit.
    if cli.setup {
        run_setup(cfg.clone());
        return;
    }

    info!(
        camera = cfg.camera,
        width = cfg.width,
        height = cfg.height,
        fps = cfg.fps,
        format = ?cfg.format,
        buffers = cfg.buffers,
        multi_reader = cfg.multi_reader,
        exclusive_caps = cfg.exclusive_caps,
        "starting vcam-proxy"
    );

    // Handle dry-run mode: no loopback output, just test capture.
    if cli.dry_run {
        info!("dry-run mode: capture only, no virtual camera output");
        return run_dry_run(cfg, shutdown);
    }

    // Build the module parameters based on multi-reader setting.
    let exclusive_caps = cfg.exclusive_caps;
    let multi_reader = cfg.multi_reader;
    let module_params = if multi_reader {
        format!(
            "exclusive_caps={exclusive_caps} card_label=\"vcam-proxy\" devices=2 timeout={}",
            cfg.timeout
        )
    } else {
        format!(
            "exclusive_caps={exclusive_caps} card_label=\"vcam-proxy\" devices=1 timeout={}",
            cfg.timeout
        )
    };

    // Optionally auto-load the v4l2loopback module via pkexec FIRST,
    // before trying to find a device. This way --auto-load-module has
    // a chance to create the /dev/video* node we need.
    if cli.auto_load_module && !sink::is_module_loaded() {
        info!("attempting to auto-load v4l2loopback module (with auto-install fallback)");
        match sink::ensure_module_loaded_with_install(&module_params) {
            Ok(()) => {
                // Module loaded — give kernel a moment to create device nodes
                std::thread::sleep(Duration::from_millis(500));
                info!("v4l2loopback module loaded; device should appear shortly");
            }
            Err(e) => {
                match &e {
                    sink::ModuleError::PkexecNotAvailable => {
                        eprintln!(
                            "Note: pkexec not available. Run manually:\n  sudo modprobe v4l2loopback {module_params}"
                        );
                    }
                    sink::ModuleError::DistroNotSupported => {
                        eprintln!("Auto-install not supported on this distribution.");
                        eprintln!("Install v4l2loopback-dkms manually, then run:\n  sudo modprobe v4l2loopback {module_params}");
                    }
                    sink::ModuleError::InstallFailed(code) => {
                        eprintln!("Package installation failed (exit code {code}).");
                        eprintln!(
                            "Check your network connection and try again, or install manually."
                        );
                    }
                    _ => {
                        eprintln!("Failed to auto-load v4l2loopback module: {e}");
                    }
                }
                eprintln!("\nYou can also load it manually:\n  sudo modprobe v4l2loopback {module_params}");
            }
        }
    }

    // Determine the loopback device to use (auto-detect if needed).
    let loopback_path = match sink::find_loopback_device(Path::new(&cfg.device)) {
        Ok(path) => path,
        Err(e) => {
            match &e {
                sink::LoopbackError::NoDeviceFound => {
                    eprintln!(
                        "Error: No virtual camera device found.\n\
                         \n\
                         To create one, run:\n\
                           sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1\n\
                         \n\
                         Then verify with: vcam-proxy --list-loopback"
                    );
                }
                sink::LoopbackError::ScanFailed { source } => {
                    eprintln!("Error scanning for video devices: {source}");
                }
            }
            std::process::exit(1);
        }
    };

    // Check permissions before opening device.
    if let Err(e) = sink::check_device_access(&loopback_path) {
        match &e {
            sink::AccessError::NotFound { path } => {
                eprintln!(
                    "Error: {} not found. Is v4l2loopback loaded?\n  sudo modprobe v4l2loopback exclusive_caps=1",
                    path.display()
                );
            }
            sink::AccessError::PermissionDenied { suggestion, .. } => {
                eprintln!("Error: {e}\n\nSuggestion:\n  {suggestion}");
            }
            sink::AccessError::Other { .. } => {
                eprintln!("Error accessing {}: {e}", loopback_path.display());
            }
        }
        std::process::exit(1);
    }

    // Shared switch: tray toggles it, sink loop reads it. Allows the user to
    // start/stop the virtual camera output without tearing down capture.
    let sink_switch = tray::SinkSwitch::new(true);

    // Spawn the system-tray icon for on/off toggle (unless disabled).
    let tray_handle = if cli.no_tray {
        info!("tray icon disabled via --no-tray");
        None
    } else {
        // Gracefully continues without a tray if D-Bus is unavailable.
        let tray_stats = tray::TrayStats::new(cfg.width, cfg.height, cfg.fps);
        tray::spawn(sink_switch.clone(), shutdown.clone(), tray_stats)
    };

    // Slot size covers the worst wire format (RGB24). Slots grow transparently
    // if the camera negotiates something larger.
    let slot_bytes = cfg.width as usize * cfg.height as usize * 3;
    let pool = BufferPool::new(cfg.buffers, slot_bytes);

    // Bounded hand-off: a full channel means "sink is behind" and frames are
    // dropped at the capture side, never queued unboundedly.
    let (tx, rx) = crossbeam_channel::bounded(cfg.buffers);

    let stats = Arc::new(Stats::default());
    let sink_handle = pipeline::spawn_sink(
        cfg.clone(),
        loopback_path,
        rx,
        pool.clone(),
        shutdown.clone(),
        stats.clone(),
        sink_switch.clone(),
    );
    let capture_handle = capture::spawn(cfg, pool, tx, shutdown.clone(), stats);

    while !shutdown.is_set() {
        std::thread::sleep(Duration::from_millis(100));
    }
    info!("shutdown requested; draining pipeline");

    // Join with timeout — the capture thread may be blocked on cam.frame()
    // and won't notice shutdown until the next frame. If it doesn't exit
    // within a reasonable time, log a warning and continue (the OS will
    // clean up the threads when the process exits).
    join_with_timeout("capture", capture_handle, Duration::from_secs(3));
    join_with_timeout("sink", sink_handle, Duration::from_secs(3));
    if let Some(h) = tray_handle {
        join_with_timeout("tray", h, Duration::from_secs(2));
    }

    info!("all threads stopped; descriptors released");
}

/// Join a thread with a timeout. Uses a parked watcher thread since
/// JoinHandle has no built-in timeout. Logs a warning if the thread
/// doesn't exit within the timeout — it will be killed when the process exits.
fn join_with_timeout(name: &str, handle: std::thread::JoinHandle<()>, timeout: Duration) {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::Builder::new()
        .name(format!("{name}-watcher"))
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        })
        .expect("failed to spawn watcher thread");

    match rx.recv_timeout(timeout) {
        Ok(()) => {} // Thread exited cleanly
        Err(_) => {
            tracing::warn!(
                thread = name,
                timeout_secs = timeout.as_secs(),
                "{name} thread did not exit within timeout; will be killed on process exit"
            );
        }
    }
    // Watcher thread continues running in background — will exit when
    // the watched thread finally finishes (or when the process exits).
    drop(watcher);
}

/// Print all discovered loopback-capable output devices to stdout.
fn list_loopback_devices() {
    match sink::discover_loopback_devices() {
        Ok(devices) => {
            if devices.is_empty() {
                println!("No video output devices found.");
                println!("\nTo create a virtual camera, run:");
                println!(
                    "  sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1"
                );
                return;
            }
            println!("Video output devices ({} found):", devices.len());
            for dev in &devices {
                let is_loopback = dev.driver == "v4l2loopback";
                println!(
                    "  {} {}{}",
                    dev.path.display(),
                    dev.card,
                    if is_loopback {
                        " [v4l2loopback ✓]"
                    } else {
                        ""
                    }
                );
            }
        }
        Err(e) => {
            eprintln!("Error scanning /dev: {e}");
            std::process::exit(1);
        }
    }
}

/// Run in dry-run mode: capture frames but discard them (no loopback output).
/// Useful for testing camera access without a virtual device.
fn run_dry_run(cfg: Arc<ResolvedConfig>, shutdown: Shutdown) {
    let slot_bytes = cfg.width as usize * cfg.height as usize * 3;
    let pool = BufferPool::new(cfg.buffers, slot_bytes);
    let (tx, rx) = crossbeam_channel::bounded(cfg.buffers);
    let stats = Arc::new(Stats::default());

    // Spawn capture thread
    let capture_handle = capture::spawn(
        cfg.clone(),
        pool.clone(),
        tx,
        shutdown.clone(),
        stats.clone(),
    );

    // Drain frames from channel and discard them
    let shutdown_drain = shutdown.clone();
    let drain_handle = std::thread::spawn(move || {
        let mut count = 0u64;
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => {
                    count += 1;
                    if count % 30 == 0 {
                        info!(
                            frames = count,
                            format = ?frame.format,
                            res = format!("{}x{}", frame.width, frame.height),
                            "dry-run: capturing (frames discarded)"
                        );
                    }
                    pool.release(frame);
                }
                Err(_) => {
                    if shutdown_drain.is_set() {
                        break;
                    }
                }
            }
        }
        count
    });

    while !shutdown.is_set() {
        std::thread::sleep(Duration::from_millis(100));
    }
    info!("shutdown requested; stopping dry-run");

    // Use timeout helpers — same graceful shutdown as normal mode
    join_with_timeout("capture", capture_handle, Duration::from_secs(3));
    let total = drain_handle.join().unwrap_or(0);
    info!(total_frames = total, "dry-run complete");
}

/// Run automatic setup: check system, load module, fix permissions, validate.
/// Prints actionable guidance and exits.
fn run_setup(cfg: Arc<ResolvedConfig>) {
    println!("=== vcam-proxy automatic setup ===\n");

    let mut ok = true;

    // 1. Check if v4l2loopback module is loaded
    print!("[1/4] Checking v4l2loopback kernel module... ");
    if sink::is_module_loaded() {
        println!("✓ already loaded");
    } else {
        println!("✗ NOT loaded");
        print!("        Attempting to load via pkexec (with auto-install)... ");
        match sink::ensure_module_loaded_with_install(
            "exclusive_caps=1 card_label=vcam-proxy devices=1",
        ) {
            Ok(()) => {
                println!("✓ loaded successfully");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                println!("✗ failed: {e}");
                ok = false;
                println!("\n        → Try installing v4l2loopback-dkms for your distro:");
                println!("          Debian/Ubuntu: sudo apt install v4l2loopback-dkms v4l-utils");
                println!("          Fedora:       sudo dnf install v4l2loopback");
                println!("          Arch:         sudo pacman -S v4l2loopback-dkms v4l-utils");
                println!("          openSUSE:     sudo zypper install v4l2loopback");
                println!("\n        → Then load with:");
                println!("          sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1");
                println!("\n        → To load at boot, create /etc/modules-load.d/v4l2loopback.conf with:");
                println!("          v4l2loopback");
                println!("\n        → And /etc/modprobe.d/v4l2loopback.conf with:");
                println!("          options v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1");
            }
        }
    }

    // 2. Discover devices
    print!("\n[2/4] Scanning for video output devices... ");
    match sink::discover_loopback_devices() {
        Ok(devices) => {
            if devices.is_empty() {
                println!("✗ none found");
                ok = false;
            } else {
                println!("✓ found {} device(s):", devices.len());
                for dev in &devices {
                    let is_loopback = dev.driver == "v4l2 loopback" || dev.driver == "v4l2loopback";
                    let marker = if is_loopback {
                        " ✓"
                    } else {
                        " (not v4l2loopback)"
                    };
                    println!("        - {} [{}]{}", dev.path.display(), dev.card, marker);
                }
            }
        }
        Err(e) => {
            println!("✗ scan failed: {e}");
            ok = false;
        }
    }

    // 3. Check permissions
    print!("\n[3/4] Checking device permissions... ");
    let target = std::path::PathBuf::from(&cfg.device);
    if !target.exists() {
        // Try to find any loopback device to test
        match sink::discover_loopback_devices() {
            Ok(devices) if !devices.is_empty() => {
                let path = &devices[0].path;
                print!("(testing {}) ", path.display());
                match sink::check_device_access(path) {
                    Ok(()) => println!("✓ accessible"),
                    Err(e) => {
                        println!("✗ {e}");
                        ok = false;
                        print_permissions_help();
                    }
                }
            }
            _ => {
                println!("? no device to test (load module first)");
            }
        }
    } else {
        match sink::check_device_access(&target) {
            Ok(()) => println!("✓ accessible"),
            Err(e) => {
                println!("✗ {e}");
                ok = false;
                print_permissions_help();
            }
        }
    }

    // 4. Summary
    println!("\n[4/4] Summary");
    if ok {
        println!("        ✓ Everything looks good! You can now run:");
        println!("          cargo run -- --auto-load-module");
        println!("        Or just:");
        println!("          cargo run --release");
    } else {
        println!("        ✗ Some issues need fixing. See above for guidance.");
        println!("        After fixing, run this command again to verify.");
    }
    println!("\n=== setup complete ===");
}

fn print_permissions_help() {
    println!("\n        → Fix permissions with:");
    println!("          sudo usermod -aG video $USER");
    println!("        → Then LOG OUT and log back in for the group change to take effect.");
    println!("        → Verify with: groups | grep video");
}

/// Print a formatted table of current settings and their source.
fn print_settings_table(cfg: &ResolvedConfig, settings: &settings::Settings) {
    let config_path = settings::Settings::config_path();
    let sep = "-".repeat(60);
    println!("Current settings (source: [C]LI / [F]ile / [D]efault):");
    println!("{}", sep);
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "camera",
        cfg.camera,
        if cfg.camera != settings.camera {
            'C'
        } else if cfg.camera != 0 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "device",
        cfg.device,
        if cfg.device != settings.device {
            'C'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "width",
        cfg.width,
        if cfg.width != settings.width {
            'C'
        } else if cfg.width != 1280 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "height",
        cfg.height,
        if cfg.height != settings.height {
            'C'
        } else if cfg.height != 720 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "fps",
        cfg.fps,
        if cfg.fps != settings.fps {
            'C'
        } else if cfg.fps != 30 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "buffers",
        cfg.buffers,
        if cfg.buffers != settings.buffers {
            'C'
        } else if cfg.buffers != 4 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "retry_ms",
        cfg.retry_ms,
        if cfg.retry_ms != settings.retry_ms {
            'C'
        } else if cfg.retry_ms != 1000 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "multi_reader",
        cfg.multi_reader,
        if cfg.multi_reader != settings.multi_reader {
            'C'
        } else if settings.multi_reader {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "exclusive_caps",
        cfg.exclusive_caps,
        if cfg.exclusive_caps != settings.exclusive_caps {
            'C'
        } else if settings.exclusive_caps != 1 {
            'F'
        } else {
            'D'
        }
    );
    println!(
        "  {:<20} {:<15} [{:<1}]",
        "timeout",
        cfg.timeout,
        if cfg.timeout != settings.timeout {
            'C'
        } else if settings.timeout != 1000 {
            'F'
        } else {
            'D'
        }
    );
    println!("{}", sep);
    println!("\nConfig file: {}", config_path.display());
    println!("To edit: vcam-proxy --edit-config");
    println!("To save current settings: vcam-proxy --save-config");
}
