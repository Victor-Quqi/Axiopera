mod agent;
mod client;
mod config;
mod daemon;
mod diagnostics;
mod identity;
mod ipc;
mod mcp;
mod paths;
mod platform;
mod plugin;
mod skill;
mod store;
mod top;

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ipc::{Agent, Request, Tier};

#[derive(Parser)]
#[command(
    name = "axiopera",
    version,
    about = "Dispatch coding agents as sub-agents of each other"
)]
struct Cli {
    /// Print structured JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Call as this session when its variable does not reach axiopera, e.g. over ssh:
    /// `<agent>:<session ID>`, the same on every command.
    #[arg(long, global = true, value_name = "AGENT:SESSION")]
    caller: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List model IDs, resolved aliases and supported reasoning efforts.
    Models {
        agent: Agent,
        /// Working directory (default: the current one).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Start a sub-agent on a task. Returns at once with its task ID.
    Spawn {
        agent: Agent,
        /// Use `-f FILE`, or `-` to read it from stdin.
        task: Vec<String>,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "work")]
        tier: Tier,
        /// Model name, passed to the agent as is.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort, passed to the agent as is.
        #[arg(long)]
        effort: Option<String>,
        /// Working directory (default: the current one).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// A name to refer to the task by.
        #[arg(long)]
        name: Option<String>,
        /// Interrupt each execution after this long, e.g. 30m; send resumes with a fresh limit.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
    },
    /// Message a task: steers its running turn, or starts a new turn in the same session.
    Send {
        task: String,
        /// Change permissions when resuming an idle task in the same session.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
        /// Use `-f FILE`, or `-` to read it from stdin.
        message: Vec<String>,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Block until your sub-tasks have news, then list it. Takes nothing; use inbox for that.
    Wait {
        /// Only these tasks (default: all of yours).
        tasks: Vec<String>,
        /// Give up after this long (exit code 3), e.g. 10m.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
    },
    /// Take the results, questions and approval requests your sub-tasks have for you. Each is
    /// handed over once. Never blocks.
    Inbox {
        /// Only these tasks (default: all of yours).
        tasks: Vec<String>,
    },
    /// By default, print sub-task news until stopped; for a sub-agent, until a minute after its
    /// sub-tasks are done and their news taken. Takes nothing.
    Watch {
        /// Show progress until no sub-tasks are running or queued.
        #[arg(long)]
        status: bool,
    },
    /// List your sub-tasks, or show one.
    Status {
        task: Option<String>,
        /// Tasks from all callers.
        #[arg(long)]
        all: bool,
    },
    /// For people: every task, what it was given and what it is doing, redrawn
    /// until Ctrl-C. Reads task files without consuming events.
    Top {
        /// Show only this task, in detail: its ID, or its name if no other task has it.
        task: Option<String>,
        /// Also tasks stopped more than an hour ago.
        #[arg(long)]
        all: bool,
    },
    /// Recent events of a task.
    Log {
        task: String,
        /// How many of the latest events to show.
        #[arg(long, default_value_t = 30)]
        tail: usize,
    },
    /// The result of a task's latest (or given) turn, if not delivered already.
    Result {
        task: String,
        #[arg(long)]
        turn: Option<u32>,
        /// Print it even if it was delivered before.
        #[arg(long)]
        force: bool,
    },
    /// Stop a task and its children. Use send to resume each task separately.
    Cancel { task: String },
    /// For sub-agents: ask your parent a question. Returns at once; keep working on what does
    /// not depend on the answer.
    Ask { question: Vec<String> },
    /// Answer a sub-agent's question.
    Reply {
        id: String,
        /// Use `-f FILE`, or `-` to read it from stdin.
        answer: Vec<String>,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// For sub-agents: your questions and the answers so far.
    Answers,
    /// Let a sub-agent do what it asked approval for, this once.
    Approve { id: String },
    /// Refuse a sub-agent's approval request.
    Deny {
        id: String,
        /// Told to the sub-agent.
        #[arg(long)]
        reason: Option<String>,
    },
    /// For sub-agents: send a note to a sibling task. Your parent gets a copy.
    Tell { to: String, message: Vec<String> },
    /// Hand a task over to the user: axiopera stops driving it and prints the native resume command.
    Detach { task: String },
    /// Remove stopped tasks and the request log now; the daemon does it after 30 days without activity.
    Gc {
        /// Keep what was active within this many days.
        #[arg(long, default_value_t = 0)]
        days: u64,
    },
    /// How to use axiopera, for agents.
    Guide {
        /// Only what applies to this host.
        #[arg(long, value_enum)]
        host: Option<Agent>,
    },
    /// Serve ask, tell and answers as MCP tools; axiopera sets this up for Codex sub-agents.
    #[command(hide = true)]
    Mcp {
        /// Serve the Codex plugin's hooks instead.
        #[arg(long)]
        codex_hooks: bool,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage the Codex plugin.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Install the skill for a host agent.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Install the bundled skill for one host or all supported hosts.
    Install {
        #[arg(value_enum)]
        host: skill::Host,
        /// Replace an existing skill with different contents.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// After you uninstall the plugin from Codex, remove what Codex keeps of it.
    Cleanup,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// The daemon's version, PID and number of running tasks.
    Status,
    /// Stop the daemon.
    Stop {
        /// Stop even if tasks are running (they become interrupted).
        #[arg(long)]
        force: bool,
    },
    #[command(hide = true)]
    Run {
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        idle_secs: Option<u64>,
    },
}

/// These commands change the user's agent configuration, which only the user asks for.
fn not_in_task(command: &str) -> Result<()> {
    // Empty counts as unset, as in identity::detect.
    if std::env::var_os(identity::TASK_VAR).is_some_and(|v| !v.is_empty()) {
        bail!("{command} is for the main session or the user, not sub-agents");
    }
    Ok(())
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("bad duration {s:?}; use e.g. 90s, 10m, 2h")),
    };
    Ok(Duration::from_secs(secs))
}

fn task_text(words: Vec<String>, file: Option<PathBuf>) -> Result<String> {
    let text = match (file, words.as_slice()) {
        (Some(f), []) => {
            std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?
        }
        (None, [dash]) if dash == "-" => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
        (None, words) if !words.is_empty() => words.join(" "),
        _ => bail!("give the text using one source: arguments, -f FILE, or - for stdin"),
    };
    if text.trim().is_empty() {
        bail!("the text is empty");
    }
    Ok(text)
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("axiopera: {e:#}");
            ipc::exit::ERROR
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    let home = paths::default_home();
    let caller = match &cli.caller {
        Some(given) => {
            // Empty counts as unset, as in identity::detect.
            let task = std::env::var(identity::TASK_VAR)
                .ok()
                .filter(|v| !v.is_empty());
            Some(identity::explicit(given, task.as_deref())?)
        }
        None => None,
    };
    let request = match cli.command {
        Command::Models { agent, cwd } => Request::Models {
            agent,
            cwd: match cwd {
                Some(c) => std::path::absolute(c)?,
                None => std::env::current_dir()?,
            },
        },
        Command::Guide { host } => {
            print!(
                "{}",
                guide(host, &caller.unwrap_or_else(client::detect_caller))
            );
            return Ok(0);
        }
        Command::Plugin {
            command: PluginCommand::Cleanup,
        } => {
            not_in_task("plugin cleanup")?;
            return plugin::cleanup(&home).await;
        }
        Command::Skill {
            command: SkillCommand::Install { host, force },
        } => {
            not_in_task("skill install")?;
            return Ok(skill::install(host, force, cli.json));
        }
        Command::Top { task, all } => {
            if cli.json {
                bail!("top has no JSON output; use `axiopera status --all --json`");
            }
            return top::run(&home, task.as_deref(), all).await;
        }
        Command::Mcp { codex_hooks } => {
            return mcp::serve(client::Client { home, json: false }, codex_hooks, caller).await;
        }
        Command::Daemon {
            command: DaemonCommand::Run { home, idle_secs },
        } => {
            let mut opts = daemon::options(home)?;
            if let Some(s) = idle_secs {
                opts.idle = Duration::from_secs(s);
            }
            daemon::run(opts).await?;
            return Ok(0);
        }
        Command::Daemon {
            command: DaemonCommand::Status,
        } => Request::Ping,
        Command::Daemon {
            command: DaemonCommand::Stop { force },
        } => Request::Stop { force },
        Command::Spawn {
            agent,
            task,
            file,
            tier,
            model,
            effort,
            cwd,
            name,
            timeout,
        } => {
            let cwd = match cwd {
                Some(c) => std::path::absolute(c)?,
                None => std::env::current_dir()?,
            };
            Request::Spawn(ipc::Spawn {
                agent,
                tier,
                task: task_text(task, file)?,
                cwd,
                name,
                model,
                effort,
                timeout_secs: timeout.map(|d| d.as_secs()),
            })
        }
        Command::Send {
            task,
            tier,
            message,
            file,
        } => Request::Send {
            task,
            tier,
            message: task_text(message, file)?,
        },
        Command::Wait { tasks, timeout } => Request::Wait {
            tasks,
            timeout_ms: timeout.map(|d| d.as_millis() as u64),
        },
        Command::Inbox { tasks } => Request::Inbox { tasks },
        Command::Watch { status } => Request::Watch { status },
        Command::Status { task, all } => Request::Status { task, all },
        Command::Log { task, tail } => Request::Log { task, tail },
        Command::Result { task, turn, force } => Request::Result { task, turn, force },
        Command::Cancel { task } => Request::Cancel { task },
        Command::Ask { question } => Request::Ask {
            text: task_text(question, None)?,
        },
        Command::Reply { id, answer, file } => Request::Reply {
            id,
            text: task_text(answer, file)?,
        },
        Command::Answers => Request::Answers,
        Command::Approve { id } => Request::Approve { id },
        Command::Deny { id, reason } => Request::Deny { id, reason },
        Command::Gc { days } => Request::Gc { days },
        Command::Tell { to, message } => Request::Tell {
            to,
            text: task_text(message, None)?,
        },
        Command::Detach { task } => Request::Detach { task },
    };
    client::Client {
        home,
        json: cli.json,
    }
    .run(caller, request)
    .await
}

/// The guide for one host, or for all with each one's part labelled; lines in `guide.md`
/// that start with `@<host> ` apply to that host only, and `@user` lines only to a caller
/// taken for `user`, such as a host whose session variable did not reach axiopera. The watch
/// command is filled in for a Grok session or a sub-agent, as their monitors cannot tell who
/// started them.
fn guide(host: Option<Agent>, caller: &str) -> String {
    let mut text = String::new();
    let mut labelled = None;
    for line in include_str!("guide.md").lines() {
        let Some((tag, rest)) = line
            .strip_prefix('@')
            .map(|l| l.split_once(' ').unwrap_or((l, "")))
        else {
            text.push_str(line);
            text.push('\n');
            labelled = None;
            continue;
        };
        if tag == "user" {
            if caller == "user" {
                text.push_str(rest);
                text.push('\n');
                labelled = None;
            }
            continue;
        }
        let (name, label) = match tag {
            "claude" => (Agent::Claude, "Claude Code"),
            "codex" => (Agent::Codex, "Codex"),
            _ => (Agent::Grok, "Grok"),
        };
        match host {
            Some(h) if h == name => text.push_str("  "),
            Some(_) => continue,
            None => {
                if labelled != Some(name) {
                    text.push_str(&format!("  {label}:\n"));
                    labelled = Some(name);
                }
                text.push_str("    ");
            }
        }
        text.push_str(rest);
        text.push('\n');
    }
    if caller.starts_with("grok:") || caller.starts_with("task:") {
        text = text.replace("grok:<session>", caller);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TextFile(PathBuf);

    impl TextFile {
        fn new(contents: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("axiopera message {}.md", uuid::Uuid::new_v4()));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
    }

    impl Drop for TextFile {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }

    #[test]
    fn skill_install_arguments() {
        for (name, expected) in [
            ("claude", skill::Host::Claude),
            ("codex", skill::Host::Codex),
            ("grok", skill::Host::Grok),
            ("all", skill::Host::All),
        ] {
            for force in [false, true] {
                let mut args = vec!["axiopera", "--json", "skill", "install", name];
                if force {
                    args.push("--force");
                }
                let cli = Cli::try_parse_from(args).unwrap();
                assert!(cli.json);
                match cli.command {
                    Command::Skill {
                        command:
                            SkillCommand::Install {
                                host,
                                force: actual,
                            },
                    } => {
                        assert_eq!(host, expected);
                        assert_eq!(actual, force);
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert!(Cli::try_parse_from(["axiopera", "skill", "install"]).is_err());
        assert!(Cli::try_parse_from(["axiopera", "skill", "install", "other"]).is_err());
    }

    #[test]
    fn message_file_options_match_spawn() {
        let text = "  审查意见\r\n\"C:\\work dir\\src\\main.rs\"\n'quoted'\n";
        let source = TextFile::new(text.as_bytes());
        for (command, id) in [("spawn", "codex"), ("send", "task"), ("reply", "question")] {
            for option in ["-f", "--file"] {
                let cli = Cli::try_parse_from([
                    "axiopera",
                    command,
                    id,
                    option,
                    source.0.to_str().unwrap(),
                ])
                .unwrap();
                let (words, file) = match cli.command {
                    Command::Spawn { task, file, .. } => (task, file),
                    Command::Send {
                        task,
                        message,
                        file,
                        ..
                    } => {
                        assert_eq!(task, id);
                        (message, file)
                    }
                    Command::Reply {
                        id: question,
                        answer,
                        file,
                    } => {
                        assert_eq!(question, id);
                        (answer, file)
                    }
                    _ => unreachable!(),
                };
                assert_eq!(file.as_ref(), Some(&source.0));
                assert_eq!(task_text(words, file).unwrap(), text);
            }
        }
    }

    #[tokio::test]
    async fn message_file_errors_precede_daemon_connection() {
        for (command, id) in [("spawn", "codex"), ("send", "task"), ("reply", "question")] {
            for contents in [b"".as_slice(), b" \r\n\t", b"\xff"] {
                let source = TextFile::new(contents);
                let cli = Cli::try_parse_from([
                    "axiopera",
                    command,
                    id,
                    "-f",
                    source.0.to_str().unwrap(),
                ])
                .unwrap();
                let error = run(cli).await.unwrap_err().to_string();
                if contents == b"\xff" {
                    assert_eq!(error, format!("reading {}", source.0.display()));
                } else {
                    assert_eq!(error, "the text is empty");
                }
            }
            let missing = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
            for words in [vec![], vec!["extra"], vec!["-"]] {
                let mut args = vec!["axiopera", command, id, "--file", missing.to_str().unwrap()];
                args.extend_from_slice(&words);
                let error = run(Cli::try_parse_from(args).unwrap())
                    .await
                    .unwrap_err()
                    .to_string();
                let expected = if words.is_empty() {
                    format!("reading {}", missing.display())
                } else {
                    "give the text using one source: arguments, -f FILE, or - for stdin".into()
                };
                assert_eq!(error, expected);
            }
        }
    }

    #[test]
    fn the_guide_gives_each_host_its_part() {
        let all = guide(None, "user");
        assert!(!all.contains('@'));
        for label in ["  Claude Code:\n    ", "  Codex:\n    ", "  Grok:\n    "] {
            assert_eq!(all.matches(label).count(), 1, "{label}");
        }
        let claude = guide(Some(Agent::Claude), "claude:s0");
        assert!(claude.contains("\n  Run `axiopera wait` as a background command"));
        assert!(!claude.contains("Codex:") && !claude.contains("watch --caller"));
        let codex = guide(Some(Agent::Codex), "codex:s0");
        assert!(codex.contains("`axiopera watch --status`"));
        assert!(!claude.contains("watch --status"));
        let grok = guide(Some(Agent::Grok), "grok:s1");
        assert!(grok.contains("`axiopera watch --caller grok:s1`"));
        assert!(!grok.contains("background command"));
        assert!(all.contains("`axiopera watch --caller grok:<session>`"));
        // Monitor commands need the full caller because they omit the native session variable.
        let sub_agent = guide(Some(Agent::Grok), "task:t1");
        assert!(sub_agent.contains("`axiopera watch --caller task:t1`"));
        let native = guide(Some(Agent::Grok), "task:t1/grok:child");
        assert!(native.contains("`axiopera watch --caller task:t1/grok:child`"));
    }

    #[test]
    fn only_a_caller_taken_for_user_is_told_to_name_itself() {
        let hint = "\n\naxiopera takes you for `user`";
        // A host over ssh asks for its own part but arrives without its session variable.
        for host in [
            None,
            Some(Agent::Claude),
            Some(Agent::Codex),
            Some(Agent::Grok),
        ] {
            let text = guide(host, "user");
            assert_eq!(text.matches(hint).count(), 1);
            assert!(text.contains("keeps using it.\n\nDispatch\n"));
            assert!(!text.contains('@'));
        }
        for caller in ["claude:s0", "pi:s0", "task:t1"] {
            let text = guide(Some(Agent::Claude), caller);
            assert!(!text.contains("--caller") && !text.contains("takes you for"));
            assert!(text.contains("session ends.\n\nDispatch\n"));
        }
    }

    #[test]
    fn the_caller_option_goes_anywhere() {
        for args in [
            vec!["axiopera", "--caller", "claude:s0", "inbox"],
            vec!["axiopera", "inbox", "--caller", "claude:s0"],
            vec![
                "axiopera",
                "spawn",
                "codex",
                "fix",
                "it",
                "--caller",
                "claude:s0",
            ],
            vec!["axiopera", "watch", "--status", "--caller", "claude:s0"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert_eq!(cli.caller.as_deref(), Some("claude:s0"), "{args:?}");
        }
        let cli = Cli::try_parse_from(["axiopera", "watch", "--status"]).unwrap();
        assert!(cli.caller.is_none());
        assert!(matches!(cli.command, Command::Watch { status: true }));
        let cli = Cli::try_parse_from(["axiopera", "spawn", "codex", "a", "b"]).unwrap();
        assert!(matches!(cli.command, Command::Spawn { task, .. } if task == ["a", "b"]));
    }

    #[tokio::test]
    async fn malformed_callers_fail_before_daemon_connection() {
        for given in ["claude:", "user"] {
            let cli = Cli::try_parse_from(["axiopera", "--caller", given, "inbox"]).unwrap();
            let error = run(cli).await.unwrap_err().to_string();
            assert!(error.starts_with(&format!("--caller {given:?}")), "{error}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("5d").is_err());
        assert!(parse_duration("m").is_err());
    }
}
