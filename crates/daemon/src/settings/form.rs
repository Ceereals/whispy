//! Pure state of the settings screen: sections, cursors, chosen values, and the
//! unsaved-changes flag. No I/O and no terminal types, so navigation and editing
//! are unit-tested; [`super::run`] maps keys to [`Input`] and acts on [`Effect`].

/// Which setting a section edits (or the read-only history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKey {
    /// `audio.source`.
    Microphone,
    /// `stt.language`.
    Language,
    /// `injection.mode`.
    Typing,
    /// `audio.silence_timeout_ms`.
    AutoStop,
    /// Recent transcripts (read-only).
    History,
}

/// One selectable option: the label shown, the config value it stores, and a
/// dim detail shown after the label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub value: String,
    pub detail: String,
}

impl Choice {
    pub fn new(
        label: impl Into<String>,
        value: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            detail: detail.into(),
        }
    }
}

/// One line of the transcript history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Whether the transcript was accepted (typed) or dropped.
    pub accepted: bool,
    /// Why it was dropped, when it was.
    pub reason: Option<String>,
    /// The transcript text.
    pub text: String,
}

/// What a section holds.
#[derive(Debug, Clone)]
pub enum Body {
    /// Pick exactly one of `choices`; `chosen` indexes the selected one.
    Radio { choices: Vec<Choice>, chosen: usize },
    /// Read-only transcript history, newest first.
    History(Vec<HistoryEntry>),
}

/// A titled section of the screen.
#[derive(Debug, Clone)]
pub struct Section {
    pub key: SectionKey,
    pub title: &'static str,
    /// One-line explanation shown above the options.
    pub hint: &'static str,
    pub body: Body,
}

impl Section {
    /// Number of rows the cursor can move over.
    fn rows(&self) -> usize {
        match &self.body {
            Body::Radio { choices, .. } => choices.len(),
            Body::History(entries) => entries.len(),
        }
    }
}

/// A key press, already decoded from the terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    NextSection,
    PrevSection,
    Select,
    Save,
    Quit,
    Test,
}

/// What the driver should do after an [`Input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing beyond redrawing.
    None,
    /// The cursor or section changed: retarget the level meter.
    Moved,
    /// Write the config and restart the daemon.
    Save,
    /// Quit was pressed with unsaved changes; a second press quits.
    ConfirmQuit,
    /// Leave the settings screen.
    Quit,
    /// Start or stop a test dictation on the highlighted microphone.
    Test,
}

/// The whole screen state.
#[derive(Debug, Clone)]
pub struct Form {
    pub sections: Vec<Section>,
    /// Index of the focused section.
    pub section: usize,
    /// Cursor row per section.
    cursors: Vec<usize>,
    /// True when a choice differs from what was loaded or last saved.
    pub dirty: bool,
    quit_armed: bool,
}

impl Form {
    /// Build a form whose cursors start on each section's chosen option.
    pub fn new(sections: Vec<Section>) -> Self {
        let cursors = sections
            .iter()
            .map(|s| match &s.body {
                Body::Radio { chosen, .. } => *chosen,
                Body::History(_) => 0,
            })
            .collect();
        Self {
            sections,
            section: 0,
            cursors,
            dirty: false,
            quit_armed: false,
        }
    }

    /// The focused section.
    pub fn current(&self) -> &Section {
        &self.sections[self.section]
    }

    /// Cursor row in the focused section.
    pub fn cursor(&self) -> usize {
        self.cursors[self.section]
    }

    /// The choice under the cursor, if the focused section is a radio list.
    pub fn highlighted(&self) -> Option<&Choice> {
        match &self.current().body {
            Body::Radio { choices, .. } => choices.get(self.cursor()),
            Body::History(_) => None,
        }
    }

    /// The chosen config value of the section `key`.
    pub fn value(&self, key: SectionKey) -> Option<&str> {
        self.sections
            .iter()
            .find(|s| s.key == key)
            .and_then(|s| match &s.body {
                Body::Radio { choices, chosen } => choices.get(*chosen).map(|c| c.value.as_str()),
                Body::History(_) => None,
            })
    }

    /// Replace the history entries (keeps the cursor in range).
    pub fn set_history(&mut self, entries: Vec<HistoryEntry>) {
        for (i, s) in self.sections.iter_mut().enumerate() {
            if s.key == SectionKey::History {
                self.cursors[i] = self.cursors[i].min(entries.len().saturating_sub(1));
                s.body = Body::History(entries);
                return;
            }
        }
    }

