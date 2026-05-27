//! Text injection: clipboard save/restore + ydotool Ctrl+V.
//!
//! [`Injector::inject`] pastes a transcript into the focused window without
//! clobbering the user's clipboard: it saves the current selection
//! (`wl-paste`), copies the transcript (`wl-copy`), pastes it with
//! `ydotool key <injection.paste_keys>`, then restores the previous clipboard
//! after `injection.restore_clipboard_delay_ms`.
//!
//! Only the paste step is a hard error; clipboard save/restore are best-effort
//! (logged via `tracing::warn!` and otherwise ignored). Non-text clipboards
//! (e.g. `image/png`) are not restored — restoring binary selections faithfully
//! through `wl-copy` stdin is fragile, so we warn and move on.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use tracing::{debug, warn};

/// Pastes text into the focused window via the Wayland clipboard, preserving
/// whatever the user had on the clipboard before.
pub struct Injector {
    /// ydotool key tokens (e.g. `["29:1", "47:1", "47:0", "29:0"]`).
    paste_keys: Vec<String>,
    /// Delay (ms) between ydotool key events, so the held modifier registers.
    key_delay_ms: u64,
    /// How long to leave the transcript on the clipboard before restoring.
    restore_delay: Duration,
}

/// A failure during text injection.
#[derive(Debug)]
pub enum InjectError {
    /// A clipboard operation (`wl-copy`/`wl-paste`) failed.
    Clipboard(String),
    /// The paste keystroke (`ydotool`) could not be delivered.
    Paste(String),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InjectError::Clipboard(msg) => write!(f, "clipboard error: {msg}"),
            InjectError::Paste(msg) => write!(f, "paste error: {msg}"),
        }
    }
}

impl std::error::Error for InjectError {}

/// A snapshot of the clipboard taken before injection, used to restore it after.
enum Saved {
    /// The clipboard was empty (or unreadable) — restore by clearing.
    Empty,
    /// A text selection of the given MIME type and bytes.
    Text { mime: String, bytes: Vec<u8> },
    /// A non-text selection (e.g. an image) — cannot be restored, only warned about.
    Binary { mime: String },
}

impl Injector {
    /// Build an injector from the `[injection]` config section.
    pub fn new(cfg: &crate::config::Injection) -> Self {
        let paste_keys = cfg.paste_keys.split_whitespace().map(String::from).collect();
        Self {
            paste_keys,
            key_delay_ms: cfg.key_delay_ms,
            restore_delay: Duration::from_millis(cfg.restore_clipboard_delay_ms),
        }
    }

    /// Save the current clipboard, copy `text`, paste it with Ctrl+V via ydotool,
    /// then restore the previous clipboard. Best-effort restore (logs on failure).
    pub fn inject(&self, text: &str) -> Result<(), InjectError> {
        if self.paste_keys.is_empty() {
            return Err(InjectError::Paste("no paste_keys configured".to_string()));
        }

        // 1. Save the existing clipboard (best-effort).
        let saved = self.save_clipboard();

        // 2. Copy the transcript onto the clipboard.
        copy_clipboard(None, text.as_bytes())
            .map_err(|e| InjectError::Clipboard(format!("wl-copy transcript: {e}")))?;

        // 3. Paste via ydotool. This is the only hard error.
        self.paste()?;

        // 4. Give the focused app time to read the clipboard before we restore.
        std::thread::sleep(self.restore_delay);

        // 5. Restore the previous clipboard (best-effort).
        self.restore_clipboard(&saved);

        Ok(())
    }

    /// Capture the current clipboard so it can be restored after pasting.
    ///
    /// Never fails: an unreadable or empty clipboard is reported as [`Saved::Empty`].
    fn save_clipboard(&self) -> Saved {
        let mime = match top_mime_type() {
            Some(mime) => mime,
            None => {
                debug!("clipboard empty or unreadable; nothing to restore");
                return Saved::Empty;
            }
        };

        if !mime_is_text(&mime) {
            warn!(mime = %mime, "non-text clipboard contents will not be restored");
            return Saved::Binary { mime };
        }

        match run_capture("wl-paste", &["--no-newline", "--type", &mime]) {
            Ok(bytes) => {
                debug!(mime = %mime, len = bytes.len(), "saved text clipboard");
                Saved::Text { mime, bytes }
            }
            Err(e) => {
                warn!(mime = %mime, error = %e, "failed to read clipboard contents; treating as empty");
                Saved::Empty
            }
        }
    }

    /// Deliver the configured paste keystroke via `ydotool key`.
    fn paste(&self) -> Result<(), InjectError> {
        let status = Command::new("ydotool")
            .arg("key")
            .arg("--key-delay")
            .arg(self.key_delay_ms.to_string())
            .args(&self.paste_keys)
            .status()
            .map_err(|e| {
                InjectError::Paste(format!(
                    "failed to spawn ydotool ({e}); is ydotoold running with uinput access? \
                     see scripts/setup-ydotool.sh"
                ))
            })?;
        if !status.success() {
            return Err(InjectError::Paste(format!(
                "ydotool exited with {status}; is ydotoold running with uinput access? \
                 see scripts/setup-ydotool.sh"
            )));
        }
        debug!(keys = ?self.paste_keys, "pasted via ydotool");
        Ok(())
    }

