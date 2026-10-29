//! The dictation session: the product's core flow as one state machine.
//!
//! [`App`] maps socket commands onto the lifecycle of a dictation:
//!
//! ```text
//!            start                    stop / trailing silence
//!   Idle ─────────────► Recording ───────────────────────────► Idle
//!    ▲                     │ max duration (too_long)            │ clip handed off
//!    │  stop / cancel /    ▼                                    ▼
//!    └───── start ─────  Overrun            Transcribing ─► filter ─► Delivering
//!                                           (off-thread)             ─► success / error ─► Idle
//! ```
//!
//! Only one capture is live at a time (the `Recording` slot); the clips it hands
//! off transcribe on their own threads, so a new recording may start while an
//! older clip is still in flight. Deliveries are serialized so overlapping clips
//! paste one at a time, in order.
//!
//! The session reaches the outside world only through [`Ports`]: the microphone,
//! the STT server, the focused-window probe, delivery (post-processing +
//! injection), desktop notifications, the transcript log and thread spawning.
//! [`Desktop`] is the production adapter; the tests drive the whole lifecycle
//! through a fake. Published state goes through the concrete [`Status`].

use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use whispy_common::{Cmd, Resp};

use crate::audio::{self, Recorder};
use crate::config::{self, Config};
use crate::display::DisplayServer;
use crate::filter::{self, Decision, Hallucinations, Metrics};
use crate::inject::{InjectError, Injector};
use crate::pipeline::{self, AppContext};
use crate::state::{Status, now};
use crate::stt::{SttClient, SttError, Transcription};

/// The callbacks a capture reports through while it runs (see [`Recorder::start`]).
pub struct CaptureEvents {
    /// Called roughly every `rms_interval_ms` with a normalized RMS in `[0, 1]`.
    pub rms: Box<dyn Fn(f32) + Send>,
    /// Fires once if the clip reaches `max_clip_secs`; the capture auto-stops.
    pub too_long: Box<dyn FnOnce() + Send>,
    /// Fires once when trailing silence after speech ends the clip.
    pub silence: Box<dyn FnOnce() + Send>,
}

/// A live capture, as the session drives it.
pub trait Capture: Send {
    /// True once the capture hit the max-duration cap and stopped on its own.
    fn auto_stopped(&self) -> bool;
    /// Stop and return the clip, or `None` if it is too short or held no speech.
    fn finish(self: Box<Self>, min_samples: usize) -> Option<Vec<i16>>;
    /// Stop and discard the clip.
    fn cancel(self: Box<Self>);
}

/// Everything the session needs from outside the process. One adapter per world:
/// [`Desktop`] in production, an in-memory fake in the tests.
pub trait Ports: Send + Sync {
    /// Open a capture that reports through `events`.
    fn start_capture(&self, events: CaptureEvents) -> std::io::Result<Box<dyn Capture>>;
    /// Transcribe a clip of 16 kHz mono PCM.
    fn transcribe(&self, samples: &[i16]) -> Result<Transcription, SttError>;
    /// The focused window's class, probed when a recording starts.
    fn focused_window(&self) -> Option<String>;
    /// Post-process an accepted transcript for `ctx` and inject it.
    fn deliver(&self, text: String, ctx: &AppContext) -> Result<(), InjectError>;
    /// A desktop notification for a hard failure (the adapter decides whether it shows).
    fn notify(&self, summary: &str, body: &str);
    /// Append one transcription record (accepted or dropped) to the transcript log.
    fn log_transcript(&self, transcription: &Transcription, drop_reason: Option<&str>);
    /// Run `job` on its own thread.
    fn spawn(&self, job: Box<dyn FnOnce() + Send>);
}

/// The live recording: its capture plus what its transcript needs for
/// post-processing (the workflow the hotkey asked for and the window that was
/// focused when capture began). Kept together so a later recording can never
/// swap the context out from under a clip that is still transcribing.
struct Recording {
    capture: Box<dyn Capture>,
    ctx: AppContext,
}

/// A finished recording on its way to transcription and delivery.
struct Clip {
    samples: Vec<i16>,
    ctx: AppContext,
}

/// How one clip ended.
enum Outcome {
    /// Injected into the focused window.
    Delivered,
    /// Transcription, the filter or injection failed; `notify` names the
    /// desktop notification to raise, if the failure warrants one.
    Failed {
        kind: &'static str,
        message: String,
        notify: Option<&'static str>,
    },
}

