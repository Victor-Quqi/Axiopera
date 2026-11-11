//! The per-user daemon: owns every sub-agent process and all task state.

mod delivery;
mod push;
pub mod render;
mod task;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};

use crate::agent;
use crate::config;
use crate::identity;
use crate::ipc::{self, Agent, Envelope, ErrorCode, Reply, Request};
use crate::paths;
use crate::platform;
use crate::store::{self, Event, Ledger, Status, Store, Task};
use push::Reach;

pub use task::Cmd;

/// Broadcast-only signal that a task's process is gone.
const RELEASED: &str = "_released";
/// Least time between two lines of `watch`, below the rate Grok's monitor keeps up with: ten
/// events at once, then one every two seconds.
const WATCH_GAP: Duration = Duration::from_secs(2);
/// A sub-agent's watch ends once its sub-tasks have been done, and their news taken, this long.
/// The monitor running it would otherwise keep the sub-agent busy, so its process is never
/// released. Long enough for the spawn that usually follows starting the watch.
const WATCH_IDLE: Duration = Duration::from_secs(60);
const DAY_MS: u64 = 86_400_000;
/// Stopped tasks untouched this many days, and older log lines, go without being asked.
const AUTO_GC_DAYS: u64 = 30;
/// A daemon that keeps running does that again this often.
const AUTO_GC_EVERY: Duration = Duration::from_secs(86_400);

pub struct Options {
    pub home: PathBuf,
    pub idle: Duration,
    pub concurrency: usize,
    pub stall: Duration,
    /// Unanswered approval requests are denied after this long.
    pub approval_timeout: Duration,
    pub agents: config::Agents,
}

pub struct Daemon {
    opts: Options,
    store: Store,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    shutdown: Notify,
    conns: AtomicUsize,
    log: Mutex<Option<File>>,
    /// Held while appending to or trimming `daemon/requests.jsonl`.
    requests: Mutex<()>,
    previews: Mutex<delivery::Previews>,
}

struct State {
    seq: u64,
    tasks: HashMap<String, Entry>,
    ledger: Ledger,
    /// Tasks with a live actor (an agent process running or shutting down).
    running: usize,
    last_active: Instant,
    stopping: bool,
    /// Approval requests waiting for a decision, by request ID. In memory only: they die with
    /// the agent process that asked.
    approvals: HashMap<String, Pending>,
    /// One delivery at a time per caller, by `wait` or by a push, so none repeats another.
    deliveries: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// When each host's next push is due.
    pushing: HashMap<String, tokio::time::Instant>,
    /// Failed pushes in a row, per host.
    push_failures: HashMap<String, u32>,
    host_checked: HashMap<String, Instant>,
}

struct Pending {
    task: String,
    /// Caller key of whoever decides.
    approver: String,
    summary: String,
    /// The full request reached the approver through `inbox` or a push.
    delivered: bool,
    reply: oneshot::Sender<agent::Decision>,
}

struct Entry {
    task: Task,
    actor: Option<mpsc::UnboundedSender<Cmd>>,
    /// The actor is letting go of the process; new input waits in `pending`.
    releasing: bool,
    /// Caller environment for starting the agent. Kept in memory only.
    env: Option<Vec<(String, String)>>,
    /// Input for the next turn when no actor can take it right now.
    pending: Vec<String>,
    /// Notes from siblings, read at the start of its next turn; they do not start one.
    mail: Vec<String>,
    /// Events for `inbox`, in order.
    deliverable: Vec<Event>,
    approval_events: HashMap<String, Vec<usize>>,
    blocked: usize,
    /// Notices from the adapter, for the end of the turn.
    notices: Vec<String>,
    /// The running turn so far, for progress reports: when it started, how many tools it
    /// called, and the last call with its time.
    in_turn: bool,
    turn_started_ms: u64,
    tool_calls: u32,
    last_tool: Option<(String, u64, String)>,
    turn_ended_ms: u64,
}

/// One request's reply stream.
pub struct Out<'a, W> {
    w: &'a mut W,
    /// Bytes of text written so far, for the request log.
    bytes: usize,
}

impl<'a, W: AsyncWrite + Unpin> Out<'a, W> {
    fn new(w: &'a mut W) -> Self {
        Out { w, bytes: 0 }
    }

    pub async fn text(&mut self, text: impl Into<String>, data: Value) -> std::io::Result<()> {
        let text = text.into();
        self.bytes += text.len();
        ipc::write(self.w, &Reply::Out { text, data }).await
    }

    async fn done(&mut self, exit: i32) -> std::io::Result<()> {
        ipc::write(self.w, &Reply::Done { exit }).await
    }

    async fn error(&mut self, code: ErrorCode, message: impl Into<String>) -> std::io::Result<()> {
        ipc::write(
            self.w,
            &Reply::Error {
                code,
                message: message.into(),
            },
        )
        .await
    }
}

/// Fail the request with a protocol error rather than an internal one.
fn refuse<T>(code: ErrorCode, msg: impl Into<String>) -> Result<T> {
    Err(anyhow!(RefuseErr(code, msg.into())))
}

#[derive(Debug)]
struct RefuseErr(ErrorCode, String);

impl std::fmt::Display for RefuseErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for RefuseErr {}

pub async fn run(opts: Options) -> Result<()> {
    let daemon_dir = opts.home.join("daemon");
    std::fs::create_dir_all(&daemon_dir)?;
    std::fs::create_dir_all(opts.home.join("tasks"))?;
    let lock = File::create(daemon_dir.join("lock"))?;
    if lock.try_lock().is_err() {
        return Ok(()); // another daemon owns this home
    }
    let log_path = daemon_dir.join("daemon.log");
    let trimmed = trim_log(
        &log_path,
        store::now_ms().saturating_sub(AUTO_GC_DAYS * DAY_MS),
    );
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let mut listener = platform::Listener::bind(&opts.home).context("creating the endpoint")?;
    std::fs::write(
        daemon_dir.join("info.json"),
        serde_json::to_vec(&json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "started_ms": store::now_ms(),
        }))?,
    )?;

    let d = Arc::new(Daemon::load(opts, log)?);
    d.log(&format!("started, pid {}", std::process::id()));
    if let Err(e) = trimmed {
        d.log(&format!("trimming daemon.log: {e:#}"));
    }
    d.auto_gc();
    tokio::spawn(d.clone().watchdog());
    tokio::spawn(d.clone().pusher());

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let d = d.clone();
                    d.conns.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        if let Err(e) = d.serve(stream, peer).await {
                            d.log(&format!("connection error: {e:#}"));
                        }
                        d.conns.fetch_sub(1, Ordering::SeqCst);
                        d.lock().last_active = Instant::now();
                    });
                }
                Err(e) => {
                    d.log(&format!("accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = d.shutdown.notified() => break,
        }
    }
    d.stop_all().await;
    d.log("stopped");
    drop(lock);
    Ok(())
}

impl Daemon {
    fn load(opts: Options, log: Option<File>) -> Result<Daemon> {
        let store = Store::new(&opts.home);
        let mut ledger = Ledger::load(&opts.home)?;
        let mut tasks = HashMap::new();
        let mut recovered_timeouts = Vec::new();
        let mut seq = 0;
        let (loaded, mut errors) = store.recover_tasks()?;
        for mut task in loaded {
            let mut deliverable = Vec::new();
            if let Some(warning) = store
                .recover_events(&task.id, |event| {
                    seq = seq.max(event.seq);
                    if store::deliverable(&event.kind) {
                        deliverable.push(event);
                    }
                })
                .with_context(|| format!("recovering events for {}", task.id))?
            {
                errors.push(format!(
                    "recovering {}: {warning}; original event log retained",
                    task.id
                ));
            }
            if task.status == Status::Cancelled
                && deliverable
                    .iter()
                    .rev()
                    .find(|e| matches!(e.kind.as_str(), "cancelled" | "interrupted"))
                    .is_some_and(|e| {
                        e.kind == "cancelled"
                            && e.data.get("by").and_then(Value::as_str) == Some("timeout")
                    })
            {
                let previous_fingerprint = task.session_file.take();
                task.status = Status::Interrupted;
                if let Err(e) = store.save(&task) {
                    errors.push(format!("recovering timed out task {}: {e:#}", task.id));
                    task.status = Status::Cancelled;
                    task.session_file = previous_fingerprint;
                } else {
                    recovered_timeouts.push(task.id.clone());
                }
            }
            deliverable.sort_by_key(|event| event.seq);
            let entry = Entry {
                blocked: 0,
                notices: Vec::new(),
                in_turn: false,
                turn_started_ms: 0,
                tool_calls: 0,
                last_tool: None,
                turn_ended_ms: 0,
                // Persisted input becomes schedulable when a caller supplies its environment.
                pending: Vec::new(),
                mail: if task.detached {
                    Vec::new()
                } else {
                    task.pending_mail.clone()
                },
                task,
                actor: None,
                releasing: false,
                env: None,
                approval_events: delivery::approval_index(&deliverable),
                deliverable,
            };
            tasks.insert(entry.task.id.clone(), entry);
        }
        seq = seq.max(ledger.seq);
        ledger.cursors.retain(|key, cursor| {
            key.rsplit_once('|').is_some_and(|(caller, id)| {
                !caller.is_empty() && tasks.contains_key(id) && *cursor <= seq
            })
        });
        if let Err(e) = ledger.save(&opts.home) {
            errors.push(format!("saving recovered ledger: {e:#}"));
        }
        let (events, _) = broadcast::channel(1024);
        let d = Daemon {
            opts,
            store,
            state: Mutex::new(State {
                seq,
                tasks,
                ledger,
                running: 0,
                last_active: Instant::now(),
                stopping: false,
                approvals: HashMap::new(),
                deliveries: HashMap::new(),
                pushing: HashMap::new(),
                push_failures: HashMap::new(),
                host_checked: HashMap::new(),
            }),
            events,
            shutdown: Notify::new(),
            conns: AtomicUsize::new(0),
            log: Mutex::new(log),
            requests: Mutex::new(()),
            previews: Mutex::new(delivery::Previews::default()),
        };
        for error in errors {
            d.log(&error);
        }
        for id in recovered_timeouts {
            d.emit(&id, "interrupted", json!({"reason": "recovered a task cancelled by a previous timeout", "by": "timeout"}));
        }
        // Whatever was running when the last daemon died has lost its process.
        let ids: Vec<String> = {
            let st = d.lock();
            st.tasks
                .values()
                .filter(|e| matches!(e.task.status, Status::Running | Status::Queued))
                .map(|e| e.task.id.clone())
                .collect()
        };
        for id in ids {
            if let Err(e) = d.try_update(&id, |task| {
                task.status = Status::Interrupted;
                task.session_file = None;
            }) {
                d.log(&format!("recovering interrupted task {id}: {e:#}"));
            }
            d.emit(
                &id,
                "interrupted",
                json!({"reason": "the daemon stopped while this task was running"}),
            );
        }
        Ok(d)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A request that panicked should not take every later one down with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, msg: &str) {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.opts.home.join("daemon/daemon.log");
        if log.is_none() {
            *log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok();
        }
        if let Some(f) = log.as_mut() {
            let _ = crate::diagnostics::append(
                f,
                &path,
                &format!("{} {msg}\n", store::now_ms()),
                crate::diagnostics::DAEMON_BYTES,
            );
        }
    }

