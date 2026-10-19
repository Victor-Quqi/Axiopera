mod agent;
mod client;
mod config;
mod daemon;
mod identity;
mod ipc;
mod mcp;
mod paths;
mod platform;
mod plugin;
mod store;
mod worktree;

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
    /// Print structured JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a sub-agent on a task. Returns at once with its task ID.
    Spawn {
        agent: Agent,
        /// The task. Use `-f FILE`, or `-` to read it from stdin.
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
        /// A short name to refer to the task by.
        #[arg(long)]
        name: Option<String>,
        /// Cancel the task after this long, e.g. 30m.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
        /// Work in a new git worktree on branch axiopera/<name or ID>, from your HEAD.
        #[arg(long)]
        worktree: bool,
    },
    /// Message a task: steers its running turn, or starts a new turn in the same session.
    Send {
        task: String,
        /// The message. Use `-f FILE`, or `-` to read it from stdin.
        message: Vec<String>,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Wait for the next results or problems from your sub-tasks.
    Wait {
        /// Only these tasks (default: all of yours).
        tasks: Vec<String>,
        /// Give up after this long (exit code 3), e.g. 60s.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
    },
    /// Print every event of your sub-tasks, one line each, until interrupted.
    Watch,
    /// List your sub-tasks, or show one.
    Status {
        task: Option<String>,
        /// All tasks, not only yours.
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
    /// Cancel a task and everything it spawned.
    Cancel { task: String },
    /// For sub-agents: ask your parent a question. Returns at once; keep working.
    Ask { question: Vec<String> },
    /// Answer a sub-agent's question.
    Reply {
        id: String,
        /// The answer. Use `-f FILE`, or `-` to read it from stdin.
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
    /// Hand a task over to you: axiopera stops driving it and prints the native resume command.
    Detach { task: String },
    /// Remove finished tasks untouched for a while, and their worktrees if nothing in them is
    /// uncommitted.
    Gc {
        /// Untouched for at least this many days.
        #[arg(long, default_value_t = 7)]
        days: u64,
    },
    /// How to use axiopera, for agents.
    Guide,
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
    /// The Codex plugin.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
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
        _ => bail!("give the text as arguments, with -f FILE, or as - for stdin"),
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
    let request = match cli.command {
        Command::Guide => {
            print!("{}", include_str!("guide.md"));
            return Ok(0);
        }
        Command::Plugin {
            command: PluginCommand::Cleanup,
        } => return plugin::cleanup(&home).await,
        Command::Mcp { codex_hooks } => {
            return mcp::serve(client::Client { home, json: false }, codex_hooks).await;
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
            worktree,
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
                worktree,
            })
        }
        Command::Send {
            task,
            message,
            file,
        } => Request::Send {
            task,
            message: task_text(message, file)?,
        },
        Command::Wait { tasks, timeout } => Request::Wait {
            tasks,
            timeout_ms: timeout.map(|d| d.as_millis() as u64),
        },
        Command::Watch => Request::Watch,
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
    .run(request)
    .await
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
                    "give the text as arguments, with -f FILE, or as - for stdin".into()
                };
                assert_eq!(error, expected);
            }
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
