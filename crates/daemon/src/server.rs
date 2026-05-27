//! Unix socket server: line-based JSON protocol (`Cmd` in, `Resp` out).
//!
//! Accepts connections on `cfg.ipc.socket_path()` and dispatches commands. The
//! accept loop polls a shutdown flag so SIGTERM can stop it cleanly. `Ping` and
//! `Status` are handled here; capture commands arrive in later steps.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{debug, info, warn};
use whispy_common::{Cmd, Resp, StateSnapshot};

/// How often the accept loop wakes to re-check the shutdown flag.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bind the socket and serve until `shutdown` is set. Removes a stale socket file
/// first and cleans up the socket on exit.
pub fn serve(
    socket_path: &Path,
    shared: Arc<Mutex<StateSnapshot>>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove a stale socket from a previous run (a left-over file blocks bind).
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    info!(socket = %socket_path.display(), "listening");

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_conn(stream, &shared) {
                    warn!(error = %e, "connection error");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => warn!(error = %e, "accept error"),
        }
    }

    std::fs::remove_file(socket_path).ok();
    info!("socket closed");
    Ok(())
}

/// Read newline-delimited commands from one connection and write one response each.
fn handle_conn(stream: UnixStream, shared: &Arc<Mutex<StateSnapshot>>) -> std::io::Result<()> {
    // The listener is non-blocking; the accepted stream must block on reads.
    stream.set_nonblocking(false)?;
    let mut writer = &stream;
    let reader = BufReader::new(stream.try_clone()?);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Cmd>(&line) {
            Ok(cmd) => {
                debug!(?cmd, "command");
                dispatch(cmd, shared)
            }
            Err(e) => Resp::err(format!("invalid command: {e}")),
        };
        writeln!(writer, "{}", serde_json::to_string(&resp).expect("Resp serializes"))?;
        writer.flush()?;
    }
    Ok(())
}

/// Map a command to a response. Capture commands are stubbed until Step 3.
fn dispatch(cmd: Cmd, shared: &Arc<Mutex<StateSnapshot>>) -> Resp {
    match cmd {
        Cmd::Ping => Resp::ok(),
        Cmd::Status => Resp::status(shared.lock().expect("state lock").clone()),
        Cmd::Start | Cmd::Stop | Cmd::Cancel => Resp::err("not implemented yet (Step 3+)"),
    }
}
