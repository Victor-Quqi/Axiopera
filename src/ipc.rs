//! CLI ↔ daemon protocol: one request per connection, newline-delimited JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

/// Bump on any incompatible change to the types below.
pub const PROTOCOL: u32 = 1;

/// Longest accepted line; task descriptions and environments travel inline.
const MAX_LINE: usize = 16 << 20;

#[derive(Serialize, Deserialize, Debug)]
pub struct Envelope {
    pub protocol: u32,
    pub version: String,
    pub caller: String,
    pub cwd: PathBuf,
    /// The caller's environment; sent only by requests that may start an agent process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<(String, String)>>,
    pub request: Request,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Claude,
    Codex,
    Grok,
}

#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Read,
    Work,
    Full,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Spawn {
    pub agent: Agent,
    pub tier: Tier,
    pub task: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub worktree: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Stop {
        force: bool,
    },
    Spawn(Spawn),
    Send {
        task: String,
        message: String,
    },
    Wait {
        tasks: Vec<String>,
        timeout_ms: Option<u64>,
    },
    Watch,
    Status {
        task: Option<String>,
        all: bool,
    },
    Log {
        task: String,
        tail: usize,
    },
    Result {
        task: String,
        turn: Option<u32>,
        force: bool,
    },
    Cancel {
        task: String,
    },
    Ask {
        text: String,
    },
    Reply {
        id: String,
        text: String,
    },
    Inbox,
    Approve {
        id: String,
    },
    Deny {
        id: String,
        reason: Option<String>,
    },
    Gc {
        days: u64,
    },
    Tell {
        to: String,
        text: String,
    },
    Detach {
        task: String,
    },
}

impl Request {
    /// A sandboxed caller may talk to tasks it already has, including resuming one with `send`,
    /// but may not create new ones outside its sandbox.
    pub fn allowed_in_sandbox(&self) -> bool {
        !matches!(
            self,
            Request::Spawn(_) | Request::Stop { .. } | Request::Gc { .. }
        )
    }

    /// Requests that may start an agent process or run git, and so carry the caller's
    /// environment.
    pub fn needs_env(&self) -> bool {
        matches!(
            self,
            Request::Spawn(_) | Request::Send { .. } | Request::Reply { .. } | Request::Gc { .. }
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Version,
    Sandboxed,
    NotFound,
    Invalid,
    Internal,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// Output for the caller: `text` for agents and people, `data` for `--json`.
    Out {
        text: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        data: Value,
    },
    /// Last line of a successful request.
    Done { exit: i32 },
    /// Last line of a failed request.
    Error { code: ErrorCode, message: String },
}

pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const TIMEOUT: i32 = 3;
    pub const NOTHING_TO_WAIT: i32 = 4;
    pub const FAILED: i32 = 5;
    pub const CANCELLED: i32 = 6;
}

pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

pub struct Reader<R>(Lines<BufReader<R>>);

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(r: R) -> Self {
        Reader(BufReader::new(r).lines())
    }

    /// Next message, or None at end of stream.
    pub async fn read<T: for<'de> Deserialize<'de>>(&mut self) -> std::io::Result<Option<T>> {
        let Some(line) = self.0.next_line().await? else {
            return Ok(None);
        };
        if line.len() > MAX_LINE {
            return Err(std::io::Error::other("message too long"));
        }
        serde_json::from_str(&line)
            .map(Some)
            .map_err(std::io::Error::other)
    }
}
