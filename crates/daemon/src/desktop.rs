//! The desktop seam: everything the daemon asks of the display server, behind one
//! [`Desktop`] trait with a Wayland and an X11 adapter.
//!
//! The operations are the focused window's class (for app-based workflow
//! selection), the clipboard (top MIME type, read, copy, clear, read as text) and
//! typing text into the focused window. Each adapter drives that display
//! server's CLI tools through [`proc`], so a wedged tool can't hang a thread:
//!
//! - Wayland: `hyprctl`/`swaymsg` (window class), `wl-copy`/`wl-paste`, `wtype`.
//! - X11: `xdotool` (window class, typing), `xclip` (preferred) or `xsel`.
//!
//! [`connect`] picks the adapter for the resolved [`DisplayServer`]. Paste
//! orchestration (save, copy, keystroke, restore) is not here: it is portable and
//! lives in [`crate::inject::Injector`], over a `Desktop`.

use std::process::Command;
use std::sync::Arc;

use tracing::debug;

use crate::display::DisplayServer;
use crate::proc;

/// The display-server-specific operations, implemented once per display server.
///
/// Errors are human-readable messages; callers decide whether they're fatal.
pub trait Desktop: Send + Sync {
    /// A short adapter name for logs/tests (`"wayland"` / `"x11"`).
    fn name(&self) -> &'static str;

    /// Best-effort: the focused window's class, `None` when it can't be read.
    /// Never fails loudly: app-based workflow selection just degrades.
    fn window_class(&self) -> Option<String>;

    /// Type `text` directly into the focused window.
    fn type_text(&self, text: &str) -> Result<(), String>;

    /// The top MIME type currently advertised by the clipboard, if any.
    fn top_mime_type(&self) -> Option<String>;

    /// Read the clipboard contents for `mime`.
    fn read_clipboard(&self, mime: &str) -> Result<Vec<u8>, String>;

    /// Copy `bytes` onto the clipboard, optionally with an explicit MIME type.
    fn copy_clipboard(&self, mime: Option<&str>, bytes: &[u8]) -> Result<(), String>;

    /// Clear the clipboard.
    fn clear_clipboard(&self) -> Result<(), String>;

    /// The clipboard as text (for the `{{CLIPBOARD}}` snippet placeholder).
    ///
    /// Default: read the top MIME type if it is text and decode it lossily; empty
    /// string on any failure or non-text clipboard. The result is intentionally
    /// **not trimmed**: clipboard whitespace may be meaningful.
    fn clipboard_text(&self) -> String {
        match self.top_mime_type() {
            Some(mime) if mime_is_text(&mime) => match self.read_clipboard(&mime) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => String::new(),
            },
            _ => String::new(),
        }
    }
}

/// The [`Desktop`] adapter for `server`. On X11 this probes once for the
/// clipboard tool (`xclip`, else `xsel`).
pub fn connect(server: DisplayServer) -> Arc<dyn Desktop> {
    let desktop: Arc<dyn Desktop> = match server {
        DisplayServer::Wayland => Arc::new(WaylandDesktop),
        DisplayServer::X11 => Arc::new(X11Desktop::detect()),
    };
    debug!(desktop = desktop.name(), "desktop adapter selected");
    desktop
}

/// Whether a clipboard MIME type holds text we can capture and restore.
///
/// Treats anything starting with `text/`, the X11 `STRING`/`UTF8_STRING`
/// targets, or any type whose name contains "text" as text; everything else
/// (images, octet streams, ...) is binary.
pub fn mime_is_text(mime: &str) -> bool {
    let lower = mime.to_ascii_lowercase();
    lower.starts_with("text/")
        || lower == "string"
        || lower == "utf8_string"
        || lower.contains("text")
}

// --- Wayland adapter --------------------------------------------------------

/// Wayland: `hyprctl`/`swaymsg` window class, `wl-copy`/`wl-paste`, `wtype`.
struct WaylandDesktop;

