//! vcam-proxy: physical camera -> virtual loopback proxy.
//!
//! Thread topology:
//! - `main`    : setup, signal handling, GUI/tray, join & teardown
//! - `capture` : owns the camera, fills pooled frames, drops when behind
//! - `sink`    : owns the virtual device, writes frames, recycles buffers
//!
//! Frames flow capture -> sink through a bounded channel; free buffer slots
//! flow back through the pool. No allocation happens per frame in steady
//! state.
//!
//! Configuration is primarily done through an in-app settings window (egui).
//! It opens automatically on first run (no config file) for guided setup, and
//! is reachable afterwards from the tray icon. CLI flags still work and take
//! precedence; `--no-gui` restores a fully headless, args-only mode.

mod capture;
mod config;
mod convert;
mod frame;
mod pipeline;
mod settings;
mod shutdown;
mod sink;
mod tray;
mod ui;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use config::{Config, ResolvedConfig};
use frame::BufferPool;
use pipeline::Stats;
use shutdown::Shutdown;
use tracing::{info, warn};
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

    // Handle --list: enumerate physical cameras and exit.
    if cli.list {
        capture::list_cameras();
        return;
    }

    // Handle --list-loopback: enumerate output devices, print, exit.
    if cli.list_loopback {
        list_loopback_devices();
        return;
    }

    // Determine whether the GUI should run. By default it does, unless the user
    // passes --no-gui. First-run detection: no config file on disk yet.
    let config_path = settings::Settings::config_path();
    let first_run = !config_path.exists();
    let gui_enabled = !cli.no_gui;

    // Resolve the initial config: CLI args override settings file.
    let initial_cfg =
        Arc::new(ResolvedConfig::from_cli_and_settings(&cli, &settings).sanitized());

    // Shared live switch: GUI and tray both drive it; the sink reads it.
    let sink_switch = tray::SinkSwitch::new(true);

    // Build the shared GUI state (seeded from default settings on first run so
    // the setup window starts blank, or from the resolved config afterwards).
    let gui_state: Option<Arc<Mutex<ui::GuiState>>> = if gui_enabled {
        let seed = if first_run {
            ui::settings_to_resolved(&settings::Settings::default())
        } else {
            (*initial_cfg).clone()
        };
        Some(ui::GuiState::new(seed, first_run || cli.settings))
    } else {
        None
    };
    let gui_wake = gui_state.as_ref().map(|s| ui::GuiWake::new(s.clone()));

    // Handle --setup: auto-configure system, then exit.
    if cli.setup {
        run_setup(initial_cfg.clone());
        return;
    }

    if first_run && gui_enabled {
        info!("first run: opening settings window for guided setup");
    }

    // Run the pipeline on a background "controller" thread. egui/winit require
    // the event loop on the main thread, so the GUI owns main and the pipeline
    // lives elsewhere. The controller re-spawns the pipeline when the user hits
    // "Apply & Restart", and stops on shutdown or GUI "Quit".
    let controller = {
        let cli = cli.clone();
        let initial_cfg = initial_cfg.clone();
        let gui_state = gui_state.clone();
        let gui_wake = gui_wake.clone();
        let sink_switch = sink_switch.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("controller".into())
            .spawn(move || {
                run_controller(&cli, initial_cfg, gui_state, gui_wake, sink_switch, shutdown)
            })
            .expect("failed to spawn controller thread")
    };

    // Main thread: run the GUI if enabled; otherwise just wait for the
    // controller to finish (it exits on shutdown / GUI Quit).
    match gui_state {
        Some(state) => {
            ui::run(state, shutdown.clone());
            // GUI closed (Quit or shutdown): make sure the controller unwinds.
            shutdown.request();
            let _ = controller.join();
        }
        None => {
            // No GUI: block until the controller thread ends on its own.
            let _ = controller.join();
        }
    }

    info!("all threads stopped; descriptors released");
}

