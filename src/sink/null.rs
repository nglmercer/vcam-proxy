//! Sink that discards frames after counting them. Useful for benchmarking
//! capture/conversion in isolation and for CI smoke tests without kernel
//! modules loaded.

use std::io;

use crate::frame::Frame;

#[derive(Default)]
pub struct NullSink {
    bytes: u64,
    frames: u64,
}

impl super::Sink for NullSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        self.frames += 1;
        self.bytes += frame.len as u64;
        Ok(())
    }

    fn describe(&self) -> String {
        "null".into()
    }
}
