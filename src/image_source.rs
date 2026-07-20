//! Static-image capture source for deterministic testing and demos.
//!
//! Loads an image file, converts it to the wire format (YUYV by default), and
//! re-emits the same frame at the configured FPS. Used by `--image` and by the
//! loopback pixel-integrity integration suite.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use tracing::{info, warn};

use crate::config::{FormatPref, ResolvedConfig};
use crate::convert;
use crate::frame::{BufferPool, Frame, PixelFormat};
use crate::pipeline::Stats;
use crate::shutdown::Shutdown;

/// Load `path`, letterbox/center-crop into `width`×`height`, and pack as YUYV
/// (or NV12 / RGB24 if explicitly requested).
pub fn load_frame(path: &Path, width: u32, height: u32, pref: FormatPref) -> Result<Frame, String> {
    let img = image::open(path)
        .map_err(|e| format!("failed to open image {}: {e}", path.display()))?
        .to_rgb8();

    let (src_w, src_h) = img.dimensions();
    let mut rgb = vec![0u8; (width as usize) * (height as usize) * 3];
    // Nearest-neighbor scale into the target canvas (deterministic, no deps).
    for y in 0..height {
        for x in 0..width {
            let sx = (x as u64 * src_w as u64 / width as u64) as u32;
            let sy = (y as u64 * src_h as u64 / height as u64) as u32;
            let p = img.get_pixel(sx.min(src_w - 1), sy.min(src_h - 1));
            let i = ((y * width + x) * 3) as usize;
            rgb[i] = p[0];
            rgb[i + 1] = p[1];
            rgb[i + 2] = p[2];
        }
    }

    // YUYV needs even width; bump down by 1 if needed so packing never fails.
    let w = width & !1;
    let h = height;
    if w == 0 || h == 0 {
        return Err("image output size must be non-zero (and width even for YUYV)".into());
    }

    let (fmt, out_len) = match pref {
        FormatPref::Nv12 => (
            PixelFormat::Nv12,
            PixelFormat::Nv12.packed_size(w, h).unwrap(),
        ),
        FormatPref::Rgb24 => (
            PixelFormat::Rgb24,
            PixelFormat::Rgb24.packed_size(w, h).unwrap(),
        ),
        // Auto / Yuy2 / Mjpeg → YUYV (universally accepted wire format).
        FormatPref::Auto | FormatPref::Yuy2 | FormatPref::Mjpeg => (
            PixelFormat::Yuy2,
            PixelFormat::Yuy2.packed_size(w, h).unwrap(),
        ),
    };

    let mut frame = Frame {
        buf: vec![0; out_len],
        len: out_len,
        width: w,
        height: h,
        format: fmt,
        seq: 0,
        ts: Instant::now(),
    };

    let ok = match fmt {
        PixelFormat::Yuy2 => convert::rgb24_to_yuy2(&rgb, frame.payload_mut(out_len), w, h),
        PixelFormat::Nv12 => convert::rgb24_to_nv12(&rgb, frame.payload_mut(out_len), w, h),
        PixelFormat::Rgb24 => {
            frame.payload_mut(out_len).copy_from_slice(&rgb[..out_len]);
            true
        }
        PixelFormat::Mjpeg => false,
    };
    if !ok {
        return Err(format!("failed to pack image as {fmt:?}"));
    }
    Ok(frame)
}

pub fn spawn(
    cfg: Arc<ResolvedConfig>,
    image_path: std::path::PathBuf,
    pool: BufferPool,
    tx: Sender<Frame>,
    shutdown: Shutdown,
    stats: Arc<Stats>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("image-source".into())
        .spawn(move || run(&cfg, &image_path, &pool, &tx, &shutdown, &stats))
        .expect("failed to spawn image-source thread")
}

fn run(
    cfg: &ResolvedConfig,
    image_path: &Path,
    pool: &BufferPool,
    tx: &Sender<Frame>,
    shutdown: &Shutdown,
    stats: &Stats,
) {
    let template = match load_frame(image_path, cfg.width, cfg.height, cfg.format) {
        Ok(f) => {
            info!(
                path = %image_path.display(),
                width = f.width,
                height = f.height,
                format = ?f.format,
                "image source loaded"
            );
            f
        }
        Err(e) => {
            warn!("image source failed: {e}");
            return;
        }
    };

    let period = Duration::from_nanos(1_000_000_000 / cfg.fps.max(1) as u64);
    let mut seq = 0u64;
    let mut next_tick = Instant::now();

    while !shutdown.is_set() {
        let now = Instant::now();
        if now < next_tick {
            let sleep_for = next_tick - now;
            // Wake often enough to notice Ctrl+C promptly.
            thread::sleep(sleep_for.min(Duration::from_millis(50)));
            continue;
        }
        next_tick += period;
        // If we fell far behind (debugger / heavy load), resync so we don't
        // blast a backlog of catch-up frames.
        if next_tick + period < Instant::now() {
            next_tick = Instant::now() + period;
        }

        let Some(mut frame) = pool.try_acquire() else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        let n = template.len;
        frame.payload_mut(n).copy_from_slice(template.payload());
        frame.width = template.width;
        frame.height = template.height;
        frame.format = template.format;
        frame.seq = seq;
        frame.ts = Instant::now();
        seq += 1;
        stats.captured.fetch_add(1, Ordering::Relaxed);

        match tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(f)) | Err(TrySendError::Disconnected(f)) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                pool.release(f);
            }
        }
    }
    info!(frames = seq, "image source exit");
}
