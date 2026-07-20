//! Linux sink: streams frames into a v4l2loopback device node.
//!
//! Kernel interface used (via the `v4l` crate):
//! - `VIDIOC_QUERYCAP` to detect loopback output devices and validate capabilities
//! - `VIDIOC_S_FMT` to negotiate width/height/pixelformat
//! - `VIDIOC_REQBUFS` + `mmap` for kernel-allocated, userspace-mapped buffers
//! - `VIDIOC_QBUF`/`VIDIOC_DQBUF` to cycle frames (`V4L2_BUF_TYPE_VIDEO_OUTPUT`)
//! - `VIDIOC_STREAMON`/`STREAMOFF` on start/stop (handled by the stream impl)
//!
//! Frame data lands directly in the kernel-mapped buffer -- the only copy is
//! `memcpy` from our pooled slot into the mmap region; nothing crosses a
//! syscall boundary per frame beyond QBUF/DQBUF ioctls.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tracing::{debug, info, warn};
use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::control::{Control, Value as CtrlValue};
use v4l::device::Device;
use v4l::format::FourCC;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::OutputStream;
use v4l::video::Output;
use v4l::Format;

use crate::frame::{Frame, PixelFormat};

// ---------------------------------------------------------------------------
// Distribution detection & auto-install
// ---------------------------------------------------------------------------

/// Known Linux package-manager families we can auto-install for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Ubuntu,
    Fedora,
    Arch,
    OpenSuse,
    Unknown,
}

/// Read `/etc/os-release` and map the `ID` (and fallback `ID_LIKE`) to a
/// [`Distro`] variant. Returns [`Distro::Unknown`] if the file is missing or
/// contains an unrecognised ID.
fn detect_distro() -> Distro {
    let Ok(contents) = fs::read_to_string("/etc/os-release") else {
        return Distro::Unknown;
    };

    let mut id = "";
    let mut id_like = "";
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"');
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = v.trim_matches('"');
        }
    }

    match id {
        "debian" => Distro::Debian,
        "ubuntu" => Distro::Ubuntu,
        "fedora" => Distro::Fedora,
        "arch" => Distro::Arch,
        "opensuse" | "opensuse-tumbleweed" | "opensuse-leap" => Distro::OpenSuse,
        _ => {
            // Fall back to ID_LIKE (e.g. "ubuntu" for Pop!_OS, "debian" for Mint).
            if id_like.contains("debian") || id_like.contains("ubuntu") {
                Distro::Debian
            } else if id_like.contains("fedora") || id_like.contains("rhel") {
                Distro::Fedora
            } else if id_like.contains("arch") {
                Distro::Arch
            } else {
                Distro::Unknown
            }
        }
    }
}

/// Build the `pkexec <pm> install …` command for the detected distro.
/// Returns `None` when we don't recognise the distro.
fn install_command(distro: Distro) -> Option<(String, Vec<String>)> {
    match distro {
        Distro::Debian | Distro::Ubuntu => Some((
            "apt-get".to_string(),
            vec![
                "install".into(),
                "-y".into(),
                "v4l2loopback-dkms".into(),
                "v4l-utils".into(),
            ],
        )),
        Distro::Fedora => Some((
            "dnf".to_string(),
            vec!["install".into(), "-y".into(), "v4l2loopback".into()],
        )),
        Distro::Arch => Some((
            "pacman".into(),
            vec![
                "-S".into(),
                "--noconfirm".into(),
                "v4l2loopback-dkms".into(),
                "v4l-utils".into(),
            ],
        )),
        Distro::OpenSuse => Some((
            "zypper".into(),
            vec!["install".into(), "-y".into(), "v4l2loopback".into()],
        )),
        Distro::Unknown => None,
    }
}