/// The dictation session, wired into the socket server.
pub struct App {
    cfg: Config,
    status: Status,
    blacklist: Hallucinations,
    ports: Box<dyn Ports>,
    /// The live (or overrun) recording, if any.
    recording: Mutex<Option<Recording>>,
    /// Serializes delivery across clip threads: a new clip can finish while the
    /// previous one is still pasting, and two pastes interleaving their clipboard
    /// save/restore would lose the user's clipboard.
    inject_lock: Mutex<()>,
}

impl App {
    /// A session on the real desktop: `pw-record`, the STT server, the configured
    /// injector and `notify-send`.
    pub fn new(
        cfg: Config,
        status: Status,
        stt: SttClient,
        blacklist: Hallucinations,
        server: DisplayServer,
    ) -> Self {
        let desktop = Desktop {
            recorder: Recorder::new(cfg.audio.clone()),
            stt,
            injector: Injector::new(&cfg.injection, server),
            notify: should_notify(&cfg.ui.notify, server),
            server,
            cfg: cfg.clone(),
        };
        Self::with_ports(cfg, status, blacklist, Box::new(desktop))
    }

    /// A session over arbitrary ports.
    fn with_ports(
        cfg: Config,
        status: Status,
        blacklist: Hallucinations,
        ports: Box<dyn Ports>,
    ) -> Self {
        Self {
            cfg,
            status,
            blacklist,
            ports,
            recording: Mutex::new(None),
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
        let mut slot = self.recording.lock().expect("recording lock");
        // An overrun recording (too_long, already reported) that no stop/cancel
        // reclaimed: tear it down and start fresh.
        if let Some(stale) = slot.take_if(|r| r.capture.auto_stopped()) {
            stale.capture.cancel();
        }
        if slot.is_some() {
            return Resp::err("already recording");
        }

        match self.ports.start_capture(self.capture_events()) {
            Ok(capture) => {
                self.status.recording(0.0);
                // Snapshot the workflow + focused window now: this is the target
                // the transcript will be post-processed for and pasted into.
                let ctx = AppContext {
                    workflow,
                    window_class: self.ports.focused_window(),
                };
                *slot = Some(Recording { capture, ctx });
                info!("recording started");
                Resp::ok()
            }
            Err(e) => Resp::err(format!("failed to start capture: {e}")),
        }
    }

    /// The callbacks for a new capture: meter, max-duration overrun, auto-stop.
    fn capture_events(self: &Arc<Self>) -> CaptureEvents {
        let rms_status = self.status.clone();
        let too_long = Arc::clone(self);
        // The capture fires `on_silence` from its reader thread, and finishing
        // joins that very thread: finish on a fresh thread to avoid a self-join.
        let silence = Arc::clone(self);
        CaptureEvents {
            rms: Box::new(move |rms| rms_status.recording(rms)),
            too_long: Box::new(move || {
                warn!("clip exceeded max duration");
                let message = "clip exceeded the maximum duration";
                too_long.status.error("too_long", message);
                too_long
                    .ports
                    .notify("Dictation too long", "Clip exceeded the maximum duration.");
            }),
            silence: Box::new(move || {
                info!("auto-stop on trailing silence");
                let app = Arc::clone(&silence);
                silence.ports.spawn(Box::new(move || app.auto_finish()));
            }),
        }
    }

    fn stop(self: &Arc<Self>) -> Resp {
        let Some(recording) = self.take_recording() else {
            return Resp::err("not recording");
        };
        if let Some(clip) = self.close(recording) {
            // Transcribe off-thread: the client returns now, the UI follows state.
            let app = Arc::clone(self);
            self.ports.spawn(Box::new(move || app.process(clip)));
        }
        Resp::ok()
    }

    /// Finish a recording that auto-stopped on trailing silence, exactly as an
    /// explicit `stop` would. Runs on its own thread; a no-op if a stop or cancel
    /// already reclaimed the recording.
    fn auto_finish(&self) {
        if let Some(clip) = self.take_recording().and_then(|r| self.close(r)) {
            self.process(clip);
        }
    }

    fn cancel(&self) -> Resp {
        if let Some(recording) = self.take_recording() {
            recording.capture.cancel();
        }
        self.status.idle();
        Resp::ok()
    }

    /// Take the recording out of the slot, releasing the lock before the capture
    /// is joined.
    fn take_recording(&self) -> Option<Recording> {
        self.recording.lock().expect("recording lock").take()
    }

    /// Stop a recording and hand off its clip. An overrun capture is discarded
    /// (`too_long` was already reported) and a too-short or speechless clip goes
    /// back to idle silently; neither yields a clip.
    fn close(&self, recording: Recording) -> Option<Clip> {
        let Recording { capture, ctx } = recording;
        if capture.auto_stopped() {
            capture.cancel();
            return None;
        }
        let Some(samples) = capture.finish(self.cfg.audio.min_samples()) else {
            self.status.idle();
            return None;
        };
        self.status.transcribing();
        Some(Clip { samples, ctx })
    }

    /// Transcribe, filter and deliver one clip, then publish how it ended.
    fn process(&self, clip: Clip) {
        match self.outcome(clip) {
            Outcome::Delivered => self.status.success(),
            Outcome::Failed {
                kind,
                message,
                notify,
            } => {
                self.status.error(kind, &message);
                if let Some(summary) = notify {
                    self.ports.notify(summary, &message);
                }
            }
        }
    }

    fn outcome(&self, clip: Clip) -> Outcome {
        let transcription = match self.ports.transcribe(&clip.samples) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "transcription failed");
                return Outcome::Failed {
                    kind: "stt_error",
                    message: e.to_string(),
                    notify: Some("Dictation failed"),
                };
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
        self.ports.log_transcript(&transcription, reason);

        match decision {
            Decision::Accept(text) => {
                info!(lang = ?transcription.language, "transcript accepted");
                self.deliver(text, &clip.ctx)
            }
            Decision::Drop(reason) => {
                info!(reason = reason.kind(), "transcript dropped");
                Outcome::Failed {
                    kind: reason.kind(),
                    message: "transcript rejected by filter".to_string(),
                    notify: None,
                }
            }
        }
    }

