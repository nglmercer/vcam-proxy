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

use crate::shutdown::Shutdown;

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
}

/// One tray instance: owns the menu, the icons, and the switch + shutdown refs.
struct VcamTray {
    sink_switch: SinkSwitch,
    shutdown: Shutdown,
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
        if self.sink_switch.is_on() {
            "vcam-proxy — Virtual Camera ON"
        } else {
            "vcam-proxy — Virtual Camera OFF"
        }
        .into()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
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
pub fn spawn(sink_switch: SinkSwitch, shutdown: Shutdown) -> Option<thread::JoinHandle<()>> {
    let handle = thread::Builder::new().name("tray".into()).spawn(move || {
        let tray = VcamTray {
            sink_switch,
            shutdown,
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
