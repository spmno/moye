use std::collections::HashMap;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::{
    ClientInfo, ClientRequest, ListToolsRequest, PaginatedRequestParams, ServerResult,
};
use rmcp::service::{PeerRequestOptions, ServerSink};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use tokio::process::Command;
use tracing::{error, info};

use crate::config::McpServerConfig;

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
        let mut command = Command::new(cmd);
        command.args(&cfg.args);
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        let transport = TokioChildProcess::new(command)
            .with_context(|| format!("spawn MCP server '{name}': {cmd}"))?;
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
