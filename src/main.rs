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
mod shutdown;
mod sink;
mod tray;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::Config;
use frame::BufferPool;
use pipeline::Stats;
use shutdown::Shutdown;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::parse());

    if cfg.list {
        capture::list_cameras();
        return;
    }

    // Handle --list-loopback: enumerate output devices, print, exit.
    if cfg.list_loopback {
        list_loopback_devices();
        return;
    }

    info!(
        camera = cfg.camera,
        width = cfg.width,
        height = cfg.height,
        fps = cfg.fps,
        format = ?cfg.format,
        buffers = cfg.buffers,
        "starting vcam-proxy"
    );

    // Handle dry-run mode: no loopback output, just test capture.
    if cfg.dry_run {
        info!("dry-run mode: capture only, no virtual camera output");
        return run_dry_run(cfg);
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
                         Or use --auto-load-module to attempt automatic loading."
                    );
                }
                sink::LoopbackError::ScanFailed { source } => {
                    eprintln!("Error scanning for video devices: {source}");
                }
            }
            std::process::exit(1);
        }
    };

    // Optionally auto-load the v4l2loopback module via pkexec.
    if cfg.auto_load_module && !sink::is_module_loaded() {
        if let Err(e) = sink::load_module() {
            match &e {
                sink::ModuleError::PkexecNotAvailable => {
                    eprintln!(
                        "Note: pkexec not available. Run manually:\n  sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1"
                    );
                }
                _ => {
                    eprintln!("Failed to auto-load v4l2loopback module: {e}");
                }
            }
        }
        // Re-discover device after loading module
        if sink::find_loopback_device(Path::new(&cfg.device)).is_err() {
            eprintln!("Still no loopback device after module load");
            std::process::exit(1);
        }
    }

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

    let shutdown = Shutdown::install();

    // Shared switch: tray toggles it, sink loop reads it. Allows the user to
    // start/stop the virtual camera output without tearing down capture.
    let sink_switch = tray::SinkSwitch::new(true);

    // Spawn the system-tray icon for on/off toggle (unless disabled).
    let tray_handle = if cfg.no_tray {
        info!("tray icon disabled via --no-tray");
        None
    } else {
        // Gracefully continues without a tray if D-Bus is unavailable.
        tray::spawn(sink_switch.clone(), shutdown.clone())
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

    if let Err(e) = capture_handle.join() {
        tracing::error!("capture thread panicked: {e:?}");
    }
    if let Err(e) = sink_handle.join() {
        tracing::error!("sink thread panicked: {e:?}");
    }
    if let Some(h) = tray_handle {
        let _ = h.join();
    }

    info!("all threads stopped; descriptors released");
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
fn run_dry_run(cfg: Arc<Config>) {
    let shutdown = Shutdown::install();
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

    if let Err(e) = capture_handle.join() {
        tracing::error!("capture thread panicked: {e:?}");
    }
    let total = drain_handle.join().unwrap_or(0);
    info!(total_frames = total, "dry-run complete");
}
