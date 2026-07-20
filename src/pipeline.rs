//! Sink-side plumbing: receives filled frames, pushes them into the virtual
//! device, and recycles buffers into the pool. Also owns the pipeline
//! statistics counters and the periodic throughput report.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use tracing::{info, warn};

use crate::config::ResolvedConfig;
use crate::frame::{BufferPool, Frame};
use crate::shutdown::Shutdown;
use crate::sink;

#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub written: AtomicU64,
    pub dropped: AtomicU64,
}

pub fn spawn_sink(
    cfg: Arc<ResolvedConfig>,
    loopback_path: PathBuf,
    rx: Receiver<Frame>,
    pool: BufferPool,
    shutdown: Shutdown,
    stats: Arc<Stats>,
    sink_switch: crate::tray::SinkSwitch,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("sink".into())
        .spawn(move || {
            run(
                &cfg,
                &loopback_path,
                &rx,
                &pool,
                &shutdown,
                &stats,
                &sink_switch,
            )
        })
        .expect("failed to spawn sink thread")
}

fn run(
    cfg: &ResolvedConfig,
    loopback_path: &Path,
    rx: &Receiver<Frame>,
    pool: &BufferPool,
    shutdown: &Shutdown,
    stats: &Stats,
    sink_switch: &crate::tray::SinkSwitch,
) {
    let mut sink = sink::build_with_path(cfg, loopback_path);
    info!(sink = %sink.describe(), "sink ready");

    let mut last = Instant::now();
    let mut last_written = 0u64;
    let mut last_dropped = 0u64;
    let mut write_errors = 0u64;

    loop {
        if shutdown.is_set() && rx.is_empty() {
            break;
        }

        let frame = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => {
                report(&mut last, &mut last_written, &mut last_dropped, stats);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if !sink_switch.is_on() {
            // Virtual camera is toggled off: recycle frame without writing.
            pool.release(frame);
            report(&mut last, &mut last_written, &mut last_dropped, stats);
            continue;
        }

        match sink.write(&frame) {
            Ok(()) => {
                stats.written.fetch_add(1, Ordering::Relaxed);
                write_errors = 0;
            }
            // No consumer draining the virtual device right now: drop
            // gracefully, this is the designed back-pressure escape hatch.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                write_errors += 1;
                // Throttle logs while a missing/busy device keeps failing.
                if write_errors == 1 || write_errors % 120 == 0 {
                    warn!(count = write_errors, "sink write failed: {e}");
                }
            }
        }
        pool.release(frame);
        report(&mut last, &mut last_written, &mut last_dropped, stats);
    }

    // Recycle anything still queued so no buffer is stranded.
    while let Ok(f) = rx.try_recv() {
        pool.release(f);
    }
    info!("sink thread exit");
}

fn report(last: &mut Instant, last_w: &mut u64, last_d: &mut u64, stats: &Stats) {
    if last.elapsed() < Duration::from_secs(5) {
        return;
    }
    let secs = last.elapsed().as_secs_f64();
    let w = stats.written.load(Ordering::Relaxed);
    let d = stats.dropped.load(Ordering::Relaxed);
    let c = stats.captured.load(Ordering::Relaxed);
    info!(
        fps = format_args!("{:.1}", (w - *last_w) as f64 / secs),
        captured = c,
        written = w,
        dropped_total = d,
        dropped_delta = d - *last_d,
        "pipeline stats"
    );
    *last = Instant::now();
    *last_w = w;
    *last_d = d;
}
