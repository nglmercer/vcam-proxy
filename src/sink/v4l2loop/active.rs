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
/// v4l2loopback control IDs for keep_format, sustain_framerate, timeout.
pub(crate) const CID_KEEP_FORMAT: u32 = 0x00982000 + 1;
pub(crate) const CID_SUSTAIN_FRAMERATE: u32 = 0x00982000 + 2;
pub(crate) const CID_TIMEOUT: u32 = 0x00982000 + 3;

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

        // Disable keep_format so VIDIOC_S_FMT actually applies the requested geometry.
        disable_keep_format(&dev);

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
        apply_loopback_controls(&dev);

        let stream = MmapStream::with_buffers(&dev, Type::VideoOutput, NUM_KBUF)?;

        Ok(Active {
            stream,
            dev,
            negotiated: (actual.width, actual.height, fmt),
        })
    }

    pub(crate) fn write(&mut self, payload: &[u8]) -> io::Result<()> {
        let (buf, meta) = self.stream.next().map_err(|e| {
            io::Error::other(format!("failed to get output buffer: {e}"))
        })?;

        if meta.bytesused != 0 {
            // Buffer still held by kernel (no reader drained it yet). Drop this
            // frame rather than block — the sink thread stays responsive.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no consumer attached to virtual device",
            ));
        }

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
        buf[..payload.len()].copy_from_slice(payload);
        Ok(())
    }
}

/// Disable keep_format so VIDIOC_S_FMT actually applies the requested geometry.
fn disable_keep_format(dev: &Device) {
    match dev.set_control(Control {
        id: CID_KEEP_FORMAT,
        value: CtrlValue::Boolean(false),
    }) {
        Ok(()) => debug!("keep_format disabled for format negotiation"),
        Err(e) => debug!(error = %e, "keep_format disable not set (ok on old modules)"),
    }
}

/// Enable keep_format + sustain_framerate so the virtual camera keeps advertising
/// a fixed format to CAPTURE clients (Chrome, Firefox, Zoom) between attaches.
fn apply_loopback_controls(dev: &Device) {
    for (id, name, value) in [
        (CID_KEEP_FORMAT, "keep_format", CtrlValue::Boolean(true)),
        (
            CID_SUSTAIN_FRAMERATE,
            "sustain_framerate",
            CtrlValue::Boolean(true),
        ),
        (CID_TIMEOUT, "timeout", CtrlValue::Integer(3000)),
    ] {
        match dev.set_control(Control { id, value }) {
            Ok(()) => debug!(control = name, "loopback control set"),
            Err(e) => debug!(control = name, error = %e, "loopback control not set"),
        }
    }
}