/// Attempt to install the v4l2loopback DKMS package through the native package
/// manager, escalating via `pkexec`. This covers the case where `modprobe`
/// fails because the module was never built/installed, not just unloaded.
///
/// Returns:
/// - `Ok(())` if the package manager reported success.
/// - `Err(ModuleError::InstallFailed)` on a non-zero exit.
/// - `Err(ModuleError::PkexecNotAvailable)` if `pkexec` is missing.
/// - `Err(ModuleError::DistroNotSupported)` when we cannot map the distro.
pub fn install_v4l2loopback() -> Result<(), ModuleError> {
    let distro = detect_distro();
    let (pm, args) = install_command(distro).ok_or(ModuleError::DistroNotSupported)?;

    info!(?distro, package_manager = %pm, "attempting v4l2loopback package install");

    // pkexec gives us a GUI polkit prompt; fall back to sudo if pkexec is
    // unavailable (headless server, minimal desktop).
    let escalator = if Path::new("/usr/bin/pkexec").exists() {
        "pkexec"
    } else if Path::new("/usr/bin/sudo").exists() {
        "sudo"
    } else {
        return Err(ModuleError::PkexecNotAvailable);
    };

    let status = Command::new(escalator)
        .arg(&pm)
        .args(&args)
        .status()
        .map_err(|e| ModuleError::LoadFailed {
            reason: format!("failed to invoke {escalator} {pm}: {e}"),
        })?;

    if status.success() {
        info!("v4l2loopback package installed successfully");
        Ok(())
    } else {
        Err(ModuleError::InstallFailed(status.code().unwrap_or(-1)))
    }
}

/// Ensure the v4l2loopback module is loaded, installing the package first if
/// it cannot be found by `modprobe`. Flow:
/// 1. Check `/proc/modules` — if already loaded, return immediately.
/// 2. Try `modprobe v4l2loopback …` via pkexec.
/// 3. If modprobe failed because the module isn't installed, attempt
///    [`install_v4l2loopback`] and retry modprobe once.
pub fn ensure_module_loaded_with_install(params: &str) -> Result<(), ModuleError> {
    if is_module_loaded() {
        return Ok(());
    }

    // First attempt: modprobe (module may already be built but unloaded).
    match load_module_with_params(params) {
        Ok(()) => return Ok(()),
        Err(e) if matches!(e, ModuleError::NotLoaded) => {
            // Module likely not installed at all → try to install it.
            warn!("modprobe failed; v4l2loopback may not be installed");
        }
        Err(e) => return Err(e),
    }

    // Auto-install and retry.
    install_v4l2loopback()?;
    load_module_with_params(params)
}

/// Kernel buffers requested from the loopback driver. More buffers help when
/// multiple readers (OBS + browser) drain at slightly different rates.
const NUM_KBUF: u32 = 4;
/// Bound on QBUF/DQBUF waits so the thread stays responsive to shutdown and
/// to a missing consumer (v4l2loopback only drains output buffers while a
/// reader is attached).
const POLL_TIMEOUT_MS: u64 = 500;

/// v4l2loopback private control IDs (V4L2_CID_USER_BASE + offsets used by the
/// driver). Setting these makes the virtual camera look like a real UVC device
/// to Chrome/Firefox even between consumer attach/detach cycles.
const CID_KEEP_FORMAT: u32 = 0x0098_f900;
const CID_SUSTAIN_FRAMERATE: u32 = 0x0098_f901;
const CID_TIMEOUT: u32 = 0x0098_f902;

/// True when the driver string is v4l2loopback (kernel reports either spelling).
#[inline]
fn is_loopback_driver(driver: &str) -> bool {
    driver == "v4l2 loopback" || driver == "v4l2loopback"
}

// ---------------------------------------------------------------------------
// Device discovery & validation
// ---------------------------------------------------------------------------

/// Information about a discovered loopback-capable output device.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields used for Display/debugging; not all read in main
pub struct LoopbackDevice {
    pub path: PathBuf,
    pub driver: String,
    pub card: String,
    pub bus: String,
    pub version: String,
    pub capabilities: u32,
}

impl std::fmt::Display for LoopbackDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{}] (driver: {}, caps: 0x{:08x})",
            self.path.display(),
            self.card,
            self.driver,
            self.capabilities,
        )
    }
}

