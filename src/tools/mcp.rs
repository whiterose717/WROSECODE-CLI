use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

pub struct Mcp {
    state: Mutex<State>,
    pub schemas: Vec<Value>,
}

struct State {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    id: u64,
}

impl Mcp {
    pub async fn connect(binary: &str, args: &[String]) -> Result<Arc<Self>> {
        let mut child = Command::new(binary)
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start MCP server {binary}"))?;
        let stdin = child.stdin.take().context("MCP stdin unavailable")?;
        let stdout = child.stdout.take().context("MCP stdout unavailable")?;
        let client = Arc::new(Self {
            state: Mutex::new(State {
                _child: child,
                stdin,
                lines: BufReader::new(stdout).lines(),
                id: 0,
            }),
            schemas: Vec::new(),
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client.request(
                "initialize",
                json!({"protocolVersion":"2025-03-26","capabilities":{},
            "clientInfo":{"name":"wrosecode","version":env!("CARGO_PKG_VERSION")}}),
            ),
        )
        .await??;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        let listed = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client.request("tools/list", json!({})),
        )
        .await??;
        let schemas = listed["tools"].as_array().context("MCP tools/list omitted tools")?.iter().filter_map(|tool| {
            let name = tool["name"].as_str()?;
            Some(json!({"name":format!("mcp__{name}"),"description":tool["description"].as_str().unwrap_or("MCP tool"),
                "input_schema":tool["inputSchema"]}))
        }).collect();
        let mut client =
            Arc::try_unwrap(client).map_err(|_| anyhow::anyhow!("MCP client still shared"))?;
        client.schemas = schemas;
        Ok(Arc::new(client))
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name":name,"arguments":arguments}))
            .await?;
        let output = result["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result["isError"].as_bool().unwrap_or(false) {
            bail!("MCP tool error: {output}");
        }
        Ok(output)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut state = self.state.lock().await;
        state
            .stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        state.stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut state = self.state.lock().await;
        state.id += 1;
        let id = state.id;
        state
            .stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        state.stdin.flush().await?;
        while let Some(line) = state.lines.next_line().await? {
            let message: Value = serde_json::from_str(&line).context("invalid MCP JSON line")?;
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if !message["error"].is_null() {
                bail!("MCP error: {}", message["error"]);
            }
            return Ok(message["result"].clone());
        }
        bail!("MCP server closed stdout")
    }
}
