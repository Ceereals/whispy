//! End-to-end IPC test: drive the real `whispy-client` binary against a stub
//! daemon (a Unix socket we control), asserting the line-based JSON framing
//! (`Cmd` in, `Resp` out) that `whispy-common` defines.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

/// A throwaway socket path under the temp dir, unique per test.
fn temp_socket(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "whispy-client-it-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("whispy.sock")
}

/// Spawn a stub daemon that handles `count` connections. For each, it reads one
/// command line and replies with the line returned by `respond(cmd_line)`.
/// Returns the join handle holding the commands it received.
fn stub_daemon<F>(
    listener: UnixListener,
    count: usize,
    respond: F,
) -> thread::JoinHandle<Vec<String>>
where
    F: Fn(&str) -> String + Send + 'static,
{
    thread::spawn(move || {
        let mut got = Vec::new();
        for _ in 0..count {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read command");
            let cmd = line.trim().to_string();
            let reply = respond(&cmd);
            let mut w = &stream;
            writeln!(w, "{reply}").expect("write response");
            got.push(cmd);
        }
        got
    })
}

fn client() -> Command {
    Command::new(env!("CARGO_BIN_EXE_whispy-client"))
}

#[test]
fn ping_sends_cmd_and_succeeds_on_ok() {
    let socket = temp_socket("ping");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = stub_daemon(listener, 1, |_cmd| r#"{"ok":true}"#.to_string());

    let out = client()
        .arg("--socket")
        .arg(&socket)
        .arg("ping")
        .output()
        .expect("run client");

    let received = server.join().unwrap();
    assert_eq!(received, vec![r#"{"cmd":"ping"}"#]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""ok":true"#));

    std::fs::remove_dir_all(socket.parent().unwrap()).ok();
}

#[test]
fn client_exits_nonzero_when_daemon_reports_error() {
    let socket = temp_socket("err");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = stub_daemon(listener, 1, |_cmd| {
        r#"{"ok":false,"error":"not recording"}"#.to_string()
    });

    let out = client()
        .arg("--socket")
        .arg(&socket)
        .arg("stop")
        .output()
        .expect("run client");

    let received = server.join().unwrap();
    assert_eq!(received, vec![r#"{"cmd":"stop"}"#]);
    assert!(!out.status.success());

    std::fs::remove_dir_all(socket.parent().unwrap()).ok();
}

#[test]
fn toggle_queries_status_then_starts_when_idle() {
    let socket = temp_socket("toggle");
    let listener = UnixListener::bind(&socket).unwrap();
    // Toggle opens two connections: first `status` (idle), then `start`.
    let server = stub_daemon(listener, 2, |cmd| {
        if cmd.contains("status") {
            r#"{"ok":true,"snapshot":{"state":"idle","rms":0.0,"error_kind":null,"error_message":null,"timestamp":0.0}}"#.to_string()
        } else {
            r#"{"ok":true}"#.to_string()
        }
    });

    let out = client()
        .arg("--socket")
        .arg(&socket)
        .arg("toggle")
        .output()
        .expect("run client");

    let received = server.join().unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0], r#"{"cmd":"status"}"#);
    assert_eq!(received[1], r#"{"cmd":"start"}"#);
    assert!(out.status.success());

    std::fs::remove_dir_all(socket.parent().unwrap()).ok();
}
