//! `axiopera top`: every task on one line, with what matters most of it now, redrawn each
//! second for a person watching sub-agents work; `top <task>` shows one task in detail. It only
//! reads the data directory and never connects to the daemon, so it starts nothing, takes no
//! messages and marks no result delivered.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::daemon::render;
use crate::identity;
use crate::platform;
use crate::store::{self, Event, Status, Store, Task};

const REFRESH: Duration = Duration::from_secs(1);
/// How long a stopped task stays in view after its last change, without `--all`.
const RECENT_MS: u64 = 60 * 60 * 1000;
/// Events of each task kept for its own view.
const TASK_EVENTS: usize = 200;
/// Events shown in a task's view when the output is not a terminal.
const PLAIN_FEED: usize = 20;
/// Lines of a result shown in a task's view when the output is not a terminal; its task text
/// is shown whole.
const PLAIN_RESULT: usize = 8;
/// Events kept in a task's view on a short window.
const MIN_EVENTS: usize = 3;
/// Bytes read of a task text or a result. Past them, the view points to the file.
const READ_BYTES: u64 = 64 * 1024;
/// The rank of a line that goes only with the lines of its tasks: a session's line.
const WITH_TASKS: u8 = u8::MAX;
/// Assumed when the terminal does not tell its size.
const FALLBACK_SIZE: (usize, usize) = (100, 30);
/// Characters of a task name shown.
const NAME: usize = 24;
/// Characters of a host session's ID shown: enough to tell sessions apart.
const SESSION: usize = 8;
/// Columns of the agent cell of a task's line.
const AGENT: usize = 6;
/// Columns of the state cell: its longest word, `interrupted`, so that a state that comes and
/// goes does not move the cells after it.
const STATE: usize = 11;

const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

