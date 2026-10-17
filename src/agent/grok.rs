//! Grok Build over `grok agent stdio` (ACP).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::rpc::{Incoming, Rpc};
use super::{AgentEvent, Decision, Events, Proc, REFUSED, StartSpec, Steer, clip, protocol_text};
use crate::ipc::Tier;

/// Grok imports Claude Code's MCP servers, hooks, skills, rules and agents unless told not to.
pub const ENV: [(&str, &str); 5] = [
    ("GROK_CLAUDE_MCPS_ENABLED", "0"),
    ("GROK_CLAUDE_HOOKS_ENABLED", "0"),
    ("GROK_CLAUDE_SKILLS_ENABLED", "0"),
    ("GROK_CLAUDE_RULES_ENABLED", "0"),
    ("GROK_CLAUDE_AGENTS_ENABLED", "0"),
];

/// Built-in tools a read-tier Grok does without (names as of grok 1.0.52).
const READ_TIER_DISALLOWED: [&str; 12] = [
    "write",
    "search_replace",
    "spawn_subagent",
    "workflow",
    "scheduler_create",
    "scheduler_delete",
    "search_tool",
    "use_tool",
    "image_gen",
    "image_edit",
    "image_to_video",
    "reference_to_video",
];

#[derive(Default)]
struct State {
    active: bool,
    /// `session/prompt` requests still waiting for their reply. A `turn_completed` update with
    /// none outstanding belongs to a turn Grok started by itself for a late interjection.
    outstanding: u32,
    /// Latest response's text since its last tool call.
    text: String,
    /// Keep the completed answer until another response actually supplies text.
    response_complete: bool,
    /// Background commands still running. When one finishes, Grok starts a turn by itself.
    background: HashSet<String>,
    /// `session/load` replays the history as ordinary updates; ignore them.
    loading: bool,
    /// Interjections accepted by RPC but not yet admitted by the session actor.
    interjections: HashSet<String>,
}

pub struct Grok {
    pub proc: Proc,
    rpc: Rpc,
    session: String,
    state: Arc<Mutex<State>>,
}

impl Grok {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Grok> {
        let mut args: Vec<String> = vec!["agent".into(), "--no-leader".into()];
        if let Some(m) = &spec.model {
            args.extend(["-m".into(), m.clone()]);
        }
        if let Some(e) = &spec.effort {
            args.extend(["--reasoning-effort".into(), e.clone()]);
        }
        args.push("stdio".into());
        let (proc, stdout) = Proc::spawn(exe, &args, spec)?;
        let (rpc, incoming) = Rpc::start(stdout, proc.stdin.clone());
        let state: Arc<Mutex<State>> = Arc::default();
        tokio::spawn(pump(incoming, rpc.clone(), state.clone(), events.clone()));

        rpc.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "axiopera", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        // yoloMode must be explicit: otherwise an always-approve setting in the user's config wins.
        let mut meta = match spec.tier {
            // Ask mode gates the shell, not Grok's own file tools: take those away, along with
            // sub-agents, schedules, media generation, and the MCP entry points. The profile is a
            // whole agent definition; without name and description Grok ignores it silently.
            Tier::Read => json!({
                "yoloMode": false,
                "autoMode": false,
                "agentProfile": {
                    "name": "axiopera-read",
                    "description": "Read-only sub-agent started by axiopera.",
                    "disallowedTools": READ_TIER_DISALLOWED,
                },
            }),
            Tier::Work => json!({"yoloMode": false, "autoMode": true}),
            Tier::Full => json!({"yoloMode": true}),
        };
        meta["rules"] = json!(protocol_text(spec));
        let mut params = json!({"cwd": spec.cwd, "mcpServers": [], "_meta": meta});
        let session = match &spec.resume {
            Some(id) => {
                params["sessionId"] = json!(id);
                state.lock().unwrap().loading = true;
                let loaded = rpc.call("session/load", params).await;
                {
                    let mut s = state.lock().unwrap();
                    s.loading = false;
                    s.text.clear();
                }
                loaded?;
                id.clone()
            }
            None => rpc.call("session/new", params).await?["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        let _ = events.send(AgentEvent::Session {
            file: Some(session_file(spec, &session)),
            id: session.clone(),
        });
        Ok(Grok {
            proc,
            rpc,
            session,
            state,
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            s.active = true;
            s.outstanding += 1;
            s.text.clear();
        }
        let params = json!({"sessionId": self.session, "prompt": [{"type": "text", "text": text}]});
        // In order, so the turn's text and its turn_completed are seen before the reply.
        self.rpc.request_in_order("session/prompt", params).await?;
        Ok(())
    }

