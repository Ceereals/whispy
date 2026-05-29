//! Post-transcription text pipeline: corrections -> snippets -> AI workflow.

use std::collections::BTreeMap;
use std::process::Command;

use tracing::{info, warn};

use crate::config::{Config, Workflow};
use crate::display::DisplayServer;
use crate::inject::InjectBackend;

/// Context for one transcript: which workflow was requested and the focused window.
#[derive(Debug, Clone, Default)]
pub struct AppContext {
    /// Workflow name explicitly requested via the IPC command (hotkey choice).
    pub workflow: Option<String>,
    /// The focused window's class at the time recording started.
    pub window_class: Option<String>,
}

/// Run the accepted transcript through corrections, snippets, then the selected
/// workflow's LLM transform. Never fails: LLM errors fall back to the input text.
///
/// `backend` resolves the `{{CLIPBOARD}}` snippet placeholder (Wayland/X11).
pub fn process(
    text: String,
    cfg: &Config,
    ctx: &AppContext,
    backend: &dyn InjectBackend,
) -> String {
    let text = apply_corrections(&text, &cfg.dictionary.corrections);
    let text = apply_snippets(&text, &cfg.snippets.entries, backend);

    match select_workflow(cfg, ctx) {
        Some(wf) if !wf.prompt.trim().is_empty() => {
            match crate::llm::transform(&cfg.llm, &wf.prompt, &text) {
                Ok(out) if !out.is_empty() => {
                    info!(workflow = %wf.name, "applied AI workflow");
                    out
                }
                Ok(_) => text,
                Err(e) => {
                    warn!(workflow = %wf.name, error = %e, "AI workflow failed; injecting raw transcript");
                    text
                }
            }
        }
        _ => text,
    }
}

/// Pick the workflow for this transcript. An explicit request (hotkey) wins over
/// app-based auto-selection; with neither, no workflow runs.
fn select_workflow<'a>(cfg: &'a Config, ctx: &AppContext) -> Option<&'a Workflow> {
    pick_workflow(
        &cfg.workflow,
        ctx.workflow.as_deref(),
        ctx.window_class.as_deref(),
    )
}

/// Core selection logic, factored out for unit testing over a plain slice.
fn pick_workflow<'a>(
    workflows: &'a [Workflow],
    requested: Option<&str>,
    class: Option<&str>,
) -> Option<&'a Workflow> {
    if let Some(name) = requested {
        return workflows.iter().find(|w| w.name.eq_ignore_ascii_case(name));
    }
    if let Some(class) = class {
        return workflows
            .iter()
            .find(|w| w.apps.iter().any(|a| a.eq_ignore_ascii_case(class)));
    }
    None
}

/// Apply case-insensitive, whole-word dictionary corrections in order.
fn apply_corrections(text: &str, corrections: &BTreeMap<String, String>) -> String {
    if corrections.is_empty() {
        return text.to_string();
    }
    corrections
        .iter()
        .fold(text.to_string(), |acc, (from, to)| {
            replace_phrase_ci(&acc, from, to)
        })
}

/// Expand text-expansion snippets. Longer triggers are applied first so they win
/// over shorter overlapping ones; placeholders in replacements are resolved live.
fn apply_snippets(
    text: &str,
    entries: &BTreeMap<String, String>,
    backend: &dyn InjectBackend,
) -> String {
    if entries.is_empty() {
        return text.to_string();
    }
    let mut sorted: Vec<(&String, &String)> = entries.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.0.chars().count()));

    let mut acc = text.to_string();
    for (trigger, replacement) in sorted {
        let resolved = resolve_placeholders(replacement, backend);
        acc = replace_phrase_ci(&acc, trigger, &resolved);
    }
    acc
}

