//! The recording pipeline.
//!
//! Maps socket commands to the capture -> transcribe -> filter -> (inject) flow
//! and drives the published state. `start` opens a capture; `stop` closes it and
//! runs transcription off-thread so the client returns immediately and the UI
//! follows `state.json`.

use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use whispy_common::{Cmd, Resp};

use crate::audio::{Capture, Recorder};
use crate::config::{self, Config};
use crate::display::DisplayServer;
use crate::filter::{self, Decision, Hallucinations, Metrics};
use crate::inject::Injector;
use crate::pipeline::{self, AppContext};
use crate::state::{Status, now};
use crate::stt::{SttClient, Transcription};

/// What a recording session needs to post-process its transcript: the workflow
/// the hotkey asked for and the window that was focused when capture began.
struct StartCtx {
    workflow: Option<String>,
    window_class: Option<String>,
}

/// Shared application state wired into the socket server.
pub struct App {
    cfg: Config,
    status: Status,
    recorder: Recorder,
    stt: Arc<SttClient>,
    blacklist: Arc<Hallucinations>,
    injector: Injector,
    /// Resolved display server: drives window-class detection and notify gating.
    server: DisplayServer,
    capture: Mutex<Option<Capture>>,
    start_ctx: Mutex<Option<StartCtx>>,
}

impl App {
    pub fn new(
        cfg: Config,
        status: Status,
        stt: SttClient,
        blacklist: Hallucinations,
        server: DisplayServer,
    ) -> Self {
        let recorder = Recorder::new(cfg.audio.clone());
        let injector = Injector::new(&cfg.injection, server);
        Self {
            cfg,
            status,
            recorder,
            stt: Arc::new(stt),
            blacklist: Arc::new(blacklist),
            injector,
            server,
            capture: Mutex::new(None),
            start_ctx: Mutex::new(None),
        }
    }

    /// Dispatch one command from the socket.
    pub fn handle(self: &Arc<Self>, cmd: Cmd) -> Resp {
        match cmd {
            Cmd::Ping => Resp::ok(),
            Cmd::Status => Resp::status(self.status.snapshot()),
            Cmd::Start { workflow } => self.start(workflow),
            Cmd::Stop => self.stop(),
            Cmd::Cancel => self.cancel(),
        }
    }

    fn start(self: &Arc<Self>, workflow: Option<String>) -> Resp {
        let mut slot = self.capture.lock().expect("capture lock");
        if let Some(cap) = slot.as_ref() {
            if cap.auto_stopped() {
                // A previous session hit the max-duration cap (too_long) and was
                // never reclaimed by a stop/cancel. Tear it down (kills the orphan
                // pw-record, joins the finished reader) and start fresh.
                if let Some(stale) = slot.take() {
                    stale.cancel();
                }
            } else {
                return Resp::err("already recording");
            }
        }

        let rms_status = self.status.clone();
        let on_rms = move |rms: f32| rms_status.recording(rms);

        let too_long_status = self.status.clone();
        let too_long_notify = should_notify(&self.cfg.ui.notify, self.server);
        let on_too_long = move || {
            warn!("clip exceeded max duration");
            too_long_status.error("too_long", "clip exceeded the maximum duration");
            if too_long_notify {
                notify("Dictation too long", "Clip exceeded the maximum duration.");
            }
        };

        // Trailing silence finalizes the clip just like an explicit stop. The
        // recorder fires this from its reader thread, so hand the finish+transcribe
        // (which joins that very thread) off to a fresh thread to avoid self-join.
        let silence_app = Arc::clone(self);
        let on_silence = move || {
            info!("auto-stop on trailing silence");
            std::thread::spawn(move || silence_app.auto_finish());
        };

        match self.recorder.start(on_rms, on_too_long, on_silence) {
            Ok(cap) => {
                self.status.recording(0.0);
                *slot = Some(cap);
                // Snapshot the workflow + focused window now: this is the target
                // the transcript will be post-processed for and pasted into.
                *self.start_ctx.lock().expect("start_ctx lock") = Some(StartCtx {
                    workflow,
                    window_class: pipeline::active_window_class(self.server),
                });
                info!("recording started");
                Resp::ok()
            }
            Err(e) => Resp::err(format!("failed to start capture: {e}")),
        }
    }

    fn stop(self: &Arc<Self>) -> Resp {
        let cap = match self.capture.lock().expect("capture lock").take() {
            Some(c) => c,
            None => return Resp::err("not recording"),
        };

        // The max-duration case already reported `too_long` via on_too_long.
        if cap.auto_stopped() {
            cap.cancel();
            return Resp::ok();
        }

        match cap.finish(self.cfg.audio.min_samples()) {
            None => {
                // Clip too short: discard silently, back to idle.
                self.status.idle();
                Resp::ok()
            }
            Some(buf) => {
                self.status.transcribing();
                let app = Arc::clone(self);
                std::thread::spawn(move || app.transcribe(buf));
                Resp::ok()
            }
        }
    }

    /// Finalize a capture that auto-stopped on trailing silence: transcribe what
    /// was said, mirroring the explicit `stop` path. Runs on its own thread.
    /// A no-op if an explicit stop/cancel already reclaimed the capture.
    fn auto_finish(self: Arc<Self>) {
        let cap = match self.capture.lock().expect("capture lock").take() {
            Some(c) => c,
            None => return,
        };
        match cap.finish(self.cfg.audio.min_samples()) {
            None => self.status.idle(),
            Some(buf) => {
                self.status.transcribing();
                self.transcribe(buf);
            }
        }
    }

