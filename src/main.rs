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

    info!(
        camera = cfg.camera,
        width = cfg.width,
        height = cfg.height,
        fps = cfg.fps,
        format = ?cfg.format,
        buffers = cfg.buffers,
        "starting vcam-proxy"
    );

    let shutdown = Shutdown::install();

    // Shared switch: tray toggles it, sink loop reads it. Allows the user to
    // start/stop the virtual camera output without tearing down capture.
    let sink_switch = tray::SinkSwitch::new(true);

    // Spawn the system-tray icon for on/off toggle. Gracefully continues
    // without a tray if D-Bus is unavailable (e.g. SSH session, no desktop).
    let tray_handle = tray::spawn(sink_switch.clone(), shutdown.clone());

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
