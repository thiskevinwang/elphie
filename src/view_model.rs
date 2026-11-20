use std::collections::VecDeque;

use agent_client_protocol::v2::schema::RequestId;
use elphie_core::{Message, ModelOption, Role};
use tokio::sync::oneshot;

const MODEL_COMMAND: &str = "/model";

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum Phase {
    #[default]
    Connecting,
    Ready,
    Working,
    LoadingModels,
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
    ChooseModel {
        request_id: RequestId,
        message: String,
        current: Option<String>,
        models: Vec<ModelOption>,
        reply: oneshot::Sender<ModelChoice>,
    },
    DismissModel(RequestId),
    ModelChanged(String),
}

#[derive(Debug, Eq, PartialEq)]
pub enum ModelChoice {
    Select(String),
    Decline,
    Cancel,
}

pub struct ModelPicker {
    request_id: RequestId,
    pub message: String,
    pub models: Vec<ModelOption>,
    pub selected: usize,
    pub current: Option<usize>,
    reply: oneshot::Sender<ModelChoice>,
}

impl ModelPicker {
    pub fn previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1).min(self.models.len() - 1);
    }
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

    pub fn up(&mut self) {
        let Some((earlier, column)) = self.text[..self.cursor].rsplit_once('\n') else {
            return;
        };
        let line = earlier.rsplit('\n').next().unwrap_or_default();
        self.cursor = earlier.len() - line.len() + byte_at_column(line, column.chars().count());
    }

    pub fn down(&mut self) {
        let Some((rest, later)) = self.text[self.cursor..].split_once('\n') else {
            return;
        };
        let column = self.text[..self.cursor]
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count();
        let line = later.split('\n').next().unwrap_or_default();
        self.cursor += rest.len() + 1 + byte_at_column(line, column);
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

fn byte_at_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(i, _)| i)
}

#[derive(Default)]
pub struct Chat {
    pub draft: Draft,
    pub phase: Phase,
    pub entries: Vec<(String, Message)>,
    pub error: Option<String>,
    pub model: String,
    pub pickers: VecDeque<ModelPicker>,
}

impl Chat {
    fn ready(&self) -> bool {
        self.pickers.is_empty() && matches!(self.phase, Phase::Ready | Phase::Cancelled)
    }

    pub fn suggests_model(&self) -> bool {
        self.ready()
            && !self.draft.text.is_empty()
            && self.draft.cursor == self.draft.text.len()
            && MODEL_COMMAND.starts_with(&self.draft.text)
    }

    pub fn complete_command(&mut self) -> Option<Action> {
        if !self.suggests_model() {
            return None;
        }
        self.draft.insert(&MODEL_COMMAND[self.draft.text.len()..]);
        self.submit()
    }

    pub fn type_char(&mut self, ch: char) -> Option<Action> {
        self.draft.insert(ch.encode_utf8(&mut [0; 4]));
        if self.draft.text == MODEL_COMMAND {
            return self.submit();
        }
        None
    }

    pub fn submit(&mut self) -> Option<Action> {
        if !self.ready() || self.draft.text.trim().is_empty() {
            return None;
        }
        self.phase = if self.draft.text == MODEL_COMMAND {
            Phase::LoadingModels
        } else {
            Phase::Working
        };
        self.error = None;
        Some(Action::Send(std::mem::take(&mut self.draft).text))
    }

    pub fn choose_model(&mut self) {
        if let Some(picker) = self.pickers.pop_front() {
            let id = picker.models[picker.selected].id.clone();
            let _ = picker.reply.send(ModelChoice::Select(id));
        }
    }

    pub fn dismiss_model(&mut self, choice: ModelChoice) {
        if let Some(picker) = self.pickers.pop_front() {
            let _ = picker.reply.send(choice);
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Phase(Phase::Working) if self.phase == Phase::LoadingModels => {}
            Event::Phase(phase) => self.phase = phase,
            Event::Failed(error) => {
                self.phase = Phase::Ready;
                self.error = Some(error);
            }
            Event::ChooseModel {
                request_id,
                message,
                current,
                models,
                reply,
            } => {
                if models.is_empty() {
                    let _ = reply.send(ModelChoice::Cancel);
                    self.error = Some("No models available".into());
                    return;
                }
                let current = models
                    .iter()
                    .position(|option| Some(&option.id) == current.as_ref());
                if self.phase == Phase::LoadingModels {
                    self.phase = Phase::Working;
                }
                self.pickers.push_back(ModelPicker {
                    request_id,
                    message,
                    models,
                    selected: current.unwrap_or(0),
                    current,
                    reply,
                });
            }
            Event::DismissModel(request_id) => {
                self.pickers
                    .retain(|picker| picker.request_id != request_id);
            }
            Event::ModelChanged(model) => self.model = model,
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
    fn picking_a_model_needs_confirmation_and_keeps_the_draft() {
        let mut chat = Chat {
            model: "small".into(),
            ..Chat::default()
        };
        chat.draft.insert("follow-up");
        let models: Vec<_> = ["large", "small"]
            .into_iter()
            .map(|id| ModelOption {
                id: id.into(),
                name: id.into(),
            })
            .collect();
        let (reply, mut answer) = oneshot::channel();
        chat.apply(Event::ChooseModel {
            request_id: 1.into(),
            message: "Select a model".into(),
            current: Some("small".into()),
            models: models.clone(),
            reply,
        });
        let picker = chat.pickers.front_mut().unwrap();
        assert_eq!(picker.selected, 1);
        picker.previous();
        chat.choose_model();
        assert_eq!(
            answer.try_recv().unwrap(),
            ModelChoice::Select("large".into())
        );
        assert_eq!(chat.model, "small");
        chat.apply(Event::ModelChanged("large".into()));
        let (reply, mut answer) = oneshot::channel();
        chat.apply(Event::ChooseModel {
            request_id: 2.into(),
            message: "Select a model".into(),
            current: None,
            models,
            reply,
        });
        chat.apply(Event::Phase(Phase::Cancelled));
        assert_eq!(chat.pickers.len(), 1);
        chat.dismiss_model(ModelChoice::Cancel);
        assert_eq!(answer.try_recv().unwrap(), ModelChoice::Cancel);
        assert!(chat.pickers.is_empty());
        assert_eq!(chat.model, "large");
        assert_eq!(chat.draft.parts(), ("follow-up", ""));
    }

    #[test]
    fn cancelling_one_request_keeps_the_other_picker() {
        let mut chat = Chat::default();
        let mut answers = Vec::new();
        for id in 1..=3 {
            let (reply, answer) = oneshot::channel();
            answers.push(answer);
            chat.apply(Event::ChooseModel {
                request_id: id.into(),
                message: "Select a model".into(),
                current: None,
                models: vec![ModelOption {
                    id: id.to_string(),
                    name: id.to_string(),
                }],
                reply,
            });
        }
        chat.apply(Event::DismissModel(2.into()));
        chat.apply(Event::Phase(Phase::Ready));
        chat.dismiss_model(ModelChoice::Decline);
        chat.apply(Event::DismissModel(1.into()));
        chat.choose_model();
        assert_eq!(answers[0].try_recv().unwrap(), ModelChoice::Decline);
        assert_eq!(
            answers[1].try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        assert_eq!(
            answers[2].try_recv().unwrap(),
            ModelChoice::Select("3".into())
        );
    }

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
