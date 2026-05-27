//! whispy dictation daemon.
//!
//! Holds the whisper-server child resident, captures audio on demand, runs the
//! hallucination filter, and injects the transcript. Clients drive it over a Unix
//! socket; the Quickshell pill UI reads `state.json`.

// TODO(step-3+): drop once audio/stt/inject/filter are wired into the run loop.
#![allow(dead_code)]

mod audio;
mod config;
mod filter;
mod inject;
mod server;
mod state;
mod stt;
mod whisper;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tracing::{error, info};
use tracing_appender::non_blocking::WorkerGuard;
use whispy_common::{State, StateSnapshot};

use crate::config::Config;
use crate::state::{now, StatePublisher};
use crate::whisper::WhisperServer;

/// How long to wait for whisper-server to load the model and start listening.
const WHISPER_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(name = "whispy-daemon", version, about = "Push-to-talk dictation daemon for Hyprland")]
struct Args {
    /// TOML config file (default: $XDG_CONFIG_HOME/whispy/config.toml, else built-in defaults).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
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

    let _guard = match init_logging() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("whispy-daemon: failed to init logging: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(&cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "fatal");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: &Config) -> std::io::Result<()> {
    info!(model = %cfg.stt.model, "starting whispy-daemon");

    // Install SIGTERM/SIGINT handlers that flip a flag; the accept loop polls it.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    let publisher = StatePublisher::new(cfg.ipc.state_path());
    let shared = Arc::new(Mutex::new(StateSnapshot {
        state: State::Idle,
        rms: 0.0,
        error_kind: None,
        error_message: None,
        timestamp: now(),
    }));

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

    publisher.publish_state(State::Idle, 0.0).ok();

    // Serve until SIGTERM/SIGINT.
    let result = server::serve(&cfg.ipc.socket_path(), Arc::clone(&shared), Arc::clone(&shutdown));

    info!("shutting down");
    whisper.shutdown();
    result
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
