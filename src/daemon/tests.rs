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
    let home = std::env::temp_dir().join(format!("axiopera-test-{}", uuid::Uuid::new_v4()));
    let task: Task = serde_json::from_value(json!({
        "id": "test", "parent": "codex:host", "agent": "codex", "tier": "read",
        "cwd": ".", "status": "idle", "turns": 0, "delivered": 0,
        "changed_files": [], "stalled": false, "detached": false,
        "created_ms": 0, "updated_ms": 0,
    }))
    .unwrap();
    Store::new(&home).save(&task).unwrap();
    let opts = Options {
        home: home.clone(),
        idle: Duration::from_secs(10),
        concurrency: 0,
        stall: Duration::from_secs(60),
        approval_timeout: Duration::from_secs(60),
        agents: config::Agents::default(),
    };
    Fixture {
        d: Arc::new(Daemon::load(opts, None)),
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
async fn wait_lists_news_without_taking_it_and_inbox_takes_all() {
    let f = fixture();
    f.d.emit("test", "message", json!({"to": "other", "text": "a note"}));
    // A sibling note is no news: with nothing running, wait returns at once.
    let mut buf = Vec::new();
    let exit =
        f.d.wait("codex:host", &[], None, &mut Out { w: &mut buf })
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
        f.d.wait("codex:host", &[], None, &mut Out { w: &mut buf })
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
        f.d.inbox("codex:host", &[], &mut Out { w: &mut buf })
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
        f.d.inbox("codex:host", &[], &mut Out { w: &mut buf })
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
    let batch = f.d.batch(&events);
    assert_eq!(batch.items.len(), 1);
    assert!(batch.items[0].1.ends_with(&format!("{half}\n</result>")));
    assert_eq!(batch.turns, [("test".to_string(), 1)]);
    assert_eq!(batch.left.len(), 1);

    // A result larger than the budget still goes, cut down to fit.
    let f = fixture();
    let huge = format!("{}END", "y".repeat(render::RESPONSE_TOKENS * 8));
    f.d.update("test", |t| t.turns = 1);
    f.d.store.save_turn("test", 1, &huge).unwrap();
    f.d.emit("test", "turn_ended", json!({"turn": 1, "ok": true}));
    let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
    let batch = f.d.batch(&events);
    let text = &batch.items[0].1;
    assert!(text.contains("[truncated") && text.ends_with("END\n</result>"));
    assert!(render::tokens(text) <= render::RESPONSE_TOKENS);
    assert!(batch.left.is_empty());
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
    f.d.inbox("codex:host", &[], &mut Out { w: &mut buf })
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
