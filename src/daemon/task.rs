//! One actor per task with a live agent process: drives turns, steers, and lets go of the
//! process as soon as a turn ends with nothing else to do.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::{Daemon, agent_env};
use crate::agent::{self, AgentEvent, Backend, StartSpec, Steer};
use crate::ipc::Agent;
use crate::store::{Fingerprint, Status};

impl Daemon {
    pub(super) fn file_changed(&self, id: &str, path: &str) -> Result<()> {
        let mut st = self.lock();
        let Some(entry) = st.tasks.get_mut(id) else {
            return Err(anyhow!("no task {id}"));
        };
        if entry.task.changed_files.iter().any(|p| p == path) {
            return Ok(());
        }
        let updated = entry.task.updated_ms;
        entry.task.changed_files.push(path.to_string());
        entry.task.updated_ms = crate::store::now_ms();
        if let Err(e) = self.store.save(&entry.task) {
            entry.task.changed_files.pop();
            entry.task.updated_ms = updated;
            return Err(e);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum Cmd {
    /// A message from the parent: steer the running turn, or start the next one.
    Input(Input),
    /// A note from a sibling: steer the running turn, or keep it for the next one.
    Mail(String),
    Cancel(Cancellation),
    /// The daemon is going away; the task can be resumed later.
    Abort,
}

#[derive(Debug, serde::Serialize)]
pub struct Cancellation {
    pub reason: String,
    pub by: String,
    #[serde(skip)]
    pub interrupted: bool,
}

#[derive(Debug)]
pub struct Input {
    pub text: String,
    pub reply: oneshot::Sender<&'static str>,
}

impl Input {
    fn queued(self) -> String {
        let _ = self.reply.send("queued for a new turn");
        self.text
    }
}

#[derive(Default)]
struct InputTracking {
    pending: BTreeMap<u64, String>,
    serial: u64,
    grok_unconfirmed: usize,
}

impl InputTracking {
    async fn steer(
        &mut self,
        d: &Daemon,
        id: &str,
        agent: Agent,
        backend: &mut Backend,
        inputs: &[String],
    ) -> Result<Option<&'static str>> {
        let input = inputs.join("\n\n");
        let queued = !backend.in_turn();
        self.serial += 1;
        if matches!(
            backend.steer(&input, Some(self.serial)).await?,
            Steer::NoActiveTurn
        ) {
            return Ok(None);
        }
        if agent == Agent::Grok {
            self.grok_unconfirmed += 1;
        } else {
            self.pending.insert(self.serial, input.to_string());
        }
        d.emit_with(id, "steered", json!({"queued": queued}), Some(&input))?;
        delivered(d, id, inputs, &[])?;
        Ok(Some(if queued {
            "queued for a following turn"
        } else {
            "sent to its running turn"
        }))
    }
}

// Grok broadcasts interjections on admission, before the model consumes them. Its final
// drain and background wake can race turn_completed, with no consumed notification.
const GROK_GRACE: Duration = Duration::from_secs(1);

/// Characters of an agent's message kept in its `said` event; the whole of the last one is the
/// turn's result.
const SAID: usize = 500;

enum Exit {
    Released,
    Failed(String),
    Stopped(Cancellation),
    Aborted,
    Detached,
}

pub async fn run(d: Arc<Daemon>, id: String, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let exit = match drive(&d, &id, &mut rx).await {
        Ok(exit) => exit,
        Err(e) => Exit::Failed(format!("{e:#}")),
    };
    let exit = if let Exit::Failed(error) = exit {
        let query = model_error(&d, &id, error);
        tokio::pin!(query);
        loop {
            tokio::select! {
                error = &mut query => break Exit::Failed(error),
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Cancel(cause)) => break Exit::Stopped(cause),
                    Some(Cmd::Abort) | None => break Exit::Aborted,
                    Some(Cmd::Input(m)) => {
                        if let Some(entry) = d.lock().tasks.get_mut(&id) { entry.pending.push(m.queued()); }
                    }
                    Some(Cmd::Mail(m)) => keep_mail(&d, &id, m),
                },
            }
        }
    } else {
        exit
    };
    if let Err(e) = finish_exit(&d, &id, exit) {
        d.log(&format!("finishing {id}: {e:#}"));
    }
    d.actor_done(&id);
}

fn finish_exit(d: &Daemon, id: &str, exit: Exit) -> Result<()> {
    let released = matches!(exit, Exit::Released);
    let detached = matches!(exit, Exit::Detached);
    let (status, event) = match exit {
        Exit::Released => (None, None),
        Exit::Failed(error) => (
            Some(Status::Failed),
            Some(("failed", json!({"error": error}))),
        ),
        Exit::Stopped(cause) => {
            let (status, kind) = if cause.interrupted {
                (Status::Interrupted, "interrupted")
            } else {
                (Status::Cancelled, "cancelled")
            };
            (Some(status), Some((kind, json!(cause))))
        }
        Exit::Aborted => (
            Some(Status::Interrupted),
            Some(("interrupted", json!({"reason": "the daemon stopped"}))),
        ),
        Exit::Detached => (
            None,
            Some((
                "detached",
                json!({"reason": "the native session was continued outside axiopera"}),
            )),
        ),
    };
    {
        let mut st = d.lock();
        let entry = st
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("task vanished"))?;
        let mut task = entry.task.clone();
        // Input accepted after a stop belongs to the next actor, not this exit.
        let resumed = entry.releasing && task.status == Status::Queued && !entry.pending.is_empty();
        if let Some(status) = status
            && !resumed
            && !task.status.is_stopped()
        {
            task.status = status;
        }
        if detached {
            task.detached = true;
        }
        task.updated_ms = crate::store::now_ms();
        let saved = if released {
            Ok(())
        } else {
            d.store.save(&task)
        };
        entry.releasing = true;
        entry.mail = entry.task.pending_mail.clone();
        if !released && !resumed {
            entry.pending.clear();
        }
        saved?;
        entry.task = task;
    }
    if let Some((kind, data)) = event {
        d.try_emit(id, kind, data)?;
    }
    Ok(())
}