    fn cancel(&self) -> Resp {
        if let Some(cap) = self.capture.lock().expect("capture lock").take() {
            cap.cancel();
        }
        *self.start_ctx.lock().expect("start_ctx lock") = None;
        self.status.idle();
        Resp::ok()
    }

    /// Off-thread tail of `stop`: transcribe, filter, (inject), publish outcome.
    fn transcribe(self: Arc<Self>, buf: Vec<i16>) {
        let transcription = match self.stt.transcribe(&buf) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "transcription failed");
                self.status.error("stt_error", &e.to_string());
                self.notify("Dictation failed", &e.to_string());
                return;
            }
        };

        let metrics = Metrics {
            avg_logprob: transcription.avg_logprob,
            no_speech_prob: transcription.no_speech_prob,
        };
        let decision = filter::evaluate(
            &transcription.text,
            metrics,
            &self.cfg.filter,
            &self.blacklist,
        );

        // Log every transcription (accepted or dropped) for calibration.
        let reason = match &decision {
            Decision::Accept(_) => None,
            Decision::Drop(r) => Some(r.kind()),
        };
        log_transcript(&transcription, reason);

        match decision {
            Decision::Accept(text) => {
                // Post-process for the session's target window (corrections,
                // snippets, then the selected AI workflow). Never fails: LLM
                // errors fall back to the raw transcript inside `process`.
                let ctx = {
                    let guard = self.start_ctx.lock().expect("start_ctx lock");
                    guard
                        .as_ref()
                        .map(|c| AppContext {
                            workflow: c.workflow.clone(),
                            window_class: c.window_class.clone(),
                        })
                        .unwrap_or_default()
                };
                let text = pipeline::process(text, &self.cfg, &ctx, self.injector.backend());
                info!(chars = text.len(), lang = ?transcription.language, "transcript accepted");
                match self.injector.inject(&text) {
                    Ok(()) => self.status.success(),
                    Err(e) => {
                        warn!(error = %e, "injection failed");
                        self.status.error("inject_error", &e.to_string());
                        self.notify("Dictation paste failed", &e.to_string());
                    }
                }
            }
            Decision::Drop(reason) => {
                info!(reason = reason.kind(), "transcript dropped");
                self.status
                    .error(reason.kind(), "transcript rejected by filter");
            }
        }
    }

    /// Best-effort desktop notification, gated by `ui.notify` and the display
    /// server (see [`should_notify`]). A no-op when notifications are disabled for
    /// this session; `state.json` is still published unconditionally.
    fn notify(&self, summary: &str, body: &str) {
        if should_notify(&self.cfg.ui.notify, self.server) {
            notify(summary, body);
        }
    }
}

/// Decide whether to fire a desktop notification for a transient state transition.
///
/// - `"off"` — never.
/// - `"on"` — always.
/// - `"auto"` (default) — only on non-layer-shell sessions (X11), where the
///   Quickshell pill can't run; Wayland users see the pill instead.
///
/// Unknown values (rejected by `Config::validate` before we get here) behave as
/// `"auto"` defensively.
fn should_notify(mode: &str, server: DisplayServer) -> bool {
    match mode {
        "off" => false,
        "on" => true,
        _ => server == DisplayServer::X11,
    }
}

/// Append one transcription record (accepted or dropped) to `transcripts.jsonl`
/// under `$XDG_STATE_HOME/whispy`, for offline calibration of the filter.
fn log_transcript(t: &Transcription, drop_reason: Option<&str>) {
    use std::io::Write;

    let record = serde_json::json!({
        "timestamp": now(),
        "text": t.text,
        "accepted": drop_reason.is_none(),
        "drop_reason": drop_reason,
        "avg_logprob": t.avg_logprob,
        "no_speech_prob": t.no_speech_prob,
        "language": t.language,
    });

    let path = config::state_dir().join("transcripts.jsonl");
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{record}"));
    if let Err(e) = result {
        warn!(error = %e, "failed to log transcript");
    }
}

/// Best-effort desktop notification for hard failures (not routine filter drops).
fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .arg("--app-name=whispy")
        .arg(summary)
        .arg(body)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_off_never_fires() {
        assert!(!should_notify("off", DisplayServer::X11));
        assert!(!should_notify("off", DisplayServer::Wayland));
    }

    #[test]
    fn notify_on_always_fires() {
        assert!(should_notify("on", DisplayServer::X11));
        assert!(should_notify("on", DisplayServer::Wayland));
    }

    #[test]
    fn notify_auto_fires_only_on_x11() {
        // X11 has no layer-shell pill, so the notification is the only UI there.
        assert!(should_notify("auto", DisplayServer::X11));
        // Wayland gets the Quickshell pill, so auto stays quiet to avoid double-serving.
        assert!(!should_notify("auto", DisplayServer::Wayland));
        // Unknown values (validated away) behave as auto.
        assert!(should_notify("bogus", DisplayServer::X11));
        assert!(!should_notify("bogus", DisplayServer::Wayland));
    }
}
