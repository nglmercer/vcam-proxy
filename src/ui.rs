//! In-app settings GUI (egui / eframe).
//!
//! This is the primary configuration surface: every option that used to live
//! behind a CLI flag is editable here. The window is:
//!
//! - **Open on first run** (no config file present) as a guided setup.
//! - **Hidden afterwards**, reachable from the tray "Settings…" menu item or
//!   the `--settings` flag. A `--no-gui` flag restores pure CLI behaviour.
//!
//! egui's event loop *must* run on the main thread (winit restriction), so the
//! GUI owns the main thread. `main` runs the capture/sink pipeline on a
//! background "controller" thread and communicates with the GUI through a shared
//! `GuiState`:
//! - `desired`: the configuration the user is editing (and can apply).
//! - `live_on`: mirrors the virtual-camera on/off switch (live, no restart).
//! - `open_window`: set by the tray / first-run to pop the window up.
//! - `restart`: set by "Apply & Restart" so the controller tears down and
//!   re-spawns the pipeline with the new `desired` config.
//! - `quit`: set by the GUI's Quit button.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::config::{FormatPref, ResolvedConfig, SinkKind};
use crate::settings::Settings;
use crate::shutdown::Shutdown;
use crate::sink::all_loopback_users;

/// Shared mutable state between the GUI thread (main) and the controller thread.
pub struct GuiState {
    /// Configuration currently shown / edited in the window.
    pub desired: ResolvedConfig,
    /// Live on/off mirroring the sink switch (toggle is immediate).
    pub live_on: bool,
    /// Set true to pop the window open; the GUI clears it after showing.
    pub open_window: bool,
    /// Set true when the user clicks "Apply & Restart".
    pub restart: bool,
    /// Set true when the user clicks "Quit".
    pub quit: bool,
    /// Set true once the window has been shown at least once (first-run done).
    pub seen: bool,
}

impl GuiState {
    pub fn new(initial: ResolvedConfig, open_immediately: bool) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            desired: initial,
            live_on: true,
            open_window: open_immediately,
            restart: false,
            quit: false,
            seen: false,
        }))
    }
}

/// Handle the tray (or first-run path) uses to pop the settings window open.
#[derive(Clone)]
pub struct GuiWake {
    state: Arc<Mutex<GuiState>>,
}

impl GuiWake {
    pub fn new(state: Arc<Mutex<GuiState>>) -> Arc<Self> {
        Arc::new(Self { state })
    }

    /// Request the window to open. The GUI polls this flag and re-shows itself
    /// (no OS-level event needed, so it works from any thread).
    pub fn open(&self) {
        self.state.lock().unwrap().open_window = true;
    }
}

/// Run the egui application on the current thread (the main thread). Blocks until
/// the window is allowed to close — i.e. on GUI "Quit" or process shutdown.
///
/// `start_visible` controls whether the native window is mapped at startup.
/// It must be created hidden (rather than hidden after creation) because on
/// Wayland a mapped toplevel window CANNOT be unmapped — `Visible(false)` is
/// a no-op there, which previously left an empty, undrawn window on screen.
pub fn run(state: Arc<Mutex<GuiState>>, shutdown: Shutdown, start_visible: bool) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("vcam-proxy — Settings")
            .with_inner_size([540.0, 680.0])
            .with_resizable(true)
            .with_visible(start_visible),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "vcam-proxy",
        options,
        Box::new(|_cc| Ok(Box::new(App::new(state, shutdown)))),
    );
}

struct App {
    state: Arc<Mutex<GuiState>>,
    shutdown: Shutdown,
    /// Whether the window is currently shown. We drive this explicitly so we
    /// don't toggle visibility every frame (which made the window flash to
    /// black right after opening).
    visible: bool,
    /// Tracks the last visibility state we sent to the viewport, to avoid
    /// redundant Wayland protocol messages. On Wayland, repeatedly sending
    /// `Minimized(true)` every 200ms while hidden spams the compositor with
    /// xdg-toplevel events; this flag ensures we only send it once on
    /// transition. Similarly, `Visible(true)` is only sent when transitioning
    /// from hidden to shown.
    shown: bool,
    // Local UI scratch values (so we can validate before committing).
    camera_text: String,
    device_text: String,
    width_text: String,
    height_text: String,
    fps_text: String,
    buffers_text: String,
    retry_text: String,
    timeout_text: String,
    exclusive_caps_text: String,
    devices_text: String,
    multi_app_timeout_text: String,
    status: String,
}