/// Scan /dev/video* for loopback devices and other VIDEO_OUTPUT nodes.
///
/// With `exclusive_caps=1` an idle v4l2loopback node only advertises
/// `VIDEO_CAPTURE` (so browsers see a camera). We still list it by driver
/// name so discovery works before the producer opens the device.
pub fn discover_loopback_devices() -> io::Result<Vec<LoopbackDevice>> {
    let mut devices = Vec::new();

    for entry in fs::read_dir("/dev")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) if n.starts_with("video") => n,
            _ => continue,
        };
        let path = PathBuf::from(format!("/dev/{}", name));

        if let Some(dev) = probe_loopback_candidate(&path) {
            devices.push(dev);
        }
    }

    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

/// Probe a single device path. Accepts:
/// - any v4l2loopback node (by driver name), regardless of current caps mask
/// - any other device that currently advertises `VIDEO_OUTPUT`
fn probe_loopback_candidate(path: &Path) -> Option<LoopbackDevice> {
    let dev = Device::with_path(path).ok()?;
    let caps = dev.query_caps().ok()?;

    let loopback = is_loopback_driver(&caps.driver);
    let has_output = caps.capabilities.contains(Flags::VIDEO_OUTPUT);
    if !loopback && !has_output {
        return None;
    }

    let (major, minor, patch) = caps.version;
    Some(LoopbackDevice {
        path: path.to_path_buf(),
        driver: caps.driver.clone(),
        card: caps.card.clone(),
        bus: caps.bus.clone(),
        version: format!("{}.{}.{}", major, minor, patch),
        capabilities: caps.capabilities.bits(),
    })
}

/// Discover the best loopback device for output.
///
/// Priority:
/// 1. `preferred` path if it exists and is a usable loopback/output node
/// 2. First v4l2loopback device found by scanning
/// 3. Any VIDEO_OUTPUT device if no v4l2loopback found
pub fn find_loopback_device(preferred: &Path) -> Result<PathBuf, LoopbackError> {
    // 1. Try preferred device first
    if preferred.exists() {
        if let Some(dev) = probe_loopback_candidate(preferred) {
            info!(device = %dev.path.display(), card = %dev.card, "using preferred loopback device");
            return Ok(dev.path);
        }
        warn!(
            path = %preferred.display(),
            "preferred device is not a loopback/output node; scanning for alternatives"
        );
    }

    // 2. Scan all /dev/video* for loopback devices
    let all_devices =
        discover_loopback_devices().map_err(|e| LoopbackError::ScanFailed { source: e })?;

    // 3. Prefer v4l2loopback devices (driver name may include a space).
    let loopback_devices: Vec<_> = all_devices
        .iter()
        .filter(|d| is_loopback_driver(&d.driver))
        .cloned()
        .collect();

    if let Some(dev) = loopback_devices.first() {
        info!(device = %dev.path.display(), card = %dev.card, "auto-detected v4l2loopback device");
        return Ok(dev.path.clone());
    }

    // 4. Fall back to any output-capable device
    if let Some(dev) = all_devices.first() {
        warn!(
            device = %dev.path.display(), driver = %dev.driver,
            "no v4l2loopback device found; using alternative (may not work with all apps)"
        );
        return Ok(dev.path.clone());
    }

    // 5. Nothing available
    Err(LoopbackError::NoDeviceFound)
}

/// Read `/sys/module/v4l2loopback/parameters/exclusive_caps` and return whether
/// the first device has exclusive_caps enabled. `None` if the module is not
/// loaded or the sysfs node is unreadable.
///
/// Browsers refuse devices that advertise both CAPTURE and OUTPUT; exclusive
/// caps is what makes the virtual node look like a real webcam.
pub fn exclusive_caps_active() -> Option<bool> {
    let raw = fs::read_to_string("/sys/module/v4l2loopback/parameters/exclusive_caps").ok()?;
    // Format is "Y,N,N,..." or "1,0,0,..." depending on kernel/module version.
    let first = raw.split(',').next()?.trim();
    Some(matches!(first, "Y" | "y" | "1"))
}

// ---------------------------------------------------------------------------
// Permissions check
// ---------------------------------------------------------------------------

