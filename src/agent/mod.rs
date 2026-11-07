//! Agent adapters: one native process per running turn, driven over its stdio protocol.

mod claude;
mod codex;
pub mod codex_remote;
mod grok;
pub mod models;
mod rpc;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::ipc::{Agent, Tier};
use crate::platform;

/// What adapters report to the task actor.
#[derive(Debug)]
pub enum AgentEvent {
    Session {
        id: String,
        file: Option<PathBuf>,
    },
    Tool {
        id: Option<String>,
        name: String,
        detail: String,
    },
    ToolUpdated {
        id: String,
        name: String,
        detail: String,
    },
    FileChanged(String),
    /// A message the agent wrote, once it is complete. The last one of a turn is also its
    /// result.
    Said(String),
    /// A request we answered with a refusal without asking anyone.
    Blocked(String),
    /// A request that needs someone's decision.
    Approval(Approval),
    /// The backend put this input into the turn's conversation.
    InputRead(u64),
    /// The agent started a turn by itself, e.g. when background work ended.
    TurnStarted,
    /// A problem in the agent's own setup that its parent should hear of.
    Notice(String),
    /// The model or reasoning effort now in effect. `None` leaves that one as it was; an effort
    /// of `Some(None)` means the model runs without one.
    Model {
        model: Option<String>,
        effort: Option<Option<String>>,
    },
    TurnEnded {
        ok: bool,
        result: String,
        error: Option<String>,
        usage: Option<Value>,
    },
    Exited(String),
}

pub type Events = mpsc::UnboundedSender<AgentEvent>;

#[derive(Debug)]
pub struct Approval {
    /// One line on what the agent wants to do.
    pub summary: String,
    /// Everything the request says, for whoever decides: a summary can hide what matters.
    pub detail: String,
    pub reply: oneshot::Sender<Decision>,
    /// Fires when the backend stops waiting, so the request must not be decided any more.
    pub withdrawn: oneshot::Receiver<()>,
}

#[derive(Debug)]
pub enum Decision {
    Approve,
    /// With the reason the agent is told.
    Deny(String),
}

/// Hand a request to the daemon and wait for its decision; a dropped request is a refusal.
/// Adapters await this off their message loop, which must keep running meanwhile.
fn ask_approval(
    events: &Events,
    summary: String,
    detail: String,
) -> impl Future<Output = Decision> + use<> {
    let (reply, rx) = oneshot::channel();
    // A sender dropped at once: this request is never withdrawn.
    let (_, withdrawn) = oneshot::channel();
    let _ = events.send(AgentEvent::Approval(Approval {
        summary,
        detail,
        reply,
        withdrawn,
    }));
    decided(rx)
}

/// `ask_approval` for a backend that can stop waiting. Once `gone` fires, the daemon drops
/// the request undecided and this yields None.
fn ask_withdrawable_approval(
    events: &Events,
    summary: String,
    detail: String,
    gone: oneshot::Receiver<()>,
) -> impl Future<Output = Option<Decision>> + use<> {
    let (reply, rx) = oneshot::channel();
    let (withdraw, withdrawn) = oneshot::channel();
    let _ = events.send(AgentEvent::Approval(Approval {
        summary,
        detail,
        reply,
        withdrawn,
    }));
    async move {
        tokio::select! {
            decision = decided(rx) => Some(decision),
            Ok(()) = gone => {
                let _ = withdraw.send(());
                None
            }
        }
    }
}

async fn decided(rx: oneshot::Receiver<Decision>) -> Decision {
    rx.await.unwrap_or_else(|_| {
        Decision::Deny("Refused by axiopera: the approval channel closed.".into())
    })
}

/// Environment an agent needs on top of its caller's.
pub fn extra_env(agent: Agent) -> &'static [(&'static str, &'static str)] {
    match agent {
        Agent::Grok => &grok::ENV,
        _ => &[],
    }
}

pub enum Steer {
    Accepted,
    NoActiveTurn,
}

