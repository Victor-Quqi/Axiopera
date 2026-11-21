//! OpenCode v2 over a private HTTP server. Subscribe before prompting and checkpoint the session watermark for cold resume.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::models::{Catalog, Model};
use super::{AgentEvent, Approval, Decision, Events, Proc, StartSpec, Steer};
use crate::{identity, ipc::Tier, paths};

#[derive(Debug)]
pub struct SessionTakenOver;

impl std::fmt::Display for SessionTakenOver {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("OpenCode session changed outside Axiopera")
    }
}

impl std::error::Error for SessionTakenOver {}

#[derive(Clone)]
struct Api {
    client: Client,
    url: Url,
    password: String,
    cwd: String,
}

impl Api {
    async fn start(exe: &Path, cwd: &Path, env: &[(String, String)]) -> Result<(Proc, Self)> {
        let password = Uuid::new_v4().to_string();
        let mut env = env.to_owned();
        env.retain(|(k, _)| {
            ![
                "OPENCODE_PASSWORD",
                "OPENCODE_USERNAME",
                "OPENCODE_SERVER_PASSWORD",
                "OPENCODE_SERVER_USERNAME",
            ]
            .iter()
            .any(|n| k.eq_ignore_ascii_case(n))
        });
        env.push(("OPENCODE_PASSWORD".into(), password.clone()));
        env.push(("OPENCODE_USERNAME".into(), "opencode".into()));
        let args = ["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"].map(String::from);
        let (proc, stdout) = Proc::spawn_with(exe, &args, cwd, &env, None)?;
        let mut output = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(15), output.next_line())
            .await
            .context("OpenCode startup timed out; install OpenCode v2 (@opencode/cli)")??
            .context(
                "OpenCode did not announce a private server; install OpenCode v2 (@opencode/cli)",
            )?;
        let hello: Value =
            serde_json::from_str(&line).context("invalid OpenCode server greeting")?;
        let url = Url::parse(
            hello["url"]
                .as_str()
                .context("OpenCode returned no server URL")?,
        )?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "unexpected OpenCode server address"
        );
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok((
            proc,
            Self {
                client,
                url,
                password,
                cwd: cwd.to_string_lossy().into_owned(),
            },
        ))
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let mut url = self.url.join(path)?;
        url.query_pairs_mut()
            .append_pair("location[directory]", &self.cwd);
        let mut req = self
            .client
            .request(method.clone(), url)
            .basic_auth("opencode", Some(&self.password));
        if path != "/api/event" {
            req = req.timeout(Duration::from_secs(30));
        }
        if let Some(value) = body {
            req = req.json(&value);
        }
        let res = req.send().await?;
        ensure!(
            res.status().is_success(),
            "OpenCode {method} {path} returned {}",
            res.status()
        );
        Ok(res)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let body: Value = self.request(Method::GET, path, None).await?.json().await?;
        body.get("data")
            .cloned()
            .context("OpenCode returned no data")
    }

    async fn log(&self, session: &str, after: Option<u64>) -> Result<(u64, Vec<Value>)> {
        let mut path = format!("/api/experimental/session/{session}/log?follow=false");
        if let Some(seq) = after {
            path.push_str(&format!("&after={seq}"));
        }
        let text = self.request(Method::GET, &path, None).await?.text().await?;
        let mut seq = None;
        let mut events = Vec::new();
        for line in text.lines().filter_map(|s| s.strip_prefix("data:")) {
            let event: Value = serde_json::from_str(line)?;
            if event["type"] == "log.synced" {
                seq = event["seq"].as_u64();
            } else {
                events.push(event);
            }
        }
        Ok((
            seq.context("OpenCode log ended before synchronization")?,
            events,
        ))
    }

    async fn boundary(&self, session: &str) -> Result<u64> {
        let endpoint = format!("/api/session/{session}");
        let user = format!("{endpoint}/message?order=desc&type=user&limit=1");
        let last = format!("{endpoint}/message?order=desc&limit=1");
        let inbox = format!("{endpoint}/inbox");
        let (info, user, last, inbox) = tokio::try_join!(
            self.get(&endpoint),
            self.get(&user),
            self.get(&last),
            self.get(&inbox)
        )?;
        // Titles, viewed timestamps, and unrelated sessions never change this fingerprint.
        let projection = json!([
            user,
            last[0]["id"],
            inbox,
            info["agent"],
            info["model"],
            info["location"],
            info["permissions"],
            info["revert"],
            info["time"]["busy"],
            info["time"]["idle"]
        ]);
        Ok(paths::fnv1a(&serde_json::to_vec(&projection)?))
    }

    async fn agent(&self, selected: Option<&str>) -> Result<Value> {
        for attempt in 1..=30 {
            let agents = self.get("/api/agent").await?;
            let available = agents.as_array().context("invalid OpenCode agent list")?;
            // OpenCode puts its configured default primary agent first.
            if let Some(agent) = available.iter().find(|a| {
                a["mode"] != "subagent"
                    && a["hidden"] != true
                    && selected.is_none_or(|id| a["id"] == id)
            }) {
                return Ok(agent.clone());
            }
            if attempt < 30 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        anyhow::bail!("no selectable OpenCode primary agent was registered")
    }

    async fn jobs(&self, session: &str) -> Result<Vec<Value>> {
        let jobs = self
            .get(&format!(
                "/api/experimental/job?sessionID={session}&descendants=true"
            ))
            .await?;
        serde_json::from_value(jobs).context("invalid OpenCode jobs list")
    }
}

