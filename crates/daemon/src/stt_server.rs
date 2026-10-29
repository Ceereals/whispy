//! Supervises the STT server child process: whisper.cpp's whisper-server, or
//! `whispy-stt` (transcribe.cpp/Parakeet) when `stt.engine = "parakeet"`. Both take
//! the same command line and serve the same `/inference` API, so the rest of the
//! daemon doesn't care which one runs.
//!
//! [`SttSupervisor::start`] rotates the server's log (`<server name>.log` in the
//! state dir), spawns the engine's server binary with the configured model, and
//! blocks until its HTTP port accepts connections. A monitor thread then owns the
//! child: if it dies, the monitor respawns it, backing off exponentially while
//! restarts keep failing (a missing binary must not be retried every two seconds
//! forever). Without the monitor the daemon would keep running but every
//! transcription would fail: systemd only restarts us if *we* exit, not the child.
//! [`SttSupervisor::stop`] (or dropping the supervisor) ends the monitor and kills
//! the child.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use crate::config::{self, Stt};
use crate::engine::Engine;

/// The resident STT server, kept alive by a monitor thread until [`Self::stop`].
#[derive(Debug)]
pub struct SttSupervisor {
    /// Set by [`Self::stop`]; the monitor also honours the daemon's shutdown flag.
    stop: Arc<AtomicBool>,
    /// The monitor thread, which owns the child. `None` once joined.
    monitor: Option<JoinHandle<()>>,
}

impl SttSupervisor {
    /// Start the configured engine's server and block until it is ready (the model
    /// is loaded and the port accepts connections), then keep it alive in the
    /// background. Fails if the binary or model is missing, or if the server exits
    /// or stays deaf for 60 s; the error names the server and its log. Setting
    /// `shutdown` aborts the wait (an error) or, once started, stops the monitor.
    pub fn start(cfg: &Stt, state_dir: &Path, shutdown: Arc<AtomicBool>) -> std::io::Result<Self> {
        Self::start_with(cfg, state_dir, shutdown, Timing::DAEMON)
    }

    /// [`Self::start`] with explicit timing, so tests run in milliseconds.
    fn start_with(
        cfg: &Stt,
        state_dir: &Path,
        shutdown: Arc<AtomicBool>,
        timing: Timing,
    ) -> std::io::Result<Self> {
        let name = cfg.engine.server_name();
        let log_path = log_path(state_dir, cfg.engine);
        std::fs::create_dir_all(state_dir)?;
        // Rotate only here, at daemon start: in-process restarts append, so the
        // output of a crash survives the monitor respawning the server.
        if let Err(e) = config::rotate_if_larger(&log_path, config::STT_LOG_MAX_BYTES) {
            warn!(error = %e, log = %log_path.display(), "could not rotate the {name} log");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let halt = Halt {
            shutdown,
            stop: Arc::clone(&stop),
        };
        let mut child = spawn(cfg, &log_path)?;
        if !wait_ready(&mut child, cfg, timing, &halt) {
            terminate(&mut child, name);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{name} did not become ready (see {} for details)",
                    log_path.display()
                ),
            ));
        }
        info!("{name} ready");

        let monitor = Monitor {
            cfg: cfg.clone(),
            log_path,
            timing,
            halt,
            child: Some(child),
            backoff: Backoff::new(timing.poll, timing.backoff_cap),
        };
        Ok(Self {
            stop,
            monitor: Some(std::thread::spawn(move || monitor.run())),
        })
    }

    /// Stop the monitor and kill the server. Returns promptly even mid-restart,
    /// and whether or not the daemon's shutdown flag is set.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for SttSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(monitor) = self.monitor.take() {
            monitor.join().ok();
        }
    }
}

/// The server's log in the state dir: `<server name>.log`, so whisper-server and
/// whispy-stt each keep their own.
fn log_path(state_dir: &Path, engine: Engine) -> PathBuf {
    state_dir.join(format!("{}.log", engine.server_name()))
}

/// The supervisor's clocks. Fixed for the daemon; tests shrink them.
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// How often the monitor checks the child is alive; also the first restart delay.
    poll: Duration,
    /// Longest wait between restart attempts while they keep failing.
    backoff_cap: Duration,
    /// How long a (re)started server may take to load the model and listen.
    ready_timeout: Duration,
    /// Pause between connection attempts while waiting for readiness.
    ready_poll: Duration,
}

impl Timing {
    /// What the daemon runs with.
    const DAEMON: Self = Self {
        poll: Duration::from_secs(2),
        backoff_cap: Duration::from_secs(60),
        ready_timeout: Duration::from_secs(60),
        ready_poll: Duration::from_millis(200),
    };
}