    /// Put the previously-saved clipboard contents back (best-effort).
    fn restore_clipboard(&self, saved: &Saved) {
        let result = match saved {
            Saved::Empty => clear_clipboard(),
            Saved::Text { mime, bytes } => copy_clipboard(Some(mime), bytes),
            Saved::Binary { mime } => {
                warn!(mime = %mime, "skipping restore of non-text clipboard");
                return;
            }
        };
        if let Err(e) = result {
            warn!(error = %e, "failed to restore previous clipboard");
        } else {
            debug!("restored previous clipboard");
        }
    }
}

/// Read the top MIME type advertised by the clipboard, if any.
///
/// Runs `wl-paste --list-types` and returns the first non-empty trimmed line.
/// Returns `None` if the command fails or the clipboard is empty.
fn top_mime_type() -> Option<String> {
    let bytes = run_capture("wl-paste", &["--list-types"]).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(String::from)
}

/// Whether a clipboard MIME type holds text we can capture and restore.
///
/// Treats anything starting with `text/`, the X11 `STRING`/`UTF8_STRING`
/// targets, or any type whose name contains "text" as text; everything else
/// (images, octet streams, ...) is binary.
fn mime_is_text(mime: &str) -> bool {
    let lower = mime.to_ascii_lowercase();
    lower.starts_with("text/")
        || lower == "string"
        || lower == "utf8_string"
        || lower.contains("text")
}

/// Run a command and return its stdout bytes, erroring on a non-zero exit.
fn run_capture(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited with {}", output.status));
    }
    Ok(output.stdout)
}

/// Copy `bytes` onto the clipboard via `wl-copy`, optionally with an explicit MIME type.
fn copy_clipboard(mime: Option<&str>, bytes: &[u8]) -> Result<(), String> {
    let mut cmd = Command::new("wl-copy");
    if let Some(mime) = mime {
        cmd.arg("--type").arg(mime);
    }
    feed_stdin(cmd, bytes)
}

/// Clear the clipboard via `wl-copy --clear`.
fn clear_clipboard() -> Result<(), String> {
    let status = Command::new("wl-copy")
        .arg("--clear")
        .status()
        .map_err(|e| format!("failed to spawn wl-copy: {e}"))?;
    if !status.success() {
        return Err(format!("wl-copy --clear exited with {status}"));
    }
    Ok(())
}

/// Spawn `cmd` with a piped stdin, write `bytes` to it, then wait for it to exit.
fn feed_stdin(mut cmd: Command, bytes: &[u8]) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn wl-copy: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "wl-copy stdin unavailable".to_string())?;
        stdin
            .write_all(bytes)
            .map_err(|e| format!("failed to write to wl-copy stdin: {e}"))?;
        // `stdin` drops here, closing the pipe so wl-copy sees EOF.
    }
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for wl-copy: {e}"))?;
    if !status.success() {
        return Err(format!("wl-copy exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injector_with_keys(paste_keys: &str) -> Injector {
        Injector::new(&crate::config::Injection {
            restore_clipboard_delay_ms: 150,
            paste_keys: paste_keys.to_string(),
        })
    }

    #[test]
    fn paste_keys_parse_into_tokens() {
        let inj = injector_with_keys("29:1 47:1 47:0 29:0");
        assert_eq!(inj.paste_keys, vec!["29:1", "47:1", "47:0", "29:0"]);
    }

    #[test]
    fn paste_keys_parsing_collapses_extra_whitespace() {
        let inj = injector_with_keys("  29:1\t47:1\n47:0   29:0  ");
        assert_eq!(inj.paste_keys, vec!["29:1", "47:1", "47:0", "29:0"]);
    }

    #[test]
    fn new_converts_delay_to_duration() {
        let inj = injector_with_keys("29:1 29:0");
        assert_eq!(inj.restore_delay, Duration::from_millis(150));
    }

    #[test]
    fn empty_paste_keys_is_a_paste_error() {
        let inj = injector_with_keys("   ");
        assert!(inj.paste_keys.is_empty());
        match inj.inject("hello") {
            Err(InjectError::Paste(msg)) => assert!(msg.contains("no paste_keys")),
            other => panic!("expected Paste error, got {other:?}"),
        }
    }

    #[test]
    fn text_mime_types_are_classified_as_text() {
        for mime in [
            "text/plain",
            "text/plain;charset=utf-8",
            "TEXT/HTML",
            "STRING",
            "UTF8_STRING",
            "utf8_string",
            "application/x-moz-nativehtml-text",
        ] {
            assert!(mime_is_text(mime), "{mime} should be text");
        }
    }

    #[test]
    fn binary_mime_types_are_classified_as_binary() {
        for mime in [
            "image/png",
            "image/jpeg",
            "application/octet-stream",
            "application/pdf",
        ] {
            assert!(!mime_is_text(mime), "{mime} should be binary");
        }
    }

    #[test]
    fn error_display_is_prefixed_by_kind() {
        assert_eq!(
            InjectError::Clipboard("boom".to_string()).to_string(),
            "clipboard error: boom"
        );
        assert_eq!(
            InjectError::Paste("nope".to_string()).to_string(),
            "paste error: nope"
        );
    }
}
