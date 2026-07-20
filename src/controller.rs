//! Pipeline controller: capture + sink + tray lifecycle with live restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use crate::capture;
use crate::cli_cmds;
use crate::config::{Config, FormatPref, ResolvedConfig};
use crate::frame::BufferPool;
use crate::image_source;
use crate::messages;
use crate::pipeline::{self, Stats};
use crate::shutdown::Shutdown;
use crate::sink;
use crate::tray;
use crate::ui;

/// Background thread: owns the capture/sink/tray pipeline and supports live
/// reconfiguration via GUI "Apply & Restart".
pub fn run(
    cli: &Config,
    initial_cfg: Arc<ResolvedConfig>,
    gui_state: Option<Arc<Mutex<ui::GuiState>>>,
    gui_wake: Option<Arc<ui::GuiWake>>,
    sink_switch: tray::SinkSwitch,
    shutdown: Shutdown,
) {
    let mut current_cfg = initial_cfg.clone();
    loop {
        if let Some(state) = &gui_state {
            let g = state.lock().unwrap();
            current_cfg = Arc::new(g.desired.clone().sanitized());
        }

        if let Some(state) = &gui_state {
            let live = state.lock().unwrap().live_on;
            sink_switch.set(live);
        }

        info!(
            camera = current_cfg.camera,
            width = current_cfg.width,
            height = current_cfg.height,
            fps = current_cfg.fps,
            format = ?current_cfg.format,
            buffers = current_cfg.buffers,
            multi_reader = current_cfg.multi_reader,
            exclusive_caps = current_cfg.exclusive_caps,
            auto_resolution = current_cfg.auto_resolution,
            auto_load_module = current_cfg.auto_load_module,
            "{}",
            messages::LOG_STARTING
        );

        if matches!(current_cfg.format, FormatPref::Rgb24 | FormatPref::Mjpeg) {
            warn!(format = ?current_cfg.format, "{}", messages::LOG_WIRE_FORMAT_WARN);
        }

        if cli.dry_run {
            info!("{}", messages::LOG_DRY_RUN);
            cli_cmds::run_dry_run(current_cfg.clone(), shutdown.clone());
            break;
        }

        let desired_devices = effective_devices(&current_cfg);
        if desired_devices > current_cfg.devices.clamp(1, 8) {
            info!(
                configured = current_cfg.devices,
                effective = desired_devices,
                "multi-reader: creating one virtual camera per app \
                 (v4l2loopback ≥ 0.14 allows only one reader per node)"
            );
        }
        let module_params = messages::module_params(
            current_cfg.exclusive_caps,
            desired_devices,
            current_cfg.timeout,
        );

        ensure_module(&current_cfg, &module_params);
        ensure_multi_node(desired_devices, &module_params);

        let loopback_path = match sink::find_loopback_device(Path::new(&current_cfg.device)) {
            Ok(path) => path,
            Err(e) => {
                match &e {
                    sink::LoopbackError::NoDeviceFound => messages::error_no_loopback_device(),
                    sink::LoopbackError::ScanFailed { source } => {
                        messages::error_scan_failed(source)
                    }
                }
                if wait_for_restart_or_quit(&gui_state, &shutdown) {
                    continue;
                }
                std::process::exit(1);
            }
        };

        if let Err(e) = sink::check_device_access(&loopback_path) {
            match &e {
                sink::AccessError::NotFound { path } => messages::error_device_not_found(path),
                sink::AccessError::PermissionDenied { suggestion, .. } => {
                    messages::error_permission(&e, suggestion)
                }
                sink::AccessError::Other { .. } => {
                    messages::error_device_access(&loopback_path, &e)
                }
            }
            if wait_for_restart_or_quit(&gui_state, &shutdown) {
                continue;
            }
            std::process::exit(1);
        }

        let stats = Arc::new(Stats::default());
        let tray_handle = spawn_tray(
            cli,
            &current_cfg,
            &stats,
            &sink_switch,
            &shutdown,
            &gui_wake,
        );

        let slot_bytes = current_cfg.width as usize * current_cfg.height as usize * 3;
        let pool = BufferPool::new(current_cfg.buffers, slot_bytes);
        let sink_impl = build_sink(
            &current_cfg,
            desired_devices,
            &loopback_path,
            &module_params,
        );

        let (tx, rx) = crossbeam_channel::bounded(current_cfg.buffers);
        let sink_handle = pipeline::spawn_sink(
            current_cfg.clone(),
            sink_impl,
            rx,
            pool.clone(),
            shutdown.clone(),
            stats.clone(),
            sink_switch.clone(),
        );
        let capture_handle = spawn_capture(&current_cfg, pool, tx, &shutdown, stats);

        let restart = block_until_done(&gui_state, &sink_switch, &shutdown);
        info!("{}", messages::LOG_SHUTDOWN_DRAIN);

        join_with_timeout("capture", capture_handle, Duration::from_secs(3));
        join_with_timeout("sink", sink_handle, Duration::from_secs(3));
        if let Some(h) = tray_handle {
            join_with_timeout("tray", h, Duration::from_secs(2));
        }

        if gui_state
            .as_ref()
            .map(|s| s.lock().unwrap().quit)
            .unwrap_or(false)
        {
            info!("{}", messages::LOG_QUIT_GUI);
            break;
        }

        if !restart {
            break;
        }

        info!("{}", messages::LOG_RESTART);
        std::thread::sleep(Duration::from_millis(200));
    }

    info!("{}", messages::LOG_THREADS_STOPPED);
}

