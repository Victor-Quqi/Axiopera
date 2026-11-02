//! On-disk task state. The daemon is the only writer.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_mail: Vec<String>,
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

/// Event kinds `inbox` hands to a task's parent, or to the approver of a request.
pub fn deliverable(kind: &str) -> bool {
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
        match fs::remove_dir_all(self.dir(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// What the task was spawned with. Written before `task.json`, so that anyone who sees the
    /// task can read it.
    pub fn save_prompt(&self, id: &str, text: &str) -> Result<()> {
        let dir = self.dir(id);
        fs::create_dir_all(&dir)?;
        write_synced(&self.prompt_path(id), text.as_bytes())?;
        Ok(())
    }

    pub fn prompt_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("prompt.md")
    }

    pub fn save(&self, task: &Task) -> Result<()> {
        self.save_task(task, false)
    }

    pub fn save_durable(&self, task: &Task) -> Result<()> {
        self.save_task(task, true)
    }

    fn save_task(&self, task: &Task, durable: bool) -> Result<()> {
        let dir = self.dir(&task.id);
        fs::create_dir_all(dir.join("turns"))?;
        let tmp = dir.join("task.json.tmp");
        let bytes = serde_json::to_vec_pretty(task)?;
        if durable {
            write_synced(&tmp, &bytes)?;
        } else {
            fs::write(&tmp, bytes)?;
        }
        fs::rename(tmp, dir.join("task.json"))?;
        Ok(())
    }

    pub fn append(&self, event: &Event) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(self.events_path(&event.task))?;
        self.repair_tail(&event.task, &mut f)?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        f.write_all(&line)?;
        if deliverable(&event.kind)
            || matches!(
                event.kind.as_str(),
                "turn_started" | "steered" | "spawned" | "answered" | "approved" | "denied"
            )
        {
            f.sync_data()?;
        }
        Ok(())
    }

    pub fn turn_path(&self, id: &str, turn: u32) -> PathBuf {
        self.dir(id).join("turns").join(format!("{turn}.md"))
    }

    /// Where a task's text too long to show whole is kept: an approval request, a question or
    /// a note, under `approvals`, `questions` or `notes`.
    pub fn text_path(&self, id: &str, kind: &str, name: &str) -> PathBuf {
        self.dir(id).join(kind).join(format!("{name}.txt"))
    }

    /// Where a message the task's agent was given is kept whole, by the sequence number of the
    /// event that tells it was given.
    pub fn message_path(&self, id: &str, seq: u64) -> PathBuf {
        self.dir(id).join("messages").join(format!("{seq}.md"))
    }

    pub fn save_message(&self, id: &str, seq: u64, text: &str) -> Result<()> {
        let path = self.message_path(id, seq);
        fs::create_dir_all(self.dir(id).join("messages"))?;
        write_synced(&path, text.as_bytes())?;
        Ok(())
    }

    pub fn save_turn(&self, id: &str, turn: u32, text: &str) -> Result<PathBuf> {
        let path = self.turn_path(id, turn);
        let tmp = path.with_extension("md.tmp");
        write_synced(&tmp, text.as_bytes())?;
        fs::rename(tmp, &path)?;
        Ok(path)
    }

    pub fn events_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("events.jsonl")
    }

    #[cfg(test)]
    pub fn events(&self, id: &str) -> Vec<Event> {
        let Ok(f) = fs::File::open(self.events_path(id)) else {
            return Vec::new();
        };
        BufReader::new(f)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    }

    /// Recover a torn last record before another append can join onto it.
    fn repair_tail(&self, id: &str, f: &mut fs::File) -> Result<()> {
        let len = f.metadata()?.len();
        if len == 0 {
            return Ok(());
        }
        f.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        f.read_exact(&mut last)?;
        if last[0] == b'\n' {
            return Ok(());
        }
        let mut end = len;
        let mut tail = Vec::new();
        let start = loop {
            let start = end.saturating_sub(8192);
            f.seek(SeekFrom::Start(start))?;
            let mut block = vec![0; (end - start) as usize];
            f.read_exact(&mut block)?;
            if let Some(pos) = block.iter().rposition(|b| *b == b'\n') {
                tail.extend(block[pos + 1..].iter().rev());
                break start + pos as u64 + 1;
            }
            tail.extend(block.iter().rev());
            if start == 0 {
                break 0;
            }
            end = start;
        };
        tail.reverse();
        if serde_json::from_slice::<Event>(&tail).is_ok() {
            f.write_all(b"\n")?;
        } else {
            let backup = self
                .dir(id)
                .join(format!("events.tail-{}", uuid::Uuid::new_v4()));
            write_synced(&backup, &tail)?;
            OpenOptions::new()
                .write(true)
                .open(self.events_path(id))?
                .set_len(start)?;
        }
        f.sync_data()?;
        Ok(())
    }

    /// Startup scans records once without retaining non-deliverable events.
    pub fn recover_events(&self, id: &str, mut visit: impl FnMut(Event)) -> Result<()> {
        let mut f = match OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.events_path(id))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        self.repair_tail(id, &mut f)?;
        f.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(f);
        let mut line = Vec::new();
        let mut error = None;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            match serde_json::from_slice::<Event>(&line).context("invalid event record") {
                Ok(event) if event.task == id => visit(event),
                Ok(_) => {
                    error.get_or_insert_with(|| {
                        anyhow::anyhow!("event task does not match directory")
                    });
                }
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn quarantine(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.parent().unwrap().join("quarantine");
        fs::create_dir_all(&root)?;
        let name = path.file_name().unwrap().to_string_lossy();
        let target = root.join(format!("{name}-{}", uuid::Uuid::new_v4()));
        fs::rename(path, &target)?;
        Ok(target)
    }

    #[cfg(test)]
    pub fn load_all(&self) -> Vec<Task> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                if !e.file_type().ok()?.is_dir() {
                    return None;
                }
                let task: Task =
                    serde_json::from_slice(&fs::read(e.path().join("task.json")).ok()?).ok()?;
                (e.file_name().to_str() == Some(task.id.as_str())).then_some(task)
            })
            .collect()
    }

    pub fn recover_tasks(&self) -> (Vec<Task>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut errors = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (tasks, errors),
            Err(e) => {
                errors.push(format!("reading task directories: {e:#}"));
                return (tasks, errors);
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    errors.push(e.to_string());
                    continue;
                }
            };
            let read = || -> Result<Task> {
                if !entry.file_type()?.is_dir() {
                    bail!("task directory is not a real directory");
                }
                let task: Task =
                    serde_json::from_slice(&fs::read(entry.path().join("task.json"))?)?;
                if entry.file_name().to_str() != Some(task.id.as_str()) {
                    bail!("task ID does not match directory");
                }
                Ok(task)
            };
            match read() {
                Ok(task) => tasks.push(task),
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() != std::io::ErrorKind::NotFound) =>
                {
                    errors.push(format!(
                        "reading task directory {}: {e:#}; directory kept for retry",
                        entry.path().display()
                    ));
                }
                Err(e) => {
                    let isolated = self.quarantine(&entry.path());
                    errors.push(format!(
                        "invalid task directory {}: {e:#}; quarantine: {isolated:?}",
                        entry.path().display()
                    ));
                }
            }
        }
        (tasks, errors)
    }
}

