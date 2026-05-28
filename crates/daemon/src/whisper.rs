//! Supervises the whisper-server child process (whisper.cpp Vulkan build).
//!
//! Spawns `cfg.stt.server_bin` with the configured model, waits for the HTTP port
//! to accept connections, and kills the child on daemon shutdown. A monitor thread
//! in `main` polls [`WhisperServer::is_alive`] and calls [`WhisperServer::restart`]
//! if the child dies after startup — without it the daemon would keep running but
//! every transcription would fail (systemd only restarts us if *we* exit, not the
//! child).

use std::fs::File;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::config::Stt;

/// A running whisper-server child.
pub struct WhisperServer {
    child: Child,
    cfg: Stt,
    log_path: PathBuf,
}

impl WhisperServer {
    /// Spawn whisper-server with the configured model. Its stdout/stderr go to
    /// `log_path` for post-mortem debugging.
    pub fn spawn(cfg: &Stt, log_path: &Path) -> std::io::Result<Self> {
        let child = Self::start_child(cfg, log_path)?;
        Ok(Self {
            child,
            cfg: cfg.clone(),
            log_path: log_path.to_path_buf(),
        })
    }

    /// Launch one whisper-server process (shared by `spawn` and `restart`).
    fn start_child(cfg: &Stt, log_path: &Path) -> std::io::Result<Child> {
        let bin = cfg.server_bin_path();
        let model = cfg.model_file();
        if !bin.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("whisper-server binary not found at {}", bin.display()),
            ));
        }
        if !model.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("model file not found at {}", model.display()),
            ));
        }

        let log = File::create(log_path)?;
        let errlog = log.try_clone()?;
        info!(bin = %bin.display(), model = %model.display(), port = cfg.port, "spawning whisper-server");
        Command::new(&bin)
            .arg("-m")
            .arg(&model)
            .arg("-l")
            .arg(&cfg.language)
            .arg("--host")
            .arg(&cfg.host)
            .arg("--port")
            .arg(cfg.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errlog))
            .spawn()
    }

    /// Block until the HTTP port accepts a TCP connection, or `timeout` elapses.
    /// Returns early if `shutdown` is set. The child loads the model before it
    /// starts listening, so a successful connect means it is ready to serve.
    pub fn wait_ready(&self, timeout: Duration, shutdown: &Arc<AtomicBool>) -> bool {
        let addr = format!("{}:{}", self.cfg.host, self.cfg.port);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if shutdown.load(Ordering::Relaxed) {
                return false;
            }
            if TcpStream::connect_timeout(
                &addr.parse().expect("valid host:port"),
                Duration::from_millis(500),
            )
            .is_ok()
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    /// Has the child exited? `try_wait` is non-blocking; `Ok(None)` means still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Reap the dead child and spawn a fresh one, then wait for it to become ready.
    /// Returns whether the new child reached the listening state within `timeout`.
    pub fn restart(
        &mut self,
        timeout: Duration,
        shutdown: &Arc<AtomicBool>,
    ) -> std::io::Result<bool> {
        warn!("whisper-server is not running; restarting");
        self.child.wait().ok();
        self.child = Self::start_child(&self.cfg, &self.log_path)?;
        Ok(self.wait_ready(timeout, shutdown))
    }

    /// Terminate the child and reap it.
    pub fn shutdown(&mut self) {
        info!("terminating whisper-server");
        if let Err(e) = self.child.kill() {
            warn!(error = %e, "failed to kill whisper-server");
        }
        self.child.wait().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_cfg() -> Stt {
        Config::load(None).expect("defaults load").stt
    }

    #[test]
    fn is_alive_reports_false_after_child_exits() {
        // A short-lived process stands in for whisper-server: once it exits,
        // `is_alive` must report false so the monitor can restart it.
        let child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test child");
        let mut server = WhisperServer {
            child,
            cfg: test_cfg(),
            log_path: PathBuf::from("/dev/null"),
        };
        // Give the child a moment to exit.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!server.is_alive());
    }
}
