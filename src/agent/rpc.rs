//! JSON-RPC 2.0 over a child's stdio, shared by the Codex and Grok adapters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::process::ChildStdout;
use tokio::sync::{mpsc, oneshot};

use super::Writer;

pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The reply to a `request_in_order`, queued behind the messages that preceded it.
    Reply(Result<Value, RpcError>),
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

enum Slot {
    Waiter(oneshot::Sender<Result<Value, RpcError>>),
    InOrder,
}

type Pending = Arc<Mutex<HashMap<u64, Slot>>>;

#[derive(Clone)]
pub struct Rpc {
    writer: Writer,
    next_id: Arc<AtomicU64>,
    pending: Pending,
}

impl Rpc {
    /// Start reading `stdout`. The returned channel closes when the child's stdout does.
    pub fn start(stdout: ChildStdout, writer: Writer) -> (Rpc, mpsc::UnboundedReceiver<Incoming>) {
        let pending: Pending = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, pending.clone(), tx, writer.trace.clone()));
        (
            Rpc {
                writer,
                next_id: Arc::new(AtomicU64::new(1)),
                pending,
            },
            rx,
        )
    }

    /// Send a request; the receiver yields its result whenever it arrives.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::io::Result<oneshot::Receiver<Result<Value, RpcError>>> {
        let (tx, rx) = oneshot::channel();
        self.send_request(Slot::Waiter(tx), method, params).await?;
        Ok(rx)
    }

    /// Send a request whose reply arrives as `Incoming::Reply`, after every notification the
    /// agent sent before it. A separate waiter could see the reply before those.
    pub async fn request_in_order(&self, method: &str, params: Value) -> std::io::Result<()> {
        self.send_request(Slot::InOrder, method, params).await
    }

    async fn send_request(&self, slot: Slot, method: &str, params: Value) -> std::io::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, slot);
        self.writer
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
    }

    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let rx = self.request(method, params).await?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow::Error::new(e).context(format!("{method} failed"))),
            Err(_) => anyhow::bail!("{method}: agent exited before replying"),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> std::io::Result<()> {
        self.writer
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> std::io::Result<()> {
        self.writer
            .send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    pub async fn respond_error(&self, id: Value, message: &str) -> std::io::Result<()> {
        self.writer
            .send(
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message}}),
            )
            .await
    }
}

async fn read_loop(
    stdout: ChildStdout,
    pending: Pending,
    tx: mpsc::UnboundedSender<Incoming>,
    trace: super::Trace,
) {
    let mut lines = super::lines(stdout);
    while let Ok(Some(line)) = lines.next_line().await {
        trace.line("recv", &line);
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (msg.get("id"), method) {
            (Some(id), Some(method)) => {
                let _ = tx.send(Incoming::Request {
                    id: id.clone(),
                    method,
                    params,
                });
            }
            (None, Some(method)) => {
                let _ = tx.send(Incoming::Notification { method, params });
            }
            (Some(id), None) => {
                let Some(slot) = id
                    .as_u64()
                    .and_then(|id| pending.lock().unwrap().remove(&id))
                else {
                    continue;
                };
                let result = match msg.get("error") {
                    Some(e) => Err(RpcError {
                        code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = match slot {
                    Slot::Waiter(tx) => tx.send(result).ok(),
                    Slot::InOrder => tx.send(Incoming::Reply(result)).ok(),
                };
            }
            (None, None) => {}
        }
    }
    // Fail whatever is still waiting.
    pending.lock().unwrap().clear();
}
