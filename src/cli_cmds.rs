//! One-shot CLI utilities: list devices, setup wizard, settings table, dry-run.

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::capture;
use crate::config::ResolvedConfig;
use crate::controller;
use crate::frame::BufferPool;
use crate::messages::{self, DEFAULT_MODULE_PARAMS};
use crate::pipeline::Stats;
use crate::settings::Settings;
use crate::shutdown::Shutdown;
use crate::sink;

/// Print all discovered loopback-capable output devices to stdout.
pub fn list_loopback_devices() {
    match sink::discover_loopback_devices() {
        Ok(devices) => {
            if devices.is_empty() {
                messages::print_no_loopback_list_hint();
                return;
            }
            println!("Video output devices ({} found):", devices.len());
            for dev in &devices {
                let is_loopback = sink::is_loopback_driver(&dev.driver);
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

/// Capture frames but discard them (no loopback output).
pub fn run_dry_run(cfg: Arc<ResolvedConfig>, shutdown: Shutdown) {
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
                            "{}",
                            messages::LOG_DRY_RUN_FRAME
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
    info!("{}", messages::LOG_DRY_RUN_STOP);

    controller::join_with_timeout("capture", capture_handle, Duration::from_secs(3));
    let total = drain_handle.join().unwrap_or(0);
    info!(total_frames = total, "{}", messages::LOG_DRY_RUN_DONE);
}

/// Check system, load module, fix permissions, print guidance, exit.
pub fn run_setup(cfg: Arc<ResolvedConfig>) {
    println!("=== vcam-proxy automatic setup ===\n");

    let mut ok = true;

    print!("[1/4] Checking v4l2loopback kernel module... ");
    if sink::is_module_loaded() {
        println!("✓ already loaded");
    } else {
        println!("✗ NOT loaded");
        print!("        Attempting to load via pkexec (with auto-install)... ");
        match sink::ensure_module_loaded_with_install(DEFAULT_MODULE_PARAMS) {
            Ok(()) => {
                println!("✓ loaded successfully");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                println!("✗ failed: {e}");
                ok = false;
                messages::print_setup_install_hints();
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
                    let is_loopback = sink::is_loopback_driver(&dev.driver);
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
                        messages::print_permissions_help();
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
                messages::print_permissions_help();
            }
        }
    }

    println!("\n[4/4] Summary");
    if ok {
        messages::print_setup_ok();
    } else {
        messages::print_setup_fail();
    }
    println!("\n=== setup complete ===");
}

/// Print a formatted table of current settings and their source.
pub fn print_settings_table(cfg: &ResolvedConfig, settings: &Settings) {
    let config_path = Settings::config_path();
    let sep = "-".repeat(60);
    println!("Current settings (source: [C]LI / [F]ile / [D]efault):");
    println!("{sep}");

    row(
        "camera",
        &cfg.camera,
        source_num(cfg.camera, settings.camera, 0),
    );
    row(
        "device",
        &cfg.device,
        if cfg.device != settings.device {
            'C'
        } else {
            'D'
        },
    );
    row(
        "width",
        &cfg.width,
        source_num(cfg.width, settings.width, 1280),
    );
    row(
        "height",
        &cfg.height,
        source_num(cfg.height, settings.height, 720),
    );
    row("fps", &cfg.fps, source_num(cfg.fps, settings.fps, 30));
    row(
        "buffers",
        &cfg.buffers,
        source_num(cfg.buffers, settings.buffers, 4),
    );
    row(
        "retry_ms",
        &cfg.retry_ms,
        source_num(cfg.retry_ms, settings.retry_ms, 1000),
    );
    row(
        "multi_reader",
        &cfg.multi_reader,
        if cfg.multi_reader != settings.multi_reader {
            'C'
        } else if settings.multi_reader {
            'F'
        } else {
            'D'
        },
    );
    row(
        "devices",
        &cfg.devices,
        source_num(cfg.devices, settings.devices, 1),
    );
    row(
        "exclusive_caps",
        &cfg.exclusive_caps,
        source_num(cfg.exclusive_caps, settings.exclusive_caps, 1),
    );
    row(
        "timeout",
        &cfg.timeout,
        source_num(cfg.timeout, settings.timeout, 0),
    );
    row(
        "auto_load_module",
        &cfg.auto_load_module,
        if cfg.auto_load_module != settings.auto_load_module {
            'C'
        } else {
            'D'
        },
    );
    row(
        "auto_resolution",
        &cfg.auto_resolution,
        if cfg.auto_resolution != settings.auto_resolution {
            'C'
        } else {
            'D'
        },
    );

    println!("{sep}");
    println!("\nConfig file: {}", config_path.display());
    println!("Edit: tray → Settings…  or  vcam-proxy --edit-config");
}

fn row(name: &str, value: &impl std::fmt::Display, src: char) {
    println!("  {name:<20} {value:<15} [{src}]");
}

fn source_num<T: PartialEq>(effective: T, file: T, default: T) -> char {
    if effective != file {
        'C'
    } else if file != default {
        'F'
    } else {
        'D'
    }
}
