use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use url::{Host, Url};

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_SERVERS: usize = 16;
const MAX_TOOLS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpConfig {
    servers: Vec<ServerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    servers: Vec<ServerConfig>,
}

impl McpConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).context("Cannot inspect the MCP configuration file")?;
        ensure!(
            metadata.is_file(),
            "MCP configuration must be a regular file"
        );
        let file = File::open(path).context("Cannot open the MCP configuration file")?;
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("Cannot read the MCP configuration file")?;
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_CONFIG_BYTES,
            "MCP configuration exceeds 64 KiB"
        );
        let config: ConfigFile = serde_json::from_slice(bytes).map_err(|error| {
            anyhow!(
                "Invalid MCP configuration at line {}, column {}",
                error.line(),
                error.column()
            )
        })?;
        validate_servers(&config.servers)?;
        Ok(Self {
            servers: config.servers,
        })
    }

    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub tools: Vec<String>,
    #[serde(flatten)]
    pub transport: TransportConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TransportConfig {
    Http {
        url: Url,
        #[serde(rename = "bearerTokenEnv")]
        bearer_token_env: Option<String>,
    },
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
    },
}

pub(crate) fn validate_servers(servers: &[ServerConfig]) -> Result<()> {
    ensure!(
        servers.len() <= MAX_SERVERS,
        "MCP configuration allows up to {MAX_SERVERS} servers"
    );
    let mut names = HashSet::new();
    for server in servers {
        ensure!(
            (1..=32).contains(&server.name.len())
                && server
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "MCP server names require 1–32 letters, digits, hyphens or underscores"
        );
        ensure!(names.insert(&server.name), "Duplicate MCP server name");
        ensure!(
            (1..=MAX_TOOLS).contains(&server.tools.len()),
            "Specify 1–{MAX_TOOLS} allowed tools for each MCP server"
        );
        let mut tools = HashSet::new();
        for tool in &server.tools {
            ensure!(
                (1..=128).contains(&tool.len())
                    && tool
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')),
                "Invalid MCP tool name"
            );
            ensure!(
                tools.insert(tool),
                "Duplicate MCP tool in server configuration"
            );
        }
        match &server.transport {
            TransportConfig::Http {
                url,
                bearer_token_env,
            } => {
                let loopback = match url.host() {
                    Some(Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(Host::Ipv6(ip)) => ip.is_loopback(),
                    Some(Host::Domain(name)) => name == "localhost",
                    None => false,
                };
                ensure!(
                    url.has_host()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none(),
                    "MCP URLs require a host and cannot contain user information or a fragment"
                );
                ensure!(
                    url.scheme() == "https" || (url.scheme() == "http" && loopback),
                    "MCP servers require HTTPS or loopback HTTP"
                );
                ensure!(
                    bearer_token_env.as_deref().is_none_or(valid_env_name),
                    "Invalid MCP bearer token environment variable name"
                );
            }
            TransportConfig::Stdio { command, args, env } => {
                ensure!(
                    command.is_absolute() && !command.as_os_str().as_encoded_bytes().contains(&0),
                    "MCP stdio commands require an absolute executable path"
                );
                ensure!(
                    args.len() <= 64 && args.iter().all(|arg| !arg.contains('\0')),
                    "MCP stdio allows up to 64 arguments without null characters"
                );
                ensure!(
                    env.len() <= 64 && env.iter().all(|name| valid_env_name(name)),
                    "MCP stdio allows up to 64 environment variable names"
                );
                ensure!(
                    env.iter().collect::<HashSet<_>>().len() == env.len(),
                    "Duplicate MCP stdio environment variable name"
                );
            }
        }
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn http() -> Value {
        json!({"type":"http","name":"docs","tools":["docs.lookup"],"url":"https://example.com/mcp?mode=read"})
    }

    fn parse(servers: Value) -> Result<McpConfig> {
        McpConfig::parse(&serde_json::to_vec(&json!({"servers":servers})).unwrap())
    }

    #[test]
    fn reads_explicit_grants_from_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("mcp.json");
        let command = directory.path().join("server");
        let mut remote = http();
        remote["bearerTokenEnv"] = json!("DOCS_TOKEN");
        fs::write(
            &config_path,
            json!({"servers":[remote, {
                "type":"stdio","name":"calendar","command":command,"args":["--serve"],
                "tools":["lookup"],"env":["CALENDAR_ZONE"]
            }]})
            .to_string(),
        )
        .unwrap();
        let config = McpConfig::load(&config_path).unwrap();
        let [remote, local] = config.servers() else {
            panic!("expected two servers")
        };
        assert!(
            matches!(&remote.transport, TransportConfig::Http { url, bearer_token_env: Some(key) }
            if url.query() == Some("mode=read") && key == "DOCS_TOKEN")
        );
        assert!(
            matches!(&local.transport, TransportConfig::Stdio { command: path, args, env }
            if path == &command && args == &["--serve"] && env == &["CALENDAR_ZONE"])
        );
        assert!(McpConfig::default().servers().is_empty());
        assert!(McpConfig::load(directory.path()).is_err());
    }

    #[test]
    fn rejects_invalid_grants_without_printing_the_supplied_values() {
        let base = http();
        for (field, value) in [
            ("type", json!("websocket")),
            ("url", json!("https://dummysecret@example.com")),
            ("url", json!("http://example.com/mcp")),
            ("url", json!("https://example.com/mcp#dummysecret")),
            ("tools", json!([])),
            ("tools", json!(["look", "look"])),
            ("tools", json!(["*"])),
            ("bearerTokenEnv", json!("invalid=dummysecret")),
            ("headers", json!({"Authorization":"dummysecret"})),
            ("name", json!("a/b")),
            ("args", json!([])),
        ] {
            let mut invalid = base.clone();
            invalid[field] = value;
            let error = parse(json!([invalid])).unwrap_err().to_string();
            assert!(!error.contains("dummysecret"));
        }
        assert!(parse(json!([base, base])).is_err());
        assert!(McpConfig::parse(&vec![b' '; MAX_CONFIG_BYTES + 1]).is_err());
        assert!(
            parse(json!([{"name":"local","type":"stdio","tools":["lookup"],"command":"server"}]))
                .is_err()
        );
        for endpoint in [
            "http://127.0.0.1:8080/mcp",
            "http://[::1]/mcp",
            "http://localhost/mcp",
        ] {
            let mut local = http();
            local["url"] = json!(endpoint);
            assert!(parse(json!([local])).is_ok());
        }
    }
}
