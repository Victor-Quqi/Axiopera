//! Who is calling: a sub-agent task, a host agent session, or a person at a terminal.

/// Set by the daemon in every sub-agent's environment.
pub const TASK_VAR: &str = "AXIOPERA_TASK_ID";

/// Session variables hosts inject into their shell commands, and the caller prefix for each.
pub const HOST_VARS: [(&str, &str); 3] = [
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_THREAD_ID", "codex"),
    ("GROK_SESSION_ID", "grok"),
];

/// Host identity variables a sub-agent must not inherit from its caller, including the
/// credentials of a Claude Code session's inbox, and the client name the Codex app and IDE
/// extension give their sessions.
pub const STRIP_VARS: [&str; 11] = [
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "GROK_SESSION_ID",
    "GROK_AGENT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Caller key: `task:<id>/<host>:<session>`, `task:<id>`, `<host>:<session>`, or `user`.
///
/// Hosts pass their session variable down to everything they start, so a Codex launched from a
/// Claude Code shell sees both. Then the nearest host among `ancestors` (executable names of the
/// parent processes, nearest first) decides. Keep both task and native identity: a native child
/// inherits the task but owns a separate inbox. The daemon folds the task's own session back
/// to `task:<id>` so shell calls and its MCP server keep the same identity.
pub fn detect(
    get: impl Fn(&str) -> Option<String>,
    ancestors: impl FnOnce() -> Vec<String>,
) -> String {
    let task = get(TASK_VAR).filter(|v| !v.is_empty());
    let present: Vec<(&str, String)> = HOST_VARS
        .iter()
        .filter_map(|(var, host)| get(var).filter(|v| !v.is_empty()).map(|id| (*host, id)))
        .collect();
    let chosen = match present.as_slice() {
        [] => return task.map_or_else(|| "user".into(), |id| format!("task:{id}")),
        [only] => only,
        _ => ancestors()
            .iter()
            .find_map(|name| {
                present
                    .iter()
                    .find(|(host, _)| name.to_lowercase().starts_with(host))
            })
            .unwrap_or(&present[0]),
    };
    let host = format!("{}:{}", chosen.0, chosen.1);
    match task {
        Some(id) => format!("task:{id}/{host}"),
        None => host,
    }
}

/// Owning Axiopera task, including callers in its native child sessions.
pub fn task_of(caller: &str) -> Option<&str> {
    caller
        .strip_prefix("task:")
        .map(|s| s.split('/').next().unwrap())
}

/// Native host identity supplied alongside an owning task.
pub fn task_session(caller: &str) -> Option<(&str, &str)> {
    caller.strip_prefix("task:")?.split_once('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn no_ancestors() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn nearest_host_wins_when_nested() {
        let e = env(&[("CODEX_THREAD_ID", "c1"), ("CLAUDE_CODE_SESSION_ID", "s1")]);
        let chain = || {
            vec![
                "pwsh.exe".to_string(),
                "codex.exe".into(),
                "bash.exe".into(),
                "claude.exe".into(),
            ]
        };
        assert_eq!(detect(e, chain), "codex:c1");
    }

    #[test]
    fn task_retains_the_native_session() {
        let e = env(&[
            ("AXIOPERA_TASK_ID", "tab12"),
            ("CLAUDE_CODE_SESSION_ID", "s1"),
        ]);
        assert_eq!(detect(e, no_ancestors), "task:tab12/claude:s1");
    }

    #[test]
    fn native_children_keep_their_own_identity_and_task_ancestry() {
        for (var, host) in [("CODEX_THREAD_ID", "codex"), ("GROK_SESSION_ID", "grok")] {
            let parent = detect(env(&[(TASK_VAR, "t1"), (var, "parent")]), no_ancestors);
            let child = detect(env(&[(TASK_VAR, "t1"), (var, "child")]), no_ancestors);
            assert_ne!(parent, child);
            assert_eq!(task_of(&parent), Some("t1"));
            assert_eq!(task_of(&child), Some("t1"));
            assert_eq!(
                task_session(&child),
                Some(("t1", format!("{host}:child").as_str()))
            );
        }
        assert_eq!(detect(env(&[(TASK_VAR, "t1")]), no_ancestors), "task:t1");
        assert_eq!(task_of("task:t1"), Some("t1"));
        assert_eq!(task_session("task:t1"), None);
        assert_eq!(task_of("codex:child"), None);
    }

    #[test]
    fn first_host_variable_wins_without_ancestry() {
        let e = env(&[("CODEX_THREAD_ID", "c1"), ("CLAUDE_CODE_SESSION_ID", "s1")]);
        assert_eq!(detect(e, no_ancestors), "claude:s1");
    }

    #[test]
    fn nothing_means_user() {
        assert_eq!(
            detect(env(&[("CODEX_THREAD_ID", "")]), no_ancestors),
            "user"
        );
    }
}
