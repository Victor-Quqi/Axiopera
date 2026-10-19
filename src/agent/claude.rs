//! Claude Code over `-p --input-format stream-json --output-format stream-json`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::ChildStdout;
use tokio::sync::oneshot;

use super::{AgentEvent, Decision, Events, Proc, StartSpec, Steer, clip, protocol_text};
use crate::ipc::Tier;

#[derive(Default)]
struct State {
    /// A turn is running or about to start.
    active: bool,
    /// Claude can merge queued inputs into a single replay with one text block per input.
    inputs: super::Inputs,
    /// Background tasks still running. When one finishes, Claude starts a turn by itself.
    background: usize,
    /// Tool calls that came to us for approval; the turn's `permission_denials` repeats the
    /// ones refused, which are reported already.
    asked: HashSet<String>,
}

pub struct Claude {
    pub proc: Proc,
    state: Arc<Mutex<State>>,
}

impl Claude {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Claude> {
        let session = spec
            .resume
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut args: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--replay-user-messages",
            "--permission-prompts",
            "host",
        ]
        .map(String::from)
        .into();
        let mode = match spec.tier {
            Tier::Read => "default",
            Tier::Work => "auto",
            Tier::Full => "bypassPermissions",
        };
        args.extend(["--permission-mode".into(), mode.into()]);
        if spec.tier == Tier::Read {
            args.extend([
                "--permission-prompt-tool".into(),
                "stdio".into(),
                "--disallowedTools".into(),
                "Edit,Write,NotebookEdit,Agent".into(),
            ]);
        }
        // The sub-agent must always be able to talk back through axiopera.
        args.extend(["--allowedTools".into(), "Bash(axiopera *)".into()]);
        // Unattended, Claude starts project .mcp.json servers nobody approved; the TUI would ask.
        let unapproved = unapproved_project_servers(spec);
        if !unapproved.is_empty() {
            let settings = json!({"disabledMcpjsonServers": unapproved});
            args.extend(["--settings".into(), settings.to_string()]);
        }
        args.extend(["--append-system-prompt".into(), protocol_text(spec)]);
        if let Some(m) = &spec.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(e) = &spec.effort {
            args.extend(["--effort".into(), e.clone()]);
        }
        if spec.resume.is_some() {
            args.extend(["--resume".into(), session.clone()]);
        } else {
            args.extend(["--session-id".into(), session.clone()]);
        }
        let (proc, stdout) = Proc::spawn(exe, &args, spec)?;
        let _ = events.send(AgentEvent::Session {
            file: Some(session_file(spec, &session)),
            id: session,
        });
        let state: Arc<Mutex<State>> = Arc::default();
        tokio::spawn(pump(
            stdout,
            proc.stdin.clone(),
            state.clone(),
            events,
            spec.tier,
        ));
        Ok(Claude { proc, state })
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        self.input(text, None).await
    }

    async fn input(&mut self, text: &str, id: Option<u64>) -> Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.active = true;
            s.inputs.push(id, text);
        }
        let msg = json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
        self.proc.stdin.send(&msg).await?;
        Ok(())
    }

    /// A user message mid-turn is injected after the current tool result.
    pub async fn steer(&mut self, text: &str, id: Option<u64>) -> Result<Steer> {
        if !self.busy() {
            return Ok(Steer::NoActiveTurn);
        }
        self.input(text, id).await?;
        Ok(Steer::Accepted)
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let msg = json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}});
        self.proc.stdin.send(&msg).await?;
        Ok(())
    }

    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.active || s.background > 0
    }

    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().active
    }
}

/// `~/.claude/projects/<cwd with every non-alphanumeric char as '-'>/<id>.jsonl`
fn session_file(spec: &StartSpec, id: &str) -> PathBuf {
    let config = config_dir(spec);
    let slug: String = spec
        .cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config
        .join("projects")
        .join(slug)
        .join(format!("{id}.jsonl"))
}

