//! `whispy-daemon settings`: a terminal UI for the everyday settings — microphone
//! (with a live level meter and a test dictation), speech engine, language,
//! typing mode, and the silence auto-stop — plus the recent transcript history.
//!
//! It edits the user's `config.toml` in place (comments kept, see [`editable`]) and
//! restarts the daemon on save, since the daemon reads its config only at start.
//! Meant to run in a floating terminal, like Omarchy's own TUIs:
//! `omarchy-launch-or-focus-tui --app-id=TUI.float whispy-daemon settings`.

mod editable;
mod form;
mod mic;
mod sources;
mod ui;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::{self, Config};
use crate::proc;
use crate::stt::SttClient;
use form::{Body, Effect, Form, HistoryEntry, Input, Section, SectionKey};
use mic::Mic;

/// Redraw cadence (also how often the meter updates).
const TICK: Duration = Duration::from_millis(50);
/// How many transcripts the History section shows.
const HISTORY_LEN: usize = 50;
/// Terminal title; the Omarchy bar widget matches it to focus an open window.
const WINDOW_TITLE: &str = "whispy-settings";
/// Bound on `systemctl --user try-restart`.
const RESTART_TIMEOUT: Duration = Duration::from_secs(20);

/// Run the settings screen until the user quits.
pub fn run(cfg: &Config, config_path: Option<&Path>) -> ExitCode {
    let Some(path) = config_path
        .map(Path::to_path_buf)
        .or_else(config::user_config_path)
    else {
        eprintln!("whispy-daemon settings: cannot locate the config (no $HOME)");
        return ExitCode::FAILURE;
    };
    let mut app = App::new(cfg.clone(), path);
    let mut terminal = ratatui::init();
    // The bar widget finds an open settings window by this title and focuses it
    // (`omarchy-launch-or-focus whispy-settings …`) instead of opening another.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::SetTitle(WINDOW_TITLE)
    )
    .ok();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("whispy-daemon settings: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The running screen: form state plus the side effects it drives.
struct App {
    cfg: Config,
    path: PathBuf,
    form: Form,
    mic: Mic,
    status: Option<String>,
    /// Pending test transcription result.
    test_result: Option<Receiver<String>>,
}

impl App {
    fn new(cfg: Config, path: PathBuf) -> Self {
        let (sources, default_source) = sources::list();
        let mut sections = editable::sections(&cfg, &sources, default_source.as_deref());
        sections.push(Section {
            key: SectionKey::History,
            title: "History",
            hint: "Latest dictations: ✓ typed, ✗ dropped by the filter (with the reason).",
            body: Body::History(Vec::new()),
        });
        let form = Form::new(sections);
        let mic = Mic::new(cfg.audio.clone());
        Self {
            cfg,
            path,
            form,
            mic,
            status: None,
            test_result: None,
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        self.on_moved();
        loop {
            self.tick();
            let status = self.status.clone();
            terminal.draw(|frame| {
                ui::render(
                    frame,
                    &ui::View {
                        form: &self.form,
                        level: mic::meter_fill(self.mic.level()),
                        status: status.as_deref(),
                        testing: self.mic.is_testing(),
                        config_path: &self.path,
                    },
                );
            })?;
            if !event::poll(TICK)? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(());
            }
            let Some(input) = decode(key) else {
                continue;
            };
            self.status = None;
            match self.form.handle(input) {
                Effect::None => {}
                Effect::Moved => self.on_moved(),
                Effect::Save => self.save(),
                Effect::ConfirmQuit => {
                    self.status = Some("Unsaved changes — q again to discard, s to save".into());
                }
                Effect::Quit => return Ok(()),
                Effect::Test => self.toggle_test(),
            }
        }
    }

    /// Per-frame work: keep the meter on the highlighted source, collect results.
    fn tick(&mut self) {
        if self.form.current().key == SectionKey::Microphone {
            let source = self
                .form
                .highlighted()
                .map(|c| c.value.clone())
                .unwrap_or_default();
            self.mic.listen(&source);
        }
        if let Some(rx) = &self.test_result
            && let Ok(text) = rx.try_recv()
        {
            self.status = Some(text);
            self.test_result = None;
        }
    }

    /// The focused section or cursor changed.
    fn on_moved(&mut self) {
        match self.form.current().key {
            SectionKey::Microphone => {} // `tick` retargets the meter
            SectionKey::History => {
                self.mic.stop();
                self.form.set_history(load_history());
            }
            _ => self.mic.stop(),
        }
    }

    fn save(&mut self) {
        if let Err(e) = editable::save(&self.path, &self.form) {
            self.status = Some(format!("✗ Not saved: {e}"));
            return;
        }
        self.form.mark_saved();
        let restarted = proc::status(
            Command::new("systemctl").args(["--user", "try-restart", "whispy-daemon.service"]),
            RESTART_TIMEOUT,
        )
        .is_ok_and(|s| s.success());
        self.status = Some(if restarted {
            "✓ Saved · whispy restarted with the new settings".into()
        } else {
            "✓ Saved · restart whispy to apply: systemctl --user restart whispy-daemon".into()
        });
    }

    /// `t`: start a test dictation, or stop it and transcribe in the background.
    fn toggle_test(&mut self) {
        if self.test_result.is_some() {
            return; // still transcribing the previous one
        }
        if !self.mic.is_testing() {
            let source = self
                .form
                .highlighted()
                .map(|c| c.value.clone())
                .unwrap_or_default();
            if let Err(e) = self.mic.start_test(&source) {
                self.status = Some(format!("✗ Could not record: {e}"));
            }
            return;
        }
        let Some(samples) = self.mic.finish_test() else {
            self.status = Some("Nothing heard — speak louder or pick another microphone".into());
            return;
        };
        let mut stt = self.cfg.stt.clone();
        if let Some(language) = self.form.value(SectionKey::Language) {
            stt.language = language.to_string();
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let msg = match SttClient::new(&stt, None).transcribe(&samples) {
                Ok(t) if t.text.trim().is_empty() => "Heard nothing intelligible".to_string(),
                Ok(t) => format!(
                    "“{}”  ({}, confidence {:.2})",
                    t.text.trim(),
                    t.language.as_deref().unwrap_or("?"),
                    t.avg_logprob.exp()
                ),
                Err(e) => format!("✗ Transcription failed (is whispy running?): {e:?}"),
            };
            tx.send(msg).ok();
        });
        self.status = Some("Transcribing…".into());
        self.test_result = Some(rx);
    }
}