    fn trim_daemon_log(&self, cutoff_ms: u64) -> Result<()> {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.opts.home.join("daemon/daemon.log");
        let reopen = log.take().is_some() || path.exists();
        let result = trim_log(&path, cutoff_ms)
            .and_then(|()| trim_log(&path.with_extension("log.1"), cutoff_ms));
        if reopen {
            *log = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            );
        }
        result
    }

    /// Append an event and wake waiters. `data` must be a JSON object.
    pub fn emit(&self, task: &str, kind: &str, data: Value) {
        if let Err(e) = self.try_emit(task, kind, data) {
            self.log(&format!("writing event for {task}: {e:#}"));
        }
    }

    fn try_emit(&self, task: &str, kind: &str, data: Value) -> Result<()> {
        self.emit_with(task, kind, data, None)
    }

    /// Like `emit`, for an event that tells the task's agent was given `message`: the message
    /// is saved whole before the event is appended.
    #[cfg(test)]
    pub fn emit_message(&self, task: &str, kind: &str, data: Value, message: &str) {
        if let Err(e) = self.emit_with(task, kind, data, Some(message)) {
            self.log(&format!("writing message for {task}: {e:#}"));
        }
    }

    fn emit_with(&self, task: &str, kind: &str, data: Value, message: Option<&str>) -> Result<()> {
        let mut st = self.lock();
        st.seq += 1;
        let event = Event {
            seq: st.seq,
            ts: store::now_ms(),
            task: task.to_string(),
            kind: kind.to_string(),
            data: match data {
                Value::Object(m) => m,
                _ => Map::new(),
            },
        };
        if let Some(message) = message {
            self.store.save_message(task, event.seq, message)?;
        }
        self.store.append(&event)?;
        if let Some(entry) = st.tasks.get_mut(task) {
            entry.task.updated_ms = event.ts;
            match kind {
                "blocked" => entry.blocked += 1,
                "notice" => {
                    let text = event.data.get("text").and_then(Value::as_str);
                    entry.notices.extend(text.map(str::to_string));
                }
                "turn_started" => {
                    entry.in_turn = true;
                    entry.turn_started_ms = event.ts;
                    entry.tool_calls = 0;
                    entry.last_tool = None;
                }
                "turn_ended" => {
                    entry.in_turn = false;
                    entry.turn_ended_ms = event.ts;
                }
                "tool" | "tool_updated" => {
                    let field = |k: &str| event.data.get(k).and_then(Value::as_str).unwrap_or("");
                    if kind == "tool_updated" {
                        if let Some((call, _, id)) = &mut entry.last_tool
                            && !field("id").is_empty()
                            && id == field("id")
                        {
                            *call = match (field("name"), field("detail")) {
                                (name, "") => name.to_string(),
                                (name, detail) => format!("{name}: {detail}"),
                            };
                        }
                    } else {
                        // A turn the agent started by itself that its adapter did not report.
                        if !entry.in_turn {
                            entry.in_turn = true;
                            entry.turn_started_ms = event.ts;
                            entry.tool_calls = 0;
                        }
                        let call = match (field("name"), field("detail")) {
                            (name, "") => name.to_string(),
                            (name, detail) => format!("{name}: {detail}"),
                        };
                        entry.tool_calls += 1;
                        entry.last_tool = Some((call, event.ts, field("id").to_string()));
                    }
                }
                _ => {}
            }
            if store::deliverable(kind) {
                delivery::index_approval(
                    &mut entry.approval_events,
                    &event,
                    entry.deliverable.len(),
                );
                entry.deliverable.push(event.clone());
                if entry.deliverable.len() > 1
                    && entry.deliverable[entry.deliverable.len() - 2].seq > event.seq
                {
                    entry.deliverable.sort_by_key(|e| e.seq);
                    entry.approval_events = delivery::approval_index(&entry.deliverable);
                }
            }
        }
        let _ = self.events.send(event);
        Ok(())
    }

    /// Change a task and persist it.
    #[cfg(test)]
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Task)) {
        if let Err(e) = self.try_update(id, f) {
            self.log(&format!("saving {id}: {e:#}"));
        }
    }

    fn try_update(&self, id: &str, f: impl FnOnce(&mut Task)) -> Result<()> {
        let mut st = self.lock();
        let Some(entry) = st.tasks.get_mut(id) else {
            return Err(anyhow!("no task {id}"));
        };
        let mut task = entry.task.clone();
        f(&mut task);
        task.updated_ms = store::now_ms();
        // Saved under the lock, so concurrent updates reach the disk in order.
        self.store.save(&task)?;
        entry.task = task;
        Ok(())
    }

    #[cfg(test)]
    pub fn set_status(&self, id: &str, status: Status) {
        self.update(id, |t| t.status = status);
    }

    async fn watchdog(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        let mut last_gc = Instant::now();
        loop {
            tick.tick().await;
            if last_gc.elapsed() >= AUTO_GC_EVERY {
                last_gc = Instant::now();
                self.auto_gc();
            }
            let idle = {
                let st = self.lock();
                st.running == 0
                    && self.conns.load(Ordering::SeqCst) == 0
                    && st.last_active.elapsed() >= self.opts.idle
            };
            if idle {
                self.log("idle, exiting");
                self.shutdown.notify_one();
                return;
            }
        }
    }

    /// Kill every agent process; their tasks become interrupted and can be resumed later.
    async fn stop_all(&self) {
        let actors: Vec<_> = {
            let mut st = self.lock();
            st.stopping = true;
            st.tasks.values().filter_map(|e| e.actor.clone()).collect()
        };
        for a in &actors {
            let _ = a.send(Cmd::Abort);
        }
        for _ in 0..50 {
            if self.lock().running == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        self: &Arc<Self>,
        stream: S,
        peer: platform::Peer,
    ) -> Result<()> {
        let (r, mut w) = tokio::io::split(stream);
        let mut reader = ipc::Reader::new(r);
        let Some(mut env) = reader.read::<Envelope>().await? else {
            return Ok(());
        };
        let mut out = Out::new(&mut w);
        let compatible = env.protocol == ipc::PROTOCOL && env.version == env!("CARGO_PKG_VERSION");
        // Hooks run in every host session; one from another version must not restart us.
        if !compatible && matches!(env.request, Request::Pending) {
            out.done(ipc::exit::OK).await?;
            return Ok(());
        }
        if !compatible && !matches!(env.request, Request::Stop { .. }) {
            if self.lock().running == 0 {
                out.error(ErrorCode::Version, "daemon version differs; restarting it")
                    .await?;
                self.shutdown.notify_one();
            } else {
                let next = if identity::task_of(&env.caller).is_some() {
                    "only the main session or the user can stop it"
                } else {
                    "wait for them or run `axiopera daemon stop --force`, which interrupts active tasks from all sessions"
                };
                out.error(
                    ErrorCode::Invalid,
                    format!(
                        "the running daemon is version {} and has live tasks; {next}",
                        env!("CARGO_PKG_VERSION")
                    ),
                )
                .await?;
            }
            return Ok(());
        }
        let entry = self.request_entry(&env.caller, &env.request);
        if !env.request.allowed_in_sandbox() && peer.restricted() {
            self.record(entry, Some(ipc::exit::ERROR), Some(ErrorCode::Sandboxed), 0);
            out.error(
                ErrorCode::Sandboxed,
                "re-run this command with escalated permissions outside the agent sandbox",
            )
            .await?;
            return Ok(());
        }
        // Agents started with it would run outside the sandbox, on a PATH the sandbox chose.
        if peer.restricted() {
            env.env = None;
        }
        // Clients send nothing after the request, so a read only returns once they are gone.
        // Commands that block indefinitely stop then; the others run to completion.
        let result = if matches!(
            env.request,
            Request::Wait { .. } | Request::Watch { .. } | Request::Models { .. }
        ) {
            tokio::select! {
                r = self.dispatch(env, &mut out) => Some(r),
                _ = reader.read::<Value>() => None,
            }
        } else {
            Some(self.dispatch(env, &mut out).await)
        };
        // Answer before recording: `daemon stop` may end the process right after the answer.
        let (exit, error, sent) = match result {
            None => (None, None, Ok(())),
            Some(Ok(exit)) => (Some(exit), None, out.done(exit).await),
            Some(Err(e)) => {
                let (code, msg) = match e.downcast_ref::<RefuseErr>() {
                    Some(RefuseErr(code, msg)) => (*code, msg.clone()),
                    None => (ErrorCode::Internal, format!("{e:#}")),
                };
                (
                    Some(ipc::exit::ERROR),
                    Some(code),
                    out.error(code, msg).await,
                )
            }
        };
        self.record(entry, exit, error, out.bytes);
        Ok(sent?)
    }

    /// What the request log keeps of a request: the kind of caller, the command and the names
    /// of the arguments given, never their values. Hook polls and daemon commands are left out.
    fn request_entry(&self, caller: &str, request: &Request) -> Option<Map<String, Value>> {
        if matches!(
            request,
            Request::Pending | Request::Ping | Request::Stop { .. }
        ) {
            return None;
        }
        let Ok(Value::Object(fields)) = serde_json::to_value(request) else {
            return None;
        };
        let mut op = Value::Null;
        let mut args = Vec::new();
        for (name, value) in fields {
            match value {
                _ if name == "op" => op = value,
                Value::Null | Value::Bool(false) => {}
                Value::Array(a) if a.is_empty() => {}
                Value::String(s) if s.is_empty() => {}
                _ => args.push(name),
            }
        }
        let caller = match identity::task_of(caller) {
            Some(id) => match self.lock().tasks.get(id) {
                Some(e) => format!("{} sub-agent", json!(e.task.agent).as_str().unwrap_or("")),
                None => "sub-agent".into(),
            },
            None => caller.split(':').next().unwrap_or(caller).to_string(),
        };
        let entry = json!({"ts": store::now_ms(), "caller": caller, "op": op, "args": args});
        entry.as_object().cloned()
    }

    /// Append a request to `daemon/requests.jsonl` with how it ended: no exit code when the
    /// client left first.
    fn record(
        &self,
        entry: Option<Map<String, Value>>,
        exit: Option<i32>,
        error: Option<ErrorCode>,
        bytes: usize,
    ) {
        let Some(mut entry) = entry else {
            return;
        };
        entry.insert("exit".into(), json!(exit));
        if let Some(code) = error {
            entry.insert("error".into(), json!(code));
        }
        entry.insert("bytes".into(), json!(bytes));
        let _held = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.opts.home.join("daemon").join("requests.jsonl");
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{}", Value::Object(entry)));
        if let Err(e) = written {
            self.log(&format!("writing {}: {e}", path.display()));
        }
    }

    /// Drop the request log's lines from before `cutoff_ms`.
    fn trim_requests(&self, cutoff_ms: u64) -> Result<()> {
        let _held = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.opts.home.join("daemon").join("requests.jsonl");
        crate::diagnostics::retain_lines(&path, |line| {
            serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|v| v["ts"].as_u64())
                .is_some_and(|ts| ts >= cutoff_ms)
        })?;
        Ok(())
    }

    async fn dispatch<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        env: Envelope,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let caller = self.resolve_caller(&env.caller);
        match env.request {
            Request::Models { agent, cwd } => {
                let catalog = self
                    .model_catalog(
                        agent,
                        &cwd,
                        &env.env.unwrap_or_default(),
                        Duration::from_secs(20),
                    )
                    .await?;
                out.text(catalog.render(agent), serde_json::to_value(&catalog)?)
                    .await?;
                Ok(ipc::exit::OK)
            }
            Request::Ping => {
                let running = self.lock().running;
                let text = format!(
                    "daemon {} pid {}, {running} running",
                    env!("CARGO_PKG_VERSION"),
                    std::process::id()
                );
                out.text(
                    text,
                    json!({"pid": std::process::id(), "version": env!("CARGO_PKG_VERSION")}),
                )
                .await?;
                Ok(ipc::exit::OK)
            }
            Request::Stop { force } => {
                if identity::task_of(&caller).is_some() {
                    return refuse(
                        ErrorCode::Invalid,
                        "daemon stop is for the main session or the user, not sub-agents",
                    );
                }
                let running = self.lock().running;
                if running > 0 && !force {
                    return refuse(
                        ErrorCode::Invalid,
                        format!("{running} task(s) running; use --force to interrupt them"),
                    );
                }
                out.text("stopping", Value::Null).await?;
                self.shutdown.notify_one();
                Ok(ipc::exit::OK)
            }
            Request::Spawn(spawn) => {
                self.spawn(&caller, spawn, env.env.unwrap_or_default(), out)
                    .await
            }
            Request::Send {
                task,
                message,
                tier,
            } => self.send(&caller, &task, message, tier, env.env, out).await,
            Request::Wait { tasks, timeout_ms } => {
                self.wait(&caller, &tasks, timeout_ms, out).await
            }
            Request::Inbox { tasks } => self.inbox(&caller, &tasks, out).await,
            Request::Pending => {
                if let Some((text, seq)) = self.reminder(&caller) {
                    out.text(text, json!({"seq": seq})).await?;
                }
                Ok(ipc::exit::OK)
            }
            Request::Watch { status: true } => self.watch_status(&caller, out).await,
            Request::Watch { status: false } => self.watch(&caller, out).await,
            Request::Status { task, all } => self.status(&caller, task.as_deref(), all, out).await,
            Request::Log { task, tail } => self.log_cmd(&caller, &task, tail, out).await,
            Request::Result { task, turn, force } => {
                self.result(&caller, &task, turn, force, out).await
            }
            Request::Ask { text } => self.ask(&caller, text, out).await,
            Request::Reply { id, text } => self.reply(&caller, &id, text, env.env, out).await,
            Request::Answers => self.answers(&caller, out).await,
            Request::Gc { days } => self.gc(&caller, days, out).await,
            Request::Tell { to, text } => self.tell(&caller, &to, text, out).await,
            Request::Detach { task } => self.detach(&caller, &task, out).await,
            Request::Approve { id } => {
                let task = self.resolve(&id, Some(&caller), agent::Decision::Approve)?;
                out.text(format!("approved {id}"), json!({"id": id, "task": task}))
                    .await?;
                Ok(ipc::exit::OK)
            }
            Request::Deny { id, reason } => {
                let reason = match reason {
                    Some(r) => format!("Denied: {r}"),
                    None => "Denied.".into(),
                };
                let task = self.resolve(&id, Some(&caller), agent::Decision::Deny(reason))?;
                out.text(format!("denied {id}"), json!({"id": id, "task": task}))
                    .await?;
                Ok(ipc::exit::OK)
            }
            Request::Cancel { task } => {
                let id = self.find(&caller, &task)?;
                let mut rx = self.events.subscribe();
                let n = self.try_cancel_tree(&id, "cancelled by request", &caller)?;
                // Interrupting takes a few seconds; answer once the processes are gone, so a
                // status right after shows the outcome.
                let settled = async {
                    while !self.subtree_stopped(&id) {
                        if let Err(broadcast::error::RecvError::Closed) = rx.recv().await {
                            break;
                        }
                    }
                };
                let done = tokio::time::timeout(Duration::from_secs(10), settled)
                    .await
                    .is_ok();
                let verb = if done { "cancelled" } else { "cancelling" };
                out.text(
                    format!("{verb} {id} ({n} task(s))"),
                    json!({"task": id, "count": n, "done": done}),
                )
                .await?;
                Ok(ipc::exit::OK)
            }
        }
    }

    /// Preserve the task's original identity for its own native session and MCP server.
    /// Other native sessions under that task get separate inboxes with the same ancestry.
    fn resolve_caller(&self, caller: &str) -> String {
        if let Some((id, native)) = identity::task_session(caller)
            && let Some((host, session)) = native.split_once(':')
            && let Some(entry) = self.lock().tasks.get(id)
            && entry.task.session.as_deref() == Some(session)
            && matches!(
                (entry.task.agent, host),
                (Agent::Claude, "claude") | (Agent::Codex, "codex") | (Agent::Grok, "grok")
            )
        {
            return format!("task:{id}");
        }
        caller.to_string()
    }

    /// Resolve a task reference: an ID, or a name among the caller's children.
    fn find(&self, caller: &str, reference: &str) -> Result<String> {
        let st = self.lock();
        if st.tasks.contains_key(reference) {
            return Ok(reference.to_string());
        }
        let named: Vec<_> = st
            .tasks
            .values()
            .filter(|e| e.task.parent == caller && e.task.name.as_deref() == Some(reference))
            .map(|e| e.task.id.clone())
            .collect();
        match named.as_slice() {
            [id] => Ok(id.clone()),
            [] => refuse(ErrorCode::NotFound, format!("no task {reference}")),
            _ => refuse(
                ErrorCode::Invalid,
                format!("task name {reference} is ambiguous"),
            ),
        }
    }

    fn depth(&self, caller: &str) -> usize {
        let st = self.lock();
        let mut depth = 0;
        let mut cur = caller.to_string();
        while let Some(id) = identity::task_of(&cur) {
            depth += 1;
            match st.tasks.get(id) {
                Some(e) => cur = e.task.parent.clone(),
                None => break,
            }
        }
        depth
    }

    async fn model_catalog(
        &self,
        agent: Agent,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<agent::models::Catalog> {
        let config = self.opts.agents.get(agent);
        let exe = agent::resolve_exe(agent, config.path.as_deref(), env)?;
        let mut query_env = agent_env(env, "", agent, &self.opts.home);
        query_env.retain(|(k, _)| !k.eq_ignore_ascii_case(identity::TASK_VAR));
        let mut catalog = agent::models::query(agent, &exe, cwd, &query_env, timeout).await?;
        catalog.configured_model = config.model.clone();
        Ok(catalog)
    }

    async fn spawn<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        s: ipc::Spawn,
        env: Vec<(String, String)>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        if !s.cwd.is_dir() {
            return refuse(
                ErrorCode::Invalid,
                format!("{} is not a directory", s.cwd.display()),
            );
        }
        if let Some(why) = self.at_limit(&self.lock(), caller) {
            return refuse(ErrorCode::Invalid, why);
        }
        if self.depth(caller) >= 2 {
            return refuse(
                ErrorCode::Invalid,
                "tasks two levels deep cannot spawn further tasks",
            );
        }
        if let Some(parent) = identity::task_of(caller) {
            let parent_tier = self.lock().tasks.get(parent).map(|e| e.task.tier);
            if parent_tier.is_some_and(|t| s.tier > t) {
                return refuse(
                    ErrorCode::Invalid,
                    format!(
                        "a {} task cannot spawn a {} task",
                        render::tier_name(parent_tier.unwrap()),
                        render::tier_name(s.tier)
                    ),
                );
            }
        }
        if let Some(name) = &s.name {
            let taken = self
                .lock()
                .tasks
                .values()
                .any(|e| e.task.parent == caller && e.task.name.as_ref() == Some(name));
            if taken {
                return refuse(
                    ErrorCode::Invalid,
                    format!("you already have a task named {name}"),
                );
            }
        }
        agent::resolve_exe(s.agent, self.opts.agents.get(s.agent).path.as_deref(), &env)
            .map_err(|e| anyhow!(RefuseErr(ErrorCode::Invalid, format!("{e:#}"))))?;
        let (reach, unreachable) = self.register_host(caller, &env).await;

        let now = store::now_ms();
        let id = store::new_id('t', |id| self.store.exists(id));
        let cwd = s.cwd.clone();
        let task = Task {
            id: id.clone(),
            name: s.name.clone(),
            parent: caller.to_string(),
            agent: s.agent,
            tier: s.tier,
            cwd: cwd.clone(),
            model: s
                .model
                .clone()
                .or_else(|| self.opts.agents.get(s.agent).model.clone()),
            effort: s.effort.clone(),
            actual_model: None,
            actual_effort: None,
            timeout_secs: s.timeout_secs,
            status: Status::Queued,
            session: None,
            session_file: None,
            turns: 0,
            delivered: 0,
            changed_files: Vec::new(),
            questions: Vec::new(),
            pending_inputs: vec![s.task.clone()],
            pending_mail: Vec::new(),
            stalled: false,
            detached: false,
            created_ms: now,
            updated_ms: now,
        };
        self.store.save_prompt(&id, &s.task)?;
        self.store.save(&task)?;
        {
            let mut st = self.lock();
            // Again under the lock that adds the task, so spawns at once cannot pass it together.
            if let Some(why) = self.at_limit(&st, caller) {
                drop(st);
                let _ = self.store.remove(&id);
                return refuse(ErrorCode::Invalid, why);
            }
            // The parent may have changed tier while host registration was awaiting I/O.
            if let Some(parent) = identity::task_of(caller)
                && st.tasks.get(parent).is_none_or(|p| s.tier > p.task.tier)
            {
                drop(st);
                let _ = self.store.remove(&id);
                return refuse(
                    ErrorCode::Invalid,
                    "the requested tier exceeds the parent task's tier",
                );
            }
            let entry = Entry {
                blocked: 0,
                notices: Vec::new(),
                in_turn: false,
                turn_started_ms: 0,
                tool_calls: 0,
                last_tool: None,
                turn_ended_ms: 0,
                task,
                actor: None,
                releasing: false,
                env: Some(env),
                pending: vec![s.task.clone()],
                mail: Vec::new(),
                deliverable: Vec::new(),
                approval_events: HashMap::new(),
            };
            st.tasks.insert(id.clone(), entry);
        }
        self.try_emit(
            &id,
            "spawned",
            json!({"agent": s.agent, "tier": s.tier, "cwd": cwd}),
        )?;
        self.schedule(&id);

        let mut notes = render::capability_notes(s.agent, s.tier);
        if s.tier != ipc::Tier::Read {
            let approver = approver(&self.lock(), &id);
            if approver != caller {
                notes.push(format!("approval requests go to {approver}"));
            }
        }
        let host = identity::task_session(caller).map_or(caller, |(_, native)| native);
        let grok_host = host.starts_with("grok:")
            || host.strip_prefix("task:").is_some_and(|id| {
                self.lock()
                    .tasks
                    .get(id)
                    .is_some_and(|e| e.task.agent == Agent::Grok)
            });
        let take = if grok_host {
            "Take results and questions with `axiopera inbox` when your watch monitor reports news."
        } else {
            "Take results and questions with `axiopera inbox`; `axiopera wait` blocks until there are some."
        };
        let next = match (reach, unreachable) {
            (Reach::Push, _) => {
                "Results and questions arrive as messages: do not wait or poll; end your turn if nothing else is left.".to_string()
            }
            (Reach::Doorbell, _) => {
                "After your turn ends, a message tells you when results or questions are ready for `axiopera inbox`: do not poll; end your turn if nothing else is left.".to_string()
            }
            (Reach::Wait, Some(why)) => {
                format!("Axiopera cannot send this Codex session messages: {why}. {take}")
            }
            (Reach::Wait, None) => take.to_string(),
        };
        let state = if self.lock().tasks[&id].actor.is_some() {
            "running"
        } else {
            "queued"
        };
        let mut text = format!("{id}: {state}. {next}");
        for note in &notes {
            text.push_str(&format!("\n{note}"));
        }
        out.text(
            text,
            json!({"task": id, "agent": s.agent, "tier": s.tier, "notes": notes, "reach": reach, "status": state}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    /// Start an actor for the task.
    fn schedule(self: &Arc<Self>, id: &str) {
        let mut st = self.lock();
        if st.stopping {
            return;
        }
        let Some(entry) = st.tasks.get(id) else {
            return;
        };
        if entry.actor.is_some()
            || entry.task.status.is_stopped()
            || entry.task.detached
            || entry.env.is_none()
        {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        st.running += 1;
        st.tasks.get_mut(id).unwrap().actor = Some(tx);
        drop(st);
        tokio::spawn(task::run(self.clone(), id.to_string(), rx));
    }

    /// Called by an actor when its process is gone.
    fn actor_done(self: &Arc<Self>, id: &str) {
        let again = {
            let mut st = self.lock();
            st.running -= 1;
            st.last_active = Instant::now();
            // Whoever asked is gone; its requests can no longer be acted on.
            st.approvals.retain(|_, p| p.task != id);
            st.tasks.get_mut(id).is_some_and(|e| {
                e.actor = None;
                e.releasing = false;
                !e.pending.is_empty() && !e.task.status.is_stopped()
            })
        };
        // Not an event of its own: lets `wait` notice the task went idle.
        let _ = self.events.send(Event {
            seq: 0,
            ts: store::now_ms(),
            task: id.to_string(),
            kind: RELEASED.into(),
            data: Map::new(),
        });
        if again {
            self.schedule(id);
        }
    }

    /// Why `caller` may not start another task now: `concurrency` caps the top-level tasks
    /// running at once, across sessions. It guards against runaway spawning, so going over
    /// it is refused rather than queued. Nested tasks share their parent's place, which it
    /// holds while it waits for them; new turns of existing tasks are not limited.
    fn at_limit(&self, st: &State, caller: &str) -> Option<String> {
        if identity::task_of(caller).is_some() {
            return None;
        }
        let running = st
            .tasks
            .values()
            .filter(|e| identity::task_of(&e.task.parent).is_none())
            .filter(|e| e.actor.is_some() || !e.pending.is_empty())
            .count();
        (running >= self.opts.concurrency).then(|| {
            format!(
                "{running} top-level tasks, yours and other sessions', are running; the global concurrency limit is {} (configured in {}). Wait for one to finish",
                self.opts.concurrency,
                self.opts.home.join("config.toml").display()
            )
        })
    }

    async fn send<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        reference: &str,
        message: String,
        tier: Option<ipc::Tier>,
        env: Option<Vec<(String, String)>>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let how = self.input(caller, &id, message, tier, env).await?;
        let mut text = format!("{id}: {how}");
        let mut data = json!({"task": id, "result": how});
        if let Some(tier) = tier {
            text.push_str(&format!("; tier {}", render::tier_name(tier)));
            data["tier"] = json!(tier);
        }
        out.text(text, data).await?;
        Ok(ipc::exit::OK)
    }

    /// Hand a message to a task: into its running turn, or as the input of a new one.
    async fn input(
        self: &Arc<Self>,
        caller: &str,
        id: &str,
        message: String,
        tier: Option<ipc::Tier>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<&'static str> {
        self.input_with_answer(caller, id, message, tier, env, None)
            .await
    }

    async fn input_with_answer(
        self: &Arc<Self>,
        caller: &str,
        id: &str,
        message: String,
        tier: Option<ipc::Tier>,
        env: Option<Vec<(String, String)>>,
        answer: Option<(&str, &str)>,
    ) -> Result<&'static str> {
        let (reply, receive) = oneshot::channel();
        let mut input = task::Input {
            text: message,
            reply,
        };
        let handed = 'handoff: {
            let mut st = self.lock();
            let Some(entry) = st.tasks.get(id) else {
                return refuse(ErrorCode::NotFound, format!("no task {id}"));
            };
            if entry.task.detached {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{id} was taken over outside axiopera"),
                );
            }
            if let Some(tier) = tier.filter(|tier| *tier != entry.task.tier) {
                if identity::task_of(caller).is_some() && entry.task.parent != caller {
                    return refuse(ErrorCode::Invalid, format!("{id} is not your sub-task"));
                }
                if entry.actor.is_some()
                    || !entry.pending.is_empty()
                    || !entry.task.pending_inputs.is_empty()
                {
                    return refuse(
                        ErrorCode::Invalid,
                        format!(
                            "{id} is still running or has queued input; wait until it is idle before changing --tier"
                        ),
                    );
                }
                if let Some(parent) = identity::task_of(&entry.task.parent)
                    && st.tasks.get(parent).is_none_or(|p| tier > p.task.tier)
                {
                    return refuse(
                        ErrorCode::Invalid,
                        format!(
                            "{id} cannot use {}: it exceeds its parent task's tier",
                            render::tier_name(tier)
                        ),
                    );
                }
                if st.tasks.values().any(|e| {
                    identity::task_of(&e.task.parent) == Some(id)
                        && !e.task.detached
                        && e.task.tier > tier
                }) {
                    return refuse(
                        ErrorCode::Invalid,
                        format!(
                            "{id} has sub-tasks above {}; lower their tiers first",
                            render::tier_name(tier)
                        ),
                    );
                }
            }
            let entry = st.tasks.get_mut(id).unwrap();
            let mut task = entry.task.clone();
            if let Some((qid, answer)) = answer {
                if identity::task_of(caller).is_some() && task.parent != caller {
                    return refuse(ErrorCode::Invalid, format!("{id} is not your sub-task"));
                }
                let Some(question) = task.questions.iter_mut().find(|q| q.id == qid) else {
                    return refuse(ErrorCode::NotFound, format!("no question {qid}"));
                };
                if question.answer.is_some() {
                    return refuse(ErrorCode::Invalid, format!("{qid} is already answered"));
                }
                question.answer = Some(answer.to_string());
            }
            if let Some(tier) = tier {
                task.tier = tier;
            }
            // Explicit input waits for the stopped actor to release its process.
            if (entry.actor.is_none() || entry.releasing) && task.status.is_stopped() {
                task.status = Status::Queued;
            }
            // Claim the answer and persist accepted input in the same snapshot.
            task.pending_inputs.push(input.text.clone());
            task.updated_ms = store::now_ms();
            self.store.save(&task)?;
            if entry.actor.is_none() || (entry.releasing && entry.task.status.is_stopped()) {
                entry.pending = entry.task.pending_inputs.clone();
            }
            entry.task = task;
            if entry.env.is_none() {
                entry.env = env;
            }
            if let (Some(actor), false) = (&entry.actor, entry.releasing) {
                match actor.send(Cmd::Input(input)) {
                    Ok(()) => break 'handoff true,
                    Err(error) => {
                        let Cmd::Input(returned) = error.0 else {
                            unreachable!()
                        };
                        input = returned;
                    }
                }
            }
            entry.pending.push(input.text);
            false
        };
        if handed {
            return receive
                .await
                .context("task stopped before confirming message submission");
        }
        self.schedule(id);
        Ok("queued for a new turn")
    }

    async fn ask<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        text: String,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let Some(task) = identity::task_of(caller).map(str::to_string) else {
            return refuse(
                ErrorCode::Invalid,
                "only sub-agents started by axiopera can ask",
            );
        };
        let qid = {
            let st = self.lock();
            store::new_id('q', |id| {
                st.tasks
                    .values()
                    .any(|e| e.task.questions.iter().any(|q| q.id == id))
            })
        };
        self.try_update(&task, |t| {
            t.questions.push(store::Question {
                id: qid.clone(),
                text: text.clone(),
                answer: None,
                asked_ms: store::now_ms(),
            })
        })?;
        let mut data = json!({"id": qid, "text": text});
        self.keep_whole(&mut data, &task, "questions", &qid, &text)?;
        self.try_emit(&task, "question", data)?;
        let mut receipt = format!("asked {qid}.");
        if let Some((_, native)) = identity::task_session(caller) {
            let answers = if native.starts_with("codex:") {
                "the `answers` tool"
            } else {
                "`axiopera answers`"
            };
            receipt.push_str(&format!(
                " The answer goes to this task's main session; {answers} lists it once given. \
                 Keep working on what does not depend on it."
            ));
        }
        out.text(receipt, json!({"id": qid})).await?;
        Ok(ipc::exit::OK)
    }

    async fn reply<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        qid: &str,
        answer: String,
        env: Option<Vec<(String, String)>>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let (task, question) = {
            let st = self.lock();
            let Some(entry) = st
                .tasks
                .values()
                .find(|e| e.task.questions.iter().any(|q| q.id == qid))
            else {
                return refuse(ErrorCode::NotFound, format!("no question {qid}"));
            };
            let task = &entry.task;
            // Tasks answer only their own children; main sessions and people stand for the user.
            if identity::task_of(caller).is_some() && task.parent != caller {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{qid} was asked by {}, which is not your sub-task", task.id),
                );
            }
            let q = task.questions.iter().find(|q| q.id == qid).unwrap();
            if q.answer.is_some() {
                return refuse(ErrorCode::Invalid, format!("{qid} is already answered"));
            }
            (task.id.clone(), q.text.clone())
        };
        let message = format!(
            "[axiopera] Answer to question {qid} (\"{}\"):\n{answer}",
            agent::clip(&question, 300)
        );
        let how = self
            .input_with_answer(caller, &task, message, None, env, Some((qid, &answer)))
            .await?;
        self.try_emit(&task, "answered", json!({"id": qid}))?;
        out.text(
            format!("{qid}: {how}"),
            json!({"id": qid, "task": task, "result": how}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    async fn answers<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let Some(task) = identity::task_of(caller) else {
            return refuse(
                ErrorCode::Invalid,
                "only sub-agents started by axiopera ask questions",
            );
        };
        let questions = self
            .lock()
            .tasks
            .get(task)
            .map(|e| e.task.questions.clone())
            .unwrap_or_default();
        let text = if questions.is_empty() {
            "no questions asked yet".to_string()
        } else {
            questions
                .iter()
                .map(|q| match &q.answer {
                    Some(a) => format!("{} (answered): {}\n  answer: {a}", q.id, q.text),
                    None => format!("{} (open): {}", q.id, q.text),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        out.text(text, serde_json::to_value(&questions)?).await?;
        Ok(ipc::exit::OK)
    }

    /// A note to a sibling: into its running turn, or kept for its next one. The parent gets a
    /// copy as an event.
    async fn tell<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        to: &str,
        text: String,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let Some(me) = identity::task_of(caller) else {
            return refuse(
                ErrorCode::Invalid,
                "only sub-agents started by axiopera have siblings",
            );
        };
        let parent = self
            .lock()
            .tasks
            .get(me)
            .map(|e| e.task.parent.clone())
            .unwrap_or_default();
        let to = self.find(&parent, to)?;
        let message =
            format!("[axiopera] Note from sibling task {me} (not an instruction):\n{text}");
        let how = {
            let mut st = self.lock();
            let Some(entry) = st.tasks.get_mut(&to) else {
                return refuse(ErrorCode::NotFound, format!("no task {to}"));
            };
            if entry.task.parent != parent || to == me {
                return refuse(ErrorCode::Invalid, format!("{to} is not your sibling"));
            }
            if entry.task.detached {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{to} is no longer driven by axiopera"),
                );
            }
            let mut task = entry.task.clone();
            task.pending_mail.push(message.clone());
            task.updated_ms = store::now_ms();
            self.store.save(&task)?;
            entry.task = task;
            if let (Some(actor), false) = (&entry.actor, entry.releasing) {
                match actor.send(Cmd::Mail(message)) {
                    Ok(()) => "sent",
                    Err(e) => {
                        if let Cmd::Mail(message) = e.0 {
                            entry.mail.push(message);
                        }
                        "kept for its next turn"
                    }
                }
            } else {
                entry.mail.push(message);
                "kept for its next turn"
            }
        };
        let mut data = json!({"to": to, "text": text});
        let name = store::new_id('n', |n| self.store.text_path(me, "notes", n).exists());
        self.keep_whole(&mut data, me, "notes", &name, &text)?;
        self.try_emit(me, "message", data)?;
        out.text(format!("{to}: {how}"), json!({"task": to, "result": how}))
            .await?;
        Ok(ipc::exit::OK)
    }

    /// Stop driving a task and print how to continue it natively.
    async fn detach<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        reference: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let task = {
            let mut st = self.lock();
            let Some(entry) = st.tasks.get_mut(&id) else {
                return refuse(ErrorCode::NotFound, format!("no task {id}"));
            };
            if entry.actor.is_some() {
                return refuse(
                    ErrorCode::Invalid,
                    format!(
                        "{id} is running; wait for its execution and background work to end, or cancel it first"
                    ),
                );
            }
            let Some(session) = &entry.task.session else {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{id} has no native session yet"),
                );
            };
            let resume = match entry.task.agent {
                Agent::Claude => format!("claude --resume {session}"),
                Agent::Codex => format!("codex resume {session}"),
                Agent::Grok => format!("grok -r {session}"),
            };
            let mut task = entry.task.clone();
            task.detached = true;
            task.updated_ms = store::now_ms();
            self.store.save(&task)?;
            entry.task = task;
            entry.pending.clear();
            entry.mail.clear();
            (entry.task.cwd.clone(), resume)
        };
        self.try_emit(
            &id,
            "detached",
            json!({"reason": "handed over with axiopera detach"}),
        )?;
        let (cwd, resume) = task;
        out.text(
            format!("{id} detached; the user can continue it in the native CLI:\n  cd \"{}\"\n  {resume}", cwd.display()),
            json!({"task": id, "cwd": cwd, "command": resume}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    /// Forget stopped tasks untouched for `days`, and older lines of the request log.
    async fn gc<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        days: u64,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        if identity::task_of(caller).is_some() {
            return refuse(
                ErrorCode::Invalid,
                "gc is for the main session or the user, not sub-agents",
            );
        }
        let removed = self.collect(store::now_ms().saturating_sub(days * DAY_MS))?;
        out.text(
            format!("removed {removed} task(s)"),
            json!({"removed": removed}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    /// `gc` by itself, for what nobody touched in `AUTO_GC_DAYS`.
    fn auto_gc(&self) {
        let cutoff = store::now_ms().saturating_sub(AUTO_GC_DAYS * DAY_MS);
        if let Err(e) = self.trim_daemon_log(cutoff) {
            self.log(&format!("trimming daemon.log: {e:#}"));
        }
        match self.collect(cutoff) {
            Ok(0) => {}
            Ok(n) => self.log(&format!("gc: removed {n} task(s)")),
            Err(e) => self.log(&format!("gc: {e:#}")),
        }
    }

    /// Forget stopped tasks untouched since `cutoff_ms`, and older lines of the request log. A
    /// task stays while any of its children does. Returns how many tasks went.
    fn collect(&self, cutoff_ms: u64) -> Result<usize> {
        self.trim_requests(cutoff_ms)?;
        {
            let mut st = self.lock();
            st.ledger.seq = st.seq;
            st.ledger.save(&self.opts.home)?;
        }
        let settled = |e: &Entry| {
            e.actor.is_none()
                && e.pending.is_empty()
                && (e.task.detached
                    || (e.task.pending_inputs.is_empty() && e.task.pending_mail.is_empty()))
                && !matches!(e.task.status, Status::Queued | Status::Running)
                && e.task.updated_ms < cutoff_ms
        };
        // Children first, so their parents can go in the same run.
        let mut old: Vec<(usize, String)> = {
            let st = self.lock();
            st.tasks
                .values()
                .filter(|e| settled(e))
                .map(|e| {
                    let depth = std::iter::successors(Some(&e.task), |t| {
                        identity::task_of(&t.parent)
                            .and_then(|p| st.tasks.get(p))
                            .map(|p| &p.task)
                    })
                    .count();
                    (depth, e.task.id.clone())
                })
                .collect()
        };
        old.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        let mut removed = 0;
        for (_, id) in old {
            // Checked and removed under one lock, so nothing can resume the task halfway.
            let mut st = self.lock();
            let Some(e) = st.tasks.get(&id) else {
                continue;
            };
            if !settled(e)
                || st
                    .tasks
                    .values()
                    .any(|c| identity::task_of(&c.task.parent) == Some(id.as_str()))
            {
                continue;
            }
            self.store.remove(&id)?;
            st.tasks.remove(&id);
            let suffix = format!("|{id}");
            st.ledger.cursors.retain(|k, _| !k.ends_with(&suffix));
            removed += 1;
        }
        let mut st = self.lock();
        let State { tasks, ledger, .. } = &mut *st;
        ledger
            .codex_hosts
            .retain(|host, _| tasks.values().any(|e| e.task.parent == *host));
        ledger
            .pending_pushes
            .retain(|host, _| ledger.codex_hosts.contains_key(host));
        ledger
            .doorbells
            .retain(|host, _| ledger.codex_hosts.contains_key(host));
        ledger.save(&self.opts.home)?;
        Ok(removed)
    }

    /// Route an agent's approval request to whoever may decide it, or refuse it at once.
    pub fn request_approval(self: &Arc<Self>, id: &str, a: agent::Approval) {
        let agent::Approval {
            summary,
            detail,
            reply,
            withdrawn,
        } = a;
        let read_only = self
            .lock()
            .tasks
            .get(id)
            .is_none_or(|e| e.task.tier == ipc::Tier::Read);
        if read_only {
            // A read-only task has nothing to get approved.
            let _ = reply.send(agent::Decision::Deny(agent::REFUSED.into()));
            self.emit(id, "blocked", json!({"detail": summary}));
            return;
        }
        let (rid, approver) = {
            let mut st = self.lock();
            let approver = approver(&st, id);
            let rid = store::new_id('r', |r| st.approvals.contains_key(r));
            st.approvals.insert(
                rid.clone(),
                Pending {
                    task: id.to_string(),
                    approver: approver.clone(),
                    summary: summary.clone(),
                    delivered: false,
                    reply,
                },
            );
            (rid, approver)
        };
        let mut data =
            json!({"id": rid, "summary": summary, "approver": approver, "detail": detail});
        if let Err(e) = self
            .keep_whole(&mut data, id, "approvals", &rid, &detail)
            .and_then(|()| self.try_emit(id, "approval_request", data))
        {
            if let Some(pending) = self.lock().approvals.remove(&rid) {
                let _ = pending.reply.send(agent::Decision::Deny(format!(
                    "Could not persist approval request: {e:#}"
                )));
            }
            self.log(&format!("saving approval request: {e:#}"));
            return;
        }
        let d = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(d.opts.approval_timeout) => {
                    let _ = d.resolve(
                        &rid,
                        None,
                        agent::Decision::Deny("Approval timed out; the action was denied.".into()),
                    );
                }
                Ok(()) = withdrawn => d.withdraw(&rid),
            }
        });
    }

    /// The backend stopped waiting for a decision: drop the request undecided.
    fn withdraw(&self, rid: &str) {
        let Some(pending) = self.lock().approvals.remove(rid) else {
            return;
        };
        self.emit(&pending.task, "withdrawn", json!({"id": rid}));
    }

    /// Keep a text too long to show whole in a file, and note the file in the event's `data`.
    fn keep_whole(
        &self,
        data: &mut Value,
        task: &str,
        kind: &str,
        name: &str,
        text: &str,
    ) -> Result<()> {
        if render::tokens(text) <= render::WHOLE_TOKENS {
            return Ok(());
        }
        let file = self.store.text_path(task, kind, name);
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, text)?;
        data["file"] = json!(file);
        Ok(())
    }

    /// Answer an approval request. `by` is the deciding caller, or None for the timeout.
    fn resolve(&self, rid: &str, by: Option<&str>, decision: agent::Decision) -> Result<String> {
        let pending = {
            let mut st = self.lock();
            let Some(p) = st.approvals.get(rid) else {
                return refuse(
                    ErrorCode::NotFound,
                    format!(
                        "no pending request {rid}; it may have timed out, been decided or been withdrawn"
                    ),
                );
            };
            // Like questions: tasks decide only what is routed to them; sessions and people
            // stand for the user.
            if let Some(by) = by
                && identity::task_of(by).is_some()
                && p.approver != by
            {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{rid} is for {} to decide", p.approver),
                );
            }
            // An agent approves what it has read in full, not a one-line reminder of it.
            if let (Some(by), agent::Decision::Approve) = (by, &decision)
                && by != "user"
                && !p.delivered
            {
                return refuse(
                    ErrorCode::Invalid,
                    format!("read {rid} in full first: run `axiopera inbox`"),
                );
            }
            st.approvals.remove(rid).unwrap()
        };
        let (kind, reason) = match &decision {
            agent::Decision::Approve => ("approved", None),
            agent::Decision::Deny(r) => ("denied", Some(r.clone())),
        };
        if let Err(e) = self.try_emit(
            &pending.task,
            kind,
            json!({"id": rid, "by": by.unwrap_or("timeout"), "reason": reason}),
        ) {
            self.lock().approvals.insert(rid.to_string(), pending);
            return Err(e);
        }
        pending
            .reply
            .send(decision)
            .map_err(|_| anyhow!("approval requester has stopped"))?;
        Ok(pending.task)
    }

    /// Cancel a task and its descendants. Returns how many tasks were affected.
    #[cfg(test)]
    fn cancel_tree(self: &Arc<Self>, id: &str, reason: &str, by: &str) -> usize {
        match self.try_cancel_tree(id, reason, by) {
            Ok(n) => n,
            Err(e) => {
                self.log(&format!("cancelling {id}: {e:#}"));
                0
            }
        }
    }

    fn try_cancel_tree(self: &Arc<Self>, id: &str, reason: &str, by: &str) -> Result<usize> {
        self.stop_tree(id, reason, by, false)
    }

    fn stop_tree(
        self: &Arc<Self>,
        id: &str,
        reason: &str,
        by: &str,
        interrupted: bool,
    ) -> Result<usize> {
        let children: Vec<String> = self
            .lock()
            .tasks
            .values()
            .filter(|e| identity::task_of(&e.task.parent) == Some(id))
            .map(|e| e.task.id.clone())
            .collect();
        let mut n = 0;
        for child in children {
            n += self.stop_tree(
                &child,
                if interrupted {
                    "parent timed out"
                } else {
                    "parent cancelled"
                },
                by,
                interrupted,
            )?;
        }
        let actor = {
            let mut st = self.lock();
            let Some(entry) = st.tasks.get_mut(id) else {
                return Ok(n);
            };
            if entry.task.status == Status::Cancelled {
                return Ok(n);
            }
            let mut task = entry.task.clone();
            task.status = if interrupted {
                Status::Interrupted
            } else {
                Status::Cancelled
            };
            task.updated_ms = store::now_ms();
            self.store.save(&task)?;
            entry.task = task;
            entry.pending.clear();
            // An actor letting go of its process no longer reads commands.
            let actor = entry.actor.clone().filter(|_| !entry.releasing);
            entry.releasing = entry.actor.is_some();
            actor
        };
        n += 1;
        let cause = task::Cancellation {
            reason: reason.to_string(),
            by: by.to_string(),
            interrupted,
        };
        match actor {
            Some(a) => {
                let _ = a.send(Cmd::Cancel(cause));
            }
            None => {
                self.try_emit(
                    id,
                    if interrupted {
                        "interrupted"
                    } else {
                        "cancelled"
                    },
                    json!(cause),
                )?;
            }
        }
        Ok(n)
    }

    /// No process left in the task or any of its descendants.
    fn subtree_stopped(&self, id: &str) -> bool {
        let st = self.lock();
        st.tasks
            .values()
            .filter(|e| within(&st, &e.task, id))
            .all(|e| e.actor.is_none())
    }

    fn interrupt_children(self: &Arc<Self>, id: &str) {
        let children: Vec<String> = self
            .lock()
            .tasks
            .values()
            .filter(|e| identity::task_of(&e.task.parent) == Some(id))
            .map(|e| e.task.id.clone())
            .collect();
        for child in children {
            if let Err(e) = self.stop_tree(&child, "parent timed out", "timeout", true) {
                self.log(&format!("interrupting {child}: {e:#}"));
            }
        }
    }

    fn children(&self, st: &State, caller: &str, only: &[String]) -> Vec<String> {
        st.tasks
            .values()
            .filter(|e| e.task.parent == caller && (only.is_empty() || only.contains(&e.task.id)))
            .map(|e| e.task.id.clone())
            .collect()
    }

    /// Block until the caller's sub-tasks (or those in `only`) have news, then list it without
    /// handing anything over.
    async fn wait<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        only: &[String],
        timeout_ms: Option<u64>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let only = self.find_all(caller, only)?;
        let deadline = timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
        loop {
            enum Check {
                News(String, Value),
                Idle,
                Wait(broadcast::Receiver<Event>),
            }
            let check = {
                let st = self.lock();
                let news = self.news(&st, caller, &only);
                if !news.is_empty() {
                    Check::News(self.news_text(&st, &news), serde_json::to_value(&news)?)
                } else if !self.live(&st, caller, &only) {
                    Check::Idle
                } else {
                    // Subscribe while holding the lock so no event slips between check and wait.
                    Check::Wait(self.events.subscribe())
                }
            };
            let mut rx = match check {
                Check::News(text, data) => {
                    out.text(text, data).await?;
                    return Ok(ipc::exit::OK);
                }
                Check::Idle => return self.nothing_running(caller, &only, out).await,
                Check::Wait(rx) => rx,
            };
            let next = async {
                loop {
                    match rx.recv().await {
                        Ok(e) if store::deliverable(&e.kind) || e.kind == RELEASED => return,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            };
            match deadline {
                Some(d) => {
                    if tokio::time::timeout_at(d, next).await.is_err() {
                        return self.nothing_new(caller, &only, out).await;
                    }
                }
                None => next.await,
            }
        }
    }

    /// Hand the caller what its sub-tasks (or those in `only`) have for it: approval requests
    /// waiting for it, then whole events oldest first, as many as one reply holds. The rest stays
    /// for the next call.
    async fn inbox<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        only: &[String],
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let only = self.find_all(caller, only)?;
        let delivering = self.delivery(caller).lock_owned().await;
        self.reconcile_before_take(caller).await?;
        let (approvals, events, live) = {
            let st = self.lock();
            (
                self.waiting_approvals(&st, caller, &only),
                self.unseen(&st, caller, &only),
                self.live(&st, caller, &only),
            )
        };
        let batch = self.batch(&approvals, &events);
        if batch.items.is_empty() {
            // At most approval requests already handed over or decided, which show nothing.
            if !batch.seen.is_empty() {
                self.mark_seen(caller, &batch.seen, &[], &[])?;
            }
            drop(delivering);
            self.retract_doorbell(caller);
            return if live {
                self.nothing_new(caller, &only, out).await
            } else {
                self.nothing_running(caller, &only, out).await
            };
        }
        for (e, text) in &batch.items {
            out.text(text.clone(), serde_json::to_value(e)?).await?;
        }
        if batch.left > 0 {
            let more = match batch.left_approvals {
                0 => format!("{} more", batch.left),
                n => format!("{} more, {n} of them approval requests", batch.left),
            };
            out.text(format!("{more}: run `axiopera inbox` again."), Value::Null)
                .await?;
        }
        // The text has reached the caller, whether or not its cursors get saved.
        self.record_delivery(
            caller,
            "inbox",
            &delivery::mark(),
            &batch.shown(),
            &batch.turns,
        );
        self.mark_seen(caller, &batch.seen, &batch.turns, &batch.approvals)?;
        drop(delivering);
        self.retract_doorbell(caller);
        Ok(ipc::exit::OK)
    }

    fn find_all(&self, caller: &str, references: &[String]) -> Result<Vec<String>> {
        references.iter().map(|r| self.find(caller, r)).collect()
    }

    /// Whether any of the caller's children (or those in `only`) is running or about to.
    fn live(&self, st: &State, caller: &str, only: &[String]) -> bool {
        self.children(st, caller, only).iter().any(|id| {
            let e = &st.tasks[id];
            e.actor.is_some() || !e.pending.is_empty()
        })
    }

    /// Nothing new yet: where each running sub-task stands.
    async fn nothing_new<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        only: &[String],
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let lines = {
            let st = self.lock();
            let mut live: Vec<&Entry> = self
                .children(&st, caller, only)
                .iter()
                .map(|id| &st.tasks[id])
                .filter(|e| e.actor.is_some() || !e.pending.is_empty())
                .collect();
            live.sort_by_key(|e| e.task.created_ms);
            live.iter()
                .map(|e| {
                    let mut line = format!("  {}", render::task_label(&e.task));
                    let progress = self.progress(&st, e, caller);
                    if !progress.is_empty() {
                        line.push_str(&format!(": {progress}"));
                    }
                    line
                })
                .collect::<Vec<_>>()
        };
        out.text(
            format!(
                "nothing new; still running:
{}",
                lines.join(
                    "
"
                )
            ),
            Value::Null,
        )
        .await?;
        Ok(ipc::exit::NOTHING_NEW)
    }

    /// Where a task stands: its running turn, and what it waits for. Empty when idle.
    fn progress(&self, st: &State, e: &Entry, caller: &str) -> String {
        let now = store::now_ms();
        let t = &e.task;
        let mut parts = Vec::new();
        if t.status == Status::Queued {
            parts.push("queued".to_string());
        } else if e.actor.is_some() && e.in_turn {
            let mut turn = format!(
                "turn {}, {}",
                t.turns + 1,
                render::duration(now.saturating_sub(e.turn_started_ms))
            );
            match &e.last_tool {
                Some((call, at, _)) => turn.push_str(&format!(
                    ", {} tool call(s), last {} ago: {call}",
                    e.tool_calls,
                    render::duration(now.saturating_sub(*at))
                )),
                None => turn.push_str(", no tool calls yet"),
            }
            parts.push(turn);
        } else if e.releasing {
            // Done; the process is on its way out.
        } else if e.actor.is_some() && e.turn_ended_ms > 0 {
            parts.push(format!(
                "turn {} ended {} ago; its background work still runs",
                t.turns,
                render::duration(now.saturating_sub(e.turn_ended_ms))
            ));
        } else if e.actor.is_some() {
            parts.push("starting".to_string());
        }
        for (rid, p) in st.approvals.iter().filter(|(_, p)| p.task == t.id) {
            parts.push(if p.approver == caller {
                format!("waiting for your approval of {rid}")
            } else {
                format!("waiting for approval of {rid} from {}", p.approver)
            });
        }
        let open: Vec<&str> = t.open_questions().map(|q| q.id.as_str()).collect();
        if !open.is_empty() {
            parts.push(format!("waiting for an answer to {}", open.join(", ")));
        }
        parts.join("; ")
    }

    /// Nothing new and nothing running: what still waits for the caller.
    async fn nothing_running<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        only: &[String],
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let (none, open) = {
            let st = self.lock();
            let ids = self.children(&st, caller, only);
            let questions = ids.iter().flat_map(|id| {
                st.tasks[id].task.open_questions().map(move |q| {
                    format!(
                        "  {id} asks {}: {}  (axiopera reply {} \"...\")",
                        q.id,
                        agent::clip(&q.text, 300),
                        q.id
                    )
                })
            });
            let approvals = st
                .approvals
                .iter()
                .filter(|(_, p)| p.approver == caller)
                .map(|(rid, p)| {
                    format!(
                        "  {} needs approval {rid}: {}  (axiopera approve {rid} | axiopera deny {rid})",
                        p.task,
                        agent::clip(&p.summary, 300)
                    )
                });
            let open: Vec<String> = questions.chain(approvals).collect();
            (ids.is_empty(), open)
        };
        let text = if !open.is_empty() {
            format!(
                "no running sub-tasks; these wait for you:\n{}",
                open.join("\n")
            )
        } else if none {
            "no sub-tasks".to_string()
        } else {
            "no running sub-tasks; nothing new".to_string()
        };
        out.text(text, Value::Null).await?;
        Ok(ipc::exit::NOTHING_RUNNING)
    }

    /// News as reminders show it: a line each, at most ten, and how to take it.
    fn news_text(&self, st: &State, news: &[Event]) -> String {
        render::summary(news.iter().map(|e| self.news_line(st, e)))
    }

    /// One line on a piece of news: which task, what happened, and a preview.
    fn news_line(&self, st: &State, e: &Event) -> String {
        let t = &st.tasks[&e.task].task;
        let who = render::task_label(t);
        let s = |k: &str| e.data.get(k).and_then(Value::as_str).unwrap_or("");
        let n = |k: &str| e.data.get(k).and_then(Value::as_u64).unwrap_or(0);
        let line = match e.kind.as_str() {
            "turn_ended" => {
                let turn = n("turn") as u32;
                let preview = self
                    .previews
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&self.store.turn_path(&e.task, turn));
                if e.data.get("ok") == Some(&Value::Bool(true)) {
                    format!("{who} finished turn {turn}: {preview}")
                } else {
                    format!("{who} turn {turn} failed ({}): {preview}", s("error"))
                }
            }
            "question" => format!(
                "{who} asks {}: {}",
                s("id"),
                agent::clip(s("text"), render::PREVIEW)
            ),
            "approval_request" => format!(
                "{who} needs approval {}: {}",
                s("id"),
                agent::clip(s("summary"), render::PREVIEW)
            ),
            "stalled" => format!(
                "{who} no activity for {}",
                render::duration(n("quiet_secs") * 1000)
            ),
            "failed" => format!("{who} failed: {}", agent::clip(s("error"), render::PREVIEW)),
            "interrupted" => format!("{who} interrupted: {}", s("reason")),
            "cancelled" => format!("{who} cancelled: {}", s("reason")),
            _ => render::event_line(e, t.agent),
        };
        render::fit_bytes(&line, render::LINE_BYTES)
    }

    /// The caller's news as reminders show it, and the newest news's sequence number; nothing
    /// for hosts that get their news pushed.
    pub(super) fn reminder(&self, caller: &str) -> Option<(String, u64)> {
        let st = self.lock();
        if st
            .ledger
            .codex_hosts
            .get(caller)
            .is_some_and(|h| !h.doorbell)
        {
            return None;
        }
        let news = self.news(&st, caller, &[]);
        let seq = news.iter().map(|e| e.seq).max()?;
        Some((self.news_text(&st, &news), seq))
    }

    /// What the caller has not taken and is worth interrupting it for. An approval request
    /// counts while it waits for a decision and has not reached the caller.
    fn news(&self, st: &State, caller: &str, only: &[String]) -> Vec<Event> {
        let mut news: Vec<Event> = self
            .unseen(st, caller, only)
            .into_iter()
            .filter(|e| e.kind != "approval_request" && is_news(e, caller))
            .collect();
        news.extend(self.waiting_approvals(st, caller, only));
        news.sort_by_key(|e| e.seq);
        news
    }

    /// Approval requests routed to the caller (from the tasks in `only`) that wait for a
    /// decision and have not reached it, oldest first. They go ahead of other events, whatever
    /// the caller's cursors.
    fn waiting_approvals(&self, st: &State, caller: &str, only: &[String]) -> Vec<Event> {
        let mut found: Vec<Event> = st
            .approvals
            .iter()
            .filter(|(_, p)| p.approver == caller && !p.delivered)
            .filter_map(|(rid, p)| {
                let task = st.tasks.get(&p.task)?;
                if !only.is_empty() && !only.iter().any(|root| within(st, &task.task, root)) {
                    return None;
                }
                task.approval_events
                    .get(caller)?
                    .iter()
                    .map(|&at| &task.deliverable[at])
                    .find(|e| {
                        e.kind == "approval_request"
                            && e.data.get("id").and_then(Value::as_str) == Some(rid)
                    })
                    .cloned()
            })
            .collect();
        found.sort_by_key(|e| e.seq);
        found
    }

    /// What one reply hands over: the waiting `approvals` first, then `events` oldest first;
    /// whole, within the reply budget. The first goes in any case, a result cut down to fit.
    /// Approval requests among `events` are passed over: while they wait they come from
    /// `approvals`, and once decided there is nothing left to do about them.
    fn batch(&self, approvals: &[Event], events: &[Event]) -> Batch {
        let mut budget = render::RESPONSE_TOKENS;
        let mut b = Batch {
            items: Vec::new(),
            turns: Vec::new(),
            seen: Vec::new(),
            approvals: Vec::new(),
            left: 0,
            left_approvals: 0,
        };
        let mut full = false;
        for e in approvals {
            let Some((text, _)) = self.render_wake(e, None) else {
                continue;
            };
            let cost = render::tokens(&text);
            if !b.items.is_empty() && cost > budget {
                full = true;
                break;
            }
            budget = budget.saturating_sub(cost);
            b.approvals
                .extend(e.data.get("id").and_then(Value::as_str).map(str::to_string));
            b.items.push((e.clone(), text));
        }
        for e in events.iter().take_while(|_| !full) {
            if e.kind == "approval_request" {
                b.seen.push(e.clone());
                continue;
            }
            let first = b.items.is_empty();
            let Some((text, turn)) = self.render_wake(e, first.then_some(budget)) else {
                b.seen.push(e.clone());
                continue;
            };
            let cost = render::tokens(&text);
            if !first && cost > budget {
                break;
            }
            budget = budget.saturating_sub(cost);
            b.turns.extend(turn.map(|t| (e.task.clone(), t)));
            b.seen.push(e.clone());
            b.items.push((e.clone(), text));
        }
        b.left_approvals = approvals.len() - b.approvals.len();
        b.left = b.left_approvals
            + events[b.seen.len()..]
                .iter()
                .filter(|e| e.kind != "approval_request")
                .count();
        b.items.sort_by_key(|(e, _)| urgency(e));
        b
    }

    /// Held while handing the caller its events, from reading them to marking them seen.
    fn delivery(&self, caller: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut st = self.lock();
        st.deliveries.entry(caller.to_string()).or_default().clone()
    }

    /// Once a batch reached the caller: advance its cursors past the events `seen`, and mark the
    /// results in `turns` and the approval requests in `approvals` delivered.
    fn mark_seen(
        &self,
        caller: &str,
        seen: &[Event],
        turns: &[(String, u32)],
        approvals: &[String],
    ) -> Result<()> {
        let mut st = self.lock();
        for (task, turn) in turns {
            if let Some(entry) = st.tasks.get_mut(task) {
                let mut task = entry.task.clone();
                task.delivered = task.delivered.max(*turn);
                task.updated_ms = store::now_ms();
                self.store.save(&task)?;
                entry.task = task;
            }
        }
        let mut ledger = st.ledger.clone();
        for e in seen {
            // A task gc removed meanwhile took its cursors along.
            if !st.tasks.contains_key(&e.task) {
                continue;
            }
            let cursor = ledger
                .cursors
                .entry(cursor_key(caller, &e.task))
                .or_default();
            *cursor = (*cursor).max(e.seq);
        }
        ledger.save(&self.opts.home)?;
        st.ledger = ledger;
        for rid in approvals {
            if let Some(p) = st.approvals.get_mut(rid) {
                p.delivered = true;
            }
        }
        Ok(())
    }

    /// Render one event; a turn's result text is included the first time only. With `fit`, the
    /// result is cut down to that many tokens. Nothing if gc has removed the task.
    fn render_wake(&self, e: &Event, fit: Option<usize>) -> Option<(String, Option<u32>)> {
        let (agent, delivered) = {
            let st = self.lock();
            let t = &st.tasks.get(&e.task)?.task;
            (t.agent, t.delivered)
        };
        if e.kind != "turn_ended" {
            return Some((render::event_full(e, agent), None));
        }
        let head = render::event_line(e, agent);
        let turn = e.data.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        if turn <= delivered {
            return Some((format!("{head}\n(result already delivered)"), None));
        }
        let path = self.store.turn_path(&e.task, turn);
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(_) if !self.lock().tasks.contains_key(&e.task) => return None,
            Err(_) => String::new(),
        };
        let body = match fit {
            Some(budget) => {
                let frame = render::tokens(&format!("{head}\n{}", render::tagged("result", "")));
                render::result_body(&body, &path, budget.saturating_sub(frame))
            }
            None => body,
        };
        Some((
            format!("{head}\n{}", render::tagged("result", &body)),
            Some(turn),
        ))
    }

    /// A line for each piece of news, until stopped, or for a sub-agent until it has been idle
    /// for `WATCH_IDLE`; news within `WATCH_GAP` of the last line is merged into the next one.
    /// Takes nothing.
    async fn watch<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let (mut rx, mut news) = {
            let st = self.lock();
            (self.events.subscribe(), self.news(&st, caller, &[]))
        };
        let mut due = tokio::time::Instant::now();
        let sub_agent = identity::task_of(caller).is_some();
        let mut tick = tokio::time::interval(WATCH_GAP);
        let mut idle_since = None;
        loop {
            if sub_agent && news.is_empty() {
                let idle = {
                    let st = self.lock();
                    !self.live(&st, caller, &[]) && self.news(&st, caller, &[]).is_empty()
                };
                if !idle {
                    idle_since = None;
                } else if idle_since
                    .get_or_insert_with(tokio::time::Instant::now)
                    .elapsed()
                    >= WATCH_IDLE
                {
                    return Ok(ipc::exit::NOTHING_RUNNING);
                }
            }
            if !news.is_empty() && tokio::time::Instant::now() >= due {
                let line = {
                    let st = self.lock();
                    // Since it came, the caller may have taken it, an approval request may have
                    // been decided, or gc may have removed its task.
                    let still = self.news(&st, caller, &[]);
                    news.retain(|e| still.iter().any(|s| s.seq == e.seq));
                    news.first().map(|first| {
                        let line = self.news_line(&st, first);
                        match news.len() {
                            1 => line,
                            n => render::fit_bytes(
                                &format!("{line} (+{} more)", n - 1),
                                render::LINE_BYTES,
                            ),
                        }
                    })
                };
                if let Some(line) = line {
                    out.text(line, serde_json::to_value(&news)?).await?;
                    due = tokio::time::Instant::now() + WATCH_GAP;
                }
                news.clear();
            }
            let wake = tokio::time::sleep_until(due);
            tokio::select! {
                e = rx.recv() => match e {
                    Ok(e) if is_news(&e, caller) && self.concerns(&e, caller) => news.push(e),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(ipc::exit::OK),
                },
                _ = wake, if !news.is_empty() => {}
                _ = tick.tick(), if sub_agent => {}
            }
        }
    }

    /// Keep a host-owned background terminal visible for as long as its sub-tasks run.
    async fn watch_status<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let mut seen = HashMap::new();
        let mut tick = tokio::time::interval(WATCH_GAP);
        loop {
            tick.tick().await;
            let (rows, live) = {
                let st = self.lock();
                let mut entries: Vec<_> = st
                    .tasks
                    .values()
                    .filter(|e| {
                        e.task.parent == caller
                            && (e.actor.is_some()
                                || !e.pending.is_empty()
                                || seen.contains_key(&e.task.id))
                    })
                    .collect();
                entries.sort_by_key(|e| (e.task.created_ms, &e.task.id));
                let rows = entries
                    .iter()
                    .map(|e| {
                        (
                            e.task.id.clone(),
                            self.status_text(&st, e, caller),
                            serde_json::to_value(&e.task),
                        )
                    })
                    .collect::<Vec<_>>();
                (rows, self.live(&st, caller, &[]))
            };
            for (id, text, data) in rows {
                if seen.get(&id) != Some(&text) {
                    out.text(text.clone(), data?).await?;
                    seen.insert(id, text);
                }
            }
            if !live {
                out.text("no running sub-tasks", Value::Null).await?;
                return Ok(ipc::exit::OK);
            }
        }
    }

    /// Whether `inbox` would hand the event to the caller.
    fn concerns(&self, e: &Event, caller: &str) -> bool {
        if e.kind == "approval_request" {
            return e.data.get("approver").and_then(Value::as_str) == Some(caller);
        }
        self.lock()
            .tasks
            .get(&e.task)
            .is_some_and(|t| t.task.parent == caller)
    }

    async fn status<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        reference: Option<&str>,
        all: bool,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let target = reference.map(|r| self.find(caller, r)).transpose()?;
        let (text, data) = {
            let st = self.lock();
            let mut rows: Vec<&Entry> = st
                .tasks
                .values()
                .filter(|e| match &target {
                    Some(id) => &e.task.id == id,
                    None => all || e.task.parent == caller,
                })
                .collect();
            rows.sort_by_key(|e| e.task.created_ms);
            let text = if rows.is_empty() {
                "no tasks".to_string()
            } else {
                rows.iter()
                    .map(|e| self.status_text(&st, e, caller))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let data = serde_json::to_value(rows.iter().map(|e| &e.task).collect::<Vec<_>>())?;
            (text, data)
        };
        out.text(text, data).await?;
        Ok(ipc::exit::OK)
    }

    fn status_text(&self, st: &State, e: &Entry, caller: &str) -> String {
        let mut line = render::status_line(&e.task, e.actor.is_some() && !e.releasing);
        let progress = self.progress(st, e, caller);
        if !progress.is_empty() {
            line.push_str(&format!("  ({progress})"));
        }
        line
    }

    async fn log_cmd<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        reference: &str,
        tail: usize,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let Some(agent) = self.lock().tasks.get(&id).map(|e| e.task.agent) else {
            return refuse(ErrorCode::NotFound, format!("no task {id}"));
        };
        let events = delivery::tail_events(&self.store.events_path(&id), tail)?;
        let text = events
            .iter()
            .map(|e| render::event_line(e, agent))
            .collect::<Vec<_>>()
            .join("\n");
        out.text(text, serde_json::to_value(&events)?).await?;
        Ok(ipc::exit::OK)
    }

    async fn result<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        reference: &str,
        turn: Option<u32>,
        force: bool,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let Some((turns, delivered)) = self
            .lock()
            .tasks
            .get(&id)
            .map(|e| (e.task.turns, e.task.delivered))
        else {
            return refuse(ErrorCode::NotFound, format!("no task {id}"));
        };
        let turn = turn.unwrap_or(turns);
        let path = self.store.turn_path(&id, turn);
        if turn == 0 || !path.exists() {
            return refuse(
                ErrorCode::NotFound,
                format!("{id} has no finished turn {turn} yet"),
            );
        }
        if turn <= delivered && !force {
            out.text(
                format!(
                    "{id} turn {turn}: result already delivered. Full text: {}",
                    path.display()
                ),
                Value::Null,
            )
            .await?;
            return Ok(ipc::exit::OK);
        }
        let body = std::fs::read_to_string(&path)?;
        let frame = render::tokens(&render::tagged("result", ""));
        let body = render::result_body(&body, &path, render::RESPONSE_TOKENS - frame);
        out.text(
            render::tagged("result", &body),
            json!({"task": id, "turn": turn, "path": path}),
        )
        .await?;
        self.record_delivery(
            caller,
            "result",
            &delivery::mark(),
            &[],
            &[(id.clone(), turn)],
        );
        self.try_update(&id, |t| t.delivered = t.delivered.max(turn))?;
        Ok(ipc::exit::OK)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn stall_after(&self) -> Duration {
        self.opts.stall
    }
}

