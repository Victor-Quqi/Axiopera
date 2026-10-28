//! One actor per task with a live agent process: drives turns, steers, and lets go of the
//! process as soon as a turn ends with nothing else to do.

use std::collections::BTreeMap;
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
        input: &str,
    ) -> Result<Option<&'static str>> {
        let queued = !backend.in_turn();
        self.serial += 1;
        if matches!(
            backend.steer(input, Some(self.serial)).await?,
            Steer::NoActiveTurn
        ) {
            return Ok(None);
        }
        if agent == Agent::Grok {
            self.grok_unconfirmed += 1;
        } else {
            self.pending.insert(self.serial, input.to_string());
        }
        d.emit(id, "steered", json!({"queued": queued}));
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

enum Exit {
    Released,
    Failed(String),
    Cancelled(Cancellation),
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
                    Some(Cmd::Cancel(cause)) => break Exit::Cancelled(cause),
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
    match exit {
        Exit::Released => {}
        Exit::Failed(error) => {
            d.set_status(&id, Status::Failed);
            d.emit(&id, "failed", json!({"error": error}));
        }
        Exit::Cancelled(cause) => {
            d.set_status(&id, Status::Cancelled);
            d.emit(&id, "cancelled", json!(cause));
        }
        Exit::Aborted => {
            d.set_status(&id, Status::Interrupted);
            d.emit(&id, "interrupted", json!({"reason": "the daemon stopped"}));
        }
        Exit::Detached => {
            d.update(&id, |t| t.detached = true);
            d.emit(
                &id,
                "detached",
                json!({"reason": "the native session was continued outside axiopera"}),
            );
        }
    }
    d.actor_done(&id);
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
    let deadline = task.timeout_secs.map(|s| {
        let elapsed = Duration::from_millis(crate::store::now_ms().saturating_sub(task.created_ms));
        Instant::now() + Duration::from_secs(s).saturating_sub(elapsed)
    });
    let timed_out = || {
        d.cancel_children(id, "timeout");
        Cancellation {
            reason: format!("timed out after {}s", task.timeout_secs.unwrap_or(0)),
            by: "timeout".into(),
        }
    };
    d.update(id, |t| {
        t.status = Status::Running;
        t.stalled = false;
    });
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
                Some(Cmd::Cancel(cause)) => return Ok(Exit::Cancelled(cause)),
                Some(Cmd::Abort) | None => return Ok(Exit::Aborted),
            },
            _ = sleep_until(deadline) => return Ok(Exit::Cancelled(timed_out())),
        }
    };
    let mut session_file = task.session_file.as_ref().map(|f| f.path.clone());
    start_turn(d, id, &mut backend, &inputs.join("\n\n")).await?;
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
                let input = deferred.join("\n\n");
                deferred.clear();
                if backend.busy()
                    && tracking
                        .steer(d, id, task.agent, &mut backend, &input)
                        .await?
                        .is_some()
                {
                    continue;
                }
                start_turn(d, id, &mut backend, &input).await?;
                turn_open = true;
                continue;
            }
            if !backend.busy() {
                match release_or_continue(d, id, rx) {
                    Next::Turn(input) => {
                        start_turn(d, id, &mut backend, &input.join("\n\n")).await?;
                        turn_open = true;
                    }
                    Next::Stop(exit) => break exit,
                }
            }
        }
        tokio::select! {
            biased;
            ev = erx.recv() => {
                let Some(ev) = ev else {
                    break Exit::Failed("agent event stream closed".into());
                };
                last_activity = Instant::now();
                if stalled {
                    stalled = false;
                    d.update(id, |t| t.stalled = false);
                }
                match ev {
                    AgentEvent::Session { id: sid, file } => {
                        d.update(id, |t| t.session = Some(sid));
                        if file.is_some() {
                            session_file = file;
                        }
                    }
                    AgentEvent::Tool { name, detail } => d.emit(id, "tool", json!({"name": name, "detail": detail})),
                    AgentEvent::FileChanged(path) => {
                        d.update(id, |t| {
                            if !t.changed_files.contains(&path) {
                                t.changed_files.push(path.clone());
                            }
                        });
                        d.emit(id, "file_changed", json!({"path": path}));
                    }
                    AgentEvent::InputRead(message) => {
                        if tracking.pending.remove(&message).is_some() {
                            d.emit(id, "input_read", json!({}));
                        }
                    }
                    AgentEvent::Blocked(detail) => d.emit(id, "blocked", json!({"detail": detail})),
                    AgentEvent::Notice(text) => d.emit(id, "notice", json!({"text": text})),
                    AgentEvent::TurnStarted => {
                        // Unless it is the turn we have just started.
                        let turn = d
                            .lock()
                            .tasks
                            .get(id)
                            .filter(|e| !e.in_turn)
                            .map(|e| e.task.turns + 1);
                        if let Some(turn) = turn {
                            d.emit(id, "turn_started", json!({"turn": turn}));
                        }
                    }
                    AgentEvent::Approval(a) => d.request_approval(id, a),
                    AgentEvent::TurnEnded { ok, result, mut error, usage } => {
                        if !ok && let Some(message) = error.take() {
                            let hint = model_error(d, id, message);
                            tokio::pin!(hint);
                            loop {
                                tokio::select! {
                                    message = &mut hint => { error = Some(message); break; }
                                    cmd = rx.recv() => match cmd {
                                        Some(Cmd::Input(m)) => deferred.push(m.queued()),
                                        Some(Cmd::Mail(m)) => keep_mail(d, id, m),
                                        Some(Cmd::Cancel(cause)) => return Ok(cancel(&mut backend, cause).await),
                                        Some(Cmd::Abort) | None => return Ok(Exit::Aborted),
                                    },
                                    _ = sleep_until(deadline) => return Ok(cancel(&mut backend, timed_out()).await),
                                }
                            }
                        }
                        last_ok = ok;
                        turn_open = false;
                        let next = next_input(d, id, rx, &mut backend).await;
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
                            tokio::time::sleep(GROK_GRACE).await;
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
            cmd = rx.recv() => match cmd {
                Some(Cmd::Input(input)) => {
                    if backend.busy() {
                        match tracking.steer(d, id, task.agent, &mut backend, &input.text).await? {
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
                        start_turn(d, id, &mut backend, &text).await?;
                        turn_open = true;
                    }
                }
                Some(Cmd::Mail(msg)) => {
                    if backend.in_turn() && matches!(backend.steer(&msg, None).await?, Steer::Accepted) {
                        d.emit(id, "steered", json!({"chars": msg.len()}));
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
                    d.update(id, |t| t.stalled = true);
                    d.emit(id, "stalled", json!({"quiet_secs": last_activity.elapsed().as_secs()}));
                }
            }
            _ = sleep_until(deadline) => break cancel(&mut backend, timed_out()).await,
        }
    };

    backend.shutdown().await;
    if let Exit::Released = exit {
        let fp = session_file.as_deref().and_then(Fingerprint::of);
        d.update(id, |t| {
            t.session_file = fp;
            // A cancel that arrived while letting go has already settled the status.
            if !t.status.is_final() {
                t.status = if !last_ok {
                    Status::Failed
                } else if t.open_questions().next().is_some() {
                    Status::Waiting
                } else {
                    Status::Idle
                };
            }
        });
    }
    Ok(exit)
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
            catalog.render()
        ),
        _ => format!(
            "{error}\nRun `{}` to query models and supported reasoning efforts.",
            agent::models::command(agent)
        ),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Sibling notes kept for this turn go first.
async fn start_turn(d: &Daemon, id: &str, backend: &mut Backend, input: &str) -> Result<()> {
    let mut parts: Vec<String> = d
        .lock()
        .tasks
        .get_mut(id)
        .map(|e| e.mail.drain(..).collect())
        .unwrap_or_default();
    parts.push(input.to_string());
    let input = parts.join(
        "

",
    );
    backend.turn(&input).await?;
    let turn = d.lock().tasks.get(id).map_or(0, |e| e.task.turns) + 1;
    d.emit(
        id,
        "turn_started",
        json!({"turn": turn, "chars": input.len()}),
    );
    Ok(())
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
    let mut turn = 0;
    d.update(id, |t| {
        t.turns += 1;
        turn = t.turns;
    });
    d.store().save_turn(id, turn, result)?;
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
    d.emit(id, "turn_ended", json!({"unread": unread, "unconfirmed": unconfirmed, "blocked": blocked, "notices": notices, "turn": turn, "ok": ok, "error": error, "usage": usage, "chars": result.len(), "files": files, "open_questions": open}));
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
    backend: &mut Backend,
) -> Option<Next> {
    let mut inputs = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
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
fn release_or_continue(d: &Daemon, id: &str, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Next {
    let mut st = d.lock();
    let mut inputs = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            Cmd::Input(m) => inputs.push(m.queued()),
            Cmd::Mail(m) => {
                if let Some(e) = st.tasks.get_mut(id) {
                    e.mail.push(m);
                }
            }
            // Cancellation of an idle task needs no process; the daemon handles it after release.
            Cmd::Cancel(cause) => return Next::Stop(Exit::Cancelled(cause)),
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
    Exit::Cancelled(cause)
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
                let Next::Stop(Exit::Cancelled(cause)) = release_or_continue(&f.d, "test", &mut rx)
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
        let Next::Turn(inputs) = release_or_continue(&f.d, "test", &mut rx) else {
            panic!("lost inputs")
        };
        assert_eq!(inputs, ["first", "second"]);
        for reply in replies {
            assert_eq!(reply.await.unwrap(), "queued for a new turn");
        }
        assert!(!f.d.lock().tasks["test"].releasing);
        assert!(matches!(
            release_or_continue(&f.d, "test", &mut rx),
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
        assert!(line.contains("this result predates 2 parent message(s)"));
        assert!(line.contains("axiopera log test"));
        assert!(line.ends_with("; the sandbox is off"), "{line}");
        let next = super::super::render::event_line(&events[1], Agent::Codex);
        assert!(!next.contains("sandbox"), "{next}");
    }
}