/// Map a key to a form input (vim keys included).
fn decode(key: KeyEvent) -> Option<Input> {
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => Input::Up,
        KeyCode::Down | KeyCode::Char('j') => Input::Down,
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => Input::NextSection,
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => Input::PrevSection,
        KeyCode::Enter | KeyCode::Char(' ') => Input::Select,
        KeyCode::Char('s') => Input::Save,
        KeyCode::Char('q') | KeyCode::Esc => Input::Quit,
        KeyCode::Char('t') => Input::Test,
        _ => return None,
    })
}

/// The newest transcripts from `transcripts.jsonl`, newest first.
fn load_history() -> Vec<HistoryEntry> {
    let path = config::state_dir().join("transcripts.jsonl");
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    parse_history(
        BufReader::new(file).lines().map_while(Result::ok),
        HISTORY_LEN,
    )
}

/// Parse transcript-log lines into the newest `limit` entries, newest first.
fn parse_history(lines: impl Iterator<Item = String>, limit: usize) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = lines
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(&line).ok()?;
            Some(HistoryEntry {
                accepted: v["accepted"].as_bool().unwrap_or(false),
                reason: v["drop_reason"].as_str().map(String::from),
                text: v["text"].as_str().unwrap_or_default().trim().to_string(),
            })
        })
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_newest_first_and_capped() {
        let lines = [
            r#"{"accepted":true,"text":" old "}"#,
            "not json",
            r#"{"accepted":false,"drop_reason":"hallucination","text":"Grazie."}"#,
            r#"{"accepted":true,"text":"new"}"#,
        ]
        .map(String::from);
        let h = parse_history(lines.into_iter(), 2);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].text, "new");
        assert_eq!(h[1].reason.as_deref(), Some("hallucination"));
        assert!(!h[1].accepted);
    }

    #[test]
    fn keys_decode_including_vim_bindings() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(decode(key(KeyCode::Char('j'))), Some(Input::Down));
        assert_eq!(decode(key(KeyCode::BackTab)), Some(Input::PrevSection));
        assert_eq!(decode(key(KeyCode::Char('t'))), Some(Input::Test));
        assert_eq!(decode(key(KeyCode::Char('x'))), None);
    }
}
