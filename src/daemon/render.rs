//! Text the calling agent reads. Short, one fact per line.

use std::path::Path;

use serde_json::Value;

use crate::ipc::{Agent, Tier};
use crate::store::{Event, Status, Task};

/// Token budget for one reply, under the smallest host truncation limit: Grok keeps 20,000
/// characters of a command's output, 5,000 tokens of ASCII by this count.
pub const RESPONSE_TOKENS: usize = 4_500;

/// Rough token count that errs high: four ASCII characters per token, one per other character.
pub fn tokens(s: &str) -> usize {
    s.chars()
        .map(|c| if c.is_ascii() { 1 } else { 4 })
        .sum::<usize>()
        .div_ceil(4)
}

pub fn agent_name(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Grok => "grok",
    }
}

pub fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Read => "read",
        Tier::Work => "work",
        Tier::Full => "full",
    }
}

/// What the tier actually means for this agent, including known gaps.
pub fn capability_notes(agent: Agent, tier: Tier) -> Vec<String> {
    let mut notes: Vec<String> = match (agent, tier) {
        (Agent::Grok, Tier::Work) => vec![
            "Grok file writes are unrestricted, including outside the working directory".into(),
        ],
        _ => Vec::new(),
    };
    if cfg!(windows) && agent != Agent::Codex && tier != Tier::Full {
        notes.push("no OS sandbox on Windows".into());
    }
    if tier == Tier::Read {
        notes.push(match agent {
            Agent::Codex | Agent::Claude => {
                "MCP: only tools marked read-only without a destructive hint are allowed".into()
            }
            Agent::Grok => "MCP: unavailable; search_tool and use_tool are hidden".into(),
        });
        notes.push("other requests needing approval are refused".into());
    }
    notes
}

fn get<'a>(e: &'a Event, k: &str) -> &'a Value {
    e.data.get(k).unwrap_or(&Value::Null)
}

pub fn event_line(e: &Event, agent: Agent) -> String {
    let who = format!("[{} {}]", e.task, agent_name(agent));
    let s = |k: &str| get(e, k).as_str().unwrap_or("").to_string();
    let n = |k: &str| get(e, k).as_u64().unwrap_or(0);
    match e.kind.as_str() {
        "spawned" => format!("{who} spawned"),
        "turn_started" => format!("{who} turn {} started", n("turn")),
        "steered" if get(e, "queued") == &Value::Bool(true) => {
            format!("{who} message queued for a following turn")
        }
        "steered" => {
            format!("{who} message sent to its running turn")
        }
        "input_read" => format!("{who} parent message included in the turn's model input"),
        "tool" => format!("{who} {}: {}", s("name"), s("detail")),
        "file_changed" => format!("{who} changed {}", s("path")),
        "blocked" => format!("{who} blocked: {}", s("detail")),
        "question" => format!(
            "{who} asks {}: {}\n  answer with: axiopera reply {} \"...\"",
            s("id"),
            s("text"),
            s("id")
        ),
        "answered" => format!("{who} question {} answered", s("id")),
        "approval_request" => format!(
            "{who} needs approval {}: {}\n  decide with: axiopera approve {}  |  axiopera deny {} --reason \"...\"",
            s("id"),
            s("summary"),
            s("id"),
            s("id")
        ),
        "message" => format!("{who} told {}: {}", s("to"), s("text")),
        "approved" => format!("{who} {} approved by {}", s("id"), s("by")),
        "denied" => format!("{who} {} denied by {}", s("id"), s("by")),
        "turn_ended" => {
            let status = if get(e, "ok") == &Value::Bool(true) {
                "finished".to_string()
            } else {
                format!("failed ({})", s("error"))
            };
            let mut line = format!("{who} turn {} {status}", n("turn"));
            if n("unread") > 0 {
                line.push_str(&format!(
                    "; this result predates {} parent message(s); queued for a following turn",
                    n("unread")
                ));
            }
            if n("unconfirmed") > 0 {
                line.push_str(&format!(
                    "; {} message(s) of yours may have arrived after this result",
                    n("unconfirmed")
                ));
            }
            if n("blocked") > 0 {
                line.push_str(&format!(
                    "; {} operation(s) refused this turn; see `axiopera log {}`",
                    n("blocked"),
                    e.task
                ));
            }
            let open: Vec<&str> = get(e, "open_questions")
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !open.is_empty() {
                line.push_str(&format!("; waiting for your answer to {}", open.join(", ")));
            }
            line
        }
        "stalled" => format!("{who} no activity for {}s (still running)", n("quiet_secs")),
        "failed" => format!("{who} failed: {}", s("error")),
        "cancelled" if s("reason") == "cancelled by request" && !s("by").is_empty() => {
            format!("{who} cancelled by {}", s("by"))
        }
        "cancelled" => format!("{who} cancelled: {}", s("reason")),
        "interrupted" => format!(
            "{who} interrupted: {}; continue with `axiopera send {} \"...\"`",
            s("reason"),
            e.task
        ),
        "detached" => format!("{who} detached: {}", s("reason")),
        other => format!("{who} {other}"),
    }
}

/// The text as is, or its tail within `max_tokens` with a note on where the rest is.
pub fn result_body(text: &str, path: &Path, max_tokens: usize) -> String {
    if tokens(text) <= max_tokens {
        return text.to_string();
    }
    let note = format!(
        "[truncated: showing the end only; full text in {}]
…",
        path.display()
    );
    let mut room = max_tokens.saturating_sub(tokens(&note)) * 4;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let cost = if c.is_ascii() { 1 } else { 4 };
        if cost > room {
            break;
        }
        room -= cost;
        start = i;
    }
    format!("{note}{}", &text[start..])
}

/// "t3 review (claude)": ID, name if any, and agent.
pub fn task_label(t: &Task) -> String {
    match &t.name {
        Some(name) => format!("{} {name} ({})", t.id, agent_name(t.agent)),
        None => format!("{} ({})", t.id, agent_name(t.agent)),
    }
}

/// "45s", "4m12s", "1h05m".
pub fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

pub fn status_line(t: &Task, live: bool) -> String {
    let status = match t.status {
        Status::Queued => "queued",
        Status::Running if live => "running",
        Status::Running => "finishing",
        Status::Idle => "idle",
        Status::Waiting => "waiting for an answer",
        Status::Cancelled => "cancelled",
        Status::Failed => "failed",
        Status::Interrupted => "interrupted",
    };
    let mut line = format!(
        "{}  {}/{}  {status}  turns {}",
        t.id,
        agent_name(t.agent),
        tier_name(t.tier),
        t.turns
    );
    if let Some(name) = &t.name {
        line.push_str(&format!("  name {name}"));
    }
    if let Some(w) = &t.worktree {
        line.push_str(&format!("  worktree {}", w.branch));
    }
    if t.stalled {
        line.push_str("  STALLED");
    }
    if t.detached {
        line.push_str("  DETACHED");
    }
    if !t.changed_files.is_empty() {
        line.push_str(&format!("  files {}", t.changed_files.len()));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_results_keep_the_tail_within_budget() {
        let text = format!("{}END", "中".repeat(20_000));
        let out = result_body(&text, Path::new("x.md"), 1_000);
        assert!(out.starts_with("[truncated"));
        assert!(out.ends_with("END"));
        assert!(tokens(&out) <= 1_000);
    }

    #[test]
    fn short_results_are_untouched() {
        assert_eq!(result_body("done", Path::new("x.md"), 100), "done");
    }
}
