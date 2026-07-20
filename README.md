# vcam-proxy

Physical camera → virtual loopback proxy for Linux.

Captures from a real webcam and streams the frames to a `v4l2loopback` virtual device, making your real camera available as a "virtual camera" in Chrome, Firefox, Zoom, Teams, OBS, etc.

## Features

- **Zero-copy pipeline**: Frames go camera → kernel-mapped buffer → virtual device (one memcpy per frame)
- **Tray icon**: Right-click to toggle the virtual camera on/off without stopping capture (uses D-Bus, no GTK)
- **Auto-detect**: Scans `/dev/video*` for loopback devices, falls back gracefully
- **Error recovery**: Camera disconnect/reconnect handled transparently
- **Dry-run mode**: Test capture without a virtual device

---

## Install (CachyOS / Arch Linux)

### 1. Install dependencies

```bash
# v4l2loopback kernel module + utilities
sudo pacman -S v4l2loopback-dkms v4l-utils

# Build dependencies (if building from source)
sudo pacman -S base-devel clang
```

### 2. Load the kernel module

```bash
# Load now (temporary)
sudo modprobe v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=1

# Load at boot (persistent)
echo 'options v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=1' \
    | sudo tee /etc/modprobe.d/v4l2loopback.conf
echo 'v4l2loopback' | sudo tee /etc/modules-load.d/v4l2loopback.conf
```

### 3. Fix permissions

```bash
# Add yourself to the 'video' group
sudo usermod -aG video $USER

# IMPORTANT: Log out and log back in for the group change to take effect!
```

### 4. Verify

```bash
# Should show your camera and the virtual one
v4l2-ctl --list-devices

# Or use vcam-proxy's built-in scanner
cargo run -- --list-loopback
expected output:
  Video output devices (1 found):
    /dev/video10 vam-proxy [v4l2loopback ✓]
```

---

## Build & Run

```bash
# Build
cargo build --release

# Basic usage (uses /dev/video10 by default)
cargo run --release

# Auto-load the kernel module (requires pkexec/polkit)
cargo run --release -- --auto-load-module

# Specify a different virtual device
cargo run --release -- --device /dev/video12

# Test capture without a virtual device
cargo run --release -- --dry-run

# List available virtual devices and exit
cargo run --release -- --list-loopback

# Run without tray icon
cargo run --release -- --no-tray

# Capture from camera 1, output 720p at 30fps
cargo run --release -- --camera 1 --width 1280 --height 720 --fps 30
```

---

## CLI Reference

| Flag | Default | Description |
|------|---------|-------------|
| `--list` | false | List physical cameras and exit |
| `--list-loopback` | false | List virtual output devices and exit |
| `--camera` | 0 | Physical camera index |
| `--device` | `/dev/video10` | Virtual device node |
| `--width` | 1280 | Requested capture width |
| `--height` | 720 | Requested capture height |
| `--fps` | 30 | Requested frame rate |
| `--buffers` | 4 | Number of frame buffers in circulation |
| `--format` | `auto` | Wire format: `auto`, `yuy2`, `rgb24`, `nv12`, `mjpeg` |
| `--sink` | `auto` | Sink backend: `auto`, `v4l2`, `null` |
| `--dry-run` | false | Test capture without writing to virtual device |
| `--no-tray` | false | Disable system tray icon |
| `--auto-load-module` | false | Auto-load v4l2loopback via pkexec |
| `--retry-ms` | 1000 | Backoff between camera re-open attempts |

---

## Tray Icon

When the tray icon is active (default on Linux desktop):

- **Left-click icon**: Opens context menu
- **Right-click icon**: Opens context menu
- **🎥 Camera icon**: Green = virtual camera ON, Red = OFF
- **Menu items**:
  - `Turn Virtual Camera ON/OFF` — toggles output without restarting capture
  - `Quit` — stops the application gracefully

To disable the tray icon: `--no-tray`

---

## Troubleshooting

### "No virtual camera device found"

```bash
# Load the module
sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1

# Or let vcam-proxy try to load it
cargo run --release -- --auto-load-module

# Verify the device appeared
ls -la /dev/video*
v4l2-ctl --list-devices
```

### "Permission denied" on /dev/videoN

```bash
# Add yourself to video group
sudo usermod -aG video $USER

# Log out and back in!
# Verify with:
groups | grep video
```

### Camera doesn't appear in Chrome/Zoom

Make sure `exclusive_caps=1` is set when loading v4l2loopback. This is what allows apps to open the device as a "camera" (capture-capable) instead of just an output device.

```bash
# Reload with correct settings
sudo modprobe -r v4l2loopback
sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1
```

### Green/purple tint in video

Format mismatch between what the camera outputs and what the sink expects. Try forcing the format:

```bash
cargo run --release -- --format yuy2    # for most USB cameras
cargo run --release -- --format rgb24   # for some webcams
```

### Module not loading on CachyOS/Arch

If `v4l2loopback-dkms` fails to build for your kernel version:

```bash
# Check if the module is available for your kernel
dkms status

# Manually build
sudo dkms install v4l2loopback/$(pacman -Q v4l2loopback-dkms | awk '{print $2}' | cut -d- -f1)

# For the CachyOS kernel (linux-cachyos), you may need:
sudo pacman -S linux-cachyos-headers
```

---

## Architecture

```
Physical Camera (/dev/video0)
        │
        ▼
┌─────────────────┐
│  capture thread  │  nokhwa → BufferPool → crossbeam channel
└────────┬────────┘
         │ Frame { width, height, format, payload }
         ▼
┌─────────────────┐
│   sink thread    │  check SinkSwitch → v4l2loopback mmap write
└────────┬────────┘
         │ YUYV/NV12 frames
         ▼
Virtual Camera (/dev/video10)
         │
         ▼
┌─────────────────┐
│  Consumer Apps   │  Chrome, Zoom, Teams, OBS
└─────────────────┘

┌─────────────────┐
│   tray thread    │  ksni D-Bus StatusNotifierItem
│  (UI control)    │  shares SinkSwitch + Shutdown with main
└─────────────────┘
```

---

## License

MIT OR Apache-2.0
