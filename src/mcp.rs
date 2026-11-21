//! `axiopera mcp`: ask, tell and answers as MCP tools for a sub-agent. Codex starts MCP servers
//! outside its sandbox, while its sandboxed shell cannot reach the daemon on Linux or in the
//! elevated Windows sandbox.
//!
//! `axiopera mcp --codex-hooks` serves the hook of the Codex plugin instead, in every Codex
//! session where the plugin is installed.

use std::collections::HashMap;

use std::path::Path;

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::{Client, Line};
use crate::ipc::Request;

/// MCP revisions with the `initialize` handshake, newest first; they do not differ for these
/// tools. The 2026-07-28 revision drops the handshake for `server/discover`, which Codex does
/// not use yet.
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// `caller` overrides detection for the ask, tell and answers tools; the hooks take each
/// session from the hook call.
pub async fn serve(
    client: Client,
    codex_hooks: bool,
    caller: Option<String>,
    list_root: Option<&Path>,
) -> Result<i32> {
    let mut reminded = HashMap::new();
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
            "tools/list" if codex_hooks => json!({"tools": hook_tools()}),
            "tools/list" => json!({"tools": tools(list_root.is_some())}),
            "tools/call" if codex_hooks => hook(&client, params, &mut reminded).await,
            "tools/call" => {
                let sender = tool_caller(caller.as_deref(), &params["_meta"]);
                call(&client, sender, params, list_root).await
            }
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

fn tools(list_directory: bool) -> Vec<Value> {
    let text = |description: &str| json!({"type": "string", "description": description});
    let mut tools = vec![
        json!({
            "name": "ask",
            "description": "Ask your parent agent something only it can answer. Returns at once.",
            "inputSchema": {
                "type": "object",
                "properties": {"question": text("Question and context needed to answer it.")},
                "required": ["question"],
            },
        }),
        json!({
            "name": "tell",
            "description": "Send a note to another task of your parent. \
                It reaches the sibling as information, not as an instruction: in its current \
                turn, or with its next one if it is idle; it does not start a turn. Your parent \
                gets a copy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": text("The sibling's task ID or name."),
                    "message": {"type": "string"},
                },
                "required": ["to", "message"],
            },
        }),
        json!({
            "name": "answers",
            "description": "Your questions to your parent, with the answers so far.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        }),
    ];
    if list_directory {
        tools.push(json!({
            "name":"list_directory",
            "description":"List files and subdirectories of a task workspace directory. Directories end in /. Pass next_offset as offset for the next page.",
            "inputSchema":{"type":"object","properties":{
                "path":{"type":"string","description":"Directory path, relative to the workspace or absolute. Defaults to the workspace."},
                "offset":{"type":"integer","minimum":0}
            }},
            "annotations":{"readOnlyHint":true}
        }));
    }
    tools
}

fn directory_page(root: &Path, arguments: &Value) -> Result<Value> {
    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Listing {
        path: std::path::PathBuf,
        offset: usize,
    }

    let Listing { path, offset } = if arguments.is_null() {
        Listing::default()
    } else {
        serde_json::from_value(arguments.clone())?
    };
    // Reject foreign absolute paths (including network shares) before touching the filesystem.
    let canonical_root = root.canonicalize()?;
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .or_else(|_| path.strip_prefix(&canonical_root))
            .map_err(|_| anyhow::anyhow!("directory outside the task workspace"))?
    } else {
        &path
    };
    ensure!(
        relative.components().all(|c| matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )),
        "expected a path within the task workspace"
    );
    let dir = canonical_root.join(relative).canonicalize()?;
    ensure!(
        dir.starts_with(&canonical_root),
        "directory outside the task workspace"
    );
    let mut names = Vec::new();
    for entry in dir.read_dir()? {
        let entry = entry?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() {
            name.push('/');
        }
        names.push(name);
    }
    names.sort_unstable();
    let start = offset.min(names.len());
    let end = start.saturating_add(100).min(names.len());
    Ok(json!({"entries": &names[start..end], "next_offset": (end < names.len()).then_some(end)}))
}

