//! Text injection into the focused window, in one of two modes (`injection.mode`):
//!
//! - `paste` (default): save the current selection, copy the transcript, paste it
//!   with `ydotool key <injection.paste_keys>`, then restore the previous clipboard
//!   after `injection.restore_clipboard_delay_ms`. Only the paste step is a hard
//!   error; clipboard save/restore are best-effort (logged via `tracing::warn!` and
//!   otherwise ignored). Non-text clipboards (e.g. `image/png`) are not restored —
//!   restoring binary selections faithfully through the clipboard tool's stdin is
//!   fragile, so we warn and move on.
//! - `type`: type the transcript directly (Wayland `wtype` virtual keyboard, or X11
//!   `xdotool type`). Works in terminals too and touches no clipboard.
//!
//! The clipboard and typing plumbing is the display server's [`Desktop`] adapter
//! (Wayland or X11). The portable `ydotool` paste keystroke is the same on both, so
//! it lives in [`Injector`] rather than the desktop (and is the only hard-error path).
//!
//! [`Injector::inject`] dispatches to the configured mode over the desktop.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use crate::config::InjectMode;
use crate::desktop::{Desktop, mime_is_text};
use crate::proc;

/// Dispatches injection to the configured mode, over the display server's
/// [`Desktop`]. Preserves the user's clipboard across pastes.
pub struct Injector {
    /// The clipboard/typing adapter (Wayland or X11).
    desktop: Arc<dyn Desktop>,
    /// Type the transcript, or clipboard + paste it.
    mode: InjectMode,
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
    /// A clipboard operation failed.
    Clipboard(String),
    /// The paste keystroke (`ydotool`) could not be delivered.
    Paste(String),
    /// Typing the transcript (`wtype`/`xdotool`) failed.
    Type(String),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clipboard(msg) => write!(f, "clipboard error: {msg}"),
            Self::Paste(msg) => write!(f, "paste error: {msg}"),
            Self::Type(msg) => write!(f, "type error: {msg}"),
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
    /// Build an injector from the `[injection]` config section, driving `desktop`'s
    /// clipboard and typing.
    pub fn new(cfg: &crate::config::Injection, desktop: Arc<dyn Desktop>) -> Self {
        let paste_keys = cfg
            .paste_keys
            .split_whitespace()
            .map(String::from)
            .collect();
        debug!(desktop = desktop.name(), mode = %cfg.mode, "injector ready");
        Self {
            desktop,
            mode: cfg.mode,
            paste_keys,
            key_delay_ms: cfg.key_delay_ms,
            restore_delay: Duration::from_millis(cfg.restore_clipboard_delay_ms),
        }
    }

    /// Inject `text` into the focused window using the configured mode.
    pub fn inject(&self, text: &str) -> Result<(), InjectError> {
        match self.mode {
            InjectMode::Type => self.desktop.type_text(text).map_err(InjectError::Type),
            InjectMode::Paste => self.paste_text(text),
        }
    }