/// Longest uninterrupted sleep, so a stop request is noticed quickly.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

/// Either reason to wind down: the daemon's shutdown flag (SIGTERM/SIGINT) or
/// [`SttSupervisor::stop`].
struct Halt {
    shutdown: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Halt {
    fn requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed)
    }

    /// Sleep for `duration` in short slices; `false` if a halt cut it short.
    fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.requested() {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(SLEEP_SLICE));
        }
    }
}

/// Exponential delay between restart attempts: the poll interval after the first
/// failure, doubling with each consecutive one up to the cap.
#[derive(Debug)]
struct Backoff {
    base: Duration,
    cap: Duration,
    failures: u32,
}

impl Backoff {
    fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            failures: 0,
        }
    }

    /// How long to wait before the next check or attempt.
    fn delay(&self) -> Duration {
        // 2^16 × base is past any sane cap; clamping the shift avoids overflow.
        let factor = 1u32 << self.failures.saturating_sub(1).min(16);
        self.base.saturating_mul(factor).min(self.cap)
    }

    /// Record a failed restart; returns the delay before the next attempt.
    fn fail(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.delay()
    }

    /// The server came up: back to the normal poll interval.
    fn reset(&mut self) {
        self.failures = 0;
    }
}

/// The monitor thread's state. It owns the child outright, so no lock is held
/// across a restart's readiness wait.
struct Monitor {
    cfg: Stt,
    log_path: PathBuf,
    timing: Timing,
    halt: Halt,
    /// `None` after a failed spawn (missing binary or model).
    child: Option<Child>,
    backoff: Backoff,
}

impl Monitor {
    /// Poll the child and restart it when it dies, until a halt; then kill it.
    fn run(mut self) {
        while self.halt.sleep(self.backoff.delay()) {
            let alive = self
                .child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
            if !alive {
                self.restart();
            }
        }
        if let Some(child) = self.child.as_mut() {
            terminate(child, self.cfg.engine.server_name());
        }
    }

    /// One restart attempt: reap the dead child, spawn a fresh one, wait for it.
    fn restart(&mut self) {
        let name = self.cfg.engine.server_name();
        warn!("{name} is not running; restarting");
        if let Some(mut dead) = self.child.take() {
            dead.wait().ok();
        }
        match spawn(&self.cfg, &self.log_path) {
            Ok(mut child) => {
                let ready = wait_ready(&mut child, &self.cfg, self.timing, &self.halt);
                self.child = Some(child);
                if ready {
                    self.backoff.reset();
                    info!("{name} restarted");
                } else if !self.halt.requested() {
                    let retry = self.backoff.fail();
                    warn!(
                        retry_in_secs = retry.as_secs(),
                        log = %self.log_path.display(),
                        "{name} restart did not become ready"
                    );
                }
            }
            Err(e) => {
                let retry = self.backoff.fail();
                error!(error = %e, retry_in_secs = retry.as_secs(), "failed to restart {name}");
            }
        }
    }
}

/// Launch one STT server process, its stdout/stderr appended to `log_path`.
fn spawn(cfg: &Stt, log_path: &Path) -> std::io::Result<Child> {
    let bin = cfg.server_bin_path();
    let model = cfg.model_file();
    let name = cfg.engine.server_name();
    if !bin.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{name} binary not found at {}", bin.display()),
        ));
    }
    if !model.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("model file not found at {}", model.display()),
        ));
    }

    // Append, don't truncate: the monitor restarts a crashed server within
    // seconds, and truncating would wipe the output explaining the crash.
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(log, "--- whispy: starting {name} ---")?;
    let errlog = log.try_clone()?;
    info!(bin = %bin.display(), model = %model.display(), port = cfg.port, "spawning {name}");
    let mut cmd = Command::new(&bin);
    cmd.arg("-m")
        .arg(&model)
        .arg("-l")
        .arg(&cfg.language)
        .arg("--host")
        .arg(&cfg.host)
        .arg("--port")
        .arg(cfg.port.to_string())
        .args(cfg.engine.extra_server_args(cfg));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errlog))
        .spawn()
}

