//! PipeWire audio capture via `pw-record` (raw s16le mono), with RMS metering,
//! digital gain, and clip-duration limits.
//!
//! `pw-record --format s16 -` streams headerless little-endian PCM to stdout. A
//! reader thread parses it into `i16` samples, applies `audio.gain`, reports RMS
//! every `audio.rms_interval_ms`, and caps the buffer at `audio.max_clip_secs`
//! (firing `on_too_long`). The first ~50 ms are dropped to skip the USB codec's
//! stream-start transient, which would otherwise dominate peak normalization.
//!
//! On stop, the buffer is peak-normalized (only amplifying quiet clips) before
//! transcription — the mic records ~20-25 dB low (see docs/stt-benchmark.md).

use std::io::{BufReader, ErrorKind, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tracing::{debug, warn};

use crate::config::Audio;

/// Milliseconds dropped at capture start to skip the codec stream-start transient.
const SKIP_MS: u64 = 50;
/// Peak target for normalization (~ -0.1 dBFS).
const NORM_TARGET: f32 = 32_440.0;
/// Peaks at or below this are treated as silence and left unamplified.
const SILENCE_FLOOR: i32 = 200;

/// Spawns capture sessions using the configured audio parameters.
#[derive(Clone)]
pub struct Recorder {
    cfg: Audio,
}

impl Recorder {
    pub fn new(cfg: Audio) -> Self {
        Self { cfg }
    }

    /// Start capturing. `on_rms` is called roughly every `rms_interval_ms` with a
    /// normalized RMS in `[0, 1]`. `on_too_long` fires once if the clip reaches
    /// `max_clip_secs` (capture stops accumulating).
    pub fn start<R, M>(&self, on_rms: R, on_too_long: M) -> std::io::Result<Capture>
    where
        R: Fn(f32) + Send + 'static,
        M: FnOnce() + Send + 'static,
    {
        let mut child = Command::new("pw-record")
            .arg("--rate")
            .arg(self.cfg.rate.to_string())
            .arg("--channels")
            .arg(self.cfg.channels.to_string())
            .arg("--format")
            .arg("s16")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stop = Arc::new(AtomicBool::new(false));
        let auto_stopped = Arc::new(AtomicBool::new(false));

        let cfg = self.cfg.clone();
        let stop_t = Arc::clone(&stop);
        let auto_t = Arc::clone(&auto_stopped);
        let reader = std::thread::spawn(move || {
            read_loop(stdout, &cfg, stop_t, auto_t, on_rms, on_too_long)
        });

        debug!(pid = child.id(), "capture started");
        Ok(Capture {
            child,
            reader: Some(reader),
            stop,
            auto_stopped,
        })
    }
}

/// A running capture: the `pw-record` child plus its reader thread.
pub struct Capture {
    child: Child,
    reader: Option<JoinHandle<Vec<i16>>>,
    stop: Arc<AtomicBool>,
    auto_stopped: Arc<AtomicBool>,
}

impl Capture {
    /// True if capture hit the max-duration cap.
    pub fn auto_stopped(&self) -> bool {
        self.auto_stopped.load(Ordering::Relaxed)
    }

    /// Stop capture and return the peak-normalized samples, or `None` if the clip
    /// is shorter than `min_samples` (dropped silently).
    pub fn finish(mut self, min_samples: usize) -> Option<Vec<i16>> {
        let buf = self.teardown();
        debug!(samples = buf.len(), min = min_samples, "capture finished");
        if buf.len() < min_samples {
            return None;
        }
        Some(normalize_peak(buf))
    }

    /// Stop capture and discard the buffer.
    pub fn cancel(mut self) {
        let _ = self.teardown();
        debug!("capture cancelled");
    }

    /// Signal stop, kill `pw-record` (closing the pipe -> reader EOF), and join.
    fn teardown(&mut self) -> Vec<i16> {
        self.stop.store(true, Ordering::Relaxed);
        self.child.kill().ok();
        let buf = self
            .reader
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        self.child.wait().ok();
        buf
    }
}

fn read_loop<R, M>(
    stdout: ChildStdout,
    cfg: &Audio,
    stop: Arc<AtomicBool>,
    auto_stopped: Arc<AtomicBool>,
    on_rms: R,
    on_too_long: M,
) -> Vec<i16>
where
    R: Fn(f32),
    M: FnOnce(),
{
    let mut reader = BufReader::new(stdout);
    let mut chunk = [0u8; 8192];
    let mut leftover: Vec<u8> = Vec::new();

    let mut samples: Vec<i16> = Vec::with_capacity(cfg.rate as usize * 4);
    let window_len = cfg.rms_window();
    let max_samples = cfg.max_samples();
    let skip_samples = (cfg.rate as u64 * SKIP_MS / 1000) as usize;
    let gain = cfg.gain;

    let mut skipped = 0usize;
    let mut sq_sum = 0.0f64;
    let mut window_count = 0usize;
    let mut too_long = false;

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                warn!(error = %e, "capture read error");
                break;
            }
        };

        leftover.extend_from_slice(&chunk[..n]);
        let full = leftover.len() - (leftover.len() % 2);
        for pair in leftover[..full].chunks_exact(2) {
            // Drop the stream-start transient.
            if skipped < skip_samples {
                skipped += 1;
                continue;
            }
            let sample = apply_gain(i16::from_le_bytes([pair[0], pair[1]]), gain);
            samples.push(sample);

            let norm = sample as f64 / 32768.0;
            sq_sum += norm * norm;
            window_count += 1;
            if window_count >= window_len {
                let rms = (sq_sum / window_count as f64).sqrt() as f32;
                on_rms(rms.clamp(0.0, 1.0));
                sq_sum = 0.0;
                window_count = 0;
            }

            if samples.len() >= max_samples {
                too_long = true;
                break 'outer;
            }
        }
        leftover.drain(..full);
    }

    if too_long {
        auto_stopped.store(true, Ordering::Relaxed);
        on_too_long();
    }
    samples
}

