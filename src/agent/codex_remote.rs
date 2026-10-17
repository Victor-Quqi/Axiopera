//! A host Codex session, reached through the user's shared Codex app-server daemon, which the
//! interactive Codex uses by default. `codex app-server proxy` relays bytes to the daemon's
//! control socket; the daemon speaks JSON-RPC over WebSocket.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct Remote {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    buf: Vec<u8>,
    next: u64,
    /// A message went out and was not refused.
    sent: bool,
}

impl Remote {
    /// `codex_home` locates the daemon; `None` means Codex's default.
    pub async fn connect(exe: &Path, codex_home: Option<&Path>) -> Result<Remote> {
        let mut cmd = Command::new(exe);
        cmd.args(["app-server", "proxy"]);
        match codex_home {
            Some(home) => cmd.env("CODEX_HOME", home),
            None => cmd.env_remove("CODEX_HOME"),
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // The daemon has no console; without this Windows opens a window for the proxy.
        #[cfg(windows)]
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let mut child = cmd.spawn().context("starting `codex app-server proxy`")?;
        let mut r = Remote {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            _child: child,
            buf: Vec::new(),
            next: 0,
            sent: false,
        };
        let key = base64(&uuid::Uuid::new_v4().into_bytes());
        r.stdin
            .write_all(
                format!(
                    "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                     Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let end = loop {
            if let Some(i) = r.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            r.read_more().await?;
        };
        let head: Vec<u8> = r.buf.drain(..end).collect();
        if !head.starts_with(b"HTTP/1.1 101") {
            bail!("the Codex daemon refused the connection");
        }
        r.call(
            "initialize",
            json!({"clientInfo": {"name": "axiopera", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        r.send_frame(
            0x1,
            json!({"method": "initialized", "params": {}})
                .to_string()
                .as_bytes(),
        )
        .await?;
        Ok(r)
    }

    /// Whether the session is open in the daemon. A closed one must not be messaged: that
    /// would run a turn nobody watches.
    pub async fn is_open(&mut self, thread: &str) -> Result<bool> {
        let mut cursor = Value::Null;
        loop {
            let page = self
                .call("thread/loaded/list", json!({"cursor": cursor}))
                .await?;
            let ids = page["data"].as_array().into_iter().flatten();
            if ids.filter_map(Value::as_str).any(|id| id == thread) {
                return Ok(true);
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                return Ok(false);
            }
        }
    }

    /// Into the running turn, or as a new turn if the session is idle. Returns which.
    pub async fn message(&mut self, thread: &str, text: &str) -> Result<&'static str> {
        let input = json!([{"type": "text", "text": text}]);
        let mut last_err = None;
        // The session may start or end a turn between our look and our message.
        for _ in 0..3 {
            let turns = self
                .call(
                    "thread/turns/list",
                    json!({"threadId": thread, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"}),
                )
                .await?;
            let last = &turns["data"][0];
            // Cleared again by a refusal.
            self.sent = true;
            let sent = if last["status"] == "inProgress" {
                self.call(
                    "turn/steer",
                    json!({"threadId": thread, "expectedTurnId": last["id"], "input": input}),
                )
                .await
                .map(|_| "steered into its running turn")
            } else {
                self.call("turn/start", json!({"threadId": thread, "input": input}))
                    .await
                    .map(|_| "started a turn")
            };
            match sent {
                Ok(how) => return Ok(how),
                Err(e) if self.sent => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap())
    }

    /// Whether `message` got its message out, even if it then failed waiting for the answer.
    pub fn sent(&self) -> bool {
        self.sent
    }

    pub async fn contains_message(&mut self, thread: &str, marker: &str) -> Result<bool> {
        let mut cursor = Value::Null;
        loop {
            let page = self
                .call(
                    "thread/turns/list",
                    json!({
                        "threadId": thread, "limit": 20, "sortDirection": "desc",
                        "itemsView": "full", "cursor": cursor,
                    }),
                )
                .await?;
            let turns = page["data"]
                .as_array()
                .context("thread/turns/list omitted data")?;
            if turns.iter().any(|t| !t["items"].is_array()) {
                bail!("thread/turns/list omitted full items");
            }
            if turns.iter().any(|t| has_marker(t, marker)) {
                return Ok(true);
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                return Ok(false);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let request = json!({"id": id, "method": method, "params": params});
        self.send_frame(0x1, request.to_string().as_bytes()).await?;
        loop {
            let m = self.recv().await?;
            // Notifications and the daemon's own requests carry a method; replies do not.
            if m["id"] != json!(id) || m.get("method").is_some() {
                continue;
            }
            if let Some(e) = m.get("error") {
                if matches!(method, "turn/steer" | "turn/start") {
                    self.sent = false;
                }
                bail!("{method}: {}", e["message"].as_str().unwrap_or("failed"));
            }
            return Ok(m["result"].clone());
        }
    }

    async fn send_frame(&mut self, opcode: u8, data: &[u8]) -> Result<()> {
        let mut frame = vec![0x80 | opcode];
        match data.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend((n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend((n as u64).to_be_bytes());
            }
        }
        // Client frames must be masked.
        let mask = uuid::Uuid::new_v4().into_bytes();
        frame.extend(&mask[..4]);
        frame.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Value> {
        let mut message = Vec::new();
        loop {
            self.fill(2).await?;
            let fin = self.buf[0] & 0x80 != 0;
            let opcode = self.buf[0] & 0x0F;
            let (start, len) = match self.buf[1] & 0x7F {
                126 => {
                    self.fill(4).await?;
                    (4, u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize)
                }
                127 => {
                    self.fill(10).await?;
                    (10, u64::from_be_bytes(self.buf[2..10].try_into()?) as usize)
                }
                n => (2, n as usize),
            };
            self.fill(start + len).await?;
            let payload: Vec<u8> = self.buf.drain(..start + len).skip(start).collect();
            match opcode {
                0x8 => bail!("the Codex daemon closed the connection"),
                0x9 => self.send_frame(0xA, &payload).await?,
                0xA => {}
                _ => {
                    message.extend(payload);
                    if fin {
                        return serde_json::from_slice(&message).map_err(|e| anyhow!(e));
                    }
                }
            }
        }
    }

    async fn fill(&mut self, n: usize) -> Result<()> {
        while self.buf.len() < n {
            self.read_more().await?;
        }
        Ok(())
    }

    async fn read_more(&mut self) -> Result<()> {
        let mut chunk = [0u8; 8192];
        let n = self.stdout.read(&mut chunk).await?;
        if n == 0 {
            bail!("the Codex daemon is not running");
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }
}

fn has_marker(turn: &Value, marker: &str) -> bool {
    turn["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "userMessage")
        .any(|item| {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains(marker))
                })
        })
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipt_matches_only_user_content() {
        use serde_json::json;
        let marker = "[axiopera:batch]";
        let turn = json!({"items": [
            {"type": "agentMessage", "content": [{"text": marker}]},
            {"type": "userMessage", "content": [{"text": "something else"}]}
        ]});
        assert!(!super::has_marker(&turn, marker));
        let turn = json!({"items": [{"type": "userMessage", "content": [{"text": format!("{marker} result")}]}]});
        assert!(super::has_marker(&turn, marker));
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
