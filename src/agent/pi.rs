//! Pi's JSONL RPC. Each native process owns a single writable session.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::process::ChildStdout;
use tokio::sync::oneshot;

use super::models::{Catalog, Model};
use super::{AgentEvent, Events, Inputs, Proc, StartSpec, Steer, Writer};
use crate::ipc::Tier;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

struct Client {
    writer: Writer,
    pending: Pending,
}

impl Client {
    async fn call(&self, mut command: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        command["id"] = json!(id);
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let result = async {
            self.writer.send(&command).await?;
            let reply = rx.await.context("Pi exited before its RPC response")?;
            ensure!(
                reply["success"] == true,
                "Pi {} failed: {}",
                command["type"].as_str().unwrap_or("command"),
                reply["error"].as_str().unwrap_or("unknown error")
            );
            Ok(reply["data"].clone())
        };
        let result = tokio::time::timeout(Duration::from_secs(60), result).await;
        self.pending.lock().unwrap().remove(&id);
        result.context("Pi RPC request timed out")?
    }
}

#[derive(Default)]
struct State {
    catalog_status: Option<String>,
    configured_effort: Option<String>,
    ready: bool,
    active: bool,
    inputs: Inputs,
    result: String,
    error: Option<String>,
    usage: Option<Value>,
    writes: HashMap<String, String>,
    background: HashSet<(String, String)>,
}

pub struct Pi {
    pub proc: Proc,
    client: Client,
    state: Arc<Mutex<State>>,
    events: Events,
    _extension: CatalogExtension,
}

struct CatalogExtension(PathBuf);

impl CatalogExtension {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("axiopera-pi-{}.ts", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path)?;
        let ext = Self(path);
        file.write_all(include_bytes!("pi-catalog.ts"))?;
        Ok(ext)
    }
}

impl Drop for CatalogExtension {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Pi {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Self> {
        ensure!(spec.tier == Tier::Full, "Pi supports only the full tier");
        let mut args = vec![
            "--mode".into(),
            "rpc".into(),
            "--append-system-prompt".into(),
            super::protocol_text(spec),
        ];
        if let Some(path) = &spec.resume_file {
            args.extend(["--session".into(), path.to_string_lossy().into_owned()]);
        } else if let Some(id) = &spec.resume {
            // Pi may not have flushed a JSONL file if the first request was cancelled.
            args.extend(["--session-id".into(), id.clone()]);
        }
        if let Some(effort) = &spec.effort {
            args.extend(["--thinking".into(), effort.clone()]);
        }
        let (pi, state) = Self::launch(
            exe,
            &args,
            &spec.cwd,
            &spec.env,
            spec.trace.as_deref(),
            events,
            json!({"model":spec.model, "effort":spec.effort, "resume":spec.resume.is_some()}),
        )
        .await?;
        let id = state["sessionId"]
            .as_str()
            .context("Pi returned no session ID")?;
        if let Some(expected) = &spec.resume {
            ensure!(expected == id, "Pi resumed a different session");
        }
        if let Some(effort) = &spec.effort {
            ensure!(
                state["thinkingLevel"].as_str() == Some(effort),
                "unsupported reasoning effort {effort}: Pi selected {}",
                state["thinkingLevel"]
            );
        }
        let file = state["sessionFile"]
            .as_str()
            .context("Pi did not open a persistent session")?;
        let _ = pi.events.send(AgentEvent::Session {
            id: id.into(),
            file: Some(file.into()),
        });
        let model = model_id(&state["model"]);
        ensure!(
            model.is_some(),
            "No models available in Pi; configure a provider with a model and credentials"
        );
        super::report_settings(
            &pi.events,
            model.as_deref(),
            state["thinkingLevel"].as_str(),
        );
        Ok(pi)
    }