/// Effective number of loopback nodes to create/feed.
///
/// v4l2loopback ≥ 0.14 grants the CAPTURE stream token to a **single** opener
/// per node: only ONE app can stream from a node at a time — every additional
/// app fails with EBUSY ("Device or resource busy"), which is exactly the
/// reported "OBS / browser can't access the virtual camera while another app
/// uses it" symptom. `max_openers` only caps open *fds*, not *streams*.
///
/// Multi-app support therefore means **one node per app**: when `multi_reader`
/// is on, always create at least 2 nodes ('vcam-proxy', 'vcam-proxy-2', …) so
/// OBS, a browser, Zoom, … can each be assigned their own virtual camera.
/// (On drivers ≤ 0.13 the extra node is unused-but-harmless: those releases
/// broadcast to any number of readers on a single node.)
fn effective_devices(cfg: &ResolvedConfig) -> u32 {
    let desired = cfg.devices.clamp(1, 8);
    if cfg.multi_reader && desired < 2 {
        2
    } else {
        desired
    }
}

fn ensure_module(cfg: &ResolvedConfig, module_params: &str) {
    if sink::is_module_loaded() {
        match sink::exclusive_caps_active() {
            Some(false) => messages::warn_exclusive_caps_zero(module_params),
            Some(true) => info!("{}", messages::LOG_EXCLUSIVE_CAPS_OK),
            None => {}
        }

        if cfg.multi_reader {
            // Report the driver's per-node reader model: on v4l2loopback
            // ≥ 0.14 a single node serves only ONE streaming app, which is
            // why multi-reader feeds one node per app (see effective_devices).
            match sink::capture_single_streamer() {
                Some(true) => info!(
                    version = ?sink::module_version(),
                    "v4l2loopback ≥ 0.14 allows only ONE reader per node; \
                     multi-reader feeds one virtual camera per app"
                ),
                Some(false) => info!(
                    version = ?sink::module_version(),
                    "v4l2loopback ≤ 0.13 supports multiple concurrent readers per node"
                ),
                None => {}
            }

            // Check max_openers for multi-reader support. If the module was
            // loaded by a previous run or the user with a low max_openers,
            // apps can't even OPEN the virtual camera concurrently.
            if let Some(max) = sink::max_openers() {
                if max < 2 {
                    warn!(
                        max_openers = max,
                        "v4l2loopback max_openers is {}, which blocks multiple apps from \
                         reading the virtual camera at once. Reload the module:\n  \
                         sudo modprobe -r v4l2loopback\n  sudo modprobe v4l2loopback {}",
                        max,
                        module_params,
                    );
                } else {
                    info!(max_openers = max, "max_openers sufficient for multi-reader");
                }
            }
        }
    }

    if !cfg.auto_load_module || !sink::is_module_loaded() {
        return;
    }

    info!("{}", messages::LOG_AUTO_LOAD_ATTEMPT);
    match sink::ensure_module_loaded_with_install(module_params) {
        Ok(()) => {
            std::thread::sleep(Duration::from_millis(500));
            info!("{}", messages::LOG_MODULE_LOADED);
        }
        Err(e) => {
            match &e {
                sink::ModuleError::PkexecNotAvailable => {
                    messages::note_pkexec_missing(module_params)
                }
                sink::ModuleError::DistroNotSupported => {
                    messages::note_distro_unsupported(module_params)
                }
                sink::ModuleError::InstallFailed(code) => messages::note_install_failed(*code),
                _ => messages::note_auto_load_failed(&e),
            }
            messages::note_manual_modprobe(module_params);
        }
    }
}