    /// The current changes were written: clear the unsaved flag.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Apply one key press.
    pub fn handle(&mut self, input: Input) -> Effect {
        if input != Input::Quit {
            self.quit_armed = false;
        }
        match input {
            Input::Up => self.move_cursor(-1),
            Input::Down => self.move_cursor(1),
            Input::NextSection => self.move_section(1),
            Input::PrevSection => self.move_section(-1),
            Input::Select => {
                let cursor = self.cursor();
                if let Body::Radio { choices, chosen } = &mut self.sections[self.section].body
                    && cursor < choices.len()
                    && *chosen != cursor
                {
                    *chosen = cursor;
                    self.dirty = true;
                }
                Effect::None
            }
            Input::Save if self.dirty => Effect::Save,
            Input::Quit if self.dirty && !self.quit_armed => {
                self.quit_armed = true;
                Effect::ConfirmQuit
            }
            Input::Quit => Effect::Quit,
            Input::Test if self.current().key == SectionKey::Microphone => Effect::Test,
            Input::Save | Input::Test => Effect::None,
        }
    }

    fn move_cursor(&mut self, delta: isize) -> Effect {
        let rows = self.current().rows();
        if rows == 0 {
            return Effect::None;
        }
        let cursor = &mut self.cursors[self.section];
        let next = cursor.saturating_add_signed(delta).min(rows - 1);
        if next == *cursor {
            return Effect::None;
        }
        *cursor = next;
        Effect::Moved
    }

    fn move_section(&mut self, delta: isize) -> Effect {
        let n = self.sections.len();
        let next = self.section.cast_signed() + delta;
        self.section = next.rem_euclid(n.cast_signed()).cast_unsigned();
        Effect::Moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(key: SectionKey, values: &[&str], chosen: usize) -> Section {
        Section {
            key,
            title: "t",
            hint: "h",
            body: Body::Radio {
                choices: values.iter().map(|v| Choice::new(*v, *v, "")).collect(),
                chosen,
            },
        }
    }

    fn form() -> Form {
        Form::new(vec![
            radio(SectionKey::Microphone, &["", "podmic"], 0),
            radio(SectionKey::Language, &["auto", "it", "en"], 1),
            Section {
                key: SectionKey::History,
                title: "h",
                hint: "",
                body: Body::History(vec![]),
            },
        ])
    }

    #[test]
    fn cursor_starts_on_the_chosen_option_and_clamps() {
        let mut f = form();
        f.handle(Input::NextSection);
        assert_eq!(f.cursor(), 1);
        assert_eq!(f.handle(Input::Down), Effect::Moved);
        assert_eq!(
            f.handle(Input::Down),
            Effect::None,
            "clamped at the last row"
        );
        assert_eq!(f.cursor(), 2);
        f.handle(Input::Up);
        f.handle(Input::Up);
        assert_eq!(
            f.handle(Input::Up),
            Effect::None,
            "clamped at the first row"
        );
    }

    #[test]
    fn sections_wrap_around() {
        let mut f = form();
        f.handle(Input::PrevSection);
        assert_eq!(f.current().key, SectionKey::History);
        f.handle(Input::NextSection);
        assert_eq!(f.current().key, SectionKey::Microphone);
    }

    #[test]
    fn select_changes_the_value_and_marks_dirty() {
        let mut f = form();
        assert_eq!(f.handle(Input::Save), Effect::None, "nothing to save yet");
        f.handle(Input::Down);
        assert_eq!(f.highlighted().map(|c| c.value.as_str()), Some("podmic"));
        f.handle(Input::Select);
        assert_eq!(f.value(SectionKey::Microphone), Some("podmic"));
        assert!(f.dirty);
        assert_eq!(f.handle(Input::Save), Effect::Save);
        f.mark_saved();
        assert!(!f.dirty);
    }

    #[test]
    fn reselecting_the_chosen_option_is_not_a_change() {
        let mut f = form();
        f.handle(Input::Select);
        assert!(!f.dirty);
    }

    #[test]
    fn quit_with_unsaved_changes_needs_a_second_press() {
        let mut f = form();
        assert_eq!(
            f.handle(Input::Quit),
            Effect::Quit,
            "clean form quits at once"
        );
        f.handle(Input::Down);
        f.handle(Input::Select);
        assert_eq!(f.handle(Input::Quit), Effect::ConfirmQuit);
        assert_eq!(f.handle(Input::Quit), Effect::Quit);
        // Any other key disarms the confirmation.
        f.handle(Input::Quit);
        f.handle(Input::Up);
        assert_eq!(f.handle(Input::Quit), Effect::ConfirmQuit);
    }

    #[test]
    fn test_dictation_only_from_the_microphone_section() {
        let mut f = form();
        assert_eq!(f.handle(Input::Test), Effect::Test);
        f.handle(Input::NextSection);
        assert_eq!(f.handle(Input::Test), Effect::None);
    }

    #[test]
    fn history_replaces_entries_and_has_no_value() {
        let mut f = form();
        f.set_history(vec![HistoryEntry {
            accepted: true,
            reason: None,
            text: "ciao".into(),
        }]);
        assert_eq!(f.value(SectionKey::History), None);
        f.handle(Input::PrevSection);
        assert!(f.highlighted().is_none());
    }
}