impl App {
    fn new(state: Arc<Mutex<GuiState>>, shutdown: Shutdown) -> Self {
        let s = state.lock().unwrap();
        let cfg = &s.desired;
        let open_immediately = s.open_window;
        let camera_text = cfg.camera.to_string();
        let device_text = cfg.device.clone();
        let width_text = cfg.width.to_string();
        let height_text = cfg.height.to_string();
        let fps_text = cfg.fps.to_string();
        let buffers_text = cfg.buffers.to_string();
        let retry_text = cfg.retry_ms.to_string();
        let timeout_text = cfg.timeout.to_string();
        let exclusive_caps_text = cfg.exclusive_caps.to_string();
        let devices_text = cfg.devices.to_string();
        let multi_app_timeout_text = cfg.multi_app_timeout.to_string();
        drop(s);
        Self {
            state,
            shutdown,
            visible: open_immediately,
            // If we start visible, the viewport was created with Visible(true)
            // so we're already "shown". If hidden, we haven't sent any command.
            shown: open_immediately,
            camera_text,
            device_text,
            width_text,
            height_text,
            fps_text,
            buffers_text,
            retry_text,
            timeout_text,
            exclusive_caps_text,
            devices_text,
            multi_app_timeout_text,
            status: String::new(),
        }
    }

    /// Parse the local text fields into `cfg`. Returns an error string if
    /// anything fails to parse.
    fn commit(&mut self, cfg: &mut ResolvedConfig) -> Result<(), String> {
        cfg.camera = self
            .camera_text
            .parse()
            .map_err(|_| "camera must be a number".to_string())?;
        cfg.width = self
            .width_text
            .parse()
            .map_err(|_| "width must be a number".to_string())?;
        cfg.height = self
            .height_text
            .parse()
            .map_err(|_| "height must be a number".to_string())?;
        cfg.fps = self
            .fps_text
            .parse()
            .map_err(|_| "fps must be a number".to_string())?;
        cfg.buffers = self
            .buffers_text
            .parse()
            .map_err(|_| "buffers must be a number".to_string())?;
        cfg.retry_ms = self
            .retry_text
            .parse()
            .map_err(|_| "retry_ms must be a number".to_string())?;
        cfg.timeout = self
            .timeout_text
            .parse()
            .map_err(|_| "timeout must be a number".to_string())?;
        cfg.exclusive_caps = self
            .exclusive_caps_text
            .parse()
            .map_err(|_| "exclusive_caps must be 0 or 1".to_string())?;
        cfg.devices = self
            .devices_text
            .parse()
            .map_err(|_| "devices must be a number (1-8)".to_string())?;
        cfg.multi_app_timeout = self
            .multi_app_timeout_text
            .parse()
            .map_err(|_| "multi_app_timeout must be a number (seconds)".to_string())?;
        cfg.device = self.device_text.clone();
        Ok(())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());

        // Open request (tray "Settings…" / first run / --settings): show.
        let want_open = {
            let mut g = self.state.lock().unwrap();
            if g.open_window {
                g.open_window = false;
                g.seen = true;
                true
            } else {
                false
            }
        };
        if want_open {
            self.visible = true;
            // Wayland-safe show: map the window (first time) and raise/focus
            // it. `Focus` uses xdg-activation, which is what restores a
            // minimized window on KWin/Mutter — xdg-shell has NO un-minimize
            // request, so we never send `Minimized(false)` (it is ignored and
            // only spams "Unminimizing is ignored on Wayland" warnings).
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.shown = true;
        }