const READ_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "axiopera_ask",
    "axiopera_tell",
    "axiopera_answers",
    "axiopera_list_directory",
];

// Native action names are ASCII. OpenCode uses '*' and '?' and ignores case on Windows.
fn matches_action(pattern: &str, action: &str) -> bool {
    let mut matched = vec![false; action.len() + 1];
    matched[0] = true;
    for ch in pattern.bytes() {
        if ch == b'*' {
            for i in 1..matched.len() {
                matched[i] |= matched[i - 1];
            }
        } else {
            for (i, a) in action.bytes().enumerate().rev() {
                matched[i + 1] = matched[i]
                    && (ch == b'?'
                        || if cfg!(windows) {
                            ch.eq_ignore_ascii_case(&a)
                        } else {
                            ch == a
                        });
            }
            matched[0] = false;
        }
    }
    matched[action.len()]
}

fn read_permissions(rules: &Value) -> Result<Value> {
    let mut result = vec![json!({"action":"*","resource":"*","effect":"deny"})];
    for name in READ_TOOLS {
        result.push(json!({"action":name,"resource":"*","effect":"allow"}));
        for r in rules
            .as_array()
            .context("invalid OpenCode agent permissions")?
        {
            if matches_action(
                r["action"].as_str().context("invalid OpenCode action")?,
                name,
            ) {
                result.push(json!({"action":name,"resource":r["resource"],"effect":r["effect"]}));
            }
        }
    }
    Ok(Value::Array(result))
}

fn model_id(model: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        model["providerID"].as_str()?,
        model["id"].as_str()?
    ))
}

async fn select_model(api: &Api, current: &Value, spec: &StartSpec) -> Result<Option<Value>> {
    if spec.model.is_none() && spec.effort.is_none() {
        return Ok(None);
    }
    let native = if current.is_null() {
        api.get("/api/model/default").await?
    } else {
        current.clone()
    };
    let id = spec
        .model
        .clone()
        .or_else(|| model_id(&native))
        .context("no default OpenCode model")?;
    let catalog = api.get("/api/model").await?;
    Ok(Some(model_ref(&catalog, &id, spec.effort.as_deref())?))
}

fn model_ref(catalog: &Value, id: &str, effort: Option<&str>) -> Result<Value> {
    let (provider, model) = id
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .context("invalid model ID: expected OpenCode provider/model")?;
    let info = catalog
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|m| m["providerID"] == provider && m["id"] == model)
        })
        .with_context(|| format!("unknown model {id} in OpenCode"))?;
    ensure!(info["enabled"] != false, "OpenCode model is disabled");
    let mut selected = json!({"id":model,"providerID":provider});
    if let Some(variant) = effort {
        ensure!(
            info["variants"]
                .as_array()
                .is_some_and(|v| v.iter().any(|x| x["id"] == variant)),
            "unknown variant {variant} for model {id}"
        );
        selected["variant"] = json!(variant);
    }
    Ok(selected)
}

fn valid_id(id: &str) -> Result<&str> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "invalid OpenCode ID"
    );
    Ok(id)
}