pub struct StartSpec {
    pub task_id: String,
    pub agent: Agent,
    pub tier: Tier,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Final environment for the agent process.
    pub env: Vec<(String, String)>,
    /// Native session to resume.
    pub resume: Option<String>,
    pub trace: Option<PathBuf>,
}

#[derive(Default)]
struct Inputs(VecDeque<(Option<u64>, String)>);

impl Inputs {
    fn push(&mut self, id: Option<u64>, text: &str) {
        self.0.push_back((id, text.to_string()));
    }

    fn read(&mut self, text: &str, events: &Events) {
        if let Some(i) = self
            .0
            .iter()
            .position(|(_, sent)| sent.trim_end() == text.trim_end())
            && let Some((Some(id), _)) = self.0.remove(i)
        {
            let _ = events.send(AgentEvent::InputRead(id));
        }
    }
}

fn nonempty(v: Option<&str>) -> Option<String> {
    v.filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Report the model a backend names, if it names one; the effort stays as it was.
fn report_model(events: &Events, model: Option<&str>) {
    if let Some(model) = nonempty(model) {
        let _ = events.send(AgentEvent::Model {
            model: Some(model),
            effort: None,
        });
    }
}

/// Report both, from a backend that states its whole setting: no effort means none applies.
fn report_settings(events: &Events, model: Option<&str>, effort: Option<&str>) {
    let _ = events.send(AgentEvent::Model {
        model: nonempty(model),
        effort: Some(nonempty(effort)),
    });
}

fn content_text(content: &Value) -> String {
    content.as_str().map(str::to_string).unwrap_or_else(|| {
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Instructions every sub-agent gets on top of its own system prompt.
fn protocol_text(spec: &StartSpec) -> String {
    let ask = if spec.agent == Agent::Codex {
        "call the `ask` tool of the axiopera MCP server"
    } else {
        "run `axiopera ask \"<question>\"`"
    };
    format!(
        "You are an axiopera sub-agent working for a parent. The parent receives your final \
         message from each turn. Include the outcome and the context it needs to act. Long \
         results may first arrive as a tail excerpt with a path to the full text.\n\
         If you need a decision or information that only your parent has, {ask} and keep \
         working on what does not depend on the answer; do not wait or poll for it. Answers \
         arrive in this task's main session, during its current turn or in a new one. If \
         blocked on the answer, end your turn and state the dependency."
    )
}

/// A plain `axiopera ...` invocation with nothing else for a shell to run. Sub-agents may always
/// talk to axiopera; anything chained to it goes through normal approval. Quote-aware for bash
/// and PowerShell, the shells the agents run, and conservative where they disagree: the scan
/// must never think it is inside quotes when the shell is not.
pub fn is_axiopera_command(command: &str) -> bool {
    let Some(rest) = command.trim().strip_prefix("axiopera") else {
        return false;
    };
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return false;
    }
    // PowerShell also takes typographic quotes as string delimiters.
    if rest.contains(['‘', '’', '‚', '‛', '“', '”', '„', '‟']) {
        return false;
    }
    // PowerShell's stop-parsing token, recognised after quote removal (`--'%'`) and with any
    // dash character, turns the quotes after it into plain characters.
    let unquoted = rest
        .replace(['\'', '"'], "")
        .replace(['\u{2013}', '\u{2014}', '\u{2015}'], "-");
    if unquoted.contains("--%") {
        return false;
    }
    let mut quote = None;
    for c in rest.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), _) => {}
            // Expansion inside double quotes, and bash's `\"`, which does not end the string.
            (Some(_), '$' | '`' | '\\') => return false,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            // Separators, redirection, substitution, grouping, and the escapes that could hide a
            // quote from this scan (bash `\`, cmd `^`, PowerShell backtick).
            (
                None,
                ';' | '&' | '|' | '<' | '>' | '(' | ')' | '{' | '}' | '$' | '`' | '\\' | '^' | '%'
                | '\n' | '\r',
            ) => return false,
            (None, _) => {}
        }
    }
    quote.is_none() && !rest.contains(['\n', '\r'])
}

