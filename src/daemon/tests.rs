use super::*;

pub(super) struct Fixture {
    pub d: Arc<Daemon>,
    home: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.home).unwrap();
    }
}

pub(super) fn fixture() -> Fixture {
    fixture_with(&[])
}

fn options(home: &Path) -> Options {
    Options {
        home: home.to_path_buf(),
        idle: Duration::from_secs(10),
        concurrency: 50,
        stall: Duration::from_secs(60),
        approval_timeout: Duration::from_secs(60),
        agents: config::Agents::default(),
    }
}

/// The task "test", and more tasks given as changes to its fields.
pub(super) fn fixture_with(more: &[Value]) -> Fixture {
    let home = std::env::temp_dir().join(format!("axiopera-test-{}", uuid::Uuid::new_v4()));
    let base = json!({
        "id": "test", "parent": "codex:host", "agent": "codex", "tier": "read",
        "cwd": ".", "status": "idle", "turns": 0, "delivered": 0,
        "changed_files": [], "stalled": false, "detached": false,
        "created_ms": 0, "updated_ms": 0,
    });
    for changes in std::iter::once(&json!({})).chain(more) {
        let mut fields = base.clone();
        let fields_map = fields.as_object_mut().unwrap();
        fields_map.extend(changes.as_object().unwrap().clone());
        let task: Task = serde_json::from_value(fields).unwrap();
        Store::new(&home).save(&task).unwrap();
    }
    Fixture {
        d: Arc::new(Daemon::load(options(&home), None)),
        home,
    }
}

#[test]
fn blocked_is_logged_without_waking_and_counted_once_per_turn() {
    let f = fixture();
    f.d.emit("test", "blocked", json!({"detail": "denied"}));
    f.d.emit("test", "blocked", json!({"detail": "denied again"}));
    assert!(f.d.lock().tasks["test"].deliverable.is_empty());
    assert_eq!(f.d.lock().tasks["test"].blocked, 2);
    assert_eq!(f.d.store.events("test").len(), 2);
    for kind in [
        "spawned",
        "turn_started",
        "steered",
        "input_read",
        "tool",
        "blocked",
    ] {
        assert!(!store::deliverable(kind));
    }
    for kind in [
        "question",
        "approval_request",
        "stalled",
        "failed",
        "cancelled",
        "turn_ended",
    ] {
        assert!(store::deliverable(kind));
    }
}

#[tokio::test]
async fn send_changes_tier_and_preserves_the_session_across_reload() {
    for status in [
        Status::Idle,
        Status::Waiting,
        Status::Failed,
        Status::Interrupted,
    ] {
        let f = fixture();
        // Hold scheduling so this test can inspect accepted input without a real CLI.
        f.d.lock().stopping = true;
        f.d.update("test", |t| {
            t.session = Some("native-session".into());
            t.turns = 3;
            t.delivered = 2;
            t.status = status;
        });
        for tier in [ipc::Tier::Full, ipc::Tier::Work, ipc::Tier::Read] {
            let mut buf = Vec::new();
            f.d.send(
                "codex:host",
                "test",
                "continue".into(),
                Some(tier),
                None,
                &mut Out::new(&mut buf),
            )
            .await
            .unwrap();
            assert!(replies(&buf).contains("queued for a new turn"));
            let again = Daemon::load(options(&f.home), None);
            let saved = again.lock().tasks["test"].task.clone();
            assert_eq!(saved.tier, tier);
            assert_eq!(saved.session.as_deref(), Some("native-session"));
            assert_eq!((saved.turns, saved.delivered, saved.status), (3, 2, status));
            let mut st = f.d.lock();
            assert_eq!(st.tasks["test"].pending, ["continue"]);
            st.tasks.get_mut("test").unwrap().pending.clear();
        }
    }
}

#[tokio::test]
async fn tier_changes_reject_busy_and_closed_tasks_without_accepting_input() {
    for case in ["running", "releasing", "pending", "cancelled", "detached"] {
        let f = fixture();
        f.d.lock().stopping = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let mut st = f.d.lock();
            let e = st.tasks.get_mut("test").unwrap();
            match case {
                "running" | "releasing" => {
                    e.actor = Some(tx);
                    e.releasing = case == "releasing";
                }
                "pending" => e.pending.push("previous".into()),
                "cancelled" => e.task.status = Status::Cancelled,
                "detached" => e.task.detached = true,
                _ => unreachable!(),
            }
        }
        let error =
            f.d.input(
                "user",
                "test",
                "new input".into(),
                Some(ipc::Tier::Full),
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        let expected = match case {
            "cancelled" => "was cancelled",
            "detached" => "taken over",
            _ => "still running or has queued input",
        };
        assert!(error.contains(expected), "{case}: {error}");
        assert_eq!(f.d.lock().tasks["test"].task.tier, ipc::Tier::Read);
        assert!(
            !f.d.lock().tasks["test"]
                .pending
                .iter()
                .any(|m| m == "new input")
        );
        assert!(rx.try_recv().is_err());
    }
}