/// Case-insensitive, whole-word phrase replacement.
///
/// Implemented on `Vec<char>` rather than byte slices because Unicode lowercasing
/// can change a string's byte length, which makes byte-index arithmetic unsafe.
fn replace_phrase_ci(text: &str, from: &str, to: &str) -> String {
    let pat: Vec<char> = from.chars().collect();
    if pat.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if matches_at(&chars, i, &pat) && boundary_ok(&chars, i, pat.len()) {
            out.push_str(to);
            i += pat.len();
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn matches_at(chars: &[char], i: usize, pat: &[char]) -> bool {
    if i + pat.len() > chars.len() {
        return false;
    }
    (0..pat.len()).all(|k| char_eq_ci(chars[i + k], pat[k]))
}

fn char_eq_ci(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

// A match is "whole word" if the chars immediately before and after are not alphanumeric.
fn boundary_ok(chars: &[char], start: usize, len: usize) -> bool {
    let before_ok = start == 0 || !chars[start - 1].is_alphanumeric();
    let after_ok = chars.get(start + len).is_none_or(|c| !c.is_alphanumeric());
    before_ok && after_ok
}

/// Substitute `{{DATE}}`, `{{TIME}}`, and `{{CLIPBOARD}}` placeholders in a snippet.
/// The clipboard read is dispatched through the active injection `backend`.
fn resolve_placeholders(s: &str, backend: &dyn InjectBackend) -> String {
    let mut out = s.to_string();
    if out.contains("{{DATE}}") {
        out = out.replace("{{DATE}}", &shell_date("+%Y-%m-%d"));
    }
    if out.contains("{{TIME}}") {
        out = out.replace("{{TIME}}", &shell_date("+%H:%M"));
    }
    if out.contains("{{CLIPBOARD}}") {
        out = out.replace("{{CLIPBOARD}}", &backend.read_clipboard_text());
    }
    out
}

/// Format the current time via `date FMT`; empty string on any failure.
fn shell_date(fmt: &str) -> String {
    match Command::new("date").arg(fmt).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Best-effort: the focused window's class, dispatched by display server.
///
/// - Wayland → `hyprctl activewindow -j` (Hyprland), falling back to `swaymsg`
///   (Sway / wlroots i3-compatible). Other Wayland compositors (GNOME/KDE) expose
///   no universal protocol, so this returns `None` there.
/// - X11 → `xdotool getactivewindow getwindowclassname`.
///
/// Always `Option<String>`, never panics: app-based workflow auto-selection
/// degrades silently when the class can't be read (an explicit `--workflow` still
/// works).
pub fn active_window_class(server: DisplayServer) -> Option<String> {
    match server {
        DisplayServer::Wayland => hyprctl_class().or_else(swaymsg_class),
        DisplayServer::X11 => xdotool_class(),
    }
}

/// Hyprland: focused window class via `hyprctl activewindow -j`.
fn hyprctl_class() -> Option<String> {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?;
    non_empty(v.get("class")?.as_str()?)
}

/// Sway / wlroots: focused node's `app_id` (Wayland-native) or
/// `window_properties.class` (XWayland) from `swaymsg -t get_tree`.
fn swaymsg_class() -> Option<String> {
    let out = Command::new("swaymsg")
        .args(["-t", "get_tree"])
        .output()
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
    if node.get("focused").and_then(|v| v.as_bool()) == Some(true) {
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

/// X11: focused window class via `xdotool getactivewindow getwindowclassname`.
fn xdotool_class() -> Option<String> {
    let out = Command::new("xdotool")
        .args(["getactivewindow", "getwindowclassname"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    non_empty(String::from_utf8_lossy(&out.stdout).trim())
}

/// `Some(s)` for a non-empty string, `None` otherwise.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub backend for snippet tests: a fixed clipboard, no real subprocesses.
    struct StubBackend {
        clipboard: String,
    }

    impl InjectBackend for StubBackend {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn type_text(&self, _text: &str) -> Result<(), crate::inject::InjectError> {
            Ok(())
        }
        fn top_mime_type(&self) -> Option<String> {
            None
        }
        fn read_clipboard(&self, _mime: &str) -> Result<Vec<u8>, String> {
            Ok(self.clipboard.as_bytes().to_vec())
        }
        fn copy_clipboard(&self, _mime: Option<&str>, _bytes: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn clear_clipboard(&self) -> Result<(), String> {
            Ok(())
        }
        fn read_clipboard_text(&self) -> String {
            self.clipboard.clone()
        }
    }

    fn stub(clipboard: &str) -> StubBackend {
        StubBackend {
            clipboard: clipboard.to_string(),
        }
    }

    #[test]
    fn replace_phrase_ci_is_case_insensitive_and_whole_word() {
        // Replaces a standalone word regardless of case.
        assert_eq!(replace_phrase_ci("Wisper", "wisper", "whisper"), "whisper");
        // Does NOT touch the substring inside a longer word.
        assert_eq!(replace_phrase_ci("wispery", "wisper", "whisper"), "wispery");
        // Hits every standalone occurrence, any case.
        assert_eq!(
            replace_phrase_ci("I love wisper and Wisper", "wisper", "whisper"),
            "I love whisper and whisper"
        );
    }

    #[test]
    fn replace_phrase_ci_handles_multi_word_triggers() {
        assert_eq!(
            replace_phrase_ci("call my email now", "my email", "X"),
            "call X now"
        );
    }

    #[test]
    fn replace_phrase_ci_empty_pattern_is_identity() {
        assert_eq!(replace_phrase_ci("unchanged", "", "X"), "unchanged");
    }

    #[test]
    fn apply_corrections_applies_multiple_entries() {
        let mut corrections = BTreeMap::new();
        corrections.insert("wisper".to_string(), "whisper".to_string());
        corrections.insert("hyprland".to_string(), "Hyprland".to_string());

        let out = apply_corrections("wisper on hyprland", &corrections);
        assert_eq!(out, "whisper on Hyprland");
    }

    #[test]
    fn apply_corrections_empty_is_identity() {
        let corrections = BTreeMap::new();
        assert_eq!(apply_corrections("untouched", &corrections), "untouched");
    }

    #[test]
    fn apply_snippets_substitutes_literal_trigger() {
        let mut entries = BTreeMap::new();
        entries.insert("brb".to_string(), "be right back".to_string());

        assert_eq!(apply_snippets("brb", &entries, &stub("")), "be right back");
    }

    #[test]
    fn apply_snippets_resolves_clipboard_placeholder_via_backend() {
        let mut entries = BTreeMap::new();
        entries.insert("paste".to_string(), "<{{CLIPBOARD}}>".to_string());

        // The placeholder is filled from the backend; whitespace is preserved (no trim).
        assert_eq!(
            apply_snippets("paste", &entries, &stub("  hi  ")),
            "<  hi  >"
        );
    }

    #[test]
    fn apply_snippets_prefers_longer_overlapping_trigger() {
        // Both triggers could match the prefix; the longer one must win.
        let mut entries = BTreeMap::new();
        entries.insert("my email".to_string(), "SHORT".to_string());
        entries.insert("my email address".to_string(), "LONG".to_string());

        // BTreeMap iterates "my email" before "my email address"; sorting by length
        // descending ensures the longer trigger is consumed first.
        assert_eq!(
            apply_snippets("send my email address", &entries, &stub("")),
            "send LONG"
        );
        // The shorter trigger still works when the longer one cannot match.
        assert_eq!(
            apply_snippets("send my email", &entries, &stub("")),
            "send SHORT"
        );
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

    #[test]
    fn pick_workflow_prefers_explicit_request_case_insensitive() {
        let workflows = vec![
            Workflow {
                name: "Email".to_string(),
                prompt: "p".to_string(),
                apps: vec!["thunderbird".to_string()],
            },
            Workflow {
                name: "Code".to_string(),
                prompt: "p".to_string(),
                apps: vec!["code".to_string()],
            },
        ];

        // Explicit request matches by name, ignoring case.
        let picked = pick_workflow(&workflows, Some("email"), Some("code")).unwrap();
        assert_eq!(picked.name, "Email");

        // The explicit request takes precedence over the window class.
        let picked = pick_workflow(&workflows, Some("CODE"), Some("thunderbird")).unwrap();
        assert_eq!(picked.name, "Code");
    }

    #[test]
    fn pick_workflow_matches_window_class_when_no_request() {
        let workflows = vec![Workflow {
            name: "Email".to_string(),
            prompt: "p".to_string(),
            apps: vec!["Thunderbird".to_string()],
        }];

        // App match is case-insensitive.
        let picked = pick_workflow(&workflows, None, Some("thunderbird")).unwrap();
        assert_eq!(picked.name, "Email");
    }

    #[test]
    fn pick_workflow_returns_none_without_match() {
        let workflows = vec![Workflow {
            name: "Email".to_string(),
            prompt: "p".to_string(),
            apps: vec!["thunderbird".to_string()],
        }];

        // Unknown explicit request.
        assert!(pick_workflow(&workflows, Some("nope"), None).is_none());
        // Unknown window class.
        assert!(pick_workflow(&workflows, None, Some("firefox")).is_none());
        // Neither provided.
        assert!(pick_workflow(&workflows, None, None).is_none());
    }
}
