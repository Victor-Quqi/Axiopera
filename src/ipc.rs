//! CLI ↔ daemon protocol: one request per connection, newline-delimited JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Bump on incompatible wire types or caller identity semantics.
pub const PROTOCOL: u32 = 6;

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
    /// Look, don't change.
    Read,
    /// The agent's own auto mode.
    Work,
    /// The agent's full-access mode.
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
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Models {
        agent: Agent,
        cwd: PathBuf,
    },
    Stop {
        force: bool,
    },
    Spawn(Spawn),
    Send {
        task: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<Tier>,
    },
    /// Block until the caller's sub-tasks have news; takes nothing.
    Wait {
        tasks: Vec<String>,
        timeout_ms: Option<u64>,
    },
    /// Take what the caller's sub-tasks have for it; never blocks.
    Inbox {
        tasks: Vec<String>,
    },
    Watch {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        status: bool,
    },
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
    Answers,
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
    /// Whether the caller has unseen events that need it; for host hooks.
    Pending,
}

impl Request {
    /// A sandboxed caller may talk to tasks it already has, including resuming one with `send`,
    /// but may not create new ones or change their permissions outside its sandbox.
    pub fn allowed_in_sandbox(&self) -> bool {
        !matches!(
            self,
            Request::Spawn(_)
                | Request::Models { .. }
                | Request::Send { tier: Some(_), .. }
                | Request::Stop { .. }
                | Request::Gc { .. }
        )
    }

    /// Requests that may start an agent process, and so carry the caller's environment.
    pub fn needs_env(&self) -> bool {
        matches!(
            self,
            Request::Spawn(_)
                | Request::Models { .. }
                | Request::Send { .. }
                | Request::Reply { .. }
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

/// Output that carries news always exits with OK: hosts cut long output harder after a nonzero
/// exit (Claude Code keeps 5,000 characters from each end), and Codex on Windows reports every
/// nonzero code as 1.
pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    /// Nothing new, or `wait` timed out; sub-tasks are still running.
    pub const NOTHING_NEW: i32 = 3;
    /// Nothing new and no sub-task running.
    pub const NOTHING_RUNNING: i32 = 4;
}

pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

pub struct Reader<R> {
    input: BufReader<R>,
    /// The line read so far; kept across a cancelled read.
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(r: R) -> Self {
        Reader {
            input: BufReader::new(r),
            line: Vec::new(),
        }
    }

    /// Next message, or None at end of stream.
    pub async fn read<T: for<'de> Deserialize<'de>>(&mut self) -> std::io::Result<Option<T>> {
        // Stop at the limit plus "\r\n": a peer that never ends its line must not grow the buffer.
        let room = (MAX_LINE + 2).saturating_sub(self.line.len()) as u64;
        (&mut self.input)
            .take(room)
            .read_until(b'\n', &mut self.line)
            .await?;
        let mut line = std::mem::take(&mut self.line);
        if line.is_empty() {
            return Ok(None);
        }
        if line.pop_if(|b| *b == b'\n').is_some() {
            line.pop_if(|b| *b == b'\r');
        }
        if line.len() > MAX_LINE {
            return Err(std::io::Error::other("message too long"));
        }
        serde_json::from_slice(&line)
            .map(Some)
            .map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_tier_requires_an_unrestricted_caller() {
        let old: Request =
            serde_json::from_str(r#"{"op":"send","task":"t1","message":"continue"}"#).unwrap();
        assert!(old.allowed_in_sandbox());
        assert!(old.needs_env());
        for tier in ["read", "work", "full"] {
            let request: Request = serde_json::from_value(serde_json::json!({
                "op": "send", "task": "t1", "message": "continue", "tier": tier,
            }))
            .unwrap();
            assert!(!request.allowed_in_sandbox());
            assert!(request.needs_env());
            assert_eq!(serde_json::to_value(&request).unwrap()["tier"], tier);
        }
    }

    #[tokio::test]
    async fn reader_splits_lines_and_stops_at_the_limit() {
        let input = b"{\"a\":1}\r\n{\"a\":2}\n{\"a\":3}";
        let mut reader = Reader::new(input.as_slice());
        for want in 1..=3 {
            let got = reader.read::<Value>().await.unwrap().unwrap();
            assert_eq!(got["a"], want);
        }
        assert!(reader.read::<Value>().await.unwrap().is_none());

        let mut longest = vec![b' '; MAX_LINE - 1];
        longest.extend_from_slice(b"7\r\n");
        longest.extend_from_slice(&vec![b' '; MAX_LINE]);
        longest.extend_from_slice(b"8\n");
        let mut reader = Reader::new(longest.as_slice());
        assert_eq!(reader.read::<Value>().await.unwrap(), Some(Value::from(7)));
        let err = reader.read::<Value>().await.unwrap_err();
        assert_eq!(err.to_string(), "message too long");

        // A peer that never ends its line is cut off instead of filling memory.
        let mut reader = Reader::new(tokio::io::repeat(b' '));
        let err = reader.read::<Value>().await.unwrap_err();
        assert_eq!(err.to_string(), "message too long");
    }

    #[test]
    fn model_queries_carry_environment_and_cannot_escape_a_sandbox() {
        let request = Request::Models {
            agent: Agent::Grok,
            cwd: PathBuf::from("."),
        };
        assert!(request.needs_env());
        assert!(!request.allowed_in_sandbox());
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["op"], "models");
        assert!(matches!(
            serde_json::from_value::<Request>(wire).unwrap(),
            Request::Models {
                agent: Agent::Grok,
                ..
            }
        ));
    }
}
