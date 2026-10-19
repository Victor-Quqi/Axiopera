//! Codex over `codex app-server` (JSON-RPC), one process per task.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::rpc::{Incoming, Rpc};
use super::{AgentEvent, Decision, Events, Proc, StartSpec, Steer, clip, protocol_text};
use crate::ipc::Tier;
use crate::{identity, paths};

#[derive(Default)]
struct State {
    turn: Option<String>,
    /// Turns already completed, so a late `turn/start` reply cannot resurrect one.
    done: HashSet<String>,
    last_message: String,
    usage: Option<Value>,
    inputs: super::Inputs,
    recorded: Vec<u64>,
    /// Changes of file edits not finished yet, by item ID, for their approval requests.
    patches: HashMap<String, Value>,
}

pub struct Codex {
    pub proc: Proc,
    rpc: Rpc,
    thread: String,
    effort: Option<String>,
    state: Arc<Mutex<State>>,
}

impl Codex {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Codex> {
        let (proc, stdout) = Proc::spawn(exe, &["app-server".into()], spec)?;
        let (rpc, incoming) = Rpc::start(stdout, proc.stdin.clone());
        let state: Arc<Mutex<State>> = Arc::default();
        tokio::spawn(pump(incoming, rpc.clone(), state.clone(), events.clone()));

