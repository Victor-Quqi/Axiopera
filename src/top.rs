//! `axiopera top`: every task on one line, its name, its state and what it last said, redrawn
//! each second for a person watching sub-agents work. The arrow keys choose a task and Enter
//! shows it in detail, as `top <task>` does; Esc goes back. It only reads the data directory
//! and never connects to the daemon, so it starts nothing, takes no messages and marks no
//! result delivered.

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
/// Messages of each task kept for its view.
const MESSAGES: usize = 100;
/// Messages shown in a task's view when the output is not a terminal.
const PLAIN_MESSAGES: usize = 20;
/// Columns before the text of a message in a task's view: when it was said.
const LABEL: usize = 8;
/// Bytes read of a task text or a result. Past them, the view points to the file.
const READ_BYTES: u64 = 64 * 1024;
/// The rank of a line that goes only with the lines of its tasks: a session's line.
const WITH_TASKS: u8 = u8::MAX;
/// How long the rest of an escape sequence is waited for before an escape alone is the Esc
/// key.
const ESC_WAIT: Duration = Duration::from_millis(100);
/// Assumed when the terminal does not tell its size.
const FALLBACK_SIZE: (usize, usize) = (100, 30);
/// Columns of a task's name shown in its line.
const NAME: usize = 20;
/// Characters of a host session's ID shown: enough to tell sessions apart.
const SESSION: usize = 8;
/// Columns of the state cell: its longest word, `interrupted`, so that a state that comes and
/// goes does not move the cells after it.
const STATE: usize = 11;

const BOLD: &str = "1";
const DIM: &str = "2";
/// The chosen task's line, given as the color of an empty first piece: see `paint`.
const CHOSEN: &str = "7";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

/// Text and the SGR color it is shown in, empty for none.
type Seg = (String, &'static str);

pub async fn run(home: &Path, task: Option<&str>, all: bool) -> Result<i32> {
    let mut view = View::new(home, all);
    view.refresh();
    let focus = task.map(|r| view.find(r)).transpose()?;
    if !std::io::stdout().is_terminal() {
        let mut out = std::io::stdout().lock();
        let lines = match &focus {
            Some(id) => view.task_frame(id, store::now_ms(), None, false),
            None => view.frame(store::now_ms(), None, None),
        };
        for line in lines {
            writeln!(out, "{line}")?;
        }
        return Ok(0);
    }
    // The signal handlers are installed on the first poll. Until then a signal ends the process
    // at once, which would leave the terminal as changed below.
    let mut stop = std::pin::pin!(stopped());
    tokio::select! {
        biased;
        () = &mut stop => return Ok(0),
        () = std::future::ready(()) => {}
    }
    // Without a terminal to read keys from, such as mintty without ConPTY, the view only
    // shows, and Ctrl-C is a signal as before.
    let mut keys = match std::io::stdin().is_terminal() {
        true => platform::Keys::enable(),
        false => None,
    };
    let (send, mut read) = tokio::sync::mpsc::unbounded_channel();
    if keys.is_some() {
        std::thread::spawn(move || read_keys(send));
    }
    let mut input = Input::default();
    let mut ui = Ui {
        open: focus.clone(),
        chosen: focus,
        keys: keys.is_some(),
    };
    let _ansi = platform::Ansi::enable();
    // The alternate screen leaves the shell's scrollback as it was. Without autowrap, a line
    // wider than measured is clipped instead of pushing the frame down.
    show("\x1b[?1049h\x1b[?25l\x1b[?7l")?;
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let drawn = loop {
        let size = platform::terminal_size()
            .filter(|&(w, h)| w > 0 && h > 0)
            .or_else(env_size)
            .unwrap_or(FALLBACK_SIZE);
        // Drawn over the last frame instead of after clearing it, which would flicker.
        let lines = ui.draw(&view, store::now_ms(), size);
        if let Err(e) = show(&format!("\x1b[H{}\x1b[K\x1b[J", lines.join("\x1b[K\r\n"))) {
            break Err(e);
        }
        // Keys are followed at once; the files are read again each second.
        let pressed = tokio::select! {
            () = &mut stop => break Ok(()),
            bytes = read.recv(), if ui.keys => match bytes {
                Some(bytes) => input.feed(&bytes),
                // Keys can no longer be read: the terminal is given back its own handling of
                // Ctrl-C, and the view only shows, with nothing chosen.
                None => {
                    keys = None;
                    ui.keys = false;
                    input = Input::default();
                    Vec::new()
                }
            },
            () = tokio::time::sleep(ESC_WAIT), if input.waiting() => input.flush(),
            _ = tick.tick() => {
                view.refresh();
                Vec::new()
            }
        };
        let now = store::now_ms();
        if !pressed.into_iter().all(|key| ui.press(key, &view, now)) {
            break Ok(());
        }
    };
    show("\x1b[?7h\x1b[?25h\x1b[?1049l")?;
    drop(keys);
    drawn?;
    Ok(0)
}

/// Resolves when `top` is told to stop: by Ctrl-C as a signal, or on Unix by a request to
/// terminate, which would otherwise end it without giving the terminal back as it was. Its
/// handlers are installed on its first poll.
async fn stopped() {
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => drop(s.recv().await),
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}

/// A key that does something in `top`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Open,
    Back,
    /// `q`: back from a task, or quit from the list.
    Q,
    Quit,
}

/// Keys from the bytes a terminal sends, which may split an escape sequence between reads.
#[derive(Default)]
struct Input {
    /// The start of an escape sequence whose end has not come yet.
    pending: Vec<u8>,
}

impl Input {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = 0;
        while let Some((key, used)) = key_at(&self.pending[i..]) {
            out.extend(key);
            i += used;
        }
        self.pending.drain(..i);
        out
    }

    /// Whether an escape sequence waits for its end.
    fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// What is left once no more bytes came for `ESC_WAIT`: an escape alone is the Esc key,
    /// and the start of a longer sequence is dropped.
    fn flush(&mut self) -> Vec<Key> {
        let esc = self.pending == [0x1b];
        self.pending.clear();
        esc.then_some(Key::Back).into_iter().collect()
    }
}

/// The key at the start of `bytes`, if they are one, and how many bytes it takes; `None` when
/// there are none, or they start an escape sequence not yet whole.
fn key_at(bytes: &[u8]) -> Option<(Option<Key>, usize)> {
    Some(match bytes {
        [] | [0x1b] | [0x1b, b'[' | b'O'] => return None,
        // CSI: parameters, then a final byte. SS3: one byte. A byte that cannot end them, such
        // as Ctrl-C, ends what came before it and is read on its own.
        [0x1b, b'[', tail @ ..] => {
            let params = tail
                .iter()
                .take_while(|b| (0x20..0x40).contains(*b))
                .count();
            match *tail.get(params)? {
                end @ 0x40..=0x7e => (arrow(end), 2 + params + 1),
                _ => (None, 2 + params),
            }
        }
        [0x1b, b'O', end @ 0x40..=0x7e, ..] => (arrow(*end), 3),
        [0x1b, b'O', ..] => (None, 2),
        [0x1b, ..] => (Some(Key::Back), 1),
        [b'\r' | b'\n', ..] => (Some(Key::Open), 1),
        [b'k', ..] => (Some(Key::Up), 1),
        [b'j', ..] => (Some(Key::Down), 1),
        [b'q', ..] => (Some(Key::Q), 1),
        [0x7f | 0x08, ..] => (Some(Key::Back), 1),
        [0x03, ..] => (Some(Key::Quit), 1),
        _ => (None, 1),
    })
}

