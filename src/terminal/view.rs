use std::env;

use elphie_core::Role;
use elphie_view_model::{Chat, ModelPicker, Phase, ToolStatus, TranscriptEntry};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};

pub(super) fn composer_background() -> Option<Color> {
    if env::var_os("NO_COLOR").is_some() {
        return None;
    }
    let extended_color = env::var("TERM").is_ok_and(|s| s.contains("256color"))
        || env::var("COLORTERM").is_ok_and(|s| matches!(s.as_str(), "truecolor" | "24bit"));
    if !extended_color {
        return None;
    }
    match env::var("COLORFGBG").ok()?.rsplit(';').next()? {
        "0" | "8" => Some(Color::Indexed(236)),
        "7" | "15" => Some(Color::Indexed(254)),
        _ => None,
    }
}

fn split_top(area: Rect, rows: u16) -> (Rect, Rect) {
    let height = rows.min(area.height);
    let top = Rect::new(area.x, area.y, area.width, height);
    let rest = Rect::new(area.x, area.y + height, area.width, area.height - height);
    (top, rest)
}

fn inset(area: Rect) -> Rect {
    let horizontal = u16::from(area.width >= 12) + u16::from(area.width >= 40);
    area.inner(Margin {
        horizontal,
        vertical: 0,
    })
}

fn inline(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
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

struct Turn<'a> {
    prefix: Span<'a>,
    body: Paragraph<'a>,
    height: usize,
}

impl<'a> Turn<'a> {
    fn new(prefix: Span<'a>, text: &str, width: u16) -> Self {
        let clean: String = text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        let body = Paragraph::new(clean).wrap(Wrap { trim: false });
        let height = body.line_count(width).max(1);
        Self {
            prefix,
            body,
            height,
        }
    }
}

fn transcript(frame: &mut Frame, area: Rect, chat: &Chat, above_bottom: &mut u16) {
    if area.is_empty() {
        return;
    }
    let area = inset(area);
    let gutter = if area.width >= 4 { 2 } else { 0 };
    let width = area.width - gutter;
    let mut turns: Vec<_> = chat
        .entries()
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Message { message, .. } => {
                let prefix = match message.role {
                    Role::User => "›".bold(),
                    Role::Assistant => "•".dim(),
                };
                Turn::new(prefix, &message.text, width)
            }
            TranscriptEntry::Tool(tool) => {
                let (prefix, status) = match tool.status {
                    ToolStatus::Pending => ("·".dim(), "pending"),
                    ToolStatus::InProgress => ("·".dim(), "running"),
                    ToolStatus::Paused => ("·".dim(), "paused"),
                    ToolStatus::Completed => ("✓".dim(), "done"),
                    ToolStatus::Failed => ("!".red(), "failed"),
                    ToolStatus::Cancelled => ("×".dim(), "cancelled"),
                };
                let title = inline(&tool.title);
                let title = if title.is_empty() { "Tool" } else { &title };
                Turn::new(prefix, &format!("{title} · {status}"), width)
            }
        })
        .collect();
    if let Some(error) = chat.error() {
        let mut row = Turn::new("!".red().bold(), error, width);
        row.body = row.body.red();
        turns.push(row);
    }
    let total = turns.iter().map(|t| t.height + 1).sum::<usize>();
    let last_top = total.saturating_sub(usize::from(area.height));
    *above_bottom = (*above_bottom).min(u16::try_from(last_top).unwrap_or(u16::MAX));
    let mut skip = last_top - usize::from(*above_bottom);
    let mut remaining = area;
    for turn in turns {
        if skip > turn.height {
            skip -= turn.height + 1;
            continue;
        }
        if remaining.height == 0 {
            break;
        }
        let (row, next) = split_top(
            remaining,
            u16::try_from(turn.height - skip).unwrap_or(u16::MAX),
        );
        if !row.is_empty() {
            if gutter > 0 && skip == 0 {
                frame.render_widget(turn.prefix, Rect::new(row.x, row.y, 1, 1));
            }
            let body = Rect::new(row.x + gutter, row.y, width, row.height);
            frame.render_widget(
                turn.body
                    .scroll((u16::try_from(skip).unwrap_or(u16::MAX), 0)),
                body,
            );
        }
        remaining = split_top(next, 1).1;
        skip = 0;
    }
}

