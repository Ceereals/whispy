//! Post-transcription text pipeline: vocabulary correction (Parakeet) ->
//! corrections -> snippets -> AI workflow.
//!
//! A [`TextPipeline`] is prepared once at startup from the config and the
//! engine's vocabulary strategy; [`TextPipeline::process`] then runs every
//! accepted transcript through it and never fails (LLM errors fall back to the
//! text as it stood before the workflow). Snippet placeholders are resolved
//! through a narrow [`PlaceholderEnv`]: [`LiveEnv`] in the daemon (the desktop
//! clipboard and `date`), a fake in tests.

use std::collections::BTreeMap;
use std::process::Command;

use tracing::{info, warn};

use crate::config::{Config, Llm, Workflow};
use crate::desktop::Desktop;
use crate::engine::VocabStrategy;
use crate::proc;

/// Context for one transcript: which workflow was requested and the focused window.
#[derive(Debug, Clone, Default)]
pub struct AppContext {
    /// Workflow name explicitly requested via the IPC command (hotkey choice).
    pub workflow: Option<String>,
    /// The focused window's class at the time recording started.
    pub window_class: Option<String>,
}

/// Where snippet placeholders get their live values. Each method is called only
/// when a matched snippet's replacement uses its placeholder.
pub trait PlaceholderEnv {
    /// `{{CLIPBOARD}}`: the clipboard as text, untrimmed; empty when unavailable.
    fn clipboard_text(&self) -> String;
    /// `{{DATE}}`: today's local date as `YYYY-MM-DD`; empty when unavailable.
    fn date(&self) -> String;
    /// `{{TIME}}`: the local time as `HH:MM`; empty when unavailable.
    fn time(&self) -> String;
}

/// The daemon's [`PlaceholderEnv`]: the clipboard through the [`Desktop`], the
/// date and time through `date` (so they follow the system's local time zone).
pub struct LiveEnv<'a> {
    desktop: &'a dyn Desktop,
}

impl<'a> LiveEnv<'a> {
    /// Resolve `{{CLIPBOARD}}` through `desktop`.
    pub fn new(desktop: &'a dyn Desktop) -> Self {
        Self { desktop }
    }
}

impl PlaceholderEnv for LiveEnv<'_> {
    fn clipboard_text(&self) -> String {
        self.desktop.clipboard_text()
    }

    fn date(&self) -> String {
        shell_date("+%Y-%m-%d")
    }

    fn time(&self) -> String {
        shell_date("+%H:%M")
    }
}

/// The text pipeline, prepared from the config: everything a transcript needs
/// between the hallucination filter and injection.
pub struct TextPipeline {
    /// Custom words to fuzzy-correct transcripts against; empty unless the engine
    /// can't take the vocabulary as a decoding prompt (Parakeet).
    vocabulary: Vec<String>,
    /// Case-insensitive, whole-word replacements, applied in key order.
    corrections: BTreeMap<String, String>,
    /// Snippet (trigger, replacement) pairs, longest trigger first so a longer
    /// trigger wins over a shorter overlapping one.
    snippets: Vec<(String, String)>,
    /// AI workflows, selected per transcript by request or window class.
    workflows: Vec<Workflow>,
    /// The chat endpoint workflows run against.
    llm: Llm,
}

impl TextPipeline {
    /// Prepare the pipeline from `cfg`. `vocab` is the engine's vocabulary
    /// strategy, decided once at startup: only [`VocabStrategy::Correct`] makes
    /// the pipeline correct transcripts (a prompt goes to the STT client instead).
    pub fn new(cfg: &Config, vocab: &VocabStrategy) -> Self {
        let vocabulary = match vocab {
            VocabStrategy::Correct => cfg.dictionary.vocabulary.clone(),
            VocabStrategy::Prompt(_) | VocabStrategy::Off => Vec::new(),
        };
        let mut snippets: Vec<(String, String)> = cfg
            .snippets
            .entries
            .iter()
            .map(|(trigger, replacement)| (trigger.clone(), replacement.clone()))
            .collect();
        snippets.sort_by_key(|(trigger, _)| std::cmp::Reverse(trigger.chars().count()));
        Self {
            vocabulary,
            corrections: cfg.dictionary.corrections.clone(),
            snippets,
            workflows: cfg.workflow.clone(),
            llm: cfg.llm.clone(),
        }
    }

