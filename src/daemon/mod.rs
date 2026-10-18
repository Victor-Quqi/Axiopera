//! The per-user daemon: owns every sub-agent process and all task state.

mod push;
mod render;
mod task;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
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
use crate::worktree;
use push::Reach;

pub use task::Cmd;

/// Broadcast-only signal that a task's process is gone.
const RELEASED: &str = "_released";

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
}

struct State {
    seq: u64,
    tasks: HashMap<String, Entry>,
    ledger: Ledger,
    /// Tasks with a live actor (an agent process running or shutting down).
    running: usize,
    /// Of those, the ones spawned by a session or a person rather than by another task.
    top_running: usize,
    queue: VecDeque<String>,
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
}

struct Pending {
    task: String,
    /// Caller key of whoever decides.
    approver: String,
    summary: String,
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
    /// Events that end a `wait`, in order.
    wake: Vec<Event>,
    blocked: usize,
}

/// One request's reply stream.
pub struct Out<'a, W> {
    w: &'a mut W,
}

impl<W: AsyncWrite + Unpin> Out<'_, W> {
    pub async fn text(&mut self, text: impl Into<String>, data: Value) -> std::io::Result<()> {
        ipc::write(
            self.w,
            &Reply::Out {
                text: text.into(),
                data,
            },
        )
        .await
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
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(daemon_dir.join("daemon.log"))
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

    let d = Arc::new(Daemon::load(opts, log));
    d.log(&format!("started, pid {}", std::process::id()));
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
    fn load(opts: Options, log: Option<File>) -> Daemon {
        let store = Store::new(&opts.home);
        let ledger = Ledger::load(&opts.home);
        let mut tasks = HashMap::new();
        let mut seq = 0;
        for task in store.load_all() {
            let events = store.events(&task.id);
            seq = seq.max(events.last().map_or(0, |e| e.seq));
            let wake = events
                .into_iter()
                .filter(|e| store::wakes(&e.kind))
                .collect();
            let entry = Entry {
                blocked: 0,
                task,
                actor: None,
                releasing: false,
                env: None,
                pending: Vec::new(),
                mail: Vec::new(),
                wake,
            };
            tasks.insert(entry.task.id.clone(), entry);
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
                top_running: 0,
                queue: VecDeque::new(),
                last_active: Instant::now(),
                stopping: false,
                approvals: HashMap::new(),
                deliveries: HashMap::new(),
                pushing: HashMap::new(),
                push_failures: HashMap::new(),
            }),
            events,
            shutdown: Notify::new(),
            conns: AtomicUsize::new(0),
            log: Mutex::new(log),
        };
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
            d.set_status(&id, Status::Interrupted);
            d.emit(
                &id,
                "interrupted",
                json!({"reason": "the daemon stopped while this task was running"}),
            );
        }
        d
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A request that panicked should not take every later one down with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, msg: &str) {
        if let Some(f) = self.log.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{} {msg}", store::now_ms());
        }
    }

    /// Append an event and wake waiters. `data` must be a JSON object.
    pub fn emit(&self, task: &str, kind: &str, data: Value) {
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
        if let Err(e) = self.store.append(&event) {
            drop(st);
            self.log(&format!("writing event for {task}: {e:#}"));
            st = self.lock();
        }
        if let Some(entry) = st.tasks.get_mut(task) {
            entry.task.updated_ms = event.ts;
            if kind == "blocked" {
                entry.blocked += 1;
            }
            if store::wakes(kind) {
                entry.wake.push(event.clone());
            }
        }
        let _ = self.events.send(event);
    }

    /// Change a task and persist it.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Task)) {
        let mut st = self.lock();
        let Some(entry) = st.tasks.get_mut(id) else {
            return;
        };
        f(&mut entry.task);
        entry.task.updated_ms = store::now_ms();
        // Saved under the lock, so concurrent updates reach the disk in order.
        if let Err(e) = self.store.save(&entry.task) {
            self.log(&format!("saving {id}: {e:#}"));
        }
    }

    pub fn set_status(&self, id: &str, status: Status) {
        self.update(id, |t| t.status = status);
    }

    async fn watchdog(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
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
        let mut out = Out { w: &mut w };
        let compatible = env.protocol == ipc::PROTOCOL && env.version == env!("CARGO_PKG_VERSION");
        if !compatible && !matches!(env.request, Request::Stop { .. }) {
            if self.lock().running == 0 {
                out.error(ErrorCode::Version, "daemon version differs; restarting it")
                    .await?;
                self.shutdown.notify_one();
            } else {
                out.error(
                    ErrorCode::Invalid,
                    format!(
                        "the running daemon is version {} and has live tasks; wait for them or run `axiopera daemon stop --force`",
                        env!("CARGO_PKG_VERSION")
                    ),
                )
                .await?;
            }
            return Ok(());
        }
        if !env.request.allowed_in_sandbox() && peer.restricted() {
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
        let result = if matches!(env.request, Request::Wait { .. } | Request::Watch) {
            tokio::select! {
                r = self.dispatch(env, &mut out) => r,
                _ = reader.read::<Value>() => return Ok(()),
            }
        } else {
            self.dispatch(env, &mut out).await
        };
        match result {
            Ok(exit) => out.done(exit).await?,
            Err(e) => match e.downcast_ref::<RefuseErr>() {
                Some(RefuseErr(code, msg)) => out.error(*code, msg.clone()).await?,
                None => out.error(ErrorCode::Internal, format!("{e:#}")).await?,
            },
        }
        Ok(())
    }

    async fn dispatch<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        env: Envelope,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let caller = env.caller.clone();
        match env.request {
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
            Request::Send { task, message } => {
                self.send(&caller, &task, message, env.env, out).await
            }
            Request::Wait { tasks, timeout_ms } => {
                let exit = self.wait(&caller, &tasks, timeout_ms, out).await;
                self.retract_doorbell(&caller);
                exit
            }
            Request::Watch => self.watch(&caller, out).await,
            Request::Status { task, all } => self.status(&caller, task.as_deref(), all, out).await,
            Request::Log { task, tail } => self.log_cmd(&caller, &task, tail, out).await,
            Request::Result { task, turn, force } => {
                self.result(&caller, &task, turn, force, out).await
            }
            Request::Ask { text } => self.ask(&caller, text, out).await,
            Request::Reply { id, text } => self.reply(&caller, &id, text, env.env, out).await,
            Request::Inbox => self.inbox(&caller, out).await,
            Request::Gc { days } => {
                self.gc(&caller, days, env.env.unwrap_or_default(), out)
                    .await
            }
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
                    Some(r) => format!("Denied by your parent: {r}"),
                    None => "Denied by your parent. Continue without this action.".into(),
                };
                let task = self.resolve(&id, Some(&caller), agent::Decision::Deny(reason))?;
                out.text(format!("denied {id}"), json!({"id": id, "task": task}))
                    .await?;
                Ok(ipc::exit::OK)
            }
            Request::Cancel { task } => {
                let id = self.find(&caller, &task)?;
                let mut rx = self.events.subscribe();
                let n = self.cancel_tree(&id, "cancelled by request", &caller);
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
                        "a {:?} task cannot spawn a {:?} task",
                        parent_tier.unwrap(),
                        s.tier
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
        let reach = self.register_host(caller, &env).await;

        let now = store::now_ms();
        let id = store::new_id('t', |id| self.store.exists(id));
        let (cwd, worktree) = if s.worktree {
            let name = s.name.as_deref().unwrap_or(&id);
            let (wt, cwd) = worktree::create(&self.opts.home, &s.cwd, &id, name, &env)
                .map_err(|e| anyhow!(RefuseErr(ErrorCode::Invalid, format!("{e:#}"))))?;
            (cwd, Some(wt))
        } else {
            (s.cwd.clone(), None)
        };
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
            timeout_secs: s.timeout_secs,
            status: Status::Queued,
            session: None,
            session_file: None,
            turns: 0,
            delivered: 0,
            changed_files: Vec::new(),
            questions: Vec::new(),
            worktree: worktree.clone(),
            stalled: false,
            detached: false,
            created_ms: now,
            updated_ms: now,
        };
        self.store.save(&task)?;
        {
            let mut st = self.lock();
            let entry = Entry {
                blocked: 0,
                task,
                actor: None,
                releasing: false,
                env: Some(env),
                pending: vec![s.task.clone()],
                mail: Vec::new(),
                wake: Vec::new(),
            };
            st.tasks.insert(id.clone(), entry);
        }
        self.emit(
            &id,
            "spawned",
            json!({"agent": s.agent, "tier": s.tier, "cwd": cwd}),
        );
        self.schedule(&id);

        let mut notes = render::capability_notes(s.agent, s.tier);
        if let Some(wt) = &worktree {
            notes.push(format!(
                "Worktree: {}; your uncommitted changes are absent. To merge its work, commit there, then run `git merge {}`.",
                wt.path.display(),
                wt.branch
            ));
        }
        if s.tier != ipc::Tier::Read {
            let approver = approver(&self.lock(), &id);
            if approver != caller {
                notes.push(format!("approval requests go to {approver}"));
            }
        }
        let next = match reach {
            Reach::Push => {
                "Results and questions arrive as messages: do not wait or poll; end your turn if nothing else is left."
            }
            Reach::Doorbell => {
                "After your turn ends, a message tells you when results or questions are ready for `axiopera wait`: do not poll; end your turn if nothing else is left."
            }
            Reach::Wait => "Get results and questions with `axiopera wait`.",
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

    /// Start an actor for the task if a slot is free, otherwise queue it.
    fn schedule(self: &Arc<Self>, id: &str) {
        let mut st = self.lock();
        if st.stopping {
            return;
        }
        let Some(entry) = st.tasks.get(id) else {
            return;
        };
        if entry.actor.is_some() || entry.task.status.is_final() || entry.task.detached {
            return;
        }
        // Only top-level tasks count against the limit: a parent waiting for its children
        // holds a slot, and queueing the children behind it would deadlock.
        let top = identity::task_of(&entry.task.parent).is_none();
        if top && st.top_running >= self.opts.concurrency {
            if !st.queue.iter().any(|q| q == id) {
                st.queue.push_back(id.to_string());
            }
            drop(st);
            self.set_status(id, Status::Queued);
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        st.running += 1;
        st.top_running += top as usize;
        st.tasks.get_mut(id).unwrap().actor = Some(tx);
        drop(st);
        tokio::spawn(task::run(self.clone(), id.to_string(), rx));
    }

    /// Called by an actor when its process is gone.
    fn actor_done(self: &Arc<Self>, id: &str) {
        let (next, again) = {
            let mut st = self.lock();
            st.running -= 1;
            if st
                .tasks
                .get(id)
                .is_some_and(|e| identity::task_of(&e.task.parent).is_none())
            {
                st.top_running -= 1;
            }
            st.last_active = Instant::now();
            // Whoever asked is gone; its requests can no longer be acted on.
            st.approvals.retain(|_, p| p.task != id);
            let again = st.tasks.get_mut(id).is_some_and(|e| {
                e.actor = None;
                e.releasing = false;
                !e.pending.is_empty() && !e.task.status.is_final()
            });
            (st.queue.pop_front(), again)
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
        if let Some(next) = next {
            self.schedule(&next);
        }
    }

    async fn send<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        reference: &str,
        message: String,
        env: Option<Vec<(String, String)>>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let how = self.input(&id, message, env).await?;
        out.text(format!("{id}: {how}"), json!({"task": id, "result": how}))
            .await?;
        Ok(ipc::exit::OK)
    }

    /// Hand a message to a task: into its running turn, or as the input of a new one.
    async fn input(
        self: &Arc<Self>,
        id: &str,
        message: String,
        env: Option<Vec<(String, String)>>,
    ) -> Result<&'static str> {
        let (reply, receive) = oneshot::channel();
        let mut input = task::Input {
            text: message,
            reply,
        };
        let handed = 'handoff: {
            let mut st = self.lock();
            let entry = st.tasks.get_mut(id).unwrap();
            if entry.task.status.is_final() {
                return refuse(ErrorCode::Invalid, format!("{id} was cancelled"));
            }
            if entry.task.detached {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{id} was taken over outside axiopera"),
                );
            }
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
                "only sub-agents started by axiopera can ask; you have no parent to ask",
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
        self.update(&task, |t| {
            t.questions.push(store::Question {
                id: qid.clone(),
                text: text.clone(),
                answer: None,
                asked_ms: store::now_ms(),
            })
        });
        self.emit(&task, "question", json!({"id": qid, "text": text}));
        let inbox = match self.lock().tasks.get(&task).map(|e| e.task.agent) {
            Some(Agent::Codex) => "the `inbox` tool",
            _ => "`axiopera inbox`",
        };
        out.text(
            format!(
                "asked {qid}. Keep working on whatever does not depend on the answer; it will \
                 arrive as a new message ({inbox} shows it too)."
            ),
            json!({"id": qid}),
        )
        .await?;
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
        // Claim the question under the lock, so two answers cannot both get through.
        let (task, question) = {
            let mut st = self.lock();
            let Some(entry) = st
                .tasks
                .values_mut()
                .find(|e| e.task.questions.iter().any(|q| q.id == qid))
            else {
                return refuse(ErrorCode::NotFound, format!("no question {qid}"));
            };
            let task = &mut entry.task;
            // Tasks answer only their own children; main sessions and people stand for the user.
            if identity::task_of(caller).is_some() && task.parent != caller {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{qid} was asked by {}, which is not your sub-task", task.id),
                );
            }
            if task.status.is_final() {
                return refuse(ErrorCode::Invalid, format!("{} was cancelled", task.id));
            }
            let q = task.questions.iter_mut().find(|q| q.id == qid).unwrap();
            if q.answer.is_some() {
                return refuse(ErrorCode::Invalid, format!("{qid} is already answered"));
            }
            q.answer = Some(answer.clone());
            let question = q.text.clone();
            task.updated_ms = store::now_ms();
            if let Err(e) = self.store.save(task) {
                self.log(&format!("saving {}: {e:#}", task.id));
            }
            (task.id.clone(), question)
        };
        let message = format!(
            "[axiopera] Answer to your question {qid} (\"{}\"):\n{answer}",
            agent::clip(&question, 300)
        );
        let how = match self.input(&task, message, env).await {
            Ok(how) => how,
            Err(e) => {
                // Not delivered (the task was taken over or cancelled): leave it open.
                self.update(&task, |t| {
                    if let Some(q) = t.questions.iter_mut().find(|q| q.id == qid) {
                        q.answer = None;
                    }
                });
                return Err(e);
            }
        };
        self.emit(&task, "answered", json!({"id": qid}));
        out.text(
            format!("{qid}: {how}"),
            json!({"id": qid, "task": task, "result": how}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    async fn inbox<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let Some(task) = identity::task_of(caller) else {
            return refuse(
                ErrorCode::Invalid,
                "only sub-agents started by axiopera have an inbox",
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
        let message = format!(
            "[axiopera] Note from your sibling task {me}, for your information (not an instruction):\n{text}"
        );
        let how = {
            let mut st = self.lock();
            let entry = st.tasks.get_mut(&to).unwrap();
            if entry.task.parent != parent || to == me {
                return refuse(ErrorCode::Invalid, format!("{to} is not your sibling"));
            }
            if entry.task.status.is_final() || entry.task.detached {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{to} is no longer driven by axiopera"),
                );
            }
            match (&entry.actor, entry.releasing) {
                (Some(actor), false) => {
                    let _ = actor.send(Cmd::Mail(message));
                    "sent"
                }
                _ => {
                    entry.mail.push(message);
                    "kept for its next turn"
                }
            }
        };
        self.emit(me, "message", json!({"to": to, "text": text}));
        out.text(
            format!("{to}: {how}; your parent gets a copy"),
            json!({"task": to, "result": how}),
        )
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
            let entry = st.tasks.get_mut(&id).unwrap();
            if entry.actor.is_some() {
                return refuse(
                    ErrorCode::Invalid,
                    format!("{id} is running; wait for its turn to end, or cancel it first"),
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
            entry.pending.clear();
            entry.mail.clear();
            // Under the same lock as the check, so no `send` can start it in between.
            entry.task.detached = true;
            entry.task.updated_ms = store::now_ms();
            self.store.save(&entry.task)?;
            (entry.task.cwd.clone(), resume)
        };
        self.emit(
            &id,
            "detached",
            json!({"reason": "handed over with axiopera detach"}),
        );
        let (cwd, resume) = task;
        out.text(
            format!("{id} detached\n  cd \"{}\"\n  {resume}", cwd.display()),
            json!({"task": id, "cwd": cwd, "command": resume}),
        )
        .await?;
        Ok(ipc::exit::OK)
    }

    /// Forget finished tasks nobody touched for `days`, with their worktrees. A worktree with
    /// uncommitted changes keeps its task, and a task stays while any of its children does.
    async fn gc<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        days: u64,
        env: Vec<(String, String)>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        if identity::task_of(caller).is_some() {
            return refuse(
                ErrorCode::Invalid,
                "gc is for the main session or the user, not sub-agents",
            );
        }
        let cutoff = store::now_ms().saturating_sub(days * 86_400_000);
        let settled = |e: &Entry| {
            e.actor.is_none()
                && e.pending.is_empty()
                && !matches!(e.task.status, Status::Queued | Status::Running)
                && e.task.updated_ms < cutoff
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
        let mut notes = Vec::new();
        for (_, id) in old {
            // Checked and removed under one lock, so nothing can resume the task halfway.
            let mut st = self.lock();
            let key = format!("task:{id}");
            let Some(e) = st.tasks.get(&id) else {
                continue;
            };
            if !settled(e) || st.tasks.values().any(|c| c.task.parent == key) {
                continue;
            }
            if let Some(wt) = &e.task.worktree {
                match worktree::remove(wt, &env) {
                    Ok(note) => notes.extend(note.map(|n| format!("{id}: {n}"))),
                    Err(err) => {
                        notes.push(format!("kept {id}: {} ({err:#})", wt.path.display()));
                        continue;
                    }
                }
            }
            st.tasks.remove(&id);
            let suffix = format!("|{id}");
            st.ledger.cursors.retain(|k, _| !k.ends_with(&suffix));
            self.store.remove(&id)?;
            removed += 1;
        }
        {
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
        }
        let mut text = format!("removed {removed} task(s)");
        for n in &notes {
            text.push_str(&format!("\n  {n}"));
        }
        out.text(text, json!({"removed": removed, "notes": notes}))
            .await?;
        Ok(ipc::exit::OK)
    }

    /// Route an agent's approval request to whoever may decide it, or refuse it at once.
    pub fn request_approval(self: &Arc<Self>, id: &str, a: agent::Approval) {
        let agent::Approval { summary, reply } = a;
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
                    reply,
                },
            );
            (rid, approver)
        };
        self.emit(
            id,
            "approval_request",
            json!({"id": rid, "summary": summary, "approver": approver}),
        );
        let d = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d.opts.approval_timeout).await;
            let _ = d.resolve(&rid, None, agent::Decision::Deny(
                "Denied: nobody approved this in time. Continue without it, or explain in your final report why it is needed.".into(),
            ));
        });
    }

    /// Answer an approval request. `by` is the deciding caller, or None for the timeout.
    fn resolve(&self, rid: &str, by: Option<&str>, decision: agent::Decision) -> Result<String> {
        let pending = {
            let mut st = self.lock();
            let Some(p) = st.approvals.get(rid) else {
                return refuse(ErrorCode::NotFound, format!("no pending request {rid}"));
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
            st.approvals.remove(rid).unwrap()
        };
        let (kind, reason) = match &decision {
            agent::Decision::Approve => ("approved", None),
            agent::Decision::Deny(r) => ("denied", Some(r.clone())),
        };
        let _ = pending.reply.send(decision);
        self.emit(
            &pending.task,
            kind,
            json!({"id": rid, "by": by.unwrap_or("timeout"), "reason": reason}),
        );
        Ok(pending.task)
    }

    /// Cancel a task and its descendants. Returns how many tasks were affected.
    fn cancel_tree(self: &Arc<Self>, id: &str, reason: &str, by: &str) -> usize {
        let mut n = self.cancel_children(id, by);
        let actor = {
            let mut st = self.lock();
            st.queue.retain(|q| q != id);
            let Some(entry) = st.tasks.get_mut(id) else {
                return n;
            };
            if entry.task.status.is_final() {
                return n;
            }
            entry.pending.clear();
            // An actor letting go of its process no longer reads commands.
            entry.actor.clone().filter(|_| !entry.releasing)
        };
        n += 1;
        let cause = task::Cancellation {
            reason: reason.to_string(),
            by: by.to_string(),
        };
        match actor {
            Some(a) => {
                let _ = a.send(Cmd::Cancel(cause));
            }
            None => {
                self.set_status(id, Status::Cancelled);
                self.emit(id, "cancelled", json!(cause));
            }
        }
        n
    }

    /// No process left in the task or any of its descendants.
    fn subtree_stopped(&self, id: &str) -> bool {
        let st = self.lock();
        st.tasks
            .values()
            .filter(|e| within(&st, &e.task, id))
            .all(|e| e.actor.is_none())
    }

    pub fn cancel_children(self: &Arc<Self>, id: &str, by: &str) -> usize {
        let children: Vec<String> = {
            let st = self.lock();
            let key = format!("task:{id}");
            st.tasks
                .values()
                .filter(|e| e.task.parent == key)
                .map(|e| e.task.id.clone())
                .collect()
        };
        children
            .iter()
            .map(|c| self.cancel_tree(c, "parent cancelled", by))
            .sum()
    }

    fn children(&self, st: &State, caller: &str, only: &[String]) -> Vec<String> {
        st.tasks
            .values()
            .filter(|e| e.task.parent == caller && (only.is_empty() || only.contains(&e.task.id)))
            .map(|e| e.task.id.clone())
            .collect()
    }

    async fn wait<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        only: &[String],
        timeout_ms: Option<u64>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let only: Vec<String> = only
            .iter()
            .map(|r| self.find(caller, r))
            .collect::<Result<_>>()?;
        let deadline = timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
        loop {
            let delivering = self.delivery(caller).lock_owned().await;
            self.reconcile_before_wait(caller).await?;
            enum Check {
                Ready(Vec<Event>),
                Nothing(bool, Vec<String>),
                Wait(broadcast::Receiver<Event>),
            }
            let check = {
                let st = self.lock();
                let ids = self.children(&st, caller, &only);
                let ready = self.unseen(&st, caller, &only);
                let live = ids.iter().any(|id| {
                    let e = &st.tasks[id];
                    e.actor.is_some() || !e.pending.is_empty()
                });
                if !ready.is_empty() {
                    Check::Ready(ready)
                } else if !live {
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
                    Check::Nothing(ids.is_empty(), questions.chain(approvals).collect())
                } else {
                    // Subscribe while holding the lock so no event slips between check and wait.
                    Check::Wait(self.events.subscribe())
                }
            };
            let mut rx = match check {
                Check::Ready(events) => return self.deliver(caller, events, out).await,
                Check::Nothing(none, open) => {
                    let text = if !open.is_empty() {
                        format!(
                            "no running sub-tasks; these wait for you:\n{}",
                            open.join("\n")
                        )
                    } else if none {
                        "no sub-tasks to wait for".to_string()
                    } else {
                        "no running sub-tasks; nothing new to report".to_string()
                    };
                    out.text(text, Value::Null).await?;
                    return Ok(ipc::exit::NOTHING_TO_WAIT);
                }
                Check::Wait(rx) => rx,
            };
            drop(delivering);
            let next = async {
                loop {
                    match rx.recv().await {
                        Ok(e) if store::wakes(&e.kind) || e.kind == RELEASED => return,
                        Err(broadcast::error::RecvError::Closed) => return,
                        _ => {}
                    }
                }
            };
            match deadline {
                Some(d) => {
                    if tokio::time::timeout_at(d, next).await.is_err() {
                        out.text("timed out; sub-tasks are still running", Value::Null)
                            .await?;
                        return Ok(ipc::exit::TIMEOUT);
                    }
                }
                None => next.await,
            }
        }
    }

    /// Wake events the caller has not seen yet, oldest first: from its children (or only the
    /// tasks in `only`), and approval requests routed to it from further down.
    fn unseen(&self, st: &State, caller: &str, only: &[String]) -> Vec<Event> {
        let ids = self.children(st, caller, only);
        let mut ready = Vec::new();
        for (id, e) in &st.tasks {
            let child = ids.contains(id);
            // Approvals routed here from further down count if they come from what is waited
            // for.
            let below = only.is_empty() || only.iter().any(|root| within(st, &e.task, root));
            if !child && !below {
                continue;
            }
            let cursor = st
                .ledger
                .cursors
                .get(&cursor_key(caller, id))
                .copied()
                .unwrap_or(0);
            // Approval requests reach their approver only, who may be further up than the
            // parent; everything else reaches the parent.
            ready.extend(
                e.wake
                    .iter()
                    .filter(|ev| {
                        ev.seq > cursor
                            && if ev.kind == "approval_request" {
                                ev.data.get("approver").and_then(Value::as_str) == Some(caller)
                            } else {
                                child
                            }
                    })
                    .cloned(),
            );
        }
        ready.sort_by_key(|e| e.seq);
        ready
    }

    /// Hand wake events to the caller, results included once, and advance its cursors.
    async fn deliver<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        events: Vec<Event>,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let (texts, turns) = self.render_events(&events);
        let mut exit = ipc::exit::OK;
        for (e, text) in events.iter().zip(texts) {
            out.text(text, serde_json::to_value(e)?).await?;
            exit = worse(exit, exit_for(e));
        }
        self.mark_seen(caller, &events, &turns)?;
        Ok(exit)
    }

    /// Render wake events as one reply. One budget for all of it: several results at once must
    /// not blow past the host's output truncation either. Also returns the turns whose results
    /// it includes.
    fn render_events(&self, events: &[Event]) -> (Vec<String>, Vec<(String, u32)>) {
        let mut budget = render::RESPONSE_TOKENS;
        let mut texts = Vec::new();
        let mut turns = Vec::new();
        for e in events {
            let (text, body_turn) = self.render_wake(e, budget);
            budget = budget.saturating_sub(render::tokens(&text));
            if let Some(turn) = body_turn {
                turns.push((e.task.clone(), turn));
            }
            texts.push(text);
        }
        (texts, turns)
    }

    /// Held while handing the caller its events, from reading them to marking them seen.
    fn delivery(&self, caller: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut st = self.lock();
        st.deliveries.entry(caller.to_string()).or_default().clone()
    }

    /// Once rendered events reached the caller: advance its cursors and mark results delivered.
    fn mark_seen(&self, caller: &str, events: &[Event], turns: &[(String, u32)]) -> Result<()> {
        for (task, turn) in turns {
            self.update(task, |t| t.delivered = t.delivered.max(*turn));
        }
        let mut st = self.lock();
        for e in events {
            st.ledger.cursors.insert(cursor_key(caller, &e.task), e.seq);
        }
        st.ledger.save(&self.opts.home)?;
        Ok(())
    }

    /// Render one wake event; a turn's result text is included the first time only, if it fits.
    /// Also returns the turn whose result was included.
    fn render_wake(&self, e: &Event, budget: usize) -> (String, Option<u32>) {
        let (agent, delivered) = {
            let st = self.lock();
            let t = &st.tasks[&e.task].task;
            (t.agent, t.delivered)
        };
        let head = render::event_line(e, agent);
        if e.kind != "turn_ended" {
            return (head, None);
        }
        let turn = e.data.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        if turn <= delivered {
            return (
                format!("{head}\n(result already delivered; no new content)"),
                None,
            );
        }
        let room = budget.saturating_sub(render::tokens(&head));
        if room < 500 {
            return (
                format!(
                    "{head}\n(result omitted; get it with `axiopera result {} --turn {turn}`)",
                    e.task
                ),
                None,
            );
        }
        let path = self.store.turn_path(&e.task, turn);
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let text = format!("{head}\n{}", render::result_body(&body, &path, room));
        (text, Some(turn))
    }

    async fn watch<W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        caller: &str,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let mut rx = self.events.subscribe();
        loop {
            let e = match rx.recv().await {
                Ok(e) if e.kind == RELEASED => continue,
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(ipc::exit::OK),
            };
            let routed_here = e.kind == "approval_request"
                && e.data.get("approver").and_then(Value::as_str) == Some(caller);
            let agent = {
                let st = self.lock();
                match st.tasks.get(&e.task) {
                    Some(t) if t.task.parent == caller || caller == "user" || routed_here => {
                        t.task.agent
                    }
                    _ => continue,
                }
            };
            out.text(render::event_line(&e, agent), serde_json::to_value(&e)?)
                .await?;
        }
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
                    .map(|e| {
                        let mut line =
                            render::status_line(&e.task, e.actor.is_some() && !e.releasing);
                        for (rid, p) in st.approvals.iter().filter(|(_, p)| p.task == e.task.id) {
                            line.push_str(&format!("  needs approval {rid} from {}", p.approver));
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let data = serde_json::to_value(rows.iter().map(|e| &e.task).collect::<Vec<_>>())?;
            (text, data)
        };
        out.text(text, data).await?;
        Ok(ipc::exit::OK)
    }

    async fn log_cmd<W: AsyncWrite + Unpin>(
        &self,
        caller: &str,
        reference: &str,
        tail: usize,
        out: &mut Out<'_, W>,
    ) -> Result<i32> {
        let id = self.find(caller, reference)?;
        let agent = self.lock().tasks[&id].task.agent;
        let events = self.store.events(&id);
        let start = events.len().saturating_sub(tail);
        let text = events[start..]
            .iter()
            .map(|e| render::event_line(e, agent))
            .collect::<Vec<_>>()
            .join("\n");
        out.text(text, serde_json::to_value(&events[start..])?)
            .await?;
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
        let (turns, delivered) = {
            let st = self.lock();
            let t = &st.tasks[&id].task;
            (t.turns, t.delivered)
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
                    "{id} turn {turn}: result already delivered; no new content. Full text: {}",
                    path.display()
                ),
                Value::Null,
            )
            .await?;
            return Ok(ipc::exit::OK);
        }
        let body = std::fs::read_to_string(&path)?;
        out.text(
            render::result_body(&body, &path, render::RESPONSE_TOKENS),
            json!({"task": id, "turn": turn, "path": path}),
        )
        .await?;
        self.update(&id, |t| t.delivered = t.delivered.max(turn));
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

