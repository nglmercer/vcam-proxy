//! Frame representation and the pre-allocated buffer pool.
//!
//! Steady-state operation performs **zero heap allocations per frame**:
//! a fixed set of `Frame` slots circulates between the capture thread and the
//! sink thread through two bounded lock-free channels:
//!
//! ```text
//! capture thread                      sink thread
//!   pool.acquire()  <-- free slot --+   pool.release(frame) --+
//!   fill payload                    |                         |
//!   tx.try_send(frame) -------------+--> rx.recv_timeout() ---+
//! ```

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TrySendError};

/// Pixel formats the pipeline can carry on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Packed YUV 4:2:2 `Y0 U0 Y1 V0` (V4L2 fourcc `YUYV`).
    Yuy2,
    /// Packed 24-bit RGB (V4L2 fourcc `RGB3`).
    Rgb24,
    /// Semi-planar YUV 4:2:0 (V4L2 fourcc `NV12`).
    Nv12,
    /// Baseline JPEG bitstream, one frame per buffer (`MJPG`).
    Mjpeg,
}

impl PixelFormat {
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            PixelFormat::Yuy2 => *b"YUYV",
            PixelFormat::Rgb24 => *b"RGB3",
            PixelFormat::Nv12 => *b"NV12",
            PixelFormat::Mjpeg => *b"MJPG",
        }
    }

    /// Exact buffer size for packed formats, `None` for variable-size (MJPEG).
    pub fn packed_size(self, width: u32, height: u32) -> Option<usize> {
        let px = width as usize * height as usize;
        match self {
            PixelFormat::Yuy2 => Some(px * 2),
            PixelFormat::Rgb24 => Some(px * 3),
            PixelFormat::Nv12 => Some(px * 3 / 2),
            PixelFormat::Mjpeg => None,
        }
    }
}

/// One video frame plus its reusable backing store.
pub struct Frame {
    /// Pre-allocated storage; reused across frames (never reallocated while
    /// the negotiated resolution is unchanged).
    pub buf: Vec<u8>,
    /// Valid bytes in `buf`.
    pub len: usize,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub seq: u64,
    pub ts: Instant,
}

impl Frame {
    fn with_capacity(cap: usize) -> Self {
        Frame {
            buf: vec![0; cap],
            len: 0,
            width: 0,
            height: 0,
            format: PixelFormat::Yuy2,
            seq: 0,
            ts: Instant::now(),
        }
    }

    pub fn payload(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Resize the slot if a newly negotiated format needs more room
    /// (allocation happens at most once per format change).
    pub fn reserve(&mut self, n: usize) {
        if self.buf.len() < n {
            self.buf.resize(n, 0);
        }
    }

    /// Grow if needed and set the valid length. Returns the writable payload.
    pub fn payload_mut(&mut self, n: usize) -> &mut [u8] {
        self.reserve(n);
        self.len = n;
        &mut self.buf[..n]
    }
}

/// Fixed-size pool of reusable `Frame` slots.
///
/// `release` never blocks and never fails: the channel capacity equals the
/// number of slots ever created, so there is always room to return a slot.
#[derive(Clone)]
pub struct BufferPool {
    free_tx: Sender<Frame>,
    free_rx: Receiver<Frame>,
}

impl BufferPool {
    pub fn new(slots: usize, slot_bytes: usize) -> Self {
        let (free_tx, free_rx) = crossbeam_channel::bounded(slots);
        for _ in 0..slots {
            let _ = free_tx.send(Frame::with_capacity(slot_bytes));
        }
        Self { free_tx, free_rx }
    }

    /// Non-blocking acquisition; `None` means every slot is in flight
    /// (the sink is behind) and the caller should drop the frame.
    pub fn try_acquire(&self) -> Option<Frame> {
        self.free_rx.try_recv().ok()
    }

    pub fn release(&self, mut frame: Frame) {
        frame.len = 0;
        match self.free_tx.try_send(frame) {
            Ok(()) => {}
            // Pool channels are sized to hold every slot; unreachable unless
            // logic elsewhere duplicated a frame — dropping is still safe.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}
