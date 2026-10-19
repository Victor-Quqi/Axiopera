//! Agent adapters: one native process per running turn, driven over its stdio protocol.

mod claude;
mod codex;
pub mod codex_remote;
mod grok;
mod rpc;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

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
        name: String,
        detail: String,
    },
    FileChanged(String),
    /// A request we answered with a refusal without asking anyone.
    Blocked(String),
    /// A request that needs someone's decision.
    Approval(Approval),
    /// The backend put this input into the turn's conversation.
    InputRead(u64),
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
    pub reply: oneshot::Sender<Decision>,
}

#[derive(Debug)]
pub enum Decision {
    Approve,
    /// With the reason the agent is told.
    Deny(String),
}

/// Hand a request to the daemon and wait for its decision; a dropped request is a refusal.
/// Adapters await this off their message loop, which must keep running meanwhile.
fn ask_approval(events: &Events, summary: String) -> impl Future<Output = Decision> + use<> {
    let (reply, rx) = oneshot::channel();
    let _ = events.send(AgentEvent::Approval(Approval { summary, reply }));
    async move { rx.await.unwrap_or_else(|_| Decision::Deny(REFUSED.into())) }
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
        "You are a sub-agent started through axiopera (task {}) by another coding agent, your \
         parent. Your final message of each turn is returned to the parent verbatim, so make it \
         a complete, self-contained report of what you did and found.\n\
         If you need a decision or information that only your parent has, {ask} and keep \
         working on everything that does not depend on the answer; do not wait or poll for it. \
         The answer arrives as a new message, during this turn or in a new one. If nothing is \
         left to do without it, end your turn and say what you are waiting for.",
        spec.task_id
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

pub const REFUSED: &str = "Refused by axiopera: your permission tier does not allow this action without \
                       approval, and nobody can approve it. Continue without it, or explain in your \
                       final report why it is needed. Plain `axiopera` commands, such as \
                       `axiopera ask`, are always allowed.";

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
pub struct Trace(Option<Arc<Mutex<std::fs::File>>>);

impl Trace {
    fn open(path: Option<&Path>) -> std::io::Result<Self> {
        path.map(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map(|file| Self(Some(Arc::new(Mutex::new(file)))))
        })
        .unwrap_or_else(|| Ok(Self::default()))
    }

    pub fn line(&self, direction: &str, line: &str) {
        use std::io::Write;
        if let Some(file) = &self.0 {
            let _ = writeln!(file.lock().unwrap(), "{direction} {line}");
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
    /// Last lines of stderr, for error reports. Never logged.
    pub stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Proc {
    pub fn spawn(exe: &Path, args: &[String], spec: &StartSpec) -> Result<(Proc, ChildStdout)> {
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
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
            trace: Trace::open(spec.trace.as_deref())?,
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
                t.push_back(l);
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

/// Cut a string to at most `max` bytes on a char boundary, for one-line summaries.
pub fn clip(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() <= max && !s.contains('\n') {
        return line.to_string();
    }
    let mut end = max.min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[cfg(test)]
mod tests {
    use super::is_axiopera_command as ok;

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
