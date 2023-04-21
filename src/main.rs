#![doc = include_str!("../README.md")]

use std::cell::RefCell;
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::process::exit;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "ipc")]
use std::sync::RwLock;
use std::sync::{mpsc, Arc, OnceLock};

use cfg_if::cfg_if;
#[cfg(feature = "cli")]
use clap::Parser;
use color_eyre::eyre::Result;
use color_eyre::Report;
use dirs::config_dir;
use glib::PropertySet;
use gtk::gdk::Display;
use gtk::prelude::*;
use gtk::Application;
use smithay_client_toolkit::output::OutputInfo;
use tokio::runtime::Runtime;
use tokio::task::{block_in_place, JoinHandle};
use tracing::{debug, error, info, warn};
use universal_config::ConfigLoader;

use crate::bar::{create_bar, Bar};
use crate::clients::wayland::OutputEventType;
use crate::clients::Clients;
use crate::config::{Config, MonitorConfig};
use crate::error::ExitCode;
#[cfg(feature = "ipc")]
use crate::ironvar::VariableManager;
use crate::style::load_css;

mod bar;
#[cfg(feature = "cli")]
mod cli;
mod clients;
mod config;
mod desktop_file;
mod dynamic_value;
mod error;
mod gtk_helpers;
mod image;
#[cfg(feature = "ipc")]
mod ipc;
#[cfg(feature = "ipc")]
mod ironvar;
mod logging;
mod macros;
mod modules;
mod popup;
mod script;
mod style;

const GTK_APP_ID: &str = "dev.jstanger.ironbar";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let _guard = logging::install_logging();

    cfg_if! {
        if #[cfg(feature = "cli")] {
            run_with_args();
        } else {
            start_ironbar();
        }
    }
}

#[cfg(feature = "cli")]
fn run_with_args() {
    let args = cli::Args::parse();

    match args.command {
        Some(command) => {
            let rt = create_runtime();
            rt.block_on(async move {
                let ipc = ipc::Ipc::new();
                match ipc.send(command).await {
                    Ok(res) => cli::handle_response(res),
                    Err(err) => error!("{err:?}"),
                };
            });
        }
        None => start_ironbar(),
    }
}

#[derive(Debug)]
pub struct Ironbar {
    bars: Rc<RefCell<Vec<Bar>>>,
    clients: Rc<RefCell<Clients>>,
    config: Rc<RefCell<Config>>,
}

impl Ironbar {
    fn new() -> Self {
        Self {
            bars: Rc::new(RefCell::new(vec![])),
            clients: Rc::new(RefCell::new(Clients::new())),
            config: Rc::new(RefCell::new(load_config())),
        }
    }

    fn start(self) {
        info!("Ironbar version {}", VERSION);
        info!("Starting application");

        let app = Application::builder().application_id(GTK_APP_ID).build();

        let running = AtomicBool::new(false);

        // cannot use `oneshot` as `connect_activate` is not `FnOnce`.
        let (activate_tx, activate_rx) = mpsc::channel();

        let instance = Rc::new(self);
        let instance2 = instance.clone();

        // force start wayland client ahead of ui
        let wl = instance.clients.borrow_mut().wayland();
        let mut rx_outputs = wl.subscribe_outputs();
        wl.roundtrip();

        app.connect_activate(move |app| {
            if running.load(Ordering::Relaxed) {
                info!("Ironbar already running, returning");
                return;
            }

            running.set(true);

            cfg_if! {
                if #[cfg(feature = "ipc")] {
                    let ipc = ipc::Ipc::new();
                    ipc.start(app, instance.clone());
                }
            }

            let style_path = env::var("IRONBAR_CSS").ok().map_or_else(
                || {
                    config_dir().map_or_else(
                        || {
                            let report = Report::msg("Failed to locate user config dir");
                            error!("{:?}", report);
                            exit(ExitCode::CreateBars as i32);
                        },
                        |dir| dir.join("ironbar").join("style.css"),
                    )
                },
                PathBuf::from,
            );

            if style_path.exists() {
                load_css(style_path);
            }

            let (tx, rx) = mpsc::channel();

            #[cfg(feature = "ipc")]
            let ipc_path = ipc.path().to_path_buf();
            spawn_blocking(move || {
                rx.recv().expect("to receive from channel");

                info!("Shutting down");

                #[cfg(feature = "ipc")]
                ipc::Ipc::shutdown(ipc_path);

                exit(0);
            });

            ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
                .expect("Error setting Ctrl-C handler");

            let hold = app.hold();
            send!(activate_tx, hold);
        });

        {
            let instance = instance2;
            let app = app.clone();

            glib::spawn_future_local(async move {
                let _hold = activate_rx.recv().expect("to receive activation signal");
                debug!("Received activation signal, initialising bars");

                while let Ok(event) = rx_outputs.recv().await {
                    match event.event_type {
                        OutputEventType::New => {
                            match load_output_bars(&instance, &app, event.output) {
                                Ok(mut new_bars) => {
                                    instance.bars.borrow_mut().append(&mut new_bars);
                                }
                                Err(err) => error!("{err:?}"),
                            }
                        }
                        OutputEventType::Destroyed => {
                            let Some(name) = event.output.name else {
                                continue;
                            };
                            instance
                                .bars
                                .borrow_mut()
                                .retain(|bar| bar.monitor_name() != name);
                        }
                        OutputEventType::Update => {}
                    }
                }
            });
        }

