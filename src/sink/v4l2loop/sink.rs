//! Single-device v4l2loopback sink implementation.

use std::io;
use std::path::PathBuf;

use tracing::info;

use crate::frame::Frame;

use super::active::Active;
use super::scaling::ScaleContext;

/// Writes frames to a single v4l2loopback device node.
pub struct V4l2LoopSink {
    pub(crate) path: PathBuf,
    active: Option<Active>,
    /// Pre-allocated scaling context with LUTs (reused across frames).
    scale_ctx: Option<ScaleContext>,
    /// Pre-allocated output buffer for scaled frames (reused across frames).
    scale_buf: Vec<u8>,
}

impl V4l2LoopSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        V4l2LoopSink {
            path: path.into(),
            active: None,
            scale_ctx: None,
            scale_buf: Vec::new(),
        }
    }
}

impl super::super::Sink for V4l2LoopSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let want = (frame.width, frame.height, frame.format);

        // (Re)open on first frame or whenever the pixel format changes.
        // Resolution differences are handled by scaling, not reopening.
        let reopen = match &self.active {
            None => true,
            Some(a) => a.negotiated.2 != want.2,
        };
        if reopen {
            info!(
                dev = %self.path.display(),
                w = want.0, h = want.1, fmt = ?want.2,
                "initializing loopback output"
            );
            self.active = Some(Active::open(&self.path, want.0, want.1, want.2)?);
        }

        // Check if we need to scale (driver selected different resolution than input)
        let (needs_scaling, neg_w, neg_h, neg_fmt) = {
            let active = self.active.as_ref().expect("active checked above");
            let (neg_w, neg_h, neg_fmt) = active.negotiated;
            (
                neg_fmt != frame.format || neg_w != frame.width || neg_h != frame.height,
                neg_w,
                neg_h,
                neg_fmt,
            )
        };

        if needs_scaling {
            // Only NV12 scaling is supported for now
            if neg_fmt != crate::frame::PixelFormat::Nv12 {
                // Fall back to direct write for other formats
                let active = self.active.as_mut().expect("active checked above");
                return active.write(frame.payload());
            }

            // Initialize scale context if needed (LUTs are pre-computed once)
            if self.scale_ctx.is_none() {
                self.scale_ctx = Some(ScaleContext::new(frame.width, frame.height, neg_w, neg_h));
            }

            // Grow output buffer if needed (usually only on first frame)
            let expected_size = (neg_w * neg_h * 3 / 2) as usize;
            if self.scale_buf.len() < expected_size {
                self.scale_buf.resize(expected_size, 0);
            }

            // Scale using pre-computed LUTs (no allocations in hot path)
            let ctx = self.scale_ctx.as_ref().expect("scale_ctx checked above");
            if ctx.scale_nv12(frame.payload(), &mut self.scale_buf) {
                let active = self.active.as_mut().expect("active checked above");
                match active.write(&self.scale_buf[..expected_size]) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
                    Err(e) => {
                        self.active = None;
                        Err(e)
                    }
                }
            } else {
                Err(io::Error::other(
                    "failed to scale frame to driver resolution",
                ))
            }
        } else {
            // Fast path: no scaling needed, write directly
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
    }

    fn describe(&self) -> String {
        format!("v4l2loopback:{}", self.path.display())
    }
}
