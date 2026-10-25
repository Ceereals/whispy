//! Shared protocol and state types used by `whispy-daemon` and `whispy-client`.
//!
//! The client talks to the daemon over a Unix socket using a line-based JSON
//! protocol: one [`Cmd`] per line in, one [`Resp`] per line out. The daemon also
//! publishes a [`StateSnapshot`] to `state.json` for the Quickshell pill UI.

use serde::{Deserialize, Serialize};

/// A command sent from the client to the daemon, e.g. `{"cmd":"start"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Cmd {
    /// Begin audio capture. `workflow` optionally names an AI workflow to apply.
    Start {
        /// Name of a `[workflows.<name>]` entry to run on the transcript.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// Stop capture and transcribe.
    Stop,
    /// Stop capture and discard the buffer.
    Cancel,
    /// Return the current state snapshot.
    Status,
    /// Healthcheck.
    Ping,
}

/// The recording lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Waiting for a `start`.
    #[default]
    Idle,
    /// Capturing audio from the microphone.
    Recording,
    /// Capture finished; whisper-server is transcribing the clip.
    Transcribing,
    /// Text was injected (transient flash before reverting to `Idle`).
    Success,
    /// The last recording failed or was dropped (transient flash).
    Error,
}

/// The full state record published to `state.json` and returned by `status`.
///
/// `error_kind`/`error_message` are always present (null when absent) to keep the
/// JSON schema stable for the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Current lifecycle state.
    pub state: State,
    /// Microphone RMS in `[0.0, 1.0]`, updated while recording.
    pub rms: f32,
    /// Machine-readable error identifier (e.g. `stt_error`, `too_long`).
    pub error_kind: Option<String>,
    /// Human-readable error detail for the UI.
    pub error_message: Option<String>,
    /// Unix timestamp (fractional seconds) of this snapshot.
    pub timestamp: f64,
}

/// A daemon response, e.g. `{"ok":true}` or `{"ok":false,"error":"..."}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    /// Whether the command succeeded.
    pub ok: bool,
    /// Failure reason; set only when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// State snapshot; set only in reply to `status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<StateSnapshot>,
}

impl Resp {
    /// A bare success response.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            snapshot: None,
        }
    }

    /// A success response carrying the current state snapshot.
    pub fn status(snapshot: StateSnapshot) -> Self {
        Self {
            ok: true,
            error: None,
            snapshot: Some(snapshot),
        }
    }

    /// A failure response with the given reason.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            snapshot: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_serializes_with_cmd_tag() {
        assert_eq!(
            serde_json::to_string(&Cmd::Start { workflow: None }).unwrap(),
            r#"{"cmd":"start"}"#
        );
        assert_eq!(
            serde_json::to_string(&Cmd::Start {
                workflow: Some("email".to_string())
            })
            .unwrap(),
            r#"{"cmd":"start","workflow":"email"}"#
        );
        assert_eq!(
            serde_json::to_string(&Cmd::Ping).unwrap(),
            r#"{"cmd":"ping"}"#
        );
    }

    #[test]
    fn cmd_roundtrips() {
        let parsed: Cmd = serde_json::from_str(r#"{"cmd":"stop"}"#).unwrap();
        assert_eq!(parsed, Cmd::Stop);

        let parsed: Cmd = serde_json::from_str(r#"{"cmd":"start"}"#).unwrap();
        assert_eq!(parsed, Cmd::Start { workflow: None });
    }

    #[test]
    fn snapshot_keeps_error_keys() {
        let json = serde_json::to_string(&StateSnapshot::default()).unwrap();
        assert!(json.contains(r#""error_kind":null"#));
        assert!(json.contains(r#""state":"idle""#));
    }
}