    /// Injected after the next tool result. Without a running turn Grok starts a new one.
    pub async fn steer(&mut self, text: &str) -> Result<Steer> {
        if !self.busy() {
            return Ok(Steer::NoActiveTurn);
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.state.lock().unwrap().interjections.insert(id.clone());
        self.rpc
            .call(
                "_x.ai/interject",
                json!({"sessionId": self.session, "text": text, "interjectionId": id}),
            )
            .await?;
        Ok(Steer::Accepted)
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        self.rpc
            .notify("session/cancel", json!({"sessionId": self.session}))
            .await?;
        Ok(())
    }

    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.active || !s.background.is_empty() || !s.interjections.is_empty()
    }

    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().active
    }
}

fn session_file(spec: &StartSpec, id: &str) -> PathBuf {
    let home = spec
        .env
        .iter()
        .find(|(k, _)| k == "GROK_HOME")
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(|| super::home_dir().join(".grok"));
    let cwd: String = spec
        .cwd
        .to_string_lossy()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    home.join("sessions")
        .join(cwd)
        .join(id)
        .join("chat_history.jsonl")
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
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let option = |kind: &str| {
                    params["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|o| o["kind"] == kind)
                        .and_then(|o| o["optionId"].as_str())
                        .map(str::to_string)
                };
                // In ask mode every shell command asks, including the sub-agent's own
                // `axiopera ask`.
                let command = params["toolCall"]["rawInput"]["command"].as_str();
                if command.is_some_and(super::is_axiopera_command)
                    && let Some(allow) = option("allow_once")
                {
                    let reply = json!({"outcome": {"outcome": "selected", "optionId": allow}});
                    let _ = rpc.respond(id, reply).await;
                    continue;
                }
                let title = params["toolCall"]["title"].as_str().unwrap_or("tool call");
                let summary = match command {
                    Some(c) => format!("{title}: {}", clip(c, 200)),
                    None => clip(title, 200),
                };
                let decision = super::ask_approval(&events, summary);
                let (allow, reject) = (
                    option("allow_once"),
                    option("reject_once").unwrap_or_else(|| "reject-once".into()),
                );
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    // Only the *_once options: the always ones are written to permission.toml.
                    let reply = match (decision.await, allow) {
                        (Decision::Approve, Some(allow)) => {
                            json!({"outcome": {"outcome": "selected", "optionId": allow}})
                        }
                        // A plain reject ends Grok's turn; a followup message keeps it going.
                        (Decision::Deny(reason), _) => json!({
                            "outcome": {"outcome": "selected", "optionId": reject},
                            "_meta": {"followup_message": reason},
                        }),
                        (Decision::Approve, None) => json!({
                            "outcome": {"outcome": "selected", "optionId": reject},
                            "_meta": {"followup_message": REFUSED},
                        }),
                    };
                    let _ = rpc.respond(id, reply).await;
                });
            }
            Incoming::Request { id, method, .. } => {
                let _ = events.send(AgentEvent::Blocked(format!("{method}: not supported")));
                let _ = rpc.respond_error(id, "not supported by axiopera").await;
            }
            // Only session/prompt is sent in order: its reply ends the turn.
            Incoming::Reply(reply) => {
                let (ok, error) = match reply {
                    Ok(r) => {
                        let stop = r["stopReason"].as_str().unwrap_or("").to_string();
                        (stop == "end_turn", (stop != "end_turn").then_some(stop))
                    }
                    Err(e) => (false, Some(e.to_string())),
                };
                let mut s = state.lock().unwrap();
                s.outstanding = s.outstanding.saturating_sub(1);
                s.active = s.outstanding > 0;
                let result = std::mem::take(&mut s.text);
                let _ = events.send(AgentEvent::TurnEnded {
                    ok,
                    result,
                    error,
                    usage: None,
                });
            }
        }
    }
    state.lock().unwrap().active = false;
    let _ = events.send(AgentEvent::Exited("grok exited".into()));
}

