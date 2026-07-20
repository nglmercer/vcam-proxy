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

use crate::config::{FormatPref, ResolvedConfig};
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
    cfg: Arc<ResolvedConfig>,
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

fn run(
    cfg: &ResolvedConfig,
    pool: &BufferPool,
    tx: &Sender<Frame>,
    shutdown: &Shutdown,
    stats: &Stats,
) {
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

/// Acceptable on-the-wire source formats for a given preference, ordered by
/// desirability. The first entry is the negotiation bias.
fn accept_formats(pref: FormatPref) -> &'static [FrameFormat] {
    match pref {
        FormatPref::Auto => &[FrameFormat::YUYV, FrameFormat::MJPEG, FrameFormat::NV12],
        FormatPref::Yuy2 => &[FrameFormat::YUYV],
        FormatPref::Rgb24 => &[FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12],
        FormatPref::Nv12 => &[FrameFormat::NV12],
        FormatPref::Mjpeg => &[FrameFormat::MJPEG],
    }
}

fn open_camera(cfg: &ResolvedConfig) -> Result<Camera, NokhwaError> {
    platform_init();

    let accept = accept_formats(cfg.format);

    // Auto mode: query the camera's real capabilities and pin the highest
    // resolution it supports (fps as tiebreak). This must use an *exact*
    // request, otherwise `Closest` may re-pick a smaller mode.
    if cfg.auto_resolution {
        if let Some(cam) = try_open_max_resolution(cfg, accept)? {
            return Ok(cam);
        }
        // Capability query failed or returned nothing usable — fall through to
        // the fixed-geometry path below using the sanitized fallback values.
        warn!("auto-resolution query unavailable; falling back to configured geometry");
    }

    let res = Resolution::new(cfg.width, cfg.height);
    // Bias the negotiation: Auto prefers uncompressed YUYV (cheapest path),
    // everything else prefers the first entry of its accept list.
    let preferred = accept[0];
    let wanted = CameraFormat::new(res, preferred, cfg.fps);
    let request = RequestedFormat::with_formats(RequestedFormatType::Closest(wanted), accept);

    let mut cam = Camera::new(CameraIndex::Index(cfg.camera), request)?;
    cam.open_stream()?;
    Ok(cam)
}

/// Open the camera, query its supported modes, and pin the highest-resolution
/// mode among the acceptable formats. Returns `Ok(None)` if the backend cannot
/// enumerate formats so the caller can fall back to fixed geometry.
fn try_open_max_resolution(
    cfg: &ResolvedConfig,
    accept: &[FrameFormat],
) -> Result<Option<Camera>, NokhwaError> {
    // Open with a permissive request first so we can query capabilities.
    let probe_req = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
    let mut probe = Camera::new(CameraIndex::Index(cfg.camera), probe_req)?;

    let formats = match probe.compatible_camera_formats() {
        Ok(f) if !f.is_empty() => f,
        _ => return Ok(None),
    };

    // Restrict to formats we can actually serve, then pick the largest frame
    // (width*height), breaking ties by the highest frame rate.
    let best = formats
        .into_iter()
        .filter(|f| accept.contains(&f.format()))
        .max_by_key(|f| (f.width() as u64 * f.height() as u64, f.frame_rate()));

    let Some(best) = best else {
        return Ok(None);
    };

    info!(
        width = best.width(),
        height = best.height(),
        fps = best.frame_rate(),
        format = ?best.format(),
        "auto-resolution selected camera max mode"
    );

    // Drop the probe handle before reopening with the pinned format so we don't
    // hold two handles to the same device (some backends are exclusive).
    drop(probe);

    // Reopen requesting the exact winning mode (avoids the deprecated
    // set_camera_format and guarantees the backend honours our choice).
    let request = RequestedFormat::with_formats(RequestedFormatType::Exact(best), accept);
    let mut cam = Camera::new(CameraIndex::Index(cfg.camera), request)?;
    cam.open_stream()?;
    Ok(Some(cam))
}

fn capture_loop(
    cam: &mut Camera,
    cfg: &ResolvedConfig,
    pool: &BufferPool,
    tx: &Sender<Frame>,
    shutdown: &Shutdown,
    stats: &Stats,
) -> Result<(), NokhwaError> {
    let mut seq = 0u64;

    loop {
        // Shutdown check BEFORE the blocking frame read.
        if shutdown.is_set() {
            break;
        }

        // cam.frame() blocks at the camera frame rate. Errors other than
        // ReadError propagate up (camera re-open handled by caller).
        let raw = match cam.frame() {
            Ok(raw) => raw,
            Err(NokhwaError::ReadFrameError(_)) => {
                // Transient read error — retry after a brief sleep.
                // The sleep is short so shutdown stays responsive.
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(e),
        };

        // Shutdown check AFTER the frame — exit quickly if signaled.
        if shutdown.is_set() {
            break;
        }

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
                seq += 1;
                frame.seq = seq;
                frame.ts = Instant::now();
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
            // Zero-conversion fast path: copy YUY2 verbatim. YUYV is already a
            // browser-friendly capture format so Auto passes it through.
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
            // NV12 is browser-friendly: Auto passes it straight through.
            FormatPref::Auto | FormatPref::Nv12 => passthrough(frame, PixelFormat::Nv12),
            FormatPref::Rgb24 => {
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
            // Auto: decode MJPEG then repack to NV12 so browsers accept the
            // virtual device (RGB24 is rejected by Chrome/Firefox WebRTC).
            FormatPref::Auto => {
                if !decode_mjpeg_to_nv12(frame, raw, w, h)? {
                    return Ok(false);
                }
            }
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

thread_local! {
    /// Scratch RGB24 buffer reused across frames on the capture thread so the
    /// MJPEG->NV12 path allocates at most once per resolution change.
    static RGB_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Decode an MJPEG buffer to RGB24 (scratch) then repack to NV12 in `frame`.
/// Returns `Ok(false)` if the geometry is not NV12-compatible (odd dims).
fn decode_mjpeg_to_nv12(
    frame: &mut Frame,
    raw: &Buffer,
    w: u32,
    h: u32,
) -> Result<bool, NokhwaError> {
    let rgb_len = w as usize * h as usize * 3;
    let nv12_len = w as usize * h as usize * 3 / 2;

    RGB_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.len() < rgb_len {
            scratch.resize(rgb_len, 0);
        }
        raw.decode_image_to_buffer::<RgbFormat>(&mut scratch[..rgb_len])?;

        if !convert::rgb24_to_nv12(&scratch[..rgb_len], frame.payload_mut(nv12_len), w, h) {
            return Ok(false);
        }
        frame.format = PixelFormat::Nv12;
        Ok(true)
    })
}