/// Check if we can read/write the given video device.
/// Returns Ok(()) or an Err with an actionable suggestion.
pub fn check_device_access(path: &Path) -> Result<(), AccessError> {
    if !path.exists() {
        return Err(AccessError::NotFound {
            path: path.to_path_buf(),
        });
    }

    // Try opening for read+write (loopback output needs write access)
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // Check if user is in 'video' group
            let in_video_group = std::process::Command::new("groups")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("video"))
                .unwrap_or(false);

            if in_video_group {
                Err(AccessError::PermissionDenied {
                    path: path.to_path_buf(),
                    suggestion: "User is in 'video' group but device is not writable. \
                                 Try: sudo chmod 0660 {} OR relogin"
                        .into(),
                })
            } else {
                Err(AccessError::PermissionDenied {
                    path: path.to_path_buf(),
                    suggestion: format!(
                        "Run: sudo usermod -aG video $USER\n\
                         Then LOG OUT and log back in for the group change to take effect.\n\
                         (Temporary: sudo chmod 0660 {})",
                        path.display()
                    ),
                })
            }
        }
        Err(e) => Err(AccessError::Other {
            source: e,
            path: path.to_path_buf(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Kernel module management
// ---------------------------------------------------------------------------

/// Check if the v4l2loopback kernel module is currently loaded.
pub fn is_module_loaded() -> bool {
    fs::read_to_string("/proc/modules")
        .map(|c| c.lines().any(|line| line.starts_with("v4l2loopback ")))
        .unwrap_or(false)
}

/// Load the v4l2loopback kernel module with custom parameters via pkexec.
/// The `params` string is split on whitespace and passed as arguments to modprobe.
pub fn load_module_with_params(params: &str) -> Result<(), ModuleError> {
    if is_module_loaded() {
        return Ok(());
    }

    info!("v4l2loopback module not loaded; attempting auto-load via pkexec with params: {params}");

    let args: Vec<&str> = params.split_whitespace().collect();
    let result = std::process::Command::new("pkexec")
        .arg("modprobe")
        .arg("v4l2loopback")
        .args(&args)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            std::thread::sleep(Duration::from_millis(200));
            if is_module_loaded() {
                info!("v4l2loopback module loaded successfully");
                Ok(())
            } else {
                Err(ModuleError::LoadFailed {
                    reason: "modprobe reported success but module not visible in /proc/modules"
                        .into(),
                })
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ModuleError::LoadFailed {
                reason: format!(
                    "pkexec modprobe failed (exit {:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ModuleError::PkexecNotAvailable),
        Err(e) => Err(ModuleError::LoadFailed {
            reason: e.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("no video output device found on this system")]
    NoDeviceFound,

    #[error("error scanning /dev/video*: {source}")]
    ScanFailed { source: io::Error },
}

#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error("device {} does not exist", path.display())]
    NotFound { path: PathBuf },

    #[error("permission denied on {}. {}", path.display(), suggestion)]
    PermissionDenied { path: PathBuf, suggestion: String },

    #[error("cannot access device {path}: {source}")]
    Other { source: io::Error, path: PathBuf },
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // NotLoaded is documentation-only; used in error suggestions
pub enum ModuleError {
    #[error("v4l2loopback module is not loaded; run: sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1")]
    NotLoaded,

    #[error("module load failed: {reason}")]
    LoadFailed { reason: String },

    #[error("pkexec not available; run manually: sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1")]
    PkexecNotAvailable,

    #[error(
        "cannot auto-install: unsupported Linux distribution. Install v4l2loopback-dkms manually"
    )]
    DistroNotSupported,

    #[error("package install failed (exit code {0}); check network and try installing v4l2loopback-dkms manually")]
    InstallFailed(i32),
}

// ---------------------------------------------------------------------------
// Active sink (internal streaming state)
// ---------------------------------------------------------------------------

/// Convert a V4L2 FourCC byte array to our PixelFormat.
/// Returns None for formats we don't support.
fn fourcc_to_pixel_format(fourcc: &[u8; 4]) -> Option<PixelFormat> {
    match fourcc {
        b"YUYV" => Some(PixelFormat::Yuy2),
        b"RGB3" => Some(PixelFormat::Rgb24),
        b"NV12" => Some(PixelFormat::Nv12),
        b"MJPG" => Some(PixelFormat::Mjpeg),
        _ => None,
    }
}

/// Pre-computed scaling lookup tables for fast resize.
/// Avoids per-pixel division in the hot path.
struct ScaleLUT {
    /// Maps dst x -> src x for Y plane
    x_lut: Vec<usize>,
    /// Maps dst y -> src y for Y plane
    y_lut: Vec<usize>,
    /// Maps dst uv x -> src uv x for UV plane
    uv_x_lut: Vec<usize>,
    /// Maps dst uv y -> src uv y for UV plane
    uv_y_lut: Vec<usize>,
}

impl ScaleLUT {
    fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        let mut x_lut = Vec::with_capacity(dst_w as usize);
        let mut y_lut = Vec::with_capacity(dst_h as usize);
        let mut uv_x_lut = Vec::with_capacity(dst_w as usize);
        let mut uv_y_lut = Vec::with_capacity((dst_h / 2) as usize);

        // Luma (Y) plane LUTs
        for x in 0..dst_w as usize {
            x_lut.push((x * src_w as usize) / dst_w as usize);
        }
        for y in 0..dst_h as usize {
            y_lut.push((y * src_h as usize) / dst_h as usize);
        }

        // Chroma (UV) plane LUTs - NV12 UV is at half vertical resolution
        for x in 0..dst_w as usize {
            uv_x_lut.push((x * src_w as usize) / dst_w as usize);
        }
        for y in 0..(dst_h / 2) as usize {
            uv_y_lut.push((y * (src_h / 2) as usize) / (dst_h / 2) as usize);
        }

        ScaleLUT {
            x_lut,
            y_lut,
            uv_x_lut,
            uv_y_lut,
        }
    }
}

/// Scaling context with pre-computed LUTs and reusable buffers.
/// Eliminates per-frame allocations and division operations.
struct ScaleContext {
    lut: ScaleLUT,
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
}

impl ScaleContext {
    fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        ScaleContext {
            lut: ScaleLUT::new(src_w, src_h, dst_w, dst_h),
            src_w: src_w as usize,
            src_h: src_h as usize,
            dst_w: dst_w as usize,
            dst_h: dst_h as usize,
        }
    }

    /// Scale NV12 frame using pre-computed LUTs.
    /// Returns false if buffer sizes are insufficient.
    #[inline]
    fn scale_nv12(&self, src: &[u8], dst: &mut [u8]) -> bool {
        let src_y_size = self.src_w * self.src_h;
        let dst_y_size = self.dst_w * self.dst_h;
        if src.len() < src_y_size * 3 / 2 || dst.len() < dst_y_size * 3 / 2 {
            return false;
        }

        let (src_y, src_uv) = src.split_at(src_y_size);
        let (dst_y, dst_uv) = dst.split_at_mut(dst_y_size);

        // Scale Y plane (luma) - uses LUT for O(1) coordinate lookup
        for dy in 0..self.dst_h {
            let sy = self.lut.y_lut[dy];
            let src_row_start = sy * self.src_w;
            let dst_row_start = dy * self.dst_w;

            for dx in 0..self.dst_w {
                let sx = self.lut.x_lut[dx];
                dst_y[dst_row_start + dx] = src_y[src_row_start + sx];
            }
        }

        // Scale UV plane (chroma) - NV12 has interleaved UV at half resolution
        let dst_uv_h = self.dst_h / 2;
        for dy in 0..dst_uv_h {
            let sy = self.lut.uv_y_lut[dy];
            let src_row_start = sy * self.src_w;
            let dst_row_start = dy * self.dst_w;

            for dx in 0..self.dst_w {
                let sx = self.lut.uv_x_lut[dx];
                dst_uv[dst_row_start + dx] = src_uv[src_row_start + sx];
            }
        }
        true
    }
}

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
            if !is_loopback_driver(&caps.driver) {
                warn!(
                    driver = %caps.driver,
                    "device is not v4l2loopback; browsers may not list it as a camera"
                );
            }
        }

        // Pin the negotiated format and sustain a steady frame clock so
        // consumers (especially browsers via PipeWire) always see a stable
        // UVC-like capture device even when no reader is currently attached.
        apply_loopback_controls(&dev);

        let want = Format::new(width, height, FourCC::new(&fmt.fourcc()));
        let actual = Output::set_format(&dev, &want)?;
        // Accept whatever the driver selects - v4l2loopback may adjust resolution
        // to match its internal state. We'll scale frames if needed.
        if actual.width != width || actual.height != height || actual.fourcc != want.fourcc {
            warn!(
                requested_w = width,
                requested_h = height,
                requested_fmt = ?fmt,
                actual_w = actual.width,
                actual_h = actual.height,
                actual_fmt = %actual.fourcc,
                "loopback adjusted format; frames will be scaled to match"
            );
        }
        info!(
            width = actual.width,
            height = actual.height,
            fourcc = %actual.fourcc,
            sizeimage = actual.size,
            "loopback format negotiated (visible to browsers as capture)"
        );

        let mut stream = MmapStream::with_buffers(&dev, Type::VideoOutput, NUM_KBUF)?;
        stream.set_timeout(Duration::from_millis(POLL_TIMEOUT_MS));

        // Store the actual format the driver selected (may differ from requested)
        let actual_fmt = fourcc_to_pixel_format(&actual.fourcc.repr).unwrap_or(fmt);
        let negotiated = (actual.width, actual.height, actual_fmt);

        Ok(Active {
            stream,
            dev,
            negotiated,
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
                format!(
                    "frame ({} B) exceeds driver buffer ({} B)",
                    payload.len(),
                    buf.len()
                ),
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
        buf[..payload.len()].copy_from_slice(payload);
        meta.bytesused = payload.len() as u32;
        Ok(())
    }
}

/// Enable keep_format + sustain_framerate so the virtual camera keeps advertising
/// a fixed format to CAPTURE clients (Chrome, Firefox, Zoom) between attaches.
fn apply_loopback_controls(dev: &Device) {
    // Best-effort: older module builds may lack these controls.
    for (id, name, value) in [
        (CID_KEEP_FORMAT, "keep_format", CtrlValue::Boolean(true)),
        (
            CID_SUSTAIN_FRAMERATE,
            "sustain_framerate",
            CtrlValue::Boolean(true),
        ),
        // Hold the last frame for ~3s if the producer hiccups so getUserMedia
        // does not immediately fail with a black/timeout stream.
        (CID_TIMEOUT, "timeout", CtrlValue::Integer(3000)),
    ] {
        match dev.set_control(Control { id, value }) {
            Ok(()) => debug!(control = name, "loopback control set"),
            Err(e) => debug!(control = name, error = %e, "loopback control not set"),
        }
    }
}

// ---------------------------------------------------------------------------
// Public sink
// ---------------------------------------------------------------------------

pub struct V4l2LoopSink {
    path: PathBuf,
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

impl super::Sink for V4l2LoopSink {
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
            let active = self.active.as_mut().expect("active checked above");
            let (nw, nh, nf) = active.negotiated;
            (nw != frame.width || nh != frame.height, nw, nh, nf)
        };

        if needs_scaling && frame.format == PixelFormat::Nv12 && neg_fmt == PixelFormat::Nv12 {
            // Fast path: reuse pre-allocated scale context and buffer
            let expected_size = neg_fmt.packed_size(neg_w, neg_h).unwrap_or(0);

            // Create or reuse scale context (LUTs are pre-computed once)
            if self.scale_ctx.is_none() {
                self.scale_ctx = Some(ScaleContext::new(frame.width, frame.height, neg_w, neg_h));
            }

            // Grow output buffer if needed (usually only on first frame)
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
                Err(io::Error::new(
                    io::ErrorKind::Other,
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
