//! Tiny helper: grab N YUYV frames from a V4L2 capture device via `read()`.
//!
//! Used by the pixel-integrity integration suite so each reader is a real OS
//! process (not just a thread sharing one address space).
//!
//! ```text
//! vcam-grab <device> <width> <height> <n_frames> <out_dir>
//! ```

use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let mut args = env::args().skip(1);
    let device = args.next().expect("device");
    let width: u32 = args.next().expect("width").parse().expect("width");
    let height: u32 = args.next().expect("height").parse().expect("height");
    let n: usize = args.next().expect("n_frames").parse().expect("n");
    let out_dir = PathBuf::from(args.next().expect("out_dir"));

    let frame_len = (width as usize) * (height as usize) * 2;
    let mut file = File::open(&device).unwrap_or_else(|e| panic!("open {device}: {e}"));

    std::fs::create_dir_all(&out_dir).expect("mkdir");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got = 0usize;
    let mut buf = vec![0u8; frame_len];

    while got < n {
        if Instant::now() > deadline {
            panic!("timeout after {got}/{n} frames from {device}");
        }
        match file.read(&mut buf) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(m) if m == frame_len => {
                // Skip classic green-flash all-zero frames.
                if buf.iter().all(|&b| b == 0) {
                    continue;
                }
                let path = out_dir.join(format!("frame_{got:04}.yuyv"));
                let mut out = File::create(&path).expect("create frame file");
                out.write_all(&buf).expect("write frame");
                got += 1;
            }
            Ok(_) => {
                // Partial / unexpected size — keep trying.
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("read {device}: {e}"),
        }
    }
}