#[derive(Default)]
struct State {
    active: bool,
    jobs: bool,
    last_text: String,
    usage: Option<Value>,
    pending: HashMap<String, u64>,
    delivered: Vec<u64>,
}

pub struct OpenCode {
    pub proc: Proc,
    api: Api,
    session: String,
    state: Arc<Mutex<State>>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for OpenCode {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl OpenCode {
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Self> {
        ensure!(spec.tier != Tier::Work, "OpenCode supports read and full");
        let (proc, api) = Api::start(exe, &spec.cwd, &spec.env).await?;
        let home = spec
            .env
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(paths::HOME_VAR))
            .context("missing Axiopera home")?;
        let checkpoint = Path::new(&home.1)
            .join("tasks")
            .join(&spec.task_id)
            .join("opencode-seq");
        let data = if let Some(id) = &spec.resume {
            let endpoint = format!("/api/session/{}", valid_id(id)?);
            let raw = tokio::fs::read(&checkpoint)
                .await
                .context("missing OpenCode session checkpoint")?;
            let saved: Checkpoint =
                serde_json::from_slice(&raw).context("invalid OpenCode session checkpoint")?;
            ensure!(
                saved.session == *id,
                "OpenCode checkpoint belongs to another session"
            );
            let (latest, changes) = api.log(id, Some(saved.seq)).await?;
            if latest != saved.seq {
                // Stock serve retains the watermark but omits event bodies. Check the projected
                // conversation and pending inputs so incidental views and renames remain resumable.
                if changes.iter().any(external_change)
                    || saved.boundary != Some(api.boundary(id).await?)
                {
                    return Err(SessionTakenOver.into());
                }
            }
            let info = api.get(&endpoint).await?;
            ensure!(
                Path::new(
                    info["location"]["directory"]
                        .as_str()
                        .context("OpenCode session has no directory")?
                ) == spec.cwd,
                "OpenCode session belongs to another directory"
            );
            let agent = api.agent(info["agent"].as_str()).await?;
            let permissions = if spec.tier == Tier::Read {
                read_permissions(&agent["permissions"])?
            } else {
                json!([])
            };
            if spec.tier == Tier::Read && info["permissions"] != permissions {
                ensure!(
                    api.jobs(id).await?.is_empty(),
                    "OpenCode still has background jobs; finish them before changing to read"
                );
                restrict_descendants(&api, id).await?;
            }
            api.request(
                Method::PATCH,
                &endpoint,
                Some(json!({"permissions":permissions})),
            )
            .await?;
            info
        } else {
            let agent = api.agent(None).await?;
            let permissions = if spec.tier == Tier::Read {
                read_permissions(&agent["permissions"])?
            } else {
                json!([])
            };
            let body = json!({
                "location": {"directory": spec.cwd},
                "agent": agent["id"],
                "permissions": permissions,
            });
            let response: Value = api
                .request(Method::POST, "/api/session", Some(body))
                .await?
                .json()
                .await?;
            response["data"].clone()
        };
        let session =
            valid_id(data["id"].as_str().context("missing OpenCode session ID")?)?.to_owned();
        if let Some(model) = select_model(&api, &data["model"], spec).await? {
            api.request(
                Method::POST,
                &format!("/api/session/{session}/model"),
                Some(json!({"model":model})),
            )
            .await?;
        }
        let mut cmd = vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "mcp".into(),
            "--caller".into(),
            format!("task:{}", spec.task_id),
        ];
        if spec.tier == Tier::Read {
            cmd.extend(["--list-root".into(), api.cwd.clone()]);
        }
        let mcp = json!({
            "config": {
                "type": "local",
                "command": cmd,
                "environment": {
                    (identity::TASK_VAR): spec.task_id,
                    (paths::HOME_VAR): home.1,
                },
                "codemode": false,
            },
        });
        api.request(Method::PUT, "/api/experimental/mcp/axiopera", Some(mcp))
            .await?;
        api.request(
            Method::PUT,
            &format!("/api/experimental/session/{session}/instructions/entries/axiopera"),
            Some(json!({"value":super::protocol_text(spec)})),
        )
        .await?;
        let mut stream = Feed {
            response: api.request(Method::GET, "/api/event", None).await?,
            buffer: Vec::new(),
        };
        let connected = tokio::time::timeout(Duration::from_secs(10), stream.next()).await??;
        ensure!(
            connected["type"] == "server.connected",
            "OpenCode feed did not confirm subscription"
        );
        let (seq, _) = api.log(&session, None).await?;
        let saved = Checkpoint {
            session: session.clone(),
            seq,
            boundary: Some(api.boundary(&session).await?),
        };
        saved.save(&checkpoint).await?;
        let _ = events.send(AgentEvent::Session {
            id: session.clone(),
            file: None,
        });
        let state = Arc::new(Mutex::new(State::default()));
        let pump = tokio::spawn(poll(
            api.clone(),
            session.clone(),
            spec.tier,
            state.clone(),
            events,
            (checkpoint, saved, stream),
        ));
        Ok(Self {
            proc,
            api,
            session,
            state,
            pump,
        })
    }