    async fn launch(
        exe: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        trace: Option<&Path>,
        events: Events,
        mut options: Value,
    ) -> Result<(Self, Value)> {
        loop {
            let extension = CatalogExtension::create()?;
            let mut args = args.to_vec();
            args.extend([
                "--extension".into(),
                extension.0.to_string_lossy().into_owned(),
                "--axiopera-catalog".into(),
                options.to_string(),
            ]);
            if options["retry"] != true
                && let Some(model) = options["model"].as_str()
            {
                args.extend(["--model".into(), model.into()]);
            }
            let (proc, stdout) = spawn(exe, &args, cwd, env, trace)?;
            let mut pi = Self::read(proc, stdout, events.clone(), extension);
            let state = match pi.client.call(json!({"type":"get_state"})).await {
                Ok(state) => state,
                Err(error) => {
                    // An unseeded provider has no CLI model until RPC starts discovery.
                    if options["retry"] != true
                        && options["model"].as_str().is_some_and(|m| m.contains('/'))
                        && error.downcast_ref::<oneshot::error::RecvError>().is_some()
                    {
                        options["retry"] = json!(true);
                        continue;
                    }
                    // stdout can close before the stderr reader consumes the last diagnostic.
                    pi.proc.job.kill();
                    let _ =
                        tokio::time::timeout(Duration::from_secs(1), &mut pi.proc.stderr_reader)
                            .await;
                    if let Some(diagnostic) = pi
                        .proc
                        .stderr_tail()
                        .lines()
                        .rev()
                        .map(str::trim)
                        .find(|line| !line.is_empty() && super::models::selection_error(line))
                    {
                        bail!("{error:#}: {diagnostic}");
                    }
                    return Err(error);
                }
            };
            let status = pi
                .state
                .lock()
                .unwrap()
                .catalog_status
                .take()
                .context("Pi did not load the Axiopera model extension")?;
            ensure!(status == "ready", "{status}");
            pi.state.lock().unwrap().ready = true;
            return Ok((pi, state));
        }
    }

    fn read(proc: Proc, stdout: ChildStdout, events: Events, extension: CatalogExtension) -> Self {
        let state = Arc::<Mutex<State>>::default();
        let client = Client {
            writer: proc.stdin.clone(),
            pending: Pending::default(),
        };
        tokio::spawn(reader(
            stdout,
            client.writer.clone(),
            client.pending.clone(),
            state.clone(),
            events.clone(),
        ));
        Self {
            proc,
            client,
            state,
            events,
            _extension: extension,
        }
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        let reply = self
            .client
            .call(json!({"type":"prompt","message":text}))
            .await?;
        ensure!(
            reply["disposition"] == "started",
            "Pi did not start a turn: {}",
            reply["disposition"]
        );
        Ok(())
    }

    pub async fn steer(&mut self, text: &str, id: Option<u64>) -> Result<Steer> {
        {
            let mut state = self.state.lock().unwrap();
            if !state.active {
                return Ok(Steer::NoActiveTurn);
            }
            state.inputs.push(id, text);
        }
        // A bare steer only enqueues, even if the run just settled. A streaming prompt
        // steers a live run and starts one atomically if it already settled.
        let reply = self
            .client
            .call(json!({"type":"prompt","message":text,"streamingBehavior":"steer"}))
            .await?;
        match reply["disposition"].as_str() {
            Some("queued" | "started") => {}
            Some("handled") => {
                self.state.lock().unwrap().inputs.read(text, &self.events);
                let _ = self.events.send(AgentEvent::Notice(
                    "A Pi extension handled the input.".into(),
                ));
            }
            _ => bail!("unknown Pi prompt disposition: {}", reply["disposition"]),
        }
        Ok(Steer::Accepted)
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        self.client.call(json!({"type":"clear_queue"})).await?;
        self.client.call(json!({"type":"abort"})).await?;
        Ok(())
    }

    pub fn busy(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.active || !state.background.is_empty()
    }

    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().active
    }
}

fn spawn(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    trace: Option<&Path>,
) -> Result<(Proc, ChildStdout)> {
    if exe.extension().is_some_and(|ext| ext == "js") {
        let path = env
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v);
        let node = path
            .into_iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(if cfg!(windows) { "node.exe" } else { "node" }))
            .find(|p| p.is_file())
            .context("Pi's npm package requires node on PATH")?;
        let mut node_args = vec![exe.to_string_lossy().into_owned()];
        node_args.extend_from_slice(args);
        Proc::spawn_with(&node, &node_args, cwd, env, trace)
    } else {
        Proc::spawn_with(exe, args, cwd, env, trace)
    }
}

