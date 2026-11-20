use elphie_core::{Message, Role};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum Phase {
    #[default]
    Connecting,
    Ready,
    Working,
    Cancelled,
}

pub enum Action {
    Send(String),
    Cancel,
}

pub enum Edit {
    Append(String),
    Replace(String),
}

pub enum Event {
    Phase(Phase),
    Message(Role, String, Edit),
    Failed(String),
}

#[derive(Default)]
pub struct Draft {
    text: String,
    cursor: usize,
}

impl Draft {
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn parts(&self) -> (&str, &str) {
        self.text.split_at(self.cursor)
    }

    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    pub fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
    }

    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }
}

#[derive(Default)]
pub struct Chat {
    pub draft: Draft,
    pub phase: Phase,
    pub entries: Vec<(String, Message)>,
    pub error: Option<String>,
}

impl Chat {
    pub fn submit(&mut self) -> Option<Action> {
        if !matches!(self.phase, Phase::Ready | Phase::Cancelled)
            || self.draft.text.trim().is_empty()
        {
            return None;
        }
        self.phase = Phase::Working;
        self.error = None;
        Some(Action::Send(std::mem::take(&mut self.draft).text))
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Phase(phase) => self.phase = phase,
            Event::Failed(error) => {
                self.phase = Phase::Ready;
                self.error = Some(error);
            }
            Event::Message(role, id, edit) => {
                let index = self
                    .entries
                    .iter()
                    .position(|(key, msg)| *key == id && msg.role == role)
                    .unwrap_or_else(|| {
                        self.entries.push((
                            id,
                            Message {
                                role,
                                text: String::new(),
                            },
                        ));
                        self.entries.len() - 1
                    });
                let message = &mut self.entries[index].1;
                match edit {
                    Edit::Append(text) => message.text.push_str(&text),
                    Edit::Replace(text) => message.text = text,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_waits_for_idle_and_patches_messages_by_role_and_id() {
        let mut chat = Chat::default();
        chat.draft.insert("hello");
        assert!(chat.submit().is_none());
        chat.apply(Event::Phase(Phase::Ready));
        assert!(matches!(chat.submit(), Some(Action::Send(s)) if s == "hello"));
        chat.draft.insert("next");
        assert!(chat.submit().is_none());
        chat.apply(Event::Message(
            Role::User,
            "1".into(),
            Edit::Replace("hello".into()),
        ));
        chat.apply(Event::Message(
            Role::Assistant,
            "1".into(),
            Edit::Append("hel".into()),
        ));
        chat.apply(Event::Message(
            Role::Assistant,
            "1".into(),
            Edit::Replace("hello!".into()),
        ));
        chat.apply(Event::Phase(Phase::Ready));
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.entries[1].1.text, "hello!");
        assert!(matches!(chat.submit(), Some(Action::Send(s)) if s == "next"));
    }
}
