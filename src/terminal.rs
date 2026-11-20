use std::io::{self, IsTerminal};

use anyhow::{Result, bail};
use crossterm::event::{
    Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use elphie_core::Role;
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::view_model::{Action, Chat, Event, ModelChoice, ModelPicker, Phase};

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        ratatui::restore();
    }
}

fn visible(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

pub async fn run(
    provider: &str,
    model: &str,
    actions: UnboundedSender<Action>,
    mut events: UnboundedReceiver<Event>,
) -> Result<()> {
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        bail!("Open a terminal for chat, or run elphie serve for ACP");
    }
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    #[cfg(unix)]
    crossterm::execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    let mut inputs = EventStream::new();
    let mut chat = Chat {
        model: model.to_owned(),
        ..Chat::default()
    };
    let mut above_bottom: u16 = 0;
    loop {
        terminal.draw(|frame| draw(frame, &chat, provider, &mut above_bottom))?;
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                chat.apply(event);
            }
            input = inputs.next() => {
                let Some(input) = input else { break };
                match input? {
                    Input::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            break;
                        }
                        if let Some(picker) = chat.pickers.front_mut() {
                            match key.code {
                                KeyCode::Esc => chat.dismiss_model(ModelChoice::Cancel),
                                KeyCode::Char('d' | 'D') if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => chat.dismiss_model(ModelChoice::Decline),
                                KeyCode::Enter => chat.choose_model(),
                                KeyCode::Up => picker.previous(),
                                KeyCode::Down => picker.next(),
                                _ => {}
                            }
                            continue;
                        }
                        if let Some(action) = compose(&mut chat, key, &mut above_bottom) {
                            above_bottom = 0;
                            actions.send(action)?;
                        }
                    }
                    Input::Paste(paste) if chat.pickers.is_empty() => {
                        let text = paste.replace("\r\n", "\n").replace('\r', "\n");
                        chat.draft.insert(&visible(&text));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn compose(chat: &mut Chat, key: KeyEvent, above_bottom: &mut u16) -> Option<Action> {
    match key.code {
        KeyCode::Esc if matches!(chat.phase, Phase::Working | Phase::LoadingModels) => {
            return Some(Action::Cancel);
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            chat.draft.insert("\n")
        }
        KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => chat.draft.insert("\n"),
        KeyCode::Tab => return chat.complete_command(),
        KeyCode::Enter => return chat.complete_command().or_else(|| chat.submit()),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && !c.is_control() =>
        {
            return chat.type_char(c);
        }
        KeyCode::PageUp => *above_bottom = above_bottom.saturating_add(8),
        KeyCode::PageDown => *above_bottom = above_bottom.saturating_sub(8),
        KeyCode::Left => chat.draft.left(),
        KeyCode::Right => chat.draft.right(),
        KeyCode::Up => chat.draft.up(),
        KeyCode::Down => chat.draft.down(),
        KeyCode::Delete => chat.draft.delete(),
        KeyCode::Backspace => chat.draft.backspace(),
        _ => {}
    }
    None
}

fn draw_picker(frame: &mut Frame, area: Rect, picker: &ModelPicker) {
    let items = picker.models.iter().enumerate().map(|(index, model)| {
        let label: String = model.name.chars().filter(|c| !c.is_control()).collect();
        if picker.current == Some(index) {
            format!("{label} (current)")
        } else {
            label
        }
    });
    let mut list = List::new(items)
        .highlight_style(Style::new().cyan().bold())
        .highlight_symbol("› ");
    if area.height >= 3 && area.width >= 4 {
        let message: String = picker.message.chars().filter(|c| !c.is_control()).collect();
        list = list.block(Block::bordered().title(format!(" Elphie · {message} ")));
    }
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draft_lines(before: &str, after: &str, width: usize) -> (Vec<Line<'static>>, usize) {
    let mut rows = Vec::new();
    let mut caret_row = 0;
    let mut byte = 0;
    for line in format!("{before}▏{after}").split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for token in line.split_inclusive(char::is_whitespace) {
            let word_width = Span::raw(token.trim_end()).width();
            if used > 0 && used + word_width > width {
                rows.push(Line::raw(std::mem::take(&mut row)));
                used = 0;
            }
            for grapheme in Span::raw(token).styled_graphemes(Style::default()) {
                let columns = Span::raw(grapheme.symbol).width();
                if used > 0 && used + columns > width {
                    rows.push(Line::raw(std::mem::take(&mut row)));
                    used = 0;
                }
                let next_byte = byte + grapheme.symbol.len();
                if (byte..next_byte).contains(&before.len()) {
                    caret_row = rows.len();
                }
                row.push_str(grapheme.symbol);
                used += columns;
                byte = next_byte;
            }
        }
        rows.push(Line::raw(row));
        byte += 1;
    }
    (rows, caret_row)
}

fn draw(frame: &mut Frame, chat: &Chat, provider: &str, above_bottom: &mut u16) {
    let area = frame.area();
    if let Some(picker) = chat.pickers.front()
        && area.height < 5
    {
        draw_picker(frame, area, picker);
        return;
    }
    let width = area.width.max(1);
    let (before, after) = chat.draft.parts();
    let text_width = width.saturating_sub(2).max(1);
    let (draft, caret_row) = draft_lines(before, after, usize::from(text_width));
    let input_height = match chat.pickers.front() {
        Some(picker) => (picker.models.len().min(8) + 2) as u16,
        None => (draft.len().min(4) + 2) as u16,
    };
    let mut transcript = Vec::new();
    for (_, message) in &chat.entries {
        let role = match message.role {
            Role::User => "You",
            Role::Assistant => "Elphie",
        };
        transcript.push(Line::styled(
            role,
            ratatui::style::Style::new().fg(Color::Cyan).bold(),
        ));
        transcript.extend(
            visible(&message.text)
                .split('\n')
                .map(|line| Line::raw(line.to_owned())),
        );
        transcript.push(Line::raw(""));
    }
    if let Some(error) = &chat.error {
        transcript.push(Line::styled(
            visible(error),
            ratatui::style::Style::new().fg(Color::Red),
        ));
    }
    let suggest = chat.suggests_model() && area.height >= 6;
    let [header, history, suggestion, compose, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(u16::from(suggest)),
        Constraint::Length(input_height.max(3)),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Line::from(vec![
            "Elphie  ".bold(),
            format!("{provider} / {}", visible(&chat.model)).dark_gray(),
        ]),
        header,
    );
    let history_view = Paragraph::new(transcript).wrap(Wrap { trim: false });
    let end = history_view
        .line_count(width)
        .saturating_sub(history.height as usize)
        .min(u16::MAX as usize) as u16;
    *above_bottom = (*above_bottom).min(end);
    frame.render_widget(history_view.scroll((end - *above_bottom, 0)), history);
    if suggest {
        frame.render_widget(
            Line::from(vec!["› /model  ".cyan().bold(), "Select a model".into()]),
            suggestion,
        );
    }
    if let Some(picker) = chat.pickers.front() {
        draw_picker(frame, compose, picker);
        frame.render_widget(
            Line::raw("↑/↓ move  ·  Enter select  ·  D decline  ·  Esc cancel  ·  Ctrl+C quit"),
            footer,
        );
        return;
    }
    let height = usize::from(compose.height.saturating_sub(2));
    let start = (caret_row + 1).saturating_sub(height);
    let visible = Text::from_iter(draft.into_iter().skip(start).take(height));
    frame.render_widget(
        Paragraph::new(visible).block(Block::bordered().title(" Message ")),
        compose,
    );
    let help = match chat.phase {
        _ if chat.suggests_model() => "Tab/Enter open",
        Phase::Ready => "Enter send  ·  Shift+Enter newline  ·  /model",
        Phase::Connecting => "Connecting…",
        Phase::Working => "Working…  Esc cancel",
        Phase::LoadingModels => "Loading models…  Esc cancel",
        Phase::Cancelled => "Cancelled  ·  Enter send",
    };
    frame.render_widget(
        Line::raw(format!("{help}  ·  PgUp/PgDn scroll  ·  Ctrl+C quit")),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn partial_command_renders_a_selected_suggestion_above_the_draft() {
        let mut chat = Chat {
            phase: Phase::Ready,
            ..Chat::default()
        };
        chat.draft.insert("/mo");
        let mut terminal = Terminal::new(TestBackend::new(45, 8)).unwrap();
        terminal
            .draw(|frame| draw(frame, &chat, "ollama", &mut 0))
            .unwrap();
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(45)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let hint = rows
            .iter()
            .position(|line| line.contains("› /model  Select a model"))
            .unwrap();
        assert!(rows[hint + 2].contains("/mo▏"));
    }

    #[test]
    fn typing_or_completing_a_command_opens_the_picker_and_multiline_text_stays_a_message() {
        let mut chat = Chat {
            phase: Phase::Ready,
            ..Chat::default()
        };
        for ch in "/mode".chars() {
            assert!(chat.type_char(ch).is_none());
            assert!(chat.suggests_model());
        }
        assert!(matches!(chat.type_char('l'), Some(Action::Send(command)) if command == "/model"));
        assert!(matches!(chat.phase, Phase::LoadingModels));
        chat.apply(Event::Phase(Phase::Ready));
        chat.type_char('/');
        let tab = KeyEvent::from(KeyCode::Tab);
        assert!(
            matches!(compose(&mut chat, tab, &mut 0), Some(Action::Send(command)) if command == "/model")
        );

        chat.apply(Event::Phase(Phase::Ready));
        chat.draft.insert("αβγ");
        for modifier in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert!(compose(&mut chat, KeyEvent::new(KeyCode::Enter, modifier), &mut 0).is_none());
            for ch in "/model".chars() {
                assert!(chat.type_char(ch).is_none());
            }
        }
        chat.draft.up();
        chat.draft.up();
        assert_eq!(chat.draft.parts(), ("αβγ", "\n/model\n/model"));
        chat.draft.down();
        assert_eq!(chat.draft.parts(), ("αβγ\n/mo", "del\n/model"));
        assert!(
            matches!(compose(&mut chat, KeyEvent::from(KeyCode::Enter), &mut 0),
            Some(Action::Send(text)) if text == "αβγ\n/model\n/model")
        );
    }

    #[test]
    fn picker_scrolls_to_the_selected_model() {
        let mut chat = Chat::default();
        let (reply, _answer) = tokio::sync::oneshot::channel();
        chat.apply(Event::ChooseModel {
            request_id: 1.into(),
            message: "Select a model".into(),
            current: Some("14".into()),
            models: (0..15)
                .map(|n| elphie_core::ModelOption {
                    id: n.to_string(),
                    name: format!("Model {n}"),
                })
                .collect(),
            reply,
        });
        for height in [16, 4, 1] {
            let mut terminal = Terminal::new(TestBackend::new(40, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, &chat, "ollama", &mut 0))
                .unwrap();
            let cells = &terminal.backend().buffer().content;
            assert!(cells.chunks(40).any(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("› Model 14 (current)")
            }));
        }
    }

    #[test]
    fn caret_stays_on_screen_when_the_current_word_moves_to_the_next_row() {
        let mut chat = Chat::default();
        chat.draft
            .insert("a\nb\nc\n12345678901234567890123456789012345 elephant");
        for _ in 0..6 {
            chat.draft.left();
        }
        let mut terminal = Terminal::new(TestBackend::new(42, 16)).unwrap();
        terminal
            .draw(|frame| draw(frame, &chat, "ollama", &mut 0))
            .unwrap();
        assert!(terminal.backend().buffer().content.chunks(42).any(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("el▏ephant")
        }));
    }

    #[test]
    fn the_caret_stays_visible_in_a_wrapped_draft() {
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut chat = Chat::default();
        chat.draft.insert(&"a".repeat(38));
        for _ in 0..2 {
            terminal
                .draw(|frame| draw(frame, &chat, "ollama", &mut 0))
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "▏")
            );
            chat.draft.insert(&"b".repeat(400));
            for _ in 0..300 {
                chat.draft.left();
            }
        }
    }
}