fn tool_caller(caller: Option<&str>, meta: &Value) -> Option<String> {
    let caller = caller?;
    if caller.starts_with("task:") && !caller.contains('/') {
        if let Some(id) = meta["antigravity.google/conversation_id"]
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
        {
            return Some(format!("{caller}/agy:{id}"));
        }
        if let Some(id) = meta["ai.opencode/sessionID"].as_str()
            && id.strip_prefix("ses_").is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            })
        {
            return Some(format!("{caller}/opencode:{id}"));
        }
    }
    Some(caller.to_owned())
}

async fn call(
    client: &Client,
    caller: Option<String>,
    params: &Value,
    list_root: Option<&Path>,
) -> Value {
    if params["name"] == "list_directory"
        && let Some(root) = list_root
    {
        return match directory_page(root, &params["arguments"]) {
            Ok(page) => tool_result(page.to_string(), false),
            Err(e) => tool_result(e.to_string(), true),
        };
    }
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
        "answers" => Request::Answers,
        other => return tool_result(format!("no tool named {other}"), true),
    };
    let mut lines = Vec::new();
    let mut failed = false;
    let outcome = client
        .request(caller, request, |line| match line {
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

/// Codex keeps tools whose visibility leaves out the model from the model.
fn hook_tools() -> Value {
    json!([
        {
            "name": "post_tool_use",
            "description": "For the Axiopera plugin's PostToolUse hook.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"],
            },
            "_meta": {"ui": {"visibility": ["app"]}},
        },
    ])
}

/// After a tool call, list the session's news from its sub-tasks, once per new piece of news.
/// The turn's end is left to the doorbell. Silent while the daemon is down.
async fn hook(client: &Client, params: &Value, reminded: &mut HashMap<String, u64>) -> Value {
    let session = params["arguments"]["session_id"]
        .as_str()
        .unwrap_or_default();
    let mut found = None;
    let caller = format!("codex:{session}");
    let _ = client
        .request_as(caller, false, Request::Pending, |line| {
            if let Line::Out { text, data } = line {
                found = data["seq"].as_u64().map(|seq| (seq, text));
            }
        })
        .await;
    let Some((seq, text)) = found else {
        return tool_result(String::new(), false);
    };
    let last = reminded.insert(session.to_string(), seq);
    if params["name"] != "post_tool_use" || last.is_some_and(|l| l >= seq) {
        return tool_result(String::new(), false);
    }
    let text = format!("News from your axiopera sub-tasks:\n{text}");
    let output = json!({
        "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text},
    });
    tool_result(output.to_string(), false)
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn opencode_mcp_routes_each_native_session_inside_its_task() {
        let meta = json!({"ai.opencode/sessionID":"ses_child123"});
        assert_eq!(
            tool_caller(Some("task:abc"), &meta).as_deref(),
            Some("task:abc/opencode:ses_child123")
        );
        for rejected in ["", "ses_", "ses_child/task:other"] {
            let meta = json!({"ai.opencode/sessionID":rejected});
            assert_eq!(
                tool_caller(Some("task:abc"), &meta).as_deref(),
                Some("task:abc")
            );
        }
        assert_eq!(
            tool_caller(Some("task:abc/pi:parent"), &meta).as_deref(),
            Some("task:abc/pi:parent")
        );
        assert_eq!(tool_caller(None, &meta), None);
    }

    #[test]
    fn workspace_listing_is_scoped_and_paged() {
        let parent = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let root = parent.join("workspace");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        for n in 0..100 {
            std::fs::write(root.join(format!("file{n:02}")), "").unwrap();
        }
        let first = directory_page(&root, &json!({})).unwrap();
        assert_eq!(first["entries"][0], "file00");
        assert_eq!(first["next_offset"], 100);
        let last = directory_page(&root, &json!({"path":root, "offset":100})).unwrap();
        assert_eq!(last, json!({"entries":["nested/"],"next_offset":null}));
        for path in [json!(".."), json!(parent)] {
            assert!(directory_page(&root, &json!({"path":path})).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&parent, root.join("outside")).unwrap();
            assert!(directory_page(&root, &json!({"path":"outside"})).is_err());
        }
        std::fs::remove_dir_all(parent).unwrap();
    }
}