/// Delivery state per caller: how far it has seen each task's events, and the host sessions
/// that take them as messages. The event sequence itself is recovered from the event logs.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Ledger {
    pub cursors: std::collections::HashMap<String, u64>,
    #[serde(default)]
    pub codex_hosts: std::collections::HashMap<String, CodexHost>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub pending_pushes: std::collections::HashMap<String, PendingPush>,
    /// Reminders queued for hosts that have not run `wait` since, by queued message ID.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub doorbells: std::collections::HashMap<String, String>,
    /// The last event sequence number given out, as of the last gc, which may have removed the
    /// events it was recovered from.
    #[serde(default)]
    pub seq: u64,
}

/// A batch written without an acknowledged reply, recoverable after a daemon restart.
#[derive(Serialize, Deserialize, Clone)]
pub struct PendingPush {
    pub marker: String,
    pub events: Vec<(String, u64)>,
    pub turns: Vec<(String, u32)>,
    #[serde(default)]
    pub approvals: Vec<String>,
}

/// A Codex session reached through `exe`.
#[derive(Serialize, Deserialize, Clone)]
pub struct CodexHost {
    pub exe: PathBuf,
    /// The caller's `CODEX_HOME`, which locates its daemon and data.
    pub codex_home: Option<PathBuf>,
    pub thread: String,
    /// Outside the shared daemon: reminded through Codex's queue to run `wait`, rather than
    /// sent the events.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub doorbell: bool,
}

impl Ledger {
    pub fn load(home: &Path) -> Result<Ledger> {
        let path = home.join("ledger.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "invalid {}; restore the ledger before starting the daemon",
                path.display()
            )
        })
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let tmp = home.join("ledger.json.tmp");
        write_synced(&tmp, &serde_json::to_vec(self)?)?;
        fs::rename(tmp, home.join("ledger.json"))?;
        Ok(())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
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
