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

/// Soft cap for auto-negotiated capture geometry. Browsers (Chrome/Firefox
/// WebRTC via PipeWire) frequently fail or refuse 4K virtual cameras; OBS
/// handles them fine. Users who want more can pass explicit `--width/--height`.
const AUTO_MAX_WIDTH: u32 = 1920;
const AUTO_MAX_HEIGHT: u32 = 1080;

/// Acceptable on-the-wire source formats for a given preference, ordered by
/// desirability. The first entry is the negotiation bias.
///
/// Auto prefers uncompressed YUYV when available (zero-copy browser-friendly
/// path), then MJPEG (typical USB high-res), then NV12.
fn accept_formats(pref: FormatPref) -> &'static [FrameFormat] {
    match pref {
        FormatPref::Auto => &[FrameFormat::YUYV, FrameFormat::MJPEG, FrameFormat::NV12],
        FormatPref::Yuy2 => &[FrameFormat::YUYV, FrameFormat::MJPEG],
        FormatPref::Rgb24 => &[FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12],
        FormatPref::Nv12 => &[FrameFormat::NV12, FrameFormat::MJPEG, FrameFormat::YUYV],
        FormatPref::Mjpeg => &[FrameFormat::MJPEG],
    }
}

/// Ranking used only as a tie-break at the same resolution: uncompressed
/// browser-friendly formats beat MJPEG (which needs a CPU decode).
fn format_rank(f: FrameFormat) -> u8 {
    match f {
        FrameFormat::YUYV => 3,
        FrameFormat::NV12 => 2,
        FrameFormat::MJPEG => 1,
        _ => 0,
    }
}

fn open_camera(cfg: &ResolvedConfig) -> Result<Camera, NokhwaError> {
    platform_init();

    let accept = accept_formats(cfg.format);

    // Auto mode: query the camera's real capabilities and pin the best mode
    // within a browser-friendly resolution budget. Exact request is required
    // so `Closest` cannot re-pick a smaller mode.
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

/// Open the camera, query its supported modes, and pin the best mode among the
/// acceptable formats — preferring ≤1080p for virtual-camera / browser
/// compatibility. Returns `Ok(None)` if the backend cannot enumerate formats
/// so the caller can fall back to fixed geometry.
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

    let usable: Vec<_> = formats
        .into_iter()
        .filter(|f| accept.contains(&f.format()))
        .collect();
    if usable.is_empty() {
        return Ok(None);
    }

    // Prefer modes that fit inside 1080p so Chrome/Firefox accept the virtual
    // device. Only escalate to a larger mode if the camera has nothing smaller.
    let within_budget: Vec<_> = usable
        .iter()
        .filter(|f| f.width() <= AUTO_MAX_WIDTH && f.height() <= AUTO_MAX_HEIGHT)
        .cloned()
        .collect();
    let pool = if within_budget.is_empty() {
        warn!(
            "camera has no mode ≤{AUTO_MAX_WIDTH}x{AUTO_MAX_HEIGHT}; using full camera max \
             (browsers may refuse the virtual device)"
        );
        usable
    } else {
        within_budget
    };

    // Largest area, then highest fps, then cheapest source format.
    let best = pool
        .into_iter()
        .max_by_key(|f| {
            (
                f.width() as u64 * f.height() as u64,
                f.frame_rate(),
                format_rank(f.format()),
            )
        })
        .expect("pool non-empty");

    info!(
        width = best.width(),
        height = best.height(),
        fps = best.frame_rate(),
        format = ?best.format(),
        "auto-resolution selected mode"
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
///
/// **Wire-format policy for browsers**: Chrome/Firefox WebRTC (and most
/// PipeWire camera portals) accept YUYV and NV12 from a v4l2loopback device,
/// but reject RGB24 and often fail on MJPEG loopback. Auto therefore never
/// emits RGB24 or MJPEG on the virtual camera — it always lands on YUYV/NV12.
///
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
            // Zero-conversion fast path: YUYV is the most widely accepted
            // browser capture format.
            FormatPref::Auto | FormatPref::Yuy2 => {
                // Guard against malformed/strided buffers from quirky drivers.
                if PixelFormat::Yuy2.packed_size(w, h) != Some(src.len()) {
                    return Ok(false);
                }
                passthrough(frame, PixelFormat::Yuy2)
            }
            FormatPref::Nv12 => {
                // Decode YUYV→RGB→NV12 only when the user forced NV12.
                if !yuyv_to_nv12(frame, src, w, h) {
                    return Ok(false);
                }
            }
            FormatPref::Rgb24 => {
                let n = w as usize * h as usize * 3;
                if !convert::yuy2_to_rgb24(src, frame.payload_mut(n), w, h) {
                    return Ok(false);
                }
                frame.format = PixelFormat::Rgb24;
            }
            FormatPref::Mjpeg => return Ok(false),
        },
        FrameFormat::NV12 => match pref {
            // NV12 is browser-friendly: Auto / Nv12 pass it through.
            FormatPref::Auto | FormatPref::Nv12 => {
                if PixelFormat::Nv12.packed_size(w, h) != Some(src.len()) {
                    return Ok(false);
                }
                passthrough(frame, PixelFormat::Nv12)
            }
            FormatPref::Yuy2 => {
                // NV12→YUYV via RGB scratch (rare path).
                if !decode_plane_to_yuy2(frame, |rgb| convert::nv12_to_rgb24(src, rgb, w, h), w, h)
                {
                    return Ok(false);
                }
            }
            FormatPref::Rgb24 => {
                let n = w as usize * h as usize * 3;
                if !convert::nv12_to_rgb24(src, frame.payload_mut(n), w, h) {
                    return Ok(false);
                }
                frame.format = PixelFormat::Rgb24;
            }
            FormatPref::Mjpeg => return Ok(false),
        },
        FrameFormat::MJPEG => match pref {
            // Compressed passthrough only when the user explicitly asked for
            // it — browsers usually cannot open MJPEG loopback devices.
            FormatPref::Mjpeg => passthrough(frame, PixelFormat::Mjpeg),
            // Auto / Nv12: MJPEG → NV12 (half the bandwidth of YUYV, accepted
            // by Chrome/Firefox). Explicit Yuy2 forces YUYV instead.
            FormatPref::Auto | FormatPref::Nv12 => {
                if !decode_mjpeg_to(frame, raw, w, h, PixelFormat::Nv12)? {
                    return Ok(false);
                }
            }
            FormatPref::Yuy2 => {
                if !decode_mjpeg_to(frame, raw, w, h, PixelFormat::Yuy2)? {
                    return Ok(false);
                }
            }
            FormatPref::Rgb24 => {
                let n = w as usize * h as usize * 3;
                raw.decode_image_to_buffer::<RgbFormat>(frame.payload_mut(n))?;
                frame.format = PixelFormat::Rgb24;
            }
        },
        other => {
            debug!(?other, "uncommon source format; decoding via RGB");
            match pref {
                // Keep the virtual cam browser-safe even for exotic sources.
                FormatPref::Auto | FormatPref::Nv12 => {
                    if !decode_mjpeg_to(frame, raw, w, h, PixelFormat::Nv12)? {
                        return Ok(false);
                    }
                }
                FormatPref::Yuy2 => {
                    if !decode_mjpeg_to(frame, raw, w, h, PixelFormat::Yuy2)? {
                        return Ok(false);
                    }
                }
                FormatPref::Rgb24 | FormatPref::Mjpeg => {
                    let n = w as usize * h as usize * 3;
                    raw.decode_image_to_buffer::<RgbFormat>(frame.payload_mut(n))?;
                    frame.format = PixelFormat::Rgb24;
                }
            }
        }
    }
    Ok(true)
}

