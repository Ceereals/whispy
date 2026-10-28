//! whispy dictation daemon.
//!
//! Holds the STT server child resident, captures audio on demand, runs the
//! hallucination filter, and injects the transcript. Clients drive it over a Unix
//! socket; the Quickshell pill UI reads `state.json`.

mod app;
mod audio;
mod config;
mod display;
mod engine;
mod filter;
mod inject;
mod llm;
mod pipeline;
mod proc;
mod server;
mod settings;
mod setup;
mod state;
mod stats;
mod stt;
mod stt_server;
mod vocab;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use whispy_common::{State, StateSnapshot};

use crate::app::App;
use crate::config::Config;
use crate::display::{Backend, DisplayServer};
use crate::engine::VocabStrategy;
use crate::filter::Hallucinations;
use crate::state::{StatePublisher, Status, now};
use crate::stt::SttClient;
use crate::stt_server::SttServer;

/// How long to wait for the STT server to load the model and start listening.
const STT_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[expect(
    clippy::doc_markdown,
    reason = "doc comments double as plain-text --help, so no backticks"
)]
#[command(
    name = "whispy-daemon",
    version,
    about = "Push-to-talk dictation daemon for Hyprland"
)]
struct Args {
    /// TOML config file (default: $XDG_CONFIG_HOME/whispy/config.toml, else built-in defaults).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Bootstrap whisper.cpp, the model, ydotool, config and the systemd unit.
    Setup(setup::SetupArgs),
    /// Summarize the transcript log (accepted vs dropped) to help tune the filter.
    Stats,
    /// Open the settings screen (microphone, language, typing, auto-stop, history).
    Settings,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let cfg = match Config::load(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("whispy-daemon: failed to load config: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Subcommands are terminal utilities: they print to the terminal and must not
    // initialise the JSON file logger or start the daemon.
    match args.command {
        Some(Command::Setup(setup_args)) => return setup::run(&cfg, setup_args),
        Some(Command::Stats) => return stats::run(),
        Some(Command::Settings) => return settings::run(&cfg, args.config.as_deref()),
        None => {}
    }

    // Refuse to start with a config that would only fail at first dictation.
    if let Err(e) = cfg.validate() {
        eprintln!("whispy-daemon: invalid configuration: {e}");
        return ExitCode::FAILURE;
    }

    let _guard = match init_logging() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("whispy-daemon: failed to init logging: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "fatal");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: Config) -> std::io::Result<()> {
    info!(engine = %cfg.stt.engine, model = %cfg.stt.model_file().display(), "starting whispy-daemon");

    // Bail before touching the STT server if a daemon is already live: a second
    // instance would otherwise reuse its server, steal its socket, and delete it on exit.
    server::ensure_not_running(&cfg.ipc.socket_path())?;

    // Decide which display server we're driving (Wayland vs X11) before touching
    // any injection tool. `auto` reads the session env; an explicit override forces
    // it. If nothing is detectable, default to Wayland (the historical behavior).
    let server = display::resolve_from_env(Backend::parse(&cfg.injection.backend)).unwrap_or_else(
        || {
            warn!("could not detect display server (no WAYLAND_DISPLAY/DISPLAY); defaulting to Wayland");
            DisplayServer::Wayland
        },
    );
    info!(display_server = ?server, "resolved display server");

    // Child tools (`wtype`/`xdotool`, `wl-*`/`xclip`) need WAYLAND_DISPLAY or DISPLAY;
    // a systemd user service started before the compositor imports the graphical env
    // may have neither, so discover/seed it here.
    ensure_display_env(server);

    // Surface missing injection/capture tools up front (warn-only: injection errors
    // already guide the user, but this points at the fix before the first dictation).
    warn_missing_tools(&cfg, server);

    // Install SIGTERM/SIGINT handlers that flip a flag; the accept loop polls it.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    let publisher = Arc::new(StatePublisher::new(cfg.ipc.state_path()));
    let shared = Arc::new(Mutex::new(StateSnapshot {
        state: State::Idle,
        rms: 0.0,
        error_kind: None,
        error_message: None,
        timestamp: now(),
    }));
    let status = Status::new(shared, publisher, cfg.ipc.state_max_hz);

    // Bring up the STT server (model resident) before announcing idle.
    let server_log = config::state_dir().join("whisper-server.log");
    if let Some(parent) = server_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Rotate only here, at daemon start: in-process restarts append, so the output
    // of a crash survives the monitor respawning the server.
    if let Err(e) = config::rotate_if_larger(&server_log, config::WHISPER_LOG_MAX_BYTES) {
        warn!(error = %e, "could not rotate whisper-server.log");
    }
    let stt_server = Arc::new(Mutex::new(SttServer::spawn(&cfg.stt, &server_log)?));
    if !stt_server
        .lock()
        .expect("stt server lock")
        .wait_ready(STT_READY_TIMEOUT, &shutdown)
    {
        stt_server.lock().expect("stt server lock").shutdown();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{} did not become ready (see {} for details)",
                cfg.stt.engine.server_name(),
                server_log.display()
            ),
        ));
    }
    info!("{} ready", cfg.stt.engine.server_name());