    async fn prompt(&self, text: &str, input: Option<u64>) -> Result<()> {
        let id = format!("msg_{}", Uuid::new_v4().simple());
        if let Some(n) = input {
            self.state.lock().unwrap().pending.insert(id.clone(), n);
        }
        let result = self
            .api
            .request(
                Method::POST,
                &format!("/api/session/{}/prompt", self.session),
                Some(json!({"id":id,"text":text,"delivery":"steer","resume":true})),
            )
            .await;
        if result.is_err() {
            self.state.lock().unwrap().pending.remove(&id);
        }
        result.map(|_| ())
    }
    pub async fn turn(&mut self, text: &str) -> Result<()> {
        self.state.lock().unwrap().active = true;
        self.prompt(text, None).await
    }
    pub async fn steer(&mut self, text: &str, input: Option<u64>) -> Result<Steer> {
        if !self.in_turn() {
            return Ok(Steer::NoActiveTurn);
        }
        self.prompt(text, input).await?;
        Ok(Steer::Accepted)
    }
    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.active || s.jobs || !s.pending.is_empty()
    }
    pub fn in_turn(&self) -> bool {
        self.state.lock().unwrap().active
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        let mut sessions = HashSet::from([self.session.clone()]);
        for job in self.api.jobs(&self.session).await? {
            if let Some(id) = job["childSessionID"].as_str() {
                sessions.insert(valid_id(id)?.to_owned());
            }
            if let Some(id) = job["shellID"].as_str() {
                self.api
                    .request(
                        Method::DELETE,
                        &format!("/api/shell/{}", valid_id(id)?),
                        None,
                    )
                    .await?;
            }
        }
        for id in sessions {
            let endpoint = format!("/api/session/{id}");
            let inbox = self.api.get(&format!("{endpoint}/inbox")).await?;
            for item in inbox.as_array().context("invalid OpenCode inbox")? {
                let msg = valid_id(item["id"].as_str().context("missing OpenCode inbox ID")?)?;
                self.api
                    .request(Method::DELETE, &format!("{endpoint}/inbox/{msg}"), None)
                    .await?;
            }
            self.api
                .request(
                    Method::POST,
                    &format!("{endpoint}/interrupt?resume=false"),
                    None,
                )
                .await?;
        }
        Ok(())
    }
}

// Viewing or renaming an idle session leaves its conversation available for resumption.
fn external_change(event: &Value) -> bool {
    match event["type"].as_str() {
        Some("session.inbox.enqueued") => event["data"]["item"]["type"] == "user",
        Some(
            "session.message.content.updated"
            | "session.moved"
            | "session.deleted"
            | "session.permissions"
            | "session.agent.selected"
            | "session.model.selected",
        ) => true,
        _ => false,
    }
}

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    session: String,
    seq: u64,
    boundary: Option<u64>,
}

impl Checkpoint {
    async fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("new");
        tokio::fs::write(&tmp, serde_json::to_vec(self)?).await?;
        tokio::fs::rename(tmp, path).await?;
        Ok(())
    }
}