fn session_header(frame: &mut Frame, area: Rect, chat: &Chat, provider: &str) {
    if area.is_empty() {
        return;
    }
    let area = inset(area);
    let mut details = format!("{} · {}", inline(chat.model()), inline(provider));
    if let Some(directory) = chat.read_directory() {
        details.push_str(&format!(" · read-only {}", inline(directory)));
    }
    if !chat.mcp_servers().is_empty() {
        details.push_str(&format!(
            " · MCP {}",
            inline(&chat.mcp_servers().join(", "))
        ));
    }
    if area.height >= 3 {
        let (_, content) = split_top(area, 1);
        let (title, info) = split_top(content, 1);
        frame.render_widget("Elphie".bold(), title);
        frame.render_widget(details.dim(), info);
    } else {
        frame.render_widget(
            Line::from(vec!["Elphie".bold(), "  ".into(), details.dim()]),
            area,
        );
    }
}

fn picker(frame: &mut Frame, area: Rect, model: &ModelPicker) {
    let mut area = inset(area);
    let filtered = model.filter.is_some();
    if area.height >= if filtered { 3 } else { 2 } {
        let (title, rest) = split_top(area, 1);
        frame.render_widget(
            Line::from(vec![
                "Elphie".bold(),
                " · ".dim(),
                inline(&model.message).bold(),
            ]),
            title,
        );
        area = rest;
    }
    if let Some(query) = &model.filter
        && area.height > 1
    {
        let (search, rest) = split_top(area, 1);
        let label_width = if search.width > 8 { 8 } else { 0 };
        let text = format!("{}▏", inline(query));
        let overflow = Span::raw(&text)
            .width()
            .saturating_sub(usize::from(search.width - label_width));
        let offset = u16::try_from(overflow).unwrap_or(u16::MAX);
        if label_width > 0 {
            frame.render_widget(
                "Filter: ".dim(),
                Rect::new(search.x, search.y, label_width, 1),
            );
        }
        frame.render_widget(
            Paragraph::new(text).scroll((0, offset)),
            Rect::new(
                search.x + label_width,
                search.y,
                search.width - label_width,
                1,
            ),
        );
        area = rest;
    }
    if model.options().next().is_none() {
        frame.render_widget("No matching models".dim(), area);
        return;
    }
    let items = model.options().map(|option| {
        let mut row = vec![Span::raw(inline(&option.name))];
        if model.is_current(option) {
            row.push(" (current)".dim());
        }
        if option.name != option.id && area.width >= 48 {
            row.push(format!("  {}", inline(&option.id)).dim());
        }
        Line::from(row)
    });
    let list = List::new(items)
        .highlight_symbol("› ")
        .highlight_style(Style::new().reversed().bold());
    let mut state = ListState::default().with_selected(model.selected());
    frame.render_stateful_widget(list, area, &mut state);
}

const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Send message"),
    ("Shift+Enter", "Insert newline"),
    ("↑ / ↓", "Recall previous prompts"),
    ("/model", "Choose a model"),
    ("PgUp / PgDn", "Scroll conversation"),
    ("Esc", "Cancel turn"),
    ("Ctrl+C", "Quit"),
];

fn shortcuts(frame: &mut Frame, area: Rect) {
    let (title, rows) = split_top(inset(area), 1);
    frame.render_widget("Keyboard shortcuts".bold(), title);
    let columns = if rows.width >= 72 { 2 } else { 1 };
    let count = SHORTCUTS.len().div_ceil(columns);
    let column_width = rows.width / columns as u16;
    for (i, &(key, command)) in SHORTCUTS.iter().enumerate() {
        let y = (i % count) as u16;
        if y < rows.height {
            let x = (i / count) as u16 * column_width;
            let line = Line::from(vec![format!("{key:<13}").bold(), command.into()]);
            frame.render_widget(line, Rect::new(rows.x + x, rows.y + y, column_width, 1));
        }
    }
}

