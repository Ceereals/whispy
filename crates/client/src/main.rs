//! whispy thin client: sends one command to the daemon over the Unix socket.
//!
//! Invoked from Hyprland keybinds, so it stays tiny and exits fast (no async
//! runtime, no config parsing).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use whispy_common::{Cmd, Resp, State};

#[derive(Parser, Debug)]
#[command(
    name = "whispy-client",
    version,
    about = "Thin client for the whispy dictation daemon"
)]
struct Args {
    /// Daemon socket path (default: $XDG_RUNTIME_DIR/whispy/whispy.sock).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Begin capture.
    Start {
        /// AI workflow to apply to the transcript before injection.
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
    },
    /// Stop capture and transcribe.
    Stop,
    /// Stop capture and discard.
    Cancel,
    /// Toggle capture (start if idle, else stop).
    Toggle {
        /// AI workflow to apply when this toggle starts capture.
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
    },
    /// Print the current state.
    Status,
    /// Healthcheck.
    Ping,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(default_socket);

    let result = match args.command {
        Command::Start { workflow } => send(&socket, Cmd::Start { workflow }),
        Command::Stop => send(&socket, Cmd::Stop),
        Command::Cancel => send(&socket, Cmd::Cancel),
        Command::Status => send(&socket, Cmd::Status),
        Command::Ping => send(&socket, Cmd::Ping),
        Command::Toggle { workflow } => toggle(&socket, workflow),
    };

    match result {
        Ok(resp) => {
            println!("{}", serde_json::to_string(&resp).unwrap_or_default());
            if resp.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("whispy-client: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Query the current state, then start if idle (or stop if a capture is active).
fn toggle(socket: &Path, workflow: Option<String>) -> std::io::Result<Resp> {
    let status = send(socket, Cmd::Status)?;
    let active = matches!(
        status.snapshot.as_ref().map(|s| s.state),
        Some(State::Recording) | Some(State::Transcribing)
    );
    send(
        socket,
        if active {
            Cmd::Stop
        } else {
            Cmd::Start { workflow }
        },
    )
}

fn send(socket: &Path, cmd: Cmd) -> std::io::Result<Resp> {
    let stream = UnixStream::connect(socket)?;
    let mut writer = &stream;
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&cmd).expect("Cmd serializes")
    )?;
    writer.flush()?;

    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn default_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("whispy")
        .join("whispy.sock")
}