fn on_notification(method: &str, p: &Value, state: &Mutex<State>, events: &Events) {
    // Grok's prompt queue: a running prompt we did not send is a turn it started by itself,
    // for a late interjection or a finished background command.
    if method == "_x.ai/queue/changed" {
        if p["runningPromptId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        {
            state.lock().unwrap().active = true;
        }
        return;
    }
    if method == "_x.ai/session/interjection" {
        let mut s = state.lock().unwrap();
        s.active = true;
        if let Some(id) = p["interjectionId"].as_str() {
            s.interjections.remove(id);
        }
        return;
    }
    // Standard ACP updates come as session/update; xAI's own kinds (turn_completed,
    // task_backgrounded, ...) use other methods with the same shape.
    if !p["update"]["sessionUpdate"].is_string() || state.lock().unwrap().loading {
        return;
    }
    let u = &p["update"];
    match u["sessionUpdate"].as_str().unwrap_or("") {
        "agent_message_chunk" => {
            let mut s = state.lock().unwrap();
            s.active = true;
            if s.response_complete {
                s.text.clear();
                s.response_complete = false;
            }
            s.text.push_str(u["content"]["text"].as_str().unwrap_or(""));
        }
        "response_completed" => {
            state.lock().unwrap().response_complete = true;
        }
        "tool_call" => {
            state.lock().unwrap().text.clear();
            if u["kind"] == "edit" {
                for loc in u["locations"].as_array().into_iter().flatten() {
                    if let Some(path) = loc["path"].as_str() {
                        let _ = events.send(AgentEvent::FileChanged(path.to_string()));
                    }
                }
            }
            let name = u["kind"].as_str().unwrap_or("tool").to_string();
            let _ = events.send(AgentEvent::Tool {
                name,
                detail: clip(u["title"].as_str().unwrap_or(""), 160),
            });
        }
        // Auto mode refuses some calls outright; the agent only sees the call fail.
        "tool_call_update" if u["status"] == "failed" => {
            let text: String = u["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["content"]["text"].as_str())
                .collect();
            if text.contains("Auto mode blocked this action") {
                let _ = events.send(AgentEvent::Blocked(clip(&text, 300)));
            }
        }
        "task_backgrounded" => {
            if let Some(id) = u["task_id"].as_str() {
                state.lock().unwrap().background.insert(id.to_string());
            }
        }
        "task_completed" => {
            let mut s = state.lock().unwrap();
            if let Some(id) = u["task_snapshot"]["task_id"].as_str()
                && s.background.remove(id)
            {
                s.active = true;
            }
        }
        "turn_completed" => {
            let mut s = state.lock().unwrap();
            if s.outstanding > 0 {
                return;
            }
            s.active = false;
            let result = std::mem::take(&mut s.text);
            let stop = u["stop_reason"].as_str().unwrap_or("end_turn");
            let ok = stop == "end_turn";
            let error = (!ok).then(|| stop.to_string());
            let _ = events.send(AgentEvent::TurnEnded {
                ok,
                result,
                error,
                usage: u.get("usage").cloned(),
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds an update through the method Grok uses for its kind.
    fn update(state: &Mutex<State>, events: &Events, update: Value) {
        let method = match update["sessionUpdate"].as_str() {
            Some("response_completed" | "turn_completed") => "_x.ai/session_notification",
            _ => "session/update",
        };
        on_notification(method, &json!({"update": update}), state, events);
    }

    fn mark(state: &Mutex<State>, events: &Events, kind: &str) {
        update(state, events, json!({"sessionUpdate": kind}));
    }

    fn chunk(state: &Mutex<State>, events: &Events, text: &str) {
        let content = json!({"type": "text", "text": text});
        update(
            state,
            events,
            json!({"sessionUpdate": "agent_message_chunk", "content": content}),
        );
    }

    #[test]
    fn admission_is_not_consumption_and_background_completion_keeps_session_alive() {
        let state = Mutex::new(State::default());
        let (events, mut rx) = mpsc::unbounded_channel();
        state.lock().unwrap().interjections.insert("one".into());
        on_notification(
            "_x.ai/session/interjection",
            &json!({"interjectionId": "one"}),
            &state,
            &events,
        );
        assert!(state.lock().unwrap().interjections.is_empty());
        assert!(rx.try_recv().is_err());
        update(
            &state,
            &events,
            json!({"sessionUpdate": "task_backgrounded", "task_id": "bg"}),
        );
        mark(&state, &events, "turn_completed");
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::TurnEnded { .. })));
        assert!(!state.lock().unwrap().background.is_empty());
        update(
            &state,
            &events,
            json!({"sessionUpdate": "task_completed", "task_snapshot": {"task_id": "bg"}}),
        );
        assert!(state.lock().unwrap().active);
        chunk(&state, &events, "background result");
        mark(&state, &events, "turn_completed");
        assert!(
            matches!(rx.try_recv(), Ok(AgentEvent::TurnEnded { result, .. }) if result == "background result")
        );
    }

    #[test]
    fn interjection_returns_only_last_response() {
        let state = Mutex::new(State::default());
        let (events, mut rx) = mpsc::unbounded_channel();
        chunk(&state, &events, "FIRST-");
        on_notification("_x.ai/session/interjection", &json!({}), &state, &events);
        chunk(&state, &events, "ANSWER");
        mark(&state, &events, "response_completed");
        assert_eq!(state.lock().unwrap().text, "FIRST-ANSWER");

        chunk(&state, &events, "SECOND-");
        chunk(&state, &events, "ANSWER");
        mark(&state, &events, "response_completed");
        mark(&state, &events, "turn_completed");
        match rx.try_recv().unwrap() {
            AgentEvent::TurnEnded { ok, result, .. } => {
                assert!(ok);
                assert_eq!(result, "SECOND-ANSWER");
            }
            event => panic!("unexpected event: {event:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn prompt_reply_keeps_completed_response_for_collection() {
        let state = Mutex::new(State {
            outstanding: 1,
            ..State::default()
        });
        let (events, mut rx) = mpsc::unbounded_channel();
        chunk(&state, &events, "old answer");
        mark(&state, &events, "response_completed");
        chunk(&state, &events, "final ");
        chunk(&state, &events, "answer");
        mark(&state, &events, "response_completed");
        mark(&state, &events, "turn_completed");

        assert!(rx.try_recv().is_err());
        assert_eq!(state.lock().unwrap().text, "final answer");
    }

    #[test]
    fn tool_preamble_is_not_part_of_final_answer() {
        let state = Mutex::new(State::default());
        let (events, _rx) = mpsc::unbounded_channel();
        chunk(&state, &events, "Checking files.");
        mark(&state, &events, "tool_call");
        mark(&state, &events, "response_completed");
        chunk(&state, &events, "Final ");
        chunk(&state, &events, "answer.");
        mark(&state, &events, "response_completed");

        assert_eq!(state.lock().unwrap().text, "Final answer.");
    }

    #[test]
    fn loading_ignores_response_boundaries_and_text() {
        let state = Mutex::new(State {
            loading: true,
            ..State::default()
        });
        let (events, _rx) = mpsc::unbounded_channel();
        chunk(&state, &events, "History");
        mark(&state, &events, "response_completed");

        let s = state.lock().unwrap();
        assert!(s.text.is_empty());
        assert!(!s.response_complete);
    }
}