/// Block until the HTTP port accepts a TCP connection, or the ready timeout
/// elapses. Returns early on a halt or if the child exits (a bad model or flag
/// kills it at once; no point waiting out the timeout). The child loads the model
/// before it starts listening, so a successful connect means it is ready.
fn wait_ready(child: &mut Child, cfg: &Stt, timing: Timing, halt: &Halt) -> bool {
    // Resolve rather than parse, so `localhost` and bare IPv6 hosts work.
    let addrs: Vec<SocketAddr> = match (cfg.host.as_str(), cfg.port).to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            warn!(host = %cfg.host, error = %e, "cannot resolve the STT server host");
            return false;
        }
    };
    let deadline = Instant::now() + timing.ready_timeout;
    while Instant::now() < deadline {
        if halt.requested() || !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        if addrs
            .iter()
            .any(|a| TcpStream::connect_timeout(a, Duration::from_millis(500)).is_ok())
        {
            return true;
        }
        std::thread::sleep(timing.ready_poll);
    }
    false
}

/// Kill the child and reap it.
fn terminate(child: &mut Child, name: &str) {
    info!("terminating {name}");
    if let Err(e) = child.kill() {
        warn!(error = %e, "failed to kill {name}");
    }
    child.wait().ok();
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::Config;

    /// Millisecond clocks; `ready_timeout` is generous so a test that expects
    /// readiness never flakes, and those expecting failure return early anyway.
    const FAST: Timing = Timing {
        poll: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(160),
        ready_timeout: Duration::from_secs(30),
        ready_poll: Duration::from_millis(10),
    };

    /// A scratch dir holding a fake server script, a fake model, and the state dir.
    struct Fake {
        dir: PathBuf,
        cfg: Stt,
    }

    impl Fake {
        /// `body` is the fake server's shell script. Every launch first appends its
        /// pid to `<dir>/pids`, so tests can count (and kill) launches.
        fn new(tag: &str, body: &str, port: u16) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "whispy-stt-supervisor-{tag}-{}",
                std::process::id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("server.sh");
            let script = format!(
                "#!/bin/sh\necho $$ >> '{}'\n{body}\n",
                dir.join("pids").display()
            );
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let model = dir.join("model.bin");
            std::fs::write(&model, b"").unwrap();

            let mut cfg = Config::load(None).expect("defaults load").stt;
            cfg.engine = Engine::Whisper;
            cfg.server_bin = bin.display().to_string();
            cfg.model_path = model.display().to_string();
            cfg.host = "127.0.0.1".to_string();
            cfg.port = port;
            Self { dir, cfg }
        }

        fn state_dir(&self) -> PathBuf {
            self.dir.join("state")
        }

        fn start(&self, shutdown: &Arc<AtomicBool>) -> std::io::Result<SttSupervisor> {
            SttSupervisor::start_with(&self.cfg, &self.state_dir(), Arc::clone(shutdown), FAST)
        }

        fn pids(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("pids"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Wait (bounded) until the fake has been launched `n` times.
        fn await_launches(&self, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let pids = self.pids();
                if pids.len() >= n {
                    return pids;
                }
                assert!(Instant::now() < deadline, "only {} launches", pids.len());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// A port nothing listens on (bound once to pick it, then released).
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A listener standing in for the server's HTTP port; the fake child just sleeps.
    fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    /// Is process `pid` still running (`kill -0`)?
    fn running(pid: &str) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(pid)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    fn kill(pid: &str) {
        let ok = Command::new("kill").arg("-9").arg(pid).status().unwrap();
        assert!(ok.success(), "kill {pid}");
    }

    fn no_shutdown() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn log_is_named_after_the_engine_server() {
        let dir = Path::new("/state");
        assert_eq!(
            log_path(dir, Engine::Whisper),
            PathBuf::from("/state/whisper-server.log")
        );
        assert_eq!(
            log_path(dir, Engine::Parakeet),
            PathBuf::from("/state/whispy-stt.log")
        );
    }

    #[test]
    fn a_listening_server_becomes_ready_and_stop_kills_it() {
        let (_port, port) = listener();
        let fake = Fake::new("ready", "exec sleep 30", port);
        let sup = fake.start(&no_shutdown()).expect("ready");
        let pid = fake.await_launches(1).remove(0);
        let log = std::fs::read_to_string(fake.state_dir().join("whisper-server.log")).unwrap();
        assert!(
            log.contains("--- whispy: starting whisper-server ---"),
            "{log}"
        );

        // The shutdown flag is never set: `stop` alone must end the monitor.
        sup.stop();
        assert!(!running(&pid), "child {pid} survived stop");
    }

    #[test]
    fn a_server_that_exits_fails_start_promptly_naming_the_log() {
        // A server that exits at once (corrupt model, bad flag) must not stall
        // startup for the whole ready timeout. `localhost` must resolve, not panic.
        let mut fake = Fake::new("exits", "exit 1", free_port());
        fake.cfg.host = "localhost".to_string();
        let started = Instant::now();
        let err = fake.start(&no_shutdown()).expect_err("server exits");
        assert!(started.elapsed() < Duration::from_secs(5));
        let msg = err.to_string();
        let log = fake.state_dir().join("whisper-server.log");
        assert!(msg.contains("whisper-server did not become ready"), "{msg}");
        assert!(msg.contains(&log.display().to_string()), "{msg}");
    }

    #[test]
    fn shutdown_during_the_startup_wait_aborts_start() {
        // The child lives but never listens; SIGTERM must not wait out the timeout.
        let fake = Fake::new("startup-shutdown", "exec sleep 30", free_port());
        let shutdown = no_shutdown();
        let flag = Arc::clone(&shutdown);
        let signal = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let err = fake.start(&shutdown).expect_err("aborted");
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
        signal.join().unwrap();
        let pid = fake.await_launches(1).remove(0);
        assert!(!running(&pid), "child {pid} survived the aborted start");
    }

    #[test]
    fn a_server_killed_after_ready_is_restarted() {
        let (_port, port) = listener();
        let fake = Fake::new("restart", "exec sleep 30", port);
        let sup = fake.start(&no_shutdown()).expect("ready");
        let first = fake.await_launches(1).remove(0);
        kill(&first);
        let pids = fake.await_launches(2);
        assert_ne!(pids[1], first);
        let log = std::fs::read_to_string(fake.state_dir().join("whisper-server.log")).unwrap();
        assert_eq!(
            log.matches("--- whispy: starting").count(),
            2,
            "restart appends"
        );
        sup.stop();
    }

    #[test]
    fn consecutive_failed_restarts_back_off_up_to_the_cap() {
        let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        assert_eq!(backoff.delay(), Duration::from_secs(2), "plain poll");
        let delays: Vec<u64> = (0..8).map(|_| backoff.fail().as_secs()).collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
        for _ in 0..100 {
            backoff.fail();
        }
        assert_eq!(backoff.delay(), Duration::from_secs(60), "no overflow");
        backoff.reset();
        assert_eq!(backoff.delay(), Duration::from_secs(2), "reset after ready");
    }

    #[test]
    fn stop_during_a_restart_readiness_wait_returns_promptly() {
        // Ready once on the test's listener, then the port goes deaf: the restarted
        // child lives but never listens, so the monitor sits in a 60 s ready wait.
        let (port_listener, port) = listener();
        let fake = Fake::new("stop-wait", "exec sleep 30", port);
        let timing = Timing {
            ready_timeout: Duration::from_secs(60),
            ..FAST
        };
        let sup = SttSupervisor::start_with(&fake.cfg, &fake.state_dir(), no_shutdown(), timing)
            .expect("ready");
        drop(port_listener);
        let first = fake.await_launches(1).remove(0);
        kill(&first);
        let second = fake.await_launches(2).remove(1);
        std::thread::sleep(Duration::from_millis(100)); // well into the wait

        let started = Instant::now();
        sup.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!running(&second), "restarted child {second} survived stop");
    }

    #[test]
    fn shutdown_during_a_backoff_wait_ends_the_monitor_promptly() {
        // Ready once (the first launch sleeps), then the port goes deaf and every
        // relaunch exits at once, so the monitor ends up sleeping out a long backoff.
        let (port_listener, port) = listener();
        let marker = std::env::temp_dir().join(format!(
            "whispy-stt-supervisor-backoff-marker-{}",
            std::process::id()
        ));
        std::fs::remove_file(&marker).ok();
        let body = format!(
            "if [ -e '{m}' ]; then exit 1; fi\ntouch '{m}'\nexec sleep 30",
            m = marker.display()
        );
        let fake = Fake::new("backoff", &body, port);
        let timing = Timing {
            poll: Duration::from_millis(200),
            backoff_cap: Duration::from_secs(60),
            ..FAST
        };
        let shutdown = no_shutdown();
        let sup =
            SttSupervisor::start_with(&fake.cfg, &fake.state_dir(), Arc::clone(&shutdown), timing)
                .expect("ready");
        drop(port_listener);
        let first = fake.await_launches(1).remove(0);
        kill(&first);
        // Failed relaunches wait 0.2, 0.4, 0.8 s; after the 4th failure (5th
        // launch) the monitor sleeps 1.6 s before trying again.
        fake.await_launches(5);

        // As at SIGTERM: the daemon's flag goes up, then main stops the supervisor.
        let started = Instant::now();
        shutdown.store(true, Ordering::Relaxed);
        sup.stop();
        assert!(started.elapsed() < Duration::from_millis(500));
        std::fs::remove_file(&marker).ok();
    }
}