impl Desktop for WaylandDesktop {
    fn name(&self) -> &'static str {
        "wayland"
    }

    /// `hyprctl activewindow -j` (Hyprland), falling back to `swaymsg` (Sway /
    /// wlroots i3-compatible). Other compositors (GNOME/KDE) expose no universal
    /// protocol, so this returns `None` there.
    fn window_class(&self) -> Option<String> {
        hyprctl_class().or_else(swaymsg_class)
    }

    /// Type the transcript directly with `wtype` (Wayland virtual keyboard).
    /// Preserves accents and works in terminals; no clipboard is touched.
    fn type_text(&self, text: &str) -> Result<(), String> {
        // `--` stops wtype's option parsing, so a transcript starting with `-`
        // is typed literally instead of being read as a flag.
        let status = proc::status(Command::new("wtype").arg("--").arg(text), proc::TYPING)
            .map_err(|e| format!("failed to run wtype ({e}); is wtype installed?"))?;
        if !status.success() {
            return Err(format!("wtype exited with {status}"));
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
        let status = proc::status(Command::new("wl-copy").arg("--clear"), proc::CLIPBOARD)
            .map_err(|e| format!("failed to run wl-copy: {e}"))?;
        if !status.success() {
            return Err(format!("wl-copy --clear exited with {status}"));
        }
        Ok(())
    }

    /// Read the clipboard text via `wl-paste --no-newline` (no MIME negotiation),
    /// matching the historical `{{CLIPBOARD}}` behavior. Not trimmed.
    fn clipboard_text(&self) -> String {
        match proc::output(
            Command::new("wl-paste").arg("--no-newline"),
            proc::CLIPBOARD,
        ) {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
            _ => String::new(),
        }
    }
}

