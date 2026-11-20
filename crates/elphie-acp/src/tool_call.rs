use std::{collections::HashSet, sync::Arc};

use crate::mcp::SessionMcp;
use agent_client_protocol::{
    Error, Result,
    v2::schema::{ErrorCode, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolKind},
};
use elphie_core::{ModelInput, ToolCall, ToolDefinition, ToolRequest};
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

#[derive(Clone)]
pub(super) struct SessionTools {
    pub workspace: Option<Arc<Workspace>>,
    pub mcp: Option<Arc<SessionMcp>>,
}

impl SessionTools {
    pub fn enabled(&self) -> bool {
        self.workspace.is_some() || self.mcp.is_some()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = self
            .workspace
            .as_ref()
            .map(|_| elphie_tools::definitions())
            .unwrap_or_default();
        if let Some(mcp) = &self.mcp {
            tools.extend_from_slice(mcp.client.definitions());
        }
        tools
    }

    pub fn started(&self, id: ToolCallId, call: &ToolCall) -> ToolCallUpdate {
        let (kind, verb) = match call.name.as_str() {
            "read_file" if self.workspace.is_some() => (ToolKind::Read, "Read file"),
            "list_directory" if self.workspace.is_some() => (ToolKind::Search, "List directory"),
            other => (
                ToolKind::Other,
                self.mcp
                    .as_ref()
                    .and_then(|m| m.client.title(other))
                    .unwrap_or("Unknown tool"),
            ),
        };
        let title = match call.arguments.get("path").and_then(|v| v.as_str()) {
            Some(path) if kind != ToolKind::Other => format!("{verb} {}", display_text(path)),
            _ => display_text(verb),
        };
        ToolCallUpdate::new(id)
            .name(display_text(&call.name))
            .title(title)
            .kind(kind)
            .status(ToolCallStatus::InProgress)
    }

    pub async fn execute(self, call: ToolCall) -> std::result::Result<String, String> {
        if let Some(mcp) = &self.mcp
            && mcp.client.title(&call.name).is_some()
        {
            return mcp.client.execute(&call.name, &call.arguments).await;
        }
        if let Some(workspace) = self.workspace {
            return execute_workspace(workspace, call).await;
        }
        Err("Unknown tool.".into())
    }
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

async fn execute_workspace(
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
