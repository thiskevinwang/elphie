use elphie_core::ModelOption;

use crate::{FormId, Intent, ModelChoice};

pub struct ModelPicker {
    pub(super) request_id: FormId,
    pub message: String,
    pub filter: Option<String>,
    models: Vec<ModelOption>,
    visible: Vec<usize>,
    current: Option<usize>,
    selected: usize,
}

impl ModelPicker {
    pub(super) fn new(
        request_id: FormId,
        message: String,
        models: Vec<ModelOption>,
        current_id: Option<String>,
    ) -> Self {
        let current = models
            .iter()
            .position(|model| Some(&model.id) == current_id.as_ref());
        Self {
            request_id,
            message,
            visible: (0..models.len()).collect(),
            models,
            current,
            selected: current.unwrap_or(0),
            filter: None,
        }
    }

    pub fn options(&self) -> impl ExactSizeIterator<Item = &ModelOption> {
        self.visible.iter().map(|&index| &self.models[index])
    }

    pub fn is_current(&self, model: &ModelOption) -> bool {
        self.current
            .is_some_and(|index| self.models[index].id == model.id)
    }

    pub fn selected(&self) -> Option<usize> {
        self.visible.get(self.selected).map(|_| self.selected)
    }

    pub(super) fn handle(&mut self, intent: Intent) -> Option<ModelChoice> {
        match intent {
            Intent::Up => self.selected = self.selected.saturating_sub(1),
            Intent::Down => {
                self.selected = (self.selected + 1).min(self.visible.len().saturating_sub(1))
            }
            Intent::Submit => {
                let index = *self.visible.get(self.selected)?;
                return Some(ModelChoice::Select(self.models[index].id.clone()));
            }
            Intent::Type('d' | 'D') if self.filter.is_none() => return Some(ModelChoice::Decline),
            Intent::Dismiss if self.filter.is_none() => return Some(ModelChoice::Cancel),
            Intent::Dismiss => {
                self.filter = None;
                self.refilter();
            }
            Intent::Type('/') if self.filter.is_none() => self.filter = Some(String::new()),
            Intent::Type(c) if !c.is_control() => {
                self.filter.get_or_insert_default().push(c);
                self.refilter();
            }
            Intent::Paste(text) => {
                let query = self.filter.get_or_insert_default();
                query.extend(text.chars().filter(|c| !c.is_control()));
                self.refilter();
            }
            Intent::Backspace => {
                self.filter.as_mut()?.pop();
                self.refilter();
            }
            _ => {}
        }
        None
    }

    fn refilter(&mut self) {
        let previously_selected = self.visible.get(self.selected).copied();
        let query = self.filter.as_deref().unwrap_or_default().to_lowercase();
        self.visible = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                query.is_empty()
                    || model.name.to_lowercase().contains(&query)
                    || model.id.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = previously_selected
            .and_then(|old| self.visible.iter().position(|&index| index == old))
            .or_else(|| {
                self.visible
                    .iter()
                    .position(|index| Some(*index) == self.current)
            })
            .unwrap_or(0);
    }
}
