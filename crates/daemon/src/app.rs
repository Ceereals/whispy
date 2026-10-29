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
//! paste one at a time, in order. Such an older clip still delivers, but never
//! flashes its outcome over the newer recording. `cancel` discards the recording
//! and every clip that has not started injecting: they paste and publish nothing.
//!
//! The session reaches the outside world only through [`Ports`]: the microphone,
//! the STT server, the focused-window probe, delivery (post-processing +
//! injection), desktop notifications, the transcript log and thread spawning.
//! [`LivePorts`] is the production adapter; the tests drive the whole lifecycle
//! through a fake. Published state goes through the concrete [`Status`].

use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use whispy_common::{Cmd, Resp};

use crate::audio::{self, Recorder};
use crate::config::Config;
use crate::desktop::{self, Desktop};
use crate::display::DisplayServer;
use crate::filter::{self, Decision, Hallucinations, Metrics};
use crate::inject::{InjectError, Injector};
use crate::pipeline::{AppContext, LiveEnv, TextPipeline};
use crate::state::Status;
use crate::stt::{SttClient, SttError, Transcription};
use crate::transcripts::{Record, TranscriptLog};

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
/// [`LivePorts`] in production, an in-memory fake in the tests.
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
    /// Identifies this recording and the clip it becomes (see [`Lifecycle`]).
    id: u64,
    capture: Box<dyn Capture>,
    ctx: AppContext,
}

/// A finished recording on its way to transcription and delivery.
struct Clip {
    id: u64,
    samples: Vec<i16>,
    ctx: AppContext,
}

/// The session's mutable state, under one lock.
#[derive(Default)]
struct Lifecycle {
    /// The live (or overrun) recording, if any.
    recording: Option<Recording>,
    /// The id the next recording gets. Ids only grow, so a larger id is newer.
    next_id: u64,
    /// Clips of recordings with a smaller id were cancelled: they neither
    /// deliver nor publish anything.
    cancelled_below: u64,
}

impl Lifecycle {
    /// Whether a recording is capturing right now (live, not overrun).
    fn is_recording(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|r| !r.capture.auto_stopped())
    }
}

