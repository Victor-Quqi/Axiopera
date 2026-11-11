//! Delivery queries and bounded reads for reminders and logs.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::{Daemon, State, cursor_key, render, within};
use crate::{agent, store::Event};

/// An ID for one handover: what `inbox`, `result` or a push gave a caller at once. A message
/// axiopera sends into a host session carries one as its `clientUserMessageId`, where the agent
/// does not read it.
pub(super) fn mark() -> String {
    format!(
        "axiopera:{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

pub(super) fn index_approval(index: &mut HashMap<String, Vec<usize>>, event: &Event, at: usize) {
    if event.kind == "approval_request"
        && let Some(caller) = event.data.get("approver").and_then(Value::as_str)
    {
        index.entry(caller.to_string()).or_default().push(at);
    }
}

pub(super) fn approval_index(events: &[Event]) -> HashMap<String, Vec<usize>> {
    let mut index = HashMap::new();
    for (at, event) in events.iter().enumerate() {
        index_approval(&mut index, event, at);
    }
    index
}

impl Daemon {
    /// Record what reached `to` at once, in the log of each task it tells of: the events
    /// `shown`, and the `turns` whose result text went along. Nothing reads it back here; it
    /// tells whoever watches the store what an agent was given, which the text given does not.
    pub(super) fn record_delivery(
        &self,
        to: &str,
        via: &str,
        mark: &str,
        shown: &[(String, u64)],
        turns: &[(String, u32)],
    ) {
        let mut tasks: Vec<&String> = shown
            .iter()
            .map(|(task, _)| task)
            .chain(turns.iter().map(|(task, _)| task))
            .collect();
        tasks.sort_unstable();
        tasks.dedup();
        // A task gc removed meanwhile took its log along.
        tasks.retain(|task| self.lock().tasks.contains_key(*task));
        for task in tasks {
            let events: Vec<u64> = shown
                .iter()
                .filter(|(t, _)| t == task)
                .map(|(_, seq)| *seq)
                .collect();
            let results: Vec<u32> = turns
                .iter()
                .filter(|(t, _)| t == task)
                .map(|(_, turn)| *turn)
                .collect();
            self.emit(
                task,
                "delivered",
                serde_json::json!({
                    "to": to, "via": via, "batch": mark, "events": events, "results": results,
                }),
            );
        }
    }

    /// Keep historical approval routes, including requests whose process has gone away.
    pub(super) fn unseen(&self, st: &State, caller: &str, only: &[String]) -> Vec<Event> {
        let mut ready = Vec::new();
        for (id, entry) in &st.tasks {
            let child = entry.task.parent == caller && (only.is_empty() || only.contains(id));
            let approvals = entry.approval_events.get(caller);
            if !child
                && (approvals.is_none()
                    || (!only.is_empty() && !only.iter().any(|root| within(st, &entry.task, root))))
            {
                continue;
            }
            let cursor = st
                .ledger
                .cursors
                .get(&cursor_key(caller, id))
                .copied()
                .unwrap_or(0);
            if child {
                let start = entry.deliverable.partition_point(|e| e.seq <= cursor);
                ready.extend(
                    entry.deliverable[start..]
                        .iter()
                        .filter(|e| {
                            e.kind != "approval_request"
                                || e.data.get("approver").and_then(Value::as_str) == Some(caller)
                        })
                        .cloned(),
                );
            } else if let Some(indices) = approvals {
                let start = indices.partition_point(|&at| entry.deliverable[at].seq <= cursor);
                ready.extend(
                    indices[start..]
                        .iter()
                        .map(|&at| entry.deliverable[at].clone()),
                );
            }
        }
        ready.sort_by_key(|e| e.seq);
        ready
    }
}

const PREVIEW_CAPACITY: usize = 256;
const PREVIEW_TTL: Duration = Duration::from_secs(30);

struct Preview {
    len: u64,
    modified: SystemTime,
    loaded: Instant,
    text: String,
}

#[derive(Default)]
pub(super) struct Previews(HashMap<PathBuf, Preview>);

impl Previews {
    pub(super) fn get(&mut self, path: &Path) -> String {
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        if let Some((len, modified)) = stamp
            && let Some(p) = self.0.get(path)
            && p.len == len
            && p.modified == modified
            && p.loaded.elapsed() < PREVIEW_TTL
        {
            return p.text.clone();
        }
        self.0.remove(path);
        let Ok(text) = std::fs::read_to_string(path) else {
            return String::new();
        };
        let text = agent::clip(&text, render::PREVIEW);
        // Replacements during a read must not acquire the old file's fingerprint.
        let after = std::fs::metadata(path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        if let Some((len, modified)) = stamp.filter(|_| stamp == after) {
            self.0.retain(|_, p| p.loaded.elapsed() < PREVIEW_TTL);
            if self.0.len() >= PREVIEW_CAPACITY {
                let oldest = self
                    .0
                    .iter()
                    .min_by_key(|(_, p)| p.loaded)
                    .map(|(k, _)| k.clone())
                    .unwrap();
                self.0.remove(&oldest);
            }
            self.0.insert(
                path.to_path_buf(),
                Preview {
                    len,
                    modified,
                    loaded: Instant::now(),
                    text: text.clone(),
                },
            );
        }
        text
    }
}

/// Read backwards until enough valid events have been found, including an unterminated line.
pub(super) fn tail_events(path: &Path, count: usize) -> std::io::Result<Vec<Event>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut offset = file.metadata()?.len();
    let mut chunk = [0; 8192];
    let mut line = Vec::new();
    let mut events = Vec::new();
    while offset > 0 && events.len() < count {
        let size = offset.min(chunk.len() as u64) as usize;
        offset -= size as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut chunk[..size])?;
        for &byte in chunk[..size].iter().rev() {
            if byte == b'\n' {
                line.reverse();
                if let Ok(event) = serde_json::from_slice(&line) {
                    events.push(event);
                }
                line.clear();
                if events.len() == count {
                    break;
                }
            } else {
                line.push(byte);
            }
        }
    }
    if offset == 0 && events.len() < count {
        line.reverse();
        if let Ok(event) = serde_json::from_slice(&line) {
            events.push(event);
        }
    }
    events.reverse();
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;
    use serde_json::json;

    #[test]
    fn tail_matches_full_read_with_long_unicode_crlf_and_broken_lines() {
        let f = fixture();
        let path = f.d.store.events_path("test");
        assert!(tail_events(&path, 10).unwrap().is_empty());
        let mut bytes = Vec::new();
        for seq in 1..=8 {
            let event = Event {
                seq,
                ts: 0,
                task: "test".into(),
                kind: "said".into(),
                data: json!({"text": "中文🦀".repeat(seq as usize * 1100)})
                    .as_object()
                    .unwrap()
                    .clone(),
            };
            bytes.extend(serde_json::to_vec(&event).unwrap());
            bytes.extend(b"\r\ninvalid json\n\n");
        }
        for ending in [b"".as_slice(), b"{broken", b"\n"] {
            let mut content = bytes.clone();
            content.extend(ending);
            std::fs::write(&path, content).unwrap();
            let full = f.d.store.events("test");
            for count in [0, 1, 3, 8, 20] {
                assert_eq!(
                    serde_json::to_value(tail_events(&path, count).unwrap()).unwrap(),
                    serde_json::to_value(&full[full.len().saturating_sub(count)..]).unwrap()
                );
            }
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&f.d.store.events("test")[0]).unwrap(),
        )
        .unwrap();
        assert_eq!(tail_events(&path, 10).unwrap().len(), 1);
        std::fs::write(&path, "").unwrap();
        assert!(tail_events(&path, 10).unwrap().is_empty());
    }

    #[test]
    fn previews_invalidate_on_changes_deletion_expiry_and_bound_capacity() {
        let f = fixture();
        let mut cache = Previews::default();
        let path = f.d.store.turn_path("test", 1);
        assert_eq!(cache.get(&path), "");
        f.d.store.save_turn("test", 1, "old").unwrap();
        assert_eq!(cache.get(&path), "old");
        let loaded = cache.0[&path].loaded;
        assert_eq!(cache.get(&path), "old");
        assert_eq!(cache.0[&path].loaded, loaded);
        f.d.store.save_turn("test", 1, "a changed result").unwrap();
        assert_eq!(cache.get(&path), "a changed result");
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        f.d.store.save_turn("test", 1, "same size change").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        cache.0.get_mut(&path).unwrap().loaded = Instant::now() - PREVIEW_TTL;
        assert_eq!(cache.get(&path), "same size change");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.get(&path), "");
        assert!(!cache.0.contains_key(&path));
        for turn in 0..=PREVIEW_CAPACITY {
            let path = f.d.store.save_turn("test", turn as u32, "text").unwrap();
            cache.get(&path);
        }
        assert_eq!(cache.0.len(), PREVIEW_CAPACITY);
    }

    #[test]
    fn reminders_only_load_the_visible_results_and_preserve_newest_sequence() {
        let f = fixture();
        for turn in 1..=14 {
            f.d.store.save_turn("test", turn, "one\ntwo\n中文").unwrap();
            f.d.emit("test", "turn_ended", json!({"turn": turn, "ok": true}));
        }
        let (text, seq) = f.d.reminder("codex:host").unwrap();
        assert!(text.contains("…and 4 more"));
        assert!(text.contains("one ↵ two ↵ 中文"));
        assert_eq!(seq, f.d.lock().seq);
        let cache = f.d.previews.lock().unwrap();
        assert_eq!(cache.0.len(), 10);
        assert!(!cache.0.contains_key(&f.d.store.turn_path("test", 11)));
    }

    #[test]
    fn repeated_file_changes_do_not_rewrite_the_task() {
        let f = fixture();
        f.d.file_changed("test", "a.rs").unwrap();
        assert_eq!(f.d.store.load_all()[0].changed_files, ["a.rs"]);
        let path = f.d.store.events_path("test").with_file_name("task.json");
        std::fs::write(&path, "write detector").unwrap();
        let updated = f.d.lock().tasks["test"].task.updated_ms;
        f.d.file_changed("test", "a.rs").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "write detector");
        assert_eq!(f.d.lock().tasks["test"].task.updated_ms, updated);
    }

    #[test]
    fn file_changes_can_retry_after_a_save_failure() {
        let f = fixture();
        let tmp =
            f.d.store
                .events_path("test")
                .with_file_name("task.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(f.d.file_changed("test", "retry.rs").is_err());
        assert!(f.d.lock().tasks["test"].task.changed_files.is_empty());
        std::fs::remove_dir(tmp).unwrap();
        f.d.file_changed("test", "retry.rs").unwrap();
        assert_eq!(f.d.store.load_all()[0].changed_files, ["retry.rs"]);
    }
}
