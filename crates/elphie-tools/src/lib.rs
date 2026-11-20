use elphie_core::ToolDefinition;
use serde_json::json;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod workspace;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use workspace::Workspace;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported {
    pub struct Workspace(());

    impl Workspace {
        pub fn open(_: &std::path::Path) -> anyhow::Result<Self> {
            anyhow::bail!("Workspace tools are unavailable on this operating system.")
        }

        pub fn execute(&self, _: &str, _: &serde_json::Value) -> Result<String, String> {
            Err("Workspace tools are unavailable on this operating system.".into())
        }
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use unsupported::Workspace;

const LIST: &str = "list_directory";
const READ: &str = "read_file";
const MAX_PATH_BYTES: usize = 1024;

pub fn definitions() -> Vec<ToolDefinition> {
    [
        (LIST, "List visible files and directories in a workspace directory. Use . for the workspace root."),
        (READ, "Read a UTF-8 text file of up to 32 KiB from the workspace."),
    ]
    .into_iter()
    .map(|(name, description)| ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "A relative workspace path.", "maxLength": MAX_PATH_BYTES}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    })
    .collect()
}
