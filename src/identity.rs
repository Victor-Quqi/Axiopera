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
/// credentials of a Claude Code session's inbox.
pub const STRIP_VARS: [&str; 10] = [
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "GROK_SESSION_ID",
    "GROK_AGENT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Caller key: `task:<id>`, `<host>:<session>`, or `user`.
///
/// Hosts pass their session variable down to everything they start, so a Codex launched from a
/// Claude Code shell sees both. Then the nearest host among `ancestors` (executable names of the
/// parent processes, nearest first) decides.
pub fn detect(
    get: impl Fn(&str) -> Option<String>,
    ancestors: impl FnOnce() -> Vec<String>,
) -> String {
    if let Some(task) = get(TASK_VAR).filter(|v| !v.is_empty()) {
        return format!("task:{task}");
    }
    let present: Vec<(&str, String)> = HOST_VARS
        .iter()
        .filter_map(|(var, host)| get(var).filter(|v| !v.is_empty()).map(|id| (*host, id)))
        .collect();
    let chosen = match present.as_slice() {
        [] => return "user".into(),
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
    format!("{}:{}", chosen.0, chosen.1)
}

pub fn task_of(caller: &str) -> Option<&str> {
    caller.strip_prefix("task:")
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
    fn task_wins_over_host_session() {
        let e = env(&[
            ("AXIOPERA_TASK_ID", "tab12"),
            ("CLAUDE_CODE_SESSION_ID", "s1"),
        ]);
        assert_eq!(detect(e, no_ancestors), "task:tab12");
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
