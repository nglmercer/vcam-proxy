//! Single-device v4l2loopback sink implementation.

use std::io;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::frame::Frame;

use super::active::Active;
use super::scaling::ScaleContext;

/// Consecutive write failures before we drop the device and trigger a clean
/// reopen. At 30 fps this is ~2 seconds of sustained errors — long enough to
/// ride out transient hiccups (buffer dequeue races, momentary I/O errors)
/// without disrupting attached CAPTURE readers, but short enough to recover
/// promptly when the device is truly gone (module unloaded, node removed).
///
/// **Why this matters for multi-reader support:** OBS opens its v4l2loopback
/// OUTPUT fd once and writes forever — it never re-opens mid-stream. Every
/// re-open here calls `VIDIOC_S_FMT` and toggles `keep_format`, which tears
/// down every CAPTURE client (OBS, Chrome, Zoom) attached to the node. By
/// keeping the fd open across transient errors we match OBS's behaviour and
/// preserve concurrent readers.
const REOPEN_ERROR_THRESHOLD: u32 = 60;

/// Writes frames to a single v4l2loopback device node.
pub struct V4l2LoopSink {
    pub(crate) path: PathBuf,
    /// v4l2loopback timeout control (ms); 0 = keep last frame forever.
    timeout_ms: i64,
    active: Option<Active>,
    /// Pre-allocated scaling context with LUTs (reused across frames).
    scale_ctx: Option<ScaleContext>,
    /// Pre-allocated output buffer for scaled frames (reused across frames).
    scale_buf: Vec<u8>,
    /// Consecutive non-back-pressure write errors. Resets to 0 on success.
    /// Only after [`REOPEN_ERROR_THRESHOLD`] sustained failures do we drop
    /// the device — this keeps the OUTPUT fd open (and attached CAPTURE
    /// readers alive) across transient errors.
    consecutive_errors: u32,
}

impl V4l2LoopSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_timeout(path, 0)
    }

    pub fn with_timeout(path: impl Into<PathBuf>, timeout_ms: u32) -> Self {
        V4l2LoopSink {
            path: path.into(),
            timeout_ms: timeout_ms as i64,
            active: None,
            scale_ctx: None,
            scale_buf: Vec::new(),
            consecutive_errors: 0,
        }
    }

    /// Handle a write result: reset the error counter on success, count and
    /// possibly trigger a reopen on failure.
    ///
    /// `WouldBlock` / `TimedOut` are back-pressure (no CAPTURE reader draining
    /// the buffers) — they are **not** counted and **never** cause a reopen.
    ///
    /// All other errors increment [`consecutive_errors`]; after
    /// [`REOPEN_ERROR_THRESHOLD`] sustained failures the device is dropped so
    /// the next frame triggers a clean reopen. This is the key difference from
    /// the old behaviour which dropped the device on *every* error, disrupting
    /// all attached readers.
    fn handle_write_result(&mut self, result: io::Result<()>) -> io::Result<()> {
        match result {
            Ok(()) => {
                self.consecutive_errors = 0;
                Ok(())
            }
            // Back-pressure: the kernel output queue is full because no reader
            // is draining buffers, or a poll timed out. This is the designed
            // escape hatch — NOT a real error. Don't count, don't reopen.
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Err(e)
            }
            Err(e) => {
                self.consecutive_errors += 1;
                if self.consecutive_errors >= REOPEN_ERROR_THRESHOLD {
                    warn!(
                        path = %self.path.display(),
                        errors = self.consecutive_errors,
                        error = %e,
                        "sustained write errors; reopening loopback device \
                         (attached readers will reconnect)"
                    );
                    self.active = None;
                    self.consecutive_errors = 0;
                }
                Err(e)
            }
        }
    }
}

impl super::super::Sink for V4l2LoopSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        // Open the device on the first frame only. We intentionally do NOT
        // reopen when the pixel format changes mid-stream: calling
        // `VIDIOC_S_FMT` while CAPTURE clients (OBS, Chrome, Zoom) are
        // attached latches a new format and tears down their streams. This
        // matches OBS's "open once, write forever" model that preserves
        // multiple concurrent readers.
        //
        // If a format mismatch occurs later, `handle_write_result` will
        // accumulate errors and reopen only after sustained failures — by
        // which point the old format is clearly stale and readers have had
        // time to drain buffered frames.
        if self.active.is_none() {
            info!(
                dev = %self.path.display(),
                w = frame.width, h = frame.height, fmt = ?frame.format,
                "initializing loopback output"
            );
            self.active = Some(Active::open(
                &self.path,
                frame.width,
                frame.height,
                frame.format,
                self.timeout_ms,
            )?);
            self.consecutive_errors = 0;
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
                // Fall back to direct write for other formats. The size check
                // in Active::write will reject mismatched payloads; the error
                // counter handles it without a disruptive reopen.
                let result = {
                    let active = self.active.as_mut().expect("active checked above");
                    active.write(frame.payload())
                };
                return self.handle_write_result(result);
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
                let result = {
                    let active = self.active.as_mut().expect("active checked above");
                    active.write(&self.scale_buf[..expected_size])
                };
                self.handle_write_result(result)
            } else {
                Err(io::Error::other(
                    "failed to scale frame to driver resolution",
                ))
            }
        } else {
            // Fast path: no scaling needed, write directly
            let result = {
                let active = self.active.as_mut().expect("active checked above");
                active.write(frame.payload())
            };
            self.handle_write_result(result)
        }
    }

    fn describe(&self) -> String {
        format!("v4l2loopback:{}", self.path.display())
    }
}