pub const REFUSED: &str = "Refused by axiopera: read tier does not allow this action.";

pub enum Backend {
    Claude(claude::Claude),
    Codex(codex::Codex),
    Grok(grok::Grok),
}

impl Backend {
    /// Start the native process and open (or resume) its session. Turns start with `turn`.
    pub async fn start(spec: &StartSpec, exe: &Path, events: Events) -> Result<Backend> {
        Ok(match spec.agent {
            Agent::Claude => Backend::Claude(claude::Claude::start(spec, exe, events).await?),
            Agent::Codex => Backend::Codex(codex::Codex::start(spec, exe, events).await?),
            Agent::Grok => Backend::Grok(grok::Grok::start(spec, exe, events).await?),
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<()> {
        match self {
            Backend::Claude(a) => a.turn(text).await,
            Backend::Codex(a) => a.turn(text).await,
            Backend::Grok(a) => a.turn(text).await,
        }
    }

    pub async fn steer(&mut self, text: &str, id: Option<u64>) -> Result<Steer> {
        match self {
            Backend::Claude(a) => a.steer(text, id).await,
            Backend::Codex(a) => a.steer(text, id).await,
            Backend::Grok(a) => a.steer(text).await,
        }
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        match self {
            Backend::Claude(a) => a.interrupt().await,
            Backend::Codex(a) => a.interrupt().await,
            Backend::Grok(a) => a.interrupt().await,
        }
    }

    /// Whether a turn is still running from the agent's point of view.
    pub fn busy(&self) -> bool {
        match self {
            Backend::Claude(a) => a.busy(),
            Backend::Codex(a) => a.busy(),
            Backend::Grok(a) => a.busy(),
        }
    }

    /// Whether a turn is running right now; `busy` also counts the agent's background work.
    pub fn in_turn(&self) -> bool {
        match self {
            Backend::Claude(a) => a.in_turn(),
            Backend::Codex(a) => a.busy(),
            Backend::Grok(a) => a.in_turn(),
        }
    }

    fn proc(&mut self) -> &mut Proc {
        match self {
            Backend::Claude(a) => &mut a.proc,
            Backend::Codex(a) => &mut a.proc,
            Backend::Grok(a) => &mut a.proc,
        }
    }

    pub fn kill(&mut self) {
        self.proc().job.kill();
    }

    /// Let the agent exit on its own so its session file is complete, then clear the job.
    pub async fn shutdown(&mut self) {
        let p = self.proc();
        p.stdin.close().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), p.child.wait()).await;
        p.job.kill();
        let _ = p.child.wait().await;
    }

    pub fn stderr_tail(&mut self) -> String {
        self.proc()
            .stderr
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---- process plumbing ----

#[derive(Clone, Default)]
pub struct Trace(Option<Arc<Mutex<TraceFile>>>);

struct TraceFile {
    file: std::fs::File,
    path: PathBuf,
}

impl Trace {
    fn open(path: Option<&Path>) -> std::io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        // A previous process may still be draining stdout when its successor starts.
        static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<TraceFile>>>>> = OnceLock::new();
        let mut open = OPEN.get_or_init(Mutex::default).lock().unwrap();
        open.retain(|_, file| file.strong_count() > 0);
        if let Some(file) = open.get(path).and_then(Weak::upgrade) {
            return Ok(Self(Some(file)));
        }
        let file = Arc::new(Mutex::new(TraceFile {
            file: std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
            path: path.to_owned(),
        }));
        open.insert(path.to_owned(), Arc::downgrade(&file));
        Ok(Self(Some(file)))
    }

    pub fn line(&self, direction: &str, line: &str) {
        if let Some(file) = &self.0 {
            let mut guard = file.lock().unwrap();
            let TraceFile { file, path } = &mut *guard;
            let _ = crate::diagnostics::append(
                file,
                path,
                &format!("{direction} {line}\n"),
                crate::diagnostics::PROTOCOL_BYTES,
            );
        }
    }
}

#[derive(Clone)]
pub struct Writer {
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    pub trace: Trace,
}

impl Writer {
    pub async fn send(&self, v: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        let mut guard = self.stdin.lock().await;
        let w = guard
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stdin closed"))?;
        w.write_all(&line).await?;
        self.trace.line("send", &v.to_string());
        w.flush().await
    }

    /// EOF on stdin asks every supported agent to finish up and exit.
    pub async fn close(&self) {
        self.stdin.lock().await.take();
    }
}

pub fn lines<R: AsyncRead>(r: R) -> Lines<BufReader<R>> {
    BufReader::with_capacity(1 << 16, r).lines()
}

pub struct Proc {
    pub job: platform::Job,
    pub child: Child,
    pub stdin: Writer,
    /// Last lines of stderr, each cut to a few hundred characters, for error reports. Never
    /// logged.
    pub stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Proc {
    pub fn spawn(exe: &Path, args: &[String], spec: &StartSpec) -> Result<(Proc, ChildStdout)> {
        Self::spawn_with(exe, args, &spec.cwd, &spec.env, spec.trace.as_deref())
    }

    pub fn spawn_with(
        exe: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        trace: Option<&Path>,
    ) -> Result<(Proc, ChildStdout)> {
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let job = platform::Job::new()?;
        let mut child = job
            .spawn(&mut cmd)
            .with_context(|| format!("starting {}", exe.display()))?;
        let stdin = Writer {
            stdin: Arc::new(tokio::sync::Mutex::new(child.stdin.take())),
            trace: Trace::open(trace)?,
        };
        let stdout = child.stdout.take().unwrap();
        let stderr: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let tail = stderr.clone();
        let err = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut lines = lines(err);
            while let Ok(Some(l)) = lines.next_line().await {
                let mut t = tail.lock().unwrap();
                if t.len() == 20 {
                    t.pop_front();
                }
                // Node prints the source line of an uncaught error, a whole bundle when minified.
                t.push_back(clip(&l, 300));
            }
        });
        Ok((
            Proc {
                job,
                child,
                stdin,
                stderr,
            },
            stdout,
        ))
    }
}

// ---- finding the real executables ----

/// The native executable for an agent. npm installs put a script shim on PATH; on Windows we
/// need the real `.exe` behind it so the job owns the actual agent process.
pub fn resolve_exe(
    agent: Agent,
    configured: Option<&Path>,
    env: &[(String, String)],
) -> Result<PathBuf> {
    if let Some(exe) = configured {
        if exe.is_file() {
            return Ok(exe.to_path_buf());
        }
        bail!("{} (set in config.toml) is not a file", exe.display());
    }
    let var = |name: &str| {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let name = match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Grok => "grok",
    };
    if agent == Agent::Grok {
        let home = var("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".grok"));
        let exe = home
            .join("bin")
            .join(if cfg!(windows) { "grok.exe" } else { "grok" });
        if exe.is_file() {
            return Ok(exe);
        }
    }
    let path = var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Ok(exe);
            }
            if dir.join(format!("{name}.cmd")).is_file()
                && let Some(exe) = npm_native(&dir, agent)
            {
                return Ok(exe);
            }
        } else if dir.join(name).is_file() {
            return Ok(dir.join(name));
        }
    }
    bail!("cannot find the {name} executable on the caller's PATH")
}