async fn reader(
    stdout: ChildStdout,
    writer: Writer,
    pending: Pending,
    state: Arc<Mutex<State>>,
    events: Events,
) {
    let mut lines = super::lines(stdout);
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if m["type"] == "response" {
            // State and catalog replies contain complete provider models, including custom headers.
            if !matches!(
                m["command"].as_str(),
                Some("get_state" | "get_available_models")
            ) {
                writer.trace.line("recv", &line);
            }
            if m["command"] == "prompt" && m["data"]["disposition"] == "started" {
                state.lock().unwrap().active = true;
            }
            if let Some(id) = m["id"].as_str()
                && let Some(tx) = pending.lock().unwrap().remove(id)
            {
                let _ = tx.send(m);
            }
            continue;
        }
        writer.trace.line("recv", &line);
        if m["type"] == "extension_ui_request" {
            match m["method"].as_str() {
                Some("setStatus") if m["statusKey"] == "axiopera-catalog" => {
                    state.lock().unwrap().catalog_status =
                        super::nonempty(m["statusText"].as_str());
                }
                Some("setStatus") if m["statusKey"] == "axiopera-configured-effort" => {
                    state.lock().unwrap().configured_effort =
                        super::nonempty(m["statusText"].as_str());
                }
                Some("select" | "confirm" | "input" | "editor") => {
                    let _ = events.send(AgentEvent::Blocked(format!(
                        "Pi extension requested interactive input: {}",
                        m["title"].as_str().unwrap_or("dialog")
                    )));
                    let _ = writer
                        .send(
                            &json!({"type":"extension_ui_response","id":m["id"],"cancelled":true}),
                        )
                        .await;
                }
                Some("notify") => {
                    if let Some(text) = m["message"].as_str() {
                        let _ = events.send(AgentEvent::Notice(text.into()));
                    }
                }
                _ => {}
            }
        } else {
            state.lock().unwrap().event(&m, &events);
        }
    }
    pending.lock().unwrap().clear();
    let mut state = state.lock().unwrap();
    state.active = false;
    if state.ready {
        let _ = events.send(AgentEvent::Exited("Pi exited".into()));
    }
}