async fn drive(d: &Arc<Daemon>, id: &str, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<Exit> {
    let (task, env, mut inputs) = {
        let mut st = d.lock();
        let e = st
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("task vanished"))?;
        let inputs = e.pending.drain(..).collect::<Vec<_>>();
        (e.task.clone(), e.env.clone(), inputs)
    };
    let Some(env) = env else {
        return Ok(Exit::Failed(
            "caller environment lost after daemon restart; re-send from outside the agent sandbox"
                .into(),
        ));
    };
    if let Some(fp) = &task.session_file
        && Fingerprint::of(&fp.path).as_ref() != Some(fp)
    {
        return Ok(Exit::Detached);
    }
    let exe = agent::resolve_exe(
        task.agent,
        d.opts.agents.get(task.agent).path.as_deref(),
        &env,
    )?;
    let spec = StartSpec {
        task_id: id.to_string(),
        agent: task.agent,
        tier: task.tier,
        cwd: task.cwd.clone(),
        model: task.model.clone(),
        effort: task.effort.clone(),
        env: agent_env(&env, id, task.agent, &d.opts.home),
        resume: task.session.clone(),
        trace: std::env::var_os("AXIOPERA_TRACE_PROTOCOL")
            .filter(|v| v == "1")
            .map(|_| d.opts.home.join("tasks").join(id).join("protocol.log")),
    };
    let deadline = execution_deadline(task.timeout_secs);
    let timed_out = || {
        d.interrupt_children(id);
        Cancellation {
            reason: format!("timed out after {}s", task.timeout_secs.unwrap_or(0)),
            by: "timeout".into(),
            interrupted: true,
        }
    };
    begin_execution(d, id)?;
    let (etx, mut erx) = mpsc::unbounded_channel();
    // Starting can hang (an MCP server, a slow handshake): keep honouring cancel and the
    // deadline. Dropping the half-started backend kills its process.
    let start = Backend::start(&spec, &exe, etx);
    tokio::pin!(start);
    let mut backend = loop {
        tokio::select! {
            b = &mut start => break b?,
            cmd = rx.recv() => match cmd {
                Some(Cmd::Input(m)) => inputs.push(m.queued()),
                Some(Cmd::Mail(m)) => keep_mail(d, id, m),
                Some(Cmd::Cancel(cause)) => return Ok(Exit::Stopped(cause)),
                Some(Cmd::Abort) | None => return Ok(Exit::Aborted),
            },
            _ = sleep_until(deadline) => return Ok(Exit::Stopped(timed_out())),
        }
    };
    let mut session_file = task.session_file.as_ref().map(|f| f.path.clone());
    let mut waiting = VecDeque::new();
    let outcome = async {
    macro_rules! request {
        ($future:expr) => {
            match wait_request($future, rx, &mut waiting, deadline, &timed_out).await {
                Ok(result) => result,
                Err(Exit::Stopped(cause)) => return Ok(cancel(&mut backend, cause).await),
                Err(exit) => {
                    backend.kill();
                    return Ok(exit);
                }
            }
        };
    }
    request!(start_turn(d, id, &mut backend, &inputs))?;
    let mut last_activity = Instant::now();
    let mut stalled = false;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let mut last_ok = true;
    let mut tracking = InputTracking::default();
    let mut deferred = Vec::new();
    let mut turn_open = true;

    let exit = loop {
        if !turn_open && erx.is_empty() {
            if !deferred.is_empty() {
                let input = std::mem::take(&mut deferred);
                if backend.busy()
                    && request!(tracking.steer(d, id, task.agent, &mut backend, &input))?
                        .is_some()
                {
                    continue;
                }
                request!(start_turn(d, id, &mut backend, &input))?;
                turn_open = true;
                continue;
            }
            if !backend.busy() {
                match release_or_continue(d, id, rx, &mut waiting) {
                    Next::Turn(input) => {
                        request!(start_turn(d, id, &mut backend, &input))?;
                        turn_open = true;
                    }
                    Next::Stop(exit) => break exit,
                }
            }
        }
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => break cancel(&mut backend, timed_out()).await,
            ev = erx.recv() => {
                let Some(ev) = ev else {
                    break Exit::Failed("agent event stream closed".into());
                };
                last_activity = Instant::now();
                if stalled {
                    stalled = false;
                    d.try_update(id, |t| t.stalled = false)?;
                }
                match ev {
                    AgentEvent::Session { id: sid, file } => {
                        d.try_update(id, |t| t.session = Some(sid))?;
                        if file.is_some() {
                            session_file = file;
                        }
                    }
                    AgentEvent::Tool { id: call, name, detail } => d.try_emit(id, "tool", json!({"id": call, "name": name, "detail": detail}))?,
                    AgentEvent::ToolUpdated { id: call, name, detail } => d.try_emit(id, "tool_updated", json!({"id": call, "name": name, "detail": detail}))?,
                    AgentEvent::Said(text) => d.try_emit(id, "said", json!({"text": agent::clip(&text, SAID)}))?,
                    AgentEvent::FileChanged(path) => {
                        d.file_changed(id, &path)?;
                        d.try_emit(id, "file_changed", json!({"path": path}))?;
                    }
                    AgentEvent::InputRead(message) => {
                        if tracking.pending.remove(&message).is_some() {
                            d.try_emit(id, "input_read", json!({}))?;
                        }
                    }
                    AgentEvent::Blocked(detail) => d.try_emit(id, "blocked", json!({"detail": detail}))?,
                    AgentEvent::Notice(text) => d.try_emit(id, "notice", json!({"text": text}))?,
                    AgentEvent::TurnStarted => {
                        // Unless it is the turn we have just started.
                        let turn = d
                            .lock()
                            .tasks
                            .get(id)
                            .filter(|e| !e.in_turn)
                            .map(|e| e.task.turns + 1);
                        if let Some(turn) = turn {
                            d.try_emit(id, "turn_started", json!({"turn": turn}))?;
                        }
                    }
                    AgentEvent::Approval(a) => d.request_approval(id, a),
                    AgentEvent::TurnEnded { ok, result, mut error, usage } => {
                        if !ok && let Some(message) = error.take() {
                            error = Some(request!(model_error(d, id, message)));
                        }
                        last_ok = ok;
                        turn_open = false;
                        let next = next_input(d, id, rx, &mut waiting, &mut backend).await;
                        let mut stop = None;
                        match next {
                            Some(Next::Turn(input)) => deferred.extend(input),
                            Some(Next::Stop(exit)) => stop = Some(exit),
                            None => {}
                        }
                        let unread = tracking.pending.len() + deferred.len();
                        end_turn(d, id, ok, &result, error, usage, (unread, tracking.grok_unconfirmed))?;
                        tracking.grok_unconfirmed = 0;
                        if let Some(exit) = stop {
                            break exit;
                        }
                        // Codex may persist late inputs during turn cleanup without sampling
                        // again. Explicitly start a follow-up for inputs with no model response.
                        if task.agent == Agent::Codex {
                            deferred.splice(0..0, std::mem::take(&mut tracking.pending).into_values());
                        }
                        if task.agent == Agent::Grok {
                            request!(tokio::time::sleep(GROK_GRACE));
                        }
                    }
                    AgentEvent::Exited(msg) => {
                        let tail = backend.stderr_tail();
                        break Exit::Failed(if tail.is_empty() {
                            msg
                        } else {
                            format!("{msg}: {tail}")
                        });
                    }
                }
            }
            cmd = async {
                match waiting.pop_front() {
                    Some(cmd) => Some(cmd),
                    None => rx.recv().await,
                }
            } => match cmd {
                Some(Cmd::Input(input)) => {
                    if backend.busy() {
                        match request!(tracking.steer(d, id, task.agent, &mut backend, std::slice::from_ref(&input.text)))? {
                            Some(how) => {
                                let _ = input.reply.send(how);
                            }
                            None => deferred.push(input.queued()),
                        }
                    } else if turn_open {
                        // The adapter has ended the turn, but its queued TurnEnded must be
                        // handled before any new turn can consume this input.
                        deferred.push(input.queued());
                    } else {
                        let text = input.queued();
                        request!(start_turn(d, id, &mut backend, &[text]))?;
                        turn_open = true;
                    }
                }
                Some(Cmd::Mail(msg)) => {
                    if backend.in_turn() && matches!(request!(backend.steer(&msg, None))?, Steer::Accepted) {
                        d.emit_with(id, "steered", json!({"chars": msg.len()}), Some(&msg))?;
                        delivered(d, id, &[], &[msg])?;
                    } else {
                        keep_mail(d, id, msg);
                    }
                }
                Some(Cmd::Cancel(cause)) => break cancel(&mut backend, cause).await,
                Some(Cmd::Abort) | None => {
                    backend.kill();
                    break Exit::Aborted;
                }
            },
            _ = tick.tick() => {
                if backend.busy() && !stalled && last_activity.elapsed() >= d.stall_after() {
                    stalled = true;
                    d.try_update(id, |t| t.stalled = true)?;
                    d.try_emit(id, "stalled", json!({"quiet_secs": last_activity.elapsed().as_secs()}))?;
                }
            }
        }
    };

    if let Exit::Released = exit { settle_release(d, id, last_ok)?; }
    Ok(exit)
    }.await;
    backend.shutdown().await;
    // Session announcements can precede a first turn whose admission never replies.
    while let Ok(event) = erx.try_recv() {
        if let AgentEvent::Session { id: sid, file } = event {
            d.try_update(id, |t| t.session = Some(sid))?;
            if file.is_some() {
                session_file = file;
            }
        }
    }
    record_session(d, id, session_file.as_deref())?;
    outcome
}