/// Text and the SGR color it is shown in, empty for none.
type Seg = (String, &'static str);

pub async fn run(home: &Path, task: Option<&str>, all: bool) -> Result<i32> {
    let mut view = View::new(home, all);
    view.refresh();
    let focus = task.map(|r| view.find(r)).transpose()?;
    let draw = |view: &View, size| match &focus {
        Some(id) => view.task_frame(id, store::now_ms(), size),
        None => view.frame(store::now_ms(), size),
    };
    if !std::io::stdout().is_terminal() {
        let mut out = std::io::stdout().lock();
        for line in draw(&view, None) {
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
        let size = platform::terminal_size()
            .filter(|&(w, h)| w > 0 && h > 0)
            .or_else(env_size)
            .unwrap_or(FALLBACK_SIZE);
        // Drawn over the last frame instead of after clearing it, which would flicker.
        let lines = draw(&view, Some(size));
        if let Err(e) = show(&format!("\x1b[H{}\x1b[K\x1b[J", lines.join("\x1b[K\r\n"))) {
            break Err(e);
        }
        tokio::select! {
            _ = &mut stop => break Ok(()),
            () = tokio::time::sleep(REFRESH) => {}
        }
        view.refresh();
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

/// Events that change where a task stands, as opposed to its moment-to-moment work.
fn is_change(kind: &str) -> bool {
    !matches!(kind, "tool" | "said" | "file_changed" | "input_read")
}

/// What has been read of the data directory so far.
struct View {
    store: Store,
    all: bool,
    tasks: HashMap<String, Task>,
    tails: HashMap<String, Tail>,
}

impl View {
    fn new(home: &Path, all: bool) -> View {
        View {
            store: Store::new(home),
            all,
            tasks: HashMap::new(),
            tails: HashMap::new(),
        }
    }

    /// Read every task again, and what was added since the last refresh.
    fn refresh(&mut self) {
        let mut tasks: HashMap<String, Task> = self
            .store
            .load_all()
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        // Only a removed task is forgotten. One whose `task.json` cannot be read for a moment,
        // as while the daemon replaces it, stays as last read instead of leaving the screen.
        for (id, t) in std::mem::take(&mut self.tasks) {
            if !tasks.contains_key(&id) && self.store.exists(&id) {
                tasks.insert(id, t);
            }
        }
        self.tasks = tasks;
        let store = &self.store;
        self.tails.retain(|id, _| store.exists(id));
        for (id, t) in &self.tasks {
            // `prompt.md` is written before `task.json`, so it is whole once the task is there.
            let tail = self.tails.entry(id.clone()).or_insert_with(|| Tail {
                prompt: read_text(&self.store.prompt_path(id)),
                ..Tail::default()
            });
            // The daemon counts a turn before writing its result, so the file may be missing
            // or still being written: it is read again until its size stays.
            if t.turns > 0 {
                let path = self.store.turn_path(id, t.turns);
                let read = tail.result.as_ref().is_some_and(|(n, r)| {
                    *n == t.turns && fs::metadata(&path).is_ok_and(|m| m.len() == r.len)
                });
                if !read {
                    tail.result = read_text(&path).map(|r| (t.turns, r));
                }
            }
            for e in tail.read(&self.store.events_path(id)) {
                tail.absorb(&e);
            }
        }
    }

    /// A task by its ID, or by its name when no other task has it.
    fn find(&self, reference: &str) -> Result<String> {
        if self.tasks.contains_key(reference) {
            return Ok(reference.into());
        }
        let mut named: Vec<&str> = self
            .tasks
            .values()
            .filter(|t| t.name.as_deref() == Some(reference))
            .map(|t| t.id.as_str())
            .collect();
        named.sort();
        match named.as_slice() {
            [id] => Ok(id.to_string()),
            [] => bail!("no task {reference}"),
            ids => bail!(
                "{} tasks are named {reference}; give one of their IDs: {}",
                ids.len(),
                ids.join(", ")
            ),
        }
    }

    fn parent(&self, t: &Task) -> Option<&Task> {
        identity::task_of(&t.parent).and_then(|id| self.tasks.get(id))
    }

    fn tail(&self, id: &str) -> &Tail {
        static NONE: Tail = Tail::empty();
        self.tails.get(id).unwrap_or(&NONE)
    }

    /// A line for each task: fitted to `size` with colors, or whole and plain without it.
    fn frame(&self, now: u64, size: Option<(usize, usize)>) -> Vec<String> {
        let (header, rows, hidden) = self.table(now);
        let count = |s: Status| self.tasks.values().filter(|t| t.status == s).count();
        let approvals: usize = self
            .tasks
            .values()
            .filter(|t| t.status == Status::Running)
            .map(|t| self.tail(&t.id).approvals.len())
            .sum();
        let counts: Vec<String> = [
            (count(Status::Running), "running"),
            (count(Status::Waiting), "waiting"),
            (count(Status::Queued), "queued"),
            (approvals, "approval(s)"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        let counts = match counts.is_empty() {
            true => "nothing running".to_string(),
            false => counts.join(", "),
        };
        let mut title: Vec<Seg> = vec![("axiopera top".into(), BOLD), (format!("  {counts}"), "")];
        if hidden > 0 {
            title.push((format!("  {hidden} older hidden (--all)"), DIM));
        }
        let mut lines = vec![title, vec![]];
        if rows.is_empty() {
            lines.push(vec![("no tasks".into(), DIM)]);
            return finish(&lines, size);
        }
        lines.push(header);
        match size {
            None => lines.extend(rows.into_iter().map(|r| r.segs)),
            Some((_, height)) => {
                let room = height.saturating_sub(lines.len());
                lines.extend(choose(rows, room));
            }
        }
        finish(&lines, size)
    }

    /// A line for each task, sub-tasks under their parent, and with several sessions a line
    /// for each above the tasks it spawned; with the columns' titles. Also returns how many
    /// stopped tasks are left out.
    fn table(&self, now: u64) -> (Vec<Seg>, Vec<Row>, usize) {
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
        // Sessions in the order they first spawned, hidden tasks included, so that a session
        // whose oldest task leaves the list does not move after another.
        let mut first: HashMap<&str, u64> = HashMap::new();
        for t in self.tasks.values().filter(|t| self.parent(t).is_none()) {
            let at = first.entry(t.parent.as_str()).or_insert(u64::MAX);
            *at = (*at).min(t.created_ms);
        }
        groups.sort_by_key(|(caller, _)| (first.get(caller).copied(), *caller));
        // Session IDs are cut unless that leaves two alike.
        let short: Vec<String> = groups
            .iter()
            .map(|(c, _)| caller_label(c, SESSION))
            .collect();
        let labels: HashMap<&str, String> = groups
            .iter()
            .zip(&short)
            .map(
                |((caller, _), label)| match short.iter().filter(|l| *l == label).count() {
                    1 => (*caller, label.clone()),
                    _ => (*caller, caller_label(caller, usize::MAX)),
                },
            )
            .collect();
        // Lines in order: `None` for a session's, else the task with its ID cell, indented
        // by its depth.
        let mut order: Vec<(&str, Option<(String, &Task)>)> = Vec::new();
        for (caller, roots) in &groups {
            order.push((*caller, None));
            let mut stack: Vec<(usize, &Task)> = roots.iter().rev().map(|t| (1, *t)).collect();
            while let Some((depth, t)) = stack.pop() {
                order.push((
                    *caller,
                    Some((format!("{}{}", "  ".repeat(depth), t.id), t)),
                ));
                if let Some(kids) = children.get(t.id.as_str()) {
                    stack.extend(kids.iter().rev().map(|k| (depth + 1, *k)));
                }
            }
        }
        let widths = widths(order.iter().filter_map(|(_, row)| row.as_ref()));
        let several = groups.len() > 1;
        let mut rows = Vec::new();
        let mut lines: HashMap<&str, usize> = HashMap::new();
        let mut group = None;
        for (caller, row) in order {
            let Some((cell, t)) = row else {
                if several {
                    group = Some(rows.len());
                    rows.push(Row {
                        rank: WITH_TASKS,
                        ..Row::new(vec![(labels[caller].clone(), DIM)])
                    });
                }
                continue;
            };
            // A task is shown with its session and its ancestors, so it is seen where it is.
            let mut with: Vec<usize> = group.into_iter().collect();
            let mut up = self.parent(t);
            while let Some(p) = up {
                with.extend(lines.get(p.id.as_str()));
                up = self.parent(p);
            }
            lines.insert(&t.id, rows.len());
            rows.push(Row {
                with,
                ..self.task_row(&cell, t, widths, now)
            });
        }
        (widths.header(), rows, hidden)
    }

    /// A task on one line: its ID cell, name, agent and state, the time since it last
    /// changed, and what matters most of it now.
    fn task_row(&self, cell: &str, t: &Task, widths: Widths, now: u64) -> Row {
        let tail = self.tail(&t.id);
        let (state, color) = state(t, tail);
        let (text, text_color) = latest(t, tail);
        let segs = vec![
            (
                format!(
                    "{}  {}  {}  ",
                    pad(cell, widths.id),
                    pad(&name_cell(t), widths.name),
                    pad(render::agent_name(t.agent), AGENT)
                ),
                "",
            ),
            (pad(state, STATE), color),
            (
                format!(
                    "  {:>4}  ",
                    short_age(now.saturating_sub(last_change(t, tail)))
                ),
                DIM,
            ),
            (text, text_color),
        ];
        Row {
            rank: rank(t, tail),
            spawned: t.created_ms,
            ..Row::new(segs)
        }
    }

    /// One task in detail: what it was given, what it waits for and is doing, its sub-tasks,
    /// its latest result, and its events.
    fn task_frame(&self, id: &str, now: u64, size: Option<(usize, usize)>) -> Vec<String> {
        let Some(t) = self.tasks.get(id) else {
            let gone = format!("{id} is gone: it was removed, by gc or by hand");
            return finish(&[vec![(gone, DIM)]], size);
        };
        let tail = self.tail(id);
        let ago = |ms: u64| age(now.saturating_sub(ms));
        let (state, color) = state(t, tail);
        let title: Vec<Seg> = vec![
            (format!("axiopera top {id}"), BOLD),
            (format!("  {}  {}  ", name_cell(t), agent_cell(t)), ""),
            (state.into(), color),
            (format!("  {}", meta(t, tail, now)), ""),
        ];
        let mut about = format!(
            "spawned {} ago by {} in {}",
            ago(t.created_ms),
            caller_label(&t.parent, usize::MAX),
            t.cwd.display()
        );
        if let Some(model) = &t.model {
            about.push_str(&format!(", model {model}"));
        }
        if let Some(effort) = &t.effort {
            about.push_str(&format!(", effort {effort}"));
        }
        let mut lines = vec![
            title,
            vec![(about, DIM)],
            vec![],
            vec![("Task".into(), BOLD)],
        ];
        let width = size.map(|(w, _)| w);
        let task_text = match &tail.prompt {
            Some(text) => text.wrapped(width, self.store.prompt_path(id)),
            None => Wrapped::note("  not recorded: spawned before axiopera kept it"),
        };
        let now_rows = details(t, tail);
        let mut kids: Vec<&Task> = self
            .tasks
            .values()
            .filter(|k| identity::task_of(&k.parent) == Some(id))
            .collect();
        kids.sort_by_key(|k| (k.created_ms, k.id.as_str()));
        let kids: Vec<(String, &Task)> = kids
            .into_iter()
            .map(|k| (format!("  {}", k.id), k))
            .collect();
        let kid_widths = widths(kids.iter());
        let kids: Vec<Row> = kids
            .iter()
            .map(|(cell, k)| self.task_row(cell, k, kid_widths, now))
            .collect();
        let result = tail
            .result
            .as_ref()
            .map(|(turn, r)| (turn, r.wrapped(width, self.store.turn_path(id, *turn))));
        let mut events: Vec<Vec<Seg>> = tail.recent.iter().map(|e| event_row(e, t, now)).collect();

        let (task_room, kids_room, result_room, events_room) = match size {
            None => (usize::MAX, usize::MAX, PLAIN_RESULT, PLAIN_FEED),
            Some((_, height)) => {
                // A blank line and a title before each section after the task text.
                let titles = |n: usize| if n > 0 { 2 } else { 0 };
                let fixed = lines.len()
                    + titles(now_rows.len())
                    + now_rows.len()
                    + titles(kids.len())
                    + titles(usize::from(result.is_some()))
                    + 2;
                let room = height.saturating_sub(fixed);
                let events_min = events.len().min(MIN_EVENTS);
                let result_len = result.as_ref().map_or(0, |(_, r)| r.len());
                // Enough for a text to say where all of it is, and for one sub-task or the
                // count of them.
                let mins = [
                    task_text.len().min(task_text.rest_len().max(1)),
                    kids.len().min(1),
                    result
                        .as_ref()
                        .map_or(0, |(_, r)| r.len().min(r.rest_len().max(1))),
                ];
                let wants = [task_text.len(), kids.len(), result_len];
                let got = share(&wants, &mins, room.saturating_sub(events_min));
                let used: usize = got.iter().sum();
                (
                    got[0],
                    got[1],
                    got[2],
                    room.saturating_sub(used).max(events_min),
                )
            }
        };
        lines.extend(task_text.cut(task_room));
        if !now_rows.is_empty() {
            lines.push(vec![]);
            lines.push(vec![("Now".into(), BOLD)]);
            lines.extend(now_rows.iter().map(|d| d.row("  ", now)));
        }
        if !kids.is_empty() {
            lines.push(vec![]);
            lines.push(vec![("Sub-tasks".into(), BOLD)]);
            lines.extend(choose(kids, kids_room));
        }
        if let Some((turn, r)) = result {
            lines.push(vec![]);
            lines.push(vec![(format!("Result of turn {turn}"), BOLD)]);
            lines.extend(r.cut(result_room));
        }
        lines.push(vec![]);
        lines.push(vec![("Events".into(), BOLD)]);
        events.drain(..events.len().saturating_sub(events_room));
        // Only a window too short for the least of each section has more lines than it holds.
        if let Some((_, height)) = size {
            cut(&mut lines, height.saturating_sub(events.len()));
        }
        lines.extend(events);
        finish(&lines, size)
    }
}

/// Widths of the ID and name columns of task lines: those of their widest cells, so that what
/// matters most of each task has the rest.
#[derive(Clone, Copy)]
struct Widths {
    id: usize,
    name: usize,
}

/// Widths for these task lines, given with their ID cells.
fn widths<'a>(lines: impl Iterator<Item = &'a (String, &'a Task)>) -> Widths {
    let least = Widths {
        id: columns("  TASK"),
        name: columns("NAME"),
    };
    lines.fold(least, |w, (cell, t)| Widths {
        id: w.id.max(columns(cell)),
        name: w.name.max(columns(&name_cell(t)).min(NAME)),
    })
}

impl Widths {
    /// The columns' titles.
    fn header(self) -> Vec<Seg> {
        vec![(
            format!(
                "{}  {}  {}  {}  {:>4}  LATEST",
                pad("  TASK", self.id),
                pad("NAME", self.name),
                pad("AGENT", AGENT),
                pad("STATE", STATE),
                "AGO"
            ),
            DIM,
        )]
    }
}

/// Lines for sections that want `wants` lines, out of `room`: first their `mins`, then an
/// equal share each of what is left, up to what they want.
fn share(wants: &[usize], mins: &[usize], room: usize) -> Vec<usize> {
    let mut got = mins.to_vec();
    let mut left = room.saturating_sub(mins.iter().sum());
    loop {
        let open: Vec<usize> = (0..wants.len()).filter(|&i| got[i] < wants[i]).collect();
        if open.is_empty() || left == 0 {
            return got;
        }
        let each = (left / open.len()).max(1);
        for i in open {
            let give = each.min(wants[i] - got[i]).min(left);
            got[i] += give;
            left -= give;
        }
    }
}

/// When a task last changed, by its own account or by its latest event.
fn last_change(t: &Task, tail: &Tail) -> u64 {
    tail.recent.back().map_or(0, |e| e.ts).max(t.updated_ms)
}

/// The lines painted, and no more than the window holds.
fn finish(lines: &[Vec<Seg>], size: Option<(usize, usize)>) -> Vec<String> {
    let width = size.map(|(w, _)| w);
    let mut out: Vec<String> = lines.iter().map(|l| paint(l, width)).collect();
    if let Some((_, height)) = size {
        out.truncate(height);
    }
    out
}

/// Rows cut to `room`, the last one saying how many more there were.
fn cut(rows: &mut Vec<Vec<Seg>>, room: usize) {
    if rows.len() <= room {
        return;
    }
    let kept = room.saturating_sub(1);
    let more = rows.len() - kept;
    rows.truncate(kept);
    if room > 0 {
        rows.push(vec![(format!("  …and {more} more lines"), DIM)]);
    }
}

/// A line of a list, with what decides whether it is shown when the window is too short.
struct Row {
    segs: Vec<Seg>,
    /// Lines of lower rank are shown first: see `rank`.
    rank: u8,
    /// When its task was spawned: of lines with the same rank, the later are shown first.
    spawned: u64,
    /// The lines it is shown with: its session's and its ancestors'.
    with: Vec<usize>,
}

impl Row {
    fn new(segs: Vec<Seg>) -> Row {
        Row {
            segs,
            rank: 0,
            spawned: 0,
            with: Vec::new(),
        }
    }
}

/// How much a task matters to someone watching: what waits for them, then what is at work,
/// then what went wrong, then the rest.
fn rank(t: &Task, tail: &Tail) -> u8 {
    if waits(t, tail) {
        0
    } else if matches!(t.status, Status::Running | Status::Queued) {
        1
    } else if matches!(t.status, Status::Failed | Status::Interrupted) || t.detached {
        2
    } else {
        3
    }
}

/// As many rows as fit in `room`, in their order. When not all fit, the lowest ranks are
/// chosen, the later spawned of the same rank first, and the last line says how many are left
/// out. Rows are not chosen by when they last changed: those shown would change every second.
fn choose(rows: Vec<Row>, room: usize) -> Vec<Vec<Seg>> {
    if rows.len() <= room {
        return rows.into_iter().map(|r| r.segs).collect();
    }
    let budget = room.saturating_sub(1);
    let mut order: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].rank != WITH_TASKS)
        .collect();
    order.sort_by_key(|&i| (rows[i].rank, Reverse(rows[i].spawned), Reverse(i)));
    let mut kept = vec![false; rows.len()];
    let mut used = 0;
    for i in order {
        let cost = usize::from(!kept[i]) + rows[i].with.iter().filter(|&&w| !kept[w]).count();
        if used + cost > budget {
            continue;
        }
        used += cost;
        kept[i] = true;
        for &w in &rows[i].with {
            kept[w] = true;
        }
    }
    let left = rows
        .iter()
        .zip(&kept)
        .filter(|(r, k)| !**k && r.rank != WITH_TASKS)
        .count();
    let mut out: Vec<Vec<Seg>> = rows
        .into_iter()
        .zip(kept)
        .filter(|(_, k)| *k)
        .map(|(r, _)| r.segs)
        .collect();
    if room > 0 && left > 0 {
        out.push(vec![(format!("  …and {left} more task(s)"), DIM)]);
    }
    out
}

/// An event on one line. Tool calls are dimmed, so that what the agent says and how the
/// task changes stand out among them.
fn event_row(e: &Event, t: &Task, now: u64) -> Vec<Seg> {
    let color = if is_change(&e.kind) || e.kind == "said" {
        ""
    } else {
        DIM
    };
    vec![
        (format!("{:>6}  ", age(now.saturating_sub(e.ts))), DIM),
        (render::event_line(e, t.agent), color),
    ]
}

fn name_cell(t: &Task) -> String {
    t.name.clone().unwrap_or_else(|| "-".into())
}

fn agent_cell(t: &Task) -> String {
    format!(
        "{}/{}",
        render::agent_name(t.agent),
        render::tier_name(t.tier)
    )
}

/// Where a task's turn stands, in a few words.
fn meta(t: &Task, tail: &Tail, now: u64) -> String {
    let ago = |ms: u64| age(now.saturating_sub(ms));
    match t.status {
        Status::Running if tail.in_turn => format!(
            "turn {}, {}, {} tool call(s)",
            t.turns + 1,
            ago(tail.started_ms),
            tail.tool_calls
        ),
        Status::Running if tail.ended_ms > 0 => format!(
            "turn {} ended {} ago; background work still runs",
            t.turns,
            ago(tail.ended_ms)
        ),
        Status::Running => "starting".to_string(),
        Status::Queued => "about to start".to_string(),
        _ => {
            let mut done = format!("{} turn(s), last change {} ago", t.turns, ago(t.updated_ms));
            if !t.changed_files.is_empty() {
                done.push_str(&format!(", {} file(s) changed", t.changed_files.len()));
            }
            done
        }
    }
}

/// A line under Now in a task's view: what it waits for, says or does, or how it ended badly.
struct Detail {
    label: &'static str,
    at: Option<u64>,
    text: String,
    color: &'static str,
}

impl Detail {
    fn row(&self, indent: &str, now: u64) -> Vec<Seg> {
        let at = self
            .at
            .map(|at| age(now.saturating_sub(at)))
            .unwrap_or_default();
        vec![
            (format!("{indent}{:<6} {at:>6}  ", self.label), DIM),
            (self.text.clone(), self.color),
        ]
    }
}

/// What a task waits for, what it says and does, or how it ended badly.
fn details(t: &Task, tail: &Tail) -> Vec<Detail> {
    let mut out = Vec::new();
    let line = |label, at, text: String, color| Detail {
        label,
        at,
        text,
        color,
    };
    if t.status == Status::Running {
        for (id, summary) in &tail.approvals {
            out.push(line(
                "needs",
                None,
                format!("approval {id}: {summary}"),
                YELLOW,
            ));
        }
    }
    if !t.status.is_final() {
        for q in t.open_questions() {
            out.push(line(
                "asks",
                Some(q.asked_ms),
                format!("{}: {}", q.id, q.text),
                YELLOW,
            ));
        }
    }
    match t.status {
        Status::Running | Status::Queued => {
            if let Some((text, at)) = &tail.said {
                out.push(line("says", Some(*at), text.clone(), ""));
            }
            if let (true, Some((call, at))) = (tail.in_turn, &tail.last_tool) {
                out.push(line("doing", Some(*at), call.clone(), ""));
            }
        }
        _ => {
            if let Some((label, text, at)) = stop(t, tail) {
                out.push(line(label, Some(at), text.into(), RED));
            }
        }
    }
    out
}

/// How a stopped task ended badly, if it did: a label, the text and when.
fn stop<'a>(t: &Task, tail: &'a Tail) -> Option<(&'static str, &'a str, u64)> {
    let (label, text, at) = tail.stop.as_ref()?;
    (t.status != Status::Idle || t.detached).then_some((*label, text.as_str(), *at))
}

/// Whether a task waits for someone: for an answer, or for an approval.
fn waits(t: &Task, tail: &Tail) -> bool {
    t.status == Status::Waiting
        || t.status == Status::Running && !tail.approvals.is_empty()
        || !t.status.is_final() && t.open_questions().next().is_some()
}

/// What matters most of a task now, for its line: what it waits for, what it last said or
/// else is doing, how it ended, or else what it was given. A tool call or a task text is dim,
/// so that what the agent says stands out among them.
fn latest(t: &Task, tail: &Tail) -> (String, &'static str) {
    let more = |n: usize| match n {
        0 | 1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    if t.status == Status::Running
        && let Some((id, summary)) = tail.approvals.first()
    {
        return (
            one_line(&format!("{id}: {summary}{}", more(tail.approvals.len()))),
            "",
        );
    }
    if !t.status.is_final() {
        let open: Vec<_> = t.open_questions().collect();
        if let Some(q) = open.first() {
            return (
                one_line(&format!("{}: {}{}", q.id, q.text, more(open.len()))),
                "",
            );
        }
    }
    let first = |text: Option<&Text>| text?.lines.first().cloned();
    let found = match t.status {
        Status::Running | Status::Queued => tail
            .said
            .as_ref()
            .map(|(text, _)| (text.clone(), ""))
            .or_else(|| {
                let (call, _) = tail.last_tool.as_ref().filter(|_| tail.in_turn)?;
                Some((call.clone(), DIM))
            }),
        _ => stop(t, tail)
            .map(|(_, text, _)| (text.to_string(), ""))
            .or_else(|| Some((first(tail.result.as_ref().map(|(_, r)| r))?, ""))),
    };
    let (text, color) = found
        .or_else(|| Some((first(tail.prompt.as_ref())?, DIM)))
        .unwrap_or_default();
    (one_line(&text), color)
}

/// Text on one line: its lines and runs of spaces joined by single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Like the daemon's durations, with days for tasks left for long.
fn age(ms: u64) -> String {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    if ms < 2 * DAY_MS {
        return render::duration(ms);
    }
    format!("{}d{:02}h", ms / DAY_MS, ms % DAY_MS / (60 * 60 * 1000))
}

/// A duration in its largest unit, for a task's line: it changes once a minute past the first.
fn short_age(ms: u64) -> String {
    match ms / 1000 {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s / 60),
        s @ 3600..172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn active(t: &Task) -> bool {
    matches!(t.status, Status::Queued | Status::Running | Status::Waiting)
}

/// A task's state in a word and its color: the color goes to what waits for someone and what
/// went wrong, and what is at work, so that the rest can be passed over.
fn state(t: &Task, tail: &Tail) -> (&'static str, &'static str) {
    match t.status {
        _ if t.detached => ("detached", RED),
        Status::Running if !tail.approvals.is_empty() => ("approval", YELLOW),
        // The daemon leaves the mark on a task that stops while stalled.
        Status::Running if t.stalled => ("stalled", RED),
        Status::Queued => ("queued", DIM),
        Status::Running => ("running", GREEN),
        Status::Idle => ("idle", DIM),
        Status::Waiting => ("waiting", YELLOW),
        Status::Failed => ("failed", RED),
        Status::Interrupted => ("interrupted", RED),
        Status::Cancelled => ("cancelled", DIM),
    }
}

/// Who spawned a group of tasks: a host session, `user`, or a task already removed. Agents
/// other than the known hosts call as `user` too, like a person at a terminal. A session's ID
/// is cut to `session_chars` characters.
fn caller_label(caller: &str, session_chars: usize) -> String {
    match caller.split_once(':') {
        _ if caller == "user" => "user: terminals and unrecognized agents".into(),
        Some((host, session)) if host != "task" => {
            let session: String = session.chars().take(session_chars).collect();
            format!("{host} session {session}")
        }
        _ => caller.into(),
    }
}

/// The lines with text of a task text or a result.
struct Text {
    lines: Vec<String>,
    /// The file's size when read.
    len: u64,
    /// Whether `READ_BYTES` held all of it.
    whole: bool,
}

/// A file's lines with text, or `None` without the file.
fn read_text(path: &Path) -> Option<Text> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(READ_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let lines = String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim_end().to_string())
        .collect();
    Some(Text {
        lines,
        len,
        whole: bytes.len() as u64 >= len,
    })
}

impl Text {
    /// Indented, and broken to fit a window `width` wide.
    fn wrapped(&self, width: Option<usize>, path: PathBuf) -> Wrapped {
        let lines = self
            .lines
            .iter()
            .flat_map(|l| match width {
                // Two columns of indent, and the last column stays empty.
                Some(w) => wrap(&clean(l), w.saturating_sub(3)),
                None => vec![l.clone()],
            })
            .map(|l| vec![(format!("  {l}"), "")])
            .collect();
        Wrapped {
            lines,
            file: Some((path, self.whole)),
            width,
        }
    }
}

/// A text as lines in a task's view, and its file with whether the lines hold all of it.
struct Wrapped {
    lines: Vec<Vec<Seg>>,
    file: Option<(PathBuf, bool)>,
    /// The window's, for the file's path.
    width: Option<usize>,
}

impl Wrapped {
    fn note(text: &str) -> Wrapped {
        Wrapped {
            lines: vec![vec![(text.into(), DIM)]],
            file: None,
            width: None,
        }
    }

    /// The lines that say where all of it is.
    fn rest_len(&self) -> usize {
        self.rest(0).len()
    }

    /// Lines to show it all, or as much as was read and where the rest is.
    fn len(&self) -> usize {
        let rest = match self.file {
            Some((_, false)) => self.rest(0).len(),
            _ => 0,
        };
        self.lines.len() + rest
    }

    /// No more than `room` lines. When some are left out, the last say where all of it is.
    fn cut(self, room: usize) -> Vec<Vec<Seg>> {
        let whole = match &self.file {
            Some((_, whole)) => *whole,
            None => true,
        };
        if whole && self.lines.len() <= room || self.file.is_none() {
            return self.lines.into_iter().take(room).collect();
        }
        let kept = self
            .lines
            .len()
            .min(room.saturating_sub(self.rest(0).len()));
        let rest = self.rest(self.lines.len() - kept);
        let mut lines = self.lines;
        lines.truncate(kept);
        lines.extend(rest);
        lines
    }

    /// Where to read what `more` lines leave out: the file's path on lines of its own, whole
    /// however narrow the window.
    fn rest(&self, more: usize) -> Vec<Vec<Seg>> {
        let Some((path, whole)) = &self.file else {
            return Vec::new();
        };
        let head = match (whole, more) {
            (false, 0) => "  …the rest is in:".to_string(),
            _ => format!("  …{more} more line(s); all of it is in:"),
        };
        let path = path.display().to_string();
        let pieces = match self.width {
            // Two columns of indent, and the last column stays empty.
            Some(w) => chop(&path, w.saturating_sub(3)),
            None => vec![path],
        };
        std::iter::once(head)
            .chain(pieces.into_iter().map(|p| format!("  {p}")))
            .map(|l| vec![(l, DIM)])
            .collect()
    }
}

/// Text in pieces of up to `room` columns, broken anywhere: a path must keep its spaces.
fn chop(text: &str, room: usize) -> Vec<String> {
    let room = room.max(2);
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = char_columns(c);
        if used + w > room {
            out.push(String::new());
            used = 0;
        }
        out.last_mut().unwrap().push(c);
        used += w;
    }
    out
}

/// A line broken into pieces of up to `room` columns, after a space where there is one.
fn wrap(line: &str, room: usize) -> Vec<String> {
    let room = room.max(2);
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    for c in line.chars() {
        let w = char_columns(c);
        if used + w > room {
            if c == ' ' {
                out.push(std::mem::take(&mut piece));
                used = 0;
                continue;
            }
            match piece.rfind(' ') {
                Some(i) if i > 0 => {
                    let rest = piece.split_off(i + 1);
                    out.push(piece.trim_end().to_string());
                    piece = rest;
                }
                _ => out.push(std::mem::take(&mut piece)),
            }
            used = columns(&piece);
        }
        piece.push(c);
        used += w;
    }
    out.push(piece);
    out
}

/// What a task's files tell beyond its `task.json`.
#[derive(Default)]
struct Tail {
    /// Bytes of `events.jsonl` read, up to the end of the last whole line.
    offset: u64,
    in_turn: bool,
    started_ms: u64,
    ended_ms: u64,
    tool_calls: u64,
    last_tool: Option<(String, u64)>,
    /// The agent's latest message in this turn.
    said: Option<(String, u64)>,
    /// Approval requests of the current turn without a decision, with their summaries.
    approvals: Vec<(String, String)>,
    /// How the last turn or the task ended badly: a label, the text and when.
    stop: Option<(&'static str, String, u64)>,
    /// This task's latest events, oldest first.
    recent: VecDeque<Event>,
    /// `prompt.md`, read once.
    prompt: Option<Text>,
    /// The latest result read, and its turn.
    result: Option<(u32, Text)>,
}

impl Tail {
    const fn empty() -> Tail {
        Tail {
            offset: 0,
            in_turn: false,
            started_ms: 0,
            ended_ms: 0,
            tool_calls: 0,
            last_tool: None,
            said: None,
            approvals: Vec::new(),
            stop: None,
            recent: VecDeque::new(),
            prompt: None,
            result: None,
        }
    }

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
                self.stop = None;
            }
            "turn_ended" => {
                self.in_turn = false;
                self.ended_ms = e.ts;
                if e.data.get("ok") == Some(&Value::Bool(false)) {
                    self.stop = Some(("error", text("error").into(), e.ts));
                }
            }
            // A turn the agent started by itself that its adapter did not report begins with
            // what it says or does.
            "said" => {
                if !self.in_turn && self.ended_ms > 0 {
                    self.start(e.ts);
                }
                self.said = Some((text("text").into(), e.ts));
            }
            "tool" => {
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
            "approval_request" => self
                .approvals
                .push((text("id").into(), text("summary").into())),
            "approved" | "denied" => self.approvals.retain(|(id, _)| id != text("id")),
            "failed" => self.stop = Some(("error", text("error").into(), e.ts)),
            "cancelled" | "interrupted" | "detached" => {
                self.stop = Some(("why", text("reason").into(), e.ts));
            }
            _ => {}
        }
        if self.recent.len() == TASK_EVENTS {
            self.recent.pop_front();
        }
        self.recent.push_back(e.clone());
    }

    fn start(&mut self, at: u64) {
        self.in_turn = true;
        self.started_ms = at;
        self.tool_calls = 0;
        self.last_tool = None;
        self.said = None;
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
    const HOUR: u64 = 60 * 60 * 1000;

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

    fn line_of<'a>(frame: &'a [String], start: &str) -> &'a str {
        frame
            .iter()
            .find(|l| l.trim_start().starts_with(start))
            .unwrap_or_else(|| panic!("no line starting {start:?} in {frame:#?}"))
    }

    /// A task's line split into its cells: ID, name, agent, state, age and what matters most.
    fn cells<'a>(frame: &'a [String], id: &str) -> Vec<&'a str> {
        line_of(frame, &format!("{id} "))
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect()
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    #[test]
    fn each_task_is_one_line_of_what_matters_most_now() {
        let h = home();
        let store = Store::new(&h.0);
        for (t, prompt) in [
            (
                task(
                    "t1",
                    "claude:s1234567890",
                    "running",
                    NOW - 600_000,
                    json!({"name": "审查"}),
                ),
                Some("\nReview the parser.\n\nReport findings."),
            ),
            (
                task("t2", "task:t1", "idle", NOW - 500_000, json!({})),
                Some("Run the tests."),
            ),
            // Spawned by a native sub-agent inside t1: still under t1.
            (
                task(
                    "t3",
                    "task:t1/claude:native",
                    "waiting",
                    NOW - 400_000,
                    json!({"questions": [
                        {"id": "q1", "text": "Which\nport?", "answer": null, "asked_ms": NOW - 9_000}
                    ]}),
                ),
                None,
            ),
            (task("t4", "user", "idle", NOW - 2 * HOUR, json!({})), None),
            // Old, but its sub-task is queued.
            (task("t5", "user", "idle", NOW - 3 * HOUR, json!({})), None),
            (
                task("t6", "task:t5", "queued", NOW - 1_000, json!({})),
                Some("Check the adapter.\nThen report."),
            ),
            (task("t7", "user", "failed", NOW - 1_000, json!({})), None),
        ] {
            if let Some(text) = prompt {
                store.save_prompt(&t.id, text).unwrap();
            }
            store.save(&t).unwrap();
        }
        store
            .save_turn("t2", 1, "\n\nAll tests pass.\nDetails.\n")
            .unwrap();
        for e in [
            event(1, 120_000, "t1", "turn_started", json!({"turn": 2})),
            event(
                2,
                61_000,
                "t1",
                "said",
                json!({"text": "Running\n\nthe  tests."}),
            ),
            event(
                3,
                60_000,
                "t1",
                "tool",
                json!({"name": "Bash", "detail": "cargo test"}),
            ),
            event(
                4,
                30_000,
                "t1",
                "approval_request",
                json!({"id": "r1", "summary": "push"}),
            ),
            event(
                5,
                5_000,
                "t1",
                "tool",
                json!({"name": "Read", "detail": "src/x.rs"}),
            ),
            event(
                6,
                2_000,
                "t7",
                "failed",
                json!({"error": "429 rate limited"}),
            ),
        ] {
            store.append(&e).unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame = view.frame(NOW, None);
        let text = frame.join("\n");
        assert!(
            frame[0].starts_with(
                "axiopera top  1 running, 1 waiting, 1 queued, 1 approval(s)  1 older hidden (--all)"
            ),
            "{text}"
        );
        assert!(
            frame[2].trim_start().starts_with("TASK") && frame[2].ends_with("AGO  LATEST"),
            "{text}"
        );
        // With tasks of two sessions, a line for each, its ID cut to tell them apart.
        assert!(text.contains("\nclaude session s1234567\n"), "{text}");
        assert!(
            text.contains("\nuser: terminals and unrecognized agents\n"),
            "{text}"
        );
        assert!(!text.contains("t4 "), "{text}");

        // What waits for someone comes before what the agent says or does.
        assert_eq!(
            cells(&frame, "t1")[1..],
            ["审查", "codex", "approval", "5s", "r1: push"],
            "{text}"
        );
        for child in ["t2", "t3"] {
            assert_eq!(
                indent(line_of(&frame, &format!("{child} "))),
                indent(line_of(&frame, "t1 ")) + 2,
                "{text}"
            );
        }
        assert_eq!(cells(&frame, "t2")[3..], ["idle", "8m", "All tests pass."]);
        assert_eq!(
            cells(&frame, "t3")[3..],
            ["waiting", "6m", "q1: Which port?"]
        );
        assert_eq!(cells(&frame, "t5")[3..], ["idle", "3h"]);
        assert!(indent(line_of(&frame, "t6 ")) > indent(line_of(&frame, "t5 ")));
        // Before it says anything, what it was given.
        assert_eq!(
            cells(&frame, "t6")[3..],
            ["queued", "1s", "Check the adapter."]
        );
        assert_eq!(
            cells(&frame, "t7")[3..],
            ["failed", "1s", "429 rate limited"]
        );

        // A line still being written is left for the next refresh.
        let more = serde_json::to_string(&event(
            7,
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
        assert_eq!(cells(&view.frame(NOW, None), "t1")[4], "5s");
        f.write_all(format!("{rest}\n").as_bytes()).unwrap();
        drop(f);
        store
            .append(&event(
                8,
                500,
                "t1",
                "approved",
                json!({"id": "r1", "by": "user"}),
            ))
            .unwrap();
        view.refresh();
        // What it said stays over the tool calls after it, on one line.
        assert_eq!(
            cells(&view.frame(NOW, None), "t1")[3..],
            ["running", "0s", "Running the tests."]
        );

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
    fn one_task_in_detail() {
        let h = home();
        let store = Store::new(&h.0);
        let mut t1 = task(
            "t1",
            "claude:s1",
            "idle",
            NOW - 600_000,
            json!({"name": "review"}),
        );
        t1.model = Some("gpt-5.5".into());
        t1.turns = 2;
        store
            .save_prompt("t1", "Review the parser.\nReport findings.")
            .unwrap();
        store.save(&t1).unwrap();
        store
            .save_turn("t1", 2, "No bugs.\n\n- checked the reader\n")
            .unwrap();
        store
            .save(&task(
                "t2",
                "task:t1",
                "running",
                NOW - 60_000,
                json!({"name": "tests"}),
            ))
            .unwrap();
        store
            .save(&task("t3", "user", "idle", NOW, json!({"name": "tests"})))
            .unwrap();
        for e in [
            event(1, 50_000, "t1", "turn_started", json!({"turn": 2})),
            event(
                2,
                40_000,
                "t1",
                "said",
                json!({"text": "Checking the reader."}),
            ),
            event(
                3,
                30_000,
                "t1",
                "tool",
                json!({"name": "Read", "detail": "src/reader.rs"}),
            ),
            event(4, 20_000, "t1", "said", json!({"text": "No bugs."})),
            event(
                5,
                20_000,
                "t1",
                "turn_ended",
                json!({"turn": 2, "ok": true}),
            ),
            event(6, 10_000, "t2", "said", json!({"text": "Running them."})),
        ] {
            store.append(&e).unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        assert_eq!(view.find("review").unwrap(), "t1");
        assert_eq!(view.find("t3").unwrap(), "t3");
        assert_eq!(view.find("nope").unwrap_err().to_string(), "no task nope");
        assert_eq!(
            view.find("tests").unwrap_err().to_string(),
            "2 tasks are named tests; give one of their IDs: t2, t3"
        );

        let frame = view.task_frame("t1", NOW, None);
        let text = frame.join("\n");
        assert!(
            frame[0].starts_with("axiopera top t1  review  codex/work  idle  2 turn(s)"),
            "{text}"
        );
        assert!(frame[1].contains("by claude session s1") && frame[1].ends_with(", model gpt-5.5"));
        assert!(
            text.contains("\nTask\n  Review the parser.\n  Report findings.\n"),
            "{text}"
        );
        // The result is not repeated under Now.
        assert!(!text.contains("\nNow\n"), "{text}");
        // Sub-tasks on lines like the list's.
        assert!(text.contains("\nSub-tasks\n  t2  "), "{text}");
        assert_eq!(
            cells(&frame, "t2")[1..],
            ["tests", "codex", "running", "10s", "Running them."]
        );
        assert!(
            text.contains("\nResult of turn 2\n  No bugs.\n  - checked the reader\n"),
            "{text}"
        );
        let events = &text[text.find("\nEvents\n").unwrap()..];
        for line in [
            "says: Checking the reader.",
            "Read: src/reader.rs",
            "turn 2 finished",
        ] {
            assert!(events.contains(line), "{events}");
        }

        // Fitted to the window, events keep the bottom lines.
        let fitted = view.task_frame("t1", NOW, Some((60, 10)));
        assert_eq!(fitted.len(), 10, "{fitted:#?}");
        assert!(
            strip_sgr(&fitted[9]).contains("turn 2 finished"),
            "{fitted:#?}"
        );
        assert!(fitted.iter().all(|l| columns(&strip_sgr(l)) <= 59));

        std::fs::remove_dir_all(h.0.join("tasks").join("t1")).unwrap();
        view.refresh();
        assert_eq!(
            view.task_frame("t1", NOW, None),
            ["t1 is gone: it was removed, by gc or by hand"]
        );
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
        let mut state = |e: Event| {
            store.append(&e).unwrap();
            view.refresh();
            let t = &view.tasks["t1"];
            let tail = view.tail("t1");
            let lines: Vec<String> = details(t, tail)
                .iter()
                .map(|d| format!("{} {}", d.label, d.text))
                .collect();
            (meta(t, tail, NOW), lines, latest(t, tail).0)
        };
        assert_eq!(
            state(event(1, 9_000, "t1", "turn_started", json!({"turn": 1}))),
            (
                "turn 1, 9s, 0 tool call(s)".to_string(),
                vec![],
                String::new()
            )
        );
        let waits = state(event(
            2,
            8_000,
            "t1",
            "approval_request",
            json!({"id": "r9", "summary": "push"}),
        ));
        assert_eq!(
            (&waits.1[..], &waits.2[..]),
            (&["needs approval r9: push".to_string()][..], "r9: push")
        );
        state(event(3, 7_000, "t1", "denied", json!({"id": "r9"})));
        assert_eq!(
            state(event(4, 7_000, "t1", "said", json!({"text": "Done."}))).1,
            ["says Done."]
        );
        // The process stays for background work, which may start a turn of its own.
        save(1);
        assert_eq!(
            state(event(
                5,
                6_000,
                "t1",
                "turn_ended",
                json!({"turn": 1, "ok": true})
            ))
            .0,
            "turn 1 ended 6s ago; background work still runs"
        );
        // Words start it as well as a tool call, and are not lost to the call that follows.
        state(event(
            6,
            3_000,
            "t1",
            "said",
            json!({"text": "The build finished."}),
        ));
        assert_eq!(
            state(event(
                7,
                2_000,
                "t1",
                "tool",
                json!({"name": "monitor", "detail": ""})
            )),
            (
                "turn 2, 3s, 1 tool call(s)".to_string(),
                vec![
                    "says The build finished.".to_string(),
                    "doing monitor".to_string()
                ],
                "The build finished.".to_string()
            )
        );
        // What it said stays over the tool calls after it.
        let next = state(event(
            8,
            1_500,
            "t1",
            "tool",
            json!({"name": "Read", "detail": "x"}),
        ));
        assert_eq!(next.2, "The build finished.");
        assert_eq!(
            state(event(
                9,
                1_000,
                "t1",
                "turn_ended",
                json!({"turn": 2, "ok": false, "error": "boom"})
            ))
            .1,
            ["says The build finished."]
        );
        // A new turn forgets what was said in the last: its tool calls show until it speaks.
        state(event(10, 900, "t1", "turn_started", json!({"turn": 3})));
        let (_, _, text) = state(event(
            11,
            800,
            "t1",
            "tool",
            json!({"name": "Read", "detail": "y"}),
        ));
        assert_eq!(text, "Read: y");
        store
            .save(&task("t1", "codex:s", "failed", NOW, json!({"turns": 3})))
            .unwrap();
        store
            .append(&event(
                12,
                700,
                "t1",
                "turn_ended",
                json!({"turn": 3, "ok": false, "error": "boom"}),
            ))
            .unwrap();
        view.refresh();
        let t = &view.tasks["t1"];
        assert_eq!(details(t, view.tail("t1"))[0].text, "boom");
        assert_eq!(latest(t, view.tail("t1")).0, "boom");
    }

    #[test]
    fn a_fitted_frame_stays_inside_the_window() {
        let h = home();
        let store = Store::new(&h.0);
        for i in 0..30 {
            store
                .save(&task(
                    &format!("t{i:02}"),
                    "codex:s",
                    "running",
                    NOW,
                    json!({}),
                ))
                .unwrap();
        }
        // Agent text that would move the cursor and clear the screen.
        let detail = "printf '\x1b[2J\x1b[H'\r\nrm -rf build";
        store
            .append(&event(0, 2_000, "t00", "turn_started", json!({"turn": 1})))
            .unwrap();
        for i in 1..10 {
            store
                .append(&event(
                    i,
                    1_000,
                    "t00",
                    "tool",
                    json!({"name": "Bash", "detail": detail}),
                ))
                .unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame = view.frame(NOW, Some((40, 12)));
        assert_eq!(frame.len(), 12, "{frame:#?}");
        let plain: Vec<String> = frame.iter().map(|l| strip_sgr(l)).collect();
        // One session: no line for it. The latest tasks fill the window.
        assert!(plain[3].starts_with("  t22 "), "{plain:#?}");
        assert_eq!(plain[11], "  …and 22 more task(s)", "{plain:#?}");
        for (line, plain) in frame.iter().zip(&plain) {
            assert!(!plain.contains('\x1b'), "{line:?}");
            assert!(columns(plain) <= 39, "{plain:?}");
        }
        // The tool call shows on one line, dim, without its control characters.
        let all = view.frame(NOW, Some((100, 40)));
        let t00 = line_of(&all, "t00 ");
        assert!(
            t00.ends_with("\x1b[2mBash: printf ' [2J [H' rm -rf build\x1b[0m"),
            "{t00:?}"
        );

        // A task's work does not change which tasks are shown.
        store
            .append(&event(10, 0, "t00", "said", json!({"text": "Building."})))
            .unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, Some((40, 12))), frame);
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
    fn ages_past_two_days_count_days() {
        assert_eq!(age(47 * HOUR + 60_000), "47h01m");
        assert_eq!(age(435 * HOUR + 54 * 60_000), "18d03h");
        // A task's line has room for the largest unit only.
        let short: Vec<String> = [59_999, 60_000, HOUR - 1, HOUR, 48 * HOUR - 1, 48 * HOUR]
            .into_iter()
            .map(short_age)
            .collect();
        assert_eq!(short, ["59s", "1m", "59m", "1h", "47h", "2d"]);
    }

    #[test]
    fn the_daemon_can_write_while_top_holds_the_files_open() {
        let h = home();
        let store = Store::new(&h.0);
        let t = task("t1", "user", "running", NOW, json!({}));
        store.save_prompt("t1", "x").unwrap();
        store.save(&t).unwrap();
        store
            .append(&event(1, 0, "t1", "spawned", json!({})))
            .unwrap();
        let _task = File::open(h.0.join("tasks").join("t1").join("task.json")).unwrap();
        let _events = File::open(store.events_path("t1")).unwrap();
        let _prompt = File::open(store.prompt_path("t1")).unwrap();
        // Replacing task.json and appending events must not fail on Windows either.
        store.save(&t).unwrap();
        store
            .append(&event(2, 0, "t1", "turn_started", json!({"turn": 1})))
            .unwrap();
    }

    #[test]
    fn a_task_that_cannot_be_read_for_a_moment_keeps_its_line() {
        let h = home();
        let store = Store::new(&h.0);
        store
            .save(&task("t1", "user", "running", NOW, json!({})))
            .unwrap();
        store
            .save(&task("t2", "user", "idle", NOW, json!({})))
            .unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        let before = view.frame(NOW, None);
        std::fs::write(h.0.join("tasks").join("t1").join("task.json"), "{").unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, None), before);
        store.remove("t1").unwrap();
        view.refresh();
        let after = view.frame(NOW, None).join("\n");
        assert!(!after.contains("t1 ") && after.contains("t2 "), "{after}");
    }

    #[test]
    fn a_short_window_keeps_what_waits_for_someone_and_what_works() {
        let h = home();
        let store = Store::new(&h.0);
        // Older idle tasks come first in the list.
        for i in 0..8 {
            let id = format!("t{i}");
            store
                .save(&task(&id, "user", "idle", NOW - 50_000 + i, json!({})))
                .unwrap();
            store.save_turn(&id, 1, "Done.").unwrap();
        }
        store
            .save(&task("tp", "user", "idle", NOW - 40_000, json!({})))
            .unwrap();
        store
            .save(&task("tr", "task:tp", "running", NOW - 30_000, json!({})))
            .unwrap();
        store
            .save(&task(
                "tw",
                "codex:s",
                "waiting",
                NOW - 20_000,
                json!({"questions": [
                    {"id": "q1", "text": "Which port?", "answer": null, "asked_ms": NOW - 9_000}
                ]}),
            ))
            .unwrap();
        for e in [
            event(1, 9_000, "tr", "turn_started", json!({"turn": 1})),
            event(2, 8_000, "tr", "said", json!({"text": "Pushing."})),
            event(
                3,
                7_000,
                "tr",
                "approval_request",
                json!({"id": "r1", "summary": "push"}),
            ),
        ] {
            store.append(&e).unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        // 7 lines for the 13 of the table.
        let frame = view.frame(NOW, Some((100, 10)));
        let plain: Vec<String> = frame.iter().map(|l| strip_sgr(l)).collect();
        let text = plain.join("\n");
        assert_eq!(plain.len(), 10, "{text}");
        assert_eq!(
            cells(&plain, "tw")[3..],
            ["waiting", "20s", "q1: Which port?"]
        );
        assert_eq!(cells(&plain, "tr")[3..], ["approval", "7s", "r1: push"]);
        // A sub-task is shown under its parent, and its session's line with them.
        assert!(text.contains("\n  tp "), "{text}");
        assert!(text.contains("\ncodex session s\n"), "{text}");
        // The latest of the rest fill what is left.
        assert!(
            text.contains("\n  t7 ") && !text.contains("\n  t6 "),
            "{text}"
        );
        assert_eq!(plain[9], "  …and 7 more task(s)", "{text}");
        // Order is kept: sessions in the order they first spawned, sub-tasks under parents.
        let at = |s: &str| text.find(s).unwrap();
        assert!(at("\n  t7 ") < at("\n  tp ") && at("\n  tp ") < at("\n    tr "));
        assert!(at("\n    tr ") < at("codex session s"), "{text}");
        // Only the color tells what waits for someone and what works.
        assert!(line_of(&frame, "tw ").contains("\x1b[33mwaiting"));
        assert!(line_of(&frame, "tr ").contains("\x1b[33mapproval"));
        assert!(line_of(&frame, "t7 ").contains("\x1b[2midle"));

        // A hidden task that changes is not brought in for it, and a longer state of its own
        // does not move the cells of the others.
        store
            .append(&event(4, 0, "t0", "said", json!({"text": "Late words."})))
            .unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, Some((100, 10))), frame);
        store
            .save(&task(
                "t0",
                "user",
                "cancelled",
                NOW - 50_000,
                json!({"stalled": true}),
            ))
            .unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, Some((100, 10))), frame);
        // The daemon may leave the stalled mark on a task that stopped.
        assert_eq!(state(&view.tasks["t0"], view.tail("t0")).0, "cancelled");
    }

    #[test]
    fn sessions_keep_their_place_and_can_be_told_apart() {
        let h = home();
        let store = Store::new(&h.0);
        let a = "codex:019a2b3c-1111";
        let b = "codex:019a2b3c-2222";
        // A spawned first, long ago, then B, then A again.
        store
            .save(&task("a1", a, "idle", NOW - 3 * HOUR, json!({})))
            .unwrap();
        store
            .save(&task("b1", b, "idle", NOW - 2_000, json!({})))
            .unwrap();
        store
            .save(&task("a2", a, "idle", NOW - 1_000, json!({})))
            .unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        let text = view.frame(NOW, None).join("\n");
        // A stays first though its first task is hidden; their IDs are whole, being alike cut.
        assert!(!text.contains("a1 "), "{text}");
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} in {text}"));
        assert!(
            at("\ncodex session 019a2b3c-1111\n") < at("\ncodex session 019a2b3c-2222\n"),
            "{text}"
        );
        // With room for one of them, the later spawned.
        let frame: Vec<String> = view
            .frame(NOW, Some((100, 6)))
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(
            frame[3..],
            [
                "codex session 019a2b3c-1111",
                line_of(&frame, "a2 "),
                "  …and 1 more task(s)"
            ],
            "{frame:#?}"
        );
        // A task's view tells its session whole.
        let one = view.task_frame("b1", NOW, None);
        assert!(
            one[1].contains("by codex session 019a2b3c-2222 "),
            "{one:#?}"
        );
    }

    #[test]
    fn a_result_being_written_is_read_again() {
        let h = home();
        let store = Store::new(&h.0);
        store
            .save(&task("t1", "user", "idle", NOW, json!({})))
            .unwrap();
        // The daemon counts the turn before writing its result.
        store.save_turn("t1", 1, "").unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        assert_eq!(cells(&view.frame(NOW, None), "t1").len(), 5);
        store.save_turn("t1", 1, "All tests pass.").unwrap();
        view.refresh();
        assert_eq!(cells(&view.frame(NOW, None), "t1")[5], "All tests pass.");
    }

    #[test]
    fn a_task_text_too_long_for_the_window_says_where_it_all_is() {
        let h = home();
        let store = Store::new(&h.0);
        let prompt: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        let mut long = "word ".repeat(20);
        long.push_str("end");
        store
            .save_prompt("t1", &format!("{long}\n{}", prompt.join("\n")))
            .unwrap();
        store
            .save(&task("t1", "user", "idle", NOW, json!({})))
            .unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();

        // Without a window, all of it.
        let plain = view.task_frame("t1", NOW, None).join("\n");
        assert!(
            plain.contains(&format!("\n  {long}\n  line 1\n")) && plain.contains("\n  line 30\n"),
            "{plain}"
        );

        let frame: Vec<String> = view
            .task_frame("t1", NOW, Some((40, 20)))
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        let text = frame.join("\n");
        assert_eq!(frame.len(), 20, "{text}");
        // Long lines are broken between words.
        let task = &frame[frame.iter().position(|l| l == "Task").unwrap() + 1..];
        assert_eq!(task[0], format!("  {}word", "word ".repeat(6)), "{text}");
        assert!(task[1].starts_with("  word "), "{text}");
        let note = frame
            .iter()
            .position(|l| l.starts_with("  …"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            frame[note].ends_with(" more line(s); all of it is in:"),
            "{text}"
        );
        // The path is whole, on lines of its own.
        let path = store.prompt_path("t1").display().to_string();
        let shown: String = frame[note + 1..]
            .iter()
            .take_while(|l| !l.is_empty())
            .map(|l| l.strip_prefix("  ").unwrap())
            .collect();
        assert_eq!(shown, path, "{text}");
        assert!(text.contains("\nEvents"), "{text}");

        let lines = |n| vec![vec![("  a".to_string(), "")]; n];
        let shown = |w: Wrapped, room| -> Vec<String> {
            w.cut(room).into_iter().map(|l| l[0].0.clone()).collect()
        };
        let file = Some((PathBuf::from("dir/x.md"), true));
        let full = Wrapped {
            lines: lines(5),
            file: file.clone(),
            width: None,
        };
        assert_eq!(
            shown(full, 4),
            [
                "  a",
                "  a",
                "  …3 more line(s); all of it is in:",
                "  dir/x.md"
            ]
        );
        let narrow = Wrapped {
            lines: lines(5),
            file,
            width: Some(7),
        };
        assert_eq!(
            shown(narrow, 4),
            [
                "  a",
                "  …4 more line(s); all of it is in:",
                "  dir/",
                "  x.md"
            ]
        );
        let partial = Wrapped {
            lines: lines(1),
            file: Some((PathBuf::from("x.md"), false)),
            width: None,
        };
        assert_eq!(partial.len(), 3);
        assert_eq!(shown(partial, 5), ["  a", "  …the rest is in:", "  x.md"]);
    }

    #[test]
    fn sub_tasks_that_matter_take_the_room_the_texts_leave() {
        let h = home();
        let store = Store::new(&h.0);
        store.save_prompt("tp", "Fix the parser.").unwrap();
        store
            .save(&task("tp", "user", "idle", NOW - 60_000, json!({})))
            .unwrap();
        store.save_turn("tp", 1, "Fixed.").unwrap();
        for i in 0..13 {
            let id = format!("k{i:02}");
            store.save_prompt(&id, &format!("Part {i}.")).unwrap();
            let (status, more) = match i {
                0 => ("failed", json!({})),
                12 => (
                    "waiting",
                    json!({"questions": [
                        {"id": "q1", "text": "Which port?", "answer": null, "asked_ms": NOW}
                    ]}),
                ),
                _ => ("idle", json!({})),
            };
            store
                .save(&task(&id, "task:tp", status, NOW - 50_000 + i, more))
                .unwrap();
        }
        store
            .append(&event(1, 1_000, "k00", "failed", json!({"error": "429"})))
            .unwrap();
        for i in 0..5 {
            store
                .append(&event(2 + i, 1_000, "tp", "tool", json!({"name": "Read"})))
                .unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame: Vec<String> = view
            .task_frame("tp", NOW, Some((100, 24)))
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        let text = frame.join("\n");
        assert_eq!(frame.len(), 24, "{text}");
        assert!(text.contains("\nResult of turn 1\n  Fixed.\n"), "{text}");
        // One-line texts leave 9 lines: 8 sub-tasks, what waits for someone and what went
        // wrong first, then the latest.
        assert!(text.contains("\n  …and 5 more task(s)\n"), "{text}");
        assert_eq!(
            cells(&frame, "k12")[3..],
            ["waiting", "49s", "q1: Which port?"],
            "{text}"
        );
        // A sub-task's task text gives way to what else is told of it.
        assert_eq!(cells(&frame, "k00")[3..], ["failed", "1s", "429"], "{text}");
        assert_eq!(
            cells(&frame, "k11")[3..],
            ["idle", "49s", "Part 11."],
            "{text}"
        );
        assert!(!text.contains("k05 "), "{text}");
        assert!(text.ends_with("[tp codex] Read"), "{text}");

        // A running task with long texts and many sub-tasks still says where its texts are.
        let long: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let long = long.join("\n");
        store.save_prompt("tr", &long).unwrap();
        store
            .save(&task("tr", "user", "running", NOW, json!({})))
            .unwrap();
        store.save_turn("tr", 1, &long).unwrap();
        for i in 0..7 {
            store
                .save(&task(&format!("c{i}"), "task:tr", "idle", NOW, json!({})))
                .unwrap();
        }
        for e in [
            event(20, 9_000, "tr", "turn_started", json!({"turn": 2})),
            event(21, 8_000, "tr", "said", json!({"text": "Checking."})),
            event(
                22,
                7_000,
                "tr",
                "tool",
                json!({"name": "Read", "detail": "a"}),
            ),
            event(
                23,
                6_000,
                "tr",
                "tool",
                json!({"name": "Read", "detail": "b"}),
            ),
        ] {
            store.append(&e).unwrap();
        }
        view.refresh();
        let frame: Vec<String> = view
            .task_frame("tr", NOW, Some((80, 24)))
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        let text = frame.join("\n");
        assert_eq!(frame.len(), 24, "{text}");
        let turn = store.turn_path("tr", 1).display().to_string();
        let result = &text[text.find("\nResult of turn 1\n").unwrap()..];
        assert!(
            result.contains("more line(s); all of it is in:\n"),
            "{text}"
        );
        assert!(result.replace("\n  ", "").contains(&turn), "{text}");
        assert!(text.contains("\n  …and 7 more task(s)\n"), "{text}");
        assert!(text.ends_with("[tr codex] Read: b"), "{text}");
    }

    #[test]
    fn wrapping_breaks_between_words_when_it_can() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("aaaaaaaaaa", 4), ["aaaa", "aaaa", "aa"]);
        assert_eq!(wrap("审查审查审查", 5), ["审查", "审查", "审查"]);
        assert_eq!(wrap("", 5), [""]);
    }
}