fn footer(chat: &Chat, width: u16) -> &'static str {
    if let Some(model) = chat.picker() {
        return match (model.filter.is_some(), width) {
            (true, 45..) => "↑↓ move  ·  Enter select  ·  Esc clear filter",
            (true, _) => "Enter select  ·  Esc back",
            (false, 66..) => "↑↓ move  ·  / filter  ·  Enter select  ·  D decline  ·  Esc cancel",
            (false, 35..) => "Enter select  D decline  Esc cancel",
            (false, 26..) => "Enter pick  D no  Esc back",
            (false, _) => "↵ pick  Esc back",
        };
    }
    if chat.help_open() {
        return if width >= 38 {
            "Esc close  ·  ? insert question mark"
        } else {
            "Esc close"
        };
    }
    if chat.suggests_model() {
        return "Tab or Enter to open";
    }
    match chat.phase() {
        Phase::Connecting => "Connecting…",
        Phase::Working => "Working…  ·  Esc cancel",
        Phase::LoadingModels => "Loading models…  ·  Esc cancel",
        Phase::Cancelled => "Cancelled  ·  Enter send",
        Phase::Ready if chat.draft().parts() == ("", "") => "? for shortcuts",
        Phase::Ready if width < 34 => "Enter send",
        Phase::Ready => "Enter send  ·  Shift+Enter newline",
    }
}

