//! Drawing the settings screen. Colors are terminal palette colors only (no RGB),
//! so the screen follows the terminal theme — which Omarchy sets from the current
//! Omarchy theme, like its other TUIs.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::form::{Body, Choice, Form, HistoryEntry, SectionKey};

/// Accent for focus, chosen options, and the meter.
const ACCENT: Color = Color::Blue;
/// Secondary text and borders.
const DIM: Color = Color::DarkGray;

/// Everything the screen shows besides the form itself.
pub struct View<'a> {
    pub form: &'a Form,
    /// Meter fill in `[0, 1]`.
    pub level: f32,
    /// One-line status message (save result, test transcript, …).
    pub status: Option<&'a str>,
    /// A test dictation is recording.
    pub testing: bool,
    pub config_path: &'a Path,
}

/// Draw the whole screen.
pub fn render(frame: &mut Frame<'_>, view: &View<'_>) {
    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(Line::from(vec![
            Span::styled(
                " whispy ",
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("settings ", Style::new().fg(DIM)),
        ]))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {} ", view.config_path.display()),
                Style::new().fg(DIM),
            ))
            .right_aligned(),
        );
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let [main, bar] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let [side, content] =
        Layout::horizontal([Constraint::Length(18), Constraint::Min(0)]).areas(main);
    render_sidebar(frame, side, view.form);
    render_section(frame, content, view);
    render_bar(frame, bar, view);
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, form: &Form) {
    let items: Vec<ListItem<'_>> = form
        .sections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = if i == form.section {
                Style::new()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(Span::styled(format!(" {} ", s.title), style)))
        })
        .collect();
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(DIM));
    frame.render_widget(List::new(items).block(block), area);
}

fn render_section(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let section = view.form.current();
    let area = area.inner(ratatui::layout::Margin::new(2, 1));
    let footer = if section.key == SectionKey::Microphone {
        3
    } else {
        0
    };
    let [title, hint, list, mic] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(footer),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            section.title,
            Style::new().add_modifier(Modifier::BOLD),
        )),
        title,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(section.hint, Style::new().fg(DIM))).wrap(Wrap { trim: true }),
        hint,
    );

    let cursor = view.form.cursor();
    let items: Vec<ListItem<'_>> = match &section.body {
        Body::Radio { choices, chosen } => choices
            .iter()
            .enumerate()
            .map(|(i, c)| radio_row(c, i == *chosen, i == cursor))
            .collect(),
        Body::History(entries) if entries.is_empty() => {
            vec![ListItem::new(Span::styled(
                "No transcripts yet.",
                Style::new().fg(DIM),
            ))]
        }
        Body::History(entries) => entries
            .iter()
            .enumerate()
            .map(|(i, e)| history_row(e, i == cursor))
            .collect(),
    };
    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(List::new(items), list, &mut state);

    if footer > 0 {
        render_mic(frame, mic, view);
    }
}

fn radio_row(choice: &Choice, chosen: bool, under_cursor: bool) -> ListItem<'_> {
    let pointer = if under_cursor { "› " } else { "  " };
    let (mark, mark_style) = if chosen {
        ("● ", Style::new().fg(ACCENT))
    } else {
        ("○ ", Style::new().fg(DIM))
    };
    let label_style = if under_cursor {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(pointer, Style::new().fg(ACCENT)),
        Span::styled(mark, mark_style),
        Span::styled(choice.label.as_str(), label_style),
    ];
    if !choice.detail.is_empty() {
        spans.push(Span::styled(
            format!("  {}", choice.detail),
            Style::new().fg(DIM),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn history_row(entry: &HistoryEntry, under_cursor: bool) -> ListItem<'_> {
    let pointer = Span::styled(
        if under_cursor { "› " } else { "  " },
        Style::new().fg(ACCENT),
    );
    let line = if entry.accepted {
        Line::from(vec![
            pointer,
            Span::styled("✓ ", Style::new().fg(Color::Green)),
            Span::raw(entry.text.as_str()),
        ])
    } else {
        Line::from(vec![
            pointer,
            Span::styled("✗ ", Style::new().fg(Color::Red)),
            Span::styled(
                format!("{}  ", entry.reason.as_deref().unwrap_or("dropped")),
                Style::new().fg(Color::Red),
            ),
            Span::styled(entry.text.as_str(), Style::new().fg(DIM)),
        ])
    };
    ListItem::new(line)
}

fn render_mic(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let [_, meter, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let label = "Level  ";
    let width = usize::from(meter.width).saturating_sub(label.len());
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "fill is clamped to [0, 1] and the width is a terminal column count"
    )]
    let filled = ((view.level.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    let meter_color = if view.testing { Color::Red } else { ACCENT };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, Style::new().fg(DIM)),
            Span::styled("━".repeat(filled), Style::new().fg(meter_color)),
            Span::styled("━".repeat(width - filled), Style::new().fg(DIM)),
        ])),
        meter,
    );

    let help_line = if view.testing {
        Line::from(vec![
            Span::styled(
                "● Recording",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" — speak, then press t to transcribe", Style::new().fg(DIM)),
        ])
    } else {
        Line::from(vec![
            Span::styled("t ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                "test dictation with the highlighted microphone",
                Style::new().fg(DIM),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(help_line), help);
}

fn render_bar(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let dirty = if view.form.dirty { "● unsaved " } else { "" };
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(dirty.chars().count()).unwrap_or(0)),
    ])
    .areas(area);

    let left_line = if let Some(status) = view.status {
        Line::from(Span::raw(format!(" {status}")))
    } else {
        let key =
            |k: &'static str| Span::styled(k, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));
        let txt = |t: &'static str| Span::styled(t, Style::new().fg(DIM));
        Line::from(vec![
            txt(" "),
            key("↑↓"),
            txt(" move  "),
            key("⇥"),
            txt(" section  "),
            key("⏎"),
            txt(" choose  "),
            key("s"),
            txt(" save  "),
            key("q"),
            txt(" quit"),
        ])
    };
    frame.render_widget(Paragraph::new(left_line), left);
    frame.render_widget(
        Paragraph::new(Span::styled(dirty, Style::new().fg(Color::Yellow))),
        right,
    );
}