/// Keep control commands live while a backend request holds the actor. Data commands
/// retain their reply channels and order until the request completes.
async fn wait_request<T>(
    request: impl Future<Output = T>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    waiting: &mut VecDeque<Cmd>,
    deadline: Option<Instant>,
    timed_out: impl FnOnce() -> Cancellation,
) -> std::result::Result<T, Exit> {
    tokio::pin!(request);
    loop {
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => return Err(Exit::Stopped(timed_out())),
            cmd = rx.recv() => match cmd {
                Some(Cmd::Cancel(cause)) => return Err(Exit::Stopped(cause)),
                Some(Cmd::Abort) | None => return Err(Exit::Aborted),
                Some(cmd) => waiting.push_back(cmd),
            },
            result = &mut request => return Ok(result),
        }
    }
}

fn settle_release(d: &Daemon, id: &str, last_ok: bool) -> Result<()> {
    d.try_update(id, |t| {
        // Cancellation or timeout can settle the task while its process is releasing.
        if !t.status.is_stopped() {
            t.status = if !last_ok {
                Status::Failed
            } else if t.open_questions().next().is_some() {
                Status::Waiting
            } else {
                Status::Idle
            };
        }
    })
}

fn begin_execution(d: &Daemon, id: &str) -> Result<()> {
    d.try_update(id, |t| {
        t.status = Status::Running;
        t.stalled = false;
        // Startup can write the session before it reports a path or completes its handshake.
        t.session_file = None;
    })
}

fn record_session(d: &Daemon, id: &str, path: Option<&std::path::Path>) -> Result<()> {
    let fingerprint = path.and_then(Fingerprint::of);
    d.try_update(id, |t| t.session_file = fingerprint)
}

async fn model_error(d: &Daemon, id: &str, error: String) -> String {
    if !agent::models::selection_error(&error) {
        return error;
    }
    let context = d
        .lock()
        .tasks
        .get(id)
        .map(|entry| (entry.task.agent, entry.task.cwd.clone(), entry.env.clone()));
    let Some((agent, cwd, env)) = context else {
        return error;
    };
    let catalog = match env {
        Some(env) => d
            .model_catalog(agent, &cwd, &env, Duration::from_secs(5))
            .await
            .ok(),
        None => None,
    };
    match catalog {
        Some(catalog) if !catalog.models.is_empty() => format!(
            "{error}\n\n{}:\n{}",
            agent::models::command(agent),
            catalog.render(agent)
        ),
        _ => format!(
            "{error}\nRun `{}` to query models and supported reasoning efforts.",
            agent::models::command(agent)
        ),
    }
}