/// Who decides a task's approval requests: the nearest ancestor that is a session, a person,
/// or a full-tier task. A work-tier parent cannot tell what exceeds its own tier, so requests
/// pass over it.
fn approver(st: &State, id: &str) -> String {
    let mut key = st.tasks[id].task.parent.clone();
    while let Some(parent) = identity::task_of(&key).and_then(|p| st.tasks.get(p)) {
        if parent.task.tier == ipc::Tier::Full {
            break;
        }
        key = parent.task.parent.clone();
    }
    key
}

/// Whether `t` is the task `root` or one of its descendants.
fn within<'a>(st: &'a State, mut t: &'a Task, root: &str) -> bool {
    loop {
        if t.id == root {
            return true;
        }
        match identity::task_of(&t.parent).and_then(|p| st.tasks.get(p)) {
            Some(parent) => t = &parent.task,
            None => return false,
        }
    }
}

fn cursor_key(caller: &str, task: &str) -> String {
    format!("{caller}|{task}")
}

/// What one reply hands over.
struct Batch {
    /// In the order shown: what holds a sub-task up first.
    items: Vec<(Event, String)>,
    /// Turns whose result text is included.
    turns: Vec<(String, u32)>,
    /// Events taken in order, for the caller's cursors to move past.
    seen: Vec<Event>,
    /// Approval requests handed over.
    approvals: Vec<String>,
    /// How many items are left for the next reply, and how many of them approval requests.
    left: usize,
    left_approvals: usize,
}