/// Apply a linear gain with saturation.
fn apply_gain(sample: i16, gain: f32) -> i16 {
    if gain == 1.0 {
        return sample;
    }
    (sample as f32 * gain).round().clamp(-32768.0, 32767.0) as i16
}

/// Peak-normalize toward [`NORM_TARGET`]. Only amplifies (quiet clips); never
/// attenuates, and leaves near-silent buffers untouched.
fn normalize_peak(mut buf: Vec<i16>) -> Vec<i16> {
    let peak = buf
        .iter()
        .map(|s| s.unsigned_abs() as i32)
        .max()
        .unwrap_or(0);
    if peak <= SILENCE_FLOOR {
        return buf;
    }
    let factor = NORM_TARGET / peak as f32;
    if factor <= 1.0 {
        return buf;
    }
    debug!(peak, factor, "peak-normalizing clip");
    for s in &mut buf {
        *s = (*s as f32 * factor).round().clamp(-32768.0, 32767.0) as i16;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_saturates() {
        assert_eq!(apply_gain(20000, 2.0), 32767);
        assert_eq!(apply_gain(-20000, 2.0), -32768);
        assert_eq!(apply_gain(100, 1.0), 100);
    }

    #[test]
    fn normalize_amplifies_quiet_only() {
        // Quiet clip gets amplified so its peak approaches the target.
        let out = normalize_peak(vec![0, 1000, -1000, 500]);
        let peak = out.iter().map(|s| s.unsigned_abs() as i32).max().unwrap();
        assert!(peak > 30_000, "quiet clip should be amplified, peak={peak}");

        // A clip already at/above the target peak is left as-is (never attenuated).
        let loud = vec![0, 32600, -32600];
        assert_eq!(normalize_peak(loud.clone()), loud);

        // Near-silence is untouched.
        let silence = vec![0, 50, -40];
        assert_eq!(normalize_peak(silence.clone()), silence);
    }
}