fn execution_deadline(timeout_secs: Option<u64>) -> Option<Instant> {
    timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs))
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Sibling notes kept for this turn go first.
async fn start_turn(d: &Daemon, id: &str, backend: &mut Backend, inputs: &[String]) -> Result<()> {
    let mail = d
        .lock()
        .tasks
        .get(id)
        .map(|e| e.mail.clone())
        .unwrap_or_default();
    let input = mail
        .iter()
        .chain(inputs)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    backend.turn(&input).await?;
    let turn = d.lock().tasks.get(id).map_or(0, |e| e.task.turns) + 1;
    d.emit_with(
        id,
        "turn_started",
        json!({"turn": turn, "chars": input.len()}),
        Some(&input),
    )?;
    delivered(d, id, inputs, &mail)?;
    if let Some(e) = d.lock().tasks.get_mut(id) {
        e.mail.clear();
    }
    Ok(())
}

// Backend admission and saving this removal cannot be atomic: a crash between them replays input.
fn delivered(d: &Daemon, id: &str, inputs: &[String], mail: &[String]) -> Result<()> {
    d.try_update(id, |task| {
        for (queue, sent) in [
            (&mut task.pending_inputs, inputs),
            (&mut task.pending_mail, mail),
        ] {
            for text in sent {
                if let Some(i) = queue.iter().position(|item| item == text) {
                    queue.remove(i);
                }
            }
        }
    })
}

fn end_turn(
    d: &Daemon,
    id: &str,
    ok: bool,
    result: &str,
    error: Option<String>,
    usage: Option<serde_json::Value>,
    messages: (usize, usize),
) -> Result<()> {
    let (unread, unconfirmed) = messages;
    let turn = d
        .lock()
        .tasks
        .get(id)
        .ok_or_else(|| anyhow!("task vanished"))?
        .task
        .turns
        + 1;
    d.store().save_turn(id, turn, result)?;
    d.try_update(id, |t| t.turns = turn)?;
    let (files, open) = d.lock().tasks.get(id).map_or((0, Vec::new()), |e| {
        let open: Vec<String> = e.task.open_questions().map(|q| q.id.clone()).collect();
        (e.task.changed_files.len(), open)
    });
    let (blocked, notices) = {
        let mut st = d.lock();
        st.tasks.get_mut(id).map_or((0, Vec::new()), |e| {
            (
                std::mem::take(&mut e.blocked),
                std::mem::take(&mut e.notices),
            )
        })
    };
    d.try_emit(id, "turn_ended", json!({"unread": unread, "unconfirmed": unconfirmed, "blocked": blocked, "notices": notices, "turn": turn, "ok": ok, "error": error, "usage": usage, "chars": result.len(), "files": files, "open_questions": open}))?;
    Ok(())
}

enum Next {
    Turn(Vec<String>),
    Stop(Exit),
}

fn keep_mail(d: &Daemon, id: &str, message: String) {
    if let Some(e) = d.lock().tasks.get_mut(id) {
        e.mail.push(message);
    }
}

/// Commands that arrived while the turn was ending.
async fn next_input(
    d: &Daemon,
    id: &str,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    waiting: &mut VecDeque<Cmd>,
    backend: &mut Backend,
) -> Option<Next> {
    let mut inputs = Vec::new();
    while let Some(cmd) = waiting.pop_front().or_else(|| rx.try_recv().ok()) {
        match cmd {
            Cmd::Input(m) => inputs.push(m.queued()),
            Cmd::Mail(m) => keep_mail(d, id, m),
            Cmd::Cancel(cause) => return Some(Next::Stop(cancel(backend, cause).await)),
            Cmd::Abort => {
                backend.kill();
                return Some(Next::Stop(Exit::Aborted));
            }
        }
    }
    (!inputs.is_empty()).then_some(Next::Turn(inputs))
}

/// Under the daemon lock, either pick up input that slipped in, or mark the task as releasing so
/// that later input waits for a fresh process.
fn release_or_continue(
    d: &Daemon,
    id: &str,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    waiting: &mut VecDeque<Cmd>,
) -> Next {
    let mut st = d.lock();
    let mut inputs = Vec::new();
    while let Some(cmd) = waiting.pop_front().or_else(|| rx.try_recv().ok()) {
        match cmd {
            Cmd::Input(m) => inputs.push(m.queued()),
            Cmd::Mail(m) => {
                if let Some(e) = st.tasks.get_mut(id) {
                    e.mail.push(m);
                }
            }
            // Cancellation of an idle task needs no process; the daemon handles it after release.
            Cmd::Cancel(cause) => return Next::Stop(Exit::Stopped(cause)),
            Cmd::Abort => return Next::Stop(Exit::Aborted),
        }
    }
    if !inputs.is_empty() {
        return Next::Turn(inputs);
    }
    if let Some(e) = st.tasks.get_mut(id) {
        e.releasing = true;
    }
    Next::Stop(Exit::Released)
}