    /// Save the current clipboard, copy `text`, paste it with Ctrl+V via ydotool,
    /// then restore the previous clipboard. Best-effort restore (logs on failure).
    fn paste_text(&self, text: &str) -> Result<(), InjectError> {
        if self.paste_keys.is_empty() {
            return Err(InjectError::Paste("no paste_keys configured".to_string()));
        }

        // 1. Save the existing clipboard (best-effort).
        let saved = self.save_clipboard();

        // 2. Copy the transcript onto the clipboard.
        self.desktop
            .copy_clipboard(None, text.as_bytes())
            .map_err(|e| InjectError::Clipboard(format!("copy transcript: {e}")))?;

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
        let Some(mime) = self.desktop.top_mime_type() else {
            debug!("clipboard empty or unreadable; nothing to restore");
            return Saved::Empty;
        };

        if !mime_is_text(&mime) {
            warn!(mime = %mime, "non-text clipboard contents will not be restored");
            return Saved::Binary { mime };
        }

        match self.desktop.read_clipboard(&mime) {
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

    /// Deliver the configured paste keystroke via `ydotool key`. Portable across
    /// Wayland and X11 (uinput-based), so it stays here rather than in the desktop.
    fn paste(&self) -> Result<(), InjectError> {
        let status = proc::status(
            Command::new("ydotool")
                .arg("key")
                .arg("--key-delay")
                .arg(self.key_delay_ms.to_string())
                .args(&self.paste_keys),
            proc::KEYS,
        )
        .map_err(|e| {
            InjectError::Paste(format!(
                "failed to run ydotool ({e}); is ydotoold running with uinput access? \
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
            Saved::Empty => self.desktop.clear_clipboard(),
            Saved::Text { mime, bytes } => self.desktop.copy_clipboard(Some(mime), bytes),
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A recording [`Desktop`] fake: logs every clipboard/typing call, runs nothing.
    #[derive(Default)]
    struct FakeDesktop {
        calls: Mutex<Vec<String>>,
        type_error: Option<&'static str>,
    }

    impl FakeDesktop {
        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Desktop for FakeDesktop {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn window_class(&self) -> Option<String> {
            None
        }
        fn type_text(&self, text: &str) -> Result<(), String> {
            self.log(format!("type {text}"));
            self.type_error.map_or(Ok(()), |e| Err(e.to_string()))
        }
        fn top_mime_type(&self) -> Option<String> {
            self.log("top_mime_type".to_string());
            None
        }
        fn read_clipboard(&self, mime: &str) -> Result<Vec<u8>, String> {
            self.log(format!("read {mime}"));
            Ok(Vec::new())
        }
        fn copy_clipboard(&self, _mime: Option<&str>, bytes: &[u8]) -> Result<(), String> {
            self.log(format!("copy {}", String::from_utf8_lossy(bytes)));
            Ok(())
        }
        fn clear_clipboard(&self) -> Result<(), String> {
            self.log("clear".to_string());
            Ok(())
        }
    }

    fn injection(mode: InjectMode, paste_keys: &str) -> crate::config::Injection {
        crate::config::Injection {
            mode,
            backend: crate::display::Backend::Auto,
            restore_clipboard_delay_ms: 150,
            paste_keys: paste_keys.to_string(),
            key_delay_ms: 25,
        }
    }

    fn injector_with_keys(paste_keys: &str) -> Injector {
        Injector::new(
            &injection(InjectMode::Paste, paste_keys),
            Arc::new(FakeDesktop::default()),
        )
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
    fn empty_paste_keys_is_a_paste_error_before_touching_the_clipboard() {
        let desktop = Arc::new(FakeDesktop::default());
        let inj = Injector::new(
            &injection(InjectMode::Paste, "   "),
            Arc::<FakeDesktop>::clone(&desktop),
        );
        assert_eq!(inj.paste_keys, Vec::<String>::new());
        match inj.inject("hello") {
            Err(InjectError::Paste(msg)) => assert!(msg.contains("no paste_keys")),
            other => panic!("expected Paste error, got {other:?}"),
        }
        assert_eq!(desktop.calls(), Vec::<String>::new());
    }

    #[test]
    fn type_mode_types_through_the_desktop_without_the_clipboard() {
        let desktop = Arc::new(FakeDesktop::default());
        let inj = Injector::new(
            &injection(InjectMode::Type, "29:1 29:0"),
            Arc::<FakeDesktop>::clone(&desktop),
        );
        inj.inject("ciao").expect("typed");
        assert_eq!(desktop.calls(), ["type ciao"]);
    }

    #[test]
    fn type_mode_failure_is_a_type_error() {
        let desktop = Arc::new(FakeDesktop {
            type_error: Some("wtype exited with 1"),
            ..FakeDesktop::default()
        });
        let inj = Injector::new(&injection(InjectMode::Type, "29:1 29:0"), desktop);
        match inj.inject("ciao") {
            Err(e @ InjectError::Type(_)) => {
                assert_eq!(e.to_string(), "type error: wtype exited with 1");
            }
            other => panic!("expected Type error, got {other:?}"),
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
