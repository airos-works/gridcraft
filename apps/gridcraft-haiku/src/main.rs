//! GridCraft for air/OS (and Haiku).
//!
//! The same interface as the desktop app (`gridcraft-ui-egui`), hosted natively: a
//! `BApplication` and `BWindow` from the Be API (`native/host.cpp`) take the input and show the
//! frames, which egui's output becomes through a software rasteriser (`raster`), with the
//! system's file panels, clipboard, cursors, file types and locale. eframe, winit and wgpu don't
//! run on Haiku; nothing below the interface changes.
//!
//! Usage: `GridCraft [--sample NAME] [--control PORT [--control-token TOKEN]] [FILE…]`, as the
//! desktop app.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Elsewhere only the platform-neutral parts build (and are tested).
#![cfg_attr(not(target_os = "haiku"), allow(dead_code))]

mod keys;
mod print;
mod raster;

/// The desktop app's control server and log file, as they are (`apps/gridcraft/src`). An inline
/// module with a folder path, so their own submodules (`logging/tests.rs`) resolve as there.
#[cfg(target_os = "haiku")]
#[path = "../../gridcraft/src"]
mod desktop {
    pub mod control_server;
    pub mod logging;
}
#[cfg(target_os = "haiku")]
use desktop::{control_server, logging};
#[cfg(target_os = "haiku")]
mod host;
#[cfg(target_os = "haiku")]
mod services;
#[cfg(target_os = "haiku")]
mod shell;

#[cfg(not(target_os = "haiku"))]
fn main() {
    let _ = std::io::Write::write_all(&mut std::io::stderr(), b"GridCraft for air/OS runs on Haiku; elsewhere run `gridcraft`.\n");
    std::process::exit(1);
}

#[cfg(target_os = "haiku")]
use haiku::*;

#[cfg(target_os = "haiku")]
fn main() {
    haiku::main()
}

#[cfg(target_os = "haiku")]
mod haiku {
    use std::path::PathBuf;
    use std::rc::Rc;

    use gridcraft_engine::Session;
    use gridcraft_ui_egui::SheetApp;
    use serde_json::json;

    use crate::{control_server, host, logging, services, shell};

    const USAGE: &str = "\
GridCraft — the GridCraft spreadsheet for air/OS

USAGE:
  GridCraft [--sample NAME] [--control PORT [--control-token TOKEN]] [FILE…]

OPTIONS:
  --sample NAME       open a sample workbook (sales by default)
  --control PORT      start the localhost control server on PORT, for agents and scripts
                      (or set GRIDCRAFT_CONTROL_PORT); see docs/control-protocol.md
  --control-token T   the token clients must send first (or GRIDCRAFT_CONTROL_TOKEN); without
                      one a random token is written to ~/.cache/gridcraft
  --version           print the version
  -h, --help          print this help
";

    /// The default window: content size in points, and the smallest (the desktop app's).
    const DEFAULT_SIZE: [f32; 2] = [1440.0, 900.0];
    const MIN_SIZE: [f32; 2] = [640.0, 420.0];