async fn cancel(backend: &mut Backend, cause: Cancellation) -> Exit {
    let _ = tokio::time::timeout(Duration::from_secs(2), backend.interrupt()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    backend.kill();
    Exit::Stopped(cause)
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[tokio::test]
    async fn missing_query_context_keeps_the_error_and_adds_the_command() {
        let f = fixture();
        let error = "Unknown model 'missing'";
        let message = model_error(&f.d, "test", error.into()).await;
        assert!(message.starts_with(error));
        assert!(message.contains("axiopera models codex"));
        let auth = "model does not exist or you do not have access to it";
        assert_eq!(model_error(&f.d, "test", auth.into()).await, auth);
        f.d.lock().tasks.get_mut("test").unwrap().env = Some(Vec::new());
        assert!(
            model_error(&f.d, "test", error.into())
                .await
                .contains("axiopera models codex")
        );
    }

    #[test]
    fn cancellation_keeps_its_author_while_running_or_releasing() {
        for releasing in [false, true] {
            let f = fixture();
            let (tx, mut rx) = mpsc::unbounded_channel();
            {
                let mut st = f.d.lock();
                let entry = st.tasks.get_mut("test").unwrap();
                entry.actor = Some(tx);
                entry.releasing = releasing;
            }
            f.d.cancel_tree("test", "cancelled by request", "codex:host");
            if releasing {
                assert_eq!(
                    f.d.lock().tasks["test"].deliverable[0].data["by"],
                    "codex:host"
                );
                assert!(rx.try_recv().is_err());
            } else {
                let Next::Stop(Exit::Stopped(cause)) =
                    release_or_continue(&f.d, "test", &mut rx, &mut VecDeque::new())
                else {
                    panic!("lost cancellation");
                };
                assert_eq!(cause.by, "codex:host");
                assert_eq!(cause.reason, "cancelled by request");
            }
        }
    }

    #[tokio::test]
    async fn two_inputs_at_release_stay_ordered_and_new_input_survives_shutdown() {
        let f = fixture();
        let (tx, mut rx) = mpsc::unbounded_channel();
        f.d.lock().tasks.get_mut("test").unwrap().actor = Some(tx.clone());
        let mut replies = Vec::new();
        for text in ["first", "second"] {
            let (reply, receive) = oneshot::channel();
            tx.send(Cmd::Input(Input {
                text: text.into(),
                reply,
            }))
            .unwrap();
            replies.push(receive);
        }
        let Next::Turn(inputs) = release_or_continue(&f.d, "test", &mut rx, &mut VecDeque::new())
        else {
            panic!("lost inputs")
        };
        assert_eq!(inputs, ["first", "second"]);
        for reply in replies {
            assert_eq!(reply.await.unwrap(), "queued for a new turn");
        }
        assert!(!f.d.lock().tasks["test"].releasing);
        assert!(matches!(
            release_or_continue(&f.d, "test", &mut rx, &mut VecDeque::new()),
            Next::Stop(Exit::Released)
        ));
        assert_eq!(
            f.d.input("codex:host", "test", "during shutdown".into(), None, None)
                .await
                .unwrap(),
            "queued for a new turn"
        );
        {
            let mut st = f.d.lock();
            st.running = 1;
            // Keeps the next turn from starting a real agent here.
            st.stopping = true;
        }
        f.d.actor_done("test");
        let st = f.d.lock();
        assert_eq!(st.tasks["test"].pending, ["during shutdown"]);
        assert!(st.tasks["test"].actor.is_none());
    }

    #[test]
    fn tool_updates_preserve_progress_count_and_latest_call() {
        let f = fixture();
        f.d.emit(
            "test",
            "tool",
            json!({"id": "a", "name": "use_tool", "detail": ""}),
        );
        let started = f.d.lock().tasks["test"].last_tool.as_ref().unwrap().1;
        f.d.emit(
            "test",
            "tool_updated",
            json!({"id": "a", "name": "web__search", "detail": "pi rpc"}),
        );
        {
            let st = f.d.lock();
            let entry = &st.tasks["test"];
            assert_eq!(entry.tool_calls, 1);
            assert_eq!(
                entry.last_tool,
                Some(("web__search: pi rpc".into(), started, "a".into()))
            );
        }
        f.d.emit(
            "test",
            "tool",
            json!({"id": "b", "name": "read_file", "detail": "src/main.rs"}),
        );
        f.d.emit(
            "test",
            "tool_updated",
            json!({"id": "a", "name": "web__search", "detail": "older"}),
        );
        let st = f.d.lock();
        assert_eq!(st.tasks["test"].tool_calls, 2);
        assert_eq!(
            st.tasks["test"].last_tool.as_ref().unwrap().0,
            "read_file: src/main.rs"
        );
    }

    #[test]
    fn turn_end_reports_late_input_and_resets_refusals() {
        let f = fixture();
        f.d.emit("test", "blocked", json!({"detail": "denied"}));
        f.d.emit("test", "notice", json!({"text": "the sandbox is off"}));
        end_turn(
            &f.d,
            "test",
            true,
            "answer before input",
            None,
            None,
            (2, 0),
        )
        .unwrap();
        end_turn(&f.d, "test", true, "answer after input", None, None, (0, 0)).unwrap();
        let st = f.d.lock();
        let events = &st.tasks["test"].deliverable;
        assert_eq!(events[0].data["unread"], 2);
        assert_eq!(events[0].data["blocked"], 1);
        assert_eq!(events[1].data["blocked"], 0);
        let line = super::super::render::event_line(&events[0], Agent::Codex);
        assert!(line.contains("this result predates 2 of your messages"));
        assert!(line.contains("axiopera log test"));
        assert!(line.ends_with("; the sandbox is off"), "{line}");
        let next = super::super::render::event_line(&events[1], Agent::Codex);
        assert!(!next.contains("sandbox"), "{next}");
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn result_is_saved_before_the_turn_is_registered() {
        let f = fixture();
        let path = f.d.store.turn_path("test", 1);
        std::fs::create_dir(path.with_extension("md.tmp")).unwrap();
        assert!(end_turn(&f.d, "test", true, "answer", None, None, (0, 0)).is_err());
        assert_eq!(f.d.lock().tasks["test"].task.turns, 0);
        assert!(f.d.lock().tasks["test"].deliverable.is_empty());
        std::fs::remove_dir(path.with_extension("md.tmp")).unwrap();
        let task_tmp = path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("task.json.tmp");
        std::fs::create_dir(&task_tmp).unwrap();
        assert!(end_turn(&f.d, "test", true, "answer", None, None, (0, 0)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "answer");
        assert_eq!(f.d.lock().tasks["test"].task.turns, 0);
        std::fs::remove_dir(task_tmp).unwrap();
        end_turn(&f.d, "test", true, "answer", None, None, (0, 0)).unwrap();
        assert_eq!(f.d.lock().tasks["test"].task.turns, 1);
    }

    #[test]
    fn admission_removes_only_the_delivered_occurrence_and_failed_save_keeps_it() {
        let f = fixture();
        f.d.try_update("test", |t| {
            t.pending_inputs = vec!["same".into(), "same".into(), "later".into()]
        })
        .unwrap();
        delivered(&f.d, "test", &["same".into()], &[]).unwrap();
        assert_eq!(
            f.d.lock().tasks["test"].task.pending_inputs,
            ["same", "later"]
        );
        let tmp =
            f.d.store
                .prompt_path("test")
                .parent()
                .unwrap()
                .join("task.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(delivered(&f.d, "test", &["same".into()], &[]).is_err());
        assert_eq!(
            f.d.lock().tasks["test"].task.pending_inputs,
            ["same", "later"]
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::super::tests::{fixture, fixture_with};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn each_execution_gets_a_full_timeout() {
        let f = fixture();
        assert_eq!(f.d.lock().tasks["test"].task.created_ms, 0);
        let first = execution_deadline(Some(30)).unwrap();
        assert_eq!(first - Instant::now(), Duration::from_secs(30));
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(
            execution_deadline(Some(30)).unwrap() - Instant::now(),
            Duration::from_secs(30)
        );
        assert!(execution_deadline(None).is_none());
    }

    #[tokio::test]
    async fn timeout_is_resumable_and_does_not_restart_old_queues() {
        let f = fixture_with(&[json!({"id":"child", "parent":"task:test"})]);
        f.d.lock().stopping = true;
        for id in ["test", "child"] {
            f.d.input("user", id, "saved input".into(), None, Some(Vec::new()))
                .await
                .unwrap();
        }
        f.d.interrupt_children("test");
        finish_exit(
            &f.d,
            "test",
            Exit::Stopped(Cancellation {
                reason: "timed out".into(),
                by: "timeout".into(),
                interrupted: true,
            }),
        )
        .unwrap();
        {
            let mut st = f.d.lock();
            st.stopping = false;
            st.running = 1;
        }
        f.d.actor_done("test");
        for id in ["test", "child"] {
            let st = f.d.lock();
            assert_eq!(st.tasks[id].task.status, Status::Interrupted);
            assert!(st.tasks[id].actor.is_none());
            assert!(st.tasks[id].pending.is_empty());
            assert_eq!(st.tasks[id].task.pending_inputs, ["saved input"]);
            assert_eq!(st.tasks[id].deliverable.last().unwrap().kind, "interrupted");
        }
        f.d.lock().stopping = true;
        f.d.input("user", "test", "resume".into(), None, Some(Vec::new()))
            .await
            .unwrap();
        assert_eq!(f.d.lock().tasks["test"].pending, ["saved input", "resume"]);
        f.d.try_cancel_tree("test", "cancelled by request", "user")
            .unwrap();
        assert_eq!(f.d.lock().tasks["test"].task.status, Status::Cancelled);
        f.d.input("user", "test", "continue after cancel".into(), None, None)
            .await
            .unwrap();
        assert_eq!(
            f.d.lock().tasks["test"].pending,
            ["saved input", "resume", "continue after cancel"]
        );
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn interruption_records_current_session_and_handshake_failure_drops_old_fingerprint() {
        let f = fixture();
        let path =
            f.d.store
                .prompt_path("test")
                .with_file_name("native-session.jsonl");
        std::fs::write(&path, "old").unwrap();
        let old = Fingerprint::of(&path).unwrap();
        f.d.try_update("test", |t| t.session_file = Some(old.clone()))
            .unwrap();
        begin_execution(&f.d, "test").unwrap();
        assert!(f.d.lock().tasks["test"].task.session_file.is_none());
        std::fs::write(&path, "old and new turn").unwrap();
        record_session(&f.d, "test", Some(&path)).unwrap();
        finish_exit(
            &f.d,
            "test",
            Exit::Stopped(Cancellation {
                reason: "timed out".into(),
                by: "timeout".into(),
                interrupted: true,
            }),
        )
        .unwrap();
        let task = f.d.lock().tasks["test"].task.clone();
        assert_eq!(task.status, Status::Interrupted);
        assert_ne!(task.session_file, Some(old));
        assert_eq!(task.session_file, Fingerprint::of(&path));
        begin_execution(&f.d, "test").unwrap();
        record_session(&f.d, "test", None).unwrap();
        finish_exit(&f.d, "test", Exit::Aborted).unwrap();
        assert!(f.d.lock().tasks["test"].task.session_file.is_none());
    }
}

#[cfg(test)]
mod release_tests {
    use super::super::tests::fixture;
    use super::*;

    #[tokio::test]
    async fn cancel_then_send_survives_actor_exit_and_repeated_cancellation() {
        for releasing in [false, true] {
            for after_exit in [false, true] {
                let f = fixture();
                let (tx, mut rx) = mpsc::unbounded_channel();
                {
                    let mut st = f.d.lock();
                    st.stopping = true;
                    st.running = 1;
                    let entry = st.tasks.get_mut("test").unwrap();
                    entry.actor = Some(tx);
                    entry.releasing = releasing;
                    entry.task.status = Status::Running;
                    entry.task.session = Some("same-session".into());
                    entry.task.pending_inputs.push("earlier".into());
                }
                f.d.try_cancel_tree("test", "cancelled by request", "user")
                    .unwrap();
                let exit = if releasing {
                    Exit::Released
                } else {
                    let Cmd::Cancel(cause) = rx.try_recv().unwrap() else {
                        panic!("expected cancellation")
                    };
                    Exit::Stopped(cause)
                };
                if after_exit {
                    finish_exit(&f.d, "test", exit).unwrap();
                    assert_eq!(f.d.lock().tasks["test"].task.status, Status::Cancelled);
                    f.d.input("user", "test", "resume".into(), None, Some(Vec::new()))
                        .await
                        .unwrap();
                } else {
                    f.d.input("user", "test", "resume".into(), None, Some(Vec::new()))
                        .await
                        .unwrap();
                    if releasing {
                        settle_release(&f.d, "test", true).unwrap();
                    }
                    finish_exit(&f.d, "test", exit).unwrap();
                }
                assert!(rx.try_recv().is_err());
                {
                    let st = f.d.lock();
                    assert_eq!(st.tasks["test"].pending, ["earlier", "resume"]);
                    assert!(!st.tasks["test"].task.status.is_stopped());
                    assert_eq!(
                        st.tasks["test"].task.session.as_deref(),
                        Some("same-session")
                    );
                }
                // A second cancel stops the queued continuation; it must not restart itself.
                f.d.try_cancel_tree("test", "cancelled again", "user")
                    .unwrap();
                f.d.actor_done("test");
                assert!(f.d.lock().tasks["test"].actor.is_none());
                assert!(f.d.lock().tasks["test"].pending.is_empty());
                assert_eq!(f.d.lock().tasks["test"].task.status, Status::Cancelled);
            }
        }
    }

    #[test]
    fn failed_exit_save_still_recovers_mail_for_release() {
        let f = fixture();
        f.d.try_update("test", |t| t.pending_mail.push("unread".into()))
            .unwrap();
        std::fs::create_dir(
            f.d.store
                .prompt_path("test")
                .with_file_name("task.json.tmp"),
        )
        .unwrap();
        assert!(finish_exit(&f.d, "test", Exit::Failed("stopped".into())).is_err());
        let st = f.d.lock();
        assert!(st.tasks["test"].releasing);
        assert_eq!(st.tasks["test"].mail, ["unread"]);
    }

    #[test]
    fn timeout_during_release_keeps_interrupted_status() {
        let f = fixture();
        let (tx, _rx) = mpsc::unbounded_channel();
        {
            let mut st = f.d.lock();
            let entry = st.tasks.get_mut("test").unwrap();
            entry.actor = Some(tx);
            entry.releasing = true;
            entry.task.status = Status::Running;
        }
        f.d.stop_tree("test", "parent timed out", "timeout", true)
            .unwrap();
        settle_release(&f.d, "test", true).unwrap();
        assert_eq!(f.d.lock().tasks["test"].task.status, Status::Interrupted);
        assert_eq!(f.d.store.load_all()[0].status, Status::Interrupted);
    }
}

#[cfg(test)]
mod request_tests {
    use super::super::tests::{Fixture, fixture};
    use super::*;
    use std::path::{Path, PathBuf};

    #[tokio::test]
    async fn send_after_cancel_starts_a_new_process_in_the_same_session() {
        let binary = fixture();
        let exe = compile_peer(&binary);
        let f = fixture();
        prepare(&f, &exe, "none", 60);
        let dir = directory(&f);
        f.d.schedule("test");
        observed(&dir.join("turn-start")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while f.d.lock().tasks["test"].task.session.is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let first_pid = std::fs::read_to_string(dir.join("pid")).unwrap();
        f.d.try_cancel_tree("test", "cancelled by request", "user")
            .unwrap();
        f.d.input("user", "test", "resume-now".into(), None, None)
            .await
            .unwrap();
        observed(&dir.join("thread-resume")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f.d.lock().tasks["test"].task.pending_inputs.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_ne!(std::fs::read_to_string(dir.join("pid")).unwrap(), first_pid);
        let requests = std::fs::read_to_string(dir.join("requests")).unwrap();
        assert!(requests.contains("thread/resume") && requests.contains("fake-session"));
        assert_eq!(requests.matches("resume-now").count(), 1);
        f.d.try_cancel_tree("test", "cancelled again", "user")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while f.d.lock().tasks["test"].actor.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(f.d.lock().tasks["test"].task.status, Status::Cancelled);
        assert_eq!(f.d.lock().running, 0);
        assert_reaped(&dir).await;
    }

    fn directory(f: &Fixture) -> PathBuf {
        f.d.store.prompt_path("test").parent().unwrap().to_owned()
    }

    fn compile_peer(f: &Fixture) -> PathBuf {
        let exe = directory(f).join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon/fixtures/timeout_agent.rs");
        let output = std::process::Command::new("rustc")
            .args(["--edition=2021", "--crate-name=timeout_agent"])
            .arg(source)
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    }

    fn prepare(f: &Fixture, exe: &Path, hang: &str, seconds: u64) {
        let dir = directory(f);
        let session = dir.join("session.jsonl");
        std::fs::write(&session, "old session\n").unwrap();
        f.d.try_update("test", |t| {
            t.cwd = dir.clone();
            t.timeout_secs = Some(seconds);
            t.pending_inputs = vec!["initial".into()];
            t.session_file = Fingerprint::of(&session);
        })
        .unwrap();
        let mut st = f.d.lock();
        let entry = st.tasks.get_mut("test").unwrap();
        entry.pending = vec!["initial".into()];
        entry.env = Some(vec![
            (
                "PATH".into(),
                exe.parent().unwrap().to_string_lossy().into_owned(),
            ),
            (
                "AXIOPERA_FAKE_HOME".into(),
                dir.to_string_lossy().into_owned(),
            ),
            ("AXIOPERA_FAKE_HANG".into(), hang.into()),
            (
                "AXIOPERA_FAKE_SESSION".into(),
                serde_json::to_string(&session).unwrap(),
            ),
        ]);
    }

    async fn observed(path: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    fn input(
        f: &Fixture,
        tx: &mpsc::UnboundedSender<Cmd>,
        text: &str,
    ) -> oneshot::Receiver<&'static str> {
        f.d.try_update("test", |t| t.pending_inputs.push(text.into()))
            .unwrap();
        let (reply, receive) = oneshot::channel();
        tx.send(Cmd::Input(Input {
            text: text.into(),
            reply,
        }))
        .unwrap();
        receive
    }

    fn mail(f: &Fixture, tx: &mpsc::UnboundedSender<Cmd>, text: &str) {
        f.d.try_update("test", |t| t.pending_mail.push(text.into()))
            .unwrap();
        tx.send(Cmd::Mail(text.into())).unwrap();
    }

    async fn assert_reaped(dir: &Path) {
        let pid: u32 = std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::Threading::{OpenProcess, WaitForSingleObject},
            };
            // SYNCHRONIZE permits checking termination without inspecting the child.
            let handle = OpenProcess(0x00100000, 0, pid);
            if !handle.is_null() {
                // TerminateJobObject returns before its processes have exited.
                let status = WaitForSingleObject(handle, 2000);
                CloseHandle(handle);
                assert_eq!(status, 0, "fake peer is still running");
            }
        }
        #[cfg(unix)]
        // Dropping a backend during its handshake lets Tokio reap the killed child asynchronously.
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(pid as i32, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake peer was not reaped");
    }

    #[tokio::test]
    async fn hung_admissions_honor_timeout_cancel_abort_and_preserve_unconfirmed_data() {
        let binary = fixture();
        let exe = compile_peer(&binary);
        for (hang, action) in [
            ("turn/start", "timeout"),
            ("turn/start", "cancel"),
            ("turn/start", "abort"),
            ("turn/steer", "timeout"),
            ("turn/steer", "cancel"),
            ("turn/steer", "abort"),
            ("mail", "abort"),
            ("initialize", "abort"),
        ] {
            let f = fixture();
            let dir = directory(&f);
            prepare(
                &f,
                &exe,
                if hang == "mail" { "turn/steer" } else { hang },
                if action == "timeout" { 2 } else { 60 },
            );
            let (tx, mut rx) = mpsc::unbounded_channel();
            let d = f.d.clone();
            let runner = tokio::spawn(async move { drive(&d, "test", &mut rx).await });
            let mut confirmation = None;
            if matches!(hang, "turn/steer" | "mail") {
                observed(&dir.join("turn-start")).await;
                if hang == "mail" {
                    mail(&f, &tx, "hanging-mail");
                } else {
                    confirmation = Some(input(&f, &tx, "hanging-input"));
                }
                observed(&dir.join("turn-steer")).await;
            } else {
                observed(&dir.join(hang.replace('/', "-"))).await;
            }
            let later = input(&f, &tx, "later");
            mail(&f, &tx, "later-mail");
            match action {
                "cancel" => tx
                    .send(Cmd::Cancel(Cancellation {
                        reason: "requested".into(),
                        by: "user".into(),
                        interrupted: false,
                    }))
                    .unwrap(),
                "abort" => tx.send(Cmd::Abort).unwrap(),
                _ => {}
            }
            let exit = tokio::time::timeout(Duration::from_secs(12), runner)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            match (&exit, action) {
                (Exit::Stopped(c), "timeout") => assert!(c.interrupted && c.by == "timeout"),
                (Exit::Stopped(c), "cancel") => assert!(!c.interrupted && c.by == "user"),
                (Exit::Aborted, "abort") => {}
                _ => panic!("unexpected exit for {hang}/{action}"),
            }
            finish_exit(&f.d, "test", exit).unwrap();
            assert_eq!(
                f.d.lock().tasks["test"].mail,
                f.d.store.load_all()[0].pending_mail
            );
            if let Some(confirmation) = confirmation {
                assert!(confirmation.await.is_err());
            }
            // During startup the existing loop acknowledges saved queueing, never admission.
            if hang == "initialize" {
                assert_eq!(later.await.unwrap(), "queued for a new turn");
            } else {
                assert!(later.await.is_err());
            }
            let saved = f.d.store.load_all().pop().unwrap();
            assert!(saved.pending_inputs.iter().any(|s| s == "later"));
            assert!(saved.pending_mail.iter().any(|s| s == "later-mail"));
            if hang == "turn/start" {
                assert!(saved.pending_inputs.iter().any(|s| s == "initial"));
            }
            if hang == "turn/steer" {
                assert!(saved.pending_inputs.iter().any(|s| s == "hanging-input"));
            }
            if hang == "mail" {
                assert!(saved.pending_mail.iter().any(|s| s == "hanging-mail"));
            }
            if hang == "initialize" {
                assert!(saved.session_file.is_none());
            } else {
                assert_eq!(saved.session.as_deref(), Some("fake-session"));
                assert_eq!(
                    saved.session_file,
                    Fingerprint::of(&dir.join("session.jsonl"))
                );
            }
            assert_reaped(&dir).await;
        }
    }

    #[tokio::test]
    async fn first_turn_leaves_unread_mail_commands_for_single_delivery() {
        let binary = fixture();
        let exe = compile_peer(&binary);
        let f = fixture();
        prepare(&f, &exe, "none", 60);
        let (task, env) = {
            let st = f.d.lock();
            (
                st.tasks["test"].task.clone(),
                st.tasks["test"].env.clone().unwrap(),
            )
        };
        let spec = StartSpec {
            task_id: task.id,
            agent: task.agent,
            tier: task.tier,
            cwd: task.cwd,
            model: None,
            effort: None,
            env,
            resume: None,
            trace: None,
        };
        let (events, _events) = mpsc::unbounded_channel();
        let mut backend = Backend::start(&spec, &exe, events).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // The backend startup completed, but the actor has not consumed the Mail command.
        mail(&f, &tx, "unread-mail");
        start_turn(&f.d, "test", &mut backend, &["initial".into()])
            .await
            .unwrap();
        assert_eq!(f.d.lock().tasks["test"].task.pending_mail, ["unread-mail"]);
        let Cmd::Mail(text) = rx.try_recv().unwrap() else {
            panic!("missing Mail command");
        };
        assert!(matches!(
            backend.steer(&text, None).await.unwrap(),
            Steer::Accepted
        ));
        delivered(&f.d, "test", &[], &[text]).unwrap();
        backend.shutdown().await;
        let requests = std::fs::read_to_string(directory(&f).join("requests")).unwrap();
        assert_eq!(requests.matches("unread-mail").count(), 1);
        assert_reaped(&directory(&f)).await;
    }

    #[tokio::test]
    async fn delayed_admission_replays_inputs_and_mail_once_in_order() {
        let binary = fixture();
        let exe = compile_peer(&binary);
        let f = fixture();
        let dir = directory(&f);
        prepare(&f, &exe, "turn/start", 60);
        f.d.try_update("test", |t| t.pending_mail.push("old-mail".into()))
            .unwrap();
        f.d.lock()
            .tasks
            .get_mut("test")
            .unwrap()
            .mail
            .push("old-mail".into());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let d = f.d.clone();
        let runner = tokio::spawn(async move { drive(&d, "test", &mut rx).await });
        observed(&dir.join("turn-start")).await;
        let first = input(&f, &tx, "first");
        mail(&f, &tx, "first-mail");
        let second = input(&f, &tx, "second");
        mail(&f, &tx, "second-mail");
        std::fs::write(dir.join("continue"), "go").unwrap();
        for received in [first, second] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), received)
                    .await
                    .unwrap()
                    .unwrap(),
                "sent to its running turn"
            );
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f.d.lock().tasks["test"].task.pending_mail.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tx.send(Cmd::Abort).unwrap();
        assert!(matches!(runner.await.unwrap().unwrap(), Exit::Aborted));
        let requests = std::fs::read_to_string(dir.join("requests")).unwrap();
        let text: Vec<String> = requests
            .lines()
            .filter_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["params"]["input"][0]["text"].as_str().map(str::to_owned)
            })
            .collect();
        assert_eq!(
            text,
            [
                "old-mail\n\ninitial",
                "first",
                "first-mail",
                "second",
                "second-mail"
            ]
        );
        assert!(f.d.lock().tasks["test"].task.pending_inputs.is_empty());
        assert_reaped(&dir).await;
    }
}
