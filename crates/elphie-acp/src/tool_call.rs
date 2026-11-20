use std::{collections::HashSet, sync::Arc};

use agent_client_protocol::{
    Error, Result,
    v2::schema::{ErrorCode, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolKind},
};
use elphie_core::{ModelInput, ToolCall, ToolRequest};
use elphie_tools::Workspace;
use tokio::task::JoinSet;

pub(super) const CALLS_PER_PROMPT: usize = 8;

pub(super) struct Batch {
    request: ToolRequest,
    outputs: Vec<String>,
}

impl Batch {
    pub(super) fn new(request: ToolRequest, remaining: &mut usize) -> Result<Self> {
        let calls = &request.calls;
        if calls.len() > *remaining {
            return Err(Error::new(
                ErrorCode::InternalError,
                "Tool call limit reached for this prompt.",
            ));
        }
        let mut ids = HashSet::new();
        if calls.is_empty()
            || calls
                .iter()
                .any(|call| call.id.is_empty() || !ids.insert(&call.id))
        {
            return Err(Error::new(
                ErrorCode::InternalError,
                "Invalid provider tool call IDs.",
            ));
        }
        *remaining -= calls.len();
        Ok(Self {
            request,
            outputs: Vec::new(),
        })
    }

    pub(super) fn next(&self) -> Option<&ToolCall> {
        self.request.calls.get(self.outputs.len())
    }

    pub(super) fn push(&mut self, output: String) {
        self.outputs.push(output);
    }

    pub(super) fn into_history(self) -> ModelInput {
        ModelInput::Tools {
            request: self.request,
            outputs: self.outputs,
        }
    }
}

pub(super) fn started(id: ToolCallId, call: &ToolCall) -> ToolCallUpdate {
    let (kind, verb) = match call.name.as_str() {
        "read_file" => (ToolKind::Read, "Read file"),
        "list_directory" => (ToolKind::Search, "List directory"),
        _ => (ToolKind::Other, "Unknown tool"),
    };
    let title = match call.arguments.get("path").and_then(|v| v.as_str()) {
        Some(path) if kind != ToolKind::Other => format!("{verb} {}", display_text(path)),
        _ => verb.to_string(),
    };
    ToolCallUpdate::new(id)
        .name(display_text(&call.name))
        .title(title)
        .kind(kind)
        .status(ToolCallStatus::InProgress)
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::escape_debug)
        .take(160)
        .collect()
}

pub(super) fn finished(id: ToolCallId, status: ToolCallStatus, summary: &str) -> ToolCallUpdate {
    ToolCallUpdate::new(id)
        .status(status)
        .content(vec![summary.to_string().into()])
}

pub(super) async fn execute(
    workspace: Arc<Workspace>,
    call: ToolCall,
) -> std::result::Result<String, String> {
    // Dropping the set cancels queued reads. A read already in progress can only
    // finish its bounded operation on the granted directory handle.
    let mut tasks = JoinSet::new();
    tasks.spawn_blocking(move || workspace.execute(&call.name, &call.arguments));
    tasks
        .join_next()
        .await
        .expect("one read task")
        .map_err(|_| "Workspace tool stopped.".to_string())?
}
