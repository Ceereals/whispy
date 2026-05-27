//! Supervises the whisper-server child process (whisper.cpp Vulkan build).
//!
//! Spawns `cfg.stt.server_bin` with the configured model, waits for the HTTP port
//! to accept connections, and kills the child on daemon shutdown. Restart-on-crash
//! is delegated to systemd (which restarts the whole daemon) in this step.

use std::fs::File;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::config::Stt;

/// A running whisper-server child.
pub struct WhisperServer {
    child: Child,
    host: String,
    port: u16,
}

impl WhisperServer {
    /// Spawn whisper-server with the configured model. Its stdout/stderr go to
    /// `log_path` for post-mortem debugging.
    pub fn spawn(cfg: &Stt, log_path: &Path) -> std::io::Result<Self> {
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
        let child = Command::new(&bin)
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
            .spawn()?;

        Ok(Self {
            child,
            host: cfg.host.clone(),
            port: cfg.port,
        })
    }

    /// Block until the HTTP port accepts a TCP connection, or `timeout` elapses.
    /// Returns early if `shutdown` is set. The child loads the model before it
    /// starts listening, so a successful connect means it is ready to serve.
    pub fn wait_ready(&self, timeout: Duration, shutdown: &Arc<AtomicBool>) -> bool {
        let addr = format!("{}:{}", self.host, self.port);
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

    /// Terminate the child and reap it.
    pub fn shutdown(&mut self) {
        info!("terminating whisper-server");
        if let Err(e) = self.child.kill() {
            warn!(error = %e, "failed to kill whisper-server");
        }
        self.child.wait().ok();
    }
}