#[tokio::test]
async fn tier_changes_respect_task_ownership_and_parent_child_limits() {
    let native = "task:test/grok:native";
    let f = fixture_with(&[
        json!({"id": "kid", "parent": native}),
        json!({"id": "other", "parent": "task:test"}),
    ]);
    f.d.lock().stopping = true;
    for (caller, id, expected) in [
        (native, "test", "not your sub-task"),
        ("task:kid", "other", "not your sub-task"),
        ("task:test", "kid", "not your sub-task"),
        (native, "kid", "exceeds its parent"),
        ("user", "kid", "exceeds its parent"),
    ] {
        let error =
            f.d.input(caller, id, "continue".into(), Some(ipc::Tier::Full), None)
                .await
                .unwrap_err()
                .to_string();
        assert!(error.contains(expected), "{error}");
        assert!(f.d.lock().tasks[id].pending.is_empty());
    }
    f.d.input(
        "user",
        "test",
        "continue".into(),
        Some(ipc::Tier::Full),
        None,
    )
    .await
    .unwrap();
    f.d.lock().tasks.get_mut("test").unwrap().pending.clear();
    f.d.input(
        native,
        "kid",
        "continue".into(),
        Some(ipc::Tier::Full),
        None,
    )
    .await
    .unwrap();
    let error =
        f.d.input(
            "user",
            "test",
            "continue".into(),
            Some(ipc::Tier::Read),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("sub-tasks above Read"), "{error}");
    assert_eq!(f.d.lock().tasks["test"].task.tier, ipc::Tier::Full);
    // Idle children can still resume at their saved tier and also prevent a downgrade.
    f.d.lock().tasks.get_mut("kid").unwrap().pending.clear();
    assert!(
        f.d.input(
            "user",
            "test",
            "continue".into(),
            Some(ipc::Tier::Read),
            None
        )
        .await
        .is_err()
    );
    f.d.set_status("kid", Status::Cancelled);
    f.d.input(
        "user",
        "test",
        "continue".into(),
        Some(ipc::Tier::Read),
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn send_without_a_tier_change_still_steers_a_running_turn() {
    for tier in [None, Some(ipc::Tier::Read)] {
        let f = fixture();
        let (tx, mut rx) = mpsc::unbounded_channel();
        f.d.lock().tasks.get_mut("test").unwrap().actor = Some(tx);
        let receive = async {
            let Some(Cmd::Input(input)) = rx.recv().await else {
                panic!("expected input")
            };
            assert_eq!(input.text, "continue");
            input.reply.send("sent to its running turn").unwrap();
        };
        let send =
            f.d.input("codex:host", "test", "continue".into(), tier, None);
        let (result, ()) = tokio::join!(send, receive);
        assert_eq!(result.unwrap(), "sent to its running turn");
    }
}

/// The text lines of a reply stream.
fn replies(buf: &[u8]) -> String {
    buf.split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice::<Reply>(l).ok())
        .filter_map(|r| match r {
            Reply::Out { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn native_child_inboxes_are_separate_and_the_task_session_keeps_its_identity() {
    for (agent, host) in [
        (Agent::Claude, "claude"),
        (Agent::Codex, "codex"),
        (Agent::Grok, "grok"),
    ] {
        let native = format!("task:test/{host}:native");
        let root = format!("task:test/{host}:root");
        let f = fixture_with(&[
            json!({"id": "root_job", "parent": "task:test", "name": "job", "turns": 1}),
            json!({"id": "native_job", "parent": native, "name": "job", "turns": 1}),
        ]);
        f.d.update("test", |t| {
            t.agent = agent;
            t.session = Some("root".into());
        });
        assert_eq!(f.d.resolve_caller(&root), "task:test");
        assert_eq!(f.d.resolve_caller("task:test"), "task:test");
        assert_eq!(f.d.resolve_caller(&native), native);
        assert_eq!(
            f.d.resolve_caller("task:missing/codex:root"),
            "task:missing/codex:root"
        );
        let other = format!(
            "task:test/{}:root",
            if host == "grok" { "codex" } else { "grok" }
        );
        assert_eq!(f.d.resolve_caller(&other), other);
        assert_eq!(
            f.d.find(&f.d.resolve_caller(&root), "job").unwrap(),
            "root_job"
        );
        assert_eq!(f.d.find(&native, "job").unwrap(), "native_job");
        for (id, body) in [("root_job", "ROOT_RESULT"), ("native_job", "NATIVE_RESULT")] {
            f.d.store.save_turn(id, 1, body).unwrap();
            f.d.emit(id, "turn_ended", json!({"turn": 1, "ok": true}));
        }
        // Dispatch tests the same caller normalization used by CLI, watch and MCP requests.
        for (caller, own, other) in [
            (&root, "ROOT_RESULT", "NATIVE_RESULT"),
            (&native, "NATIVE_RESULT", "ROOT_RESULT"),
        ] {
            let mut buf = Vec::new();
            f.d.dispatch(
                Envelope {
                    protocol: ipc::PROTOCOL,
                    version: env!("CARGO_PKG_VERSION").into(),
                    caller: caller.clone(),
                    cwd: f.home.clone(),
                    env: None,
                    request: Request::Inbox { tasks: vec![] },
                },
                &mut Out::new(&mut buf),
            )
            .await
            .unwrap();
            let text = replies(&buf);
            assert!(text.contains(own) && !text.contains(other), "{text}");
        }
        let again = Daemon::load(options(&f.home), None);
        assert_eq!(again.resolve_caller(&root), "task:test");
        assert_eq!(again.resolve_caller(&native), native);
        assert!(again.unseen(&again.lock(), &native, &[]).is_empty());
        assert!(again.unseen(&again.lock(), "task:test", &[]).is_empty());
    }
}

#[tokio::test]
async fn native_callers_keep_the_owning_tasks_depth_and_tier_limits() {
    let native = "task:test/grok:native";
    let f = fixture_with(&[json!({"id": "kid", "parent": native})]);
    assert_eq!(f.d.depth(native), 1);
    assert_eq!(f.d.depth("task:kid/codex:native"), 2);
    let spawn = ipc::Spawn {
        agent: Agent::Grok,
        tier: ipc::Tier::Work,
        task: "unreachable".into(),
        cwd: f.home.clone(),
        name: None,
        model: None,
        effort: None,
        timeout_secs: None,
    };
    for (caller, expected) in [
        (native, "cannot spawn a Work task"),
        ("task:kid/codex:native", "two levels deep"),
    ] {
        let error =
            f.d.spawn(
                caller,
                spawn.clone(),
                vec![],
                &mut Out::new(&mut Vec::new()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(f.d.lock().tasks.len(), 2);
    let st = f.d.lock();
    assert!(within(&st, &st.tasks["kid"].task, "test"));
}

#[tokio::test]
async fn native_callers_receive_approvals_under_the_owning_tasks_tier() {
    let native = "task:test/codex:native";
    let f = fixture_with(&[json!({"id": "kid", "parent": native, "tier": "work"})]);
    assert_eq!(approver(&f.d.lock(), "kid"), "codex:host");
    f.d.update("test", |t| t.tier = ipc::Tier::Full);
    assert_eq!(approver(&f.d.lock(), "kid"), native);
    let (reply, decision) = oneshot::channel();
    f.d.request_approval(
        "kid",
        agent::Approval {
            summary: "review".into(),
            detail: "NATIVE_APPROVAL".into(),
            reply,
        },
    );
    let rid = f.d.lock().approvals.keys().next().unwrap().clone();
    let mut buf = Vec::new();
    f.d.inbox("task:test", &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    assert!(!replies(&buf).contains("NATIVE_APPROVAL"));
    assert!(
        f.d.resolve(&rid, Some("task:test"), agent::Decision::Approve)
            .is_err()
    );
    let mut buf = Vec::new();
    f.d.inbox(native, &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    assert!(replies(&buf).contains("NATIVE_APPROVAL"));
    f.d.resolve(&rid, Some(native), agent::Decision::Approve)
        .unwrap();
    assert!(matches!(decision.await.unwrap(), agent::Decision::Approve));
}

#[test]
fn cancelling_an_owner_also_cancels_tasks_spawned_by_its_native_children() {
    let f = fixture_with(&[
        json!({"id": "native_job", "parent": "task:test/grok:native"}),
        json!({"id": "root_job", "parent": "task:test"}),
        json!({"id": "unrelated"}),
    ]);
    assert_eq!(
        f.d.cancel_tree("test", "cancelled by request", "codex:host"),
        3
    );
    let st = f.d.lock();
    for id in ["test", "native_job", "root_job"] {
        assert_eq!(st.tasks[id].task.status, Status::Cancelled);
    }
    assert_eq!(st.tasks["unrelated"].task.status, Status::Idle);
}

#[tokio::test]
async fn wait_lists_news_without_taking_it_and_inbox_takes_all() {
    let f = fixture();
    f.d.emit("test", "message", json!({"to": "other", "text": "a note"}));
    // A sibling note is no news: with nothing running, wait returns at once.
    let mut buf = Vec::new();
    let exit =
        f.d.wait("codex:host", &[], None, &mut Out::new(&mut buf))
            .await
            .unwrap();
    assert_eq!(exit, ipc::exit::NOTHING_RUNNING);
    f.d.emit(
        "test",
        "question",
        json!({"id": "q1", "text": "Which port?"}),
    );
    let mut buf = Vec::new();
    let exit =
        f.d.wait("codex:host", &[], None, &mut Out::new(&mut buf))
            .await
            .unwrap();
    assert_eq!(exit, ipc::exit::OK);
    let text = replies(&buf);
    assert!(
        text.contains("asks q1") && !text.contains("a note"),
        "{text}"
    );
    assert_eq!(f.d.unseen(&f.d.lock(), "codex:host", &[]).len(), 2);

    let mut buf = Vec::new();
    let exit =
        f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
            .await
            .unwrap();
    assert_eq!(exit, ipc::exit::OK);
    let text = replies(&buf);
    // The question comes first although the note is older; both are taken.
    let (question, note) = (text.find("asks q1").unwrap(), text.find("a note").unwrap());
    assert!(question < note, "{text}");
    assert!(f.d.unseen(&f.d.lock(), "codex:host", &[]).is_empty());
    let mut buf = Vec::new();
    let exit =
        f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
            .await
            .unwrap();
    assert_eq!(exit, ipc::exit::NOTHING_RUNNING);
}

#[test]
fn a_reply_takes_whole_events_and_leaves_the_rest() {
    let f = fixture();
    let finish = |turn: u32, body: &str| {
        f.d.update("test", |t| t.turns = turn);
        f.d.store.save_turn("test", turn, body).unwrap();
        f.d.emit("test", "turn_ended", json!({"turn": turn, "ok": true}));
    };
    // Each result takes half the budget, so the second no longer fits after the first.
    let half = "x".repeat(render::RESPONSE_TOKENS * 2);
    finish(1, &half);
    finish(2, &half);
    let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
    let batch = f.d.batch(&[], &events);
    assert_eq!(batch.items.len(), 1);
    assert!(batch.items[0].1.ends_with(&format!("{half}\n</result>")));
    assert_eq!(batch.turns, [("test".to_string(), 1)]);
    assert_eq!(batch.left, 1);

    // A result larger than the budget still goes, cut down to fit.
    let f = fixture();
    let huge = format!("{}END", "y".repeat(render::RESPONSE_TOKENS * 8));
    f.d.update("test", |t| t.turns = 1);
    f.d.store.save_turn("test", 1, &huge).unwrap();
    f.d.emit("test", "turn_ended", json!({"turn": 1, "ok": true}));
    let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
    let batch = f.d.batch(&[], &events);
    let text = &batch.items[0].1;
    assert!(text.contains("[truncated") && text.ends_with("END\n</result>"));
    assert!(render::tokens(text) <= render::RESPONSE_TOKENS);
    assert_eq!(batch.left, 0);
}

#[tokio::test]
async fn long_questions_show_their_ends_and_where_the_rest_is() {
    let f = fixture();
    let long = format!("START{}END", "q".repeat(render::WHOLE_TOKENS * 8));
    f.d.ask("task:test", long.clone(), &mut Out::new(&mut Vec::new()))
        .await
        .unwrap();
    let mut buf = Vec::new();
    f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    let text = replies(&buf);
    assert!(text.contains("<question>\nSTART") && text.contains("END\n</question>"));
    assert!(render::tokens(&text) <= render::RESPONSE_TOKENS);
    let file = text
        .split("read all of it in ")
        .nth(1)
        .and_then(|rest| rest.split(" before answering").next())
        .unwrap();
    assert_eq!(std::fs::read_to_string(file).unwrap(), long);
}

#[tokio::test]
async fn approvals_go_ahead_of_earlier_news_and_reach_the_approver_once() {
    let f = fixture();
    f.d.update("test", |t| t.tier = ipc::Tier::Work);
    let finish = |turn: u32, body: &str| {
        f.d.update("test", |t| t.turns = turn);
        f.d.store.save_turn("test", turn, body).unwrap();
        f.d.emit("test", "turn_ended", json!({"turn": turn, "ok": true}));
    };
    let request = |summary: &str| {
        let (reply, decision) = oneshot::channel();
        f.d.request_approval(
            "test",
            agent::Approval {
                summary: summary.into(),
                detail: format!("Bash\ncommand: {summary}"),
                reply,
            },
        );
        decision
    };
    // Two results that fill the budget, then an approval request.
    let half = "x".repeat(render::RESPONSE_TOKENS * 2);
    finish(1, &half);
    finish(2, &half);
    let _decision = request("rm -rf build");
    let rid = f.d.lock().approvals.keys().next().unwrap().clone();

    let mut buf = Vec::new();
    f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    let first = replies(&buf);
    assert!(first.starts_with("[test codex] needs approval"), "{first}");
    assert!(
        first.ends_with("1 more: run `axiopera inbox` again."),
        "{first}"
    );
    f.d.resolve(&rid, Some("codex:host"), agent::Decision::Approve)
        .unwrap();

    // Handed over and decided: no longer news, and not shown again.
    let (left, _) = f.d.reminder("codex:host").unwrap();
    assert!(!left.contains("approval"), "{left}");
    let mut buf = Vec::new();
    f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    let second = replies(&buf);
    assert!(
        second.contains("turn 2 finished") && !second.contains("approval"),
        "{second}"
    );

    // A request decided before it was taken shows nothing.
    let _decision = request("ls");
    let rid = f.d.lock().approvals.keys().next().unwrap().clone();
    f.d.resolve(&rid, Some("user"), agent::Decision::Approve)
        .unwrap();
    assert!(f.d.reminder("codex:host").is_none());
    let mut buf = Vec::new();
    let exit =
        f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
            .await
            .unwrap();
    assert_eq!(exit, ipc::exit::NOTHING_RUNNING);
    assert!(f.d.unseen(&f.d.lock(), "codex:host", &[]).is_empty());
}

#[test]
fn progress_tells_the_turn_its_tool_calls_and_what_it_waits_for() {
    let f = fixture();
    let (actor, _commands) = mpsc::unbounded_channel();
    let (reply, _decision) = oneshot::channel();
    f.d.update("test", |t| t.status = Status::Running);
    f.d.emit("test", "turn_started", json!({"turn": 1}));
    f.d.emit(
        "test",
        "tool",
        json!({"name": "Bash", "detail": "cargo test"}),
    );
    f.d.emit("test", "tool", json!({"name": "Read", "detail": ""}));
    let mut st = f.d.lock();
    st.tasks.get_mut("test").unwrap().actor = Some(actor);
    st.approvals.insert(
        "r1".into(),
        Pending {
            task: "test".into(),
            approver: "codex:host".into(),
            summary: "Bash: rm -rf build".into(),
            delivered: false,
            reply,
        },
    );
    let progress = f.d.progress(&st, &st.tasks["test"], "codex:host");
    assert!(
        progress.starts_with("turn 1, 0s, 2 tool call(s), last 0s ago: Read;"),
        "{progress}"
    );
    assert!(
        progress.ends_with("waiting for your approval of r1"),
        "{progress}"
    );
    let other = f.d.progress(&st, &st.tasks["test"], "user");
    assert!(
        other.ends_with("waiting for approval of r1 from codex:host"),
        "{other}"
    );
    drop(st);

    // The process stays for background work, which may start a turn of its own.
    f.d.emit("test", "turn_ended", json!({"turn": 1, "ok": true}));
    f.d.update("test", |t| t.turns = 1);
    let st = f.d.lock();
    let between = f.d.progress(&st, &st.tasks["test"], "codex:host");
    drop(st);
    assert!(
        between.starts_with("turn 1 ended 0s ago; its background work still runs"),
        "{between}"
    );
    f.d.emit("test", "tool", json!({"name": "monitor", "detail": ""}));
    let st = f.d.lock();
    let next = f.d.progress(&st, &st.tasks["test"], "codex:host");
    assert!(
        next.starts_with("turn 2, 0s, 1 tool call(s), last 0s ago: monitor;"),
        "{next}"
    );
    drop(st);

    // Nothing runs once the process is on its way out.
    f.d.emit("test", "turn_ended", json!({"turn": 2, "ok": true}));
    let mut st = f.d.lock();
    st.tasks.get_mut("test").unwrap().releasing = true;
    let done = f.d.progress(&st, &st.tasks["test"], "codex:host");
    assert!(!done.contains("turn"), "{done}");
}

#[test]
fn durations_read_at_a_glance() {
    assert_eq!(render::duration(45_000), "45s");
    assert_eq!(render::duration(252_000), "4m12s");
    assert_eq!(render::duration(3_900_000), "1h05m");
}

#[tokio::test]
async fn agents_approve_only_requests_they_have_read_in_full() {
    let f = fixture();
    f.d.update("test", |t| t.tier = ipc::Tier::Work);
    let request = |summary: &str| {
        let (reply, decision) = oneshot::channel();
        f.d.request_approval(
            "test",
            agent::Approval {
                summary: summary.into(),
                detail: "Bash\ncommand:\nrm -rf build\nrm -rf ~".into(),
                reply,
            },
        );
        let rid = f.d.lock().approvals.keys().next().unwrap().clone();
        (rid, decision)
    };
    let (rid, decision) = request("Bash: rm -rf build ↵ rm -rf ~");
    let refused =
        f.d.resolve(&rid, Some("codex:host"), agent::Decision::Approve)
            .unwrap_err();
    assert!(refused.to_string().contains("read"), "{refused}");
    let mut buf = Vec::new();
    f.d.inbox("codex:host", &[], &mut Out::new(&mut buf))
        .await
        .unwrap();
    let text = replies(&buf);
    assert!(
        text.contains("<request>\nBash\ncommand:\nrm -rf build\nrm -rf ~\n</request>"),
        "{text}"
    );
    f.d.resolve(&rid, Some("codex:host"), agent::Decision::Approve)
        .unwrap();
    assert!(matches!(decision.await.unwrap(), agent::Decision::Approve));

    // A person decides without that, and anyone may deny.
    let (rid, _decision) = request("Bash: ls");
    f.d.resolve(&rid, Some("user"), agent::Decision::Approve)
        .unwrap();
    let (rid, _decision) = request("Bash: ls");
    f.d.resolve(&rid, Some("codex:host"), agent::Decision::Deny("no".into()))
        .unwrap();
}

#[test]
fn reminders_list_news_but_not_for_hosts_that_get_it_pushed() {
    let f = fixture();
    f.d.emit("test", "message", json!({"to": "other", "text": "a note"}));
    assert!(f.d.reminder("codex:host").is_none());
    f.d.emit(
        "test",
        "question",
        json!({"id": "q1", "text": "Which port?"}),
    );
    let (text, _) = f.d.reminder("codex:host").unwrap();
    assert_eq!(
        text,
        "- test (codex) asks q1: Which port?\nTake them with `axiopera inbox`."
    );
    let host = store::CodexHost {
        exe: "codex".into(),
        codex_home: None,
        thread: "host".into(),
        doorbell: false,
    };
    f.d.lock()
        .ledger
        .codex_hosts
        .insert("codex:host".into(), host);
    assert!(f.d.reminder("codex:host").is_none());
}

#[tokio::test]
async fn watch_merges_news_that_comes_together() {
    let f = fixture();
    f.d.emit(
        "test",
        "question",
        json!({"id": "q1", "text": "Which port?"}),
    );
    f.d.emit("test", "stalled", json!({"quiet_secs": 600}));
    f.d.emit("test", "message", json!({"to": "other", "text": "a note"}));
    let (client, mut server) = tokio::io::duplex(1 << 16);
    let d = f.d.clone();
    let watcher =
        tokio::spawn(async move { d.watch("codex:host", &mut Out::new(&mut server)).await });
    let mut reader = ipc::Reader::new(client);
    let Some(Reply::Out { text, .. }) = reader.read::<Reply>().await.unwrap() else {
        panic!("no line");
    };
    assert_eq!(text, "test (codex) asks q1: Which port? (+1 more)");

    // News held back for the next line may be taken meanwhile...
    f.d.emit("test", "stalled", json!({"quiet_secs": 900}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.d.inbox("codex:host", &[], &mut Out::new(&mut Vec::new()))
        .await
        .unwrap();
    tokio::time::sleep(WATCH_GAP).await;
    f.d.emit(
        "test",
        "question",
        json!({"id": "q2", "text": "Which host?"}),
    );
    let Some(Reply::Out { text, .. }) = reader.read::<Reply>().await.unwrap() else {
        panic!("no line");
    };
    assert_eq!(text, "test (codex) asks q2: Which host?");

    // ...or belong to a task gc removed meanwhile.
    f.d.emit("test", "failed", json!({"error": "gone"}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.d.lock().tasks.remove("test");
    tokio::time::sleep(WATCH_GAP + Duration::from_millis(500)).await;
    assert!(!watcher.is_finished());
    watcher.abort();
    assert!(reader.read::<Reply>().await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn a_sub_agents_watch_ends_a_minute_after_its_work_is_done_and_taken() {
    let f = fixture();
    f.d.update("test", |t| t.parent = "task:boss".into());
    let (actor, _rx) = mpsc::unbounded_channel();
    f.d.lock().tasks.get_mut("test").unwrap().actor = Some(actor);
    let (client, mut server) = tokio::io::duplex(1 << 16);
    let d = f.d.clone();
    let watcher =
        tokio::spawn(async move { d.watch("task:boss", &mut Out::new(&mut server)).await });
    let host = {
        let d = f.d.clone();
        tokio::spawn(async move { d.watch("codex:host", &mut Out::new(&mut Vec::new())).await })
    };
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(!watcher.is_finished(), "a sub-task still runs");

    f.d.emit(
        "test",
        "question",
        json!({"id": "q1", "text": "Which port?"}),
    );
    f.d.lock().tasks.get_mut("test").unwrap().actor = None;
    let mut reader = ipc::Reader::new(client);
    let Some(Reply::Out { text, .. }) = reader.read::<Reply>().await.unwrap() else {
        panic!("no line");
    };
    assert_eq!(text, "test (codex) asks q1: Which port?");
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(!watcher.is_finished(), "the question is not taken yet");

    f.d.inbox("task:boss", &[], &mut Out::new(&mut Vec::new()))
        .await
        .unwrap();
    tokio::time::sleep(WATCH_IDLE - Duration::from_secs(1)).await;
    assert!(!watcher.is_finished());
    tokio::time::sleep(WATCH_GAP * 2).await;
    assert_eq!(watcher.await.unwrap().unwrap(), ipc::exit::NOTHING_RUNNING);
    assert!(!host.is_finished(), "a session's watch runs until stopped");
    host.abort();
}

#[test]
fn the_request_log_keeps_argument_names_and_ages_out_with_gc() {
    let f = fixture();
    std::fs::create_dir_all(f.home.join("daemon")).unwrap();
    let request = Request::Wait {
        tasks: vec!["secret-name".into()],
        timeout_ms: None,
    };
    // Times of their own, so trimming at the second keeps exactly one.
    let mut entry = f.d.request_entry("task:test", &request);
    entry.as_mut().unwrap().insert("ts".into(), json!(1_000));
    f.d.record(entry, Some(ipc::exit::NOTHING_NEW), None, 42);
    assert!(f.d.request_entry("codex:host", &Request::Pending).is_none());
    let request = Request::Send {
        task: "test".into(),
        message: "do the secret thing".into(),
        tier: None,
    };
    let mut entry = f.d.request_entry("codex:host", &request);
    entry.as_mut().unwrap().insert("ts".into(), json!(2_000));
    f.d.record(entry, Some(ipc::exit::ERROR), Some(ErrorCode::NotFound), 0);

    let path = f.home.join("daemon").join("requests.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("secret"));
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["caller"], "codex sub-agent");
    assert_eq!(lines[0]["op"], "wait");
    assert_eq!(lines[0]["args"], json!(["tasks"]));
    assert_eq!(lines[0]["exit"], 3);
    assert_eq!(lines[0]["bytes"], 42);
    assert_eq!(lines[1]["caller"], "codex");
    assert_eq!(lines[1]["args"], json!(["message", "task"]));
    assert_eq!(lines[1]["error"], "not_found");

    f.d.trim_requests(lines[1]["ts"].as_u64().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    f.d.trim_requests(u64::MAX).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
}

#[test]
fn gc_takes_children_before_parents_and_leaves_running_and_recent_tasks() {
    let now = store::now_ms();
    let f = fixture_with(&[
        json!({"id": "kid", "parent": "task:test/grok:native"}),
        json!({"id": "busy"}),
        json!({"id": "mom"}),
        json!({"id": "baby", "parent": "task:mom/codex:native", "updated_ms": now}),
    ]);
    // Loading turns a running task into an interrupted one, so it starts running here.
    f.d.lock().tasks.get_mut("busy").unwrap().task.status = Status::Running;
    let left = |d: &Daemon| {
        let mut ids: Vec<String> = d.lock().tasks.keys().cloned().collect();
        ids.sort();
        ids
    };
    assert_eq!(f.d.collect(1_000).unwrap(), 2);
    assert_eq!(left(&f.d), ["baby", "busy", "mom"]);
    assert!(!f.home.join("tasks").join("kid").exists());
    assert_eq!(f.d.collect(u64::MAX).unwrap(), 2);
    assert_eq!(left(&f.d), ["busy"]);
}

#[tokio::test]
async fn a_note_kept_for_a_stopped_task_gives_it_time_before_gc() {
    let f = fixture_with(&[json!({"id": "sib"})]);
    f.d.tell(
        "task:sib",
        "test",
        "the API moved".into(),
        &mut Out::new(&mut Vec::new()),
    )
    .await
    .unwrap();
    assert_eq!(f.d.lock().tasks["test"].mail.len(), 1);
    f.d.collect(1_000).unwrap();
    assert!(f.d.lock().tasks.contains_key("test"));
}

#[test]
fn top_level_spawns_stop_at_the_limit_while_nested_ones_go_on() {
    let f = fixture_with(&[
        json!({"id": "a"}),
        json!({"id": "b", "parent": "claude:other"}),
        json!({"id": "kid", "parent": "task:a"}),
    ]);
    let d = Daemon::load(
        Options {
            concurrency: 2,
            ..options(&f.home)
        },
        None,
    );
    {
        let mut st = d.lock();
        for id in ["a", "kid"] {
            st.tasks.get_mut(id).unwrap().actor = Some(mpsc::unbounded_channel().0);
        }
        // About to start: counts as running.
        st.tasks.get_mut("b").unwrap().pending.push("go".into());
    }
    let why = d.at_limit(&d.lock(), "codex:host").unwrap();
    assert!(why.starts_with("2 top-level tasks"), "{why}");
    assert!(why.contains("config.toml"), "{why}");
    assert!(d.at_limit(&d.lock(), "task:a").is_none());
    assert!(d.at_limit(&d.lock(), "task:a/grok:native").is_none());
    d.lock().tasks.get_mut("b").unwrap().pending.clear();
    assert!(d.at_limit(&d.lock(), "codex:host").is_none());
}

#[test]
fn sequence_numbers_go_on_after_gc_removed_every_task() {
    let f = fixture();
    f.d.emit("test", "turn_ended", json!({"turn": 1}));
    let before = f.d.lock().seq;
    assert_eq!(f.d.collect(u64::MAX).unwrap(), 1);
    let again = Daemon::load(options(&f.home), None);
    assert_eq!(again.lock().seq, before);
}

#[test]
fn a_reply_passes_over_news_of_a_task_gc_removed_meanwhile() {
    let f = fixture();
    let event = |task: &str, seq: u64| -> Event {
        serde_json::from_value(json!({
            "seq": seq, "ts": 0, "task": task, "kind": "failed", "error": "boom",
        }))
        .unwrap()
    };
    let b = f.d.batch(&[], &[event("gone", 1), event("test", 2)]);
    assert_eq!(b.seen.len(), 2);
    assert_eq!(b.items.len(), 1);
    assert_eq!(b.items[0].0.task, "test");
    f.d.mark_seen("codex:host", &b.seen, &[], &[]).unwrap();
    assert!(
        !f.d.lock()
            .ledger
            .cursors
            .keys()
            .any(|k| k.ends_with("|gone"))
    );
}

#[test]
fn task_files_from_before_worktrees_went_still_load() {
    let task: Task = serde_json::from_value(json!({
        "id": "old", "parent": "user", "agent": "claude", "tier": "work",
        "cwd": ".", "status": "idle", "turns": 1, "delivered": 1,
        "changed_files": [], "worktree": null, "stalled": false, "detached": false,
        "created_ms": 0, "updated_ms": 0,
    }))
    .unwrap();
    assert_eq!(task.id, "old");
}

#[test]
fn the_daemon_log_drops_entries_before_the_cutoff_with_their_extra_lines() {
    let dir = std::env::temp_dir().join(format!("axiopera-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("daemon.log");
    assert!(super::trim_log(&path, 2_000).is_ok());
    std::fs::write(
        &path,
        "1000 old\nmore of old\n2000 kept\nmore of kept\n3000 new\n",
    )
    .unwrap();
    super::trim_log(&path, 1_500).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "2000 kept\nmore of kept\n3000 new\n"
    );
    super::trim_log(&path, 9_000).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    std::fs::remove_dir_all(&dir).unwrap();
}
