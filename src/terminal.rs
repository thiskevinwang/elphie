mod view;

use std::io::{self, IsTerminal};

use anyhow::{Result, bail};
use crossterm::event::{
    Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use elphie_view_model::{Action, Chat, Event, Intent};
use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        ratatui::restore();
    }
}

enum TerminalCommand {
    Edit(Intent),
    PageUp,
    PageDown,
    Quit,
}

fn key_command(key: KeyEvent) -> Option<TerminalCommand> {
    use TerminalCommand::{Edit, PageDown, PageUp, Quit};
    if key.kind == KeyEventKind::Release {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Quit,
        KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => Edit(Intent::Newline),
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            Edit(Intent::Newline)
        }
        KeyCode::Enter => Edit(Intent::Submit),
        KeyCode::Tab => Edit(Intent::Complete),
        KeyCode::Esc => Edit(Intent::Dismiss),
        KeyCode::Up => Edit(Intent::Up),
        KeyCode::Down => Edit(Intent::Down),
        KeyCode::Left => Edit(Intent::Left),
        KeyCode::Right => Edit(Intent::Right),
        KeyCode::Delete => Edit(Intent::Delete),
        KeyCode::Backspace => Edit(Intent::Backspace),
        KeyCode::PageUp => PageUp,
        KeyCode::PageDown => PageDown,
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && !c.is_control() =>
        {
            Edit(Intent::Type(c))
        }
        _ => return None,
    })
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
    let background = view::composer_background();
    let mut inputs = EventStream::new();
    let mut chat = Chat::new(model);
    let mut above_bottom: u16 = 0;
    loop {
        terminal.draw(|frame| view::draw(frame, &chat, provider, &mut above_bottom, background))?;
        let input = tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                chat.apply(event);
                continue;
            }
            input = inputs.next() => {
                let Some(input) = input else { break };
                input?
            }
        };
        let command = match input {
            Input::Key(key) => key_command(key),
            Input::Paste(text) => Some(TerminalCommand::Edit(Intent::Paste(text))),
            _ => None,
        };
        match command {
            Some(TerminalCommand::Quit) => break,
            Some(TerminalCommand::PageUp) if chat.picker().is_none() && !chat.help_open() => {
                above_bottom = above_bottom.saturating_add(8);
            }
            Some(TerminalCommand::PageDown) if chat.picker().is_none() && !chat.help_open() => {
                above_bottom = above_bottom.saturating_sub(8);
            }
            Some(TerminalCommand::Edit(intent)) => {
                if let Some(action) = chat.handle(intent) {
                    if matches!(action, Action::Send(_)) {
                        above_bottom = 0;
                    }
                    actions.send(action)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
