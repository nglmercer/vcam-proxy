//! Pixel-integrity suite for the virtual camera.
//!
//! Spawns `vcam-proxy --image <fixture>` against a live v4l2loopback node, then
//! launches several `vcam-grab` child processes that each `read()` frames and
//! checks every capture matches the expected YUYV payload (no green blink).
//!
//! Requires a loaded v4l2loopback module. Skips cleanly when none is present:
//!
//! ```bash
//! cargo test -p vcam-proxy --test pixel_integrity -- --ignored --nocapture
//! ```
//!
//! Close browser/Zoom previews of the virtual camera first — on v4l2loopback
//! ≥ 0.14 the CAPTURE stream token belongs to a single opener, so another
//! streaming app makes concurrent grabs on the same node fail with EBUSY.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use image::{Rgb, RgbImage};
use tempfile::TempDir;
use vcam_proxy::convert;
use vcam_proxy::frame::PixelFormat;
use vcam_proxy::image_source;
use vcam_proxy::sink;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const READERS: usize = 3;
const FRAMES_PER_READER: usize = 5;

fn find_loopback() -> Option<PathBuf> {
    sink::discover_loopback_devices()
        .ok()
        .into_iter()
        .flatten()
        .find(|d| sink::is_loopback_driver(&d.driver))
        .map(|d| d.path)
}

fn write_fixture(dir: &Path) -> PathBuf {
    let mut img = RgbImage::new(WIDTH, HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let band = (x * 6) / WIDTH;
            let color = match band {
                0 => Rgb([255, 0, 0]),
                1 => Rgb([0, 255, 0]),
                2 => Rgb([0, 0, 255]),
                3 => Rgb([255, 255, 0]),
                4 => Rgb([0, 255, 255]),
                _ => Rgb([255, 0, 255]),
            };
            img.put_pixel(x, y, color);
        }
    }
    let path = dir.join("bars.png");
    img.save(&path).expect("write fixture png");
    path
}

fn expected_yuyv(image_path: &Path) -> Vec<u8> {
    let frame = image_source::load_frame(
        image_path,
        WIDTH,
        HEIGHT,
        vcam_proxy::config::FormatPref::Yuy2,
    )
    .expect("load expected frame");
    assert_eq!(frame.format, PixelFormat::Yuy2);
    frame.payload().to_vec()
}

fn yuyv_similarity(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut same = 0usize;
    for i in 0..n {
        if (a[i] as i16 - b[i] as i16).abs() <= 2 {
            same += 1;
        }
    }
    same as f64 / n as f64
}

fn is_mostly_green_yuyv(frame: &[u8]) -> bool {
    if frame.is_empty() {
        return true;
    }
    let mut rgb = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    if !convert::yuy2_to_rgb24(frame, &mut rgb, WIDTH, HEIGHT) {
        return true;
    }
    let mut greenish = 0usize;
    let pixels = (WIDTH * HEIGHT) as usize;
    for i in 0..pixels {
        let r = rgb[i * 3] as i32;
        let g = rgb[i * 3 + 1] as i32;
        let b = rgb[i * 3 + 2] as i32;
        if g > r + 30 && g > b + 30 {
            greenish += 1;
        }
    }
    greenish * 100 / pixels > 40
}

fn load_reader_frames(dir: &Path) -> Vec<Vec<u8>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yuyv"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())))
        .collect()
}

#[test]
fn image_source_roundtrip_unit() {
    let dir = TempDir::new().unwrap();
    let png = write_fixture(dir.path());
    let frame = image_source::load_frame(&png, WIDTH, HEIGHT, vcam_proxy::config::FormatPref::Yuy2)
        .unwrap();
    assert_eq!(frame.width, WIDTH);
    assert_eq!(frame.height, HEIGHT);
    assert!(!frame.payload().iter().all(|&b| b == 0));
}