fn npm_native(dir: &Path, agent: Agent) -> Option<PathBuf> {
    let modules = dir.join("node_modules");
    let (arch, triple) = if cfg!(target_arch = "aarch64") {
        ("arm64", "aarch64-pc-windows-msvc")
    } else {
        ("x64", "x86_64-pc-windows-msvc")
    };
    let exe = match agent {
        Agent::Claude => modules.join("@anthropic-ai/claude-code/bin/claude.exe"),
        Agent::Codex => modules.join(format!(
            "@openai/codex/node_modules/@openai/codex-win32-{arch}/vendor/{triple}/bin/codex.exe"
        )),
        Agent::Grok => return None,
    };
    exe.is_file().then_some(exe)
}

pub fn home_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default(),
    )
}

/// Every argument of a tool call in full, one per line; multi-line and long values start on a
/// line of their own.
pub fn arguments(input: &Value) -> String {
    let Some(fields) = input.as_object() else {
        return serde_json::to_string_pretty(input).unwrap_or_default();
    };
    fields
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) if s.contains('\n') || s.chars().count() > 200 => {
                format!("{k}:\n{s}")
            }
            Value::String(s) => format!("{k}: {s}"),
            other => format!("{k}: {other}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line for summaries: line breaks shown as ↵, cut to `max` characters with a count of
/// what was left out.
pub fn clip(s: &str, max: usize) -> String {
    let mut kept = String::new();
    let mut total = 0;
    for (i, line) in s.trim().lines().enumerate() {
        let separator = if i == 0 { "" } else { " ↵ " };
        for c in separator.chars().chain(line.chars()) {
            if total < max {
                kept.push(c);
            }
            total += 1;
        }
    }
    if total > max {
        kept.push_str(&format!("…(+{} chars)", total - max));
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::clip;
    use super::is_axiopera_command as ok;

    #[test]
    fn overlapping_protocol_readers_share_the_rotation_lock() {
        let dir = std::env::temp_dir().join(format!("axiopera-trace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("protocol.log");
        let old = super::Trace::open(Some(&path)).unwrap();
        let new = super::Trace::open(Some(&path)).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            old.0.as_ref().unwrap(),
            new.0.as_ref().unwrap()
        ));
        old.line("out", "last old record");
        new.line("in", "first new record");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "out last old record\nin first new record\n"
        );
        drop(old);
        drop(new);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clipped_lines_keep_every_line_and_count_the_rest() {
        assert_eq!(clip("cargo test", 200), "cargo test");
        assert_eq!(
            clip("cd crates\r\ncargo test\n", 200),
            "cd crates ↵ cargo test"
        );
        assert_eq!(clip("审查意见很长", 2), "审查…(+4 chars)");
        assert_eq!(clip(" \r\n  ", 0), "");
        assert_eq!(clip("a\r\nb", 3), "a ↵…(+2 chars)");
        assert_eq!(clip(&"中\n".repeat(100_000), 1), "中…(+399996 chars)");
    }

    #[test]
    fn plain_axiopera_commands() {
        assert!(ok("axiopera answers"));
        assert!(ok(r#"axiopera ask "Which port (8080 or 3000)?""#));
        assert!(ok("axiopera ask 'it''s fine; really'"));
        assert!(ok(r#"  axiopera reply q7k2m "use 8080 & restart""#));
        assert!(ok(r#"axiopera ask "Is 30% faster enough?""#));
    }

    #[test]
    fn anything_else_needs_approval() {
        assert!(!ok("axiopera-evil ask x"));
        assert!(!ok("rm -rf / ; axiopera answers"));
        assert!(!ok(r#"axiopera ask "x"; rm -rf /"#));
        assert!(!ok("axiopera ask x && calc"));
        assert!(!ok("axiopera ask x | calc"));
        assert!(!ok(r#"axiopera ask "$(whoami)""#));
        assert!(!ok(r#"axiopera ask "`whoami`""#));
        assert!(!ok(r#"axiopera ask \"; calc \""#));
        assert!(!ok(r#"axiopera ask ^"& calc ^""#));
        assert!(!ok(r#"axiopera ask "unterminated"#));
        assert!(!ok("axiopera ask x\ncalc"));
        assert!(!ok("axiopera ask x > out.txt"));
        assert!(!ok(r#"axiopera ask "a\"b"; echo bypass #""#));
        assert!(!ok("axiopera ask \"a“; calc; echo “b\""));
        assert!(!ok("axiopera ask 'a‘; calc; echo ‘b'"));
        assert!(!ok("axiopera --'%' ask x' | calc #'"));
        assert!(!ok(r#"axiopera ask "--%" x"#));
        assert!(!ok("axiopera –-'%' ask x' | calc #'"));
    }
}
