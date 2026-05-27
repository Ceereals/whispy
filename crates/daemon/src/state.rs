//! Daemon state publishing.
//!
//! [`StatePublisher`] writes `state.json` atomically (write tmp + rename) so the
//! pill UI never reads a half-written file. [`Status`] sits on top: it keeps the
//! in-memory snapshot (served by the `status` command) and the file in sync, and
//! flashes the transient `success`/`error` states back to `idle` after a delay.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;
use whispy_common::{State, StateSnapshot};

/// How long the transient `success`/`error` states linger before reverting to idle.
const FLASH: Duration = Duration::from_millis(600);

/// Owns the state file and writes snapshots atomically.
pub struct StatePublisher {
    path: PathBuf,
}

impl StatePublisher {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Write a full snapshot atomically: write to `<path>.tmp`, then rename over `<path>`.
    pub fn publish(&self, snapshot: &StateSnapshot) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec(snapshot)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

/// Current Unix time in fractional seconds.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Shared, cloneable handle to the daemon's published state.
///
/// Every mutation bumps a generation counter; the delayed revert to idle only
/// fires if no newer state was published in the meantime (so a recording started
/// within the flash window is not clobbered).
#[derive(Clone)]
pub struct Status {
    shared: Arc<Mutex<StateSnapshot>>,
    publisher: Arc<StatePublisher>,
    generation: Arc<AtomicU64>,
}

impl Status {
    pub fn new(shared: Arc<Mutex<StateSnapshot>>, publisher: Arc<StatePublisher>) -> Self {
        Self {
            shared,
            publisher,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A copy of the current snapshot (for the `status` command).
    pub fn snapshot(&self) -> StateSnapshot {
        self.shared.lock().expect("state lock").clone()
    }

    /// Set a non-error state with the given RMS. Returns the new generation.
    pub fn set(&self, state: State, rms: f32) -> u64 {
        self.write(StateSnapshot {
            state,
            rms,
            error_kind: None,
            error_message: None,
            timestamp: now(),
        })
    }

    pub fn recording(&self, rms: f32) {
        self.set(State::Recording, rms);
    }

    pub fn transcribing(&self) {
        self.set(State::Transcribing, 0.0);
    }

    pub fn idle(&self) {
        self.set(State::Idle, 0.0);
    }

    /// Flash `success`, then revert to idle after [`FLASH`].
    pub fn success(&self) {
        let g = self.set(State::Success, 0.0);
        self.revert_to_idle_after(g);
    }

    /// Flash `error` with a kind/message, then revert to idle after [`FLASH`].
    pub fn error(&self, kind: &str, message: &str) {
        let g = self.write(StateSnapshot {
            state: State::Error,
            rms: 0.0,
            error_kind: Some(kind.to_string()),
            error_message: Some(message.to_string()),
            timestamp: now(),
        });
        self.revert_to_idle_after(g);
    }

    fn write(&self, snapshot: StateSnapshot) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.shared.lock().expect("state lock") = snapshot.clone();
        if let Err(e) = self.publisher.publish(&snapshot) {
            warn!(error = %e, "failed to publish state.json");
        }
        generation
    }

    /// Spawn a thread that reverts to idle after [`FLASH`], unless a newer state
    /// (a higher generation) was published in the meantime.
    fn revert_to_idle_after(&self, generation: u64) {
        let this = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(FLASH);
            if this.generation.load(Ordering::Acquire) == generation {
                this.idle();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publisher() -> (PathBuf, StatePublisher) {
        let dir = std::env::temp_dir().join(format!(
            "whispy-state-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("state.json");
        (path.clone(), StatePublisher::new(path))
    }

    #[test]
    fn publish_is_atomic_and_readable() {
        let (path, pubr) = publisher();
        pubr.publish(&StateSnapshot {
            state: State::Recording,
            rms: 0.42,
            ..Default::default()
        })
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let snap: StateSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(snap.state, State::Recording);
        assert!((snap.rms - 0.42).abs() < 1e-6);
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn status_tracks_latest_snapshot() {
        let (path, pubr) = publisher();
        let status = Status::new(
            Arc::new(Mutex::new(StateSnapshot::default())),
            Arc::new(pubr),
        );
        status.recording(0.5);
        let snap = status.snapshot();
        assert_eq!(snap.state, State::Recording);
        assert!((snap.rms - 0.5).abs() < 1e-6);

        status.error("too_long", "clip too long");
        let snap = status.snapshot();
        assert_eq!(snap.state, State::Error);
        assert_eq!(snap.error_kind.as_deref(), Some("too_long"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