impl Batch {
    /// The events shown, each by its task and sequence number.
    fn shown(&self) -> Vec<(String, u64)> {
        self.items
            .iter()
            .map(|(e, _)| (e.task.clone(), e.seq))
            .collect()
    }
}

fn urgency(e: &Event) -> u8 {
    match e.kind.as_str() {
        "approval_request" => 0,
        "question" => 1,
        "failed" | "interrupted" | "stalled" | "cancelled" => 2,
        "turn_ended" if e.data.get("ok") != Some(&Value::Bool(true)) => 2,
        "turn_ended" => 3,
        _ => 4,
    }
}

/// Events worth interrupting the caller for: all that `inbox` hands over except sibling notes,
/// detach notices and the caller's own cancellations, which wait for its next `inbox`.
fn is_news(event: &Event, caller: &str) -> bool {
    if event.kind == "cancelled" {
        return event.data.get("by").and_then(Value::as_str) != Some(caller);
    }
    matches!(
        event.kind.as_str(),
        "turn_ended" | "question" | "approval_request" | "stalled" | "failed" | "interrupted"
    )
}

/// The environment a sub-agent starts with: its caller's, minus host session identity, plus ours.
pub fn agent_env(
    caller: &[(String, String)],
    task: &str,
    agent: Agent,
    home: &Path,
) -> Vec<(String, String)> {
    let strip = |k: &str| {
        identity::STRIP_VARS
            .iter()
            .any(|s| s.eq_ignore_ascii_case(k))
            || k.eq_ignore_ascii_case(identity::TASK_VAR)
            || k.eq_ignore_ascii_case(paths::HOME_VAR)
    };
    let mut env: Vec<(String, String)> =
        caller.iter().filter(|(k, _)| !strip(k)).cloned().collect();
    env.push((identity::TASK_VAR.into(), task.into()));
    env.push((paths::HOME_VAR.into(), home.display().to_string()));
    for (k, v) in agent::extra_env(agent) {
        env.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        env.push((k.to_string(), v.to_string()));
    }
    // Make sure the agent finds this very binary as `axiopera`.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sep = if cfg!(windows) { ";" } else { ":" };
        match env.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("PATH")) {
            Some((_, v)) => *v = format!("{}{sep}{v}", dir.display()),
            None => env.push(("PATH".into(), dir.display().to_string())),
        }
    }
    env
}

