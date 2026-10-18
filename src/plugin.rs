//! `axiopera plugin cleanup`: remove what Codex keeps of the Axiopera plugin after the user
//! uninstalled it: the hooks' trust records in `config.toml`, which would trust the same hooks
//! again on a reinstall without review, and an empty cache folder. Nothing is touched while the
//! plugin is still installed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent::{self, codex_remote::Remote};
use crate::ipc::Agent;

const PLUGIN: &str = "axiopera";
/// Codex keys a plugin hook's trust record `<plugin ID>:<hooks file>:<event>:...`.
const HOOKS_FILE: &str = "codex/hooks.json:";

pub async fn cleanup(home: &Path) -> Result<i32> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| agent::home_dir().join(".codex"));
    let mut done = Vec::new();
    let file = codex_home.join("config.toml");
    let keys = match std::fs::read_to_string(&file) {
        Ok(text) => trust_records(&text).with_context(|| format!("reading {}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if !keys.is_empty() {
        let configured = crate::config::load(home)?
            .agents
            .get(Agent::Codex)
            .path
            .clone();
        let env: Vec<(String, String)> = std::env::vars().collect();
        let exe = agent::resolve_exe(Agent::Codex, configured.as_deref(), &env)?;
        let paths: Vec<String> = keys
            .iter()
            .map(|k| format!("hooks.state.\"{k}\""))
            .collect();
        Remote::private(&exe, Some(&codex_home))
            .await?
            .clear_config(&paths)
            .await?;
        done.push(format!(
            "removed {} hook trust record(s) from {}",
            keys.len(),
            file.display()
        ));
    }
    let cache = codex_home.join("plugins").join("cache").join(PLUGIN);
    let empty = std::fs::read_dir(&cache).is_ok_and(|mut entries| entries.next().is_none());
    if empty {
        std::fs::remove_dir(&cache).with_context(|| format!("removing {}", cache.display()))?;
        done.push(format!("removed the empty folder {}", cache.display()));
    }
    if done.is_empty() {
        done.push("nothing to clean up".to_string());
    }
    println!("{}", done.join("\n"));
    Ok(crate::ipc::exit::OK)
}

/// The keys of our hooks' trust records; an error while the plugin is installed.
fn trust_records(config_toml: &str) -> Result<Vec<String>> {
    let config: toml::Table = toml::from_str(config_toml)?;
    let installed = config
        .get("plugins")
        .and_then(|p| p.as_table())
        .and_then(|p| p.keys().find(|id| is_ours(id)));
    if let Some(id) = installed {
        bail!("the plugin is still installed; remove it first with `codex plugin remove {id}`");
    }
    let states = config
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(|s| s.as_table());
    Ok(states
        .into_iter()
        .flat_map(|s| s.keys())
        .filter(|k| {
            k.split_once(':')
                .is_some_and(|(id, rest)| is_ours(id) && rest.starts_with(HOOKS_FILE))
        })
        .cloned()
        .collect())
}

/// Plugin IDs are `<plugin>@<marketplace>`.
fn is_ours(id: &str) -> bool {
    id.split_once('@').is_some_and(|(name, _)| name == PLUGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_hooks_trust_records_go() {
        let config = r#"
[hooks.state."axiopera@axiopera:codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:a"

[hooks.state."axiopera@fork:codex/hooks.json:post_tool_use:0:0"]
trusted_hash = "sha256:b"

[hooks.state."axiopera-extra@axiopera:codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:c"

[hooks.state."/home/me/.codex/config.toml:stop:0:0"]
trusted_hash = "sha256:d"
"#;
        let mut keys = trust_records(config).unwrap();
        keys.sort();
        assert_eq!(
            keys,
            [
                "axiopera@axiopera:codex/hooks.json:stop:0:0",
                "axiopera@fork:codex/hooks.json:post_tool_use:0:0",
            ]
        );
        let installed = format!("{config}\n[plugins.\"axiopera@axiopera\"]\nenabled = true\n");
        assert!(trust_records(&installed).is_err());
    }
}
