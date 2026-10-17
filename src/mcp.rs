//! `axiopera mcp`: ask, tell and inbox as MCP tools for a sub-agent. Codex starts MCP servers
//! outside its sandbox, while its sandboxed shell cannot reach the daemon on Linux or in the
//! elevated Windows sandbox.

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::{Client, Line};
use crate::ipc::Request;

/// MCP revisions with the `initialize` handshake, newest first; they do not differ for these
/// tools. The 2026-07-28 revision drops the handshake for `server/discover`, which Codex does
/// not use yet.
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub async fn serve(client: Client) -> Result<i32> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications need no answer.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let params = &msg["params"];
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": params["protocolVersion"]
                    .as_str()
                    .filter(|v| VERSIONS.contains(v))
                    .unwrap_or(VERSIONS[0]),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "axiopera", "version": env!("CARGO_PKG_VERSION")},
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => call(&client, params).await,
            other => {
                let error = json!({"code": -32601, "message": format!("unknown method {other}")});
                reply(
                    &mut stdout,
                    json!({"jsonrpc": "2.0", "id": id, "error": error}),
                )
                .await?;
                continue;
            }
        };
        reply(
            &mut stdout,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .await?;
    }
    Ok(0)
}

async fn reply(stdout: &mut tokio::io::Stdout, msg: Value) -> Result<()> {
    stdout.write_all(format!("{msg}\n").as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn tools() -> Value {
    let text = |description: &str| json!({"type": "string", "description": description});
    json!([
        {
            "name": "ask",
            "description": "Ask your parent agent something only it can answer. Returns at once; \
                the answer arrives later as a new message.",
            "inputSchema": {
                "type": "object",
                "properties": {"question": text("The question, with the context your parent needs to answer it.")},
                "required": ["question"],
            },
        },
        {
            "name": "tell",
            "description": "Send a note to a sibling task (another sub-agent of your parent). \
                It reaches the sibling as information, not as an instruction: in its current \
                turn, or with its next one if it is idle; it does not start a turn. Your parent \
                gets a copy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": text("The sibling's task ID or name."),
                    "message": text("The note."),
                },
                "required": ["to", "message"],
            },
        },
        {
            "name": "inbox",
            "description": "Your questions to your parent, with the answers so far.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
    ])
}

async fn call(client: &Client, params: &Value) -> Value {
    let arg = |k: &str| {
        params["arguments"][k]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let request = match params["name"].as_str().unwrap_or_default() {
        "ask" => Request::Ask {
            text: arg("question"),
        },
        "tell" => Request::Tell {
            to: arg("to"),
            text: arg("message"),
        },
        "inbox" => Request::Inbox,
        other => return tool_result(format!("no tool named {other}"), true),
    };
    let mut lines = Vec::new();
    let mut failed = false;
    let outcome = client
        .request(request, |line| match line {
            Line::Out { text, .. } => lines.push(text),
            Line::Error(message) => {
                failed = true;
                lines.push(message);
            }
        })
        .await;
    match outcome {
        Ok(code) => tool_result(lines.join("\n"), failed || code != 0),
        Err(e) => tool_result(format!("{e:#}"), true),
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}