async fn restrict_descendants(api: &Api, session: &str) -> Result<()> {
    let mut parents = vec![session.to_owned()];
    while let Some(parent) = parents.pop() {
        let mut path = format!("/api/session?parentID={parent}");
        loop {
            let page: Value = api.request(Method::GET, &path, None).await?.json().await?;
            for child in page["data"]
                .as_array()
                .context("invalid OpenCode child session list")?
            {
                let id = valid_id(child["id"].as_str().context("missing OpenCode child ID")?)?;
                api.request(
                    Method::PATCH,
                    &format!("/api/session/{id}"),
                    Some(json!({"permissions":[{"action":"*","resource":"*","effect":"deny"}]})),
                )
                .await?;
                parents.push(id.to_owned());
            }
            let Some(cursor) = page["cursor"]["next"].as_str() else {
                break;
            };
            path = format!("/api/session?cursor={}", valid_id(cursor)?);
        }
    }
    Ok(())
}

struct PendingApproval {
    reply: oneshot::Receiver<Decision>,
    withdraw: Option<oneshot::Sender<()>>,
}

impl Drop for PendingApproval {
    fn drop(&mut self) {
        if let Some(sender) = self.withdraw.take() {
            let _ = sender.send(());
        }
    }
}

async fn service_requests(
    api: &Api,
    session: &str,
    jobs: &[Value],
    tier: Tier,
    events: &Events,
    approvals: &mut HashMap<String, PendingApproval>,
) -> Result<()> {
    let mut live = HashSet::new();
    let mut sessions = HashSet::from([session]);
    for job in jobs {
        if let Some(id) = job["childSessionID"].as_str() {
            sessions.insert(valid_id(id)?);
        }
    }
    for sid in sessions {
        let base = format!("/api/session/{sid}");
        let requests = api.get(&format!("{base}/permission")).await?;
        for req in requests
            .as_array()
            .context("invalid OpenCode permissions")?
        {
            let id = valid_id(req["id"].as_str().context("missing permission ID")?)?;
            let action = req["action"].as_str().unwrap_or("permission");
            live.insert(id.to_owned());
            let decision = if tier == Tier::Read {
                let _ = events.send(AgentEvent::Blocked(action.into()));
                Decision::Deny("OpenCode read task".into())
            } else {
                let pending = approvals.entry(id.to_owned()).or_insert_with(|| {
                    let (reply, rx) = oneshot::channel();
                    let (withdraw, withdrawn) = oneshot::channel();
                    let resources = &req["resources"];
                    let _ = events.send(AgentEvent::Approval(Approval {
                        summary: format!("OpenCode {action} {resources}"),
                        detail: req.to_string(),
                        reply,
                        withdrawn,
                    }));
                    PendingApproval {
                        reply: rx,
                        withdraw: Some(withdraw),
                    }
                });
                match pending.reply.try_recv() {
                    Ok(answer) => answer,
                    Err(oneshot::error::TryRecvError::Empty) => continue,
                    Err(oneshot::error::TryRecvError::Closed) => {
                        Decision::Deny("approval channel closed".into())
                    }
                }
            };
            let body = match decision {
                Decision::Approve => json!({"decision":"once"}),
                Decision::Deny(message) => json!({"decision":"reject","message":message}),
            };
            api.request(
                Method::POST,
                &format!("{base}/permission/{id}/reply"),
                Some(body),
            )
            .await?;
            approvals.remove(id);
        }
        for form in api
            .get(&format!("{base}/form"))
            .await?
            .as_array()
            .context("invalid OpenCode forms")?
        {
            let id = valid_id(form["id"].as_str().context("missing form ID")?)?;
            api.request(Method::DELETE, &format!("{base}/form/{id}"), None)
                .await?;
            let _ = events.send(AgentEvent::Blocked(
                form["title"].as_str().unwrap_or("interactive form").into(),
            ));
        }
    }
    approvals.retain(|id, _| live.contains(id));
    Ok(())
}