        // Exit path: the Quit button (or process shutdown) must close the
        // native window so `run_native` returns and the process can exit.
        {
            let (quit, shutting_down) = {
                let g = self.state.lock().unwrap();
                (g.quit, self.shutdown.is_set())
            };
            if quit || shutting_down {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        // Closing the window (vs. an explicit Quit) just hides it — the app
        // keeps running hidden with a tray icon, per the "hidden by default"
        // requirement. We only allow the window to actually close on Quit or
        // process shutdown.
        if close_requested {
            let (quit, shutting_down) = {
                let g = self.state.lock().unwrap();
                (g.quit, self.shutdown.is_set())
            };
            if quit || shutting_down {
                return; // let eframe close → run_native returns
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.visible = false;
        }

        // Hidden: stay hidden, poll periodically for the open request.
        // Do NOT block the main/event-loop thread.
        // "Hidden" is implemented as minimized because Wayland cannot unmap a
        // toplevel window (Visible(false) is a no-op there and used to leave
        // an empty undrawn window on screen).
        //
        // Only send Minimized(true) on the transition from shown → hidden.
        // Repeatedly sending it every 200ms while hidden spams the Wayland
        // compositor with redundant xdg-toplevel events.
        if !self.visible {
            if self.shown {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.shown = false;
            }
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }

        // Visible: actually show and draw the UI. (No Minimized(false) here:
        // un-minimize does not exist on Wayland; restoration happens through
        // the Focus/activation command in the open-request path above.)
        //
        // Only send Visible(true) on the transition from hidden → visible,
        // avoiding redundant Wayland protocol messages every frame.
        if !self.shown {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.shown = true;
        }

        // Visible: edit a local copy so we don't fight the borrow checker, then
        // write changes back to the shared state after the frame.
        let s = self.state.lock().unwrap();
        let mut desired = s.desired.clone();
        let mut live_on = s.live_on;
        let mut restart = false;
        let mut quit = false;
        drop(s);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("vcam-proxy — Settings");
            ui.label(
                "Configure your virtual camera. Click “Save” to persist, “Apply & Restart” to take effect now.",
            );
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.collapsing("Source camera", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Camera index");
                        ui.text_edit_singleline(&mut self.camera_text);
                    });
                    ui.label(
                        "Use the “List cameras” button (CLI: vcam-proxy --list) to find the right index.",
                    );
                });

