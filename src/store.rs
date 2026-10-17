//! On-disk task state. The daemon is the only writer.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ipc::{Agent, Tier};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Idle,
    /// The last turn ended with questions to the parent still open; an answer resumes it.
    Waiting,
    Cancelled,
    Failed,
    Interrupted,
}

impl Status {
    /// Only cancellation is final; a failed or interrupted task can be continued with `send`.
    pub fn is_final(self) -> bool {
        self == Status::Cancelled
    }
}

/// Size and mtime of the native session file when we last let go of it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub path: PathBuf,
    pub len: u64,
    pub modified_ms: u64,
}

impl Fingerprint {
    pub fn of(path: &Path) -> Option<Fingerprint> {
        let m = fs::metadata(path).ok()?;
        let modified_ms = m
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        Some(Fingerprint {
            path: path.to_path_buf(),
            len: m.len(),
            modified_ms,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
    pub id: String,
    pub name: Option<String>,
    /// Caller key of whoever spawned it.
    pub parent: String,
    pub agent: Agent,
    pub tier: Tier,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_secs: Option<u64>,
    pub status: Status,
    /// Native session ID once the agent has reported it.
    pub session: Option<String>,
    pub session_file: Option<Fingerprint>,
    pub turns: u32,
    /// Highest turn whose result text has been handed to the parent.
    pub delivered: u32,
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub worktree: Option<crate::worktree::Worktree>,
    pub stalled: bool,
    pub detached: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl Task {
    pub fn open_questions(&self) -> impl Iterator<Item = &Question> {
        self.questions.iter().filter(|q| q.answer.is_none())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub answer: Option<String>,
    pub asked_ms: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub seq: u64,
    pub ts: u64,
    pub task: String,
    pub kind: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

/// Event kinds that end a `wait`.
pub fn wakes(kind: &str) -> bool {
    matches!(
        kind,
        "turn_ended"
            | "question"
            | "approval_request"
            | "message"
            | "stalled"
            | "failed"
            | "cancelled"
            | "interrupted"
            | "detached"
    )
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(home: &Path) -> Store {
        Store {
            root: home.join("tasks"),
        }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.dir(id).exists()
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        fs::remove_dir_all(self.dir(id))?;
        Ok(())
    }

    pub fn save(&self, task: &Task) -> Result<()> {
        let dir = self.dir(&task.id);
        fs::create_dir_all(dir.join("turns"))?;
        let tmp = dir.join("task.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(task)?)?;
        fs::rename(tmp, dir.join("task.json"))?;
        Ok(())
    }

    pub fn append(&self, event: &Event) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir(&event.task).join("events.jsonl"))?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        f.write_all(&line)?;
        Ok(())
    }

    pub fn turn_path(&self, id: &str, turn: u32) -> PathBuf {
        self.dir(id).join("turns").join(format!("{turn}.md"))
    }

    pub fn save_turn(&self, id: &str, turn: u32, text: &str) -> Result<PathBuf> {
        let path = self.turn_path(id, turn);
        fs::write(&path, text)?;
        Ok(path)
    }

    pub fn events(&self, id: &str) -> Vec<Event> {
        let Ok(f) = fs::File::open(self.dir(id).join("events.jsonl")) else {
            return Vec::new();
        };
        BufReader::new(f)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    }

    pub fn load_all(&self) -> Vec<Task> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| fs::read(e.path().join("task.json")).ok())
            .filter_map(|b| serde_json::from_slice(&b).ok())
            .collect()
    }
}

/// Delivery state per caller: how far it has seen each task's events, and the host sessions
/// that take them as messages. The event sequence itself is recovered from the event logs.
#[derive(Serialize, Deserialize, Default)]
pub struct Ledger {
    pub cursors: std::collections::HashMap<String, u64>,
    #[serde(default)]
    pub codex_hosts: std::collections::HashMap<String, CodexHost>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub pending_pushes: std::collections::HashMap<String, PendingPush>,
}

/// A batch written without an acknowledged reply, recoverable after a daemon restart.
#[derive(Serialize, Deserialize, Clone)]
pub struct PendingPush {
    pub marker: String,
    pub events: Vec<(String, u64)>,
    pub turns: Vec<(String, u32)>,
}

/// A Codex session on the user's shared app-server daemon, reached through `exe`.
#[derive(Serialize, Deserialize, Clone)]
pub struct CodexHost {
    pub exe: PathBuf,
    /// The caller's `CODEX_HOME`, which locates its daemon.
    pub codex_home: Option<PathBuf>,
    pub thread: String,
}

impl Ledger {
    pub fn load(home: &Path) -> Ledger {
        fs::read(home.join("ledger.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let tmp = home.join("ledger.json.tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(tmp, home.join("ledger.json"))?;
        Ok(())
    }
}

/// Short random ID: a prefix letter (`t` task, `q` question) and base32 characters.
pub fn new_id(prefix: char, taken: impl Fn(&str) -> bool) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    for len in 4.. {
        for _ in 0..8 {
            let bytes = uuid::Uuid::new_v4().into_bytes();
            let id: String = std::iter::once(prefix)
                .chain(
                    bytes
                        .iter()
                        .take(len)
                        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char),
                )
                .collect();
            if !taken(&id) {
                return id;
            }
        }
    }
    unreachable!()
}
