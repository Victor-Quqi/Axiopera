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
    assert!(f.d.lock().tasks["test"].wake.is_empty());
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
        assert!(!store::wakes(kind));
    }
    for kind in [
        "question",
        "approval_request",
        "stalled",
        "failed",
        "cancelled",
        "turn_ended",
    ] {
        assert!(store::wakes(kind));
    }
}
