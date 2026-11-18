//! Antigravity CLI over ACP. Each managed task owns its ACP process and MCP server.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::rpc::{Incoming, Rpc};
use super::{AgentEvent, Decision, Events, Inputs, Proc, StartSpec, Steer};
use crate::{identity, ipc::Tier, paths};

pub(super) fn initialize() -> Value {
    json!({"protocolVersion":1,"clientCapabilities":{},
           "clientInfo":{"name":"axiopera","version":env!("CARGO_PKG_VERSION")}})
}

pub(super) fn option<'a>(reply: &'a Value, id: &str) -> Option<&'a Value> {
    reply["configOptions"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == id)
}

fn settings(reply: &Value, events: &Events) {
    if option(reply, "model").is_some() {
        let current = |id| option(reply, id).and_then(|v| v["currentValue"].as_str());
        super::report_settings(events, current("model"), current("thought_level"));
    }
}

#[derive(Default)]
struct State {
    prompts: usize,
    inputs: Inputs,
    text: String,
    // Root tools still running beyond the end of the originating prompt.
    tools: HashSet<String>,
    // A spawn is visible on the wire before its child ID is written to the transcript.
    spawns: HashMap<String, Option<Instant>>,
    children: HashSet<String>,
    waking: bool,
}

pub struct Agy {
    pub proc: Proc,
    rpc: Rpc,
    session: String,
    state: Arc<Mutex<State>>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Agy {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl Agy {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Self> {
        let home = spec
            .env
            .iter()
            .find(|(k, _)| k == paths::HOME_VAR)
            .map(|(_, v)| PathBuf::from(v))
            .context("missing Axiopera home")?;
        let mut args = vec!["acp".into()];
        match spec.tier {
            Tier::Read => {
                let profile = home.join("tasks").join(&spec.task_id).join("agy-profile");
                let name = format!("axiopera-reader-{}", spec.task_id);
                let dir = profile.join(".agents/agents");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join(format!("{name}.md")),
                    format!(
                        "---\nname: {name}\ndescription: Read-only Axiopera task\ntools: [read_file]\ninheritMcp: false\nmainAgent: true\n---\n"
                    ),
                )?;
                args.extend([
                    "--add-dir".into(),
                    spec.cwd.display().to_string(),
                    "--add-dir".into(),
                    profile.display().to_string(),
                    "--agent".into(),
                    name,
                ]);
            }
            Tier::Work => args.push("--sandbox".into()),
            Tier::Full => args.push("--dangerously-skip-permissions".into()),
        }
        let (proc, stdout) = Proc::spawn(exe, &args, spec)?;
        let (rpc, incoming) = Rpc::start(stdout, proc.stdin.clone());
        rpc.call("initialize", initialize()).await?;
        let server = json!({
            "name": "axiopera",
            "command": std::env::current_exe()?,
            "args": ["mcp", "--caller", format!("task:{}", spec.task_id)],
            "env": [
                {"name": identity::TASK_VAR, "value": spec.task_id},
                {"name": paths::HOME_VAR, "value": home},
            ],
        });
        let instructions = format!(
            "{}\nYour working directory is {}. Resolve relative task paths against it.",
            super::protocol_text(spec),
            spec.cwd.display()
        );
        let mut params = json!({
            "cwd": spec.cwd,
            "mcpServers": [server],
            "_meta": {"antigravity": {"systemPrompt": {"append": instructions}}},
        });
        let session = match &spec.resume {
            Some(id) => id.clone(),
            None => rpc.call("session/new", params.clone()).await?["sessionId"]
                .as_str()
                .context("AGY returned no session ID")?
                .to_owned(),
        };
        Uuid::parse_str(&session).context("invalid AGY session ID")?;
        params["sessionId"] = json!(session);
        // Re-install session MCP tools on custom agents before the first prompt.
        let mut reply = rpc.call("session/resume", params).await?;
        let mode = if spec.tier == Tier::Work {
            "auto"
        } else {
            "default"
        };
        for (key, value) in [
            ("model", spec.model.as_deref()),
            ("thought_level", spec.effort.as_deref()),
            ("mode", Some(mode)),
        ] {
            if let Some(value) = value {
                reply = rpc
                    .call(
                        "session/set_config_option",
                        json!({"sessionId":session,"configId":key,"value":value}),
                    )
                    .await
                    .with_context(|| format!("setting AGY {key} to {value}"))?;
            }
        }
        settings(&reply, &events);
        let brain = spec
            .env
            .iter()
            .find(|(k, _)| {
                k.eq_ignore_ascii_case(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            })
            .map(|(_, v)| PathBuf::from(v))
            .unwrap_or_else(super::home_dir)
            .join(".gemini/antigravity-cli/brain");
        let path = transcript(&brain, &session);
        let cursor = std::fs::metadata(&path).map_or(0, |f| f.len());
        let _ = events.send(AgentEvent::Session {
            id: session.clone(),
            file: Some(path),
        });
        let state = Arc::new(Mutex::new(State::default()));
        let driver = Driver {
            rpc: rpc.clone(),
            root: session.clone(),
            brain,
            cwd: spec.cwd.clone(),
            tier: spec.tier,
            events,
            state: state.clone(),
            journals: HashMap::from([(
                session.clone(),
                Journal {
                    offset: cursor,
                    ..Default::default()
                },
            )]),
            candidates: HashMap::new(),
            approvals: HashMap::new(),
            parent_woke: false,
        };
        let pump = tokio::spawn(driver.run(incoming));
        Ok(Self {
            proc,
            rpc,
            session,
            state,
            pump,
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        self.prompt(text, None).await
    }
    async fn prompt(&self, text: &str, id: Option<u64>) -> Result<()> {
        {
            let mut s = self.state.lock().unwrap();
            if s.prompts == 0 {
                s.text.clear();
            }
            s.prompts += 1;
            if id.is_some() {
                s.inputs.push(id, text);
            }
        }
        let result = self
            .rpc
            .request_in_order(
                "session/prompt",
                json!({"sessionId":self.session,"prompt":[{"type":"text","text":text}]}),
            )
            .await;
        if result.is_err() {
            let mut state = self.state.lock().unwrap();
            state.prompts = state.prompts.saturating_sub(1);
        }
        Ok(result?)
    }
    pub async fn steer(&mut self, text: &str, id: Option<u64>) -> Result<Steer> {
        if !self.in_turn() {
            return Ok(Steer::NoActiveTurn);
        }
        self.prompt(text, id).await?;
        Ok(Steer::Accepted)
    }
    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().prompts > 0
    }
    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.prompts > 0
            || !s.tools.is_empty()
            || !s.spawns.is_empty()
            || !s.children.is_empty()
            || s.waking
    }
    pub async fn interrupt(&mut self) -> Result<()> {
        self.rpc
            .notify("session/cancel", json!({"sessionId":self.session}))
            .await?;
        Ok(())
    }
}

fn transcript(brain: &Path, id: &str) -> PathBuf {
    brain
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}

#[derive(Default)]
struct Journal {
    offset: u64,
    user_inputs: Vec<String>,
    spawn_slots: HashSet<u64>,
    delivered_after_pending: HashSet<u64>,
    version: Option<(std::time::SystemTime, u64)>,
}

impl Journal {
    // AGY rewrites running steps in place. Seek past the settled prefix only; later
    // steps (including concurrent spawns) can already be complete.
    fn drain(&mut self, path: &Path) -> std::io::Result<Vec<Value>> {
        let mut file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        let version = metadata.modified().ok().map(|time| (time, metadata.len()));
        if version.is_some() && self.version == version {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = std::io::BufReader::new(file);
        let mut rows = Vec::new();
        let mut buf = Vec::new();
        let mut settled_prefix = true;
        while reader.read_until(b'\n', &mut buf)? > 0 {
            if !buf.ends_with(b"\n") {
                break;
            }
            let Ok(row) = serde_json::from_slice::<Value>(&buf) else {
                break;
            };
            let Some(index) = row["step_index"].as_u64() else {
                break;
            };
            if row["status"] != "DONE" {
                settled_prefix = false;
            } else if settled_prefix {
                self.offset += buf.len() as u64;
                if !self.delivered_after_pending.remove(&index) {
                    rows.push(row);
                }
            } else if self.delivered_after_pending.insert(index) {
                rows.push(row);
            }
            buf.clear();
        }
        self.version = version;
        Ok(rows)
    }
}

fn child_id(body: &str) -> Option<String> {
    let mut lines = body
        .lines()
        .skip_while(|s| s.starts_with("Created At: ") || s.starts_with("Completed At: "));
    if lines.next()? != "Created the following subagent:" {
        return None;
    }
    let rest = lines.collect::<Vec<_>>().join("\n");
    let data = serde_json::Deserializer::from_str(&rest)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    Some(
        Uuid::parse_str(data["conversationId"].as_str()?)
            .ok()?
            .to_string(),
    )
}

fn system_sender(row: &Value) -> Option<&str> {
    if row["type"] != "SYSTEM_MESSAGE" || row["source"] != "SYSTEM" {
        return None;
    }
    row["content"]
        .as_str()?
        .lines()
        .find(|l| l.starts_with("[Message] timestamp="))?
        .split_whitespace()
        .nth(2)?
        .strip_prefix("sender=")
}

struct Driver {
    rpc: Rpc,
    root: String,
    brain: PathBuf,
    cwd: PathBuf,
    tier: Tier,
    events: Events,
    state: Arc<Mutex<State>>,
    journals: HashMap<String, Journal>,
    candidates: HashMap<String, String>,
    approvals: HashMap<(String, String), oneshot::Sender<()>>,
    parent_woke: bool,
}

impl Drop for Driver {
    fn drop(&mut self) {
        for (_, pending) in self.approvals.drain() {
            let _ = pending.send(());
        }
    }
}

impl Driver {
    async fn run(mut self, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
        let mut tick = tokio::time::interval(Duration::from_millis(300));
        loop {
            tokio::select! {
                msg = incoming.recv() => match msg {
                    Some(msg) => self.receive(msg).await,
                    None => break,
                },
                _ = tick.tick() => self.inspect().await,
            }
        }
        let _ = self.events.send(AgentEvent::Exited("agy exited".into()));
    }

    async fn receive(&mut self, msg: Incoming) {
        match msg {
            Incoming::Reply(reply) => {
                // AGY records interjections in its transcript without echoing a user chunk.
                self.inspect().await;
                let mut s = self.state.lock().unwrap();
                s.prompts = s.prompts.saturating_sub(1);
                if s.prompts > 0 {
                    return;
                }
                let (error, usage) = match reply {
                    Ok(r) => {
                        let stop = r["stopReason"].as_str().unwrap_or("unknown");
                        (
                            (stop != "end_turn").then(|| stop.to_owned()),
                            r.get("usage").cloned(),
                        )
                    }
                    Err(e) => (Some(e.to_string()), None),
                };
                let _ = self.events.send(AgentEvent::TurnEnded {
                    ok: error.is_none(),
                    result: std::mem::take(&mut s.text),
                    error,
                    usage,
                });
            }
            Incoming::Notification { method, params } if method == "session/update" => {
                self.update(&params)
            }
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                self.permission(id, &params).await
            }
            Incoming::Request { id, .. } => {
                let _ = self
                    .rpc
                    .respond_error(id, "unsupported client operation")
                    .await;
            }
            _ => {}
        }
    }

    fn update(&mut self, p: &Value) {
        let Some(session) = p["sessionId"].as_str() else {
            return;
        };
        if !self.journals.contains_key(session) {
            return;
        }
        let root = session == self.root;
        let u = &p["update"];
        let mut s = self.state.lock().unwrap();
        match u["sessionUpdate"].as_str().unwrap_or("") {
            "config_option_update" if root => settings(u, &self.events),
            "user_message_chunk" if root => s
                .inputs
                .read(u["content"]["text"].as_str().unwrap_or(""), &self.events),
            "agent_message_chunk" if root => {
                s.text.push_str(u["content"]["text"].as_str().unwrap_or(""))
            }
            "tool_call" | "tool_call_update" => {
                let Some(id) = u["toolCallId"].as_str() else {
                    return;
                };
                let key = format!("{session}:{id}");
                let name = u["name"].as_str().unwrap_or("tool");
                let finish = matches!(u["status"].as_str(), Some("completed" | "failed"));
                if finish {
                    if let Some(gone) = self.approvals.remove(&(session.to_owned(), id.to_owned()))
                    {
                        let _ = gone.send(());
                    }
                    if root && s.tools.remove(&key) && s.prompts == 0 {
                        s.waking = true;
                    }
                    if u["status"] == "failed" {
                        s.spawns.remove(&key);
                    } else if let Some(completed) = s.spawns.get_mut(&key) {
                        *completed = Some(Instant::now());
                    }
                } else if u["sessionUpdate"] == "tool_call" {
                    if root {
                        s.tools.insert(key.clone());
                    }
                    if name == "invoke_subagent" {
                        s.spawns.entry(key.clone()).or_insert(None);
                    }
                }
                if u["sessionUpdate"] == "tool_call" {
                    if root && !s.text.is_empty() {
                        let _ = self
                            .events
                            .send(AgentEvent::Said(std::mem::take(&mut s.text)));
                    }
                    let detail = u["rawInput"]["CommandLine"]
                        .as_str()
                        .or_else(|| u["rawInput"]["Path"].as_str())
                        .or_else(|| u["title"].as_str())
                        .unwrap_or("");
                    let _ = self.events.send(AgentEvent::Tool {
                        id: Some(key),
                        name: name.to_owned(),
                        detail: super::clip(detail, 300),
                    });
                }
                if u["status"] == "completed"
                    && matches!(name, "write_file" | "edit_file")
                    && let Some(path) = u["rawInput"]["Path"].as_str()
                {
                    let _ = self.events.send(AgentEvent::FileChanged(path.to_owned()));
                }
            }
            _ => {}
        }
    }

    async fn permission(&mut self, id: Value, p: &Value) {
        let session = p["sessionId"].as_str().unwrap_or("");
        let tool = &p["toolCall"];
        let name = tool["name"].as_str().unwrap_or("");
        let title = tool["title"].as_str().unwrap_or(name);
        let choice = |kind: &str| {
            p["options"]
                .as_array()?
                .iter()
                .find(|o| o["kind"] == kind)?
                .get("optionId")
                .cloned()
        };
        let allow = choice("allow_once");
        let reject = choice("reject_once");
        let known = self.journals.contains_key(session);
        let bridge = matches!(
            name,
            "mcp_axiopera_ask" | "mcp_axiopera_tell" | "mcp_axiopera_answers"
        );
        let reply = |option: Option<Value>| match option {
            Some(o) => json!({"outcome":{"outcome":"selected","optionId":o}}),
            None => json!({"outcome":{"outcome":"cancelled"}}),
        };
        if !known || bridge || self.tier == Tier::Read || allow.is_none() {
            if !bridge && known {
                let _ = self.events.send(AgentEvent::Blocked(title.to_owned()));
            }
            let answer = if known && bridge { allow } else { reject };
            let _ = self.rpc.respond(id, reply(answer)).await;
            return;
        }
        let (tx, rx) = oneshot::channel();
        let key = (
            session.to_owned(),
            tool["toolCallId"].as_str().unwrap_or("").to_owned(),
        );
        if let Some(previous) = self.approvals.insert(key, tx) {
            let _ = previous.send(());
        }
        let summary = if session == self.root {
            title.to_owned()
        } else {
            format!("subagent {session}: {title}")
        };
        let decision = super::ask_withdrawable_approval(
            &self.events,
            super::clip(&summary, 200),
            format!("{summary}\n{}", super::arguments(&tool["rawInput"])),
            rx,
        );
        let rpc = self.rpc.clone();
        tokio::spawn(async move {
            if let Some(d) = decision.await {
                let _ = rpc
                    .respond(
                        id,
                        reply(if matches!(d, Decision::Approve) {
                            allow
                        } else {
                            reject
                        }),
                    )
                    .await;
            }
        });
    }

    async fn inspect(&mut self) {
        let mut discovered = Vec::new();
        let known_children: HashSet<_> = self
            .journals
            .keys()
            .filter(|id| *id != &self.root)
            .cloned()
            .collect();
        for (sid, journal) in &mut self.journals {
            let Ok(rows) = journal.drain(&transcript(&self.brain, sid)) else {
                continue;
            };
            for r in rows {
                let body = r["content"].as_str().unwrap_or("");
                match r["type"].as_str() {
                    Some("USER_INPUT") if sid == &self.root && r["source"] == "USER_EXPLICIT" => {
                        if let Some(text) = body.strip_prefix("<USER_REQUEST>\n").and_then(|text| {
                            text.rsplit_once("\n</USER_REQUEST>")
                                .map(|(input, _)| input)
                        }) && self
                            .state
                            .lock()
                            .unwrap()
                            .inputs
                            .0
                            .iter()
                            .any(|(_, input)| input.trim_end() == text.trim_end())
                        {
                            journal.user_inputs.push(text.to_owned());
                        }
                    }
                    Some("PLANNER_RESPONSE") if r["status"] == "DONE" => {
                        if !journal.user_inputs.is_empty() {
                            let mut state = self.state.lock().unwrap();
                            for text in journal.user_inputs.drain(..) {
                                state.inputs.read(&text, &self.events);
                            }
                        }
                        let calls = r["tool_calls"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        for (offset, call) in calls.iter().enumerate() {
                            if call["name"] == "invoke_subagent" {
                                journal
                                    .spawn_slots
                                    .insert(r["step_index"].as_u64().unwrap() + offset as u64 + 1);
                            }
                        }
                        if calls.is_empty() && !body.is_empty() {
                            let mut s = self.state.lock().unwrap();
                            if sid != &self.root {
                                s.children.remove(sid);
                            } else if self.parent_woke {
                                self.parent_woke = false;
                                s.waking = false;
                                if s.prompts == 0 {
                                    s.text.clear();
                                    let _ = self.events.send(AgentEvent::TurnEnded {
                                        ok: true,
                                        result: body.to_owned(),
                                        error: None,
                                        usage: None,
                                    });
                                }
                            }
                        }
                    }
                    Some("GENERIC")
                        if r["status"] == "DONE"
                            && journal
                                .spawn_slots
                                .remove(&r["step_index"].as_u64().unwrap()) =>
                    {
                        if let Some(id) = child_id(body) {
                            discovered.push((id, sid.clone()));
                        }
                    }
                    Some("SYSTEM_MESSAGE") if sid == &self.root && r["source"] == "SYSTEM" => {
                        let mut s = self.state.lock().unwrap();
                        let from_child =
                            system_sender(&r).is_some_and(|id| known_children.contains(id));
                        if !from_child && !s.waking && s.tools.is_empty() {
                            continue;
                        }
                        if body.contains("All your subagents and background tasks have been stopped due to server restart.") {
                            continue;
                        }
                        self.parent_woke = true;
                        s.waking = true;
                        if s.prompts == 0 {
                            let _ = self.events.send(AgentEvent::TurnStarted);
                        }
                    }
                    _ => {}
                }
            }
        }
        self.candidates.extend(discovered);
        // Authenticate the relationship against the child's own system record. A tool's
        // output alone must never attach an unrelated conversation to this task.
        for (id, parent) in self.candidates.clone() {
            let path = transcript(&self.brain, &id);
            let Some(first) = std::fs::File::open(&path)
                .ok()
                .and_then(|f| std::io::BufReader::new(f).lines().next())
                .and_then(Result::ok)
                .and_then(|l| serde_json::from_str::<Value>(&l).ok())
            else {
                continue;
            };
            self.candidates.remove(&id);
            if system_sender(&first) != Some(parent.as_str()) {
                continue;
            }
            if self.journals.contains_key(&id) {
                continue;
            }
            self.journals.insert(id.clone(), Journal::default());
            {
                let mut s = self.state.lock().unwrap();
                s.children.insert(id.clone());
                if let Some(key) = s
                    .spawns
                    .keys()
                    .find(|k| k.starts_with(&format!("{parent}:")))
                    .cloned()
                {
                    s.spawns.remove(&key);
                }
            }
            // Loading can replay approval requests before the load reply arrives.
            let rpc = self.rpc.clone();
            let events = self.events.clone();
            let params = json!({"sessionId":id,"cwd":self.cwd,"mcpServers":[]});
            tokio::spawn(async move {
                let load = rpc.call("session/load", params);
                let error = match tokio::time::timeout(Duration::from_secs(30), load).await {
                    Ok(Ok(_)) => return,
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "session/load timed out".into(),
                };
                let _ = events.send(AgentEvent::Exited(format!(
                    "loading AGY subagent {id}: {error}"
                )));
            });
        }
        // A changed native journal format must surface as a task error instead of silently
        // stranding the child on a permission request the parent cannot receive.
        if self
            .state
            .lock()
            .unwrap()
            .spawns
            .values()
            .flatten()
            .any(|t| t.elapsed() > Duration::from_secs(30))
        {
            let _ = self.events.send(AgentEvent::Exited(
                "could not locate the AGY subagent in its parent's transcript".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_tracks_completed_steps_across_rewrites_and_partial_lines() {
        let path = std::env::temp_dir().join(format!("axiopera-agy-{}", Uuid::new_v4()));
        let row = |index, status, body| {
            format!(
                "{}\n",
                json!({"step_index":index, "status":status, "content":body})
            )
        };
        let head = row(0, "DONE", "start");
        let later = row(2, "DONE", "interjection");
        std::fs::write(&path, format!("{head}{}{later}", row(1, "RUNNING", ""))).unwrap();
        let mut journal = Journal::default();
        let indices = |journal: &mut Journal| {
            journal
                .drain(&path)
                .unwrap()
                .iter()
                .map(|row| row["step_index"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(indices(&mut journal), [0, 2]);
        assert!(indices(&mut journal).is_empty());
        let done = format!("{head}{}{later}", row(1, "DONE", "longer result"));
        let tail = row(3, "DONE", "final");
        std::fs::write(&path, format!("{done}{}", tail.trim_end())).unwrap();
        assert_eq!(indices(&mut journal), [1]);
        std::fs::write(&path, format!("{done}{tail}")).unwrap();
        assert_eq!(indices(&mut journal), [3]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn child_routing_requires_native_spawn_output_and_a_system_sender() {
        let parent = "6584e8a1-e918-4a0f-88b6-47148672c31a";
        let id = "1cf609ad-ab02-4be5-af76-3bd314191878";
        let body = format!(
            "Created At: t\nCompleted At: t\nCreated the following subagent:\n{}\nMore information",
            json!({"conversationId":id})
        );
        assert_eq!(child_id(&body).as_deref(), Some(id));
        assert!(child_id(&format!("command output\n{body}")).is_none());
        assert!(child_id(&body.replace(id, "../other")).is_none());
        let mut row = json!({"type":"SYSTEM_MESSAGE","source":"SYSTEM",
            "content":format!("header\n[Message] timestamp=t sender={parent} priority=p content=hello")});
        assert_eq!(system_sender(&row), Some(parent));
        row["source"] = json!("MODEL");
        assert!(system_sender(&row).is_none());
    }
}
