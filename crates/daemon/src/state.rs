//! Publishes daemon state to `state.json` with atomic writes (write tmp + rename),
//! so the pill UI never reads a half-written file.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use whispy_common::{State, StateSnapshot};

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

    /// Publish a bare state with no error fields and the current timestamp.
    pub fn publish_state(&self, state: State, rms: f32) -> std::io::Result<()> {
        self.publish(&StateSnapshot {
            state,
            rms,
            error_kind: None,
            error_message: None,
            timestamp: now(),
        })
    }
}

/// Current Unix time in fractional seconds.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_is_atomic_and_readable() {
        let dir = std::env::temp_dir().join(format!("whispy-state-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let pubr = StatePublisher::new(path.clone());
        pubr.publish_state(State::Recording, 0.42).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let snap: StateSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(snap.state, State::Recording);
        assert!((snap.rms - 0.42).abs() < 1e-6);
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
