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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Minimum gap between `state.json` writes for repeated `recording` RMS ticks.
    /// `Duration::ZERO` (state_max_hz == 0) disables throttling.
    min_interval: Duration,
    last_publish: Arc<Mutex<Instant>>,
}

impl Status {
    pub fn new(
        shared: Arc<Mutex<StateSnapshot>>,
        publisher: Arc<StatePublisher>,
        state_max_hz: u32,
    ) -> Self {
        let min_interval = if state_max_hz == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / state_max_hz as f64)
        };
        Self {
            shared,
            publisher,
            generation: Arc::new(AtomicU64::new(0)),
            min_interval,
            last_publish: Arc::new(Mutex::new(Instant::now())),
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
        let prev_state = {
            let mut guard = self.shared.lock().expect("state lock");
            let prev = guard.state;
            *guard = snapshot.clone();
            prev
        };
        if self.should_publish(snapshot.state, prev_state)
            && let Err(e) = self.publisher.publish(&snapshot)
        {
            warn!(error = %e, "failed to publish state.json");
        }
        generation
    }

    /// Throttle only repeated `recording` RMS updates; every state *transition* and
    /// every non-recording state (idle/success/error/transcribing) always publishes.
    fn should_publish(&self, new: State, prev: State) -> bool {
        let throttled = new == State::Recording && prev == State::Recording;
        if !throttled || self.min_interval.is_zero() {
            *self.last_publish.lock().expect("last_publish lock") = Instant::now();
            return true;
        }
        let mut last = self.last_publish.lock().expect("last_publish lock");
        if last.elapsed() >= self.min_interval {
            *last = Instant::now();
            true
        } else {
            false
        }
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
            20,
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

    fn status_with(hz: u32) -> (PathBuf, Status) {
        let (path, pubr) = publisher();
        let status = Status::new(
            Arc::new(Mutex::new(StateSnapshot::default())),
            Arc::new(pubr),
            hz,
        );
        (path, status)
    }

    #[test]
    fn revert_to_idle_only_when_generation_unchanged() {
        let (path, status) = status_with(0);

        // A stale generation (nothing published since) reverts to idle.
        let g = status.set(State::Success, 0.0);
        status.revert_to_idle_after(g);
        std::thread::sleep(FLASH + Duration::from_millis(150));
        assert_eq!(status.snapshot().state, State::Idle);

        // A newer write during the flash window cancels the revert.
        let g = status.set(State::Success, 0.0);
        status.revert_to_idle_after(g);
        status.recording(0.3); // bumps the generation past `g`
        std::thread::sleep(FLASH + Duration::from_millis(150));
        assert_eq!(status.snapshot().state, State::Recording);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn debounce_drops_rapid_recording_writes_but_keeps_transitions() {
        // 20 Hz => 50ms min interval between recording RMS publishes.
        let (path, status) = status_with(20);

        // First recording write is a transition (idle -> recording): always published.
        assert!(status.should_publish(State::Recording, State::Idle));
        // Immediate repeat while already recording is throttled.
        assert!(!status.should_publish(State::Recording, State::Recording));
        // A non-recording state always publishes, even back-to-back.
        assert!(status.should_publish(State::Transcribing, State::Recording));
        assert!(status.should_publish(State::Idle, State::Transcribing));
        // After the interval elapses, a recording write publishes again.
        status.recording(0.1); // resets the timer via the transition path
        std::thread::sleep(Duration::from_millis(60));
        assert!(status.should_publish(State::Recording, State::Recording));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn hz_zero_disables_throttling() {
        let (path, status) = status_with(0);
        assert!(status.min_interval.is_zero());
        assert!(status.should_publish(State::Recording, State::Recording));
        assert!(status.should_publish(State::Recording, State::Recording));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
