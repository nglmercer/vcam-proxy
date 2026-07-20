//! System-tray icon + on/off toggle for the virtual camera.
//!
//! Uses ksni (pure-Rust D-Bus StatusNotifierItem) so it works on GNOME Wayland
//! with no GTK or C dependencies. The tray shares an AtomicBool with the sink
//! loop: toggling "off" stops writes to the loopback device while capture keeps
//! running (in standby).
//!
//! Gracefully degrades: if no D-Bus session is available the thread logs and
//! exits without taking the pipeline down with it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use ksni::blocking::TrayMethods;
use ksni::menu::*;
use ksni::Tray;
use tracing::{info, warn};

use crate::pipeline::Stats;
use crate::shutdown::Shutdown;
use crate::ui::GuiWake;

/// Shared flag the sink loop reads and the tray flips.
#[derive(Clone)]
pub struct SinkSwitch {
    inner: Arc<AtomicBool>,
}

impl SinkSwitch {
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn is_on(&self) -> bool {
        self.inner.load(Ordering::Relaxed)
    }

    pub fn toggle(&self) -> bool {
        let prev = self.inner.fetch_xor(true, Ordering::Relaxed);
        !prev
    }

    /// Set the switch to a specific state (used by the GUI's live on/off).
    pub fn set(&self, on: bool) {
        self.inner.store(on, Ordering::Relaxed);
    }
}

/// Pipeline statistics view for tray display.
///
/// Wraps the *same* `Arc<Stats>` the capture/sink threads increment, so the
/// tray always reflects live counters instead of a disconnected copy.
#[derive(Clone)]
pub struct TrayStats {
    stats: Arc<Stats>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl TrayStats {
    pub fn new(stats: Arc<Stats>, width: u32, height: u32, fps: u32) -> Self {
        Self {
            stats,
            width,
            height,
            fps,
        }
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.stats.captured.load(Ordering::Relaxed),
            self.stats.written.load(Ordering::Relaxed),
            self.stats.dropped.load(Ordering::Relaxed),
        )
    }
}

/// One tray instance: owns the menu, the icons, and the switch + shutdown refs.
struct VcamTray {
    sink_switch: SinkSwitch,
    shutdown: Shutdown,
    stats: TrayStats,
    config_path: String,
    gui_wake: Option<Arc<GuiWake>>,
}

impl Tray for VcamTray {
    fn id(&self) -> String {
        "vcam-proxy".into()
    }

    fn icon_name(&self) -> String {
        if self.sink_switch.is_on() {
            "camera-web"
        } else {
            "camera-web-disabled"
        }
        .into()
    }

    fn title(&self) -> String {
        let status = if self.sink_switch.is_on() {
            "ON"
        } else {
            "OFF"
        };
        let (captured, written, dropped) = self.stats.snapshot();
        format!(
            "vcam-proxy — Camera {status}\n{}×{} @ {}fps\nCaptured: {captured} | Written: {written} | Dropped: {dropped}",
            self.stats.width, self.stats.height, self.stats.fps
        )
        .into()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let (captured, written, dropped) = self.stats.snapshot();
        let status_text = if self.sink_switch.is_on() {
            format!(
                "Status: ON ({}×{} @ {}fps)",
                self.stats.width, self.stats.height, self.stats.fps
            )
        } else {
            format!(
                "Status: OFF ({}×{} @ {}fps)",
                self.stats.width, self.stats.height, self.stats.fps
            )
        };

        vec![
            // Status information (non-clickable)
            StandardItem {
                label: status_text.into(),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Captured: {captured} frames").into(),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Written: {written} frames").into(),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Dropped: {dropped} frames").into(),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // Toggle virtual camera
            StandardItem {
                label: if self.sink_switch.is_on() {
                    "Turn Virtual Camera OFF"
                } else {
                    "Turn Virtual Camera ON"
                }
                .into(),
                icon_name: if self.sink_switch.is_on() {
                    "media-playback-stop"
                } else {
                    "media-playback-start"
                }
                .into(),
                activate: Box::new(|tray: &mut Self| {
                    let now_on = tray.sink_switch.toggle();
                    info!(enabled = now_on, "virtual camera toggled from tray");
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // In-app settings window (the primary configuration surface).
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Some(wake) = &tray.gui_wake {
                        info!("opening settings window from tray");
                        wake.open();
                    }
                }),
                ..Default::default()
            }
            .into(),
            // Config file shortcut
            StandardItem {
                label: "Open Config File".into(),
                icon_name: "document-properties".into(),
                activate: Box::new(|tray: &mut Self| {
                    info!("opening config file from tray");
                    let _ = open::that(&tray.config_path);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // Quit
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    info!("quit requested from tray");
                    tray.shutdown.request();
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray on its own thread using ksni blocking API.
/// Never panics into a pipeline thread — all errors are logged.
///
/// When `gui_wake` is provided, a "Settings…" menu item pops the in-app GUI
/// window open. Pass `None` to omit it (headless / `--no-gui` mode).
pub fn spawn_with_settings(
    sink_switch: SinkSwitch,
    shutdown: Shutdown,
    stats: TrayStats,
    gui_wake: Option<Arc<GuiWake>>,
) -> Option<thread::JoinHandle<()>> {
    let config_path = crate::settings::Settings::config_path()
        .display()
        .to_string();

    let handle = thread::Builder::new().name("tray".into()).spawn(move || {
        let tray = VcamTray {
            sink_switch,
            shutdown,
            stats,
            config_path,
            gui_wake,
        };
        match tray.spawn() {
            Ok(_handle) => {
                info!("tray icon active");
                // Park forever; the tray service runs its own D-Bus event loop.
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "could not start tray (no D-Bus session?); continuing without tray"
                );
            }
        }
    });

    match handle {
        Ok(h) => Some(h),
        Err(e) => {
            warn!(
                error = %e,
                "could not spawn tray thread; continuing without tray"
            );
            None
        }
    }
}
