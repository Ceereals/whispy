//! Live input level and test dictation for the Microphone section. Both run on the
//! daemon's own [`Recorder`], so the meter and the test hear exactly what dictation
//! will: same `pw-record` arguments, gain, and speech gating.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::audio::{Capture, Recorder};
use crate::config::Audio;

/// Longest meter capture before it is restarted (bounds the unused buffer).
const METER_CLIP_SECS: f64 = 30.0;

/// Owns at most one capture: either the level meter or a test dictation.
pub struct Mic {
    audio: Audio,
    level: Arc<AtomicU32>,
    listening: Option<(String, Capture)>,
    testing: Option<Capture>,
}

impl Mic {
    /// A microphone helper using the daemon's audio settings.
    pub fn new(audio: Audio) -> Self {
        Self {
            audio,
            level: Arc::new(AtomicU32::new(0)),
            listening: None,
            testing: None,
        }
    }

    /// Latest RMS level in `[0, 1]`.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Meter `source` (empty = default source). Restarts the capture when the
    /// source changes or the previous one hit its length cap; no-op otherwise.
    /// Does nothing while a test dictation is running.
    pub fn listen(&mut self, source: &str) {
        if self.testing.is_some() {
            return;
        }
        let fresh = self
            .listening
            .as_ref()
            .is_some_and(|(s, c)| s == source && !c.auto_stopped());
        if fresh {
            return;
        }
        self.stop();
        let mut cfg = self.audio.clone();
        cfg.source = source.to_string();
        cfg.max_clip_secs = METER_CLIP_SECS;
        cfg.silence_timeout_ms = 0;
        if let Ok(capture) = self.start(cfg) {
            self.listening = Some((source.to_string(), capture));
        }
    }

    /// Stop metering (and drop any running test).
    pub fn stop(&mut self) {
        if let Some((_, capture)) = self.listening.take() {
            capture.cancel();
        }
        if let Some(capture) = self.testing.take() {
            capture.cancel();
        }
        self.level.store(0, Ordering::Relaxed);
    }

    /// True while a test dictation is recording.
    pub fn is_testing(&self) -> bool {
        self.testing.is_some()
    }

    /// Start recording a test dictation from `source`.
    pub fn start_test(&mut self, source: &str) -> std::io::Result<()> {
        self.stop();
        let mut cfg = self.audio.clone();
        cfg.source = source.to_string();
        cfg.silence_timeout_ms = 0;
        self.testing = Some(self.start(cfg)?);
        Ok(())
    }

    /// Stop the test dictation. Returns the clip, or `None` when it was too short
    /// or no speech was heard (the daemon would drop it the same way).
    pub fn finish_test(&mut self) -> Option<Vec<i16>> {
        let capture = self.testing.take()?;
        self.level.store(0, Ordering::Relaxed);
        capture.finish(self.audio.min_samples())
    }

    fn start(&self, cfg: Audio) -> std::io::Result<Capture> {
        let level = Arc::clone(&self.level);
        Recorder::new(cfg).start(
            move |rms| level.store(rms.to_bits(), Ordering::Relaxed),
            || {},
            || {},
        )
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bottom and top of the meter scale, in dBFS. Speech on a quiet mic sits around
/// -50 dB (see `audio.silence_rms_threshold`), so the scale reaches well below it.
const METER_FLOOR_DB: f32 = -70.0;
const METER_CEIL_DB: f32 = -10.0;

/// Map an RMS level to a meter fill in `[0, 1]` on a dB scale.
pub fn meter_fill(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - METER_FLOOR_DB) / (METER_CEIL_DB - METER_FLOOR_DB)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_fill_is_a_db_scale() {
        assert!(meter_fill(0.0).abs() < f32::EPSILON);
        assert!(
            meter_fill(0.0005) < 0.1,
            "room silence (-66 dB) stays near empty"
        );
        assert!((meter_fill(1.0) - 1.0).abs() < 1e-6, "clamped at full");
        // -40 dB is the middle of the -70..-10 dB scale.
        assert!((meter_fill(0.01) - 0.5).abs() < 1e-3);
    }
}
