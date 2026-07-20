# vcam-proxy

Physical camera → virtual loopback proxy for Linux.

Captures from a real webcam and streams the frames to a `v4l2loopback` virtual device, making your real camera available as a "virtual camera" in Chrome, Firefox, Zoom, Teams, OBS, etc.

## Features

- **Zero-copy pipeline**: Frames go camera → kernel-mapped buffer → virtual device (one memcpy per frame)
- **Tray icon**: Right-click to see the status (captured/written/dropped, resolution, fps), toggle the virtual camera on/off, or open the config file (uses D-Bus, no GTK)
- **Persistent settings**: Save your preferred camera, resolution, fps, and device settings to `~/.config/vcam-proxy/config.toml`
- **Multi-reader mode**: Configure v4l2loopback to allow multiple apps to use the virtual camera simultaneously
- **Auto-detect**: Scans `/dev/video*` for loopback devices, falls back gracefully
- **Error recovery**: Camera disconnect/reconnect handled transparently
- **Dry-run mode**: Test capture without a virtual device
- **Cross-platform**: Linux (v4l2loopback), Windows (named pipe), macOS (capture only)

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

#### Option A: Let vcam-proxy handle it automatically (recommended)

```bash
# vcam-proxy will auto-install v4l2loopback-dkms and load the module
cargo run --release -- --auto-load-module

# Or use the full setup wizard
cargo run --release -- --setup
```

#### Option B: Manual installation

```bash
# Debian/Ubuntu
sudo apt install v4l2loopback-dkms v4l-utils

# Fedora
sudo dnf install v4l2loopback

# Arch Linux / CachyOS
sudo pacman -S v4l2loopback-dkms v4l-utils

# openSUSE
sudo zypper install v4l2loopback

# Then load the module
sudo modprobe v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=1

# Optional: Load at boot (persistent)
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
| `--auto-load-module` | false | Auto-install (if needed) and load v4l2loopback via pkexec |
| `--retry-ms` | 1000 | Backoff between camera re-open attempts |
| `--multi.Reader` | true | Enable multi-reader virtual camera mode |
| `--exclusive-caps` | 1 | v4l2loopback exclusive_caps (0 or 1) |
| `--timeout` | 1000 | v4l2loopback frame timeout in ms |
| `--save-config` | false | Save current settings to config file |
| `--edit-config` | false | Open config file in default editor |
| `--show-config` | false | Show current settings and exit |

---

## Persistent Settings

vcam-proxy stores your preferred settings in `~/.config/vcam-proxy/config.toml`. CLI arguments override file settings.

### Quick Start

```bash
# Save your preferred settings
cargo run --release -- --camera 0 --width 1280 --height 720 --fps 30 --save-config

# Edit the config file directly
cargo run --release -- --edit-config

# Show current effective settings
cargo run --release -- --show-config
```

### Config File Example

```toml
# ~/.config/vcam-proxy/config.toml
camera = 0
device = "/dev/video10"
width = 1280
height = 720
fps = 30
buffers = 4
format = "auto"
retry_ms = 1000
multi_reader = true
exclusive_caps = 1
timeout = 1000
```

### Precedence

Settings are resolved in this order (later overrides earlier):
1. Built-in defaults
2. Values from `~/.config/vcam-proxy/config.toml`
3. CLI arguments

---

## Tray Icon

When the tray icon is active (default on Linux desktop):

- **Left-click icon**: Opens context menu
- **Right-click icon**: Opens context menu
- **🎥 Camera icon**: Green = virtual camera ON, Red = OFF
- **Hover tooltip**: Shows camera status, resolution, fps, and frame statistics
- **Menu items**:
  - `Status: ON/OFF (WxH @ Nfps)` — current status display
  - `Captured: N frames` — total frames captured
  - `Written: N frames` — total frames written to virtual device
  - `Dropped: N frames` — total frames dropped (back-pressure or no consumer)
  - `Turn Virtual Camera ON/OFF` — toggles output without restarting capture
  - `Open Config File` — opens `~/.config/vcam-proxy/config.toml` in your editor
  - `Quit` — stops the application gracefully

To disable the tray icon: `--no-tray`

---

## Multi-Reader Mode (Multiple Apps)

By default, vcam-proxy configures the virtual camera for UVC compatibility (`exclusive_caps=1`), which makes apps like Chrome and Zoom recognize it as a webcam.

**The problem**: some applications open the virtual camera with exclusive access, preventing other apps from using it simultaneously.

### Solution: Multi-Reader Mode

Enable multi-reader mode in your config file:

```toml
multi_reader = true
exclusive_caps = 1
```

Or via CLI:

```bash
cargo run --release -- --multi-reader --exclusive-caps 1 --save-config
```

### How It Works

| Mode | `devices` | Behavior |
|------|-----------|----------|
| Default | 1 | Single virtual device, first app may get exclusive access |
| Multi-reader | 2 | Creates 2 isolated device nodes for concurrent access |

> **Note**: The `exclusive_caps=1` parameter is what makes apps recognize the device as a camera. Setting it to `0` may improve multi-app compatibility but some apps won't recognize it as a camera.

### Recommended Kernel Module Configuration

For multi-reader mode:

```bash
# Single device (default, UVC-compatible)
sudo modprobe v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=1

# Two devices (better multi-app compatibility)
sudo modprobe v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=2

# Persistent configuration
echo 'options v4l2loopback exclusive_caps=1 card_label="vcam-proxy" devices=2' \
    | sudo tee /etc/modprobe.d/v4l2loopback.conf
```

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
