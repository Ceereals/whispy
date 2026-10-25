//! Subprocess helpers with deadlines.
//!
//! Every external tool the daemon drives at runtime (`wl-paste`, `hyprctl`,
//! `ydotool`, `wtype`, ...) can wedge: a clipboard owner that stops answering, a
//! compositor IPC that never replies. Without a deadline the calling thread hangs
//! with it, and the published state sticks (e.g. at `transcribing`) with nothing
//! pasted. These wrappers kill the child and return [`io::ErrorKind::TimedOut`]
//! once the deadline passes.

use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Clipboard reads and writes (`wl-copy`/`wl-paste`, `xclip`/`xsel`).
pub const CLIPBOARD: Duration = Duration::from_secs(2);
/// The paste keystroke (`ydotool key`).
pub const KEYS: Duration = Duration::from_secs(5);
/// Typing a whole transcript (`wtype`, `xdotool type` at ~12 ms per character).
pub const TYPING: Duration = Duration::from_secs(30);
/// Quick probes: focused-window class, `date`, `notify-send`.
pub const PROBE: Duration = Duration::from_secs(2);

/// How often to poll a running child for exit.
const POLL: Duration = Duration::from_millis(5);
/// After the child exits, how long its output pipes may stay open (a forked
/// grandchild can inherit them) before we give up on reading them.
const PIPE_GRACE: Duration = Duration::from_millis(200);

/// Run `cmd` with stdin closed, capturing stdout and stderr, within `timeout`.
pub fn output(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = wait_until(&mut child, deadline)?;
    Ok(Output {
        status,
        stdout: collect(&stdout, deadline)?,
        stderr: collect(&stderr, deadline)?,
    })
}

/// Run `cmd` with the stdio the caller configured and wait for it within `timeout`.
pub fn status(cmd: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    wait_until(&mut child, Instant::now() + timeout)
}

/// Run `cmd` with `input` written to its stdin (then closed), within `timeout`.
pub fn with_stdin(cmd: &mut Command, input: &[u8], timeout: Duration) -> io::Result<ExitStatus> {
    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("child stdin unavailable"))?;
    // Write from a thread: a child that never reads would block `write_all` on a
    // full pipe forever. Dropping `stdin` at the end closes it so the child sees EOF.
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let status = wait_until(&mut child, Instant::now() + timeout)?;
    match writer.join() {
        Ok(Ok(())) => Ok(status),
        // The child exited without reading everything; its status says how it went.
        Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => Ok(status),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::other("stdin writer panicked")),
    }
}

/// Poll `child` until it exits; past `deadline`, kill and reap it and time out.
fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        std::thread::sleep(POLL);
    }
}

/// Read a pipe to EOF on its own thread, so a full pipe can't stall the child.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = match pipe {
            Some(mut p) => p.read_to_end(&mut buf).map(|_| buf),
            None => Ok(buf),
        };
        let _ = tx.send(result);
    });
    rx
}

/// Collect a drained pipe once the child has exited.
fn collect(rx: &mpsc::Receiver<io::Result<Vec<u8>>>, deadline: Instant) -> io::Result<Vec<u8>> {
    let wait = deadline.saturating_duration_since(Instant::now()) + PIPE_GRACE;
    rx.recv_timeout(wait).unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "output pipe still open after exit",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_captures_stdout_and_status() {
        let out = output(
            Command::new("sh").args(["-c", "printf hello; exit 3"]),
            Duration::from_secs(5),
        )
        .expect("runs");
        assert_eq!(out.stdout, b"hello");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn output_kills_a_hung_child() {
        let started = Instant::now();
        let err = output(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_does_not_wait_on_a_grandchild_holding_the_pipe() {
        // The child exits at once but leaves a background process with stdout open.
        let started = Instant::now();
        let result = output(
            Command::new("sh").args(["-c", "sleep 30 & printf x"]),
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            result.expect_err("pipe held").kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn status_times_out() {
        let err = status(
            Command::new("sh")
                .args(["-c", "sleep 30"])
                .stdout(Stdio::null()),
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn with_stdin_feeds_input() {
        let ok = with_stdin(
            Command::new("sh").args(["-c", "[ \"$(cat)\" = whispy ]"]),
            b"whispy",
            Duration::from_secs(5),
        )
        .expect("runs");
        assert!(ok.success());
    }

    #[test]
    fn with_stdin_times_out_on_a_child_that_never_reads() {
        // 1 MiB overflows the pipe buffer, so an unthreaded write would block forever.
        let err = with_stdin(
            Command::new("sh").args(["-c", "sleep 30"]),
            &vec![b'x'; 1 << 20],
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