fn ensure_multi_node(desired_devices: u32, module_params: &str) {
    if desired_devices < 2 || !sink::is_module_loaded() {
        return;
    }
    let current_devices = sink::count_loopback_devices();
    if current_devices >= desired_devices as usize {
        return;
    }

    info!(
        current_devices,
        desired_devices,
        "{}",
        messages::LOG_MULTI_NODE_RELOAD
    );
    messages::note_multi_node_reload(desired_devices, current_devices);

    match sink::load_module_with_params_force(module_params) {
        Ok(()) => {
            let mut devices_ready = false;
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(200));
                if sink::count_loopback_devices() >= desired_devices as usize {
                    devices_ready = true;
                    break;
                }
            }
            if devices_ready {
                messages::note_multi_node_ready(desired_devices);
                info!(desired_devices, "{}", messages::LOG_MULTI_NODE_OK);
            } else {
                let found = sink::count_loopback_devices();
                warn!(found, "module reloaded but devices still missing");
                messages::warn_multi_node_partial(found, module_params);
            }
        }
        Err(e) => {
            warn!(error = %e, "{}", messages::LOG_MULTI_NODE_RELOAD_FAIL);
            messages::error_multi_node_reload(&e, module_params);
        }
    }
}

fn spawn_tray(
    cli: &Config,
    cfg: &ResolvedConfig,
    stats: &Arc<Stats>,
    sink_switch: &tray::SinkSwitch,
    shutdown: &Shutdown,
    gui_wake: &Option<Arc<ui::GuiWake>>,
) -> Option<std::thread::JoinHandle<()>> {
    if cli.no_tray {
        info!("{}", messages::LOG_TRAY_DISABLED);
        return None;
    }
    let tray_stats = tray::TrayStats::new(stats.clone(), cfg.width, cfg.height, cfg.fps);
    tray::spawn_with_settings(
        sink_switch.clone(),
        shutdown.clone(),
        tray_stats,
        gui_wake.clone(),
    )
}

fn build_sink(
    cfg: &ResolvedConfig,
    desired_devices: u32,
    loopback_path: &Path,
    module_params: &str,
) -> Box<dyn sink::Sink> {
    if desired_devices < 2 {
        if cfg.multi_reader {
            info!("{}", messages::LOG_MULTI_READER);
        }
        return sink::build_with_path(cfg, loopback_path);
    }

    let all_paths = sink::discover_loopback_devices().unwrap_or_default();
    let loopback_paths: Vec<PathBuf> = all_paths
        .into_iter()
        .filter(|d| sink::is_loopback_driver(&d.driver))
        .map(|d| d.path)
        .collect();

    if loopback_paths.len() >= 2 {
        let chosen: Vec<_> = loopback_paths
            .into_iter()
            .take(desired_devices as usize)
            .collect();
        info!(devices = chosen.len(), "{}", messages::LOG_MULTI_NODE_SINK);
        messages::note_multi_node_active(chosen.len(), &chosen);
        sink::build_multi_with_paths(chosen, cfg.timeout)
    } else {
        warn!("{}", messages::LOG_MULTI_NODE_FALLBACK);
        messages::warn_multi_node_fallback(desired_devices, module_params);
        sink::build_with_path(cfg, loopback_path)
    }
}

fn spawn_capture(
    cfg: &Arc<ResolvedConfig>,
    pool: BufferPool,
    tx: crossbeam_channel::Sender<crate::frame::Frame>,
    shutdown: &Shutdown,
    stats: Arc<Stats>,
) -> std::thread::JoinHandle<()> {
    if let Some(ref image_path) = cfg.image {
        image_source::spawn(
            cfg.clone(),
            PathBuf::from(image_path),
            pool,
            tx,
            shutdown.clone(),
            stats,
        )
    } else {
        capture::spawn(cfg.clone(), pool, tx, shutdown.clone(), stats)
    }
}

/// Block until shutdown / quit, or GUI requests restart. Returns `true` on restart.
pub fn block_until_done(
    gui_state: &Option<Arc<Mutex<ui::GuiState>>>,
    sink_switch: &tray::SinkSwitch,
    shutdown: &Shutdown,
) -> bool {
    loop {
        if let Some(state) = gui_state {
            let g = state.lock().unwrap();
            sink_switch.set(g.live_on);
            if g.restart {
                return true;
            }
            if g.quit {
                return false;
            }
        }
        if shutdown.is_set() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait for GUI restart/quit before the pipeline is spawned.
pub fn wait_for_restart_or_quit(
    gui_state: &Option<Arc<Mutex<ui::GuiState>>>,
    shutdown: &Shutdown,
) -> bool {
    loop {
        if let Some(state) = gui_state {
            let g = state.lock().unwrap();
            if g.restart {
                return true;
            }
            if g.quit {
                return false;
            }
        }
        if shutdown.is_set() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Join a thread with a timeout (logs if it overruns).
pub fn join_with_timeout(name: &str, handle: std::thread::JoinHandle<()>, timeout: Duration) {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::Builder::new()
        .name(format!("{name}-watcher"))
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        })
        .expect("failed to spawn watcher thread");

    if rx.recv_timeout(timeout).is_err() {
        warn!(
            thread = name,
            timeout_secs = timeout.as_secs(),
            "{}",
            messages::LOG_JOIN_TIMEOUT
        );
    }
    drop(watcher);
}