/// The key an arrow's last byte stands for: right opens, left goes back.
fn arrow(b: u8) -> Option<Key> {
    match b {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Open),
        b'D' => Some(Key::Back),
        _ => None,
    }
}

/// Reads stdin until `top` ends or it cannot be read. Blocks, so it runs on a thread of its
/// own.
fn read_keys(send: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    let mut bytes = [0; 64];
    loop {
        match stdin.read(&mut bytes) {
            // A Windows console reads Ctrl-Z as the end of input, and reads on.
            Ok(0) if cfg!(windows) => std::thread::sleep(Duration::from_millis(50)),
            // Elsewhere each byte is read as it comes: nothing read means the terminal is gone.
            Ok(0) => return,
            Ok(n) => {
                if send.send(bytes[..n].to_vec()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// What a person looks at: the list or one task, and the task chosen in the list.
struct Ui {
    /// The task shown alone, if any.
    open: Option<String>,
    /// The task whose line is marked in the list, which Enter opens.
    chosen: Option<String>,
    /// Whether keys can be read: without them nothing is chosen.
    keys: bool,
}

impl Ui {
    fn draw(&mut self, view: &View, now: u64, size: (usize, usize)) -> Vec<String> {
        if let Some(id) = &self.open {
            return view.task_frame(id, now, Some(size), self.keys);
        }
        if !self.keys {
            return view.frame(now, Some(size), None);
        }
        // The first task when the chosen one is gone or none was.
        let ids = view.order(now);
        if !self.chosen.as_ref().is_some_and(|c| ids.contains(c)) {
            self.chosen = ids.into_iter().next();
        }
        view.frame(now, Some(size), self.chosen.as_deref())
    }

    /// Follows a key. Returns false when it quits.
    fn press(&mut self, key: Key, view: &View, now: u64) -> bool {
        match (key, &self.open) {
            (Key::Quit, _) | (Key::Q, None) => return false,
            (Key::Back | Key::Q, Some(_)) => self.open = None,
            (Key::Open, None) => self.open = self.chosen.clone(),
            (Key::Up | Key::Down, None) => {
                let ids = view.order(now);
                let at = self
                    .chosen
                    .as_ref()
                    .and_then(|c| ids.iter().position(|id| id == c));
                let next = match (key, at) {
                    (Key::Up, Some(i)) => i.saturating_sub(1),
                    (_, Some(i)) => (i + 1).min(ids.len().saturating_sub(1)),
                    _ => 0,
                };
                self.chosen = ids.into_iter().nth(next);
            }
            _ => {}
        }
        true
    }
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
    /// The widest name cell of the list seen since top started: the column widens once for a
    /// new task, when its line moves the others anyway, and does not narrow when one leaves,
    /// which would move every line.
    width: usize,
    /// When each session first spawned, by its caller, as seen since top started: sessions
    /// keep their order when their oldest tasks are hidden or removed.
    first_spawned: HashMap<String, u64>,
}

impl View {
    fn new(home: &Path, all: bool) -> View {
        View {
            store: Store::new(home),
            all,
            tasks: HashMap::new(),
            tails: HashMap::new(),
            width: 0,
            first_spawned: HashMap::new(),
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
        for t in self.tasks.values() {
            let mut depth = 1;
            let mut up = self.parent(t);
            // A bound, should parents ever form a loop.
            while let Some(p) = up.filter(|_| depth <= self.tasks.len()) {
                depth += 1;
                up = self.parent(p);
            }
            if depth == 1 {
                let at = self
                    .first_spawned
                    .entry(t.parent.clone())
                    .or_insert(u64::MAX);
                *at = (*at).min(t.created_ms);
            }
            self.width = self.width.max(columns(&name_cell(depth, t)));
        }
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

    /// A line for each task: fitted to `size` with colors, or whole and plain without it. The
    /// `chosen` task's line is marked and always shown.
    fn frame(&self, now: u64, size: Option<(usize, usize)>, chosen: Option<&str>) -> Vec<String> {
        let (mut rows, hidden) = self.table(now);
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
        if chosen.is_some() {
            title.push(("  ↑↓ choose  Enter open  q quit".into(), DIM));
        }
        let mut lines = vec![title, vec![]];
        if rows.is_empty() {
            lines.push(vec![("no tasks".into(), DIM)]);
            return finish(&lines, size);
        }
        if let Some(row) = chosen.and_then(|c| rows.iter_mut().find(|r| r.id.as_deref() == Some(c)))
        {
            row.chosen = true;
            row.segs.insert(0, (String::new(), CHOSEN));
        }
        match size {
            None => lines.extend(rows.into_iter().map(|r| r.segs)),
            Some((_, height)) => {
                let room = height.saturating_sub(lines.len());
                lines.extend(choose(rows, room));
            }
        }
        finish(&lines, size)
    }

    /// The tasks of the list in its order, for the keys that choose one.
    fn order(&self, now: u64) -> Vec<String> {
        self.table(now).0.into_iter().filter_map(|r| r.id).collect()
    }

    /// A line for each task, sub-tasks under their parent, and with several sessions a line
    /// for each above the tasks it spawned. Also returns how many stopped tasks are left out.
    fn table(&self, now: u64) -> (Vec<Row>, usize) {
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
        groups.sort_by_key(|(caller, _)| (self.first_spawned.get(*caller).copied(), *caller));
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
        // Lines in order: `None` for a session's, else the task with its depth.
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
        let several = groups.len() > 1;
        let mut rows = Vec::new();
        let mut lines: HashMap<&str, usize> = HashMap::new();
        let mut group = None;
        for (caller, row) in order {
            let Some((depth, t)) = row else {
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
                ..self.task_row(&name_cell(depth, t), t, self.width)
            });
        }
        (rows, hidden)
    }

    /// A task on one line: its name, indented by its depth, its state, and what it last said.
    fn task_row(&self, cell: &str, t: &Task, width: usize) -> Row {
        let tail = self.tail(&t.id);
        let (state, color) = state(t, tail);
        let mut segs = vec![(format!("{}  ", pad(cell, width)), "")];
        match words(t, tail) {
            Some(text) => {
                segs.push((pad(state, STATE), color));
                segs.push((format!("  {text}"), ""));
            }
            None => segs.push((state.into(), color)),
        }
        Row {
            rank: rank(t, tail),
            spawned: t.created_ms,
            id: Some(t.id.clone()),
            ..Row::new(segs)
        }
    }

    /// One task in detail: a title, what it waits for or how it ended badly, then what it was
    /// given and what its agent said since, the latest last, as many as the window holds. With
    /// `keys`, its title says how to go back to the list.
    fn task_frame(
        &self,
        id: &str,
        now: u64,
        size: Option<(usize, usize)>,
        keys: bool,
    ) -> Vec<String> {
        // The way back is told however narrow the window: the title gives way to it.
        let width = size.map(|(w, _)| w);
        let back = |title: Vec<Seg>| match keys {
            true => hinted(title, "  Esc back", width),
            false => title,
        };
        let Some(t) = self.tasks.get(id) else {
            let gone = format!("{id} is gone: it was removed, by gc or by hand");
            return finish(&[back(vec![(gone, DIM)])], size);
        };
        let tail = self.tail(id);
        let (state, color) = state(t, tail);
        let agent = format!(
            "{}/{}",
            render::agent_name(t.agent),
            render::tier_name(t.tier)
        );
        let about = match &t.name {
            Some(_) => format!("  {id} {agent}  "),
            None => format!("  {agent}  "),
        };
        let title = back(vec![
            (clean(t.name.as_deref().unwrap_or(id)), BOLD),
            (about, DIM),
            (state.into(), color),
            (format!("  {}", meta(t, tail, now)), ""),
        ]);
        let mut head = vec![title];
        let mut waiting: Vec<Vec<Seg>> = details(t, tail).iter().map(Detail::row).collect();
        // Approvals and questions may be more than the window holds: as many as fit, and how
        // many more there are.
        if let Some((_, height)) = size
            && waiting.len() > height.saturating_sub(head.len())
        {
            let kept = height.saturating_sub(head.len() + 1);
            let more = waiting.len() - kept;
            waiting.truncate(kept);
            waiting.push(vec![(format!("{:8}…and {more} more", ""), DIM)]);
        }
        head.extend(waiting);

        // What it was given first, then what was said, in the order it was said.
        let ago = |at: u64| age(now.saturating_sub(at));
        let mut said: Vec<(u64, Message)> = Vec::new();
        match &tail.prompt {
            Some(text) => said.push((
                0,
                Message {
                    label: "task".into(),
                    lines: text.shown(),
                    file: Some(self.store.prompt_path(id)),
                },
            )),
            None if tail.log.is_empty() => said.push((
                0,
                Message {
                    label: "task".into(),
                    lines: vec!["not recorded: spawned before axiopera kept it".into()],
                    file: None,
                },
            )),
            None => {}
        }
        let given = said.len();
        // A turn's last message is its result, whole in its file. A result whose message is
        // not among them, as of a task from before messages were kept, comes on its own.
        let result = tail.result.as_ref().filter(|(_, r)| !r.lines.is_empty());
        let as_result = |turn: u32, r: &Text, at: u64| Message {
            label: ago(at),
            lines: r.shown(),
            file: Some(self.store.turn_path(id, turn)),
        };
        let mut result_shown = false;
        for s in &tail.log {
            let message = match result.filter(|(turn, _)| s.turn == Some(*turn)) {
                Some((turn, r)) => {
                    result_shown = true;
                    as_result(*turn, r, s.at)
                }
                None => Message {
                    label: ago(s.at),
                    lines: s
                        .text
                        .split(" ↵ ")
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                    file: None,
                },
            };
            said.push((s.at, message));
        }
        if let Some((turn, r)) = result.filter(|_| !result_shown) {
            let at = match tail.ended_ms {
                0 => t.updated_ms,
                at => at,
            };
            said.push((at, as_result(*turn, r, at)));
        }
        said[given..].sort_by_key(|(at, _)| *at);
        let mut said: Vec<Message> = said.into_iter().map(|(_, m)| m).collect();
        if size.is_none() {
            let extra = (said.len() - given).saturating_sub(PLAIN_MESSAGES);
            said.drain(given..given + extra);
        }
        let mut lines = head;
        match size {
            None => {
                lines.push(vec![]);
                for m in said {
                    let text: Vec<String> = m.lines.iter().map(|l| unbold(&clean(l))).collect();
                    lines.extend(m.rows(text, false));
                }
            }
            Some((width, height)) => {
                let room = height.saturating_sub(lines.len());
                let rows = transcript(said, width, room);
                // A blank line under the head when all fits with it.
                if rows.len() < room {
                    lines.push(vec![]);
                }
                lines.extend(rows);
            }
        }
        finish(&lines, size)
    }
}

/// How many lines of a message are left out, and the file with all of it: its path whole,
/// after the count when it fits a window `width` wide, else on lines of its own.
fn left_out(more: usize, file: Option<&Path>, width: usize) -> Vec<Vec<Seg>> {
    let count = format!("{:LABEL$}…{more} more line(s)", "");
    let Some(file) = file else {
        return vec![vec![(count, DIM)]];
    };
    let path = file.display().to_string();
    let one = format!("{count}; all in {path}");
    // The last column stays empty.
    if columns(&one) < width {
        return vec![vec![(one, DIM)]];
    }
    let pieces = chop(&path, width.saturating_sub(LABEL + 1));
    std::iter::once(vec![(format!("{count}; all in:"), DIM)])
        .chain(
            pieces
                .into_iter()
                .map(|p| vec![(format!("{:LABEL$}{p}", ""), DIM)]),
        )
        .collect()
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

/// What a task was given, or a message of its agent, for its view.
struct Message {
    /// When it was said, or `task` for the task text.
    label: String,
    lines: Vec<String>,
    /// Where all of it is, when its lines are not.
    file: Option<PathBuf>,
}

impl Message {
    /// Its lines broken to fit a window `width` wide: the label before the first, the others
    /// under the first's text.
    fn wrapped(&self, width: usize) -> Vec<String> {
        // The label's columns, and the last column stays empty.
        let room = width.saturating_sub(LABEL + 1);
        self.lines
            .iter()
            .flat_map(|l| wrap(&unbold(&clean(l)), room))
            .collect()
    }

    /// The lines as rows. `cut` marks the first as following lines left out above it, in the
    /// label's columns, which the text does not use.
    fn rows(&self, text: Vec<String>, cut: bool) -> Vec<Vec<Seg>> {
        let mark = if cut { "…" } else { " " };
        text.into_iter()
            .enumerate()
            .map(|(i, line)| match i {
                0 => vec![
                    (format!("{:>w$}{mark} ", self.label, w = LABEL - 2), DIM),
                    (line, ""),
                ],
                _ => vec![(format!("{:LABEL$}{line}", ""), "")],
            })
            .collect()
    }
}

/// Messages in `room` lines of a window `width` wide, the latest last: whole while they fit,
/// then the end of the one before. When the latest alone does not fit, its start, and how much
/// more there is and where.
fn transcript(said: Vec<Message>, width: usize, room: usize) -> Vec<Vec<Seg>> {
    let mut shown: Vec<Vec<Vec<Seg>>> = Vec::new();
    let mut left = room;
    for (k, m) in said.iter().rev().enumerate() {
        let text = m.wrapped(width);
        if text.len() <= left {
            left -= text.len();
            shown.push(m.rows(text, false));
            continue;
        }
        if k == 0 {
            let room = |more| left.saturating_sub(left_out(more, m.file.as_deref(), width).len());
            // A line of it at least, when there are two, before the count.
            let keep = room(text.len()).max(usize::from(left >= 2));
            let more = text.len() - keep;
            let mut rows = m.rows(text.into_iter().take(keep).collect(), false);
            rows.extend(left_out(more, m.file.as_deref(), width));
            rows.truncate(left);
            shown.push(rows);
        } else if left > 0 {
            let from = text.len() - left;
            shown.push(m.rows(text.into_iter().skip(from).collect(), true));
        }
        break;
    }
    shown.into_iter().rev().flatten().collect()
}

/// A title with `hint` at its end, cut so that the hint fits a window `width` wide.
fn hinted(title: Vec<Seg>, hint: &str, width: Option<usize>) -> Vec<Seg> {
    // The last column stays empty.
    let mut room = width.map_or(usize::MAX, |w| w.saturating_sub(1 + columns(hint)));
    let mut out = Vec::new();
    for (text, color) in title {
        let (piece, cut) = fit(&clean(&text), room);
        room -= columns(&piece);
        out.push((piece, color));
        if cut {
            break;
        }
    }
    out.push((hint.to_string(), DIM));
    out
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

/// A line of a list, with what decides whether it is shown when the window is too short.
struct Row {
    segs: Vec<Seg>,
    /// Lines of lower rank are shown first: see `rank`.
    rank: u8,
    /// When its task was spawned: of lines with the same rank, the later are shown first.
    spawned: u64,
    /// The lines it is shown with: its session's and its ancestors'.
    with: Vec<usize>,
    /// Its task's, for a task's line.
    id: Option<String>,
    /// Whether it is the chosen task's line, which is always shown.
    chosen: bool,
}

impl Row {
    fn new(segs: Vec<Seg>) -> Row {
        Row {
            segs,
            rank: 0,
            spawned: 0,
            with: Vec::new(),
            id: None,
            chosen: false,
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

/// As many rows as fit in `room`, in their order; when not all fit, the last line says how
/// many are left out.
fn choose(rows: Vec<Row>, room: usize) -> Vec<Vec<Seg>> {
    if rows.len() <= room {
        return rows.into_iter().map(|r| r.segs).collect();
    }
    let (mut out, left) = pick(rows, room.saturating_sub(1));
    if out.len() < room {
        out.push(vec![(format!("  …and {left} more task(s)"), DIM)]);
    }
    out
}

/// No more than `budget` rows, in their order, and how many tasks are left out. The chosen
/// task's line comes first, shown even past the budget, then the lowest ranks, the later
/// spawned of the same rank first. Rows are not chosen by when they last changed: those shown
/// would change every second.
fn pick(rows: Vec<Row>, budget: usize) -> (Vec<Vec<Seg>>, usize) {
    let mut order: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].rank != WITH_TASKS)
        .collect();
    order.sort_by_key(|&i| {
        let r = &rows[i];
        (!r.chosen, r.rank, Reverse(r.spawned), Reverse(i))
    });
    let mut kept = vec![false; rows.len()];
    let mut used = 0;
    for i in order {
        let cost = usize::from(!kept[i]) + rows[i].with.iter().filter(|&&w| !kept[w]).count();
        if used + cost <= budget {
            used += cost;
            kept[i] = true;
            for &w in &rows[i].with {
                kept[w] = true;
            }
        } else if rows[i].chosen {
            // Alone, when the lines it goes with do not fit.
            used += 1;
            kept[i] = true;
        }
    }
    let left = rows
        .iter()
        .zip(&kept)
        .filter(|(r, k)| !**k && r.rank != WITH_TASKS)
        .count();
    let out = rows
        .into_iter()
        .zip(kept)
        .filter(|(_, k)| *k)
        .map(|(r, _)| r.segs)
        .collect();
    (out, left)
}

/// A task's cell in a list: its name, or its ID without one, indented by its depth.
fn name_cell(depth: usize, t: &Task) -> String {
    let name = clean(t.name.as_deref().unwrap_or(&t.id));
    format!("{}{}", "  ".repeat(depth), fit(&name, NAME).0)
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

/// A line under a task's title in its view: what it waits for, or how it ended badly. Its
/// label, in the color of the state it goes with, tells which.
struct Detail {
    label: &'static str,
    color: &'static str,
    text: String,
}

impl Detail {
    fn row(&self) -> Vec<Seg> {
        vec![
            (format!("{:>w$}  ", self.label, w = LABEL - 2), self.color),
            (one_line(&self.text), ""),
        ]
    }
}

/// What a task waits for, or how it ended badly.
fn details(t: &Task, tail: &Tail) -> Vec<Detail> {
    let mut out = Vec::new();
    let line = |label, color, text: String| Detail { label, color, text };
    if t.status == Status::Running {
        for (id, summary) in &tail.approvals {
            out.push(line("needs", YELLOW, format!("approval {id}: {summary}")));
        }
    }
    if !t.status.is_final() {
        for q in t.open_questions() {
            out.push(line("asks", YELLOW, format!("{}: {}", q.id, q.text)));
        }
    }
    if let Some((label, text)) = stop(t, tail) {
        out.push(line(label, RED, text.into()));
    }
    out
}

/// How a stopped task ended badly, if it did: a label and the text.
fn stop<'a>(t: &Task, tail: &'a Tail) -> Option<(&'static str, &'a str)> {
    let (label, text) = tail.stop.as_ref()?;
    let stopped = !matches!(t.status, Status::Running | Status::Queued | Status::Idle);
    (stopped || t.detached).then_some((*label, text.as_str()))
}

/// Whether a task waits for someone: for an answer, or for an approval.
fn waits(t: &Task, tail: &Tail) -> bool {
    t.status == Status::Waiting
        || t.status == Status::Running && !tail.approvals.is_empty()
        || !t.status.is_final() && t.open_questions().next().is_some()
}

/// What a task last said, for its line: its latest message, or a question it asks since. A
/// task whose messages are no longer in its events has the first line of its latest result.
fn words(t: &Task, tail: &Tail) -> Option<String> {
    let asked = match t.status.is_final() {
        true => None,
        false => t.open_questions().max_by_key(|q| q.asked_ms),
    };
    let text = match (&tail.words, asked) {
        (Some((said, at)), Some(q)) if *at > q.asked_ms => said.clone(),
        (_, Some(q)) => q.text.clone(),
        (Some((said, _)), None) => said.clone(),
        (None, None) => tail.result.as_ref()?.1.lines.first()?.clone(),
    };
    Some(unbold(&one_line(&text)))
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
    /// Its lines, and a last one that says so when they are not all of the file.
    fn shown(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        if !self.whole {
            lines.push("…".into());
        }
        lines
    }
}

/// A line broken into pieces of up to `room` columns, where `break_at` finds a place.
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
            match break_at(&piece, c) {
                Some(i) => {
                    let rest = piece.split_off(i);
                    out.push(piece.trim_end().to_string());
                    piece = rest;
                }
                None => out.push(std::mem::take(&mut piece)),
            }
            used = columns(&piece);
        }
        piece.push(c);
        used += w;
    }
    out.push(piece);
    out
}

/// The latest place in `piece` where a line may end, with `next` to follow: after a space, or
/// next to a wide character, as between Chinese characters, though not before closing
/// punctuation nor after opening punctuation.
fn break_at(piece: &str, next: char) -> Option<usize> {
    let mut after = next;
    for (i, before) in piece.char_indices().rev() {
        let at = i + before.len_utf8();
        let wide = char_columns(before) == 2 || char_columns(after) == 2;
        let held =
            "，。、；：？！）」』】》,.;:?!)".contains(after) || "（「『【《(".contains(before);
        if before == ' ' || wide && !held {
            return Some(at);
        }
        after = before;
    }
    None
}

/// A line without markdown's bold marks: `**` pairs around text, outside code spans, and not
/// part of a path, a glob or an expression such as `src/**/*.rs` or `2**10`.
fn unbold(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    // Outside the marks: not a letter, digit or what joins a path or expression.
    let outside =
        |c: Option<&char>| c.is_none_or(|c| !c.is_ascii_alphanumeric() && !"/*\\_".contains(*c));
    // Inside the marks: text.
    let inside = |c: Option<&char>| c.is_some_and(|c| !c.is_whitespace() && !"*/".contains(*c));
    let mut marks = Vec::new();
    let mut open = None;
    let mut code = false;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            code = !code;
        } else if !code && chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            let (before, after) = (i.checked_sub(1).map(|j| &chars[j]), chars.get(i + 2));
            match open {
                Some(at) if inside(before) && outside(after) => {
                    marks.extend([at, i]);
                    open = None;
                }
                _ if outside(before) && inside(after) => open = Some(i),
                _ => {}
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    chars
        .iter()
        .enumerate()
        .filter(|(j, _)| !marks.iter().any(|m| (*m..m + 2).contains(j)))
        .map(|(_, c)| c)
        .collect()
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
    /// The agent's latest message in any turn.
    words: Option<(String, u64)>,
    /// The agent's latest messages, oldest first.
    log: VecDeque<Said>,
    /// Approval requests of the current turn without a decision, with their summaries.
    approvals: Vec<(String, String)>,
    /// How the last turn or the task ended badly: a label and the text.
    stop: Option<(&'static str, String)>,
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
            words: None,
            log: VecDeque::new(),
            approvals: Vec::new(),
            stop: None,
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
                // Its last message, if said in it, is its result.
                let since = self.started_ms;
                if let Some(last) = self.log.back_mut().filter(|s| s.at >= since) {
                    last.turn = e.data.get("turn").and_then(Value::as_u64).map(|n| n as u32);
                }
                if e.data.get("ok") == Some(&Value::Bool(false)) {
                    self.stop = Some(("error", text("error").into()));
                }
            }
            // A turn the agent started by itself that its adapter did not report begins with
            // what it says or does.
            "said" => {
                if !self.in_turn && self.ended_ms > 0 {
                    self.start(e.ts);
                }
                self.words = Some((text("text").into(), e.ts));
                if self.log.len() == MESSAGES {
                    self.log.pop_front();
                }
                self.log.push_back(Said {
                    at: e.ts,
                    text: text("text").into(),
                    turn: None,
                });
            }
            "tool" => {
                if !self.in_turn {
                    self.start(e.ts);
                }
                self.tool_calls += 1;
            }
            "approval_request" => self
                .approvals
                .push((text("id").into(), text("summary").into())),
            "approved" | "denied" => self.approvals.retain(|(id, _)| id != text("id")),
            "failed" => self.stop = Some(("error", text("error").into())),
            "cancelled" | "interrupted" | "detached" => {
                self.stop = Some(("why", text("reason").into()));
            }
            _ => {}
        }
    }

    fn start(&mut self, at: u64) {
        self.in_turn = true;
        self.started_ms = at;
        self.tool_calls = 0;
    }
}

/// A message of a task's agent.
struct Said {
    at: u64,
    /// As the daemon recorded it: lines joined by ` ↵ `, and cut when long.
    text: String,
    /// The turn it ended, for the last message of a turn: its result.
    turn: Option<u32>,
}

/// One line from its pieces. Given a width, it is cut to fit and colored, and a line whose
/// first piece is empty with the color `CHOSEN` is shown in reverse across the window.
fn paint(segs: &[Seg], width: Option<usize>) -> String {
    // The last column stays empty: writing there makes some terminals wrap.
    let mut room = width.map_or(usize::MAX, |w| w.saturating_sub(1));
    let chosen = width.is_some()
        && segs
            .first()
            .is_some_and(|(t, c)| t.is_empty() && *c == CHOSEN);
    let sgr = |color: &str| match (chosen, color) {
        (false, "") => None,
        (false, c) => Some(c.to_string()),
        (true, "") => Some(CHOSEN.to_string()),
        (true, c) => Some(format!("{c};{CHOSEN}")),
    };
    let mut line = String::new();
    for (text, color) in segs {
        let (piece, cut) = fit(&clean(text), room);
        room -= columns(&piece);
        match sgr(color) {
            Some(sgr) if width.is_some() && !piece.is_empty() => {
                line.push_str(&format!("\x1b[{sgr}m{piece}\x1b[0m"));
            }
            _ => line.push_str(&piece),
        }
        if cut {
            break;
        }
    }
    if chosen && room > 0 {
        line.push_str(&format!("\x1b[{CHOSEN}m{}\x1b[0m", " ".repeat(room)));
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

    /// A task's line split into its cells: its name, its state and what it last said.
    fn cells<'a>(frame: &'a [String], name: &str) -> Vec<&'a str> {
        line_of(frame, &format!("{name} "))
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect()
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    #[test]
    fn each_task_is_one_line_of_its_name_state_and_words() {
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
        let frame = view.frame(NOW, None, None);
        let text = frame.join("\n");
        assert!(
            frame[0].starts_with(
                "axiopera top  1 running, 1 waiting, 1 queued, 1 approval(s)  1 older hidden (--all)"
            ),
            "{text}"
        );
        assert_eq!(frame[1..3], ["", "user: terminals and unrecognized agents"]);
        // With tasks of two sessions, a line for each, its ID cut to tell them apart.
        assert!(text.contains("\nclaude session s1234567\n"), "{text}");
        assert!(
            text.contains("\nuser: terminals and unrecognized agents\n"),
            "{text}"
        );
        assert!(!text.contains("t4 "), "{text}");

        // What it last said, on one line, past the tool calls after it; its state tells that
        // it waits for an approval.
        assert_eq!(
            cells(&frame, "审查"),
            ["审查", "approval", "Running the tests."],
            "{text}"
        );
        for child in ["t2", "t3"] {
            assert_eq!(
                indent(line_of(&frame, &format!("{child} "))),
                indent(line_of(&frame, "审查 ")) + 2,
                "{text}"
            );
        }
        // Without a message in its events, the first line of its result.
        assert_eq!(cells(&frame, "t2"), ["t2", "idle", "All tests pass."]);
        // A question is what it said last.
        assert_eq!(cells(&frame, "t3"), ["t3", "waiting", "Which port?"]);
        assert_eq!(cells(&frame, "t5"), ["t5", "idle"]);
        assert!(indent(line_of(&frame, "t6 ")) > indent(line_of(&frame, "t5 ")));
        // Before it says anything, its state alone; how it failed is left to its view.
        assert_eq!(cells(&frame, "t6"), ["t6", "queued"]);
        assert_eq!(cells(&frame, "t7"), ["t7", "failed"]);

        // A line still being written is left for the next refresh.
        let more = serde_json::to_string(&event(
            7,
            1_000,
            "t1",
            "said",
            json!({"text": "Editing a.rs."}),
        ))
        .unwrap();
        let (head, rest) = more.split_at(20);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(store.events_path("t1"))
            .unwrap();
        f.write_all(head.as_bytes()).unwrap();
        view.refresh();
        assert_eq!(
            cells(&view.frame(NOW, None, None), "审查")[2],
            "Running the tests."
        );
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
        assert_eq!(
            cells(&view.frame(NOW, None, None), "审查")[1..],
            ["running", "Editing a.rs."]
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
                .frame(NOW, None, None)
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
        t1.turns = 2;
        store
            .save_prompt("t1", "Review the parser.\nReport findings.")
            .unwrap();
        store.save(&t1).unwrap();
        store
            .save_turn(
                "t1",
                2,
                "No bugs.\n\n- checked the reader\n- and the writer\n",
            )
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
            // The daemon keeps a message on one line, and cut when long.
            event(
                4,
                20_000,
                "t1",
                "said",
                json!({"text": "No bugs. ↵ - checked the reader…(+19 chars)"}),
            ),
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

        // A title, then what it was given and what its agent said, without its tool calls
        // or its sub-tasks'; a turn's last message whole from its result.
        let frame = view.task_frame("t1", NOW, None, false);
        let text = frame.join("\n");
        assert_eq!(
            frame,
            [
                "review  t1 codex/work  idle  2 turn(s), last change 10m00s ago",
                "",
                "  task  Review the parser.",
                "        Report findings.",
                "   40s  Checking the reader.",
                "   20s  No bugs.",
                "        - checked the reader",
                "        - and the writer",
            ],
            "{text}"
        );
        // With keys, how to go back.
        assert!(view.task_frame("t1", NOW, None, true)[0].ends_with(" ago  Esc back"));

        // Fitted to the window, the latest messages whole, and the end of the one before.
        let fitted: Vec<String> = view
            .task_frame("t1", NOW, Some((60, 6)), false)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(fitted[2..], frame[4..], "{fitted:#?}");
        assert_eq!(fitted[1], "  task… Report findings.");
        // With room for all, a blank line under the title.
        let roomy = view.task_frame("t1", NOW, Some((60, 20)), false);
        assert_eq!(roomy.len(), frame.len(), "{roomy:#?}");
        // When the latest alone does not fit, its start, and how much more there is and where.
        let short: Vec<String> = view
            .task_frame("t1", NOW, Some((60, 3)), false)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(short[1], "   20s  No bugs.", "{short:#?}");
        assert!(
            short[2].starts_with("        …2 more line(s); all in"),
            "{short:#?}"
        );
        assert!(short.iter().all(|l| columns(l) <= 59), "{short:#?}");

        std::fs::remove_dir_all(h.0.join("tasks").join("t1")).unwrap();
        view.refresh();
        assert_eq!(
            view.task_frame("t1", NOW, None, false),
            ["t1 is gone: it was removed, by gc or by hand"]
        );
        assert_eq!(
            view.task_frame("t1", NOW, None, true),
            ["t1 is gone: it was removed, by gc or by hand  Esc back"]
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
            (
                meta(t, tail, NOW),
                lines,
                words(t, tail).unwrap_or_default(),
            )
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
        assert_eq!(waits.1, ["needs approval r9: push"]);
        state(event(3, 7_000, "t1", "denied", json!({"id": "r9"})));
        assert_eq!(
            state(event(4, 7_000, "t1", "said", json!({"text": "Done."}))),
            (
                "turn 1, 9s, 0 tool call(s)".to_string(),
                vec![],
                "Done.".to_string()
            )
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
                vec![],
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
        // A failed turn of a task still running is not told: the task goes on.
        assert!(
            state(event(
                9,
                1_000,
                "t1",
                "turn_ended",
                json!({"turn": 2, "ok": false, "error": "boom"})
            ))
            .1
            .is_empty()
        );
        // Its line keeps what was said in the last turn until the agent says more.
        state(event(10, 900, "t1", "turn_started", json!({"turn": 3})));
        let (_, lines, text) = state(event(
            11,
            800,
            "t1",
            "tool",
            json!({"name": "Read", "detail": "y"}),
        ));
        assert_eq!((lines.len(), &text[..]), (0, "The build finished."));
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
        // The last message of each turn, said in it, is its result.
        let turns: Vec<_> = view
            .tail("t1")
            .log
            .iter()
            .map(|s| (s.text.as_str(), s.turn))
            .collect();
        assert_eq!(
            turns,
            [("Done.", Some(1)), ("The build finished.", Some(2))]
        );
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
        store
            .append(&event(1, 1_000, "t00", "said", json!({"text": detail})))
            .unwrap();
        // Tool calls after it are left to the task's view.
        for i in 2..10 {
            store
                .append(&event(
                    i,
                    1_000,
                    "t00",
                    "tool",
                    json!({"name": "Bash", "detail": "cargo build"}),
                ))
                .unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame = view.frame(NOW, Some((40, 12)), None);
        assert_eq!(frame.len(), 12, "{frame:#?}");
        let plain: Vec<String> = frame.iter().map(|l| strip_sgr(l)).collect();
        // One session: no line for it. The latest tasks fill the window.
        assert!(plain[2].starts_with("  t21 "), "{plain:#?}");
        assert_eq!(plain[11], "  …and 21 more task(s)", "{plain:#?}");
        for (line, plain) in frame.iter().zip(&plain) {
            assert!(!plain.contains('\x1b'), "{line:?}");
            assert!(columns(plain) <= 39, "{plain:?}");
        }
        // What it said shows on one line, without its control characters.
        let all = view.frame(NOW, Some((100, 40)), None);
        let t00 = line_of(&all, "t00 ");
        assert!(
            t00.ends_with("\x1b[0m  printf ' [2J [H' rm -rf build"),
            "{t00:?}"
        );

        // A task's work does not change which tasks are shown.
        store
            .append(&event(10, 0, "t00", "said", json!({"text": "Building."})))
            .unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, Some((40, 12)), None), frame);
    }

    fn strip_sgr(line: &str) -> String {
        let mut out = String::new();
        let mut rest = line;
        while let Some(start) = rest.find("\x1b[") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find(|c: char| !c.is_ascii_digit() && c != ';') {
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
        let before = view.frame(NOW, None, None);
        std::fs::write(h.0.join("tasks").join("t1").join("task.json"), "{").unwrap();
        view.refresh();
        assert_eq!(view.frame(NOW, None, None), before);
        store.remove("t1").unwrap();
        view.refresh();
        let after = view.frame(NOW, None, None).join("\n");
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
        // 8 lines for the 13 of the list.
        let frame = view.frame(NOW, Some((100, 10)), None);
        let plain: Vec<String> = frame.iter().map(|l| strip_sgr(l)).collect();
        let text = plain.join("\n");
        assert_eq!(plain.len(), 10, "{text}");
        assert_eq!(cells(&plain, "tw"), ["tw", "waiting", "Which port?"]);
        assert_eq!(cells(&plain, "tr"), ["tr", "approval", "Pushing."]);
        // A sub-task is shown under its parent, and its session's line with them.
        assert!(text.contains("\n  tp "), "{text}");
        assert!(text.contains("\ncodex session s\n"), "{text}");
        // The latest of the rest fill what is left.
        assert!(
            text.contains("\n  t6 ") && !text.contains("\n  t5 "),
            "{text}"
        );
        assert_eq!(plain[9], "  …and 6 more task(s)", "{text}");
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
        assert_eq!(view.frame(NOW, Some((100, 10)), None), frame);
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
        assert_eq!(view.frame(NOW, Some((100, 10)), None), frame);
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
        let text = view.frame(NOW, None, None).join("\n");
        // A stays first though its first task is hidden; their IDs are whole, being alike cut.
        assert!(!text.contains("a1 "), "{text}");
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} in {text}"));
        assert!(
            at("\ncodex session 019a2b3c-1111\n") < at("\ncodex session 019a2b3c-2222\n"),
            "{text}"
        );
        // With room for one of them, the later spawned.
        let frame: Vec<String> = view
            .frame(NOW, Some((100, 5)), None)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(
            frame[2..],
            [
                "codex session 019a2b3c-1111",
                line_of(&frame, "a2 "),
                "  …and 1 more task(s)"
            ],
            "{frame:#?}"
        );
        // Nor does A move when gc removes its first task, or the columns when a task with a
        // long name and deep under others comes and goes.
        let before = view.frame(NOW, None, None);
        store.remove("a1").unwrap();
        store
            .save(&task("b2", "task:b1", "idle", NOW, json!({})))
            .unwrap();
        store
            .save(&task(
                "b3",
                "task:b2",
                "idle",
                NOW,
                json!({"name": "a name longer than the column"}),
            ))
            .unwrap();
        view.refresh();
        let wide = view.frame(NOW, None, None);
        let b3 = cells(&wide, "a name");
        assert_eq!(b3[0], "a name longer than …", "{wide:#?}");
        // Chosen, it is shown however short the window: alone when the lines of its session
        // and parents do not fit.
        for (height, more) in [(4, Some("  …and 3 more task(s)")), (3, None)] {
            let short = view.frame(NOW, Some((100, height)), Some("b3"));
            assert!(
                short[2].starts_with("\x1b[7m      a name longer than …"),
                "{short:#?}"
            );
            assert_eq!(short.get(3).map(|l| strip_sgr(l)).as_deref(), more);
        }
        store.remove("b3").unwrap();
        store.remove("b2").unwrap();
        view.refresh();
        let after = view.frame(NOW, None, None);
        assert_eq!(after.len(), before.len());
        assert_eq!(line_of(&after, "a2 "), line_of(&wide, "a2 "));
        assert_ne!(line_of(&after, "a2 "), line_of(&before, "a2 "));
        assert!(after[2].ends_with("019a2b3c-1111"), "{after:#?}");
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
        assert_eq!(cells(&view.frame(NOW, None, None), "t1"), ["t1", "idle"]);
        store.save_turn("t1", 1, "All tests pass.").unwrap();
        view.refresh();
        assert_eq!(
            cells(&view.frame(NOW, None, None), "t1"),
            ["t1", "idle", "All tests pass."]
        );
    }

    #[test]
    fn a_message_too_long_for_the_window_says_where_it_all_is() {
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
        let plain = view.task_frame("t1", NOW, None, false).join("\n");
        assert!(
            plain.contains(&format!("\n  task  {long}\n        line 1\n"))
                && plain.ends_with("\n        line 30"),
            "{plain}"
        );

        // In a window, its start, broken between words, and how much more there is and where.
        let frame: Vec<String> = view
            .task_frame("t1", NOW, Some((40, 8)), false)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        let text = frame.join("\n");
        assert_eq!(frame.len(), 8, "{text}");
        assert_eq!(frame[1], format!("  task  {}word", "word ".repeat(5)));
        assert!(frame[2].starts_with("        word "), "{text}");
        // The path whole, on lines of its own when it does not fit after the count.
        let path = store.prompt_path("t1").display().to_string();
        let pieces = path.chars().count().div_ceil(31);
        let note = frame.len() - 1 - pieces;
        assert_eq!(
            frame[note],
            format!("        …{} more line(s); all in:", 34 - (note - 1)),
            "{text}"
        );
        let shown: String = frame[note + 1..]
            .iter()
            .map(|l| l.strip_prefix("        ").unwrap())
            .collect();
        assert_eq!(shown, path, "{text}");
        assert!(frame.iter().all(|l| columns(l) <= 39), "{text}");
    }

    #[test]
    fn the_latest_messages_fill_the_window() {
        let h = home();
        let store = Store::new(&h.0);
        store.save_prompt("t1", "Fix the parser.").unwrap();
        store
            .save(&task("t1", "user", "running", NOW, json!({"turns": 0})))
            .unwrap();
        for e in [
            event(1, 60_000, "t1", "turn_started", json!({"turn": 1})),
            event(
                2,
                50_000,
                "t1",
                "said",
                json!({"text": "Reading. ↵ Then **fixing**."}),
            ),
            event(3, 40_000, "t1", "tool", json!({"name": "Read"})),
            event(
                4,
                30_000,
                "t1",
                "said",
                json!({"text": "Fixed the reader."}),
            ),
            event(
                5,
                10_000,
                "t1",
                "said",
                json!({"text": "**Done**: all tests pass."}),
            ),
            event(
                6,
                5_000,
                "t1",
                "approval_request",
                json!({"id": "r1", "summary": "push"}),
            ),
        ] {
            store.append(&e).unwrap();
        }
        let mut view = View::new(&h.0, false);
        view.refresh();
        // Its lines as the agent wrote them, without markdown's bold marks; what waits for
        // someone under the title, in its state's color.
        let frame = view.task_frame("t1", NOW, Some((60, 9)), false);
        let plain: Vec<String> = frame.iter().map(|l| strip_sgr(l)).collect();
        assert_eq!(
            plain[1..],
            [
                " needs  approval r1: push",
                "",
                "  task  Fix the parser.",
                "   50s  Reading.",
                "        Then fixing.",
                "   30s  Fixed the reader.",
                "   10s  Done: all tests pass.",
            ],
            "{plain:#?}"
        );
        assert!(
            frame[1].starts_with("\x1b[33m needs  \x1b[0m"),
            "{frame:#?}"
        );
        assert_eq!(
            cells(&view.frame(NOW, None, None), "t1"),
            ["t1", "approval", "Done: all tests pass."]
        );
        // On a short window, the latest whole, and the end of the one before.
        let short: Vec<String> = view
            .task_frame("t1", NOW, Some((60, 5)), false)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(
            short[2..],
            [
                "   50s… Then fixing.",
                "   30s  Fixed the reader.",
                "   10s  Done: all tests pass.",
            ],
            "{short:#?}"
        );

        // Bold marks go; a path, a glob, an expression or code keeps its stars.
        for (line, plain) in [
            ("**Done**: all", "Done: all"),
            ("里加了**断言**，然后", "里加了断言，然后"),
            ("**`code`** here", "`code` here"),
            (
                "src/**/*.rs and tests/**/*.rs",
                "src/**/*.rs and tests/**/*.rs",
            ),
            ("2**10 and 3**2", "2**10 and 3**2"),
            ("a ** b ** c", "a ** b ** c"),
            ("`**x**` stays", "`**x**` stays"),
        ] {
            assert_eq!(unbold(line), plain);
        }

        // A turn that ended with nothing to say keeps what was said in it.
        store.save_turn("t1", 1, "").unwrap();
        store
            .save(&task("t1", "user", "failed", NOW, json!({"turns": 1})))
            .unwrap();
        store
            .append(&event(
                7,
                1_000,
                "t1",
                "turn_ended",
                json!({"turn": 1, "ok": false, "error": "max turns"}),
            ))
            .unwrap();
        view.refresh();
        let ended = view.task_frame("t1", NOW, None, false).join("\n");
        assert!(ended.ends_with("   10s  Done: all tests pass."), "{ended}");
        assert!(ended.contains("\n error  max turns\n"), "{ended}");
    }

    #[test]
    fn a_result_without_its_message_still_shows() {
        // A task from before messages were kept: its result, and no message.
        let h = home();
        let store = Store::new(&h.0);
        store
            .save(&task("t1", "user", "idle", NOW - 60_000, json!({})))
            .unwrap();
        store.save_turn("t1", 1, "All done.\nNo bugs.").unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        assert_eq!(
            view.task_frame("t1", NOW, None, false)[1..],
            [
                "",
                "  task  not recorded: spawned before axiopera kept it",
                " 1m00s  All done.",
                "        No bugs.",
            ]
        );
    }

    #[test]
    fn a_task_view_counts_what_waits_past_the_window_and_skips_what_is_not_there() {
        let h = home();
        let store = Store::new(&h.0);
        let questions: Vec<Value> = (0..25)
            .map(|i| json!({"id": format!("q{i}"), "text": "Which?", "answer": null, "asked_ms": NOW}))
            .collect();
        store
            .save(&task(
                "t1",
                "user",
                "waiting",
                NOW,
                json!({"questions": questions}),
            ))
            .unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        let frame: Vec<String> = view
            .task_frame("t1", NOW, Some((80, 24)), false)
            .iter()
            .map(|l| strip_sgr(l))
            .collect();
        assert_eq!(frame.len(), 24, "{frame:#?}");
        assert!(frame[22].contains("q21: Which?"), "{frame:#?}");
        assert_eq!(frame[23], "        …and 3 more");

        // Without a task text kept or anything said, it says so.
        let frame = view.task_frame("t1", NOW, None, false).join("\n");
        assert!(
            frame.ends_with("\n\n  task  not recorded: spawned before axiopera kept it"),
            "{frame}"
        );
    }

    #[test]
    fn keys_choose_a_task_open_it_and_go_back() {
        use Key::*;
        let mut input = Input::default();
        assert_eq!(
            input.feed(b"\x1b[A\x1b[B\x1bOC\x1b[1;5D\x1b[5~\rkjq\x1b\x03\x7fx"),
            [Up, Down, Open, Back, Open, Up, Down, Q, Back, Quit, Back]
        );
        // An escape sequence split between reads is one key; an escape alone is Esc once no
        // more comes, and the start of a sequence that never ends is nothing.
        assert!(input.feed(b"\x1b").is_empty() && input.waiting());
        assert_eq!(input.feed(b"[B"), [Down]);
        assert_eq!(input.feed(b"j\x1b[1;"), [Down]);
        assert_eq!(input.feed(b"5A"), [Up]);
        assert!(!input.waiting());
        input.feed(b"\x1b");
        assert_eq!(input.flush(), [Back]);
        input.feed(b"\x1bO");
        assert!(input.flush().is_empty() && !input.waiting());
        // Ctrl-C is never taken into a sequence.
        for start in [&b"\x1b["[..], b"\x1b[1;", b"\x1bO"] {
            assert!(input.feed(start).is_empty());
            assert_eq!(input.feed(b"\x03"), [Quit]);
            assert!(!input.waiting());
        }

        let h = home();
        let store = Store::new(&h.0);
        // Older idle tasks first, then one that waits.
        for i in 0..6 {
            store
                .save(&task(
                    &format!("t{i}"),
                    "user",
                    "idle",
                    NOW - 50_000 + i,
                    json!({}),
                ))
                .unwrap();
        }
        store
            .save(&task(
                "tw",
                "user",
                "waiting",
                NOW - 1_000,
                json!({"name": "ask"}),
            ))
            .unwrap();
        let mut view = View::new(&h.0, false);
        view.refresh();
        let mut ui = Ui {
            open: None,
            chosen: None,
            keys: true,
        };
        let plain = |ui: &mut Ui, view: &View| -> Vec<String> {
            ui.draw(view, NOW, (80, 6))
                .iter()
                .map(|l| strip_sgr(l))
                .collect()
        };

        // The first task is chosen, marked across the window, and shown though the older
        // tasks give way to the one that waits.
        let frame = ui.draw(&view, NOW, (80, 6));
        assert_eq!(ui.chosen.as_deref(), Some("t0"));
        assert!(frame[2].starts_with("\x1b[7m  t0 "), "{frame:#?}");
        let shown = plain(&mut ui, &view);
        assert!(
            shown[0].ends_with("  ↑↓ choose  Enter open  q quit"),
            "{shown:#?}"
        );
        assert_eq!(columns(&shown[2]), 79, "{shown:#?}");
        assert!(shown[3].starts_with("  t5 ") && shown[4].starts_with("  ask "));
        assert_eq!(shown[5], "  …and 4 more task(s)");

        // Up and down move through the list in its order, and stop at its ends.
        for (key, chosen) in [(Down, "t1"), (Up, "t0"), (Up, "t0")] {
            assert!(ui.press(key, &view, NOW));
            assert_eq!(ui.chosen.as_deref(), Some(chosen));
        }
        for _ in 0..10 {
            ui.press(Down, &view, NOW);
        }
        assert_eq!(ui.chosen.as_deref(), Some("tw"));

        // Enter opens it; up and down do nothing there, and Esc goes back to it in the list.
        ui.press(Open, &view, NOW);
        let one = plain(&mut ui, &view);
        assert!(
            one[0].starts_with("ask  tw codex/work  waiting") && one[0].ends_with("  Esc back")
        );
        ui.press(Down, &view, NOW);
        assert_eq!(ui.open.as_deref(), Some("tw"));
        ui.press(Back, &view, NOW);
        assert_eq!(
            (ui.open.as_deref(), ui.chosen.as_deref()),
            (None, Some("tw"))
        );
        // q goes back from a task, and quits from the list; Ctrl-C quits from both.
        ui.press(Open, &view, NOW);
        assert!(ui.press(Q, &view, NOW) && ui.open.is_none());
        assert!(!ui.press(Q, &view, NOW));
        ui.press(Open, &view, NOW);
        assert!(!ui.press(Quit, &view, NOW));

        // When the chosen task is removed, the first is chosen.
        ui.open = None;
        store.remove("tw").unwrap();
        view.refresh();
        plain(&mut ui, &view);
        assert_eq!(ui.chosen.as_deref(), Some("t0"));

        // Without keys, as when they can no longer be read, no line is marked and the title
        // tells of no keys.
        ui.keys = false;
        let frame = ui.draw(&view, NOW, (80, 6));
        assert!(!frame.iter().any(|l| l.contains("\x1b[7m")), "{frame:#?}");
        let shown = plain(&mut ui, &view);
        assert!(!shown[0].contains("choose"), "{shown:#?}");
        assert!(!shown.iter().any(|l| columns(l) == 79), "{shown:#?}");
    }

    #[test]
    fn wrapping_breaks_between_words_when_it_can() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("aaaaaaaaaa", 4), ["aaaa", "aaaa", "aa"]);
        assert_eq!(wrap("审查审查审查", 5), ["审查", "审查", "审查"]);
        // Chinese breaks between its characters, but not before closing punctuation.
        assert_eq!(wrap("审查 abc 审查审查", 6), ["审查", "abc 审", "查审查"]);
        assert_eq!(wrap("审查。审查", 4), ["审", "查。", "审查"]);
        assert_eq!(wrap("", 5), [""]);
    }
}