        // Ignore CLI args
        // Some are provided by swaybar_config but not currently supported
        app.run_with_args(&Vec::<&str>::new());
    }

    /// Gets the current Tokio runtime.
    #[must_use]
    pub fn runtime() -> Arc<Runtime> {
        static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
        RUNTIME.get_or_init(|| Arc::new(create_runtime())).clone()
    }

    /// Gets a `usize` ID value that is unique to the entire Ironbar instance.
    /// This is just a static `AtomicUsize` that increments every time this function is called.
    pub fn unique_id() -> usize {
        static COUNTER: AtomicUsize = AtomicUsize::new(1);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// Gets the `Ironvar` manager singleton.
    #[cfg(feature = "ipc")]
    #[must_use]
    pub fn variable_manager() -> Arc<RwLock<VariableManager>> {
        static VARIABLE_MANAGER: OnceLock<Arc<RwLock<VariableManager>>> = OnceLock::new();
        VARIABLE_MANAGER
            .get_or_init(|| arc_rw!(VariableManager::new()))
            .clone()
    }

    /// Gets a clone of a bar by its unique name.
    ///
    /// Since the bar contains mostly GTK objects,
    /// the clone is cheap enough to not worry about.
    #[must_use]
    pub fn bar_by_name(&self, name: &str) -> Option<Bar> {
        self.bars
            .borrow()
            .iter()
            .find(|&bar| bar.name() == name)
            .cloned()
    }

    /// Re-reads the config file from disk and replaces the active config.
    /// Note this does *not* reload bars, which must be performed separately.
    #[cfg(feature = "ipc")]
    fn reload_config(&self) {
        self.config.replace(load_config());
    }
}

fn start_ironbar() {
    let ironbar = Ironbar::new();
    ironbar.start();
}

/// Loads the config file from disk.
fn load_config() -> Config {
    let mut config = env::var("IRONBAR_CONFIG")
        .map_or_else(
            |_| ConfigLoader::new("ironbar").find_and_load(),
            ConfigLoader::load,
        )
        .unwrap_or_else(|err| {
            error!("Failed to load config: {}", err);
            warn!("Falling back to the default config");
            info!("If this is your first time using Ironbar, you should create a config in ~/.config/ironbar/");
            info!("More info here: https://github.com/JakeStanger/ironbar/wiki/configuration-guide");

            Config::default()
        });

    debug!("Loaded config file");

    #[cfg(feature = "ipc")]
    if let Some(ironvars) = config.ironvar_defaults.take() {
        let variable_manager = Ironbar::variable_manager();
        for (k, v) in ironvars {
            if write_lock!(variable_manager).set(k.clone(), v).is_err() {
                warn!("Ignoring invalid ironvar: '{k}'");
            }
        }
    }

    config
}

/// Gets the GDK `Display` instance.
pub fn get_display() -> Display {
    Display::default().map_or_else(
        || {
            let report = Report::msg("Failed to get default GTK display");
            error!("{:?}", report);
            exit(ExitCode::GtkDisplay as i32)
        },
        |display| display,
    )
}

/// Loads all the bars associated with an output.
fn load_output_bars(
    ironbar: &Rc<Ironbar>,
    app: &Application,
    output: OutputInfo,
) -> Result<Vec<Bar>> {
    let Some(monitor_name) = &output.name else {
        return Err(Report::msg("Output missing monitor name"));
    };

    let config = ironbar.config.borrow();
    let display = get_display();

    let pos = output.logical_position.unwrap_or_default();
    let monitor = display
        .monitor_at_point(pos.0, pos.1)
        .expect("monitor to exist");

    let show_default_bar =
        config.start.is_some() || config.center.is_some() || config.end.is_some();

    let bars = match config
        .monitors
        .as_ref()
        .and_then(|config| config.get(monitor_name))
    {
        Some(MonitorConfig::Single(config)) => {
            vec![create_bar(
                app,
                &monitor,
                monitor_name.to_string(),
                config.clone(),
                ironbar.clone(),
            )?]
        }
        Some(MonitorConfig::Multiple(configs)) => configs
            .iter()
            .map(|config| {
                create_bar(
                    app,
                    &monitor,
                    monitor_name.to_string(),
                    config.clone(),
                    ironbar.clone(),
                )
            })
            .collect::<Result<_>>()?,
        None if show_default_bar => vec![create_bar(
            app,
            &monitor,
            monitor_name.to_string(),
            config.clone(),
            ironbar.clone(),
        )?],
        None => vec![],
    };

    Ok(bars)
}

fn create_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio to create a valid runtime")
}

/// Calls `spawn` on the Tokio runtime.
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ironbar::runtime().spawn(f)
}

/// Calls `spawn_blocking` on the Tokio runtime.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Ironbar::runtime().spawn_blocking(f)
}

/// Blocks on a `Future` until it resolves.
///
/// This is not an `async` operation
/// so can be used outside of an async function.
///
/// Use sparingly, as this risks blocking the UI thread!
/// Prefer async functions wherever possible.
pub fn await_sync<F: Future>(f: F) -> F::Output {
    block_in_place(|| Ironbar::runtime().block_on(f))
}
