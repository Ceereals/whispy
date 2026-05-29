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
//! The clipboard/type plumbing is a swappable [`InjectBackend`] chosen from the
//! resolved [`DisplayServer`] ([`WaylandClipboard`] for Wayland, [`X11Clipboard`]
//! for X11). The portable `ydotool` paste keystroke is the same on both, so it
//! lives in [`Injector`] rather than the backend (and is the only hard-error path).
//!
//! [`Injector::inject`] dispatches to the configured mode over the chosen backend.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use tracing::{debug, warn};

use crate::display::DisplayServer;

/// A swappable clipboard + direct-typing backend (Wayland or X11). The portable
/// `ydotool` paste keystroke is *not* part of this trait — it lives in [`Injector`].
pub trait InjectBackend: Send + Sync {
    /// A short backend name for logs/tests (`"wayland"` / `"x11"`).
    fn name(&self) -> &'static str;

    /// Type `text` directly into the focused window (Wayland `wtype` / X11 `xdotool`).
    fn type_text(&self, text: &str) -> Result<(), InjectError>;

    /// The top MIME type currently advertised by the clipboard, if any.
    fn top_mime_type(&self) -> Option<String>;

    /// Read the clipboard contents for `mime`.
    fn read_clipboard(&self, mime: &str) -> Result<Vec<u8>, String>;

    /// Copy `bytes` onto the clipboard, optionally with an explicit MIME type.
    fn copy_clipboard(&self, mime: Option<&str>, bytes: &[u8]) -> Result<(), String>;

    /// Clear the clipboard.
    fn clear_clipboard(&self) -> Result<(), String>;

    /// Read the clipboard as text for the `{{CLIPBOARD}}` snippet placeholder.
    ///
    /// Default: read the top MIME type if it is text and decode it lossily; empty
    /// string on any failure or non-text clipboard. The result is intentionally
    /// **not trimmed** — clipboard whitespace may be meaningful.
    fn read_clipboard_text(&self) -> String {
        match self.top_mime_type() {
            Some(mime) if mime_is_text(&mime) => match self.read_clipboard(&mime) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => String::new(),
            },
            _ => String::new(),
        }
    }
}

/// Dispatches injection to the configured mode, over a [`InjectBackend`] chosen
/// from the resolved display server. Preserves the user's clipboard across pastes.
pub struct Injector {
    /// The clipboard/type backend (Wayland or X11).
    backend: Box<dyn InjectBackend>,
    /// When true, type the transcript instead of clipboard + paste.
    use_type: bool,
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
            InjectError::Clipboard(msg) => write!(f, "clipboard error: {msg}"),
            InjectError::Paste(msg) => write!(f, "paste error: {msg}"),
            InjectError::Type(msg) => write!(f, "type error: {msg}"),
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
    /// Build an injector from the `[injection]` config section and the resolved
    /// display server (which selects the clipboard/type backend).
    pub fn new(cfg: &crate::config::Injection, server: DisplayServer) -> Self {
        let paste_keys = cfg
            .paste_keys
            .split_whitespace()
            .map(String::from)
            .collect();
        let use_type = cfg.mode.eq_ignore_ascii_case("type");
        if !use_type && !cfg.mode.eq_ignore_ascii_case("paste") {
            warn!(
                mode = %cfg.mode,
                "unknown injection.mode; expected \"paste\" or \"type\", falling back to paste"
            );
        }
        let backend: Box<dyn InjectBackend> = match server {
            DisplayServer::Wayland => Box::new(WaylandClipboard),
            DisplayServer::X11 => Box::new(X11Clipboard::detect()),
        };
        debug!(backend = backend.name(), mode = %cfg.mode, "injection backend selected");
        Self {
            backend,
            use_type,
            paste_keys,
            key_delay_ms: cfg.key_delay_ms,
            restore_delay: Duration::from_millis(cfg.restore_clipboard_delay_ms),
        }
    }

    /// The clipboard/type backend this injector drives (for pipeline clipboard reads).
    pub fn backend(&self) -> &dyn InjectBackend {
        self.backend.as_ref()
    }