/// Hyprland: focused window class via `hyprctl activewindow -j`.
fn hyprctl_class() -> Option<String> {
    let out = proc::output(
        Command::new("hyprctl").args(["activewindow", "-j"]),
        proc::PROBE,
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?;
    non_empty(v.get("class")?.as_str()?)
}

/// Sway / wlroots: focused node's `app_id` (Wayland-native) or
/// `window_properties.class` (`XWayland`) from `swaymsg -t get_tree`.
fn swaymsg_class() -> Option<String> {
    let out = proc::output(
        Command::new("swaymsg").args(["-t", "get_tree"]),
        proc::PROBE,
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let tree = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?;
    let node = focused_node(&tree)?;
    let class = node
        .get("app_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            node.get("window_properties")
                .and_then(|wp| wp.get("class"))
                .and_then(|v| v.as_str())
        })?;
    non_empty(class)
}

/// Depth-first search for the `focused: true` node in a sway tree.
fn focused_node(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if node.get("focused").and_then(serde_json::Value::as_bool) == Some(true) {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(|v| v.as_array()) {
            for child in children {
                if let Some(found) = focused_node(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

// --- X11 adapter ------------------------------------------------------------

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

/// X11: `xdotool` window class and typing, `xclip` (preferred) or `xsel` clipboard.
struct X11Desktop {
    clip: X11Clip,
}

impl X11Desktop {
    /// Probe once for the clipboard CLI: `xclip` preferred, `xsel` fallback.
    fn detect() -> Self {
        Self {
            clip: pick_x11_clip(crate::setup::have("xclip"), crate::setup::have("xsel")),
        }
    }
}

impl Desktop for X11Desktop {
    fn name(&self) -> &'static str {
        "x11"
    }

    /// `xdotool getactivewindow getwindowclassname`.
    fn window_class(&self) -> Option<String> {
        let out = proc::output(
            Command::new("xdotool").args(["getactivewindow", "getwindowclassname"]),
            proc::PROBE,
        )
        .ok()?;
        if !out.status.success() {
            return None;
        }
        non_empty(String::from_utf8_lossy(&out.stdout).trim())
    }

    /// Type the transcript directly with `xdotool type`. `--clearmodifiers` drops
    /// any held modifier (so a held push-to-talk key doesn't corrupt the text);
    /// `--` stops option parsing so a leading `-` is typed literally.
    fn type_text(&self, text: &str) -> Result<(), String> {
        let status = proc::status(
            Command::new("xdotool")
                .args(["type", "--clearmodifiers", "--"])
                .arg(text),
            proc::TYPING,
        )
        .map_err(|e| format!("failed to run xdotool ({e}); is xdotool installed?"))?;
        if !status.success() {
            return Err(format!("xdotool exited with {status}"));
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
                let status = proc::status(
                    Command::new("xsel").args(["--clipboard", "--clear"]),
                    proc::CLIPBOARD,
                )
                .map_err(|e| format!("failed to run xsel: {e}"))?;
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

// --- shared helpers ---------------------------------------------------------

/// `Some(s)` for a non-empty string, `None` otherwise.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Run a command and return its stdout bytes, erroring on a non-zero exit.
fn run_capture(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = proc::output(Command::new(program).args(args), proc::CLIPBOARD)
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited with {}", output.status));
    }
    Ok(output.stdout)
}

/// Run `cmd` with `bytes` on its stdin (then closed), bounded by [`proc::CLIPBOARD`].
/// `name` is the program name, used only in error messages.
fn feed_stdin(mut cmd: Command, name: &str, bytes: &[u8]) -> Result<(), String> {
    let status = proc::with_stdin(&mut cmd, bytes, proc::CLIPBOARD)
        .map_err(|e| format!("failed to run {name}: {e}"))?;
    if !status.success() {
        return Err(format!("{name} exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clipboard-only fake for the default [`Desktop::clipboard_text`].
    struct FakeClipboard {
        mime: Option<&'static str>,
        bytes: Result<&'static [u8], &'static str>,
    }

    impl Desktop for FakeClipboard {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn window_class(&self) -> Option<String> {
            None
        }
        fn type_text(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
        fn top_mime_type(&self) -> Option<String> {
            self.mime.map(String::from)
        }
        fn read_clipboard(&self, _mime: &str) -> Result<Vec<u8>, String> {
            self.bytes.map(<[u8]>::to_vec).map_err(String::from)
        }
        fn copy_clipboard(&self, _mime: Option<&str>, _bytes: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn clear_clipboard(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn connect_picks_the_adapter_for_the_display_server() {
        assert_eq!(connect(DisplayServer::Wayland).name(), "wayland");
        assert_eq!(connect(DisplayServer::X11).name(), "x11");
    }

    #[test]
    fn clipboard_text_reads_text_untrimmed_and_skips_binary_or_failures() {
        let text = FakeClipboard {
            mime: Some("text/plain"),
            bytes: Ok(b"  hi  "),
        };
        assert_eq!(text.clipboard_text(), "  hi  ");

        let image = FakeClipboard {
            mime: Some("image/png"),
            bytes: Ok(b"\x89PNG"),
        };
        assert_eq!(image.clipboard_text(), "");

        let empty = FakeClipboard {
            mime: None,
            bytes: Ok(b""),
        };
        assert_eq!(empty.clipboard_text(), "");

        let broken = FakeClipboard {
            mime: Some("text/plain"),
            bytes: Err("boom"),
        };
        assert_eq!(broken.clipboard_text(), "");
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
    fn focused_node_finds_focused_app_id_in_sway_tree() {
        let tree = serde_json::json!({
            "focused": false,
            "nodes": [
                {
                    "focused": false,
                    "nodes": [
                        { "focused": true, "app_id": "firefox" }
                    ],
                    "floating_nodes": []
                }
            ],
            "floating_nodes": []
        });
        let node = focused_node(&tree).expect("a focused node exists");
        assert_eq!(node.get("app_id").and_then(|v| v.as_str()), Some("firefox"));
    }

    #[test]
    fn focused_node_returns_none_when_nothing_focused() {
        let tree = serde_json::json!({
            "focused": false,
            "nodes": [{ "focused": false, "nodes": [], "floating_nodes": [] }],
            "floating_nodes": []
        });
        assert!(focused_node(&tree).is_none());
    }
}