/// Background thread: owns the capture/sink/tray pipeline and supports live
/// reconfiguration. Loops until the shutdown flag is set or the GUI requests
/// "Quit"; re-spawns the pipeline when the GUI requests "Apply & Restart".
fn run_controller(
    cli: &Config,
    initial_cfg: Arc<ResolvedConfig>,
    gui_state: Option<Arc<Mutex<ui::GuiState>>>,
    gui_wake: Option<Arc<ui::GuiWake>>,
    sink_switch: tray::SinkSwitch,
    shutdown: Shutdown,
) {
    // ---- Main run loop (supports live reconfiguration via "Apply & Restart") ----
    let mut current_cfg = initial_cfg.clone();
    loop {
        // Sync the GUI's desired/live state into what we launch with.
        if let Some(state) = &gui_state {
            let g = state.lock().unwrap();
            current_cfg = Arc::new(g.desired.clone().sanitized());
        }

        // Mirror the GUI live on/off into the shared switch.
        if let Some(state) = &gui_state {
            let live = state.lock().unwrap().live_on;
            sink_switch.set(live);
        }

        info!(
            camera = current_cfg.camera,
            width = current_cfg.width,
            height = current_cfg.height,
            fps = current_cfg.fps,
            format = ?current_cfg.format,
            buffers = current_cfg.buffers,
            multi_reader = current_cfg.multi_reader,
            exclusive_caps = current_cfg.exclusive_caps,
            auto_resolution = current_cfg.auto_resolution,
            "starting vcam-proxy"
        );

        if matches!(
            current_cfg.format,
            config::FormatPref::Rgb24 | config::FormatPref::Mjpeg
        ) {
            tracing::warn!(
                format = ?current_cfg.format,
                "wire format {:?} is often rejected by browsers; prefer --format auto \
                 (YUYV/NV12) for Chrome/Firefox/Zoom",
                current_cfg.format
            );
        }

        // Handle dry-run mode: no loopback output, just test capture.
        if cli.dry_run {
            info!("dry-run mode: capture only, no virtual camera output");
            run_dry_run(current_cfg.clone(), shutdown.clone());
            break;
        }

        // Build the module parameters based on multi-reader setting.
        let exclusive_caps = current_cfg.exclusive_caps;
        let multi_reader = current_cfg.multi_reader;
        let devices = if multi_reader { 2 } else { 1 };
        let module_params = format!(
            "exclusive_caps={exclusive_caps} card_label=\"vcam-proxy\" devices={devices} \
             max_buffers=4 max_openers=10 timeout={}",
            current_cfg.timeout
        );

        if sink::is_module_loaded() {
            match sink::exclusive_caps_active() {
                Some(false) => {
                    eprintln!(
                        "WARNING: v4l2loopback is loaded with exclusive_caps=0.\n\
                         Chrome, Firefox, Zoom, and Teams will NOT list the virtual camera.\n\
                         OBS may still work. Reload the module:\n\
                           sudo modprobe -r v4l2loopback\n\
                           sudo modprobe v4l2loopback {module_params}"
                    );
                }
                Some(true) => {
                    info!("v4l2loopback exclusive_caps is active (browser-compatible)");
                }
                None => {}
            }
        }

        if cli.auto_load_module && !sink::is_module_loaded() {
            info!("attempting to auto-load v4l2loopback module (with auto-install fallback)");
            match sink::ensure_module_loaded_with_install(&module_params) {
                Ok(()) => {
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

        if multi_reader && sink::is_module_loaded() {
            let current_devices = sink::count_loopback_devices();
            if current_devices < 2 {
                info!(
                    current_devices,
                    "multi-reader mode requires 2 devices, reloading module with devices=2"
                );
                eprintln!(
                    "WARNING: Multi-reader mode requires 2 virtual cameras but only {current_devices} found.\n\
                     Reloading v4l2loopback module with devices=2...\n\
                     (A polkit authentication dialog may appear)"
                );
                match sink::load_module_with_params_force(&module_params) {
                    Ok(()) => {
                        let mut devices_ready = false;
                        for _ in 0..50 {
                            std::thread::sleep(Duration::from_millis(200));
                            if sink::count_loopback_devices() >= 2 {
                                devices_ready = true;
                                break;
                            }
                        }
                        if devices_ready {
                            eprintln!("✓ Multi-reader mode ready: 2 virtual cameras available");
                            info!("multi-reader module reload successful, 2 devices available");
                        } else {
                            warn!("module reloaded but only {} devices found after waiting", sink::count_loopback_devices());
                            eprintln!(
                                "WARNING: Module reloaded but only {} device(s) found.\n\
                                 The second device should appear shortly. If not, try:\n\
                                 \n\
                                 sudo modprobe -r v4l2loopback\n\
                                 sudo modprobe v4l2loopback {module_params}",
                                sink::count_loopback_devices()
                            );
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to reload module with devices=2");
                        eprintln!(
                            "ERROR: Failed to reload v4l2loopback with devices=2: {e}\n\
                             \n\
                             Multi-reader mode requires 2 virtual camera devices so that\n\
                             multiple apps can open different nodes simultaneously.\n\
                             \n\
                             Please reload manually:\n\
                             \n\
                               sudo modprobe -r v4l2loopback\n\
                               sudo modprobe v4l2loopback {module_params}\n\
                             \n\
                             Or run vcam-proxy with:\n\
                               vcam-proxy --auto-load-module"
                        );
                        eprintln!("\nContinuing with single-device mode (only one app at a time)...");
                    }
                }
            }
        }

        let loopback_path = match sink::find_loopback_device(Path::new(&current_cfg.device)) {
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
                // If the GUI is up, let the user fix the device in settings and
                // hit "Apply & Restart" rather than hard-exiting.
                if wait_for_restart_or_quit(&gui_state, &shutdown) {
                    continue;
                }
                std::process::exit(1);
            }
        };

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
            if wait_for_restart_or_quit(&gui_state, &shutdown) {
                continue;
            }
            std::process::exit(1);
        }

        // Shared pipeline counters — created up front so the tray can display the
        // *same* live atomics the capture/sink threads increment.
        let stats = Arc::new(Stats::default());

        // Spawn the system-tray icon for on/off toggle (unless disabled).
        let tray_handle = if cli.no_tray {
            info!("tray icon disabled via --no-tray");
            None
        } else {
            let tray_stats =
                tray::TrayStats::new(stats.clone(), current_cfg.width, current_cfg.height, current_cfg.fps);
            // The tray's "Settings…" item opens the GUI window.
            let gui_wake_for_tray = gui_wake.clone();
            tray::spawn_with_settings(sink_switch.clone(), shutdown.clone(), tray_stats, gui_wake_for_tray)
        };

        let slot_bytes = current_cfg.width as usize * current_cfg.height as usize * 3;
        let pool = BufferPool::new(current_cfg.buffers, slot_bytes);

        let sink_impl = if multi_reader {
            let all_paths = sink::discover_loopback_devices().unwrap_or_default();
            let loopback_paths: Vec<_> = all_paths
                .into_iter()
                .filter(|d| sink::is_loopback_driver(&d.driver))
                .map(|d| d.path)
                .collect();
            if loopback_paths.len() >= 2 {
                info!(devices = loopback_paths.len(), "multi-reader sink: feeding all loopback devices");
                eprintln!("✓ Multi-reader mode active: writing to {} virtual cameras", loopback_paths.len());
                eprintln!("  App 1 can open: {}", loopback_paths[0].display());
                eprintln!("  App 2 can open: {}", loopback_paths[1].display());
                sink::build_multi_with_paths(loopback_paths)
            } else {
                warn!("multi-reader sink: only one loopback device found, using single sink");
                eprintln!("\n⚠ Multi-reader mode is ENABLED but only 1 virtual camera found.");
                eprintln!("  Only ONE app can use the virtual camera at a time.");
                eprintln!("  Other apps will get 'Device busy' errors.\n");
                eprintln!("  To fix this, reload v4l2loopback with 2 devices:\n");
                eprintln!("    sudo modprobe -r v4l2loopback");
                eprintln!("    sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"vcam-proxy\" devices=2\n");
                sink::build_with_path(&current_cfg, Path::new(&loopback_path))
            }
        } else {
            sink::build_with_path(&current_cfg, Path::new(&loopback_path))
        };

        let (tx, rx) = crossbeam_channel::bounded(current_cfg.buffers);

        let sink_handle = pipeline::spawn_sink(
            current_cfg.clone(),
            sink_impl,
            rx,
            pool.clone(),
            shutdown.clone(),
            stats.clone(),
            sink_switch.clone(),
        );
        let capture_handle = capture::spawn(current_cfg.clone(), pool, tx, shutdown.clone(), stats);

        // ---- Block until shutdown, GUI "Apply & Restart", or GUI "Quit" ----
        let restart = block_until_done(&gui_state, &sink_switch, &shutdown);

        info!("shutdown requested; draining pipeline");

        join_with_timeout("capture", capture_handle, Duration::from_secs(3));
        join_with_timeout("sink", sink_handle, Duration::from_secs(3));
        if let Some(h) = tray_handle {
            join_with_timeout("tray", h, Duration::from_secs(2));
        }

        if gui_state
            .as_ref()
            .map(|s| s.lock().unwrap().quit)
            .unwrap_or(false)
        {
            info!("quit requested from GUI; exiting");
            break;
        }

        if !restart {
            // Shutdown flag (Ctrl+C / tray Quit / window closed) — stop.
            break;
        }

        // Restart: loop again with the GUI's (already-synced) desired config.
        info!("restarting pipeline with new settings");
        // Small settle so device fds are fully released.
        std::thread::sleep(Duration::from_millis(200));
    }

    info!("all threads stopped; descriptors released");
}

/// Block the main thread until either the pipeline should stop (shutdown flag,
/// GUI quit, or window closed with no restart), or the GUI requests a restart.
/// Returns `true` if a restart was requested.
///
/// While blocked, the live on/off switch is mirrored from the GUI into the
/// shared `SinkSwitch` so toggles take effect without a restart.
fn block_until_done(
    gui_state: &Option<Arc<std::sync::Mutex<ui::GuiState>>>,
    sink_switch: &tray::SinkSwitch,
    shutdown: &Shutdown,
) -> bool {
    loop {
        if let Some(state) = gui_state {
            let g = state.lock().unwrap();
            sink_switch.set(g.live_on);
            if g.restart {
                return true;
            }
            if g.quit {
                return false;
            }
        }
        if shutdown.is_set() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Like `block_until_done` but used before the pipeline is even spawned (e.g.
/// device not found). Only returns `true` (restart) or `false` (give up).
fn wait_for_restart_or_quit(
    gui_state: &Option<Arc<std::sync::Mutex<ui::GuiState>>>,
    shutdown: &Shutdown,
) -> bool {
    loop {
        if let Some(state) = gui_state {
            let g = state.lock().unwrap();
            if g.restart {
                return true;
            }
            if g.quit {
                return false;
            }
        }
        if shutdown.is_set() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
                let is_loopback =
                    dev.driver == "v4l2loopback" || dev.driver == "v4l2 loopback";
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

    let capture_handle = capture::spawn(
        cfg.clone(),
        pool.clone(),
        tx,
        shutdown.clone(),
        stats.clone(),
    );

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

    join_with_timeout("capture", capture_handle, Duration::from_secs(3));
    let total = drain_handle.join().unwrap_or(0);
    info!(total_frames = total, "dry-run complete");
}

/// Run automatic setup: check system, load module, fix permissions, validate.
/// Prints actionable guidance and exits.
fn run_setup(cfg: Arc<ResolvedConfig>) {
    println!("=== vcam-proxy automatic setup ===\n");

    let mut ok = true;

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

    print!("\n[3/4] Checking device permissions... ");
    let target = std::path::PathBuf::from(&cfg.device);
    if !target.exists() {
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