        rpc.call(
            "initialize",
            json!({"clientInfo": {"name": "axiopera", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        rpc.notify("initialized", json!({})).await?;

        let (sandbox, approval) = match spec.tier {
            Tier::Read => ("read-only", "never"),
            Tier::Work => ("workspace-write", "on-request"),
            // on-request rather than never: never turns Codex's own dangerous-command checks into
            // silent failures; on-request turns them into approval requests.
            Tier::Full => ("danger-full-access", "on-request"),
        };
        let mut params = json!({
            "cwd": spec.cwd,
            "sandbox": sandbox,
            "approvalPolicy": approval,
            "developerInstructions": protocol_text(spec),
        });
        // Explicit every time: a reviewer set in the user's config would otherwise carry over.
        let reviewer = if spec.tier == Tier::Work {
            "auto_review"
        } else {
            "user"
        };
        params["approvalsReviewer"] = json!(reviewer);
        if let Some(m) = &spec.model {
            params["model"] = json!(m);
        }
        let mut config = Map::new();
        if spec.tier == Tier::Full {
            // on-request brings back per-tool MCP approval; full keeps MCP calls unprompted.
            for name in mcp_servers(&rpc, &spec.cwd).await {
                config.insert(
                    format!("mcp_servers.{name}.default_tools_approval_mode"),
                    json!("approve"),
                );
            }
        }
        config.insert("mcp_servers.axiopera".into(), axiopera_mcp(spec)?);
        params["config"] = Value::Object(config);
        let reply = match &spec.resume {
            Some(id) => {
                params["threadId"] = json!(id);
                rpc.call("thread/resume", params).await?
            }
            None => rpc.call("thread/start", params).await?,
        };
        let thread = reply["thread"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let file = reply["thread"]["path"].as_str().map(PathBuf::from);
        let _ = events.send(AgentEvent::Session {
            id: thread.clone(),
            file,
        });
        Ok(Codex {
            proc,
            rpc,
            thread,
            effort: spec.effort.clone(),
            state,
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        let mut params =
            json!({"threadId": self.thread, "input": [{"type": "text", "text": text}]});
        if let Some(e) = &self.effort {
            params["effort"] = json!(e);
        }
        let reply = self.rpc.call("turn/start", params).await?;
        if let Some(id) = reply["turn"]["id"].as_str() {
            let mut s = self.state.lock().unwrap();
            if !s.done.contains(id) {
                s.turn = Some(id.to_string());
            }
        }
        Ok(())
    }

    pub async fn steer(&mut self, text: &str, id: Option<u64>) -> Result<Steer> {
        let Some(turn) = self.state.lock().unwrap().turn.clone() else {
            return Ok(Steer::NoActiveTurn);
        };
        let params = json!({"threadId": self.thread, "expectedTurnId": turn, "input": [{"type": "text", "text": text}]});
        self.state.lock().unwrap().inputs.push(id, text);
        let rx = self.rpc.request("turn/steer", params).await?;
        match rx.await {
            Ok(Ok(_)) => Ok(Steer::Accepted),
            // The turn ended between our check and the steer.
            Ok(Err(e)) if e.code == -32600 => {
                self.state
                    .lock()
                    .unwrap()
                    .inputs
                    .0
                    .retain(|(sent_id, sent)| *sent_id != id || sent != text);
                Ok(Steer::NoActiveTurn)
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => anyhow::bail!("codex exited"),
        }
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        let turn = self.state.lock().unwrap().turn.clone();
        if let Some(turn) = turn {
            self.rpc
                .call(
                    "turn/interrupt",
                    json!({"threadId": self.thread, "turnId": turn}),
                )
                .await?;
        }
        Ok(())
    }

    pub fn busy(&self) -> bool {
        self.state.lock().unwrap().turn.is_some()
    }
}

/// ask, tell and answers as MCP tools. Codex starts MCP servers outside its sandbox, so they reach
/// the daemon where its sandboxed shell cannot (Linux, the elevated Windows sandbox). It passes
/// MCP servers only a few environment variables; ours get the two they need.
fn axiopera_mcp(spec: &StartSpec) -> Result<Value> {
    let var = |name: &str| {
        spec.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    Ok(json!({
        "command": std::env::current_exe()?,
        "args": ["mcp"],
        "env": {
            identity::TASK_VAR: spec.task_id,
            paths::HOME_VAR: var(paths::HOME_VAR),
        },
        "default_tools_approval_mode": "approve",
    }))
}

/// Names of the configured MCP servers. Only the keys are read; values may hold secrets.
async fn mcp_servers(rpc: &Rpc, cwd: &Path) -> Vec<String> {
    match rpc.call("config/read", json!({"cwd": cwd})).await {
        Ok(v) => v["config"]["mcp_servers"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn pump(
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    rpc: Rpc,
    state: Arc<Mutex<State>>,
    events: Events,
) {
    while let Some(msg) = incoming.recv().await {
        match msg {
            Incoming::Notification { method, params } => {
                on_notification(&method, &params, &state, &events)
            }
            Incoming::Request { id, method, params } => {
                let (summary, detail) = {
                    let patches = &state.lock().unwrap().patches;
                    (
                        summary(&method, &params, patches),
                        request_detail(&method, &params, patches),
                    )
                };
                if !FORWARDED.contains(&method.as_str()) {
                    let _ = events.send(AgentEvent::Blocked(summary));
                    let _ = match refusal(&method) {
                        Some(r) => rpc.respond(id, r).await,
                        None => rpc.respond_error(id, "not supported by axiopera").await,
                    };
                    continue;
                }
                let decision = super::ask_approval(&events, summary, detail);
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    match decision.await {
                        Decision::Approve => {
                            if let Some(r) = approval(&method, &params) {
                                let _ = rpc.respond(id, r).await;
                            }
                        }
                        Decision::Deny(reason) => {
                            if let Some(r) = refusal(&method) {
                                let _ = rpc.respond(id, r).await;
                            }
                            // Codex's refusals carry no message; tell the turn why.
                            let steer = json!({
                                "threadId": params["threadId"],
                                "expectedTurnId": params["turnId"],
                                "input": [{"type": "text", "text": reason}],
                            });
                            let _ = rpc.request("turn/steer", steer).await;
                        }
                    }
                });
            }
            Incoming::Reply(_) => {}
        }
    }
    state.lock().unwrap().turn = None;
    let _ = events.send(AgentEvent::Exited("codex exited".into()));
}

fn on_notification(method: &str, p: &Value, state: &Mutex<State>, events: &Events) {
    let item = &p["item"];
    // Finishing a turn can persist pending input without another model request. Only a
    // subsequent model response shows that the recorded input reached this turn's model.
    if method == "item/started"
        && matches!(item["type"].as_str(), Some("agentMessage" | "reasoning"))
    {
        for id in state.lock().unwrap().recorded.drain(..) {
            let _ = events.send(AgentEvent::InputRead(id));
        }
    }
    match method {
        "turn/started" => {
            if let Some(id) = p["turn"]["id"].as_str() {
                let mut s = state.lock().unwrap();
                if !s.done.contains(id) {
                    s.turn = Some(id.to_string());
                }
            }
        }
        "item/started" => match item["type"].as_str() {
            Some("commandExecution") => {
                let detail = clip(item["command"].as_str().unwrap_or(""), 200);
                let _ = events.send(AgentEvent::Tool {
                    name: "shell".into(),
                    detail,
                });
            }
            Some("fileChange") => {
                if let Some(id) = item["id"].as_str() {
                    state
                        .lock()
                        .unwrap()
                        .patches
                        .insert(id.to_string(), item["changes"].clone());
                }
            }
            Some("mcpToolCall") => {
                let name = format!(
                    "{}.{}",
                    item["server"].as_str().unwrap_or("?"),
                    item["tool"].as_str().unwrap_or("?")
                );
                let _ = events.send(AgentEvent::Tool {
                    name,
                    detail: String::new(),
                });
            }
            _ => {}
        },
        "item/completed" => match item["type"].as_str() {
            Some("userMessage") => {
                let mut s = state.lock().unwrap();
                let text = super::content_text(&item["content"]);
                if let Some(i) = s.inputs.0.iter().position(|(_, sent)| *sent == text)
                    && let Some((Some(id), _)) = s.inputs.0.remove(i)
                {
                    s.recorded.push(id);
                }
            }
            Some("agentMessage") => {
                state.lock().unwrap().last_message =
                    item["text"].as_str().unwrap_or("").to_string();
            }
            Some("fileChange") => {
                if let Some(id) = item["id"].as_str() {
                    state.lock().unwrap().patches.remove(id);
                }
                for c in item["changes"].as_array().into_iter().flatten() {
                    if let Some(path) = c["path"].as_str() {
                        let _ = events.send(AgentEvent::FileChanged(path.to_string()));
                    }
                }
            }
            _ => {}
        },
        "thread/tokenUsage/updated" => state.lock().unwrap().usage = p.get("tokenUsage").cloned(),
        // The auto_review reviewer turned an escalation down; the agent only sees a failure.
        // The payload is marked unstable, so only the plain fields are read.
        "item/autoApprovalReview/completed" => {
            let review = &p["review"];
            let status = review["status"].as_str().unwrap_or("");
            if matches!(status, "denied" | "timedOut") {
                let rationale = review["rationale"].as_str().unwrap_or("");
                let _ = events.send(AgentEvent::Blocked(format!(
                    "auto review {status}: {}",
                    clip(rationale, 300)
                )));
            }
        }
        "turn/completed" => {
            let turn = &p["turn"];
            let ok = turn["status"] == "completed";
            let mut s = state.lock().unwrap();
            if let Some(id) = turn["id"].as_str() {
                s.done.insert(id.to_string());
            }
            s.turn = None;
            s.inputs.0.clear();
            s.recorded.clear();
            let result = std::mem::take(&mut s.last_message);
            let usage = s.usage.clone();
            let error = (!ok).then(|| {
                turn["error"]["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| turn["status"].to_string())
            });
            let _ = events.send(AgentEvent::TurnEnded {
                ok,
                result,
                error,
                usage,
            });
        }
        _ => {}
    }
}

/// Requests passed on for someone's decision; the rest are refused on the spot.
const FORWARDED: [&str; 3] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
];

fn summary(method: &str, p: &Value, patches: &HashMap<String, Value>) -> String {
    let text = |k: &str| p[k].as_str().map(str::to_string);
    let (kind, what) = match method {
        "item/commandExecution/requestApproval" => ("shell", text("command").or(text("reason"))),
        "item/fileChange/requestApproval" => {
            let paths: Vec<&str> = patches
                .get(p["itemId"].as_str().unwrap_or_default())
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c["path"].as_str())
                .collect();
            let paths = (!paths.is_empty()).then(|| paths.join(", "));
            ("edit", paths.or(text("reason")))
        }
        "item/permissions/requestApproval" => ("permissions", text("reason")),
        _ => (method, text("message").or(text("reason"))),
    };
    let what = what.unwrap_or_else(|| p["permissions"].to_string());
    format!("{kind}: {}", clip(&what, 200))
}

/// The whole request: a command with its directory and reason, or a file edit's diffs.
fn request_detail(method: &str, p: &Value, patches: &HashMap<String, Value>) -> String {
    let mut lines = Vec::new();
    let field = |k: &str| p[k].as_str().filter(|v| !v.is_empty());
    if let Some(command) = field("command") {
        lines.push(format!("command:\n{command}"));
    }
    for (k, label) in [
        ("cwd", "cwd"),
        ("reason", "reason"),
        ("grantRoot", "grant root"),
    ] {
        if let Some(v) = field(k) {
            lines.push(format!("{label}: {v}"));
        }
    }
    if method == "item/permissions/requestApproval" {
        lines.push(format!("permissions: {}", p["permissions"]));
    }
    let item = p["itemId"].as_str().unwrap_or_default();
    for change in patches
        .get(item)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = change["path"].as_str().unwrap_or("?");
        let diff = change["diff"].as_str().unwrap_or("");
        let count = |sign: char, header: &str| {
            diff.lines()
                .filter(|l| l.starts_with(sign) && !l.starts_with(header))
                .count()
        };
        let kind = match change["kind"]["type"].as_str() {
            Some("update") => match change["kind"]["move_path"].as_str() {
                Some(to) => format!("update and move to {to}"),
                None => "update".to_string(),
            },
            Some(other) => other.to_string(),
            None => "change".to_string(),
        };
        lines.push(format!(
            "{kind} {path} (+{} -{}):\n{diff}",
            count('+', "+++"),
            count('-', "---")
        ));
    }
    if lines.is_empty() {
        return format!("{method}\n{}", super::arguments(p));
    }
    lines.join("\n")
}

/// Approval without anything that persists (no acceptForSession, no execpolicy amendment).
fn approval(method: &str, p: &Value) -> Option<Value> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({"decision": "accept"}))
        }
        "item/permissions/requestApproval" => {
            Some(json!({"permissions": p["permissions"], "scope": "turn"}))
        }
        _ => None,
    }
}

fn refusal(method: &str) -> Option<Value> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({"decision": "decline"}))
        }
        "item/permissions/requestApproval" => Some(json!({"permissions": {}})),
        "mcpServer/elicitation/request" => Some(json!({"action": "decline"})),
        "item/tool/requestUserInput" => Some(json!({"answers": {}})),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(state: &Mutex<State>, events: &Events, text: &str) {
        on_notification(
            "item/completed",
            &json!({"item": {
                "type": "userMessage", "content": [{"type": "text", "text": text}]
            }}),
            state,
            events,
        );
    }

    #[test]
    fn user_item_needs_a_following_model_response() {
        let state = Mutex::new(State::default());
        let (events, mut rx) = mpsc::unbounded_channel();
        state.lock().unwrap().inputs.push(Some(1), "first");
        state.lock().unwrap().inputs.push(Some(2), "second");
        recorded(&state, &events, "first");
        recorded(&state, &events, "second");
        assert!(rx.try_recv().is_err());
        on_notification(
            "item/started",
            &json!({"item": {"type": "agentMessage"}}),
            &state,
            &events,
        );
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::InputRead(1))));
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::InputRead(2))));
    }

    #[test]
    fn recorded_at_turn_end_is_not_confirmed_by_the_next_turn() {
        let state = Mutex::new(State::default());
        let (events, mut rx) = mpsc::unbounded_channel();
        state.lock().unwrap().inputs.push(Some(1), "late");
        recorded(&state, &events, "late");
        on_notification(
            "turn/completed",
            &json!({"turn": {"id": "one", "status": "completed"}}),
            &state,
            &events,
        );
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::TurnEnded { .. })));
        on_notification(
            "item/started",
            &json!({"item": {"type": "agentMessage"}}),
            &state,
            &events,
        );
        assert!(rx.try_recv().is_err());
    }
}
