//! `axiopera top`: every task and its latest events, redrawn each second, for a person
//! watching sub-agents work. It only reads the data directory and never connects to the
//! daemon, so it starts nothing, takes no messages and marks no result delivered.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::daemon::render;
use crate::identity;
use crate::platform;
use crate::store::{self, Event, Status, Store, Task};

const REFRESH: Duration = Duration::from_secs(1);
/// How long a stopped task stays in view after its last change, without `--all`.
const RECENT_MS: u64 = 60 * 60 * 1000;
/// Events kept for the feed.
const FEED: usize = 200;
/// Events listed when the output is not a terminal.
const PLAIN_FEED: usize = 20;
/// Assumed when the terminal does not tell its size.
const FALLBACK_SIZE: (usize, usize) = (100, 30);
/// Characters of a task name shown.
const NAME: usize = 24;

const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

/// Text and the SGR color it is shown in, empty for none.
type Seg = (String, &'static str);

pub async fn run(home: &Path, all: bool) -> Result<i32> {
    let mut view = View::new(home, all);
    if !std::io::stdout().is_terminal() {
        view.refresh();
        let mut out = std::io::stdout().lock();
        for line in view.frame(store::now_ms(), None) {
            writeln!(out, "{line}")?;
        }
        return Ok(0);
    }
    // The handler is installed on the first poll. Until then Ctrl-C ends the process at once,
    // which would leave the terminal as changed below.
    let mut stop = std::pin::pin!(tokio::signal::ctrl_c());
    tokio::select! {
        biased;
        _ = &mut stop => return Ok(0),
        () = std::future::ready(()) => {}
    }
    let _ansi = platform::Ansi::enable();
    // The alternate screen leaves the shell's scrollback as it was. Without autowrap, a line
    // wider than measured is clipped instead of pushing the frame down.
    show("\x1b[?1049h\x1b[?25l\x1b[?7l")?;
    let drawn = loop {
        view.refresh();
        let size = platform::terminal_size()
            .filter(|&(w, h)| w > 0 && h > 0)
            .or_else(env_size)
            .unwrap_or(FALLBACK_SIZE);
        // Drawn over the last frame instead of after clearing it, which would flicker.
        let lines = view.frame(store::now_ms(), Some(size));
        if let Err(e) = show(&format!("\x1b[H{}\x1b[K\x1b[J", lines.join("\x1b[K\r\n"))) {
            break Err(e);
        }
        tokio::select! {
            _ = &mut stop => break Ok(()),
            () = tokio::time::sleep(REFRESH) => {}
        }
    };
    show("\x1b[?7h\x1b[?25h\x1b[?1049l")?;
    drawn?;
    Ok(0)
}

fn show(s: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// `COLUMNS` and `LINES`, for terminals that are not consoles, such as mintty without ConPTY.
fn env_size() -> Option<(usize, usize)> {
    let get = |k: &str| -> Option<usize> {
        std::env::var(k)
            .ok()?
            .trim()
            .parse()
            .ok()
            .filter(|&n| n > 0)
    };
    Some((get("COLUMNS")?, get("LINES").unwrap_or(FALLBACK_SIZE.1)))
}

/// What has been read of the data directory so far.
struct View {
    store: Store,
    all: bool,
    tasks: HashMap<String, Task>,
    tails: HashMap<String, Tail>,
    /// The latest events of all tasks, oldest first.
    feed: Vec<Event>,
}

impl View {
    fn new(home: &Path, all: bool) -> View {
        View {
            store: Store::new(home),
            all,
            tasks: HashMap::new(),
            tails: HashMap::new(),
            feed: Vec::new(),
        }
    }

    /// Read every task again, and the events added since the last refresh.
    fn refresh(&mut self) {
        self.tasks = self
            .store
            .load_all()
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        // Only a removed task is forgotten: its events would be read again from the start if
        // a `task.json` that could not be read for a moment dropped them.
        let store = &self.store;
        self.tails.retain(|id, _| store.exists(id));
        let tails = &self.tails;
        self.feed.retain(|e| tails.contains_key(&e.task));
        let before = self.feed.len();
        for id in self.tasks.keys() {
            let tail = self.tails.entry(id.clone()).or_default();
            let events = tail.read(&self.store.events_path(id));
            for e in &events {
                tail.absorb(e);
            }
            self.feed.extend(events);
        }
        if self.feed.len() > before {
            self.feed.sort_by_key(|e| e.seq);
            let excess = self.feed.len().saturating_sub(FEED);
            self.feed.drain(..excess);
        }
    }

    fn parent(&self, t: &Task) -> Option<&Task> {
        identity::task_of(&t.parent).and_then(|id| self.tasks.get(id))
    }

    /// The screen as lines: fitted to `size` with colors, or whole and plain without it.
    fn frame(&self, now: u64, size: Option<(usize, usize)>) -> Vec<String> {
        let (mut rows, hidden) = self.table(now);
        let count = |s: Status| self.tasks.values().filter(|t| t.status == s).count();
        let mut title: Vec<Seg> = vec![
            ("axiopera top".into(), BOLD),
            (
                format!(
                    "  running {}, waiting for answers {}, queued {}",
                    count(Status::Running),
                    count(Status::Waiting),
                    count(Status::Queued)
                ),
                "",
            ),
        ];
        if hidden > 0 {
            title.push((
                format!("  ({hidden} stopped over an hour ago; --all shows them)"),
                DIM,
            ));
        }
        if size.is_some() {
            title.push(("  Ctrl-C to quit".into(), DIM));
        }
        let mut feed: Vec<Vec<Seg>> = self
            .feed
            .iter()
            .filter_map(|e| {
                let agent = self.tasks.get(&e.task)?.agent;
                Some(vec![
                    (format!("{:>6}  ", age(now.saturating_sub(e.ts))), DIM),
                    (render::event_line(e, agent), ""),
                ])
            })
            .collect();
        match size {
            None => {
                feed.drain(..feed.len().saturating_sub(PLAIN_FEED));
            }
            Some((_, height)) => {
                // Besides the rows and the feed: the title, the feed's title and a blank line
                // after each title. Tasks come first, but the feed keeps a few lines.
                let room = height.saturating_sub(4);
                let rows_room = room - room.min(3).min(feed.len());
                if rows.len() > rows_room {
                    let kept = rows_room.saturating_sub(1);
                    let more = rows.len() - kept;
                    rows.truncate(kept);
                    if rows_room > 0 {
                        rows.push(vec![(format!("  …and {more} more lines"), DIM)]);
                    }
                }
                let feed_room = room.saturating_sub(rows.len());
                feed.drain(..feed.len().saturating_sub(feed_room));
            }
        }
        let mut lines = vec![title, vec![]];
        lines.extend(rows);
        lines.push(vec![]);
        lines.push(vec![("Latest events".into(), BOLD)]);
        lines.extend(feed);
        let width = size.map(|(w, _)| w);
        let mut out: Vec<String> = lines.iter().map(|l| paint(l, width)).collect();
        if let Some((_, height)) = size {
            out.truncate(height);
        }
        out
    }

    /// Task rows under a line for each session that spawned them, sub-tasks under their
    /// parent. Also returns how many stopped tasks are left out.
    fn table(&self, now: u64) -> (Vec<Vec<Seg>>, usize) {
        let mut shown: HashSet<&str> = self
            .tasks
            .values()
            .filter(|t| self.all || active(t) || now.saturating_sub(t.updated_ms) < RECENT_MS)
            .map(|t| t.id.as_str())
            .collect();
        // A shown sub-task brings its ancestors, so that it appears where it belongs.
        for id in shown.clone() {
            let mut up = self.parent(&self.tasks[id]);
            while let Some(p) = up {
                if !shown.insert(&p.id) {
                    break;
                }
                up = self.parent(p);
            }
        }
        let hidden = self.tasks.len() - shown.len();
        let mut sorted: Vec<&Task> = shown.iter().map(|id| &self.tasks[*id]).collect();
        sorted.sort_by_key(|t| (t.created_ms, t.id.as_str()));
        let mut groups: Vec<(&str, Vec<&Task>)> = Vec::new();
        let mut children: HashMap<&str, Vec<&Task>> = HashMap::new();
        for t in sorted {
            match self.parent(t) {
                Some(p) => children.entry(p.id.as_str()).or_default().push(t),
                None => match groups.iter_mut().find(|(c, _)| *c == t.parent) {
                    Some((_, roots)) => roots.push(t),
                    None => groups.push((t.parent.as_str(), vec![t])),
                },
            }
        }
        // Rows in order: `None` for a group's line, else the task and its depth.
        let mut order: Vec<(&str, Option<(usize, &Task)>)> = Vec::new();
        for (caller, roots) in &groups {
            order.push((*caller, None));
            let mut stack: Vec<(usize, &Task)> = roots.iter().rev().map(|t| (1, *t)).collect();
            while let Some((depth, t)) = stack.pop() {
                order.push((*caller, Some((depth, t))));
                if let Some(kids) = children.get(t.id.as_str()) {
                    stack.extend(kids.iter().rev().map(|k| (depth + 1, *k)));
                }
            }
        }
        if order.is_empty() {
            return (vec![vec![("no tasks".into(), DIM)]], hidden);
        }
        let id_cell = |depth: usize, t: &Task| format!("{}{}", "  ".repeat(depth), t.id);
        let name_cell = |t: &Task| t.name.clone().unwrap_or_else(|| "-".into());
        let tasks = order.iter().filter_map(|(_, row)| *row);
        let id_width = tasks
            .clone()
            .map(|(d, t)| columns(&id_cell(d, t)))
            .max()
            .unwrap_or(0)
            .max(6);
        let name_width = tasks
            .map(|(_, t)| columns(&name_cell(t)).min(NAME))
            .max()
            .unwrap_or(0)
            .max(4);
        let mut rows = vec![vec![(
            format!(
                "{}  {}  {}  {}  ACTIVITY",
                pad("  TASK", id_width),
                pad("NAME", name_width),
                pad("AGENT", 11),
                pad("STATUS", 11)
            ),
            DIM,
        )]];
        for (caller, row) in order {
            let Some((depth, t)) = row else {
                rows.push(vec![(caller_label(caller), BOLD)]);
                continue;
            };
            let (status, color) = status_cell(t);
            rows.push(vec![
                (
                    format!(
                        "{}  {}  {}  ",
                        pad(&id_cell(depth, t), id_width),
                        pad(&name_cell(t), name_width),
                        pad(
                            &format!(
                                "{}/{}",
                                render::agent_name(t.agent),
                                render::tier_name(t.tier)
                            ),
                            11
                        )
                    ),
                    "",
                ),
                (pad(status, 11), color),
                (format!("  {}", self.activity(t, now)), ""),
            ]);
        }
        (rows, hidden)
    }

    /// What a task is doing, or when it last did something.
    fn activity(&self, t: &Task, now: u64) -> String {
        let ago = |ms: u64| age(now.saturating_sub(ms));
        let none = Tail::default();
        let tail = self.tails.get(&t.id).unwrap_or(&none);
        // What waits for someone comes first, so that a narrow window does not cut it off.
        let mut parts = Vec::new();
        if t.status == Status::Running {
            parts.extend(
                tail.approvals
                    .iter()
                    .map(|id| format!("approval {id} pending")),
            );
        }
        let open: Vec<&str> = t.open_questions().map(|q| q.id.as_str()).collect();
        if !open.is_empty() && !t.status.is_final() {
            parts.push(format!("question {} unanswered", open.join(", ")));
        }
        if t.stalled {
            parts.push("STALLED".into());
        }
        if t.detached {
            parts.push("DETACHED".into());
        }
        match t.status {
            Status::Running if tail.in_turn => {
                let mut turn = format!("turn {}, {}", t.turns + 1, ago(tail.started_ms));
                match &tail.last_tool {
                    Some((call, at)) => turn.push_str(&format!(
                        ", {} tool call(s), last {} ago: {call}",
                        tail.tool_calls,
                        ago(*at)
                    )),
                    None => turn.push_str(", no tool calls yet"),
                }
                parts.push(turn);
            }
            Status::Running if tail.ended_ms > 0 => parts.push(format!(
                "turn {} ended {} ago; background work still runs",
                t.turns,
                ago(tail.ended_ms)
            )),
            Status::Running => parts.push("starting".to_string()),
            Status::Queued => parts.push("about to start".to_string()),
            _ => {
                let mut done =
                    format!("{} turn(s), last change {} ago", t.turns, ago(t.updated_ms));
                if !t.changed_files.is_empty() {
                    done.push_str(&format!(", {} file(s) changed", t.changed_files.len()));
                }
                parts.push(done);
            }
        }
        parts.join("; ")
    }
}

/// Like the daemon's durations, with days for tasks left for long.
fn age(ms: u64) -> String {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    if ms < 2 * DAY_MS {
        return render::duration(ms);
    }
    format!("{}d{:02}h", ms / DAY_MS, ms % DAY_MS / (60 * 60 * 1000))
}

fn active(t: &Task) -> bool {
    matches!(t.status, Status::Queued | Status::Running | Status::Waiting)
}

fn status_cell(t: &Task) -> (&'static str, &'static str) {
    let (word, color) = match t.status {
        Status::Queued => ("queued", YELLOW),
        Status::Running => ("running", GREEN),
        Status::Idle => ("idle", ""),
        Status::Waiting => ("waiting", YELLOW),
        Status::Failed => ("failed", RED),
        Status::Interrupted => ("interrupted", RED),
        Status::Cancelled => ("cancelled", DIM),
    };
    (word, if t.stalled || t.detached { RED } else { color })
}

/// Who spawned a group of tasks: a host session, `user`, or a task already removed. Agents
/// other than the known hosts call as `user` too, like a person at a terminal.
fn caller_label(caller: &str) -> String {
    match caller.split_once(':') {
        _ if caller == "user" => "user: terminals and unrecognized agents".into(),
        Some((host, session)) if host != "task" => format!("{host} session {session}"),
        _ => caller.into(),
    }
}

/// What a task's events tell beyond its `task.json`.
#[derive(Default)]
struct Tail {
    /// Bytes of `events.jsonl` read, up to the end of the last whole line.
    offset: u64,
    in_turn: bool,
    started_ms: u64,
    ended_ms: u64,
    tool_calls: u64,
    last_tool: Option<(String, u64)>,
    /// Approval requests of the current turn without a decision.
    approvals: Vec<String>,
}

impl Tail {
    /// The events appended since the last read. A line still being written waits for the
    /// next read.
    fn read(&mut self, path: &Path) -> Vec<Event> {
        let mut bytes = Vec::new();
        let read = File::open(path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.read_to_end(&mut bytes)
        });
        let Some(end) = read.ok().and(bytes.iter().rposition(|&b| b == b'\n')) else {
            return Vec::new();
        };
        self.offset += end as u64 + 1;
        bytes[..end]
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()
    }

    /// Follows the daemon's own account of turns in `Daemon::emit`.
    fn absorb(&mut self, e: &Event) {
        let text = |k: &str| e.data.get(k).and_then(Value::as_str).unwrap_or("");
        match e.kind.as_str() {
            "turn_started" => {
                self.start(e.ts);
                self.approvals.clear();
            }
            "turn_ended" => {
                self.in_turn = false;
                self.ended_ms = e.ts;
            }
            "tool" => {
                // A turn the agent started by itself that its adapter did not report.
                if !self.in_turn {
                    self.start(e.ts);
                }
                let call = match (text("name"), text("detail")) {
                    (name, "") => name.to_string(),
                    (name, detail) => format!("{name}: {detail}"),
                };
                self.tool_calls += 1;
                self.last_tool = Some((call, e.ts));
            }
            "approval_request" => self.approvals.push(text("id").into()),
            "approved" | "denied" => self.approvals.retain(|id| id != text("id")),
            _ => {}
        }
    }

    fn start(&mut self, at: u64) {
        self.in_turn = true;
        self.started_ms = at;
        self.tool_calls = 0;
        self.last_tool = None;
    }
}

/// One line from its pieces. Given a width, it is cut to fit and colored.
fn paint(segs: &[Seg], width: Option<usize>) -> String {
    // The last column stays empty: writing there makes some terminals wrap.
    let mut room = width.map_or(usize::MAX, |w| w.saturating_sub(1));
    let mut line = String::new();
    for (text, color) in segs {
        let (piece, cut) = fit(&clean(text), room);
        room -= columns(&piece);
        if width.is_some() && !color.is_empty() && !piece.is_empty() {
            line.push_str(&format!("\x1b[{color}m{piece}\x1b[0m"));
        } else {
            line.push_str(&piece);
        }
        if cut {
            break;
        }
    }
    line
}

/// Agent text on one line, without control characters that could move the cursor or change
/// the terminal.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => '↵',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// The text cut to `room` columns, ending in `…` when something was left out.
fn fit(text: &str, room: usize) -> (String, bool) {
    if columns(text) <= room {
        return (text.to_string(), false);
    }
    let mut used = 0;
    let mut kept = String::new();
    for c in text.chars() {
        let w = char_columns(c);
        if used + w + 1 > room {
            break;
        }
        used += w;
        kept.push(c);
    }
    if room > 0 {
        kept.push('…');
    }
    (kept, true)
}

fn pad(text: &str, width: usize) -> String {
    let (mut s, _) = fit(text, width);
    s.push_str(&" ".repeat(width.saturating_sub(columns(&s))));
    s
}

fn columns(s: &str) -> usize {
    s.chars().map(char_columns).sum()
}

/// Two columns for East Asian wide and fullwidth characters, and for emoji and symbol blocks
/// where most are: counting a narrow one as wide only leaves its line a column short.
fn char_columns(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x231A..=0x23FF
        | 0x25FD..=0x27BF
        | 0x2B1B..=0x2B55
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    const NOW: u64 = 100_000_000;

    struct Home(PathBuf);

    impl Drop for Home {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn home() -> Home {
        let path = std::env::temp_dir().join(format!("axiopera-top-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Home(path)
    }

    fn task(id: &str, parent: &str, status: &str, at: u64, more: Value) -> Task {
        let mut fields = json!({
            "id": id, "parent": parent, "agent": "codex", "tier": "work", "cwd": ".",
            "status": status, "turns": 1, "delivered": 0, "changed_files": [],
            "stalled": false, "detached": false, "created_ms": at, "updated_ms": at,
        });
        fields
            .as_object_mut()
            .unwrap()
            .extend(more.as_object().unwrap().clone());
        serde_json::from_value(fields).unwrap()
    }

    fn event(seq: u64, ago_ms: u64, task: &str, kind: &str, data: Value) -> Event {
        Event {
            seq,
            ts: NOW - ago_ms,
            task: task.into(),
            kind: kind.into(),
            data: data.as_object().unwrap().clone(),
        }
    }

    fn line_of<'a>(frame: &'a [String], id: &str) -> &'a str {
        frame
            .iter()
            .find(|l| l.trim_start().starts_with(&format!("{id} ")))
            .unwrap_or_else(|| panic!("no row for {id} in {frame:#?}"))
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    #[test]
    fn shows_the_task_tree_and_what_runs_without_touching_delivery() {
        let h = home();
        let store = Store::new(&h.0);
        let hour = RECENT_MS;
        for t in [
            task(
                "t1",
                "claude:s1",
                "running",
                NOW - 600_000,
                json!({"name": "审查"}),
            ),
            task("t2", "task:t1", "idle", NOW - 500_000, json!({})),
            // Spawned by a native sub-agent inside t1: still under t1.
            task(
                "t3",
                "task:t1/claude:native",
                "waiting",
                NOW - 400_000,
                json!({"questions": [{"id": "q1", "text": "?", "answer": null, "asked_ms": 0}]}),
            ),
            task("t4", "user", "idle", NOW - 2 * hour, json!({})),
            // Old, but its sub-task runs.
            task("t5", "user", "idle", NOW - 3 * hour, json!({})),
            task("t6", "task:t5", "queued", NOW - 1_000, json!({})),
        ] {
            store.save(&t).unwrap();
        }
        for e in [
            event(1, 120_000, "t1", "turn_started", json!({"turn": 2})),
            event(
                2,
                60_000,
                "t1",
                "tool",
                json!({"name": "Bash", "detail": "cargo test"}),
            ),
            event(
                3,
                30_000,
                "t1",
                "approval_request",
                json!({"id": "r1", "summary": "push"}),
            ),
            event(
                4,
                5_000,
                "t1",
                "tool",
                json!({"name": "Read", "detail": "src/x.rs"}),
            ),
        ] {
            store.append(&e).unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame = view.frame(NOW, None);
        let text = frame.join("\n");
        assert!(
            text.contains("running 1, waiting for answers 1, queued 1"),
            "{text}"
        );
        assert!(text.contains("(1 stopped over an hour ago; --all shows them)"));
        assert!(text.contains("\nclaude session s1\n"), "{text}");
        assert!(
            text.contains("\nuser: terminals and unrecognized agents\n"),
            "{text}"
        );
        assert!(!text.contains("t4 "), "{text}");
        let t1 = line_of(&frame, "t1");
        assert!(t1.contains("审查"), "{t1}");
        assert!(
            t1.contains(
                "approval r1 pending; turn 2, 2m00s, 2 tool call(s), last 5s ago: Read: src/x.rs"
            ),
            "{t1}"
        );
        for child in ["t2", "t3"] {
            assert_eq!(indent(line_of(&frame, child)), indent(t1) + 2, "{text}");
        }
        assert!(line_of(&frame, "t3").contains("question q1 unanswered"));
        assert!(line_of(&frame, "t5").contains("idle"));
        assert!(indent(line_of(&frame, "t6")) > indent(line_of(&frame, "t5")));
        assert!(
            text.ends_with(
                "
    5s  [t1 codex] Read: src/x.rs"
            ),
            "{text}"
        );

        // A line still being written is left for the next refresh.
        let more = serde_json::to_string(&event(
            5,
            1_000,
            "t1",
            "tool",
            json!({"name": "Edit", "detail": "a.rs"}),
        ))
        .unwrap();
        let (head, rest) = more.split_at(20);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(store.events_path("t1"))
            .unwrap();
        f.write_all(head.as_bytes()).unwrap();
        view.refresh();
        assert!(line_of(&view.frame(NOW, None), "t1").contains("2 tool call(s)"));
        f.write_all(format!("{rest}\n").as_bytes()).unwrap();
        drop(f);
        store
            .append(&event(
                6,
                500,
                "t1",
                "approved",
                json!({"id": "r1", "by": "user"}),
            ))
            .unwrap();
        view.refresh();
        let t1 = view.frame(NOW, None);
        let t1 = line_of(&t1, "t1");
        assert!(
            t1.contains("3 tool call(s), last 1s ago: Edit: a.rs"),
            "{t1}"
        );
        assert!(!t1.contains("approval"), "{t1}");

        // Nothing the daemon delivers was changed.
        for t in store.load_all() {
            assert_eq!(t.delivered, 0);
        }
        assert!(!h.0.join("ledger.json").exists() && !h.0.join("daemon").exists());

        let mut everything = View::new(&h.0, true);
        everything.refresh();
        assert!(
            everything
                .frame(NOW, None)
                .iter()
                .any(|l| l.contains("t4 "))
        );
    }

    #[test]
    fn a_fitted_frame_stays_inside_the_window() {
        let h = home();
        let store = Store::new(&h.0);
        for i in 0..30 {
            store
                .save(&task(
                    &format!("t{i}"),
                    "codex:s",
                    "running",
                    NOW,
                    json!({}),
                ))
                .unwrap();
        }
        // Agent text that would move the cursor and clear the screen.
        let detail = "printf '\x1b[2J\x1b[H'\r\nrm -rf build";
        for i in 0..10 {
            store
                .append(&event(
                    i,
                    1_000,
                    "t0",
                    "tool",
                    json!({"name": "Bash", "detail": detail}),
                ))
                .unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame = view.frame(NOW, Some((40, 12)));
        assert_eq!(frame.len(), 12, "{frame:#?}");
        let text = frame.join("\n");
        assert_eq!(strip_sgr(&frame[6]), "  …and 28 more lines", "{text}");
        assert!(text.contains("Latest events"), "{text}");
        for line in &frame {
            let plain = strip_sgr(line);
            assert!(!plain.contains('\x1b'), "{line:?}");
            assert!(columns(&plain) <= 39, "{plain:?}");
        }
        // The last three events fill the feed.
        assert!(
            strip_sgr(&frame[11]).contains("[t0 codex] Bash: printf"),
            "{frame:#?}"
        );
    }

    fn strip_sgr(line: &str) -> String {
        let mut out = String::new();
        let mut rest = line;
        while let Some(start) = rest.find("\x1b[") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find(|c: char| !c.is_ascii_digit()) {
                Some(end) if after[end..].starts_with('m') => rest = &after[end + 1..],
                _ => {
                    out.push_str(&rest[start..]);
                    return out;
                }
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(fit("审查任务", 5), ("审查…".to_string(), true));
        assert_eq!(fit("审查", 4), ("审查".to_string(), false));
        assert_eq!(pad("审", 4), "审  ");
        assert_eq!(fit("abc", 0), (String::new(), true));
        assert_eq!(columns("🚀⏳✅"), 6);
    }

    #[test]
    fn turns_follow_the_daemon_account() {
        let h = home();
        let store = Store::new(&h.0);
        let save = |turns: u32| {
            store
                .save(&task(
                    "t1",
                    "codex:s",
                    "running",
                    NOW,
                    json!({"turns": turns}),
                ))
                .unwrap()
        };
        save(0);
        let mut view = View::new(&h.0, false);
        let mut activity = |e: Event| {
            store.append(&e).unwrap();
            view.refresh();
            view.activity(&view.tasks["t1"], NOW)
        };
        assert_eq!(
            activity(event(1, 9_000, "t1", "turn_started", json!({"turn": 1}))),
            "turn 1, 9s, no tool calls yet"
        );
        assert_eq!(
            activity(event(
                2,
                8_000,
                "t1",
                "approval_request",
                json!({"id": "r9"})
            )),
            "approval r9 pending; turn 1, 9s, no tool calls yet"
        );
        activity(event(3, 7_000, "t1", "denied", json!({"id": "r9"})));
        // The process stays for background work, which may start a turn of its own.
        save(1);
        assert_eq!(
            activity(event(4, 6_000, "t1", "turn_ended", json!({"turn": 1}))),
            "turn 1 ended 6s ago; background work still runs"
        );
        assert_eq!(
            activity(event(
                5,
                2_000,
                "t1",
                "tool",
                json!({"name": "monitor", "detail": ""})
            )),
            "turn 2, 2s, 1 tool call(s), last 2s ago: monitor"
        );

        // What waits for someone stays in view in a narrow window.
        store
            .append(&event(
                6,
                1_000,
                "t1",
                "approval_request",
                json!({"id": "r10"}),
            ))
            .unwrap();
        view.refresh();
        let frame = view.frame(NOW, Some((70, 20)));
        let row = strip_sgr(frame.iter().find(|l| l.contains("t1 ")).unwrap());
        assert!(
            row.contains("approval r10 pending") && columns(&row) <= 69,
            "{row}"
        );
    }

    #[test]
    fn ages_past_two_days_count_days() {
        assert_eq!(age(47 * 3_600_000 + 60_000), "47h01m");
        assert_eq!(age(435 * 3_600_000 + 54 * 60_000), "18d03h");
    }

    #[test]
    fn the_daemon_can_write_while_top_holds_the_files_open() {
        let h = home();
        let store = Store::new(&h.0);
        let t = task("t1", "user", "running", NOW, json!({}));
        store.save(&t).unwrap();
        store
            .append(&event(1, 0, "t1", "spawned", json!({})))
            .unwrap();
        let _task = File::open(h.0.join("tasks").join("t1").join("task.json")).unwrap();
        let _events = File::open(store.events_path("t1")).unwrap();
        // Replacing task.json and appending events must not fail on Windows either.
        store.save(&t).unwrap();
        store
            .append(&event(2, 0, "t1", "turn_started", json!({"turn": 1})))
            .unwrap();
    }
}