impl State {
    fn event(&mut self, frame: &Value, events: &Events) {
        let msg = &frame["message"];
        match frame["type"].as_str() {
            Some("agent_start") => {
                self.active = true;
                let _ = events.send(AgentEvent::TurnStarted);
            }
            Some("message_update" | "tool_execution_update") => events.progress(),
            Some("message_end") if msg["role"] == "user" => {
                self.inputs
                    .read(&super::content_text(&msg["content"]), events);
            }
            Some("message_end") if msg["role"] == "custom" => self.finish_background(msg),
            Some("message_end") if msg["role"] == "assistant" => {
                self.result = super::content_text(&msg["content"]);
                if !self.result.is_empty() {
                    let _ = events.send(AgentEvent::Said(self.result.clone()));
                }
                self.error = match msg["stopReason"].as_str() {
                    Some("error" | "aborted") => Some(
                        msg["errorMessage"]
                            .as_str()
                            .unwrap_or("Pi generation failed")
                            .into(),
                    ),
                    _ => None,
                };
                if let Some(model) = msg["model"].as_str()
                    && let Some(provider) = msg["provider"].as_str()
                {
                    super::report_model(events, Some(&format!("{provider}/{model}")));
                }
                if let Some(usage) = msg.get("usage") {
                    let total = self.usage.get_or_insert_with(|| json!({}));
                    for key in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
                        if let Some(count) = usage[key].as_u64() {
                            total[key] = json!(
                                total[key]
                                    .as_u64()
                                    .unwrap_or_default()
                                    .saturating_add(count)
                            );
                        }
                    }
                }
            }
            Some("tool_execution_start") => {
                let name = frame["toolName"].as_str().unwrap_or("?");
                let args = &frame["args"];
                let id = frame["toolCallId"].as_str();
                if matches!(name, "edit" | "write")
                    && let (Some(id), Some(path)) = (id, args["path"].as_str())
                {
                    self.writes.insert(id.into(), path.into());
                }
                let detail = ["command", "path", "task", "pattern"]
                    .iter()
                    .find_map(|k| args[k].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| super::arguments(args));
                let _ = events.send(AgentEvent::Tool {
                    id: id.map(str::to_owned),
                    name: name.into(),
                    detail: super::clip(&detail, 500),
                });
            }
            Some("tool_execution_end") => {
                if frame["isError"] == false {
                    self.track_background(frame["toolName"].as_str(), &frame["result"]["details"]);
                    if frame["toolName"] == "get_subagent_result" {
                        self.consume_tintin_result(&frame["result"]["content"]);
                    }
                }
                if let Some(id) = frame["toolCallId"].as_str()
                    && let Some(path) = self.writes.remove(id)
                    && frame["isError"] == false
                {
                    let _ = events.send(AgentEvent::FileChanged(path));
                }
            }
            Some("thinking_level_changed") => {
                let _ = events.send(AgentEvent::Model {
                    model: None,
                    effort: Some(super::nonempty(frame["level"].as_str())),
                });
            }
            Some("extension_error") => {
                let _ = events.send(AgentEvent::Notice(format!(
                    "Pi extension: {}",
                    frame["error"].as_str().unwrap_or("runtime error")
                )));
            }
            Some("agent_settled") => {
                self.active = false;
                let error = self
                    .error
                    .take()
                    .or_else(|| (frame["aborted"] == true).then(|| "Pi aborted".into()));
                let _ = events.send(AgentEvent::TurnEnded {
                    ok: error.is_none(),
                    result: std::mem::take(&mut self.result),
                    error,
                    usage: self.usage.take(),
                });
                self.writes.clear();
            }
            _ => {}
        }
    }

    // These extensions launch independent Pi processes. The parent's agent_settled
    // can arrive before their completion message starts the next run.
    fn track_background(&mut self, tool: Option<&str>, details: &Value) {
        let (kind, id) = match tool {
            Some("Agent") if details["status"] == "background" => {
                ("subagent-notification", &details["agentId"])
            }
            Some("subagent") => ("subagent-notify", &details["asyncId"]),
            _ => return,
        };
        if let Some(id) = id.as_str() {
            self.background.insert((kind.into(), id.into()));
        }
    }

    fn finish_background(&mut self, msg: &Value) {
        let kind = msg["customType"].as_str().unwrap_or("");
        let mut remove = |value: &Value| {
            if let Some(id) = value.as_str() {
                self.background.remove(&(kind.into(), id.into()));
            }
        };
        match kind {
            "subagent-notification" => {
                remove(&msg["details"]["id"]);
                if let Some(others) = msg["details"]["others"].as_array() {
                    for other in others {
                        remove(&other["id"]);
                    }
                }
            }
            "subagent-notify" => {
                if let Some(runs) = msg["details"]["runs"].as_array() {
                    for run in runs {
                        remove(&run["runId"]);
                    }
                }
            }
            _ => {}
        }
    }

    // Reading a completed tintin agent cancels its pending notification. The
    // result tool reports the resolved ID and status in the first two lines.
    fn consume_tintin_result(&mut self, content: &Value) {
        let text = super::content_text(content);
        let mut lines = text.lines();
        let Some(id) = lines.next().and_then(|l| l.strip_prefix("Agent: ")) else {
            return;
        };
        let status = lines
            .next()
            .and_then(|l| l.split_once(" | Status: "))
            .and_then(|(_, tail)| tail.split_whitespace().next());
        if matches!(
            status,
            Some("completed" | "steered" | "aborted" | "stopped" | "error")
        ) {
            self.background
                .remove(&("subagent-notification".into(), id.into()));
        }
    }
}

fn model_id(model: &Value) -> Option<String> {
    if ["api", "provider", "id"]
        .iter()
        .all(|key| model[key] == "unknown")
    {
        return None;
    }
    Some(format!(
        "{}/{}",
        model["provider"].as_str()?,
        model["id"].as_str()?
    ))
}

// Mirrors Pi's getSupportedThinkingLevels: xhigh and max are opt-in, null opts any level out.
fn levels(model: &Value) -> Vec<String> {
    if model["reasoning"] != true {
        return vec!["off".into()];
    }
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .filter(|level| match model["thinkingLevelMap"].get(*level) {
            Some(value) => !value.is_null(),
            None => !matches!(*level, "xhigh" | "max"),
        })
        .map(str::to_owned)
        .collect()
}