    /// The settings folder: `~/config/settings/GridCraft`, beside other applications'. A copy
    /// started without `HOME` (by a server, say) uses Haiku's one home folder.
    pub fn config_dir() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map_or_else(|| PathBuf::from("/boot/home"), PathBuf::from);
        Some(home.join("config/settings/GridCraft"))
    }

    /// Runs without preferences (`GRIDCRAFT_NO_PREFS`, agents' test runs) neither read nor
    /// write them, and keep no log file.
    fn prefs_enabled() -> bool {
        std::env::var_os("GRIDCRAFT_NO_PREFS").is_none()
    }

    fn log_dir() -> Option<PathBuf> {
        Some(config_dir()?.join("logs"))
    }

    /// As the desktop app: `ui.json` (the interface) and `prefs.json` (the engine's options).
    fn load_prefs(app: &mut SheetApp) {
        if !prefs_enabled() {
            return;
        }
        // First run (or a ui.json without a usable language): follow the system's language and
        // its light or dark appearance, until they are chosen and saved in ui.json.
        app.ui.language = gridcraft_ui_egui::i18n::Language::system();
        app.ui.system_theme = true;
        let Some(dir) = config_dir() else { return };
        if let Ok(b) = std::fs::read(dir.join("ui.json"))
            && let Ok(ui) = serde_json::from_slice::<gridcraft_ui_egui::UiState>(&b)
        {
            let saved = serde_json::from_slice(&b).ok().and_then(|v| gridcraft_ui_egui::i18n::saved_language(&v));
            let language = saved.unwrap_or(app.ui.language);
            app.ui = ui.at_startup();
            app.ui.language = language;
        }
        if let Ok(b) = std::fs::read(dir.join("prefs.json"))
            && let Ok(p) = serde_json::from_slice(&b)
        {
            app.session.prefs = p;
        }
    }

    pub fn save_prefs(app: &SheetApp) {
        if !prefs_enabled() {
            return;
        }
        let Some(dir) = config_dir() else { return };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("preferences: cannot create {}: {e}", dir.display());
            return;
        }
        for (name, bytes) in [("ui.json", serde_json::to_vec_pretty(&app.ui)), ("prefs.json", serde_json::to_vec_pretty(&app.session.prefs))] {
            let Ok(bytes) = bytes else { continue };
            // Through a temporary file, so a failed write keeps the old preferences.
            let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
            if let Err(e) = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, dir.join(name))) {
                let _ = std::fs::remove_file(&tmp);
                log::warn!("preferences: cannot save {name}: {e}");
            }
        }
    }

    fn build_sha() -> &'static str {
        option_env!("GRIDCRAFT_BUILD_SHA").map(|s| s.get(..8).unwrap_or(s)).unwrap_or("dev")
    }

    struct Args {
        sample: Option<String>,
        control: Option<u16>,
        control_token: Option<String>,
        files: Vec<String>,
    }

    fn args() -> Args {
        let mut a = Args {
            sample: None,
            control: std::env::var("GRIDCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok()),
            control_token: std::env::var("GRIDCRAFT_CONTROL_TOKEN").ok(),
            files: Vec::new(),
        };
        let mut it = std::env::args_os().skip(1).map(|s| s.to_string_lossy().into_owned()).peekable();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--sample" => a.sample = Some(it.next_if(|n| !n.starts_with('-')).unwrap_or_else(|| "sales".into())),
                "--control" => match it.next().and_then(|p| p.parse::<std::num::NonZeroU16>().ok()) {
                    Some(p) => a.control = Some(p.get()),
                    None => {
                        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"GridCraft: --control needs a port number from 1 to 65535\n");
                        std::process::exit(2);
                    }
                },
                "--control-token" => a.control_token = it.next(),
                "--version" | "-V" => {
                    let _ = std::io::Write::write_fmt(
                        &mut std::io::stdout(),
                        format_args!("GridCraft {} ({}) for air/OS\n", env!("CARGO_PKG_VERSION"), build_sha()),
                    );
                    std::process::exit(0);
                }
                "--help" | "-h" => {
                    let _ = std::io::Write::write_all(&mut std::io::stdout(), USAGE.as_bytes());
                    std::process::exit(0);
                }
                _ => a.files.push(arg),
            }
        }
        a
    }

    /// The interface thread: builds the app and runs the window's event loop.
    fn interface(args: Args, scale: f32) {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(true);
        let mut session = Session::new();
        if let Some(s) = &args.sample
            && let Err(e) = session.execute("file.new", json!({"sample": s}))
        {
            log::warn!("sample {s}: {e}");
        }
        if session.documents().is_empty() {
            session.new_workbook();
        }
        let dialogs = Rc::new(services::Dialogs::default());
        let mut app = SheetApp::new(session, services::services(dialogs.clone()));
        app.after_engine(); // Show warnings from the sample.
        load_prefs(&mut app);
        SheetApp::setup_context_for_language(&ctx, app.ui.dark, app.ui.language);
        if let Some(port) = args.control {
            match control_server::start(port, args.control_token.clone(), ctx.clone()) {
                Ok(rx) => app.control_rx = Some(rx),
                Err(e) => log::error!("control channel on port {port}: {e}"),
            }
        }
        // Files named on the command line open as File › Open opens them; Tracker's arrive as
        // messages (`shell`). Never the program itself, which a launcher may name.
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
        for f in &args.files {
            if exe.is_some() && std::fs::canonicalize(f).ok() == exe {
                log::warn!("not opening {f}: it is GridCraft itself");
                continue;
            }
            app.open_path(f);
        }
        let mut shell = shell::Shell::new(app, ctx, dialogs, scale);
        shell.run();
        shell.finish();
    }

    pub fn main() {
        // Before any thread starts: the locale variables come from Haiku's Locale preferences.
        host::export_locale();
        let logger = logging::install();
        std::panic::set_hook(Box::new(|info| {
            let trace = std::backtrace::Backtrace::capture();
            let report = if trace.status() == std::backtrace::BacktraceStatus::Captured {
                format!("internal error: {info}\n{trace}")
            } else {
                format!("internal error: {info}")
            };
            if log::log_enabled!(log::Level::Error) {
                log::error!("{report}");
            } else {
                let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("GridCraft: {report}\n"));
            }
        }));
        let args = args();
        if let Some(logger) = logger {
            match log_dir().filter(|_| prefs_enabled()) {
                Some(dir) => match logger.attach_dir(&dir) {
                    Ok(path) => log::info!("GridCraft {} ({}) for air/OS, log file {}", env!("CARGO_PKG_VERSION"), build_sha(), path.display()),
                    Err(e) => log::warn!("no log file: {e}"),
                },
                None => logger.no_file(),
            }
        }
        if let Err(e) = host::init(services::SIGNATURE) {
            log::error!("{e}");
            std::process::exit(1);
        }
        if prefs_enabled() {
            services::claim_document_types();
        }
        // The rasteriser's workers: up to 8 (more cost more in waking than they save on a
        // frame), named for the system's process lists.
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 8));
        if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(workers).thread_name(|i| format!("GridCraft raster {i}")).build_global() {
            log::warn!("rasteriser threads: {e}");
        }
        let scale = host::ui_scale();
        let px = |v: f32| v * scale;
        host::open_window(&host::WindowSpec {
            title: "GridCraft".into(),
            position: None,
            size: DEFAULT_SIZE.map(px),
            min_size: MIN_SIZE.map(px),
            zoomed: false,
        });
        let ui = std::thread::Builder::new().name("GridCraft interface".into()).stack_size(16 << 20).spawn(move || {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| interface(args, scale)));
            if ran.is_err() {
                log::error!("the interface stopped after an internal error");
                host::alert("GridCraft", "GridCraft ran into an internal error and has to close. The log is in ~/config/settings/GridCraft/logs.");
            }
            host::quit();
        });
        match ui {
            Ok(handle) => {
                host::run();
                let _ = handle.join();
            }
            Err(e) => log::error!("couldn't start the interface: {e}"),
        }
    }
}
