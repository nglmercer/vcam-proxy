# Virtual Camera Architecture Report & Roadmap

## 1. How OBS Implements Virtual Camera

OBS creates a fake webcam visible to Zoom, Chrome, Teams, etc.

**Pipeline**: Capture source -> composite/scale -> format convert -> push to virtual device

**Tray Integration** (Qt's QSystemTrayIcon):

```
System Tray (OBS icon)
|-- Show/Hide
|-- Start/Stop Virtual Camera   <- label toggles dynamically
|-- Virtual Camera Output...    <- config dialog (program/scene/source)
|-- Start Streaming / Recording
|-- Exit
```

Toggle: check VirtualCamActive() -> StopVirtualCam() or StartVirtualCam()
State change: QAction label updates + frontend event emitted.

Config: Main Output, Specific Scene, Specific Source
Mid-stream change: "delayed restart" (stop -> 100ms -> restart)

## 2. Linux: v4l2loopback Deep Dive

### Technology Stack
- Kernel Module: v4l2loopback.ko (loadable kernel module)
- Userspace: V4L2 ioctls + write() syscall to /dev/videoN
- Critical parameter: exclusive_caps=1 (masks as output-only, required for WebRTC apps)

### What is v4l2loopback?
A kernel module that creates virtual V4L2 (Video4Linux2) devices.
When loaded, it registers /dev/videoN nodes that behave like real cameras
but receive data from userspace instead of hardware.

```bash
# Load module (creates /dev/video10)
sudo modprobe v4l2loopback devices=1 video_nr=10 exclusive_caps=1 \
    card_label="vcam-proxy Virtual Camera"
```

### OBS Integration (plugins/linux-v4l2/v4l2-output.c)
1. Checks /proc/modules for v4l2loopback
2. If not loaded: pkexec modprobe v4l2loopback exclusive_caps=1 card_label='OBS Virtual Camera'
3. Scans /dev/video* for device with V4L2_CAP_VIDEO_OUTPUT
4. Sets format via V4L2 ioctls, writes frames via write() syscall
5. Frame format: YUY2 (V4L2_PIX_FMT_YUYV)

### V4L2 Ioctl Sequence
```
VIDIOC_QUERYCAP    -> verify V4L2_CAP_VIDEO_OUTPUT
VIDIOC_G_FMT       -> get current format
VIDIOC_S_PARM      -> set framerate (timeperframe)
VIDIOC_S_FMT       -> set format: YUYV, width, height, sizeimage
VIDIOC_STREAMON    -> start streaming
--- frames flow via write() syscall ---
VIDIOC_STREAMOFF   -> stop
```

### Manual Setup
```bash
# Install (Debian/Ubuntu)
sudo apt install v4l2loopback-dkms v4l-utils

# Persistent config
echo 'options v4l2loopback devices=1 video_nr=10 exclusive_caps=1 card_label="Virtual Camera"' \
    | sudo tee /etc/modprobe.d/v4l2loopback.conf

# Load now
sudo modprobe v4l2loopback

# Verify
v4l2-ctl --list-devices
```

## 3. Windows: DirectShow Filter

- NOT a kernel driver - user-mode COM DLL
- IPC: Named Shared Memory ("OBSVirtualCamVideo")
- Format: Internal NV12, converts to I420/YUY2 on demand
- Registration: regsvr32 obs-virtualcam-module64.dll at install time
- Architecture: obs64.exe -> shared memory -> DirectShow DLL -> consumer apps

## 4. macOS: CoreMediaIO DAL Plugin

- CoreMediaIO Device Abstraction Layer (DAL) plugin (.plugin bundle)
- IPC: Mach IPC
- Format: NV12
- Location: /Library/CoreMediaIO/Plug-Ins/DAL/

## 5. Alternatives & Libraries

| Library | Language | Platforms | Approach |
|---------|----------|-----------|----------|
| v4l2loopback | C (kernel) | Linux | Canonical kernel module |
| pyvirtualcam | Python | Win/Mac | DirectShow (Win) / CoreMediaIO (Mac) |
| akvirtualcamera | C++ | Lin/Win | v4l2loopback + DirectShow |
| gstreamer | C | All | v4l2 sink element |

Rust: Use v4l crate (already in vcam-proxy deps) for Linux. Cross-platform needs per-OS backends.

## 6. Next Steps & Recommendations for vcam-proxy

### Phase 1 (Done)
- [x] Fix nokhwa-core Closest fulfillment bug (vendored patch)
- [x] Capture pipeline working

### Phase 2 (Current) - Tray Icon & Easy Toggle
- [ ] Add system tray icon with on/off toggle
- [ ] Show camera state visually (green/red indicator)
- [ ] One-click start/stop virtual camera
- [ ] Survives terminal close (daemon mode)

### Phase 3 (Future)
- [ ] Auto-detect v4l2loopback, offer to load via pkexec
- [ ] Resolution/format selection from tray menu
- [ ] Frame rate + drop counter display
- [ ] Cross-platform backends (Windows DirectShow, macOS DAL)

## 7. Quick Start Guide

```bash
# 1. Load v4l2loopback kernel module
sudo modprobe v4l2loopback devices=1 video_nr=10 exclusive_caps=1 \
    card_label="vcam-proxy Virtual Camera"

# 2. Fix permissions (then log out/in)
sudo usermod -aG video $USER

# 3. Run vcam-proxy (capture /dev/video0 -> output /dev/video10)
cargo run --release

# 4. Open Chrome/Zoom and select "vcam-proxy Virtual Camera"
```

### Troubleshooting

| Problem | Fix |
|---------|-----|
| /dev/video10 not found | sudo modprobe v4l2loopback devices=1 video_nr=10 |
| Permission denied | sudo usermod -aG video $USER + relogin |
| Camera not in app list | Add exclusive_caps=1 to module params |
| Green/purple tint | Format mismatch - ensure YUYV passthrough |

## References

- https://github.com/umlaeute/v4l2loopback
- https://github.com/obsproject/obs-studio
- https://www.kernel.org/doc/html/latest/userspace-api/media/v4l/v4l2.html
