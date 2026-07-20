//! Windows sink: streams frames over a Win32 named pipe to a virtual-camera
//! driver component (DirectShow source filter / MF virtual camera), in the
//! spirit of the OBS Virtual Camera IPC design.
//!
//! # Wire protocol
//!
//! Every frame is sent as a fixed 40-byte little-endian header followed by
//! the raw pixel payload:
//!
//! ```text
//! offset  size  field
//!      0     4  magic        = "VCAM" (0x5643_414D)
//!      4     4  version      = 1
//!      8     4  width
//!     12     4  height
//!     16     4  fourcc       = "YUYV" | "RGB3" | "NV12" | "MJPG"
//!     20     4  payload_len  (bytes following this header)
//!     24     8  sequence number
//!     32     8  timestamp    (µs since sink start)
//!     40     *  payload      (payload_len bytes)
//! ```
//!
//! The pipe is created with `FILE_FLAG_OVERLAPPED`; connect and write waits
//! are bounded (`WAIT_MS`) so the sink thread remains responsive to shutdown
//! and a slow/absent consumer degrades to graceful frame drops.

#![cfg(target_os = "windows")]

use std::io;
use std::time::Instant;

use windows::core::{Error, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    WriteFile, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

use crate::frame::Frame;

const MAGIC: u32 = 0x5643_414D; // "VCAM"
const VERSION: u32 = 1;
const HEADER_LEN: usize = 40;
/// Bounded wait for client connect / write completion.
const WAIT_MS: u32 = 200;
/// Kernel-side output buffering for the pipe.
const OUT_BUF: u32 = 1 << 22;

struct Conn {
    pipe: HANDLE,
    event: HANDLE,
    ov: OVERLAPPED,
    connected: bool,
    epoch: Instant,
}

// Win32 kernel handles are process-wide resources usable from any thread;
// `Conn` is exclusively owned by the sink thread, so moving it across threads
// once (inside `Box<dyn Sink>`) is sound. All pipe operations are serialized
// by the owning thread.
unsafe impl Send for Conn {}

impl Conn {
    fn create(name: &str) -> io::Result<Self> {
        let wide: Vec<u16> = format!(r"\\.\pipe\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let pipe = CreateNamedPipeW(
                PCWSTR::from_raw(wide.as_ptr()),
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                OUT_BUF,
                0,
                0,
                None,
            );
            if pipe.is_invalid() {
                return Err(to_io(Error::from_win32()));
            }
            let event = CreateEventW(None, true, false, PCWSTR::null()).map_err(to_io)?;
            Ok(Conn {
                pipe,
                event,
                ov: OVERLAPPED::default(),
                connected: false,
                epoch: Instant::now(),
            })
        }
    }

    /// Rearm the one-shot overlapped state before each operation.
    unsafe fn rearm(&mut self) {
        let _ = ResetEvent(self.event);
        self.ov = OVERLAPPED::default();
        self.ov.hEvent = self.event;
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.connected {
            return Ok(());
        }
        unsafe {
            self.rearm();
            match ConnectNamedPipe(self.pipe, Some(&mut self.ov)) {
                Ok(()) => self.connected = true,
                Err(e) if is_err(&e, ERROR_PIPE_CONNECTED) => self.connected = true,
                Err(e) if is_err(&e, ERROR_IO_PENDING) => {
                    if WaitForSingleObject(self.event, WAIT_MS) != WAIT_OBJECT_0 {
                        // Consumer didn't attach in time; cancel the pending
                        // connect so the pipe stays in a clean state.
                        let _ = CancelIo(self.pipe);
                        let _ = WaitForSingleObject(self.event, 50);
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "waiting for a pipe consumer",
                        ));
                    }
                    self.connected = true;
                }
                Err(e) => return Err(to_io(e)),
            }
        }
        Ok(())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        unsafe {
            self.rearm();
            match WriteFile(self.pipe, Some(bytes), None, Some(&mut self.ov)) {
                Ok(()) => Ok(()),
                Err(e) if is_err(&e, ERROR_IO_PENDING) => {
                    if WaitForSingleObject(self.event, WAIT_MS) != WAIT_OBJECT_0 {
                        let _ = CancelIo(self.pipe);
                        let _ = WaitForSingleObject(self.event, 50);
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "pipe consumer is not reading",
                        ));
                    }
                    let mut written = 0u32;
                    GetOverlappedResult(self.pipe, &self.ov, &mut written, false)
                        .map_err(to_io)?;
                    Ok(())
                }
                Err(e) => Err(to_io(e)),
            }
        }
    }

    fn disconnect(&mut self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.pipe);
        }
        self.connected = false;
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe {
            if self.connected {
                let _ = DisconnectNamedPipe(self.pipe);
            }
            let _ = CloseHandle(self.pipe);
            let _ = CloseHandle(self.event);
        }
    }
}

pub struct PipeSink {
    name: String,
    conn: Option<Conn>,
}

impl PipeSink {
    pub fn new(name: impl Into<String>) -> Self {
        PipeSink {
            name: name.into(),
            conn: None,
        }
    }
}

impl super::Sink for PipeSink {
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        if self.conn.is_none() {
            self.conn = Some(Conn::create(&self.name)?);
        }
        let conn = self.conn.as_mut().expect("conn checked above");

        let r = (|conn: &mut Conn| {
            conn.ensure_connected()?;

            let payload = frame.payload();
            let mut header = [0u8; HEADER_LEN];
            header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
            header[4..8].copy_from_slice(&VERSION.to_le_bytes());
            header[8..12].copy_from_slice(&frame.width.to_le_bytes());
            header[12..16].copy_from_slice(&frame.height.to_le_bytes());
            header[16..20].copy_from_slice(&frame.format.fourcc());
            header[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            header[24..32].copy_from_slice(&frame.seq.to_le_bytes());
            header[32..40]
                .copy_from_slice(&(conn.epoch.elapsed().as_micros() as u64).to_le_bytes());

            conn.write_all(&header)?;
            conn.write_all(payload)
        })(conn);

        match r {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
            Err(e) => {
                // Client vanished (or the pipe broke): reset so the next
                // frame triggers a fresh connect.
                conn.disconnect();
                Err(e)
            }
        }
    }

    fn describe(&self) -> String {
        format!(r"named-pipe:\\.\pipe\{}", self.name)
    }
}

fn is_err(e: &Error, code: WIN32_ERROR) -> bool {
    e.code() == HRESULT::from_win32(code.0)
}

fn to_io(e: Error) -> io::Error {
    let kind = if is_err(&e, ERROR_BROKEN_PIPE)
        || is_err(&e, ERROR_NO_DATA)
        || is_err(&e, ERROR_PIPE_NOT_CONNECTED)
    {
        io::ErrorKind::BrokenPipe
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, e)
}

// Silence unused warnings for constants only referenced in docs on some
// build configurations.
#[allow(unused_imports)]
use windows::Win32::Foundation::WAIT_TIMEOUT as _;