    /// Run an accepted transcript through vocabulary correction, corrections,
    /// snippets (placeholders from `env`), then the workflow `ctx` selects: an
    /// explicit request wins over the window class. Never fails: an LLM error
    /// (or empty reply) leaves the text as the snippets produced it.
    pub fn process(&self, text: String, ctx: &AppContext, env: &dyn PlaceholderEnv) -> String {
        let text = crate::vocab::apply(&text, &self.vocabulary, crate::vocab::DEFAULT_THRESHOLD);
        let text = apply_corrections(&text, &self.corrections);
        let text = self.expand_snippets(&text, env);

        match pick_workflow(
            &self.workflows,
            ctx.workflow.as_deref(),
            ctx.window_class.as_deref(),
        ) {
            Some(wf) if !wf.prompt.trim().is_empty() => {
                match crate::llm::transform(&self.llm, &wf.prompt, &text) {
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

    /// Expand text-expansion snippets, longest trigger first. Placeholders are
    /// resolved live and only for snippets whose trigger is present (each may
    /// read the clipboard or run `date`, so don't pay for every configured
    /// snippet on every transcript).
    fn expand_snippets(&self, text: &str, env: &dyn PlaceholderEnv) -> String {
        let mut acc = text.to_string();
        for (trigger, replacement) in &self.snippets {
            if !contains_phrase_ci(&acc, trigger) {
                continue;
            }
            let resolved = resolve_placeholders(replacement, env);
            acc = replace_phrase_ci(&acc, trigger, &resolved);
        }
        acc
    }
}

/// Pick the workflow for this transcript. An explicit request (hotkey) wins over
/// app-based auto-selection; with neither, no workflow runs.
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
    corrections
        .iter()
        .fold(text.to_string(), |acc, (from, to)| {
            replace_phrase_ci(&acc, from, to)
        })
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

/// Does `text` contain `phrase` as a whole-word, case-insensitive match? The
/// same matching rule as [`replace_phrase_ci`].
fn contains_phrase_ci(text: &str, phrase: &str) -> bool {
    let pat: Vec<char> = phrase.chars().collect();
    if pat.is_empty() {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| matches_at(&chars, i, &pat) && boundary_ok(&chars, i, pat.len()))
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

/// Substitute `{{DATE}}`, `{{TIME}}`, and `{{CLIPBOARD}}` placeholders in a
/// snippet replacement, asking `env` only for the ones it uses.
fn resolve_placeholders(s: &str, env: &dyn PlaceholderEnv) -> String {
    let mut out = s.to_string();
    if out.contains("{{DATE}}") {
        out = out.replace("{{DATE}}", &env.date());
    }
    if out.contains("{{TIME}}") {
        out = out.replace("{{TIME}}", &env.time());
    }
    if out.contains("{{CLIPBOARD}}") {
        out = out.replace("{{CLIPBOARD}}", &env.clipboard_text());
    }
    out
}

/// Format the current time via `date FMT`; empty string on any failure.
fn shell_date(fmt: &str) -> String {
    match proc::output(Command::new("date").arg(fmt), proc::PROBE) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// A fixed clock and clipboard, counting how often a placeholder was resolved.
    struct FakeEnv {
        clipboard: &'static str,
        lookups: Cell<usize>,
    }

    impl FakeEnv {
        fn new(clipboard: &'static str) -> Self {
            Self {
                clipboard,
                lookups: Cell::new(0),
            }
        }
        fn hit(&self) {
            self.lookups.set(self.lookups.get() + 1);
        }
    }

    impl PlaceholderEnv for FakeEnv {
        fn clipboard_text(&self) -> String {
            self.hit();
            self.clipboard.to_string()
        }
        fn date(&self) -> String {
            self.hit();
            "2026-10-29".to_string()
        }
        fn time(&self) -> String {
            self.hit();
            "09:41".to_string()
        }
    }

    /// The built-in defaults with a fast-failing LLM, ready to customize.
    fn config() -> Config {
        let mut cfg: Config =
            toml::from_str(crate::config::default_config_toml()).expect("defaults parse");
        cfg.llm.base_url = closed_port_url();
        cfg.llm.model = "test-model".to_string();
        cfg.llm.timeout_secs = 2;
        cfg
    }

    fn entries(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn workflow(name: &str, prompt: &str, apps: &[&str]) -> Workflow {
        Workflow {
            name: name.to_string(),
            prompt: prompt.to_string(),
            apps: apps.iter().map(ToString::to_string).collect(),
        }
    }

    fn ctx(workflow: Option<&str>, window_class: Option<&str>) -> AppContext {
        AppContext {
            workflow: workflow.map(String::from),
            window_class: window_class.map(String::from),
        }
    }

    /// A loopback URL nothing listens on, so an LLM call fails fast.
    fn closed_port_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        format!("http://127.0.0.1:{port}/v1")
    }

    /// Serve one chat-completions reply of `content` on a loopback port. Returns
    /// the base URL and a receiver for the raw request the pipeline sent.
    fn llm_replying(content: &str) -> (String, mpsc::Receiver<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let body =
            serde_json::json!({ "choices": [{ "message": { "content": content } }] }).to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            // Drain the request before answering.
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
            while let Ok(n @ 1..) = conn.read(&mut buf) {
                request.extend_from_slice(&buf[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&request).to_string());
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    #[test]
    fn process_runs_every_stage_in_order_then_the_window_workflow() {
        let (base_url, request) = llm_replying("REWRITTEN");
        let mut cfg = config();
        cfg.llm.base_url = base_url;
        cfg.dictionary.vocabulary = vec!["Hyprland".to_string()];
        // "stemp" -> "stamp" proves corrections run before snippets.
        cfg.dictionary.corrections = entries(&[("wisper", "whisper"), ("stemp", "stamp")]);
        cfg.snippets.entries =
            entries(&[("stamp", "{{DATE}} {{TIME}}"), ("clip", "<{{CLIPBOARD}}>")]);
        cfg.workflow = vec![workflow("chat", "Rewrite it.", &["kitty"])];
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Correct);

        let out = pipeline.process(
            "wisper on Hyperland stemp clip".to_string(),
            &ctx(None, Some("Kitty")),
            &FakeEnv::new("  board  "),
        );

        assert_eq!(out, "REWRITTEN");
        let request = request
            .recv_timeout(Duration::from_secs(5))
            .expect("LLM was called");
        assert!(request.contains("Rewrite it."), "{request}");
        assert!(
            request.contains("whisper on Hyprland 2026-10-29 09:41 <  board  >"),
            "{request}"
        );
    }

    #[test]
    fn process_falls_back_to_the_processed_text_when_the_llm_fails() {
        let mut cfg = config();
        cfg.dictionary.corrections = entries(&[("wisper", "whisper")]);
        cfg.snippets.entries = entries(&[("today", "{{DATE}}")]);
        cfg.workflow = vec![workflow("chat", "Rewrite it.", &["kitty"])];
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Off);

        let out = pipeline.process(
            "wisper today".to_string(),
            &ctx(None, Some("kitty")),
            &FakeEnv::new(""),
        );
        assert_eq!(out, "whisper 2026-10-29");
    }

    #[test]
    fn explicit_workflow_request_beats_the_window_class() {
        let (base_url, request) = llm_replying("Dear team");
        let mut cfg = config();
        cfg.llm.base_url = base_url;
        cfg.workflow = vec![
            workflow("chat", "CHAT PROMPT", &["kitty"]),
            workflow("email", "EMAIL PROMPT", &[]),
        ];
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Off);

        let out = pipeline.process(
            "hello".to_string(),
            &ctx(Some("Email"), Some("kitty")),
            &FakeEnv::new(""),
        );
        assert_eq!(out, "Dear team");
        let request = request
            .recv_timeout(Duration::from_secs(5))
            .expect("LLM was called");
        assert!(request.contains("EMAIL PROMPT"), "{request}");
        assert!(!request.contains("CHAT PROMPT"), "{request}");
    }

    #[test]
    fn vocabulary_is_corrected_only_when_the_engine_asks_for_it() {
        let mut cfg = config();
        cfg.dictionary.vocabulary = vec!["Hyprland".to_string()];
        let text = || "I use Hyperland".to_string();
        let env = FakeEnv::new("");

        let parakeet = TextPipeline::new(&cfg, &VocabStrategy::Correct);
        assert_eq!(
            parakeet.process(text(), &ctx(None, None), &env),
            "I use Hyprland"
        );

        let whisper = TextPipeline::new(&cfg, &VocabStrategy::Prompt("Hyprland".to_string()));
        assert_eq!(
            whisper.process(text(), &ctx(None, None), &env),
            "I use Hyperland"
        );
    }

    #[test]
    fn snippets_substitute_literal_triggers_and_keep_clipboard_whitespace() {
        let mut cfg = config();
        cfg.snippets.entries = entries(&[("brb", "be right back"), ("paste", "<{{CLIPBOARD}}>")]);
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Off);
        let env = FakeEnv::new("  hi  ");

        assert_eq!(
            pipeline.process("brb".to_string(), &ctx(None, None), &env),
            "be right back"
        );
        assert_eq!(
            pipeline.process("paste".to_string(), &ctx(None, None), &env),
            "<  hi  >"
        );
    }

    #[test]
    fn snippets_resolve_placeholders_only_for_present_triggers() {
        let mut cfg = config();
        cfg.snippets.entries =
            entries(&[("paste", "<{{CLIPBOARD}}>"), ("stamp", "{{DATE}} {{TIME}}")]);
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Off);
        let env = FakeEnv::new("secret");

        assert_eq!(
            pipeline.process("hello world".to_string(), &ctx(None, None), &env),
            "hello world"
        );
        // A word merely containing a trigger doesn't count either.
        assert_eq!(
            pipeline.process("pasted stamps".to_string(), &ctx(None, None), &env),
            "pasted stamps"
        );
        assert_eq!(
            env.lookups.get(),
            0,
            "placeholder resolved for an absent trigger"
        );
    }

    #[test]
    fn snippets_prefer_the_longer_overlapping_trigger() {
        // BTreeMap iterates "my email" before "my email address"; the longer
        // trigger must still be consumed first.
        let mut cfg = config();
        cfg.snippets.entries = entries(&[("my email", "SHORT"), ("my email address", "LONG")]);
        let pipeline = TextPipeline::new(&cfg, &VocabStrategy::Off);
        let env = FakeEnv::new("");

        assert_eq!(
            pipeline.process("send my email address".to_string(), &ctx(None, None), &env),
            "send LONG"
        );
        // The shorter trigger still works when the longer one cannot match.
        assert_eq!(
            pipeline.process("send my email".to_string(), &ctx(None, None), &env),
            "send SHORT"
        );
    }

    #[test]
    fn live_env_formats_date_and_time() {
        let desktop = crate::desktop::connect(crate::display::DisplayServer::Wayland);
        let env = LiveEnv::new(desktop.as_ref());
        let shape = |s: &str| {
            s.chars()
                .map(|c| if c.is_ascii_digit() { 'D' } else { c })
                .collect::<String>()
        };
        assert_eq!(shape(&env.date()), "DDDD-DD-DD");
        assert_eq!(shape(&env.time()), "DD:DD");
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
    fn contains_phrase_ci_matches_whole_words_only() {
        assert!(contains_phrase_ci("Please PASTE here", "paste"));
        assert!(contains_phrase_ci("send my email now", "my email"));
        assert!(!contains_phrase_ci("pasted", "paste"));
        assert!(!contains_phrase_ci("anything", ""));
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
