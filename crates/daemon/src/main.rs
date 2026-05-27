//! whispy dictation daemon.
//!
//! Holds the whisper-server child resident, captures audio on demand, runs the
//! hallucination filter, and injects the transcript. Clients drive it over a Unix
//! socket; the Quickshell pill UI reads `state.json`.

// TODO(step-5): drop once inject is wired in (only `inject` remains a stub).
#![allow(dead_code)]

mod app;
mod audio;
mod config;
mod filter;
mod inject;
mod server;
mod setup;
mod state;
mod stt;
mod whisper;

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
use crate::filter::Hallucinations;
use crate::state::{StatePublisher, Status, now};
use crate::stt::SttClient;
use crate::whisper::WhisperServer;

/// How long to wait for whisper-server to load the model and start listening.
const WHISPER_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
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

    // `setup` is an interactive bootstrap; it prints to the terminal and must not
    // initialise the JSON file logger or start the daemon.
    if let Some(Command::Setup(setup_args)) = args.command {
        return setup::run(&cfg, setup_args);
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
    info!(model = %cfg.stt.model, "starting whispy-daemon");

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
    let status = Status::new(shared, publisher);

    // Bring up whisper-server (model resident) before announcing idle.
    let whisper_log = config::state_dir().join("whisper-server.log");
    if let Some(parent) = whisper_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut whisper = WhisperServer::spawn(&cfg.stt, &whisper_log)?;
    if !whisper.wait_ready(WHISPER_READY_TIMEOUT, &shutdown) {
        whisper.shutdown();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "whisper-server did not become ready",
        ));
    }
    info!("whisper-server ready");

    let stt = SttClient::new(&cfg.stt);
    let blacklist = load_blacklist(&cfg);
    let socket_path = cfg.ipc.socket_path();

    status.idle();
    let app = Arc::new(App::new(cfg, status, stt, blacklist));

    // Serve until SIGTERM/SIGINT.
    let result = server::serve(&socket_path, app, Arc::clone(&shutdown));

    info!("shutting down");
    whisper.shutdown();
    result
}

/// Load the hallucination blacklist from the configured path, falling back to the
/// list baked into the binary if the file is missing or unreadable.
fn load_blacklist(cfg: &Config) -> Hallucinations {
    let path = cfg.filter.hallucinations_file();
    if path.exists() {
        match Hallucinations::load(&path) {
            Ok(h) => return h,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "failed to load hallucinations file; using built-in list")
            }
        }
    }

    #[derive(serde::Deserialize)]
    struct Embedded {
        #[serde(default)]
        phrases: Vec<String>,
    }
    let embedded: Embedded = toml::from_str(config::default_hallucinations_toml())
        .expect("embedded hallucinations parse");
    Hallucinations::from_phrases(embedded.phrases)
}

/// Initialise JSON-lines logging to `$XDG_STATE_HOME/whispy/daemon.log`.
/// The returned guard flushes the non-blocking writer; keep it alive for the run.
fn init_logging() -> std::io::Result<WorkerGuard> {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir)?;
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
