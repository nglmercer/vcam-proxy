# Linux Virtual Camera Implementation Plan

## Goal
Build a fully functional virtual camera on Linux using `v4l2loopback` that:
- Captures from a physical camera → outputs to a virtual `/dev/videoN` device
- Is selectable in Chrome, Firefox, Zoom, Teams, etc.
- Has a tray icon for easy ON/OFF toggle
- Auto-recovers from camera disconnects

## Current Status

### Completed
- [x] Capture pipeline (nokhwa → v4l2loopback)
- [x] Fix nokhwa-core Closest fulfillment bug (vendored patch)
- [x] Tray icon with ON/OFF toggle (ksni D-Bus)
- [x] Sink loop respects toggle (standby mode)
- [x] Architecture report (docs/VIRTUAL_CAMERA_REPORT.md)

### Remaining Work

## Phase 1: Robust v4l2loopback Integration (Foundation)

### 1.1 Auto-detect and validate loopback device
**File**: `src/sink/v4l2loopback.rs` (new, replaces current v4l2loop.rs)

```rust
pub fn detect_loopback_device(preferred: &Path) -> Result<PathBuf, Error> {
    // 1. Check if preferred path exists and supports VIDEO_OUTPUT
    // 2. If not, scan /dev/video* for first device with V4L2_CAP_VIDEO_OUTPUT
    // 3. Validate exclusive_caps=1 by checking device caps
    // 4. Return error with actionable suggestions if nothing found
}
```

### 1.2 Smart module loading helper
**File**: `src/sink/module.rs` (new)

```rust
pub fn ensure_loopback_loaded() -> Result<(), Error> {
    // 1. Check /proc/modules for v4l2loopback
    // 2. If missing, attempt: pkexec modprobe v4l2loopback exclusive_caps=1
    // 3. Verify /dev/video* appeared
    // 4. If pkexec unavailable, print manual command and exit gracefully
}
```

### 1.3 Format negotiation resilience
**File**: `src/sink/v4l2loopback.rs`

- Handle cameras that only output MJPEG at high resolutions
- Add automatic YUYV ↔ MJPEG fallback chain
- Validate negotiated format against loopback capabilities
- Clear error messages when format is unsupported

### 1.4 Permissions helper
**File**: `src/permissions.rs` (new)

```rust
pub fn check_video_permissions(device: &Path) -> Result<(), Error> {
    // 1. Test read/write access to /dev/videoN
    // 2. If permission denied: suggest `sudo usermod -aG video $USER`
    // 3. Check if user is in 'video' group
}
```

## Phase 2: Better Error Recovery

### 2.1 Loopback device loss recovery
- Detect when /dev/videoN disappears (module unloaded, device removed)
- Queue frames while waiting for device to reappear (up to N seconds)
- Re-initialize stream on new device

### 2.2 Format change mid-stream
- Handle cameras that change format/resolution on the fly
- Re-negotiate loopback format without dropping the channel
- Reset MmapStream cleanly

### 2.3 Consumer attach/detect
- Detect when a reader connects to /dev/videoN
- Log "consumer attached" / "consumer detached" events
- Optionally pause capture when no consumer is attached (save CPU)

## Phase 3: CLI Enhancements

### 3.1 New flags
```bash
--auto-load-module    # Try to modprobe v4l2loopback if not loaded
--list-loopback       # List available loopback devices and exit
--dry-run             # Test capture without writing to loopback
--no-tray             # Disable tray icon
```

### 3.2 Status display
- Show negotiated format on startup
- Show actual output resolution (may differ from requested)
- FPS counter in tray tooltip

## Phase 4: Installation & Packaging

### 4.1 systemd user service
**File**: `assets/vcam-proxy.service`

```ini
[Unit]
Description=vcam-proxy virtual camera
After=graphical-session.target

[Service]
ExecStart=/usr/local/bin/vcam-proxy --device /dev/video10
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
```

### 4.2 udev rule for permissions
**File**: `assets/99-vcam-proxy.rules`

```
KERNEL=="video*", GROUP="video", MODE="0660"
```

### 4.3 Install script
**File**: `scripts/install.sh`

```bash
#!/bin/bash
# - Install binary to /usr/local/bin/
# - Install systemd service
# - Install udev rules
# - Load v4l2loopback with sensible defaults
# - Add user to video group
```

## Phase 5: Testing & Validation

### 5.1 Integration tests
- Test with v4l2loopback loaded/unloaded
- Test with multiple resolutions
- Test with MJPEG-only cameras
- Test consumer attach/detach cycles

### 5.2 Validation checklist
- [ ] Appears in Chrome `chrome://settings/camera`
- [ ] Appears in Firefox `about:preferences#privacy`
- [ ] Appears in Zoom video settings
- [ ] Frames flow at expected FPS
- [ ] Tray toggle works while app running in video call
- [ ] Recovers from USB camera disconnect/reconnect
- [ ] Works after suspend/resume

## Phase 6: Future Enhancements (Post-MVP)

### 6.1 Multiple virtual cameras
- Support outputting to multiple /dev/videoN simultaneously
- Different formats per output

### 6.2 Frame processing pipeline
- software crop/scale
- Overlay text (timestamp, FPS)
- Color adjustment

### 6.3 Configuration file
- `~/.config/vcam-proxy/config.toml`
- Persistent settings between runs

### 6.4 D-Bus API
- Expose control interface for external tools
- Allow other apps to enable/disable camera remotely

## Architecture Diagram

```
Physical Camera (/dev/video0)
        │
        ▼
┌─────────────────┐
│  capture thread  │  nokhwa → BufferPool → crossbeam channel
│  (auto-recovery) │
└────────┬────────┘
         │ Frame { width, height, format, payload }
         ▼
┌─────────────────┐
│   sink thread    │  check SinkSwitch → v4l2loopback write
│  (v4l2 output)   │  re-opens on format change / device loss
└────────┬────────┘
         │ YUYV/NV12 frames
         ▼
Virtual Camera (/dev/video10)
         │
         ▼
┌─────────────────┐
│  Consumer Apps   │  Chrome, Zoom, Teams, OBS, etc.
└─────────────────┘

┌─────────────────┐
│   tray thread    │  ksni D-Bus StatusNotifierItem
│  (UI control)    │  shares SinkSwitch + Shutdown with main
└─────────────────┘
```

## Next Sprint: Phase 1 Implementation

Priority order:
1. v4l2loopback device detection (`detect_loopback_device`)
2. Permissions checker (`check_video_permissions`)
3. Module loading helper (`ensure_loopback_loaded`)
4. Better error messages throughout
5. Add `--list-loopback` and `--dry-run` flags
