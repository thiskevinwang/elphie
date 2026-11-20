mod client;
mod terminal;

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use elphie_core::Provider;
use elphie_view_model::Chat;
use tokio::sync::mpsc;

#[derive(Debug, Copy, Clone, ValueEnum)]
enum Backend {
    Chatgpt,
    Ollama,
}

#[derive(Parser)]
#[command(version, about = "Chat with Elphie in the terminal")]
struct Options {
    #[arg(
        long,
        global = true,
        value_enum,
        env = "ELPHIE_PROVIDER",
        default_value = "chatgpt"
    )]
    provider: Backend,
    #[arg(
        long,
        global = true,
        env = "ELPHIE_MODEL",
        help = "Model name. Required for Ollama"
    )]
    model: Option<String>,
    #[arg(
        long,
        global = true,
        env = "ELPHIE_OLLAMA_HOST",
        default_value = "http://127.0.0.1:11434"
    )]
    ollama_host: String,
    #[arg(
        long,
        global = true,
        value_name = "DIRECTORY",
        help = "Let the model list and read files in this directory. Results go to the selected provider"
    )]
    tools: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help = "Enable MCP servers and tools listed in this JSON file"
    )]
    mcp_config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Serve ACP v2 over standard input and output")]
    Serve,
    #[command(about = "Continue with ChatGPT in your browser")]
    Login,
    #[command(about = "Sign out of the active ChatGPT registration")]
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    match options.command {
        Some(Command::Login) => return elphie_signin::login().await,
        Some(Command::Logout) => return elphie_signin::logout().await,
        _ => {}
    }
    let (name, model) = match options.provider {
        Backend::Chatgpt => (
            "chatgpt",
            options.model.unwrap_or_else(|| "gpt-6.1-sol".into()),
        ),
        Backend::Ollama => (
            "ollama",
            options
                .model
                .context("Choose an installed Ollama model with --model")?,
        ),
    };
    if model.trim().is_empty() {
        anyhow::bail!("Model name is empty");
    }
    if matches!(options.command, Some(Command::Serve)) {
        let provider: Arc<dyn Provider> = match options.provider {
            Backend::Chatgpt => {
                Arc::new(elphie_providers::ChatGpt::new(model).map_err(|e| anyhow!(e))?)
            }
            Backend::Ollama => Arc::new(
                elphie_providers::Ollama::new(&options.ollama_host, model)
                    .map_err(|e| anyhow!(e))?,
            ),
        };
        let workspace = options
            .tools
            .as_deref()
            .map(elphie_tools::Workspace::open)
            .transpose()
            .context("Could not open the workspace for tools")?
            .map(Arc::new);
        let mcp = options
            .mcp_config
            .as_deref()
            .map(elphie_mcp::McpConfig::load)
            .transpose()?
            .unwrap_or_default();
        return elphie_acp::serve_with_mcp(provider, workspace, Arc::new(mcp)).await;
    }
    let mut config = AcpAgentConfig::new(std::env::current_exe()?).args([
        "serve",
        "--provider",
        name,
        "--model",
        &model,
        "--ollama-host",
        &options.ollama_host,
    ]);
    let mut chat = Chat::new(&model);
    if let Some(path) = options.tools {
        let directory = path
            .to_str()
            .context("The terminal requires a UTF-8 workspace path")?;
        config = config.arg(format!("--tools={directory}"));
        chat = chat.with_read_directory(directory);
    }
    if let Some(path) = options.mcp_config {
        let path = path
            .to_str()
            .context("The terminal requires a UTF-8 MCP config path")?;
        config = config.arg(format!("--mcp-config={path}"));
    }
    let agent = AcpAgent::new(config);
    let (send_action, actions) = mpsc::unbounded_channel();
    let (events, receive_event) = mpsc::unbounded_channel();
    let connection = client::run(agent, actions, events);
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => result.context("ACP connection closed"),
        result = terminal::run(name, chat, send_action, receive_event) => {
            result?;
            connection.await
        }
    }
}
