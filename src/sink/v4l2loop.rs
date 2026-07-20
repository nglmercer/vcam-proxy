//! Linux sink: streams frames into a v4l2loopback device node.
//!
//! Kernel interface used (via the `v4l` crate):
//! - `VIDIOC_QUERYCAP` to log driver/card identification,
//! - `VIDIOC_S_FMT` to negotiate width/height/pixelformat,
//! - `VIDIOC_REQBUFS` + `mmap` for kernel-allocated, userspace-mapped buffers,
//! - `VIDIOC_QBUF`/`VIDIOC_DQBUF` to cycle frames (`V4L2_BUF_TYPE_VIDEO_OUTPUT`),
//! - `VIDIOC_STREAMON`/`STREAMOFF` on start/stop (handled by the stream impl).
//!
//! Frame data lands directly in the kernel-mapped buffer — the only copy is
//! `memcpy` from our pooled slot into the mmap region; nothing crosses a
//! syscall boundary per frame beyond QBUF/DQBUF ioctls.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, info};
use v4l::buffer::Type;
use v4l::device::Device;
use v4l::format::FourCC;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::OutputStream;
use v4l::video::Output;
use v4l::Format;

use crate::frame::{Frame, PixelFormat};

/// Kernel buffers requested from the loopback driver.
const NUM_KBUF: u32 = 4;
/// Bound on QBUF/DQBUF waits so the thread stays responsive to shutdown and
/// to a missing consumer (v4l2loopback only drains output buffers while a
/// reader is attached).
const POLL_TIMEOUT_MS: u64 = 500;

struct Active {
    stream: MmapStream<'static>,
    #[allow(dead_code)] // kept alive: owns the device fd backing `stream`
    dev: Device,
    negotiated: (u32, u32, PixelFormat),
}

impl Active {
    fn open(path: &Path, width: u32, height: u32, fmt: PixelFormat) -> io::Result<Self> {
        let dev = Device::with_path(path)?;

        if let Ok(caps) = dev.query_caps() {
            info!(driver = %caps.driver, card = %caps.card, bus = %caps.bus, "output device");
        }

        let want = Format::new(width, height, FourCC::new(&fmt.fourcc()));
        let actual = Output::set_format(&dev, &want)?;
        if actual.width != width || actual.height != height || actual.fourcc != want.fourcc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "loopback rejected format {}x{} {:?}: driver selected {}x{} {}",
                    width, height, fmt, actual.width, actual.height, actual.fourcc
                ),
            ));
        }
        debug!(sizeimage = actual.size, "format negotiated");

        let mut stream = MmapStream::with_buffers(&dev, Type::VideoOutput, NUM_KBUF)?;
        stream.set_timeout(Duration::from_millis(POLL_TIMEOUT_MS));

        Ok(Active {
            stream,
            dev,
            negotiated: (width, height, fmt),
        })
    }

    fn write(&mut self, payload: &[u8]) -> io::Result<()> {
        // next(): queues the previously filled buffer, then dequeues the next
        // free one. First call returns a fresh buffer without touching the
        // queue. Times out with `TimedOut` when no reader drains the device.
        let (buf, meta) = match self.stream.next() {
            Ok(bm) => bm,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "no consumer draining the loopback device",
                ));
            }
            Err(e) => return Err(e),
        };

        if payload.len() > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame ({} B) exceeds driver buffer ({} B)", payload.len(), buf.len()),
            ));
        }
        // Packed formats must exactly fill one video frame; a mismatch would
        // corrupt the loopback stream, so reject instead of writing partials.
        let (w, h, fmt) = self.negotiated;
        if let Some(expected) = fmt.packed_size(w, h) {
            if payload.len() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame ({} B) != negotiated frame size ({} B)", payload.len(), expected),
                ));
            }
        }
        buf[..payload.len()].copy_from_slice(payload);
        meta.bytesused = payload.len() as u32;
        Ok(())
    }
}

pub struct V4l2LoopSink {
    path: PathBuf,
    active: Option<Active>,
}

impl V4l2LoopSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        V4l2LoopSink {
            path: path.into(),
            active: None,
        }
    }
}

impl super::Sink for V4l2LoopSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let want = (frame.width, frame.height, frame.format);

        // (Re)open on first frame or whenever the format changes; a format
        // change requires a fresh negotiation + STREAMOFF/STREAMON cycle.
        let reopen = match &self.active {
            None => true,
            Some(a) => a.negotiated != want,
        };
        if reopen {
            info!(
                dev = %self.path.display(),
                w = want.0, h = want.1, fmt = ?want.2,
                "initializing loopback output"
            );
            self.active = Some(Active::open(&self.path, want.0, want.1, want.2)?);
        }

        let active = self.active.as_mut().expect("active checked above");
        match active.write(frame.payload()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
            Err(e) => {
                // Drop the device so the next frame triggers a clean re-open.
                self.active = None;
                Err(e)
            }
        }
    }

    fn describe(&self) -> String {
        format!("v4l2loopback:{}", self.path.display())
    }
}