    /// Deliver an accepted transcript, one clip at a time.
    fn deliver(&self, text: String, ctx: &AppContext) -> Outcome {
        let _inject = self.inject_lock.lock().expect("inject lock");
        match self.ports.deliver(text, ctx) {
            Ok(()) => Outcome::Delivered,
            Err(e) => {
                warn!(error = %e, "injection failed");
                Outcome::Failed {
                    kind: "inject_error",
                    message: e.to_string(),
                    notify: Some("Dictation paste failed"),
                }
            }
        }
    }
}

/// The production [`Ports`]: thin wrappers over the real capture, STT client,
/// pipeline + injector, `notify-send` and transcript log.
struct Desktop {
    cfg: Config,
    recorder: Recorder,
    stt: SttClient,
    injector: Injector,
    /// Resolved display server: drives the focused-window probe.
    server: DisplayServer,
    /// Whether notifications show at all (see [`should_notify`]).
    notify: bool,
}

impl Ports for Desktop {
    fn start_capture(&self, events: CaptureEvents) -> std::io::Result<Box<dyn Capture>> {
        let capture = self
            .recorder
            .start(events.rms, events.too_long, events.silence)?;
        Ok(Box::new(PwCapture(capture)))
    }

    fn transcribe(&self, samples: &[i16]) -> Result<Transcription, SttError> {
        self.stt.transcribe(samples)
    }

    fn focused_window(&self) -> Option<String> {
        pipeline::active_window_class(self.server)
    }

    fn deliver(&self, text: String, ctx: &AppContext) -> Result<(), InjectError> {
        // Corrections, snippets, then the selected AI workflow. Never fails: LLM
        // errors fall back to the raw transcript inside `process`.
        let text = pipeline::process(text, &self.cfg, ctx, self.injector.backend());
        info!(chars = text.len(), "injecting transcript");
        self.injector.inject(&text)
    }

    fn notify(&self, summary: &str, body: &str) {
        if self.notify {
            notify(summary, body);
        }
    }

    fn log_transcript(&self, transcription: &Transcription, drop_reason: Option<&str>) {
        log_transcript(transcription, drop_reason);
    }

    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(job);
    }
}

/// The production [`Capture`]: a `pw-record` capture.
struct PwCapture(audio::Capture);

impl Capture for PwCapture {
    fn auto_stopped(&self) -> bool {
        self.0.auto_stopped()
    }

    fn finish(self: Box<Self>, min_samples: usize) -> Option<Vec<i16>> {
        self.0.finish(min_samples)
    }