/// How one clip ended.
enum Outcome {
    /// Injected into the focused window.
    Delivered,
    /// Cancelled before delivery: discarded without a trace.
    Cancelled,
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
    lifecycle: Mutex<Lifecycle>,
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
        pipeline: TextPipeline,
    ) -> Self {
        let desktop = desktop::connect(server);
        let ports = LivePorts {
            recorder: Recorder::new(cfg.audio.clone()),
            stt,
            injector: Injector::new(&cfg.injection, Arc::clone(&desktop)),
            pipeline,
            notify: cfg.ui.notify.should_notify(server),
            desktop,
        };
        Self::with_ports(cfg, status, blacklist, Box::new(ports))
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
            lifecycle: Mutex::new(Lifecycle::default()),
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
        let mut lifecycle = self.lifecycle.lock().expect("lifecycle lock");
        // An overrun recording (too_long, already reported) that no stop/cancel
        // reclaimed: tear it down and start fresh.
        if let Some(stale) = lifecycle.recording.take_if(|r| r.capture.auto_stopped()) {
            stale.capture.cancel();
        }
        if lifecycle.recording.is_some() {
            return Resp::err("already recording");
        }

        let id = lifecycle.next_id;
        lifecycle.next_id += 1;
        match self.ports.start_capture(self.capture_events(id)) {
            Ok(capture) => {
                self.status.recording(0.0);
                // Snapshot the workflow + focused window now: this is the target
                // the transcript will be post-processed for and pasted into.
                let ctx = AppContext {
                    workflow,
                    window_class: self.ports.focused_window(),
                };
                lifecycle.recording = Some(Recording { id, capture, ctx });
                info!("recording started");
                Resp::ok()
            }
            Err(e) => Resp::err(format!("failed to start capture: {e}")),
        }
    }

    /// The callbacks for recording `id`: meter, max-duration overrun, auto-stop.
    fn capture_events(self: &Arc<Self>, id: u64) -> CaptureEvents {
        let rms_status = self.status.clone();
        let too_long = Arc::clone(self);
        // The capture fires `silence` from its reader thread, and finishing
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
                silence.ports.spawn(Box::new(move || app.auto_finish(id)));
            }),
        }
    }

    fn stop(self: &Arc<Self>) -> Resp {
        let Some(recording) = self.take_recording(|_| true) else {
            return Resp::err("not recording");
        };
        if let Some(clip) = self.close(recording) {
            // Transcribe off-thread: the client returns now, the UI follows state.
            let app = Arc::clone(self);
            self.ports.spawn(Box::new(move || app.process(clip)));
        }
        Resp::ok()
    }

    /// Finish recording `id`, which auto-stopped on trailing silence, exactly as
    /// an explicit `stop` would. Runs on its own thread; a no-op if a stop or
    /// cancel already reclaimed that recording (even if a newer one is live).
    fn auto_finish(&self, id: u64) {
        if let Some(clip) = self
            .take_recording(|r| r.id == id)
            .and_then(|r| self.close(r))
        {
            self.process(clip);
        }
    }

    /// Discard the recording and every clip still in flight that has not
    /// started injecting.
    fn cancel(&self) -> Resp {
        let recording = {
            let mut lifecycle = self.lifecycle.lock().expect("lifecycle lock");
            lifecycle.cancelled_below = lifecycle.next_id;
            lifecycle.recording.take()
        };
        if let Some(recording) = recording {
            recording.capture.cancel();
        }
        self.status.idle();
        Resp::ok()
    }

    /// Take the recording out of its slot if `which` matches it, releasing the
    /// lock before the capture is joined.
    fn take_recording(&self, which: impl FnOnce(&mut Recording) -> bool) -> Option<Recording> {
        let mut lifecycle = self.lifecycle.lock().expect("lifecycle lock");
        lifecycle.recording.take_if(which)
    }

    /// Whether clip `id` was cancelled.
    fn cancelled(&self, id: u64) -> bool {
        id < self
            .lifecycle
            .lock()
            .expect("lifecycle lock")
            .cancelled_below
    }

    /// Publish a state on behalf of clip `id`, unless a newer recording is
    /// capturing: the clip still runs its course, but the UI keeps showing the
    /// recording. Returns false, publishing nothing, if the clip was cancelled.
    ///
    /// Publishes under the lifecycle lock, which `start` holds while it
    /// publishes `recording`, so a clip can never land after a newer start.
    fn report(&self, id: u64, publish: impl FnOnce(&Status)) -> bool {
        let lifecycle = self.lifecycle.lock().expect("lifecycle lock");
        if id < lifecycle.cancelled_below {
            return false;
        }
        if !lifecycle.is_recording() {
            publish(&self.status);
        }
        true
    }

    /// Stop a recording and hand off its clip. An overrun capture is discarded
    /// (`too_long` was already reported) and a too-short or speechless clip goes
    /// back to idle silently; neither yields a clip.
    fn close(&self, recording: Recording) -> Option<Clip> {
        let Recording { id, capture, ctx } = recording;
        if capture.auto_stopped() {
            capture.cancel();
            return None;
        }
        let Some(samples) = capture.finish(self.cfg.audio.min_samples()) else {
            self.report(id, Status::idle);
            return None;
        };
        self.report(id, Status::transcribing)
            .then_some(Clip { id, samples, ctx })
    }

    /// Transcribe, filter and deliver one clip, then publish how it ended.
    /// A cancelled clip publishes nothing, and a clip that ends while a newer
    /// recording is capturing delivers without flashing over it.
    fn process(&self, clip: Clip) {
        let id = clip.id;
        match self.outcome(clip) {
            Outcome::Cancelled => info!("clip cancelled; discarded"),
            Outcome::Delivered => {
                self.report(id, Status::success);
            }
            Outcome::Failed {
                kind,
                message,
                notify,
            } => {
                if self.report(id, |s| s.error(kind, &message))
                    && let Some(summary) = notify
                {
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
                self.deliver(text, &clip)
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

    /// Deliver an accepted transcript, one clip at a time. A clip cancelled
    /// before its turn is discarded instead.
    fn deliver(&self, text: String, clip: &Clip) -> Outcome {
        let _inject = self.inject_lock.lock().expect("inject lock");
        if self.cancelled(clip.id) {
            return Outcome::Cancelled;
        }
        match self.ports.deliver(text, &clip.ctx) {
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
/// the display server's [`Desktop`], the text pipeline + injector,
/// `notify-send` and the [`TranscriptLog`].
struct LivePorts {
    recorder: Recorder,
    stt: SttClient,
    /// The display server's window-class probe, clipboard and typing.
    desktop: Arc<dyn Desktop>,
    /// Post-transcription text pipeline, prepared at startup.
    pipeline: TextPipeline,
    injector: Injector,
    /// Whether notifications show at all (`ui.notify` for this display server,
    /// see [`NotifyMode::should_notify`]).
    ///
    /// [`NotifyMode::should_notify`]: crate::config::NotifyMode::should_notify
    notify: bool,
}

impl Ports for LivePorts {
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
        self.desktop.window_class()
    }

    fn deliver(&self, text: String, ctx: &AppContext) -> Result<(), InjectError> {
        // Corrections, snippets, then the selected AI workflow. Never fails: LLM
        // errors fall back to the raw transcript inside `process`.
        let env = LiveEnv::new(self.desktop.as_ref());
        let text = self.pipeline.process(text, ctx, &env);
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
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::JoinHandle;

    use whispy_common::{State, StateSnapshot};

    use super::*;
    use crate::config;
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
        /// When set, the next transcription blocks until the test sends.
        stt_gate: Mutex<Option<Receiver<()>>>,
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

        /// Make the next transcription wait for the returned sender.
        fn hold_stt(&self) -> Sender<()> {
            let (tx, rx) = mpsc::channel();
            *self.stt_gate.lock().unwrap() = Some(rx);
            tx
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
            let gate = self.stt_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.recv().expect("test releases the transcription");
            }
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
    fn cancel_discards_a_clip_that_is_still_transcribing() {
        // Regression: cancel published idle, but the in-flight clip still
        // pasted and flashed success.
        let h = Harness::new("cancel-in-flight");
        let release = h.fake.hold_stt();
        h.start();
        h.stop();
        assert_eq!(h.state().state, State::Transcribing);

        assert_ok(&h.send(Cmd::Cancel));
        release.send(()).unwrap();
        h.fake.settle();

        assert!(
            h.fake.delivered().is_empty(),
            "a cancelled clip must not paste"
        );
        assert_eq!(h.state().state, State::Idle);
        assert!(h.fake.notified().is_empty(), "nothing expected");
    }

    #[test]
    fn cancel_also_silences_a_failing_clip() {
        let h = Harness::new("cancel-failing");
        let release = h.fake.hold_stt();
        h.fake.script(Err(SttError::Http("timed out".to_string())));
        h.start();
        h.stop();
        assert_ok(&h.send(Cmd::Cancel));
        release.send(()).unwrap();
        h.fake.settle();

        assert_eq!(h.state().state, State::Idle);
        assert!(h.fake.notified().is_empty(), "nothing expected");
    }

    #[test]
    fn a_clip_started_after_a_cancel_still_delivers() {
        let h = Harness::new("cancel-then-dictate");
        h.start();
        assert_ok(&h.send(Cmd::Cancel));
        h.start();
        h.stop();
        h.fake.settle();

        assert_eq!(h.fake.delivered(), ["hello world"]);
        assert_eq!(h.state().state, State::Success);
    }

    #[test]
    fn an_older_clip_delivers_without_overwriting_a_newer_recording() {
        // Regression: the older clip's success flash replaced `Recording`.
        let h = Harness::new("stale-flash");
        let release = h.fake.hold_stt();
        h.start();
        h.stop();
        h.start(); // a new recording while the first clip transcribes
        release.send(()).unwrap();
        h.fake.settle();

        assert_eq!(
            h.fake.delivered(),
            ["hello world"],
            "the older clip still pastes"
        );
        assert_eq!(h.state().state, State::Recording);
    }

    #[test]
    fn an_older_clip_failure_does_not_overwrite_a_newer_recording() {
        let h = Harness::new("stale-error");
        let release = h.fake.hold_stt();
        h.fake.script(Ok(said("Thank you.")));
        h.start();
        h.stop();
        h.start();
        release.send(()).unwrap();
        h.fake.settle();

        assert_eq!(h.state().state, State::Recording);
    }

    #[test]
    fn a_late_auto_stop_leaves_the_next_recording_alone() {
        // Regression: the auto-stop thread of a recording that an explicit stop
        // already finished took whatever recording was live when it ran.
        let h = Harness::new("late-silence");
        h.start();
        h.stop();
        h.fake.settle();
        h.start();
        h.fake.capture(0).silence(); // the first capture's auto-stop, running late
        h.fake.settle();

        assert_eq!(h.fake.capture(1).closes(), 0);
        assert_eq!(h.state().state, State::Recording);
        h.stop();
        h.fake.settle();
        assert_eq!(h.fake.delivered(), ["hello world", "hello world"]);
    }
}
