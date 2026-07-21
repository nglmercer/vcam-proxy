//! vcam-proxy: physical camera → virtual loopback.
//!
//! Configure via `~/.config/vcam-proxy/config.toml` or the Settings GUI.
//! Run with no arguments — features are on by default.

use std::sync::{Arc, Mutex};

use clap::Parser;
use tracing::{info, warn};
use vcam_proxy::cli_cmds;
use vcam_proxy::config::{Config, ResolvedConfig};
use vcam_proxy::controller;
use vcam_proxy::logging;
use vcam_proxy::messages;
use vcam_proxy::settings;
use vcam_proxy::shutdown::Shutdown;
use vcam_proxy::tray;
use vcam_proxy::ui;

fn main() {
    logging::init();
    let shutdown = Shutdown::install();
    let cli = Config::parse();

    let config_path = settings::Settings::config_path();
    let first_run = !config_path.exists();
    if first_run {
        match settings::Settings::create_default_file() {
            Ok(path) => info!(path = %path.display(), "{}", messages::LOG_CREATED_DEFAULT_CONFIG),
            Err(e) => warn!(error = %e, "{}", messages::LOG_CONFIG_CREATE_FAILED),
        }
    }

    let settings = settings::Settings::load();

    if dispatch_one_shots(&cli, &settings) {
        return;
    }

    let gui_enabled = !cli.no_gui;
    let initial_cfg = Arc::new(ResolvedConfig::from_settings(&settings).sanitized());
    let sink_switch = tray::SinkSwitch::new(true);

    let gui_state: Option<Arc<Mutex<ui::GuiState>>> = if gui_enabled {
        Some(ui::GuiState::new(
            (*initial_cfg).clone(),
            first_run || cli.settings,
        ))
    } else {
        None
    };
    let gui_wake = gui_state.as_ref().map(|s| ui::GuiWake::new(s.clone()));

    if first_run && gui_enabled {
        info!("{}", messages::LOG_FIRST_RUN_SETTINGS);
    }

    let controller = {
        let cli = cli.clone();
        let initial_cfg = initial_cfg.clone();
        let gui_state = gui_state.clone();
        let gui_wake = gui_wake.clone();
        let sink_switch = sink_switch.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("controller".into())
            .spawn(move || {
                controller::run(
                    &cli,
                    initial_cfg,
                    gui_state,
                    gui_wake,
                    sink_switch,
                    shutdown,
                )
            })
            .expect("failed to spawn controller thread")
    };

    let start_visible = first_run || cli.settings;
    match gui_state {
        Some(state) => {
            ui::run(state, shutdown.clone(), start_visible);
            shutdown.request();
            let _ = controller.join();
        }
        None => {
            let _ = controller.join();
        }
    }

    info!("{}", messages::LOG_THREADS_STOPPED);
}

/// Handle CLI utilities that exit immediately. Returns `true` if handled.
fn dispatch_one_shots(cli: &Config, settings: &settings::Settings) -> bool {
    if cli.edit_config {
        let path = settings::Settings::config_path();
        if let Err(e) = settings::Settings::create_default_file() {
            eprintln!("Failed to create config file: {e}");
            return true;
        }
        println!("Opening config file: {}", path.display());
        let _ = open::that(&path);
        return true;
    }

    if cli.show_config {
        let resolved = ResolvedConfig::from_settings(settings);
        cli_cmds::print_settings_table(&resolved, settings);
        return true;
    }

    if cli.save_config {
        let resolved = ResolvedConfig::from_settings(settings);
        match resolved.to_settings().save() {
            Ok(()) => println!(
                "Settings saved to: {}",
                settings::Settings::config_path().display()
            ),
            Err(e) => eprintln!("Failed to save settings: {e}"),
        }
        return true;
    }

    if cli.list {
        vcam_proxy::capture::list_cameras();
        return true;
    }

    if cli.list_loopback {
        cli_cmds::list_loopback_devices();
        return true;
    }

    if cli.setup {
        let cfg = Arc::new(ResolvedConfig::from_settings(settings).sanitized());
        cli_cmds::run_setup(cfg);
        return true;
    }

    false
}
