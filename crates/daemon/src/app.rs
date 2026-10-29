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
use crate::config::Config;
use crate::display::DisplayServer;
use crate::filter::{self, Decision, Hallucinations, Metrics};
use crate::inject::Injector;
use crate::pipeline::{self, AppContext};
use crate::state::Status;
use crate::stt::{SttClient, Transcription};
use crate::transcripts::{Record, TranscriptLog};

/// One recording in progress: the live capture plus what its transcript needs for
/// post-processing (the workflow the hotkey asked for and the window that was
/// focused when capture began). Kept together so a later recording can never
/// swap the context out from under a clip that is still transcribing.
struct Session {
    capture: Capture,
    ctx: AppContext,
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
    session: Mutex<Option<Session>>,
    /// Serializes post-processing + injection across transcribe threads: a new
    /// clip can finish while the previous one is still pasting, and two pastes
    /// interleaving their clipboard save/restore would lose the user's clipboard.
    inject_lock: Mutex<()>,
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
            session: Mutex::new(None),
            inject_lock: Mutex::new(()),
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
        let mut slot = self.session.lock().expect("session lock");
        if let Some(session) = slot.as_ref() {
            if session.capture.auto_stopped() {
                // A previous session hit the max-duration cap (too_long) and was
                // never reclaimed by a stop/cancel. Tear it down (kills the orphan
                // pw-record, joins the finished reader) and start fresh.
                if let Some(stale) = slot.take() {
                    stale.capture.cancel();
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
            Ok(capture) => {
                self.status.recording(0.0);
                // Snapshot the workflow + focused window now: this is the target
                // the transcript will be post-processed for and pasted into.
                let ctx = AppContext {
                    workflow,
                    window_class: pipeline::active_window_class(self.server),
                };
                *slot = Some(Session { capture, ctx });
                info!("recording started");
                Resp::ok()
            }
            Err(e) => Resp::err(format!("failed to start capture: {e}")),
        }
    }

    fn stop(self: &Arc<Self>) -> Resp {
        let Some(Session { capture, ctx }) = self.session.lock().expect("session lock").take()
        else {
            return Resp::err("not recording");
        };

        // The max-duration case already reported `too_long` via on_too_long.
        if capture.auto_stopped() {
            capture.cancel();
            return Resp::ok();
        }

        match capture.finish(self.cfg.audio.min_samples()) {
            None => {
                // Clip too short: discard silently, back to idle.
                self.status.idle();
                Resp::ok()
            }
            Some(buf) => {
                self.status.transcribing();
                let app = Arc::clone(self);
                std::thread::spawn(move || app.transcribe(buf, ctx));
                Resp::ok()
            }
        }
    }

    /// Finalize a capture that auto-stopped on trailing silence: transcribe what
    /// was said, mirroring the explicit `stop` path. Runs on its own thread.
    /// A no-op if an explicit stop/cancel already reclaimed the capture.
    fn auto_finish(self: Arc<Self>) {
        let Some(Session { capture, ctx }) = self.session.lock().expect("session lock").take()
        else {
            return;
        };
        match capture.finish(self.cfg.audio.min_samples()) {
            None => self.status.idle(),
            Some(buf) => {
                self.status.transcribing();
                self.transcribe(buf, ctx);
            }
        }
    }

    fn cancel(&self) -> Resp {
        if let Some(session) = self.session.lock().expect("session lock").take() {
            session.capture.cancel();
        }
        self.status.idle();
        Resp::ok()
    }

    /// Off-thread tail of `stop`: transcribe, filter, (inject), publish outcome.
    fn transcribe(self: Arc<Self>, buf: Vec<i16>, ctx: AppContext) {
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
                // Held through injection so overlapping clips paste one at a time,
                // in order.
                let _inject = self.inject_lock.lock().expect("inject lock");
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

/// Append one transcription record (accepted or dropped) to the
/// [`TranscriptLog`], for offline calibration of the filter. Best-effort.
fn log_transcript(t: &Transcription, drop_reason: Option<&str>) {
    if let Err(e) = TranscriptLog::open().append(&Record::new(t, drop_reason)) {
        warn!(error = %e, "failed to log transcript");
    }
}

/// Best-effort desktop notification for hard failures (not routine filter drops).
fn notify(summary: &str, body: &str) {
    let _ = crate::proc::status(
        std::process::Command::new("notify-send")
            .arg("--app-name=whispy")
            .arg(summary)
            .arg(body),
        crate::proc::PROBE,
    );
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
