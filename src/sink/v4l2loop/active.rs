//! Internal streaming state for a v4l2loopback device.

use std::io;
use std::path::PathBuf;

use tracing::{debug, info, warn};
use v4l::buffer::Type;
use v4l::control::{Control, Value as CtrlValue};
use v4l::device::Device;
use v4l::format::FourCC;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::OutputStream;
use v4l::video::Output;
use v4l::Format;

use crate::frame::PixelFormat;

use super::discovery::is_loopback_driver;
/// Kernel buffers requested from the loopback driver. More buffers help when
/// multiple readers (OBS + browser) drain at slightly different rates.
pub(crate) const NUM_KBUF: u32 = 4;
/// v4l2loopback custom control IDs (keep_format, sustain_framerate, timeout).
/// These are the values current v4l2loopback builds register
/// (`V4L2_CID_USER_BASE + 0xf000`), verified against a live device; they are
/// only a fallback — the IDs are always looked up by control NAME first,
/// because they have changed across module versions and guessing wrong means
/// `keep_format` stays latched and every VIDIOC_S_FMT is silently ignored.
pub(crate) const CID_KEEP_FORMAT: u32 = 0x0098f900;
pub(crate) const CID_SUSTAIN_FRAMERATE: u32 = 0x0098f901;
pub(crate) const CID_TIMEOUT: u32 = 0x0098f902;

/// Resolved v4l2loopback control IDs: (keep_format, sustain_framerate, timeout).
#[derive(Clone, Copy)]
struct LoopbackCids {
    keep_format: u32,
    sustain_framerate: u32,
    timeout: u32,
}

/// Look up the real control IDs by name via VIDIOC_QUERYCTRL; fall back to the
/// well-known constants when the query is unavailable.
fn loopback_cids(dev: &Device) -> LoopbackCids {
    let mut cids = LoopbackCids {
        keep_format: CID_KEEP_FORMAT,
        sustain_framerate: CID_SUSTAIN_FRAMERATE,
        timeout: CID_TIMEOUT,
    };
    match dev.query_controls() {
        Ok(ctrls) => {
            for c in ctrls {
                match c.name.as_str() {
                    "keep_format" => cids.keep_format = c.id,
                    "sustain_framerate" => cids.sustain_framerate = c.id,
                    "timeout" => cids.timeout = c.id,
                    _ => {}
                }
            }
            debug!(
                keep_format = cids.keep_format,
                sustain_framerate = cids.sustain_framerate,
                timeout = cids.timeout,
                "resolved v4l2loopback control ids"
            );
        }
        Err(e) => debug!(error = %e, "control query failed; using default v4l2loopback ids"),
    }
    cids
}

pub(crate) struct Active {
    stream: MmapStream<'static>,
    #[allow(dead_code)] // kept alive: owns the device fd backing `stream`
    pub(crate) dev: Device,
    pub(crate) negotiated: (u32, u32, PixelFormat),
}

