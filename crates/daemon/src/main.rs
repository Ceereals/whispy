//! whispy dictation daemon.
//!
//! Holds the whisper-server child resident, captures audio on demand, runs the
//! hallucination filter, and injects the transcript. Clients drive it over a Unix
//! socket; the Quickshell pill UI reads `state.json`.

// TODO(step-2): drop once every module is wired into the run loop.
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

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "whispy-daemon", version, about = "Push-to-talk dictation daemon for Hyprland")]
struct Args {
    /// TOML config file (default: $XDG_CONFIG_HOME/whispy/config.toml, else built-in defaults).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let cfg = match config::Config::load(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("whispy-daemon: failed to load config: {e}");
            return ExitCode::FAILURE;
        }
    };

    // TODO(step-2): init JSON-lines logging, supervise whisper-server, run the
    // socket server loop, publish state.json, and handle SIGTERM.
    println!(
        "whispy-daemon scaffold ok (model={}, socket={})",
        cfg.stt.model,
        cfg.ipc.socket_path().display(),
    );
    ExitCode::SUCCESS
}