pub(super) async fn query(
    exe: &Path,
    cwd: &Path,
    env: &[(String, String)],
    configured_model: Option<&str>,
    timeout: Duration,
) -> Result<Catalog> {
    let args = ["--mode", "rpc", "--no-session"].map(str::to_owned);
    let (events, _rx) = Events::channel();
    tokio::time::timeout(timeout, async {
        let (pi, state) = Pi::launch(
            exe,
            &args,
            cwd,
            env,
            None,
            events,
            json!({"catalog":true,"configured":configured_model}),
        )
        .await?;
        let available = pi
            .client
            .call(json!({"type":"get_available_models"}))
            .await?;
        let models = available["models"]
            .as_array()
            .context("Pi returned no model list")?
            .iter()
            .map(|model| {
                Ok(Model {
                    id: model_id(model).context("Pi model has no provider or ID")?,
                    name: super::nonempty(model["name"].as_str()),
                    description: None,
                    resolved_model: None,
                    disabled: None,
                    efforts: Some(levels(model)),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let native_model = model_id(&state["model"]);
        let native_effort = native_model
            .as_ref()
            .and_then(|_| super::nonempty(state["thinkingLevel"].as_str()));
        let configured_effort = pi.state.lock().unwrap().configured_effort.take();
        Ok(Catalog {
            models,
            native_model,
            native_effort,
            configured_effort,
            ..Default::default()
        })
    })
    .await
    .context("Pi model query timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_reports_stderr_when_the_requested_model_is_rejected() {
        let args = [
            "--ignored",
            "--exact",
            "--nocapture",
            "--",
            "agent::pi::tests::reject_model_fixture",
        ]
        .map(str::to_owned);
        let error = Pi::launch(
            &std::env::current_exe().unwrap(),
            &args,
            &std::env::current_dir().unwrap(),
            &[],
            None,
            Events::channel().0,
            json!({"model":"typo"}),
        )
        .await
        .err()
        .unwrap();
        let message = error.to_string();
        assert!(message.contains("Unknown model 'typo'"), "{error:#}");
        assert!(!message.contains("fixture-secret"), "{error:#}");
    }

    #[test]
    #[ignore = "subprocess fixture for Pi startup"]
    fn reject_model_fixture() {
        eprintln!("Authorization: Bearer fixture-secret");
        eprintln!("Unknown model 'typo'");
    }

    #[test]
    fn only_settled_finishes_a_run_and_failed_tools_do_not_mark_files() {
        let (events, mut rx) = Events::channel();
        let mut state = State::default();
        state.event(&json!({"type":"agent_start"}), &events);
        state.event(&json!({"type":"tool_execution_start","toolName":"write","toolCallId":"a","args":{"path":"file"}}), &events);
        state.event(
            &json!({"type":"tool_execution_end","toolCallId":"a","isError":true}),
            &events,
        );
        state.event(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"try again"}}), &events);
        state.event(&json!({"type":"agent_end","willRetry":true}), &events);
        assert!(state.active);
        state.event(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"answer"}]}}), &events);
        state.event(&json!({"type":"agent_settled","aborted":false}), &events);
        assert!(!state.active);
        let mut endings = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::TurnEnded { ok, result, .. } => endings.push((ok, result)),
                AgentEvent::FileChanged(_) => panic!("failed write reported as a change"),
                _ => {}
            }
        }
        assert_eq!(endings, [(true, "answer".to_owned())]);
    }

    #[test]
    fn provider_thinking_levels_apply_null_and_extended_overrides() {
        assert_eq!(levels(&json!({"reasoning":false})), ["off"]);
        assert_eq!(
            levels(&json!({"reasoning":true,"thinkingLevelMap":{"minimal":null,"max":"maximum"}})),
            ["off", "low", "medium", "high", "max"],
        );
    }

    #[test]
    fn each_subagent_completion_releases_only_its_own_run() {
        let mut state = State::default();
        state.track_background(
            Some("Agent"),
            &json!({"status":"background","agentId":"same"}),
        );
        state.track_background(Some("subagent"), &json!({"asyncId":"same"}));
        state.track_background(Some("subagent"), &json!({"asyncId":"second"}));
        state.track_background(Some("subagent"), &json!({"results":[]}));
        state.finish_background(&json!({"customType":"subagent-notify","details":{"runs":[{"runId":"same"},{"runId":"second"}]}}));
        assert_eq!(state.background.len(), 1);
        state.finish_background(
            &json!({"customType":"subagent-notification","details":{"id":"same"}}),
        );
        assert!(state.background.is_empty());
    }
}
