# vcam-proxy

Physical camera → virtual loopback for Linux (Chrome, Firefox, Zoom, Teams, OBS).

**Configure once in a file or the GUI — then just run with no flags.**

```bash
cargo run
# or after install:
vcam-proxy
```

Settings live in `~/.config/vcam-proxy/config.toml`. First launch creates that file with every useful feature enabled (multi-reader, auto-load module, auto-resolution, tray, settings GUI).

---

## Quick start (CachyOS / Arch)

```bash
sudo pacman -S v4l2loopback-dkms v4l-utils base-devel clang
sudo usermod -aG video $USER   # then log out/in

cargo run
```

On first start vcam-proxy will:

1. Write `~/.config/vcam-proxy/config.toml`
2. Auto-load `v4l2loopback` via pkexec if needed (polkit prompt)
3. Open the settings window (later runs stay in the tray)

Then pick **vcam-proxy** as the camera in your browser or video app.

Optional one-shot checks:

```bash
cargo run -- --list              # physical cameras
cargo run -- --list-loopback     # virtual devices
cargo run -- --setup             # system check + guidance
cargo run -- --edit-config       # open config.toml in your editor
cargo run -- --show-config       # print effective settings
```

---

## Config file (preferred)

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
devices = 1                 # nodes to feed; multi_reader auto-raises this to >= 2
exclusive_caps = 1
timeout = 0                 # 0 = keep last frame (no green reconnect flash)
auto_load_module = true     # pkexec modprobe when module missing
auto_resolution = true      # use camera's highest mode
```

| Key | Default | Meaning |
|-----|---------|---------|
| `multi_reader` | `true` | Several apps can use the virtual cam at once — feeds **one node per app** (see below) |
| `devices` | `1` | Loopback nodes to create/feed; `multi_reader = true` auto-raises to ≥ 2 |
| `auto_load_module` | `true` | Load/install v4l2loopback automatically |
| `auto_resolution` | `true` | Prefer max camera mode over `width`/`height` |
| `exclusive_caps` | `1` | Required for Chrome/Firefox/Zoom to list the device (applied to every node) |
| `timeout` | `0` | Keep last frame forever (`0`); ms otherwise |
| `format` | `auto` | Always wires YUYV to consumers |

> **How multi-app works:** v4l2loopback ≥ 0.14 allows only **one streaming reader per
> device node** — the first app to stream owns the node and every other app gets
> `EBUSY` ("Device or resource busy"). So vcam-proxy creates one node **per app**:
> `vcam-proxy` (`/dev/video10`), `vcam-proxy-2` (`/dev/video11`), … Assign each app
> its own camera name (e.g. OBS → `vcam-proxy`, browser → `vcam-proxy-2`).

Change values in the **Settings** window (tray → Settings…) and click **Save to config** / **Apply & Restart**.

CLI flags are optional overrides only. Prefer the config file.

---

## Features

- Settings GUI (egui) + system tray (ksni, Wayland-friendly)
- Persistent `config.toml`
- Auto-load / auto-install v4l2loopback
- Multi-app support: one labeled virtual camera per app (auto multi-node)
- Auto-resolution + YUYV output for browsers
- Camera reconnect recovery
- Still-image source for tests: `cargo run -- --image path.png --no-gui --no-tray`

---

## Manual module load (if you disable auto_load)

```bash
# Single node:
sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1 video_nr=10 max_openers=16

# Multi-app (one node per app) — note the per-node arrays:
sudo modprobe v4l2loopback exclusive_caps=1,1 card_label=vcam-proxy,vcam-proxy-2 devices=2 video_nr=10,11 max_openers=16

# Persist at boot
echo 'options v4l2loopback exclusive_caps=1,1 card_label=vcam-proxy,vcam-proxy-2 devices=2 video_nr=10,11 max_openers=16' \
  | sudo tee /etc/modprobe.d/v4l2loopback.conf
