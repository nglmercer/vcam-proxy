//! Multi-device v4l2loopback sink implementation.

use std::io;
use std::path::PathBuf;

use crate::frame::Frame;

use super::sink::V4l2LoopSink;

/// Writes each frame to *all* provided loopback device paths. Used when
/// `multi_reader=true` and the module was loaded with `devices >= 2`.
pub struct V4l2LoopMultiSink {
    sinks: Vec<V4l2LoopSink>,
}

impl V4l2LoopMultiSink {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            sinks: paths.into_iter().map(V4l2LoopSink::new).collect(),
        }
    }
}

impl super::super::Sink for V4l2LoopMultiSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let mut last_err = None;
        for sink in &mut self.sinks {
            if let Err(e) = sink.write(frame) {
                // WouldBlock on one device (no reader) shouldn't stop us from
                // feeding the others. Surface the first real error at the end.
                if e.kind() != io::ErrorKind::WouldBlock {
                    last_err = Some(e);
                }
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn describe(&self) -> String {
        let paths: Vec<_> = self
            .sinks
            .iter()
            .map(|s| s.path.display().to_string())
            .collect();
        format!("v4l2loopback:multi({})", paths.join(", "))
    }
}