fn config_dir(spec: &StartSpec) -> PathBuf {
    spec.env
        .iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(|| super::home_dir().join(".claude"))
}

fn read_json(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// Servers in the project's `.mcp.json` that the user has not approved in Claude Code.
/// Only the approval keys are looked at; nothing is logged.
fn unapproved_project_servers(spec: &StartSpec) -> Vec<String> {
    let declared = read_json(&spec.cwd.join(".mcp.json"));
    let Some(servers) = declared["mcpServers"].as_object() else {
        return Vec::new();
    };

    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let cwd = norm(&spec.cwd.to_string_lossy());
    let global_file = match spec.env.iter().find(|(k, _)| k == "CLAUDE_CONFIG_DIR") {
        Some((_, dir)) => PathBuf::from(dir).join(".claude.json"),
        None => super::home_dir().join(".claude.json"),
    };
    let global = read_json(&global_file);
    let project = global["projects"]
        .as_object()
        .and_then(|p| {
            p.iter()
                .find(|(k, _)| norm(k) == cwd)
                .map(|(_, v)| v.clone())
        })
        .unwrap_or(Value::Null);
    let sources = [
        project,
        read_json(&config_dir(spec).join("settings.json")),
        read_json(&spec.cwd.join(".claude/settings.json")),
        read_json(&spec.cwd.join(".claude/settings.local.json")),
    ];
    if sources
        .iter()
        .any(|s| s["enableAllProjectMcpServers"] == true)
    {
        return Vec::new();
    }
    let approved: Vec<&str> = sources
        .iter()
        .flat_map(|s| s["enabledMcpjsonServers"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    servers
        .keys()
        .filter(|name| !approved.contains(&name.as_str()))
        .cloned()
        .collect()
}

const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// The most telling argument of a tool call, for one-line summaries.
fn tool_detail(input: &Value) -> String {
    [
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|k| input[*k].as_str())
    .map(|s| clip(s, 200))
    .unwrap_or_else(|| clip(&input.to_string(), 200))
}

fn replay_input(content: &Value, state: &Mutex<State>, events: &Events) {
    let mut s = state.lock().unwrap();
    if let Some(text) = content.as_str() {
        s.inputs.read(text, events);
    } else {
        for block in content.as_array().into_iter().flatten() {
            if let Some(text) = block["text"].as_str() {
                s.inputs.read(text, events);
            }
        }
    }
}

type StatusRequests = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

fn read_only_mcp(status: &Value, tool: &str) -> bool {
    status["mcpServers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|server| {
            let Some(name) = server["name"].as_str() else {
                return false;
            };
            server["tools"].as_array().into_iter().flatten().any(|t| {
                let Some(name) = t["name"].as_str().map(|t| format!("mcp__{name}__{t}")) else {
                    return false;
                };
                // mcp_status drops the Hint suffix from the server's annotation keys.
                name == tool
                    && t["annotations"]["readOnly"] == true
                    && t["annotations"]["destructive"] != true
            })
        })
}

async fn read_decision(tool: &str, stdin: &super::Writer, pending: &StatusRequests) -> Decision {
    if !tool.starts_with("mcp__") {
        return Decision::Deny(super::REFUSED.into());
    }
    // Read fresh metadata while this call waits for approval; tools can change mid-session.
    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    pending.lock().unwrap().insert(id.clone(), tx);
    let sent = stdin
        .send(&json!({"type": "control_request", "request_id": id,
                      "request": {"subtype": "mcp_status"}}))
        .await;
    let response = if sent.is_ok() {
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .ok()
            .and_then(Result::ok)
    } else {
        None
    };
    pending.lock().unwrap().remove(&id);
    match response {
        Some(r) if r["subtype"] == "success" => {
            if read_only_mcp(&r["response"], tool) {
                Decision::Approve
            } else {
                Decision::Deny(
                    "Read-tier MCP calls require readOnlyHint:true without destructiveHint:true."
                        .into(),
                )
            }
        }
        _ => Decision::Deny("MCP status unavailable; read-tier call refused.".into()),
    }
}

async fn pump(
    stdout: ChildStdout,
    stdin: super::Writer,
    state: Arc<Mutex<State>>,
    events: Events,
    tier: Tier,
) {
    let pending: StatusRequests = Arc::default();
    let mut lines = super::lines(stdout);
    while let Ok(Some(line)) = lines.next_line().await {
        stdin.trace.line("recv", &line);
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match m["type"].as_str().unwrap_or("") {
            "user" => replay_input(&m["message"]["content"], &state, &events),
            "assistant" => {
                for block in m["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let name = block["name"].as_str().unwrap_or("?").to_string();
                    let input = &block["input"];
                    let detail = tool_detail(input);
                    if EDIT_TOOLS.contains(&name.as_str()) {
                        for k in ["file_path", "notebook_path"] {
                            if let Some(p) = input[k].as_str() {
                                let _ = events.send(AgentEvent::FileChanged(p.to_string()));
                            }
                        }
                    }
                    let _ = events.send(AgentEvent::Tool { name, detail });
                }
            }
            "result" => {
                let denied: Vec<String> = {
                    let s = state.lock().unwrap();
                    // Calls Claude's own permission mode refused, without asking anyone.
                    m["permission_denials"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|d| {
                            !d["tool_use_id"]
                                .as_str()
                                .is_some_and(|id| s.asked.contains(id))
                        })
                        .map(|d| {
                            let tool = d["tool_name"].as_str().unwrap_or("?");
                            format!("{tool}: {}", tool_detail(&d["tool_input"]))
                        })
                        .collect()
                };
                for d in denied {
                    let _ = events.send(AgentEvent::Blocked(d));
                }
                let ok = m["subtype"] == "success" && m["is_error"] != true;
                let result = m["result"].as_str().unwrap_or("").to_string();
                let error = (!ok).then(|| m["subtype"].as_str().unwrap_or("error").to_string());
                let mut s = state.lock().unwrap();
                s.active = !s.inputs.0.is_empty();
                let _ = events.send(AgentEvent::TurnEnded {
                    ok,
                    result,
                    error,
                    usage: m.get("usage").cloned(),
                });
            }
            "system" => match m["subtype"].as_str().unwrap_or("") {
                "background_tasks_changed" => {
                    let n = m["tasks"].as_array().map_or(0, Vec::len);
                    state.lock().unwrap().background = n;
                }
                // A finished background task makes Claude start a turn to report it.
                "task_notification" => {
                    let mut s = state.lock().unwrap();
                    if !s.active {
                        s.active = true;
                        let _ = events.send(AgentEvent::TurnStarted);
                    }
                }
                _ => {}
            },
            "control_request" => {
                let id = m["request_id"].clone();
                let req = &m["request"];
                if req["subtype"] != "can_use_tool" {
                    let subtype = req["subtype"].as_str().unwrap_or("unknown request");
                    let _ = events.send(AgentEvent::Blocked(format!(
                        "Claude {subtype}: not supported by axiopera"
                    )));
                    let response = json!({"subtype": "error", "request_id": id, "error": "not supported by axiopera"});
                    let _ = stdin
                        .send(&json!({"type": "control_response", "response": response}))
                        .await;
                    continue;
                }
                if let Some(id) = req["tool_use_id"].as_str() {
                    state.lock().unwrap().asked.insert(id.to_string());
                }
                let tool = req["tool_name"].as_str().unwrap_or("?").to_string();
                let input = req["input"].clone();
                let summary = format!("{tool}: {}", tool_detail(&input));
                let detail = format!("{tool}\n{}", super::arguments(&input));
                let approval = (tier != Tier::Read)
                    .then(|| super::ask_approval(&events, summary.clone(), detail));
                let stdin = stdin.clone();
                let pending = pending.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let decision = match approval {
                        Some(approval) => approval.await,
                        None => {
                            let decision = read_decision(&tool, &stdin, &pending).await;
                            if let Decision::Deny(reason) = &decision {
                                let _ = events
                                    .send(AgentEvent::Blocked(format!("{summary}: {reason}")));
                            }
                            decision
                        }
                    };
                    // Never updatedPermissions: nothing the parent approves is remembered.
                    let behavior = match decision {
                        Decision::Approve => json!({"behavior": "allow", "updatedInput": input}),
                        Decision::Deny(message) => json!({"behavior": "deny", "message": message}),
                    };
                    let response =
                        json!({"subtype": "success", "request_id": id, "response": behavior});
                    let _ = stdin
                        .send(&json!({"type": "control_response", "response": response}))
                        .await;
                });
            }
            "control_response" => {
                let response = &m["response"];
                if let Some(id) = response["request_id"].as_str()
                    && let Some(tx) = pending.lock().unwrap().remove(id)
                {
                    let _ = tx.send(response.clone());
                }
            }
            _ => {}
        }
    }
    pending.lock().unwrap().clear();
    state.lock().unwrap().active = false;
    let _ = events.send(AgentEvent::Exited("claude exited".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_replay_confirms_both_inputs_and_ignores_tool_results() {
        let state = Mutex::new(State::default());
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.lock().unwrap().inputs.push(None, "initial");
        state.lock().unwrap().inputs.push(Some(1), "first");
        state.lock().unwrap().inputs.push(Some(2), "second");
        replay_input(
            &json!([{"type": "tool_result", "content": "first"}]),
            &state,
            &events,
        );
        assert_eq!(state.lock().unwrap().inputs.0.len(), 3);
        replay_input(&json!("initial"), &state, &events);
        replay_input(
            &json!([
                {"type": "text", "text": "first\n"},
                {"type": "text", "text": "second\n"}
            ]),
            &state,
            &events,
        );
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::InputRead(1))));
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::InputRead(2))));
        assert!(state.lock().unwrap().inputs.0.is_empty());
    }

    #[test]
    fn mcp_requires_explicit_read_only_and_no_destructive_hint() {
        for (annotations, expected) in [
            (json!({"readOnly": true}), true),
            (json!({"readOnly": true, "destructive": false}), true),
            (json!({"readOnly": true, "destructive": true}), false),
            (json!({"readOnly": false, "destructive": false}), false),
            (json!({"destructive": false}), false),
            (json!({}), false),
            (Value::Null, false),
        ] {
            let status = json!({"mcpServers": [{"name": "server", "tools": [
                {"name": "tool", "annotations": annotations}
            ]}]});
            assert_eq!(read_only_mcp(&status, "mcp__server__tool"), expected);
        }
    }

    #[test]
    fn mcp_matches_the_server_and_tool_from_status() {
        let status = json!({"mcpServers": [{"name": "server", "tools": [
            {"name": "tool", "annotations": {"readOnly": true}},
            {"name": "unmarked"}
        ]}]});
        for tool in [
            "mcp__other__tool",
            "mcp__server__unknown",
            "mcp__server__unmarked",
            "Bash",
        ] {
            assert!(!read_only_mcp(&status, tool), "{tool}");
        }
        assert!(!read_only_mcp(&Value::Null, "mcp__server__tool"));
    }

    #[tokio::test]
    async fn unavailable_status_refuses_and_clears_the_request() {
        let stdin = super::super::Writer {
            stdin: Arc::new(tokio::sync::Mutex::new(None)),
            trace: Default::default(),
        };
        let pending = StatusRequests::default();
        assert!(matches!(
            read_decision("mcp__server__tool", &stdin, &pending).await,
            Decision::Deny(_)
        ));
        assert!(pending.lock().unwrap().is_empty());
        assert!(matches!(
            read_decision("Bash", &stdin, &pending).await,
            Decision::Deny(_)
        ));
        assert!(pending.lock().unwrap().is_empty());
    }
}
