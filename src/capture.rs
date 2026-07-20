//! Hardware ingestion thread (nokhwa: V4L2 / Media Foundation / AVFoundation).
//!
//! The thread owns the `Camera` handle exclusively (it is `!Sync`). On any
//! stream error the camera is dropped, re-opened after a backoff, and the
//! loop resumes — USB disconnects and driver hiccups are recovered
//! automatically. Blocking calls (`Camera::frame`) are the only suspension
//! points; the shutdown flag is checked between frames.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::{Buffer, Camera, NokhwaError};
use tracing::{debug, info, warn};

use crate::config::{Config, FormatPref};
use crate::convert;
use crate::frame::{BufferPool, Frame, PixelFormat};
use crate::pipeline::Stats;
use crate::shutdown::Shutdown;

/// One-time platform camera stack initialization (Windows needs Media
/// Foundation brought up before any query/open).
fn platform_init() {
    #[cfg(target_os = "windows")]
    {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            nokhwa::nokhwa_initialize(|granted| {
                if !granted {
                    tracing::warn!("camera access not granted by the OS");
                }
            });
        });
    }
}

/// Enumerate physical cameras for `--list`.
pub fn list_cameras() {
    platform_init();

    match nokhwa::query(ApiBackend::Auto) {
        Ok(cams) if cams.is_empty() => println!("no cameras found"),
        Ok(cams) => {
            for c in cams {
                println!("{c}");
            }
        }
        Err(e) => eprintln!("camera query failed: {e}"),
    }
}

pub fn spawn(
    cfg: Arc<Config>,
    pool: BufferPool,
    tx: Sender<Frame>,
    shutdown: Shutdown,
    stats: Arc<Stats>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("capture".into())
        .spawn(move || run(&cfg, &pool, &tx, &shutdown, &stats))
        .expect("failed to spawn capture thread")
}

fn run(cfg: &Config, pool: &BufferPool, tx: &Sender<Frame>, shutdown: &Shutdown, stats: &Stats) {
    let backoff = Duration::from_millis(cfg.retry_ms);

    while !shutdown.is_set() {
        match open_camera(cfg) {
            Ok(mut cam) => {
                info!(
                    index = cfg.camera,
                    format = ?cam.camera_format(),
                    "camera stream open"
                );
                if let Err(e) = capture_loop(&mut cam, cfg, pool, tx, shutdown, stats) {
                    warn!("camera stream lost: {e}");
                }
                let _ = cam.stop_stream();
                info!("camera closed; scheduling re-init");
            }
            Err(e) => warn!("camera open failed: {e}"),
        }

        if !shutdown.is_set() {
            // Interruptible backoff.
            let deadline = Instant::now() + backoff;
            while !shutdown.is_set() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    info!("capture thread exit");
}

fn open_camera(cfg: &Config) -> Result<Camera, NokhwaError> {
    platform_init();

    let res = Resolution::new(cfg.width, cfg.height);
    // Acceptable on-the-wire source formats; the closest match to the
    // requested geometry is picked among these.
    let accept: &[FrameFormat] = match cfg.format {
        FormatPref::Auto => &[FrameFormat::YUYV, FrameFormat::MJPEG, FrameFormat::NV12],
        FormatPref::Yuy2 => &[FrameFormat::YUYV],
        FormatPref::Rgb24 => &[FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12],
        FormatPref::Nv12 => &[FrameFormat::NV12],
        FormatPref::Mjpeg => &[FrameFormat::MJPEG],
    };
    // Bias the negotiation: Auto prefers uncompressed YUYV (cheapest path),
    // everything else prefers the first entry of its accept list.
    let preferred = accept[0];
    let wanted = CameraFormat::new(res, preferred, cfg.fps);
    let request =
        RequestedFormat::with_formats(RequestedFormatType::Closest(wanted), accept);

    let mut cam = Camera::new(CameraIndex::Index(cfg.camera), request)?;
    cam.open_stream()?;
    Ok(cam)
}

fn capture_loop(
    cam: &mut Camera,
    cfg: &Config,
    pool: &BufferPool,
    tx: &Sender<Frame>,
    shutdown: &Shutdown,
    stats: &Stats,
) -> Result<(), NokhwaError> {
    let mut seq = 0u64;

    while !shutdown.is_set() {
        let raw = cam.frame()?; // blocks at camera frame rate
        let res = raw.resolution();

        // No free slot -> the sink is behind; drop at the source instead of
        // building back-pressure (bounded latency, no queue bloat).
        let Some(mut frame) = pool.try_acquire() else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        match fill(&mut frame, &raw, cfg.format) {
            Ok(true) => {
                frame.width = res.width();
                frame.height = res.height();
                frame.seq = seq;
                frame.ts = Instant::now();
                seq += 1;
                stats.captured.fetch_add(1, Ordering::Relaxed);

                match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(TrySendError::Full(f)) => {
                        stats.dropped.fetch_add(1, Ordering::Relaxed);
                        pool.release(f);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Ok(false) => pool.release(frame), // skipped (unsupported source)
            Err(e) => {
                pool.release(frame);
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Convert/copy a raw camera buffer into a pooled frame.
/// Returns `Ok(false)` for source formats we cannot serve.
fn fill(frame: &mut Frame, raw: &Buffer, pref: FormatPref) -> Result<bool, NokhwaError> {
    let res = raw.resolution();
    let (w, h) = (res.width(), res.height());
    let src = raw.buffer();

    let passthrough = |frame: &mut Frame, fmt: PixelFormat| {
        frame.payload_mut(src.len()).copy_from_slice(src);
        frame.format = fmt;
    };

    match raw.source_frame_format() {
        FrameFormat::YUYV => match pref {
            // Zero-conversion fast path: copy YUY2 verbatim.
            FormatPref::Auto | FormatPref::Yuy2 => {
                // Guard against malformed/strided buffers from quirky drivers.
                if PixelFormat::Yuy2.packed_size(w, h) != Some(src.len()) {
                    return Ok(false);
                }
                passthrough(frame, PixelFormat::Yuy2)
            }
            FormatPref::Rgb24 => {
                let n = w as usize * h as usize * 3;
                if !convert::yuy2_to_rgb24(src, frame.payload_mut(n), w, h) {
                    return Ok(false);
                }
                frame.format = PixelFormat::Rgb24;
            }
            _ => return Ok(false),
        },
        FrameFormat::NV12 => match pref {
            FormatPref::Nv12 => passthrough(frame, PixelFormat::Nv12),
            FormatPref::Auto | FormatPref::Rgb24 => {
                let n = w as usize * h as usize * 3;
                if !convert::nv12_to_rgb24(src, frame.payload_mut(n), w, h) {
                    return Ok(false);
                }
                frame.format = PixelFormat::Rgb24;
            }
            _ => return Ok(false),
        },
        FrameFormat::MJPEG => match pref {
            FormatPref::Mjpeg => passthrough(frame, PixelFormat::Mjpeg),
            _ => {
                let n = w as usize * h as usize * 3;
                raw.decode_image_to_buffer::<RgbFormat>(frame.payload_mut(n))?;
                frame.format = PixelFormat::Rgb24;
            }
        },
        other => {
            debug!(?other, "uncommon source format; attempting RGB decode");
            let n = w as usize * h as usize * 3;
            raw.decode_image_to_buffer::<RgbFormat>(frame.payload_mut(n))?;
            frame.format = PixelFormat::Rgb24;
        }
    }
    Ok(true)
}