pub(super) fn draw(
    frame: &mut Frame,
    chat: &Chat,
    provider: &str,
    above_bottom: &mut u16,
    background: Option<Color>,
) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let row_width = inset(area).width;
    let gutter = if row_width >= 4 { 2 } else { 0 };
    let (before, after) = chat.draft().parts();
    let (draft, caret) = draft_lines(before, after, usize::from(row_width - gutter));
    let roomy = area.height >= 9;
    let pad = u16::from(roomy);
    let suggest = u16::from(chat.suggests_model() && area.height >= 5);
    let panel_wanted = if let Some(model) = chat.picker() {
        1 + u16::from(model.filter.is_some()) + model.options().count().clamp(1, 9) as u16
    } else if chat.help_open() {
        1 + SHORTCUTS
            .len()
            .div_ceil(if row_width >= 72 { 2 } else { 1 }) as u16
    } else {
        draft.len().min(5) as u16 + pad * 2 + suggest
    };
    let stack_picker_keys =
        row_width < 66 && area.height >= 6 && chat.picker().is_some_and(|p| p.filter.is_none());
    let footer_height = u16::from(area.height >= 3) + u16::from(stack_picker_keys);
    let top_height = area.height.saturating_sub(footer_height + panel_wanted);
    let (top, controls) = split_top(area, top_height);
    let (panel, hints) = split_top(controls, controls.height - footer_height);
    let header_height =
        if roomy && chat.entries().is_empty() && chat.error().is_none() && top.height >= 5 {
            3
        } else {
            1
        };
    let (header, history) = split_top(top, header_height);
    session_header(frame, header, chat, provider);
    transcript(frame, history, chat, above_bottom);

    if let Some(model) = chat.picker() {
        picker(frame, panel, model);
    } else if chat.help_open() {
        shortcuts(frame, panel);
    } else {
        let (suggestion, composer) = split_top(panel, suggest);
        if suggest > 0 {
            frame.render_widget(
                Line::from(vec!["› /model".bold(), "  Select a model".into()]),
                inset(suggestion),
            );
        }
        if let Some(color) = background {
            frame.render_widget(Block::new().bg(color), composer);
        } else if pad > 0 {
            frame.render_widget(
                "─".repeat(usize::from(composer.width)).dim(),
                split_top(composer, 1).0,
            );
        }
        let body = inset(composer.inner(Margin {
            horizontal: 0,
            vertical: pad,
        }));
        let start = (caret + 1).saturating_sub(usize::from(body.height));
        for (i, content) in draft
            .into_iter()
            .enumerate()
            .skip(start)
            .take(usize::from(body.height))
        {
            let y = body.y + (i - start) as u16;
            if gutter > 0 && i == 0 {
                frame.render_widget("›".bold(), Rect::new(body.x, y, 1, 1));
            }
            let text = if before.is_empty() && after.is_empty() {
                Line::from(vec!["▏".into(), "Ask Elphie…".dim()])
            } else {
                content
            };
            frame.render_widget(text, Rect::new(body.x + gutter, y, body.width - gutter, 1));
        }
    }
    let hints = inset(hints);
    if stack_picker_keys {
        let (first, second) = split_top(hints, 1);
        let select = match hints.width {
            31.. => "↑↓ move  / filter  Enter select",
            22.. => "/ filter  Enter select",
            12.. => "Enter select",
            _ => "↵ select",
        };
        let dismiss = match hints.width {
            21.. => "D decline  Esc cancel",
            13.. => "D no  Esc back",
            _ => "D / Esc",
        };
        frame.render_widget(select.dim(), first);
        frame.render_widget(dismiss.dim(), second);
    } else {
        frame.render_widget(footer(chat, hints.width).dim(), hints);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elphie_core::ModelOption;
    use elphie_view_model::{Edit, Event, Intent};
    use ratatui::{Terminal, backend::TestBackend};

    fn ready() -> Chat {
        let mut chat = Chat::new("small");
        chat.apply(Event::Phase(Phase::Ready));
        chat
    }

    fn screen(chat: &Chat, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, chat, "ollama", &mut 0, None))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    #[test]
    fn composer_hints_follow_input_and_shortcuts_open_on_an_empty_draft() {
        let mut chat = ready();
        let empty = screen(&chat, 80, 16).join("\n");
        assert!(empty.contains("Elphie") && empty.contains("small · ollama"));
        assert!(empty.contains("› ▏Ask Elphie…") && empty.contains("? for shortcuts"));
        chat.handle(Intent::Type('?'));
        let help = screen(&chat, 80, 16).join("\n");
        assert!(help.contains("Keyboard shortcuts") && help.contains("Recall previous prompts"));
        assert!(help.contains("Esc close"));
        chat.handle(Intent::Dismiss);
        chat.handle(Intent::Paste("/mo".into()));
        let rows = screen(&chat, 45, 8);
        let hint = rows
            .iter()
            .position(|line| line.contains("› /model  Select a model"))
            .unwrap();
        assert!(rows[hint + 1].contains("› /mo▏"));
        assert!(rows.last().unwrap().contains("Tab or Enter to open"));
    }

    #[test]
    fn transcript_preserves_the_gutter_for_explicit_and_wrapped_lines() {
        let mut chat = ready();
        for (role, id, text) in [
            (Role::User, "1", "One\ntwo"),
            (
                Role::Assistant,
                "2",
                "three four five six seven eight nine ten",
            ),
        ] {
            chat.apply(Event::Message(role, id.into(), Edit::Replace(text.into())));
        }
        let rows = screen(&chat, 24, 15);
        let user = rows.iter().position(|s| s.contains("› One")).unwrap();
        assert!(rows[user + 1].starts_with("   two"));
        let agent = rows.iter().position(|s| s.contains("• three")).unwrap();
        assert!(rows[agent + 1].starts_with("   seven"));
    }

    #[test]
    fn selected_model_remains_visible_in_short_terminals_and_while_filtering() {
        let mut chat = ready();
        chat.apply(Event::ChooseModel {
            request_id: 1,
            message: "Select a model".into(),
            current: Some("14".into()),
            models: (0..15)
                .map(|n| ModelOption {
                    id: n.to_string(),
                    name: format!("Model {n}"),
                })
                .collect(),
        });
        for height in [16, 4, 1] {
            assert!(
                screen(&chat, 40, height)
                    .iter()
                    .any(|row| row.contains("› Model 14 (current)"))
            );
        }
        let compact = screen(&chat, 40, 10).join("\n");
        for key in ["Enter select", "D decline", "Esc cancel"] {
            assert!(compact.contains(key), "{compact}");
        }
        chat.handle(Intent::Type('/'));
        chat.handle(Intent::Type('x'));
        let filtered = screen(&chat, 44, 12).join("\n");
        assert!(filtered.contains("Filter:") && filtered.contains("x▏"));
        assert!(filtered.contains("No matching models"));
    }

    #[test]
    fn cursor_stays_visible_as_words_wrap_on_resize() {
        let mut chat = ready();
        chat.handle(Intent::Paste(
            "a\nb\nc\n12345678901234567890123456789012345 elephant".into(),
        ));
        for _ in 0..6 {
            chat.handle(Intent::Left);
        }
        assert!(
            screen(&chat, 42, 16)
                .iter()
                .any(|row| row.contains("el▏ephant"))
        );
        chat.handle(Intent::Paste("界".repeat(400)));
        for _ in 0..300 {
            chat.handle(Intent::Left);
        }
        for (width, height) in [(40, 16), (12, 4), (1, 1)] {
            assert!(
                screen(&chat, width, height)
                    .iter()
                    .any(|row| row.contains("▏"))
            );
        }
    }
}
