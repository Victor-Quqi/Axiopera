//! Sub-task events sent straight into host sessions that take messages, so a host learns of
//! them while it keeps working, or after it ended its turn, without running `axiopera inbox`.
//! Only Codex sessions so far: those on the user's shared Codex daemon get the events; those of
//! the Codex app and IDE extension get a reminder to run `inbox` once their turn ends.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use serde_json::Value;
use tokio::sync::broadcast;

use super::{Daemon, RELEASED, is_news};
use crate::agent::{self, codex_remote::Remote};
use crate::ipc::Agent;
use crate::store::{CodexHost, Event, PendingPush};

/// Events that arrive close together go out as one message.
const GATHER: Duration = Duration::from_secs(1);
/// How long `spawn` may spend finding out whether the caller takes messages.
const PROBE: Duration = Duration::from_secs(5);
/// A push that takes longer is tried again later.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Retries after a failed push start here and double up to the maximum.
const RETRY: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(600);

/// How a host learns of its sub-tasks' events.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Reach {
    Push,
    Doorbell,
    Wait,
}

impl Daemon {
    /// Register the caller as a host to push to if it is a Codex session the user's Codex
    /// daemon has open, or to ring if a long-lived client runs it.
    pub(super) async fn register_host(&self, caller: &str, env: &[(String, String)]) -> Reach {
        let Some(thread) = caller.strip_prefix("codex:") else {
            return Reach::Wait;
        };
        let configured = self.opts.agents.get(Agent::Codex).path.as_deref();
        let Ok(exe) = agent::resolve_exe(Agent::Codex, configured, env) else {
            return Reach::Wait;
        };
        let codex_home = env
            .iter()
            .find(|(k, _)| k == "CODEX_HOME")
            .map(|(_, v)| PathBuf::from(v));
        let mut host = CodexHost {
            exe,
            codex_home,
            thread: thread.to_string(),
            doorbell: false,
        };
        let home = host.codex_home.as_deref();
        let reach = if self
            .probe(caller, "is not on the shared Codex daemon", async {
                Remote::connect(&host.exe, home)
                    .await?
                    .is_open(thread)
                    .await
            })
            .await
        {
            Reach::Push
        } else if self
            .probe(caller, "has no Codex queue", async {
                Remote::private(&host.exe, home)
                    .await?
                    .serves_queue(thread)
                    .await
            })
            .await
        {
            Reach::Doorbell
        } else {
            return Reach::Wait;
        };
        host.doorbell = reach == Reach::Doorbell;
        let mut st = self.lock();
        st.ledger.codex_hosts.insert(caller.to_string(), host);
        if let Err(e) = st.ledger.save(&self.opts.home) {
            self.log(&format!("saving the ledger: {e:#}"));
        }
        reach
    }

    async fn probe(
        &self,
        caller: &str,
        failure: &str,
        check: impl Future<Output = anyhow::Result<bool>>,
    ) -> bool {
        match tokio::time::timeout(PROBE, check).await {
            Ok(Ok(yes)) => yes,
            Ok(Err(e)) => {
                self.log(&format!("{caller} {failure}: {e:#}"));
                false
            }
            Err(_) => {
                self.log(&format!("{caller} {failure}: Codex did not answer"));
                false
            }
        }
    }