    /// Inject `text` into the focused window using the configured mode.
    pub fn inject(&self, text: &str) -> Result<(), InjectError> {
        if self.use_type {
            self.backend.type_text(text)
        } else {
            self.paste_text(text)
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
        self.backend
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
        let mime = match self.backend.top_mime_type() {
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

        match self.backend.read_clipboard(&mime) {
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
    /// Wayland and X11 (uinput-based), so it stays here rather than in the backend.
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
            Saved::Empty => self.backend.clear_clipboard(),
            Saved::Text { mime, bytes } => self.backend.copy_clipboard(Some(mime), bytes),
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

// --- Wayland backend --------------------------------------------------------

/// Wayland clipboard + typing via `wl-copy`/`wl-paste` and `wtype`.
struct WaylandClipboard;

impl InjectBackend for WaylandClipboard {
    fn name(&self) -> &'static str {
        "wayland"
    }

    /// Type the transcript directly with `wtype` (Wayland virtual keyboard).
    /// Preserves accents and works in terminals; no clipboard is touched.
    fn type_text(&self, text: &str) -> Result<(), InjectError> {
        // `--` stops wtype's option parsing, so a transcript starting with `-`
        // is typed literally instead of being read as a flag.
        let status = Command::new("wtype")
            .arg("--")
            .arg(text)
            .status()
            .map_err(|e| {
                InjectError::Type(format!("failed to spawn wtype ({e}); is wtype installed?"))
            })?;
        if !status.success() {
            return Err(InjectError::Type(format!("wtype exited with {status}")));
        }
        debug!(chars = text.chars().count(), "typed via wtype");
        Ok(())
    }

    /// Read the top MIME type advertised by the clipboard, if any.
    ///
    /// Runs `wl-paste --list-types` and returns the first non-empty trimmed line.
    fn top_mime_type(&self) -> Option<String> {
        let bytes = run_capture("wl-paste", &["--list-types"]).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(String::from)
    }

    fn read_clipboard(&self, mime: &str) -> Result<Vec<u8>, String> {
        run_capture("wl-paste", &["--no-newline", "--type", mime])
    }

    fn copy_clipboard(&self, mime: Option<&str>, bytes: &[u8]) -> Result<(), String> {
        let mut cmd = Command::new("wl-copy");
        if let Some(mime) = mime {
            cmd.arg("--type").arg(mime);
        }
        feed_stdin(cmd, "wl-copy", bytes)
    }

    fn clear_clipboard(&self) -> Result<(), String> {
        let status = Command::new("wl-copy")
            .arg("--clear")
            .status()
            .map_err(|e| format!("failed to spawn wl-copy: {e}"))?;
        if !status.success() {
            return Err(format!("wl-copy --clear exited with {status}"));
        }
        Ok(())
    }

    /// Read the clipboard text via `wl-paste --no-newline` (no MIME negotiation),
    /// matching the historical `{{CLIPBOARD}}` behavior. Not trimmed.
    fn read_clipboard_text(&self) -> String {
        match Command::new("wl-paste").arg("--no-newline").output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
            _ => String::new(),
        }
    }
}

// --- X11 backend ------------------------------------------------------------

/// Which X11 clipboard CLI we found at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum X11Clip {
    /// `xclip -selection clipboard`.
    Xclip,
    /// `xsel --clipboard`.
    Xsel,
    /// Neither found — clipboard ops fail loudly; typing still works via xdotool.
    None,
}

/// X11 clipboard + typing via `xdotool type` and `xclip` (preferred) or `xsel`.
struct X11Clipboard {
    clip: X11Clip,
}

impl X11Clipboard {
    /// Probe once for the clipboard CLI: `xclip` preferred, `xsel` fallback.
    fn detect() -> Self {
        Self {
            clip: pick_x11_clip(have_tool("xclip"), have_tool("xsel")),
        }
    }
}

impl InjectBackend for X11Clipboard {
    fn name(&self) -> &'static str {
        "x11"
    }

    /// Type the transcript directly with `xdotool type`. `--clearmodifiers` drops
    /// any held modifier (so a held push-to-talk key doesn't corrupt the text);
    /// `--` stops option parsing so a leading `-` is typed literally.
    fn type_text(&self, text: &str) -> Result<(), InjectError> {
        let status = Command::new("xdotool")
            .args(["type", "--clearmodifiers", "--"])
            .arg(text)
            .status()
            .map_err(|e| {
                InjectError::Type(format!(
                    "failed to spawn xdotool ({e}); is xdotool installed?"
                ))
            })?;
        if !status.success() {
            return Err(InjectError::Type(format!("xdotool exited with {status}")));
        }
        debug!(chars = text.chars().count(), "typed via xdotool");
        Ok(())
    }

    /// X11 selections are untyped (no MIME advertisement). Report a generic text
    /// target when the clipboard holds anything, so the save/restore path treats
    /// it as text (`mime_is_text("UTF8_STRING")` is true).
    fn top_mime_type(&self) -> Option<String> {
        if self.clip == X11Clip::None {
            return None;
        }
        let bytes = self.read_clipboard("UTF8_STRING").ok()?;
        if bytes.is_empty() {
            None
        } else {
            Some("UTF8_STRING".to_string())
        }
    }

    /// Read the CLIPBOARD selection. The `mime` is advisory only (X11 has no MIME
    /// layer at this level); both tools return the selection's text bytes.
    fn read_clipboard(&self, _mime: &str) -> Result<Vec<u8>, String> {
        match self.clip {
            X11Clip::Xclip => run_capture("xclip", &["-selection", "clipboard", "-o"]),
            X11Clip::Xsel => run_capture("xsel", &["--clipboard", "--output"]),
            X11Clip::None => Err("no X11 clipboard tool (install xclip or xsel)".to_string()),
        }
    }

    /// Copy onto the CLIPBOARD selection (matches `wl-copy` + Ctrl+V). The `mime`
    /// is ignored: X11 stores the bytes as the selection's text targets.
    fn copy_clipboard(&self, _mime: Option<&str>, bytes: &[u8]) -> Result<(), String> {
        match self.clip {
            X11Clip::Xclip => {
                let mut cmd = Command::new("xclip");
                cmd.args(["-selection", "clipboard"]);
                feed_stdin(cmd, "xclip", bytes)
            }
            X11Clip::Xsel => {
                let mut cmd = Command::new("xsel");
                cmd.args(["--clipboard", "--input"]);
                feed_stdin(cmd, "xsel", bytes)
            }
            X11Clip::None => Err("no X11 clipboard tool (install xclip or xsel)".to_string()),
        }
    }

    /// Clear the CLIPBOARD selection.
    fn clear_clipboard(&self) -> Result<(), String> {
        match self.clip {
            X11Clip::Xclip => {
                // xclip has no `--clear`; copying empty bytes empties the selection.
                let mut cmd = Command::new("xclip");
                cmd.args(["-selection", "clipboard"]);
                feed_stdin(cmd, "xclip", b"")
            }
            X11Clip::Xsel => {
                let status = Command::new("xsel")
                    .args(["--clipboard", "--clear"])
                    .status()
                    .map_err(|e| format!("failed to spawn xsel: {e}"))?;
                if !status.success() {
                    return Err(format!("xsel --clear exited with {status}"));
                }
                Ok(())
            }
            X11Clip::None => Err("no X11 clipboard tool (install xclip or xsel)".to_string()),
        }
    }
}

/// Pick the X11 clipboard CLI: `xclip` preferred, `xsel` fallback, else none.
/// Pure (takes presence booleans) so the choice is unit-testable.
fn pick_x11_clip(have_xclip: bool, have_xsel: bool) -> X11Clip {
    if have_xclip {
        X11Clip::Xclip
    } else if have_xsel {
        X11Clip::Xsel
    } else {
        X11Clip::None
    }
}

/// Is `tool` on PATH? (`command -v`, same probe as `setup::have`.)
fn have_tool(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// --- shared helpers ---------------------------------------------------------

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

/// Spawn `cmd` with a piped stdin, write `bytes` to it, then wait for it to exit.
/// `name` is the program name, used only in error messages.
fn feed_stdin(mut cmd: Command, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {name}: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{name} stdin unavailable"))?;
        stdin
            .write_all(bytes)
            .map_err(|e| format!("failed to write to {name} stdin: {e}"))?;
        // `stdin` drops here, closing the pipe so the tool sees EOF.
    }
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for {name}: {e}"))?;
    if !status.success() {
        return Err(format!("{name} exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injector_with_keys(paste_keys: &str) -> Injector {
        Injector::new(
            &crate::config::Injection {
                mode: "paste".to_string(),
                backend: "auto".to_string(),
                restore_clipboard_delay_ms: 150,
                paste_keys: paste_keys.to_string(),
                key_delay_ms: 25,
            },
            DisplayServer::Wayland,
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
    fn empty_paste_keys_is_a_paste_error() {
        let inj = injector_with_keys("   ");
        assert!(inj.paste_keys.is_empty());
        match inj.inject("hello") {
            Err(InjectError::Paste(msg)) => assert!(msg.contains("no paste_keys")),
            other => panic!("expected Paste error, got {other:?}"),
        }
    }

    #[test]
    fn new_selects_backend_by_display_server() {
        let cfg = crate::config::Injection {
            mode: "paste".to_string(),
            backend: "auto".to_string(),
            restore_clipboard_delay_ms: 150,
            paste_keys: "29:1 29:0".to_string(),
            key_delay_ms: 25,
        };
        assert_eq!(
            Injector::new(&cfg, DisplayServer::Wayland).backend().name(),
            "wayland"
        );
        assert_eq!(
            Injector::new(&cfg, DisplayServer::X11).backend().name(),
            "x11"
        );
    }

    #[test]
    fn x11_clip_prefers_xclip_then_xsel_then_none() {
        assert_eq!(pick_x11_clip(true, true), X11Clip::Xclip);
        assert_eq!(pick_x11_clip(true, false), X11Clip::Xclip);
        assert_eq!(pick_x11_clip(false, true), X11Clip::Xsel);
        assert_eq!(pick_x11_clip(false, false), X11Clip::None);
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