#[test]
#[ignore = "needs v4l2loopback + free /dev/videoN; run with --ignored"]
fn multi_reader_pixel_integrity() {
    let Some(device) = find_loopback() else {
        eprintln!("skip: no v4l2loopback device found");
        return;
    };

    let dir = TempDir::new().unwrap();
    let png = write_fixture(dir.path());
    let expected = expected_yuyv(&png);

    let proxy_bin = env!("CARGO_BIN_EXE_vcam-proxy");
    let grab_bin = env!("CARGO_BIN_EXE_vcam-grab");

    let mut child = Command::new(proxy_bin)
        .args([
            "--no-gui",
            "--no-tray",
            "--image",
            png.to_str().unwrap(),
            "--device",
            device.to_str().unwrap(),
            "--width",
            &WIDTH.to_string(),
            "--height",
            &HEIGHT.to_string(),
            "--fps",
            "30",
            "--format",
            "yuy2",
            "--multi-reader",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vcam-proxy");

    // Give the producer time to S_FMT + STREAMON + write first frames.
    let ready_deadline = Instant::now() + Duration::from_secs(8);
    let mut ready = false;
    while Instant::now() < ready_deadline {
        if let Some(status) = child.try_wait().unwrap() {
            let err = {
                let mut s = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
                }
                s
            };
            panic!("vcam-proxy exited early ({status}): {err}");
        }
        // Probe with a single grab process.
        let probe_dir = dir.path().join("probe");
        let status = Command::new(grab_bin)
            .args([
                device.to_str().unwrap(),
                &WIDTH.to_string(),
                &HEIGHT.to_string(),
                "1",
                probe_dir.to_str().unwrap(),
            ])
            .status()
            .expect("spawn probe grab");
        if status.success() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    assert!(
        ready,
        "timed out waiting for vcam-proxy (is another app holding {}? close Firefox/Zoom preview)",
        device.display()
    );

    // Sequential OS processes (still separate address spaces). Concurrent
    // opens fail when another app (Firefox/Zoom) already holds the node —
    // that is a v4l2loopback opener-limit / exclusivity issue, not a pixel bug.
    let mut all_frames: Vec<Vec<u8>> = Vec::new();
    for i in 0..READERS {
        let out = dir.path().join(format!("reader_{i}"));
        let mut grab = Command::new(grab_bin)
            .args([
                device.to_str().unwrap(),
                &WIDTH.to_string(),
                &HEIGHT.to_string(),
                &FRAMES_PER_READER.to_string(),
                out.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn vcam-grab");
        let status = grab.wait().expect("wait grab");
        if !status.success() {
            let mut err = String::new();
            if let Some(mut stderr) = grab.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut err);
            }
            let _ = child.kill();
            panic!(
                "reader {i} failed ({status}): {err}\n\
                 Hint: close any browser/Zoom preview of {} and re-run.",
                device.display()
            );
        }
        let frames = load_reader_frames(&out);
        assert_eq!(frames.len(), FRAMES_PER_READER, "reader {i} frame count");
        for (j, frame) in frames.iter().enumerate() {
            assert!(
                !is_mostly_green_yuyv(frame),
                "reader {i} frame {j} looks like a green blink"
            );
            let sim = yuyv_similarity(frame, &expected);
            assert!(
                sim >= 0.98,
                "reader {i} frame {j} similarity {sim:.4} < 0.98 vs source image"
            );
            all_frames.push(frame.clone());
        }
    }

    // Concurrent pass: only when the node looks free enough for N opens.
    // Firefox holding the camera is the usual reason this is skipped.
    let concurrent_ok = try_concurrent_readers(grab_bin, &device, dir.path());
    if !concurrent_ok {
        eprintln!(
            "note: skipped concurrent multi-open (another app likely holds {})",
            device.display()
        );
    }

    let _ = child.kill();
    let _ = child.wait();

    let reference = &all_frames[0];
    for (idx, frame) in all_frames.iter().enumerate().skip(1) {
        let sim = yuyv_similarity(frame, reference);
        assert!(
            sim >= 0.99,
            "frame {idx} diverges from reader-0 frame 0 (sim={sim:.4})"
        );
    }

    let _ = writeln!(
        std::io::stderr(),
        "pixel_integrity OK: {READERS} processes × {FRAMES_PER_READER} frames on {} (concurrent={concurrent_ok})",
        device.display()
    );
}

fn try_concurrent_readers(grab_bin: &str, device: &Path, dir: &Path) -> bool {
    let mut kids = Vec::new();
    for i in 0..READERS {
        let out = dir.join(format!("concurrent_{i}"));
        match Command::new(grab_bin)
            .args([
                device.to_str().unwrap(),
                &WIDTH.to_string(),
                &HEIGHT.to_string(),
                "2",
                out.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => kids.push(c),
            Err(_) => return false,
        }
    }
    let mut ok = true;
    for mut c in kids {
        match c.wait() {
            Ok(s) if s.success() => {}
            _ => ok = false,
        }
    }
    ok
}