/// Drop the start of `daemon.log` up to its first entry from `cutoff_ms` on. Each entry starts
/// with its time; lines without one belong to the entry above.
fn trim_log(path: &Path, cutoff_ms: u64) -> Result<()> {
    let mut keeping = false;
    crate::diagnostics::retain_lines(path, |line| {
        let time = line
            .split(|b| *b == b' ')
            .next()
            .and_then(|t| std::str::from_utf8(t).ok())
            .and_then(|t| t.parse::<u64>().ok());
        keeping |= time.is_some_and(|t| t >= cutoff_ms);
        keeping
    })?;
    Ok(())
}

/// Defaults, overridden by `config.toml` in `home`.
pub fn options(home: PathBuf) -> Result<Options> {
    let c = config::load(&home)?;
    let minutes = |m: Option<u64>, default: u64| Duration::from_secs(m.unwrap_or(default) * 60);
    Ok(Options {
        idle: Duration::from_secs(600),
        concurrency: c.concurrency.unwrap_or(50).max(1),
        stall: minutes(c.stall_minutes, 10),
        // Short, because a sub-agent waits for the decision and some hosts only see the request
        // when their turn ends; a denied sub-agent reports what it needed instead.
        approval_timeout: minutes(c.approval_timeout_minutes, 2),
        agents: c.agents,
        home,
    })
}
