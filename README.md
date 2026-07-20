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
devices = 1
exclusive_caps = 1
timeout = 0                 # 0 = keep last frame (no green reconnect flash)
auto_load_module = true     # pkexec modprobe when module missing
auto_resolution = true      # use camera's highest mode
```

| Key | Default | Meaning |
|-----|---------|---------|
| `multi_reader` | `true` | Several apps can open the virtual cam at once |
| `auto_load_module` | `true` | Load/install v4l2loopback automatically |
| `auto_resolution` | `true` | Prefer max camera mode over `width`/`height` |
| `exclusive_caps` | `1` | Required for Chrome/Firefox/Zoom to list the device |
| `timeout` | `0` | Keep last frame forever (`0`); ms otherwise |
| `format` | `auto` | Always wires YUYV to consumers |

Change values in the **Settings** window (tray → Settings…) and click **Save to config** / **Apply & Restart**.

CLI flags are optional overrides only. Prefer the config file.

---

## Features

- Settings GUI (egui) + system tray (ksni, Wayland-friendly)
- Persistent `config.toml`
- Auto-load / auto-install v4l2loopback
- Multi-reader on one node (`max_openers`)
- Auto-resolution + YUYV output for browsers
- Camera reconnect recovery
- Still-image source for tests: `cargo run -- --image path.png --no-gui --no-tray`

---

## Manual module load (if you disable auto_load)

```bash
sudo modprobe v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1 video_nr=10

# Persist at boot
echo 'options v4l2loopback exclusive_caps=1 card_label=vcam-proxy devices=1 video_nr=10' \
  | sudo tee /etc/modprobe.d/v4l2loopback.conf
echo 'v4l2loopback' | sudo tee /etc/modules-load.d/v4l2loopback.conf
```

---

## Wayland

Tray (ksni) works on GNOME/KDE Wayland. The winit message *“Unminimizing is ignored on Wayland”* is a compositor protocol limit — harmless. Settings reopen via tray **Focus**, not un-minimize.

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
| Green blink / reconnect flash | Fixed in current builds; keep `timeout = 0` |
| `cargo run` asks which binary | Fixed via `default-run = "vcam-proxy"` — use plain `cargo run` |

---

## Architecture

```
Physical camera (/dev/video0)
        │
        ▼
  capture thread  →  BufferPool / channel  →  sink thread
                                                   │
                                                   ▼
                                         v4l2loopback (/dev/video10)
                                                   │
                                    Chrome / Zoom / OBS / …
```

Tray + Settings GUI share the live on/off switch and config with the pipeline.

---

## License

MIT OR Apache-2.0