echo 'v4l2loopback' | sudo tee /etc/modules-load.d/v4l2loopback.conf
```

---

## Wayland

Tray (ksni) works on GNOME/KDE Wayland. The winit message *“Unminimizing is ignored on Wayland”* is a compositor protocol limit — harmless. Settings reopen via tray **Focus**, not un-minimize.

Viewport commands (`Minimized`/`Visible`) are only sent on **state transitions**, not every frame — this eliminates redundant Wayland xdg-toplevel protocol spam that previously occurred every 200ms while the window was hidden.

Headless: `cargo run -- --no-gui --no-tray`

---

## Tests

```bash
cargo test --lib
cargo test --test pixel_integrity
# live loopback (close browser camera preview first):
cargo test --test pixel_integrity multi_reader_pixel_integrity -- --ignored --nocapture
```

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| No virtual device | Ensure `auto_load_module = true`, approve pkexec, or `sudo modprobe v4l2loopback …` |
| Permission denied | `sudo usermod -aG video $USER` then re-login |
| Not listed in Chrome/Zoom | `exclusive_caps = 1` (reload module if it was loaded with `0`) |
| Second app says "Device or resource busy" | Driver ≥ 0.14 allows only **one reader per node**. Keep `multi_reader = true`, close all camera apps once so the module can reload with 2 nodes, then assign each app its own camera (`vcam-proxy`, `vcam-proxy-2`). More apps: set `devices = 3+` |
| Green blink / reconnect flash | Fixed in current builds; keep `timeout = 0` |
| `cargo run` asks which binary | Fixed via `default-run = "vcam-proxy"` — use plain `cargo run` |

### Why a second app couldn't use the virtual camera (fixed)

Two separate bugs caused the "OBS / browser can't access the virtual camera" symptom:

**1. The writer disrupted attached readers (fixed earlier).**
vcam-proxy used to re-open the v4l2loopback OUTPUT device on every transient write error and
every pixel-format change. Each re-open calls `VIDIOC_S_FMT` + toggles `keep_format`, which tears
down all attached CAPTURE clients. Like OBS's virtual camera, vcam-proxy now opens the OUTPUT fd
**once** and writes forever:

- The OUTPUT fd is opened on the first frame and kept alive across transient errors.
- The device is only re-opened after **60 consecutive** write failures (~2s at 30fps), which
  only happens when the device is truly gone (module unloaded, node removed).
- Back-pressure (`WouldBlock`/`TimedOut` — no reader draining) never triggers a re-open.

**2. The kernel driver allows only ONE reader per node (fixed by multi-node).**
v4l2loopback ≥ 0.14 hands the CAPTURE stream token to a **single** opener per device node —
the first app to stream owns it, and every additional app fails `VIDIOC_REQBUFS` with
`EBUSY` ("Device or resource busy"). `max_openers` only limits open *file descriptors*,
not *streams*, so it cannot help. (Releases ≤ 0.13 broadcast to many readers on one node —
that is the behaviour OBS's virtual camera appeared to have.)

Userspace cannot negotiate around this, so vcam-proxy now does the only thing that works on
modern drivers: **feed one labeled node per app**. With `multi_reader = true` it automatically
creates at least 2 nodes and writes every frame to all of them. Each app is then assigned its
own camera by name (`vcam-proxy`, `vcam-proxy-2`, …) and gets an exclusive, full-rate stream.

- Per-node `exclusive_caps` and `card_label` arrays are set correctly (a scalar used to leave
  extra nodes browser-invisible and nameless).
- The invalid `timeout` module parameter is no longer passed to `modprobe` (it is an ioctl
  control, still applied at runtime).
- Reloading the module requires **all camera apps closed** (`modprobe -r` fails while the
  device is busy); vcam-proxy detects this and tells you.

---

## Architecture

```
Physical camera (/dev/video0)
        │
        ▼
  capture thread  →  BufferPool / channel  →  sink thread
                                                   │
                              ┌────────────────────┼────────────────────┐
                              ▼                    ▼                    ▼
                     v4l2loopback video10   video11 ("vcam-proxy-2")   …
                     ("vcam-proxy")              │
                        OBS                    Chrome / Zoom / …
                    (one app per node — driver ≥ 0.14 allows only one reader per node)
```

Tray + Settings GUI share the live on/off switch and config with the pipeline.

---

## License

MIT OR Apache-2.0