impl Active {
    pub(crate) fn open(
        path: &PathBuf,
        width: u32,
        height: u32,
        fmt: PixelFormat,
        timeout_ms: i64,
    ) -> io::Result<Self> {
        let dev = Device::with_path(path)?;

        if let Ok(caps) = dev.query_caps() {
            info!(driver = %caps.driver, card = %caps.card, bus = %caps.bus, "output device");
            if !is_loopback_driver(&caps.driver) {
                // Discovery should have filtered this out already; if we still
                // land here, writes would fail — treat it as a hard error
                // instead of streaming black frames into the wrong device.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is driven by '{}' (not v4l2loopback); refusing to use it as a virtual camera",
                        path.display(),
                        caps.driver
                    ),
                ));
            }
        }

        // Resolve the real control IDs by name (wrong IDs mean the writes
        // below silently no-op and S_FMT stays ignored).
        let cids = loopback_cids(&dev);

        // Disable keep_format so VIDIOC_S_FMT actually applies the requested
        // geometry. NOTE: v4l2loopback latches keep_format=1 as soon as any
        // capture client opens the node, so a stale format from a previous
        // session would otherwise be impossible to change.
        disable_keep_format(&dev, cids.keep_format);

        let fourcc = FourCC::new(&fmt.fourcc());
        let format = Format {
            width,
            height,
            fourcc,
            field_order: v4l::format::FieldOrder::Any,
            stride: 0,
            size: 0,
            flags: 0.into(),
            colorspace: v4l::format::Colorspace::Default,
            quantization: v4l::format::Quantization::Default,
            transfer: v4l::format::TransferFunction::Default,
        };
        // S_FMT is a negotiation and returns what the driver ACTUALLY applied;
        // use it so frame-size checks and scaling target the real on-the-wire
        // geometry instead of the requested one.
        let actual = dev.set_format(&format)?;
        if actual.fourcc != fourcc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "driver rejected pixel format {:?} (selected {} instead)",
                    fmt, actual.fourcc
                ),
            ));
        }
        if actual.width != width || actual.height != height {
            warn!(
                requested = %format_args!("{width}x{height}"),
                actual = %format_args!("{}x{}", actual.width, actual.height),
                "driver adjusted output resolution; frames will be scaled"
            );
        }

        // Re-enable keep_format + sustain_framerate so the virtual camera keeps advertising
        // a fixed format to CAPTURE clients (Chrome, Firefox, Zoom) between attaches.
        // timeout from caller: 0 = keep last frame forever (no green timeout flash).
        apply_loopback_controls(&dev, cids, timeout_ms);

        let stream = MmapStream::with_buffers(&dev, Type::VideoOutput, NUM_KBUF)?;

        Ok(Active {
            stream,
            dev,
            negotiated: (actual.width, actual.height, fmt),
        })
    }

    pub(crate) fn write(&mut self, payload: &[u8]) -> io::Result<()> {
        // Packed formats must exactly fill one video frame; a mismatch would
        // corrupt the loopback stream, so reject instead of writing partials.
        let (w, h, fmt) = self.negotiated;
        if let Some(expected) = fmt.packed_size(w, h) {
            if payload.len() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "frame ({} B) != negotiated frame size ({} B)",
                        payload.len(),
                        expected
                    ),
                ));
            }
        }

        let (buf, meta) = self.stream.next().map_err(|e| {
            // Preserve the original error kind (WouldBlock, TimedOut, etc.)
            // so the sink can distinguish back-pressure from real failures and
            // avoid disruptive device reopens on transient errors.
            io::Error::new(e.kind(), format!("failed to get output buffer: {e}"))
        })?;

        // Never overrun the kernel-mapped buffer (e.g. if the driver handed
        // back a smaller sizeimage than the frame needs) — error out instead
        // of panicking on the slice copy.
        if payload.len() > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame ({} B) exceeds kernel output buffer ({} B)",
                    payload.len(),
                    buf.len()
                ),
            ));
        }

        // CRITICAL: set bytesused to the real frame size before the next
        // OutputStream::next() call queues this buffer. Leaving it at 0 makes
        // the kernel advertise the full page-aligned mmap length (trailing
        // zeros → classic YUYV green flash). Returning early after next()
        // without filling also queues a garbage buffer on the following write.
        // See v4l's stream_forward_mmap example and V4L2 buffer docs.
        buf[..payload.len()].copy_from_slice(payload);
        meta.bytesused = payload.len() as u32;
        meta.field = 0;
        Ok(())
    }
}

/// Disable keep_format so VIDIOC_S_FMT actually applies the requested geometry.
fn disable_keep_format(dev: &Device, cid: u32) {
    match dev.set_control(Control {
        id: cid,
        value: CtrlValue::Boolean(false),
    }) {
        Ok(()) => debug!("keep_format disabled for format negotiation"),
        Err(e) => warn!(
            error = %e,
            "could not disable keep_format; a stale format may be stuck (close apps using the virtual camera)"
        ),
    }
}

/// Enable keep_format + sustain_framerate so the virtual camera keeps advertising
/// a fixed format to CAPTURE clients (Chrome, Firefox, Zoom) between attaches.
///
/// `timeout_ms`: v4l2loopback frame timeout. `0` keeps the last good frame
/// forever (avoids green timeout frames when a reader reconnects).
fn apply_loopback_controls(dev: &Device, cids: LoopbackCids, timeout_ms: i64) {
    for (id, name, value) in [
        (cids.keep_format, "keep_format", CtrlValue::Boolean(true)),
        (
            cids.sustain_framerate,
            "sustain_framerate",
            CtrlValue::Boolean(true),
        ),
        (cids.timeout, "timeout", CtrlValue::Integer(timeout_ms)),
    ] {
        match dev.set_control(Control { id, value }) {
            Ok(()) => debug!(control = name, "loopback control set"),
            Err(e) => debug!(control = name, error = %e, "loopback control not set"),
        }
    }
}