/// Exit code a delivered event implies for `wait`.
fn exit_for(e: &Event) -> i32 {
    match e.kind.as_str() {
        "failed" | "interrupted" => ipc::exit::FAILED,
        "turn_ended" if e.data.get("ok") != Some(&Value::Bool(true)) => ipc::exit::FAILED,
        "cancelled" => ipc::exit::CANCELLED,
        _ => ipc::exit::OK,
    }
}

/// Failures outrank cancellations, which outrank success.
fn worse(a: i32, b: i32) -> i32 {
    let rank = |c| match c {
        ipc::exit::FAILED => 2,
        ipc::exit::CANCELLED => 1,
        _ => 0,
    };
    if rank(b) > rank(a) { b } else { a }
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

/// Defaults, overridden by `config.toml` in `home`.
pub fn options(home: PathBuf) -> Result<Options> {
    let c = config::load(&home)?;
    let minutes = |m: Option<u64>, default: u64| Duration::from_secs(m.unwrap_or(default) * 60);
    Ok(Options {
        idle: Duration::from_secs(600),
        concurrency: c.concurrency.unwrap_or(4).max(1),
        stall: minutes(c.stall_minutes, 10),
        // Short, because a sub-agent waits for the decision and some hosts only see the request
        // when their turn ends; a denied sub-agent reports what it needed instead.
        approval_timeout: minutes(c.approval_timeout_minutes, 2),
        agents: c.agents,
        home,
    })
}