    /// Push to every host an event concerns. Starts with what an earlier daemon left behind.
    pub(super) async fn pusher(self: Arc<Self>) {
        let mut rx = self.events.subscribe();
        self.push_all();
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.push_all();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if e.kind == RELEASED {
                continue;
            }
            let target = {
                let st = self.lock();
                if e.kind == "approval_request" {
                    e.data
                        .get("approver")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    st.tasks.get(&e.task).map(|t| t.task.parent.clone())
                }
            };
            if let Some(target) = target
                && is_news(&e, &target)
            {
                self.schedule_push(target, GATHER);
            }
        }
    }

    fn push_all(self: &Arc<Self>) {
        let hosts: Vec<String> = self.lock().ledger.codex_hosts.keys().cloned().collect();
        for host in hosts {
            self.schedule_push(host, GATHER);
        }
    }

    /// Push the caller's unseen events after `delay`, unless a push is due sooner already.
    fn schedule_push(self: &Arc<Self>, caller: String, delay: Duration) {
        let due = Instant::now() + delay;
        {
            let mut st = self.lock();
            if !st.ledger.codex_hosts.contains_key(&caller)
                || st.pushing.get(&caller).is_some_and(|d| *d <= due)
            {
                return;
            }
            st.pushing.insert(caller.clone(), due);
        }
        let d = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            {
                let mut st = d.lock();
                // Superseded by a sooner push.
                if st.pushing.get(&caller) != Some(&due) {
                    return;
                }
                st.pushing.remove(&caller);
            }
            d.push(&caller).await;
        });
    }

    fn clear_pending_push(&self, caller: &str) {
        let mut st = self.lock();
        st.ledger.pending_pushes.remove(caller);
        if let Err(e) = st.ledger.save(&self.opts.home) {
            self.log(&format!("saving push state: {e:#}"));
        }
    }

    async fn reconcile_push(
        &self,
        caller: &str,
        thread: &str,
        remote: &mut Remote,
    ) -> anyhow::Result<()> {
        let pending = self.lock().ledger.pending_pushes.get(caller).cloned();
        let Some(pending) = pending else {
            return Ok(());
        };
        let found =
            tokio::time::timeout(PROBE, remote.contains_message(thread, &pending.marker)).await;
        let delivered = match found {
            Ok(Ok(found)) => found,
            other => {
                self.log(&format!("{caller} push read-back failed ({other:?}); treating the written batch as delivered"));
                true
            }
        };
        self.settle_push(caller, pending, delivered)
    }

    fn settle_push(
        &self,
        caller: &str,
        pending: PendingPush,
        delivered: bool,
    ) -> anyhow::Result<()> {
        if delivered {
            for (task, turn) in pending.turns {
                self.update(&task, |t| t.delivered = t.delivered.max(turn));
            }
        }
        let mut st = self.lock();
        if delivered {
            for (task, seq) in pending.events {
                let cursor = st
                    .ledger
                    .cursors
                    .entry(super::cursor_key(caller, &task))
                    .or_default();
                *cursor = (*cursor).max(seq);
            }
        }
        st.ledger.pending_pushes.remove(caller);
        st.ledger.save(&self.opts.home)?;
        self.log(&format!(
            "{caller} push read-back: {}",
            if delivered {
                "delivered"
            } else {
                "absent; retrying"
            }
        ));
        Ok(())
    }

    pub(super) async fn reconcile_before_take(&self, caller: &str) -> anyhow::Result<()> {
        let pair = {
            let st = self.lock();
            st.ledger
                .pending_pushes
                .get(caller)
                .cloned()
                .zip(st.ledger.codex_hosts.get(caller).cloned())
        };
        if let Some((pending, host)) = pair {
            match tokio::time::timeout(
                PROBE,
                Remote::connect(&host.exe, host.codex_home.as_deref()),
            )
            .await
            {
                Ok(Ok(mut remote)) => {
                    self.reconcile_push(caller, &host.thread, &mut remote)
                        .await?
                }
                _ => {
                    self.log(&format!("{caller} push read-back unavailable; treating the written batch as delivered"));
                    self.settle_push(caller, pending, true)?;
                }
            }
        }
        Ok(())
    }

    /// Tell the caller of its events. Tried again later if that fails.
    async fn push(self: &Arc<Self>, caller: &str) {
        let delivering = self.delivery(caller).lock_owned().await;
        {
            let st = self.lock();
            if !self
                .unseen(&st, caller, &[])
                .iter()
                .any(|e| is_news(e, caller))
                && !st.ledger.pending_pushes.contains_key(caller)
            {
                return;
            }
        }
        let host = self.lock().ledger.codex_hosts.get(caller).cloned();
        let Some(host) = host else {
            return;
        };
        let outcome = if host.doorbell {
            self.ring(caller, &host).await.map(|how| (how, false))
        } else {
            self.send_events(caller, &host).await
        };
        match outcome {
            Ok((how, more)) => {
                self.lock().push_failures.remove(caller);
                self.log(&format!("push to {caller}: {how}"));
                if more {
                    drop(delivering);
                    self.schedule_push(caller.to_string(), GATHER);
                }
            }
            Err(e) => {
                let failures = {
                    let mut st = self.lock();
                    let n = st.push_failures.entry(caller.to_string()).or_default();
                    *n += 1;
                    *n
                };
                let delay = RETRY
                    .saturating_mul(1 << (failures - 1).min(5))
                    .min(RETRY_MAX);
                self.log(&format!(
                    "{caller} kept its events: {e}; trying again in {}s",
                    delay.as_secs()
                ));
                drop(delivering);
                self.schedule_push(caller.to_string(), delay);
            }
        }
    }

    /// Send the caller what `inbox` would print. Also says whether events are left for another
    /// message.
    async fn send_events(&self, caller: &str, host: &CodexHost) -> Result<(String, bool), String> {
        let mut remote = None;
        let mut batch = None;
        let sent = tokio::time::timeout(TIMEOUT, async {
            let r = remote.insert(Remote::connect(&host.exe, host.codex_home.as_deref()).await?);
            self.reconcile_push(caller, &host.thread, r).await?;
            let events = self.unseen(&self.lock(), caller, &[]);
            if !events.iter().any(|e| is_news(e, caller)) {
                return Ok(("no events requiring attention".to_string(), false));
            }
            if !r.is_open(&host.thread).await? {
                anyhow::bail!("the session is closed");
            }
            let taken = self.batch(&events);
            let texts: Vec<&str> = taken.items.iter().map(|(_, t)| t.as_str()).collect();
            let marker = format!(
                "[axiopera:{}]",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            );
            let message = format!(
                "{marker} From your sub-tasks (sent by axiopera, not typed by the user):\n{}",
                texts.join("\n")
            );
            let events: Vec<Event> = taken.items.iter().map(|(e, _)| e.clone()).collect();
            let pending = PendingPush {
                marker,
                events: events.iter().map(|e| (e.task.clone(), e.seq)).collect(),
                turns: taken.turns.clone(),
            };
            // Save before writing: a daemon crash can also lose the acknowledgement.
            {
                let mut st = self.lock();
                st.ledger.pending_pushes.insert(caller.to_string(), pending);
                st.ledger.save(&self.opts.home)?;
            }
            batch = Some((events, taken.turns));
            let how = r.message(&host.thread, &message).await?;
            anyhow::Ok((how.to_string(), !taken.left.is_empty()))
        })
        .await;
        let outcome = match sent {
            Ok(result) => result.map_err(|e| format!("{e:#}")),
            Err(_) => Err("timed out".to_string()),
        };
        if outcome.is_ok() {
            if let Some((events, turns)) = &batch {
                if let Err(e) = self.mark_seen(caller, events, turns) {
                    self.log(&format!("saving cursors: {e:#}"));
                }
                self.clear_pending_push(caller);
            }
        } else if batch.is_some() && !remote.as_ref().is_some_and(Remote::sent) {
            self.clear_pending_push(caller);
        }
        outcome
    }

    /// Queue a reminder for the host's next turn, unless the last one is still queued.
    async fn ring(&self, caller: &str, host: &CodexHost) -> Result<String, String> {
        let rung = self.lock().ledger.doorbells.get(caller).cloned();
        let result = tokio::time::timeout(TIMEOUT, async {
            let mut r = Remote::private(&host.exe, host.codex_home.as_deref()).await?;
            if let Some(id) = rung
                && r.queued(&host.thread, &id).await?
            {
                return Ok("its reminder is still queued".to_string());
            }
            let Some((news, _)) = self.reminder(caller) else {
                return Ok("nothing to remind of".to_string());
            };
            let text =
                format!("From your sub-tasks (sent by axiopera, not typed by the user):\n{news}");
            let id = r.enqueue(&host.thread, &text).await?;
            let mut st = self.lock();
            st.ledger.doorbells.insert(caller.to_string(), id);
            st.ledger.save(&self.opts.home)?;
            anyhow::Ok("queued a reminder".to_string())
        })
        .await;
        match result {
            Ok(result) => result.map_err(|e| format!("{e:#}")),
            Err(_) => Err("timed out".to_string()),
        }
    }

    /// After the host ran `inbox`: a reminder still queued would start a turn for nothing.
    pub(super) fn retract_doorbell(self: &Arc<Self>, caller: &str) {
        let (id, host) = {
            let mut st = self.lock();
            if self
                .unseen(&st, caller, &[])
                .iter()
                .any(|e| is_news(e, caller))
            {
                return;
            }
            let Some(id) = st.ledger.doorbells.remove(caller) else {
                return;
            };
            if let Err(e) = st.ledger.save(&self.opts.home) {
                self.log(&format!("saving the ledger: {e:#}"));
            }
            (id, st.ledger.codex_hosts.get(caller).cloned())
        };
        let Some(host) = host else {
            return;
        };
        let d = self.clone();
        let caller = caller.to_string();
        tokio::spawn(async move {
            let deleted = tokio::time::timeout(TIMEOUT, async {
                Remote::private(&host.exe, host.codex_home.as_deref())
                    .await?
                    .dequeue(&host.thread, &id)
                    .await
            })
            .await;
            let how = match deleted {
                Ok(Ok(true)) => "withdrew its queued reminder".to_string(),
                Ok(Ok(false)) => return,
                Ok(Err(e)) => format!("could not withdraw its queued reminder: {e:#}"),
                Err(_) => "could not withdraw its queued reminder: timed out".to_string(),
            };
            d.log(&format!("{caller} {how}"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn only_actionable_events_start_a_host_turn() {
        let event = |kind: &str| Event {
            seq: 1,
            ts: 0,
            task: "test".into(),
            kind: kind.into(),
            data: Default::default(),
        };
        for kind in [
            "blocked",
            "steered",
            "input_read",
            "tool",
            "file_changed",
            "message",
            "detached",
        ] {
            assert!(!is_news(&event(kind), "codex:host"));
        }
        for kind in [
            "turn_ended",
            "question",
            "approval_request",
            "stalled",
            "failed",
            "cancelled",
            "interrupted",
        ] {
            assert!(is_news(&event(kind), "codex:host"));
        }
    }

    #[test]
    fn cancellation_keeps_its_author_and_only_notifies_other_callers() {
        for by in ["codex:host", "codex:other", "user", "timeout"] {
            let f = fixture();
            f.d.cancel_tree("test", "cancelled", by);
            let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data["by"], by);
            assert_eq!(is_news(&events[0], "codex:host"), by != "codex:host");
            let saved = f.d.store.events("test");
            assert_eq!(saved[0].data["by"], by);
            assert_eq!(is_news(&saved[0], "codex:host"), by != "codex:host");
        }
    }

    #[tokio::test]
    async fn own_cancellation_stays_for_wait_or_a_later_push_without_connecting() {
        let f = fixture();
        f.d.lock().ledger.codex_hosts.insert(
            "codex:host".into(),
            CodexHost {
                exe: f.d.opts.home.join("missing-codex"),
                codex_home: Some(f.d.opts.home.clone()),
                thread: "host".into(),
                doorbell: false,
            },
        );
        f.d.cancel_tree("test", "cancelled by request", "codex:host");
        f.d.push("codex:host").await;
        {
            let st = f.d.lock();
            assert!(st.push_failures.is_empty());
            assert!(st.pushing.is_empty());
            assert_eq!(f.d.unseen(&st, "codex:host", &[]).len(), 1);
        }
        f.d.emit("test", "stalled", serde_json::json!({"quiet_secs": 60}));
        let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|e| is_news(e, "codex:host")));
        assert_eq!(events[0].kind, "cancelled");
    }

    #[test]
    fn recovered_batch_advances_only_its_cursors_and_only_when_present() {
        for found in [false, true] {
            let f = fixture();
            let pending = PendingPush {
                marker: "[axiopera:receipt]".into(),
                events: vec![("test".into(), 7)],
                turns: vec![("test".into(), 1)],
            };
            {
                let mut st = f.d.lock();
                st.ledger
                    .pending_pushes
                    .insert("codex:host".into(), pending);
                st.ledger.save(&f.d.opts.home).unwrap();
            }
            let loaded = crate::store::Ledger::load(&f.d.opts.home);
            let pending = loaded.pending_pushes["codex:host"].clone();
            assert_eq!(pending.marker, "[axiopera:receipt]");
            f.d.settle_push("codex:host", pending, found).unwrap();
            let st = f.d.lock();
            assert!(st.ledger.pending_pushes.is_empty());
            assert_eq!(st.tasks["test"].task.delivered, u32::from(found));
            assert_eq!(
                st.ledger.cursors.get("codex:host|test").copied(),
                found.then_some(7)
            );
        }
    }

    #[test]
    fn hosts_registered_before_doorbells_are_pushed_to() {
        let host: CodexHost =
            serde_json::from_value(serde_json::json!({"exe": "codex", "thread": "t"})).unwrap();
        assert!(!host.doorbell);
        assert!(
            serde_json::to_value(&host)
                .unwrap()
                .get("doorbell")
                .is_none()
        );
    }

    #[tokio::test]
    async fn waiting_withdraws_the_reminder_once_nothing_needs_the_host() {
        let f = fixture();
        {
            let mut st = f.d.lock();
            st.ledger.codex_hosts.insert(
                "codex:host".into(),
                CodexHost {
                    exe: f.d.opts.home.join("missing-codex"),
                    codex_home: Some(f.d.opts.home.clone()),
                    thread: "host".into(),
                    doorbell: true,
                },
            );
            st.ledger
                .doorbells
                .insert("codex:host".into(), "queued".into());
        }
        f.d.emit("test", "stalled", serde_json::json!({"quiet_secs": 60}));
        f.d.retract_doorbell("codex:host");
        assert!(f.d.lock().ledger.doorbells.contains_key("codex:host"));
        let events = f.d.unseen(&f.d.lock(), "codex:host", &[]);
        f.d.mark_seen("codex:host", &events, &[]).unwrap();
        f.d.retract_doorbell("codex:host");
        assert!(f.d.lock().ledger.doorbells.is_empty());
        assert!(
            crate::store::Ledger::load(&f.d.opts.home)
                .doorbells
                .is_empty()
        );
    }
}