    // Monitor: if the STT server dies after startup, respawn it. Polls often enough
    // that SIGTERM-driven shutdown stays responsive.
    let monitor = {
        let server_name = cfg.stt.engine.server_name();
        let stt_server = Arc::clone(&stt_server);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut w = stt_server.lock().expect("stt server lock");
                if !w.is_alive() {
                    match w.restart(STT_READY_TIMEOUT, &shutdown) {
                        Ok(true) => info!("{server_name} restarted"),
                        Ok(false) => warn!("{server_name} restart did not become ready"),
                        Err(e) => error!(error = %e, "failed to restart {server_name}"),
                    }
                }
            }
        })
    };

    // The engine decides how the custom vocabulary reaches it: a decoding prompt
    // (whisper) here, or a correction of the transcript in `pipeline` (Parakeet).
    let vocab_prompt = match cfg.stt.engine.vocabulary(&cfg.dictionary.vocabulary) {
        VocabStrategy::Prompt(prompt) => Some(prompt),
        VocabStrategy::Correct | VocabStrategy::Off => None,
    };
    let stt = SttClient::new(&cfg.stt, vocab_prompt);
    let blacklist = load_blacklist(&cfg);
    let socket_path = cfg.ipc.socket_path();

    status.idle();
    let app = Arc::new(App::new(cfg, status, stt, blacklist, server));

    // Serve until SIGTERM/SIGINT.
    let result = server::serve(&socket_path, app, Arc::clone(&shutdown));

    info!("shutting down");
    monitor.join().ok();
    stt_server.lock().expect("stt server lock").shutdown();
    result
}

/// Warn (don't fail) if the binaries the configured injection backend + mode need
/// are not on `PATH`. Shares the tool list with `whispy-daemon setup doctor` via
/// [`setup::required_tools`] so the two never drift.
///
/// On X11 the clipboard requirement is "xclip *or* xsel"; `required_tools` lists
/// `xclip`, so an installed `xsel` would still warn here. That's intentionally
/// conservative — the warning names the preferred tool and doctor explains the
/// fallback; injection itself uses whichever is present.
fn warn_missing_tools(cfg: &Config, server: DisplayServer) {
    for (tool, _note) in setup::required_tools(server, &cfg.injection.mode) {
        if tool == "xclip" && setup::have("xsel") {
            continue; // xsel is an accepted fallback for the X11 clipboard.
        }
        if !setup::have(tool) {
            warn!(
                tool,
                "required tool not found on PATH; dictation may fail (run `whispy-daemon setup doctor`)"
            );
        }
    }
}

/// Load the hallucination blacklist from the configured path, falling back to the
/// list baked into the binary if the file is missing or unreadable.
fn load_blacklist(cfg: &Config) -> Hallucinations {
    let path = cfg.filter.hallucinations_file();
    if path.exists() {
        match Hallucinations::load(&path) {
            Ok(h) => return h,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "failed to load hallucinations file; using built-in list");
            }
        }
    }

    Hallucinations::parse(config::default_hallucinations_toml())
        .expect("embedded hallucinations parse")
}