struct Feed {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

impl Feed {
    async fn next(&mut self) -> Result<Value> {
        loop {
            if let Some(i) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=i).collect();
                if let Some(data) = line.strip_prefix(b"data:") {
                    return Ok(serde_json::from_slice(data)?);
                }
                continue;
            }
            let chunk = self
                .response
                .chunk()
                .await?
                .context("OpenCode event stream closed")?;
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

async fn poll(
    api: Api,
    session: String,
    tier: Tier,
    state: Arc<Mutex<State>>,
    events: Events,
    initial: (PathBuf, Checkpoint, Feed),
) {
    let (path, mut checkpoint, mut stream) = initial;
    let mut approvals = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(300));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let run = async {
        loop {
            tokio::select! {
                update = stream.next() => {
                    let update = update?;
                    if update["data"]["sessionID"] != session && update["durable"]["aggregateID"] != session {
                        continue;
                    }
                    let kind = update["type"].as_str().unwrap_or("");
                    let ending = matches!(kind, "session.execution.succeeded" | "session.execution.interrupted" | "session.execution.failed");
                    if let Some(next) = update["durable"]["seq"].as_u64() {
                        if next <= checkpoint.seq {
                            continue;
                        }
                        if ending {
                            checkpoint.boundary = Some(api.boundary(&session).await?);
                        } else if kind == "session.execution.started" {
                            checkpoint.boundary = None;
                        }
                        checkpoint.seq = next;
                        checkpoint.save(&path).await?;
                    }
                    if ending {
                        let jobs = api.jobs(&session).await?;
                        state.lock().unwrap().jobs = !jobs.is_empty();
                    }
                    project(&mut state.lock().unwrap(), &events, &update);
                }
                _ = tick.tick() => {
                    let busy = {
                        let state = state.lock().unwrap();
                        state.active || state.jobs
                    };
                    if busy || !approvals.is_empty() {
                        let jobs = api.jobs(&session).await?;
                        state.lock().unwrap().jobs = !jobs.is_empty();
                        service_requests(&api, &session, &jobs, tier, &events, &mut approvals).await?;
                    }
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = run {
        let _ = events.send(AgentEvent::Exited(format!(
            "OpenCode session stream: {e:#}"
        )));
    }
}

fn project(s: &mut State, events: &Events, event: &Value) {
    let kind = event["type"].as_str().unwrap_or("");
    let data = &event["data"];
    let emit = |ev| {
        let _ = events.send(ev);
    };
    match kind {
        "session.execution.started" => {
            if !s.active {
                emit(AgentEvent::TurnStarted);
            }
            s.active = true;
            s.last_text.clear();
            s.usage = None;
        }
        "session.inbox.delivered" => {
            if let Some(input) = data["inboxID"].as_str().and_then(|id| s.pending.remove(id)) {
                s.delivered.push(input);
            }
        }
        "session.step.started" => {
            for input in s.delivered.drain(..) {
                emit(AgentEvent::InputRead(input));
            }
            let model = &data["model"];
            emit(AgentEvent::Model {
                model: model_id(model),
                effort: Some(model["variant"].as_str().map(str::to_owned)),
            });
            events.progress();
        }
        "session.step.ended" => {
            if !data["tokens"].is_null() {
                s.usage = Some(data["tokens"].clone());
            }
            for file in data["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                emit(AgentEvent::FileChanged(file.to_owned()));
            }
        }
        "session.text.ended" => {
            if let Some(text) = data["text"].as_str().filter(|x| !x.is_empty()) {
                s.last_text = text.to_owned();
                emit(AgentEvent::Said(text.to_owned()));
            }
        }
        "session.tool.input.started" => emit(AgentEvent::Tool {
            id: data["id"].as_str().map(str::to_owned),
            name: data["name"].as_str().unwrap_or("tool").to_owned(),
            detail: String::new(),
        }),
        "session.text.delta"
        | "session.reasoning.delta"
        | "session.tool.progress"
        | "session.tool.success"
        | "session.tool.failed" => events.progress(),
        "session.execution.succeeded"
        | "session.execution.failed"
        | "session.execution.interrupted" => {
            s.active = false;
            let ok = kind == "session.execution.succeeded";
            let error = if ok {
                None
            } else {
                Some(
                    data["error"]["message"]
                        .as_str()
                        .or_else(|| data["error"]["data"]["message"].as_str())
                        .or_else(|| data["reason"].as_str())
                        .unwrap_or("OpenCode execution failed")
                        .to_owned(),
                )
            };
            emit(AgentEvent::TurnEnded {
                ok,
                result: std::mem::take(&mut s.last_text),
                error,
                usage: s.usage.take(),
            });
        }
        _ => {}
    }
}

pub(super) fn npm_executable(modules: &Path, arch: &str) -> Option<PathBuf> {
    let mut suffixes = ["", "-baseline"];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if arch == "x64" && !std::arch::is_x86_feature_detected!("avx2") {
        suffixes.swap(0, 1);
    }
    for root in [
        modules.to_path_buf(),
        modules.join("@opencode/cli/node_modules"),
    ] {
        for suffix in suffixes {
            let exe = root.join(format!(
                "@opencode/cli-windows-{arch}{suffix}/bin/opencode.exe"
            ));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

pub(super) async fn query(
    exe: &Path,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<Catalog> {
    tokio::time::timeout(timeout, async {
        let (mut proc, api) = Api::start(exe, cwd, env).await?;
        let result = async {
            let (models, default) =
                tokio::try_join!(api.get("/api/model"), api.get("/api/model/default"))?;
            let native_model = model_id(&default);
            let models = models
                .as_array()
                .context("invalid OpenCode model list")?
                .iter()
                .map(|m| {
                    Ok(Model {
                        id: model_id(m).context("invalid OpenCode model ID")?,
                        name: m["name"].as_str().map(str::to_owned),
                        description: None,
                        efforts: m["variants"].as_array().map(|v| {
                            v.iter()
                                .filter_map(|i| i["id"].as_str().map(str::to_owned))
                                .collect()
                        }),
                        resolved_model: None,
                        disabled: m["enabled"].as_bool().map(|yes| !yes),
                    })
                })
                .collect::<Result<_>>()?;
            Ok(Catalog {
                models,
                default_model: native_model.clone(),
                native_model,
                native_effort: default["variant"].as_str().map(str::to_owned),
                ..Default::default()
            })
        }
        .await;
        proc.stdin.close().await;
        proc.job.kill();
        let _ = proc.child.wait().await;
        result
    })
    .await
    .context("OpenCode model query timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_errors_identify_the_rejected_selection() {
        let catalog = json!([{"providerID":"team","id":"nested/name","variants":[{"id":"high"}]}]);
        assert_eq!(
            model_ref(&catalog, "bad", None).unwrap_err().to_string(),
            "invalid model ID: expected OpenCode provider/model"
        );
        assert_eq!(
            model_ref(&catalog, "team/other", None)
                .unwrap_err()
                .to_string(),
            "unknown model team/other in OpenCode"
        );
        assert_eq!(
            model_ref(&catalog, "team/nested/name", Some("low"))
                .unwrap_err()
                .to_string(),
            "unknown variant low for model team/nested/name"
        );
        assert_eq!(
            model_ref(&catalog, "team/nested/name", Some("high")).unwrap(),
            json!({"providerID":"team","id":"nested/name","variant":"high"})
        );
    }

    #[test]
    fn read_rules_preserve_order_and_restrict_inherited_wildcards() {
        let rules = read_permissions(&json!([
            {"action":"*","resource":"*","effect":"allow"},
            {"action":"r?ad","resource":"*.env","effect":"deny"},
            {"action":"read","resource":"public.env","effect":"allow"},
            {"action":"axiopera_*","resource":"*","effect":"ask"},
            {"action":"bash","resource":"*","effect":"allow"}
        ]))
        .unwrap();
        let xs = rules.as_array().unwrap();
        assert_eq!(xs[0], json!({"action":"*","resource":"*","effect":"deny"}));
        assert!(
            xs[1..]
                .iter()
                .all(|r| READ_TOOLS.contains(&r["action"].as_str().unwrap()))
        );
        assert_eq!(
            xs.iter().rfind(|r| r["action"] == "read").unwrap()["resource"],
            "public.env"
        );
        assert!(
            xs.iter()
                .any(|r| r["action"] == "read" && r["effect"] == "deny")
        );
        assert!(
            xs.iter()
                .any(|r| r["action"] == "axiopera_tell" && r["effect"] == "ask")
        );
    }

    #[test]
    fn steering_receipt_requires_a_model_step() {
        let (events, mut received) = Events::channel();
        let mut state = State::default();
        state.pending.insert("msg_1".into(), 23);
        project(
            &mut state,
            &events,
            &json!({"type":"session.inbox.delivered","data":{"inboxID":"msg_1"}}),
        );
        assert!(received.try_recv().is_err());
        project(
            &mut state,
            &events,
            &json!({"type":"session.step.started","data":{"model":{"providerID":"test","id":"mock"}}}),
        );
        assert!(matches!(
            received.try_recv().unwrap(),
            AgentEvent::InputRead(23)
        ));
    }
}
