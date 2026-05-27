//! whispy thin client: sends one command to the daemon over the Unix socket.
//!
//! Invoked from Hyprland keybinds, so it stays tiny and exits fast (no async
//! runtime, no config parsing).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use whispy_common::{Cmd, Resp};

#[derive(Parser, Debug)]
#[command(name = "whispy-client", version, about = "Thin client for the whispy dictation daemon")]
struct Args {
    /// Daemon socket path (default: $XDG_RUNTIME_DIR/dictation.sock).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Begin capture.
    Start,
    /// Stop capture and transcribe.
    Stop,
    /// Stop capture and discard.
    Cancel,
    /// Toggle capture (start if idle, else stop).
    Toggle,
    /// Print the current state.
    Status,
    /// Healthcheck.
    Ping,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(default_socket);

    let cmd = match args.command {
        Command::Start => Cmd::Start,
        Command::Stop => Cmd::Stop,
        Command::Cancel => Cmd::Cancel,
        Command::Status => Cmd::Status,
        Command::Ping => Cmd::Ping,
        // TODO(step-6): resolve toggle to start/stop based on the current state.
        Command::Toggle => Cmd::Status,
    };

    match send(&socket, cmd) {
        Ok(resp) => {
            println!("{}", serde_json::to_string(&resp).unwrap_or_default());
            if resp.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("whispy-client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn send(socket: &Path, cmd: Cmd) -> std::io::Result<Resp> {
    let stream = UnixStream::connect(socket)?;
    let mut writer = &stream;
    writeln!(writer, "{}", serde_json::to_string(&cmd).expect("Cmd serializes"))?;
    writer.flush()?;

    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn default_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dictation.sock")
}