    fn cancel(self: Box<Self>) {
        self.0.cancel();
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
/// under `$XDG_STATE_HOME/whispy`, for offline calibration of the filter. The file
/// holds dictated text, so it is capped: past [`config::TRANSCRIPTS_MAX_BYTES`] it
/// rotates to `transcripts.jsonl.1`, replacing the older generation.
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
    if let Err(e) = config::rotate_if_larger(&path, config::TRANSCRIPTS_MAX_BYTES) {
        warn!(error = %e, "failed to rotate transcript log");
    }
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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::JoinHandle;

    use whispy_common::{State, StateSnapshot};

    use super::*;
    use crate::state::StatePublisher;

    /// Samples in a clip long enough to keep (the default minimum is 400 ms).
    const LONG_CLIP: usize = 16_000;

    /// One capture handed out by [`Fake`]; the test fires its events.
    #[derive(Default)]
    struct FakeCapture {
        events: Mutex<Option<CaptureEvents>>,
        samples: usize,
        auto_stopped: AtomicBool,
        finished: AtomicUsize,
        cancelled: AtomicUsize,
    }

    impl FakeCapture {
        fn take_events(&self) -> CaptureEvents {
            self.events.lock().unwrap().take().expect("events")
        }

        /// What the reader does at `max_clip_secs`: flag, then report.
        fn overrun(&self) {
            self.auto_stopped.store(true, Ordering::Relaxed);
            (self.take_events().too_long)();
        }

        fn silence(&self) {
            (self.take_events().silence)();
        }

        fn closes(&self) -> usize {
            self.finished.load(Ordering::Relaxed) + self.cancelled.load(Ordering::Relaxed)
        }
    }

    struct CaptureHandle(Arc<FakeCapture>);

    impl Capture for CaptureHandle {
        fn auto_stopped(&self) -> bool {
            self.0.auto_stopped.load(Ordering::Relaxed)
        }

        fn finish(self: Box<Self>, min_samples: usize) -> Option<Vec<i16>> {
            self.0.finished.fetch_add(1, Ordering::Relaxed);
            (self.0.samples >= min_samples).then(|| vec![0; self.0.samples])
        }

        fn cancel(self: Box<Self>) {
            self.0.cancelled.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// In-memory ports: scripted capture length, transcripts and inject errors;
    /// records what the session did to the outside world.
    #[derive(Default)]
    struct Fake {
        clip_samples: Mutex<Option<usize>>,
        captures: Mutex<Vec<Arc<FakeCapture>>>,
        transcripts: Mutex<VecDeque<Result<Transcription, SttError>>>,
        transcribed: AtomicUsize,
        inject_error: Mutex<Option<InjectError>>,
        delivered: Mutex<Vec<(String, Option<String>)>>,
        notified: Mutex<Vec<String>>,
        logged: Mutex<Vec<Option<String>>>,
        threads: Mutex<Vec<JoinHandle<()>>>,
    }

    impl Fake {
        fn capture(&self, n: usize) -> Arc<FakeCapture> {
            Arc::clone(&self.captures.lock().unwrap()[n])
        }

        fn script(&self, reply: Result<Transcription, SttError>) {
            self.transcripts.lock().unwrap().push_back(reply);
        }

        /// Join every thread the session spawned (and any they spawned).
        fn settle(&self) {
            loop {
                let next = self.threads.lock().unwrap().pop();
                match next {
                    Some(handle) => handle.join().expect("session thread"),
                    None => break,
                }
            }
        }

        fn delivered(&self) -> Vec<String> {
            let delivered = self.delivered.lock().unwrap();
            delivered.iter().map(|(text, _)| text.clone()).collect()
        }

        fn notified(&self) -> Vec<String> {
            self.notified.lock().unwrap().clone()
        }

        fn logged(&self) -> Vec<Option<String>> {
            self.logged.lock().unwrap().clone()
        }
    }

    impl Ports for Arc<Fake> {
        fn start_capture(&self, events: CaptureEvents) -> std::io::Result<Box<dyn Capture>> {
            let samples = self.clip_samples.lock().unwrap().unwrap_or(LONG_CLIP);
            let capture = Arc::new(FakeCapture {
                events: Mutex::new(Some(events)),
                samples,
                ..FakeCapture::default()
            });
            self.captures.lock().unwrap().push(Arc::clone(&capture));
            Ok(Box::new(CaptureHandle(capture)))
        }

        fn transcribe(&self, _samples: &[i16]) -> Result<Transcription, SttError> {
            self.transcribed.fetch_add(1, Ordering::Relaxed);
            let scripted = self.transcripts.lock().unwrap().pop_front();
            scripted.unwrap_or_else(|| Ok(said("hello world")))
        }

        fn focused_window(&self) -> Option<String> {
            Some("kitty".to_string())
        }

        fn deliver(&self, text: String, ctx: &AppContext) -> Result<(), InjectError> {
            if let Some(e) = self.inject_error.lock().unwrap().take() {
                return Err(e);
            }
            let class = ctx.window_class.clone();
            self.delivered.lock().unwrap().push((text, class));
            Ok(())
        }

        fn notify(&self, summary: &str, _body: &str) {
            self.notified.lock().unwrap().push(summary.to_string());
        }

        fn log_transcript(&self, _t: &Transcription, drop_reason: Option<&str>) {
            self.logged
                .lock()
                .unwrap()
                .push(drop_reason.map(str::to_string));
        }

        fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
            self.threads.lock().unwrap().push(std::thread::spawn(job));
        }
    }

    /// A confident transcript of `text`.
    fn said(text: &str) -> Transcription {
        Transcription {
            text: text.to_string(),
            avg_logprob: -0.1,
            no_speech_prob: 0.01,
            language: Some("en".to_string()),
        }
    }

    /// A session over fake ports, publishing to a per-test state file.
    struct Harness {
        app: Arc<App>,
        fake: Arc<Fake>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("whispy-app-{}-{name}", std::process::id()));
            let publisher = Arc::new(StatePublisher::new(dir.join("state.json")));
            let status = Status::new(Arc::new(Mutex::new(StateSnapshot::default())), publisher, 0);
            let cfg: Config =
                toml::from_str(config::default_config_toml()).expect("defaults parse");
            let blacklist = Hallucinations::from_phrases(vec!["thank you".to_string()]);
            let fake = Arc::new(Fake::default());
            let ports = Box::new(Arc::clone(&fake));
            let app = Arc::new(App::with_ports(cfg, status, blacklist, ports));
            Self { app, fake, dir }
        }

        fn send(&self, cmd: Cmd) -> Resp {
            self.app.handle(cmd)
        }

        fn start(&self) {
            assert_ok(&self.send(Cmd::Start { workflow: None }));
        }

        fn stop(&self) {
            assert_ok(&self.send(Cmd::Stop));
        }

        fn state(&self) -> StateSnapshot {
            self.send(Cmd::Status)
                .snapshot
                .expect("status carries a snapshot")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn assert_ok(resp: &Resp) {
        assert!(resp.ok, "expected ok, got {resp:?}");
    }

    fn assert_err(resp: &Resp, message: &str) {
        assert!(!resp.ok, "expected error {message:?}, got {resp:?}");
        assert_eq!(resp.error.as_deref(), Some(message));
    }

    #[test]
    fn dictation_delivers_into_the_window_focused_at_start() {
        let h = Harness::new("happy");
        h.start();
        assert_eq!(h.state().state, State::Recording);

        h.stop();
        h.fake.settle();

        let delivered = h.fake.delivered.lock().unwrap().clone();
        assert_eq!(
            delivered,
            [("hello world".to_string(), Some("kitty".to_string()))]
        );
        assert_eq!(h.fake.logged(), [None]);
        assert_eq!(h.state().state, State::Success);
        assert!(h.fake.notified().is_empty(), "nothing expected");
    }

    #[test]
    fn a_too_short_clip_goes_back_to_idle_silently() {
        let h = Harness::new("short");
        *h.fake.clip_samples.lock().unwrap() = Some(10);
        h.start();
        h.stop();
        h.fake.settle();

        assert_eq!(h.state().state, State::Idle);
        assert_eq!(h.fake.transcribed.load(Ordering::Relaxed), 0);
        assert!(h.fake.delivered().is_empty(), "nothing expected");
        assert!(h.fake.logged().is_empty(), "nothing expected");
    }

    #[test]
    fn a_filtered_transcript_reports_its_drop_reason_and_is_not_delivered() {
        let h = Harness::new("drop");
        h.fake.script(Ok(said("Thank you.")));
        h.start();
        h.stop();
        h.fake.settle();

        let snap = h.state();
        assert_eq!(snap.state, State::Error);
        assert_eq!(snap.error_kind.as_deref(), Some("hallucination"));
        assert!(h.fake.delivered().is_empty(), "nothing expected");
        assert_eq!(h.fake.logged(), [Some("hallucination".to_string())]);
        assert!(h.fake.notified().is_empty(), "filter drops are routine");
    }

    #[test]
    fn an_stt_failure_reports_stt_error_and_notifies() {
        let h = Harness::new("stt-error");
        h.fake
            .script(Err(SttError::Http("connection refused".to_string())));
        h.start();
        h.stop();
        h.fake.settle();

        let snap = h.state();
        assert_eq!(snap.state, State::Error);
        assert_eq!(snap.error_kind.as_deref(), Some("stt_error"));
        assert_eq!(h.fake.notified(), ["Dictation failed"]);
        assert!(h.fake.delivered().is_empty(), "nothing expected");
        assert!(h.fake.logged().is_empty(), "nothing expected");
    }

    #[test]
    fn an_inject_failure_reports_inject_error_and_notifies() {
        let h = Harness::new("inject-error");
        *h.fake.inject_error.lock().unwrap() = Some(InjectError::Paste("no ydotool".into()));
        h.start();
        h.stop();
        h.fake.settle();

        let snap = h.state();
        assert_eq!(snap.state, State::Error);
        assert_eq!(snap.error_kind.as_deref(), Some("inject_error"));
        assert_eq!(h.fake.notified(), ["Dictation paste failed"]);
    }

    #[test]
    fn an_overrun_recording_is_reclaimed_by_the_next_start() {
        let h = Harness::new("too-long");
        h.start();
        h.fake.capture(0).overrun();

        let snap = h.state();
        assert_eq!(snap.state, State::Error);
        assert_eq!(snap.error_kind.as_deref(), Some("too_long"));
        assert_eq!(h.fake.notified(), ["Dictation too long"]);

        // The stale capture is torn down and a fresh one starts.
        h.start();
        assert_eq!(h.fake.capture(0).cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(h.state().state, State::Recording);

        h.stop();
        h.fake.settle();
        assert_eq!(h.fake.capture(1).finished.load(Ordering::Relaxed), 1);
        assert_eq!(h.fake.delivered(), ["hello world"]);
    }

    #[test]
    fn stopping_an_overrun_recording_discards_it() {
        let h = Harness::new("too-long-stop");
        h.start();
        h.fake.capture(0).overrun();
        h.stop();
        h.fake.settle();

        assert_eq!(h.fake.capture(0).cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(h.fake.transcribed.load(Ordering::Relaxed), 0);
        assert_eq!(h.state().error_kind.as_deref(), Some("too_long"));
    }

    #[test]
    fn trailing_silence_finishes_the_recording() {
        let h = Harness::new("silence");
        h.start();
        h.fake.capture(0).silence();
        h.fake.settle();

        assert_eq!(h.fake.delivered(), ["hello world"]);
        assert_eq!(h.state().state, State::Success);
        assert_err(&h.send(Cmd::Stop), "not recording");
    }

    #[test]
    fn auto_stop_racing_an_explicit_stop_finishes_exactly_once() {
        for round in 0..50 {
            let h = Harness::new(&format!("race-{round}"));
            h.start();
            let capture = h.fake.capture(0);
            let silence = std::thread::spawn(move || capture.silence());
            // Whichever side wins, the other finds nothing to finish.
            let stopped = h.send(Cmd::Stop);
            silence.join().unwrap();
            h.fake.settle();

            assert!(
                stopped.ok || stopped.error.as_deref() == Some("not recording"),
                "{stopped:?}"
            );
            assert_eq!(h.fake.capture(0).closes(), 1);
            assert_eq!(h.fake.transcribed.load(Ordering::Relaxed), 1);
            assert_eq!(h.fake.delivered(), ["hello world"]);
        }
    }

    #[test]
    fn stop_without_a_recording_is_an_error() {
        let h = Harness::new("not-recording");
        assert_err(&h.send(Cmd::Stop), "not recording");
    }

    #[test]
    fn start_while_recording_is_an_error() {
        let h = Harness::new("already");
        h.start();
        assert_err(&h.send(Cmd::Start { workflow: None }), "already recording");
        assert_eq!(h.fake.captures.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancel_discards_the_recording() {
        let h = Harness::new("cancel");
        h.start();
        assert_ok(&h.send(Cmd::Cancel));
        h.fake.settle();

        assert_eq!(h.fake.capture(0).cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(h.fake.transcribed.load(Ordering::Relaxed), 0);
        assert_eq!(h.state().state, State::Idle);
    }

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