/// Ensure the display-server env var the injection tools need is set, so a systemd
/// user service started before the compositor imported the graphical env can still
/// reach the session. Best-effort: a failure only warns, since `paste`/`type`
/// injection is what ultimately surfaces the error.
///
/// - Wayland: discover the lowest-numbered `wayland-N` socket under
///   `$XDG_RUNTIME_DIR` and set `WAYLAND_DISPLAY` if unset.
/// - X11: default `DISPLAY` to `:0` if unset (X11 has no per-socket scan; `:0` is
///   the near-universal default display).
fn ensure_display_env(server: DisplayServer) {
    match server {
        DisplayServer::Wayland => ensure_wayland_display(),
        DisplayServer::X11 => ensure_x_display(),
    }
}

fn ensure_wayland_display() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        warn!("WAYLAND_DISPLAY unset and XDG_RUNTIME_DIR missing; injection may fail");
        return;
    };
    let entries = match std::fs::read_dir(&runtime) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(error = %e, "WAYLAND_DISPLAY unset and $XDG_RUNTIME_DIR unreadable; injection may fail");
            return;
        }
    };
    let names = entries.filter_map(|e| e.ok()?.file_name().into_string().ok());
    if let Some(socket) = pick_wayland_socket(names) {
        // SAFETY: this runs early in startup. The only other live thread is the
        // log-appender worker, which never reads the environment; the whisper
        // monitor, state-flash threads, and socket server are all spawned later,
        // so nothing races this write.
        unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket) };
        info!(wayland_display = %socket, "discovered WAYLAND_DISPLAY from $XDG_RUNTIME_DIR");
    } else {
        warn!("WAYLAND_DISPLAY unset and no wayland-N socket found; injection may fail");
    }
}

fn ensure_x_display() {
    if std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
        return;
    }
    // SAFETY: same reason as the WAYLAND_DISPLAY write above: only the
    // log-appender worker is live, and it never reads the environment.
    unsafe { std::env::set_var("DISPLAY", ":0") };
    warn!("DISPLAY unset; defaulting to \":0\" (set DISPLAY if your X server uses another)");
}

/// Pick the lowest-numbered `wayland-<N>` socket from directory entry names.
/// Only a bare `wayland-` followed by digits qualifies, so lock files
/// (`wayland-1.lock`) and helper sockets (`wayland-1-foo.sock`) are ignored.
fn pick_wayland_socket(names: impl Iterator<Item = String>) -> Option<String> {
    names
        .filter_map(|name| {
            let n: u32 = name.strip_prefix("wayland-")?.parse().ok()?;
            Some((n, name))
        })
        .min_by_key(|(n, _)| *n)
        .map(|(_, name)| name)
}

/// Initialise JSON-lines logging to `$XDG_STATE_HOME/whispy/daemon.log`, rotating
/// it to `daemon.log.1` first if it has outgrown [`config::DAEMON_LOG_MAX_BYTES`].
/// The returned guard flushes the non-blocking writer; keep it alive for the run.
fn init_logging() -> std::io::Result<WorkerGuard> {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir)?;
    // Logging isn't up yet, so a failed rotation can only be reported on stderr.
    if let Err(e) = config::rotate_if_larger(&dir.join("daemon.log"), config::DAEMON_LOG_MAX_BYTES)
    {
        eprintln!("whispy-daemon: could not rotate daemon.log: {e}");
    }
    let appender = tracing_appender::rolling::never(&dir, "daemon.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(writer)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::pick_wayland_socket;

    fn pick(names: &[&str]) -> Option<String> {
        pick_wayland_socket(names.iter().map(ToString::to_string))
    }

    #[test]
    fn picks_lowest_numbered_socket_ignoring_locks_and_helpers() {
        let got = pick(&[
            "wayland-1",
            "wayland-1.lock",
            "wayland-1-awww-daemon.sock",
            "wayland-0",
        ]);
        assert_eq!(got.as_deref(), Some("wayland-0"));
    }

    #[test]
    fn returns_none_when_no_plain_socket() {
        assert_eq!(
            pick(&["wayland-1.lock", "wayland-0-foo.sock", "pipewire-0"]),
            None
        );
        assert_eq!(pick(&[]), None);
    }
}
