use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::{
    ClientInfo, ClientRequest, ListToolsRequest, PaginatedRequestParams, ServerResult,
};
use rmcp::service::{PeerRequestOptions, ServerSink};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::McpServerConfig;

fn mcp_home() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".moye"))
}

fn local_bin_path(command: &str) -> Option<PathBuf> {
    mcp_home().map(|d| d.join("node_modules").join(".bin").join(command))
}

async fn ensure_installed(command: &str, package: &str) -> Result<PathBuf> {
    let bin = local_bin_path(command)
        .with_context(|| "HOME not set; cannot resolve ~/.moye path")?;

    if bin.exists() {
        return Ok(bin);
    }

    let dir = mcp_home()
        .with_context(|| "HOME not set; cannot resolve ~/.moye path")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create directory {:?}", dir))?;

    eprintln!("[mcp] First-time setup: installing '{package}' to {dir:?} ...");
    let output = tokio::process::Command::new("npm")
        .arg("install")
        .arg("--prefix")
        .arg(&dir)
        .arg(package)
        .output()
        .await
        .with_context(|| "run npm install")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("npm install '{}' failed: {}", package, stderr.trim());
    }

    if !bin.exists() {
        anyhow::bail!(
            "npm install succeeded but binary '{}' not found at {:?}",
            command,
            bin
        );
    }

    eprintln!("[mcp] '{package}' installed to {bin:?}");
    Ok(bin)
}

async fn ensure_indexed(resolved: &PathBuf, cfg: &McpServerConfig) -> Result<()> {
    if cfg.init.is_empty() {
        return Ok(());
    }

    let check_dir = match &cfg.init_if_missing {
        Some(d) => PathBuf::from(d),
        None => return Ok(()),
    };

    if check_dir.exists() {
        return Ok(());
    }

    eprintln!(
        "[mcp] First-time setup: running '{:?} {}' (indexing project)...",
        resolved,
        cfg.init.join(" ")
    );

    let output = tokio::process::Command::new(resolved)
        .args(&cfg.init)
        .output()
        .await
        .with_context(|| format!("run init command: {:?} {:?}", resolved, cfg.init))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("init failed: {}", stderr.trim());
    }

    eprintln!("[mcp] Indexing complete.");
    Ok(())
}

pub struct McpConnection {
    name: String,
    sink: ServerSink,
    tools: Vec<rmcp::model::Tool>,
}

impl McpConnection {
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.to_string()).collect()
    }
}

pub struct McpServerDisplay {
    pub name: String,
    pub connected: bool,
    pub tool_names: Vec<String>,
    pub error: Option<String>,
}

pub struct McpManager {
    connections: Vec<McpConnection>,
    failed: Vec<(String, String)>,
}

impl McpManager {
    pub async fn connect_all(configs: &HashMap<String, McpServerConfig>) -> Self {
        let mut connections = Vec::new();
        let mut failed = Vec::new();
        for (name, cfg) in configs {
            match connect_one(name, cfg).await {
                Ok(conn) => {
                    info!(
                        "[mcp] '{}' connected ({}): {} tools",
                        conn.name,
                        cfg.transport_type(),
                        conn.tools.len()
                    );
                    for tn in conn.tool_names() {
                        info!("[mcp]   tool: {tn}");
                    }
                    connections.push(conn);
                }
                Err(e) => {
                    let msg = e.to_string();
                    error!("[mcp] '{name}' failed: {msg}");
                    failed.push((name.clone(), msg));
                }
            }
        }
        Self { connections, failed }
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn all_tools_and_sinks(&self) -> Vec<(Vec<rmcp::model::Tool>, ServerSink)> {
        self.connections
            .iter()
            .map(|c| (c.tools.clone(), c.sink.clone()))
            .collect()
    }

    pub fn server_displays(&self) -> Vec<McpServerDisplay> {
        let mut displays: Vec<McpServerDisplay> = self
            .connections
            .iter()
            .map(|c| McpServerDisplay {
                name: c.name.clone(),
                connected: true,
                tool_names: c.tool_names(),
                error: None,
            })
            .collect();
        for (name, error) in &self.failed {
            displays.push(McpServerDisplay {
                name: name.clone(),
                connected: false,
                tool_names: vec![],
                error: Some(error.clone()),
            });
        }
        displays
    }
}

async fn connect_one(name: &str, cfg: &McpServerConfig) -> Result<McpConnection> {
    let (service, transport_desc) = if let Some(ref cmd) = cfg.command {
        let resolved = if let Some(ref pkg) = cfg.package {
            match ensure_installed(cmd, pkg).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("[mcp] '{name}' auto-install failed, falling back to '{cmd}': {e}");
                    PathBuf::from(cmd)
                }
            }
        } else {
            PathBuf::from(cmd)
        };

        if let Err(e) = ensure_indexed(&resolved, cfg).await {
            warn!("[mcp] '{name}' auto-init failed: {e}");
        }

        let mut command = Command::new(&resolved);
        command.args(&cfg.args);
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawn MCP server '{name}': {:?}", resolved))?;
        let service = ClientInfo::default()
            .serve(transport)
            .await
            .with_context(|| format!("connect to MCP server '{name}'"))?;
        (service, "stdio")
    } else if let Some(ref url) = cfg.url {
        let transport = StreamableHttpClientTransport::from_uri(url.as_str());
        let service = ClientInfo::default()
            .serve(transport)
            .await
            .with_context(|| format!("connect to MCP server '{name}' at {url}"))?;
        (service, "http")
    } else {
        anyhow::bail!("MCP server '{name}' has neither `command` nor `url` configured");
    };

    let sink = service.peer().clone();
    let tools = list_tools(&sink)
        .await
        .with_context(|| format!("list tools from MCP server '{name}'"))?;

    info!(
        "[mcp] '{name}' ({transport_desc}): {} tools",
        tools.len()
    );

    tokio::spawn(async move {
        let _ = service.waiting().await;
    });

    Ok(McpConnection {
        name: name.to_string(),
        sink,
        tools,
    })
}

async fn list_tools(peer: &ServerSink) -> Result<Vec<rmcp::model::Tool>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    loop {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor;
        let handle = peer
            .send_cancellable_request(
                ClientRequest::ListToolsRequest(ListToolsRequest::with_param(params)),
                PeerRequestOptions::no_options(),
            )
            .await?;
        let response = handle.await_response().await?;
        let page = match response {
            ServerResult::ListToolsResult(page) => page,
            _ => anyhow::bail!("unexpected response to ListToolsRequest"),
        };
        tools.extend(page.tools);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}
