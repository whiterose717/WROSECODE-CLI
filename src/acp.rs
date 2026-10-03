use crate::agent::Agent;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn serve(mut agent: Agent) -> Result<()> {
    let input = BufReader::new(tokio::io::stdin());
    let mut lines = input.lines();
    let mut output = tokio::io::stdout();
    let session = "wrosecode-session";
    while let Some(line) = lines.next_line().await? {
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or("");
        if !matches!(
            method,
            "initialize" | "session/new" | "session/prompt" | "session/cancel"
        ) {
            if let Some(id) = id {
                let reply = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}});
                output.write_all(format!("{reply}\n").as_bytes()).await?;
                output.flush().await?;
            }
            continue;
        }
        let result = match method {
            "initialize" => {
                json!({"protocolVersion":1,"agentInfo":{"name":"wrosecode","version":env!("CARGO_PKG_VERSION")},
                "agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false}},"authMethods":[]})
            }
            "session/new" => {
                if let Some(cwd) = request["params"]["cwd"].as_str() {
                    let path = std::path::Path::new(cwd);
                    if path.is_dir() {
                        let mut config = (*agent.config).clone();
                        config.root = path.canonicalize()?;
                        let mcps = agent.tools.mcps.clone();
                        agent = Agent::new(
                            Arc::new(config),
                            agent.provider.clone(),
                            agent.tools.client.clone(),
                        )?;
                        agent.tools.mcps = mcps;
                    }
                }
                json!({"sessionId":session,"models":{"currentModelId":agent.config.model,"availableModels":[]}})
            }
            "session/prompt" => {
                let text = request["params"]["prompt"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                match agent.turn(&text).await {
                    Ok(reply) => {
                        let update = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,
                            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":reply}}}});
                        output.write_all(format!("{update}\n").as_bytes()).await?;
                        json!({"stopReason":"end_turn"})
                    }
                    Err(error) => json!({"stopReason":"end_turn","error":error.to_string()}),
                }
            }
            "session/cancel" => json!({}),
            _ => json!({}),
        };
        if let Some(id) = id {
            let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
            output.write_all(format!("{reply}\n").as_bytes()).await?;
            output.flush().await?;
        }
    }
    Ok(())
}
