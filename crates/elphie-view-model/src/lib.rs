mod draft;
mod picker;

use std::collections::VecDeque;

pub use draft::Draft;
use elphie_core::{Message, ModelOption, Role};
pub use picker::ModelPicker;

const MODEL_COMMAND: &str = "/model";

pub type FormId = u64;

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub enum Phase {
    #[default]
    Connecting,
    Ready,
    Working,
    LoadingModels,
    Cancelled,
}

pub enum Intent {
    Type(char),
    Paste(String),
    Submit,
    Complete,
    Newline,
    Dismiss,
    Up,
    Down,
    Left,
    Right,
    Backspace,
    Delete,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    Send(String),
    Cancel,
    AnswerModel(FormId, ModelChoice),
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
        request_id: FormId,
        message: String,
        current: Option<String>,
        models: Vec<ModelOption>,
    },
    DismissModel(FormId),
    ModelChanged(String),
}

#[derive(Debug, Eq, PartialEq)]
pub enum ModelChoice {
    Select(String),
    Decline,
    Cancel,
}

#[derive(Default)]
pub struct Chat {
    draft: Draft,
    phase: Phase,
    entries: Vec<(String, Message)>,
    error: Option<String>,
    model: String,
    help_open: bool,
    pickers: VecDeque<ModelPicker>,
    prompts: Vec<String>,
    recalled: Option<usize>,
}

impl Chat {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    pub fn draft(&self) -> &Draft {
        &self.draft
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn entries(&self) -> &[(String, Message)] {
        &self.entries
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn help_open(&self) -> bool {
        self.help_open
    }

    pub fn picker(&self) -> Option<&ModelPicker> {
        self.pickers.front()
    }

    fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready | Phase::Cancelled)
    }

    pub fn suggests_model(&self) -> bool {
        self.ready()
            && self.picker().is_none()
            && !self.help_open
            && !self.draft.text.is_empty()
            && self.draft.cursor == self.draft.text.len()
            && MODEL_COMMAND.starts_with(&self.draft.text)
    }

    pub fn handle(&mut self, intent: Intent) -> Option<Action> {
        if let Some(picker) = self.pickers.front_mut() {
            let choice = picker.handle(intent)?;
            let action = Action::AnswerModel(picker.request_id, choice);
            self.pickers.pop_front();
            return Some(action);
        }
        if self.help_open {
            match intent {
                Intent::Dismiss => self.help_open = false,
                Intent::Type('?') => {
                    self.help_open = false;
                    self.insert("?");
                }
                _ => {}
            }
            return None;
        }

        match intent {
            Intent::Dismiss if matches!(self.phase, Phase::Working | Phase::LoadingModels) => {
                return Some(Action::Cancel);
            }
            Intent::Type('?') if self.draft.text.is_empty() => self.help_open = true,
            Intent::Type(c) if !c.is_control() => {
                self.insert(c.encode_utf8(&mut [0; 4]));
                if self.draft.text == MODEL_COMMAND {
                    return self.submit();
                }
            }
            Intent::Paste(text) => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let cleaned: String = normalized
                    .chars()
                    .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                    .collect();
                self.insert(&cleaned);
            }
            Intent::Complete | Intent::Submit if self.suggests_model() => {
                self.draft.insert(&MODEL_COMMAND[self.draft.text.len()..]);
                return self.submit();
            }
            Intent::Submit => return self.submit(),
            Intent::Newline => self.insert("\n"),
            Intent::Left => self.draft.left(),
            Intent::Right => self.draft.right(),
            Intent::Up if self.draft.parts().0.contains('\n') => self.draft.up(),
            Intent::Down if self.draft.parts().1.contains('\n') => self.draft.down(),
            Intent::Up if self.navigates_history() => {
                let index = self.recalled.unwrap_or(self.prompts.len()).checked_sub(1)?;
                self.recall(Some(index));
            }
            Intent::Down if self.navigates_history() => {
                let next = self.recalled? + 1;
                self.recall((next < self.prompts.len()).then_some(next));
            }
            Intent::Backspace if self.draft.cursor > 0 => {
                self.recalled = None;
                self.draft.backspace();
            }
            Intent::Delete if self.draft.cursor < self.draft.text.len() => {
                self.recalled = None;
                self.draft.delete();
            }
            _ => {}
        }
        None
    }

    fn navigates_history(&self) -> bool {
        self.draft.text.is_empty()
            || (self.recalled.is_some()
                && (self.draft.cursor == 0 || self.draft.cursor == self.draft.text.len()))
    }

    fn recall(&mut self, index: Option<usize>) {
        self.recalled = index;
        self.draft.text = index.map(|i| self.prompts[i].clone()).unwrap_or_default();
        self.draft.cursor = self.draft.text.len();
    }

    fn insert(&mut self, text: &str) {
        if !text.is_empty() {
            self.recalled = None;
            self.draft.insert(text);
        }
    }

    fn submit(&mut self) -> Option<Action> {
        if !self.ready() || self.draft.text.trim().is_empty() {
            return None;
        }
        let prompt = std::mem::take(&mut self.draft).text;
        self.recalled = None;
        self.phase = if prompt == MODEL_COMMAND {
            Phase::LoadingModels
        } else {
            if self.prompts.last() != Some(&prompt) {
                self.prompts.push(prompt.clone());
            }
            Phase::Working
        };
        self.error = None;
        Some(Action::Send(prompt))
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
            } => {
                self.help_open = false;
                if self.phase == Phase::LoadingModels {
                    self.phase = Phase::Working;
                }
                self.pickers
                    .push_back(ModelPicker::new(request_id, message, models, current));
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
mod tests;
