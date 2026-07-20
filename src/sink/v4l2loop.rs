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

/// Kernel buffers requested from the loopback driver.
const NUM_KBUF: u32 = 4;
/// Bound on QBUF/DQBUF waits so the thread stays responsive to shutdown and
/// to a missing consumer (v4l2loopback only drains output buffers while a
/// reader is attached).
const POLL_TIMEOUT_MS: u64 = 500;

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

/// Scan /dev/video* for all devices supporting VIDEO_OUTPUT capability.
/// Returns them sorted by path name for deterministic ordering.
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

        if let Some(dev) = probe_output_device(&path) {
            devices.push(dev);
        }
    }

    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

/// Probe a single device path; returns Some(LoopbackDevice) if it supports
/// video output, None if it cannot be opened or lacks the capability.
fn probe_output_device(path: &Path) -> Option<LoopbackDevice> {
    let dev = Device::with_path(path).ok()?;
    let caps = dev.query_caps().ok()?;

    // Must advertise video output capability
    if !caps.capabilities.contains(Flags::VIDEO_OUTPUT) {
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
/// 1. `preferred` path if it exists and supports VIDEO_OUTPUT
/// 2. First v4l2loopback device found by scanning
/// 3. Any VIDEO_OUTPUT device if no v4l2loopback found
pub fn find_loopback_device(preferred: &Path) -> Result<PathBuf, LoopbackError> {
    // 1. Try preferred device first
    if preferred.exists() {
        if let Some(dev) = probe_output_device(preferred) {
            info!(device = %dev.path.display(), card = %dev.card, "using preferred loopback device");
            return Ok(dev.path);
        }
        warn!(path = %preferred.display(), "preferred device does not support video output; scanning for alternatives");
    }

    // 2. Scan all /dev/video* for loopback devices
    let all_devices =
        discover_loopback_devices().map_err(|e| LoopbackError::ScanFailed { source: e })?;

    // 3. Prefer v4l2loopback devices
    // Note: the kernel driver name may be "v4l2 loopback" (with space) depending on version.
    let loopback_devices: Vec<_> = all_devices
        .iter()
        .filter(|d| d.driver == "v4l2 loopback" || d.driver == "v4l2loopback")
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

// ---------------------------------------------------------------------------
// Public sink
// ---------------------------------------------------------------------------

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
