//! Unix socket server: line-based JSON protocol (`Cmd` in, `Resp` out).
//!
//! Accepts connections on `cfg.ipc.socket_path()` and forwards each command to
//! [`App::handle`]. The accept loop polls a shutdown flag so SIGTERM can stop it
//! cleanly; it removes a stale socket on start and the live socket on exit.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};
use whispy_common::{Cmd, Resp};

use crate::app::App;

/// How often the accept loop wakes to re-check the shutdown flag.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long one connection may sit idle. Connections are served one at a time,
/// so a client that connects and never sends (or never reads) would otherwise
/// block every other command and the shutdown check.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// Fail if another daemon is already serving `socket_path`.
///
/// A socket file that accepts a connection belongs to a live daemon: removing it
/// would orphan that daemon (still running, unreachable) and, when this instance
/// exits, leave no socket at all. A file that refuses connections is stale and is
/// left for [`serve`] to replace. Call this before spawning whisper-server, so a
/// second instance doesn't mistake the live daemon's server for its own.
pub fn ensure_not_running(socket_path: &Path) -> std::io::Result<()> {
    if socket_path.exists() && UnixStream::connect(socket_path).is_ok() {
        return Err(std::io::Error::new(
            ErrorKind::AddrInUse,
            format!(
                "another whispy-daemon is already listening on {} \
                 (stop it with `systemctl --user stop whispy-daemon`)",
                socket_path.display()
            ),
        ));
    }
    Ok(())
}

/// Bind the socket and serve until `shutdown` is set. Refuses to take over a live
/// daemon's socket, removes a stale socket file, and cleans up the socket on exit.
pub fn serve(socket_path: &Path, app: Arc<App>, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ensure_not_running(socket_path)?;
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
                if let Err(e) = handle_conn(stream, &app) {
                    warn!(error = %e, "connection error");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
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
fn handle_conn(stream: UnixStream, app: &Arc<App>) -> std::io::Result<()> {
    // The listener is non-blocking; the accepted stream must block on reads.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    let mut writer = &stream;
    let reader = BufReader::new(stream.try_clone()?);

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            // An idle client: drop it so the next connection gets served.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                debug!("closing idle connection");
                break;
            }
            Err(e) => return Err(e),
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Cmd>(&line) {
            Ok(cmd) => {
                debug!(?cmd, "command");
                app.handle(cmd)
            }
            Err(e) => Resp::err(format!("invalid command: {e}")),
        };
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&resp).expect("Resp serializes")
        )?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_socket(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("whispy-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("whispy.sock");
        std::fs::remove_file(&path).ok();
        path
    }

    #[test]
    fn live_socket_is_refused_and_kept() {
        let path = tmp_socket("live");
        let _listener = UnixListener::bind(&path).unwrap();
        let err = ensure_not_running(&path).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        assert!(path.exists(), "a live daemon's socket must not be removed");
    }

    #[test]
    fn stale_or_missing_socket_is_ok() {
        let path = tmp_socket("stale");
        ensure_not_running(&path).unwrap();
        drop(UnixListener::bind(&path).unwrap()); // file stays, nobody listens
        assert!(path.exists());
        ensure_not_running(&path).unwrap();
    }
}