thread_local! {
    /// Scratch RGB24 buffer reused across frames on the capture thread so the
    /// MJPEG/NV12 conversion paths allocate at most once per resolution change.
    static RGB_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Decode a compressed/raw buffer to RGB (scratch) then repack to `out_fmt`
/// (NV12 or YUY2). Reuses [`RGB_SCRATCH`] so steady-state is allocation-free.
fn decode_mjpeg_to(
    frame: &mut Frame,
    raw: &Buffer,
    w: u32,
    h: u32,
    out_fmt: PixelFormat,
) -> Result<bool, NokhwaError> {
    let rgb_len = w as usize * h as usize * 3;
    let out_len = match out_fmt.packed_size(w, h) {
        Some(n) => n,
        None => return Ok(false),
    };

    RGB_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.len() < rgb_len {
            scratch.resize(rgb_len, 0);
        }
        raw.decode_image_to_buffer::<RgbFormat>(&mut scratch[..rgb_len])?;

        let ok = match out_fmt {
            PixelFormat::Nv12 => {
                convert::rgb24_to_nv12(&scratch[..rgb_len], frame.payload_mut(out_len), w, h)
            }
            PixelFormat::Yuy2 => {
                convert::rgb24_to_yuy2(&scratch[..rgb_len], frame.payload_mut(out_len), w, h)
            }
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
        frame.format = out_fmt;
        Ok(true)
    })
}

/// YUYV → NV12 via a one-shot RGB intermediate (only used for `--format nv12`).
fn yuyv_to_nv12(frame: &mut Frame, src: &[u8], w: u32, h: u32) -> bool {
    let rgb_len = w as usize * h as usize * 3;
    let nv12_len = match PixelFormat::Nv12.packed_size(w, h) {
        Some(n) => n,
        None => return false,
    };
    RGB_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.len() < rgb_len {
            scratch.resize(rgb_len, 0);
        }
        if !convert::yuy2_to_rgb24(src, &mut scratch[..rgb_len], w, h) {
            return false;
        }
        if !convert::rgb24_to_nv12(&scratch[..rgb_len], frame.payload_mut(nv12_len), w, h) {
            return false;
        }
        frame.format = PixelFormat::Nv12;
        true
    })
}

/// Helper: fill RGB via `to_rgb`, then repack to YUY2.
fn decode_plane_to_yuy2(
    frame: &mut Frame,
    to_rgb: impl FnOnce(&mut [u8]) -> bool,
    w: u32,
    h: u32,
) -> bool {
    let rgb_len = w as usize * h as usize * 3;
    let yuy2_len = match PixelFormat::Yuy2.packed_size(w, h) {
        Some(n) => n,
        None => return false,
    };
    RGB_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.len() < rgb_len {
            scratch.resize(rgb_len, 0);
        }
        if !to_rgb(&mut scratch[..rgb_len]) {
            return false;
        }
        if !convert::rgb24_to_yuy2(&scratch[..rgb_len], frame.payload_mut(yuy2_len), w, h) {
            return false;
        }
        frame.format = PixelFormat::Yuy2;
        true
    })
}