                ui.collapsing("Virtual device", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Device node");
                        ui.text_edit_singleline(&mut self.device_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Sink backend");
                        egui::ComboBox::from_label("")
                            .selected_text(format!("{:?}", desired.sink))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut desired.sink, SinkKind::Auto, "Auto");
                                ui.selectable_value(&mut desired.sink, SinkKind::V4l2, "V4l2");
                                ui.selectable_value(&mut desired.sink, SinkKind::Pipe, "Pipe");
                                ui.selectable_value(&mut desired.sink, SinkKind::Null, "Null");
                            });
                    });
                });

                ui.collapsing("Capture format", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Width");
                        ui.text_edit_singleline(&mut self.width_text);
                        ui.label("Height");
                        ui.text_edit_singleline(&mut self.height_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("FPS");
                        ui.text_edit_singleline(&mut self.fps_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Wire format");
                        egui::ComboBox::from_label("")
                            .selected_text(format!("{:?}", desired.format))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut desired.format, FormatPref::Auto, "Auto");
                                ui.selectable_value(&mut desired.format, FormatPref::Yuy2, "Yuy2");
                                ui.selectable_value(&mut desired.format, FormatPref::Rgb24, "Rgb24");
                                ui.selectable_value(&mut desired.format, FormatPref::Nv12, "Nv12");
                                ui.selectable_value(&mut desired.format, FormatPref::Mjpeg, "Mjpeg");
                            });
                    });
                    ui.label(
                        "“Auto” always outputs YUYV — accepted by every app (Chrome, Firefox, Zoom, OBS).",
                    );
                });

                ui.collapsing("Performance", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Buffers");
                        ui.text_edit_singleline(&mut self.buffers_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Retry (ms)");
                        ui.text_edit_singleline(&mut self.retry_text);
                    });
                });

                ui.collapsing("Virtual cameras (manager)", |ui| {
                    ui.label(
                        "vcam-proxy can create several virtual cameras so multiple apps \
                         (OBS, a browser, Zoom, …) each get their own device. Changing the \
                         number reloads the v4l2loopback kernel module — which only succeeds \
                         while no app is using the camera. vcam-proxy auto-retries until the \
                         apps close.",
                    );
                    ui.separator();

                    // Live view of the current virtual cameras and who uses them.
                    let cameras = all_loopback_users();
                    if cameras.is_empty() {
                        ui.label("No virtual camera loaded yet (created on start).");
                    } else {
                        ui.label("Current virtual cameras:");
                        for (dev, users) in &cameras {
                            ui.horizontal(|ui| {
                                ui.label(format!("• {}  [{}]", dev.path.display(), dev.card));
                                if users.is_empty() {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 200, 120),
                                        "free",
                                    );
                                } else {
                                    let names: Vec<String> = users
                                        .iter()
                                        .map(|u| format!("{} (pid {})", u.comm, u.pid))
                                        .collect();
                                    ui.colored_label(
                                        egui::Color32::from_rgb(230, 120, 120),
                                        format!("in use: {}", names.join(", ")),
                                    );
                                }
                            });
                        }
                    }
                    ui.separator();

                    ui.checkbox(
                        &mut desired.multi_reader,
                        "Multi-app mode (one virtual camera per app)",
                    );
                    ui.horizontal(|ui| {
                        ui.label("Number of virtual cameras (1-8)");
                        ui.text_edit_singleline(&mut self.devices_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Reload wait when busy (seconds, 0=off)");
                        ui.text_edit_singleline(&mut self.multi_app_timeout_text);
                    });
                    if ui.button("Apply & reload virtual cameras").clicked() {
                        match self.commit(&mut desired) {
                            Ok(()) => {
                                let _ = desired.clone().to_settings().save();
                                restart = true;
                                self.status.clear();
                                self.visible = false;
                            }
                            Err(e) => self.status = e,
                        }
                    }
                    ui.label(
                        "Raise the count above the current number to add cameras. The reload \
                         runs automatically; close any app shown as ‘in use’ above if it blocks.",
                    );
                });

                ui.collapsing("Module / advanced", |ui| {
                    ui.checkbox(
                        &mut desired.auto_load_module,
                        "Auto-load v4l2loopback module when missing (pkexec)",
                    );
                    ui.checkbox(
                        &mut desired.auto_resolution,
                        "Auto-resolution (highest camera mode)",
                    );
                    ui.horizontal(|ui| {
                        ui.label("exclusive_caps (0/1)");
                        ui.text_edit_singleline(&mut self.exclusive_caps_text);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Timeout (ms, 0=keep last frame)");
                        ui.text_edit_singleline(&mut self.timeout_text);
                    });
                    ui.label(
                        "exclusive_caps=1 is required for Chrome/Firefox/Zoom to list the \
                         device as a camera.",
                    );
                });
            });

            ui.separator();

            // Live on/off toggle (no restart needed).
            ui.horizontal(|ui| {
                ui.label("Virtual camera:");
                if ui
                    .selectable_label(live_on, if live_on { "ON" } else { "OFF" })
                    .clicked()
                {
                    live_on = true;
                }
                if ui
                    .selectable_label(!live_on, if live_on { "OFF" } else { "ON" })
                    .clicked()
                {
                    live_on = false;
                }
            });

            if !self.status.is_empty() {
                ui.label(egui::RichText::new(&self.status).color(egui::Color32::RED));
            }

            ui.horizontal(|ui| {
                if ui.button("Save to config").clicked() {
                    match self.commit(&mut desired) {
                        Ok(()) => {
                            if let Err(e) = desired.clone().to_settings().save() {
                                self.status = format!("Save failed: {e}");
                            } else {
                                self.status.clear();
                            }
                        }
                        Err(e) => self.status = e,
                    }
                }
                if ui.button("Apply & Restart").clicked() {
                    match self.commit(&mut desired) {
                        Ok(()) => {
                            let _ = desired.clone().to_settings().save();
                            restart = true;
                            self.status.clear();
                            // Hides to the taskbar (minimized) via the hidden
                            // branch on the next frame.
                            self.visible = false;
                        }
                        Err(e) => self.status = e,
                    }
                }
                if ui.button("Hide").clicked() {
                    self.visible = false;
                }
                if ui.button("Quit").clicked() {
                    // The exit path at the top of `update` closes the window.
                    quit = true;
                }
            });
        });

        // Write the edited local state back into the shared GuiState.
        let mut s = self.state.lock().unwrap();
        s.desired = desired;
        s.live_on = live_on;
        s.restart = restart;
        s.quit = quit;
    }
}

/// Convenience: convert a `Settings` into a `ResolvedConfig` for seeding.
pub fn settings_to_resolved(s: &Settings) -> ResolvedConfig {
    ResolvedConfig::from_settings(s)
}
