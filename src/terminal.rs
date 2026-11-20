use std::io::{self, IsTerminal};

use anyhow::{Result, bail};
use crossterm::event::{Event as Input, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use elphie_core::Role;
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Stylize},
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::view_model::{Action, Chat, Event, Phase};

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
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
    crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
    let mut inputs = EventStream::new();
    let mut chat = Chat::default();
    let mut above_bottom: u16 = 0;
    loop {
        terminal.draw(|frame| draw(frame, &chat, provider, model, &mut above_bottom))?;
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                chat.apply(event);
            }
            input = inputs.next() => {
                let Some(input) = input else { break };
                match input? {
                    Input::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Esc if chat.phase == Phase::Working => actions.send(Action::Cancel)?,
                        KeyCode::Enter => {
                            if let Some(action) = chat.submit() {
                                above_bottom = 0;
                                actions.send(action)?;
                            }
                        }
                        KeyCode::PageUp => above_bottom = above_bottom.saturating_add(8),
                        KeyCode::PageDown => above_bottom = above_bottom.saturating_sub(8),
                        KeyCode::Left => chat.draft.left(),
                        KeyCode::Right => chat.draft.right(),
                        KeyCode::Delete => chat.draft.delete(),
                        KeyCode::Backspace => chat.draft.backspace(),
                        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !c.is_control() => chat.draft.insert(c.encode_utf8(&mut [0; 4])),
                        _ => {}
                    }
                    Input::Paste(paste) => chat.draft.insert(&visible(&paste)),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, chat: &Chat, provider: &str, model: &str, above_bottom: &mut u16) {
    let area = frame.area();
    let width = area.width.max(1);
    let (before, after) = chat.draft.parts();
    let input = Paragraph::new(format!("{before}▏{after}"))
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" Message "));
    let text_width = width.saturating_sub(2).max(1);
    let input_height = input.line_count(text_width).min(6) as u16;
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
    let [header, history, compose, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(input_height.max(3)),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Line::from(vec![
            "Elphie  ".bold(),
            format!("{provider} / {}", visible(model)).dark_gray(),
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
    let offset = Paragraph::new(format!("{before}▏"))
        .wrap(Wrap { trim: false })
        .line_count(text_width)
        .saturating_sub(compose.height.saturating_sub(2) as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(input.scroll((offset, 0)), compose);
    let help = match chat.phase {
        Phase::Ready => "Enter send",
        Phase::Connecting => "Connecting…",
        Phase::Working => "Working…  Esc cancel",
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
    fn the_caret_stays_visible_in_a_wrapped_draft() {
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let mut chat = Chat::default();
        chat.draft.insert(&"a".repeat(38));
        for _ in 0..2 {
            terminal
                .draw(|frame| draw(frame, &chat, "ollama", "test", &mut 0))
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
