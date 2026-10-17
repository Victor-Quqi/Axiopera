//! `~/.axiopera/config.toml`, all optional. Read when the daemon starts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ipc::Agent;

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Top-level tasks running at once.
    pub concurrency: Option<usize>,
    pub stall_minutes: Option<u64>,
    pub approval_timeout_minutes: Option<u64>,
    pub agents: Agents,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    pub claude: AgentConfig,
    pub codex: AgentConfig,
    pub grok: AgentConfig,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// The agent's executable, instead of looking it up on the caller's PATH.
    pub path: Option<PathBuf>,
    /// Model for tasks spawned without `--model`.
    pub model: Option<String>,
}

impl Agents {
    pub fn get(&self, agent: Agent) -> &AgentConfig {
        match agent {
            Agent::Claude => &self.claude,
            Agent::Codex => &self.codex,
            Agent::Grok => &self.grok,
        }
    }
}

pub fn load(home: &Path) -> Result<Config> {
    let path = home.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("{}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects_typos() {
        let c: Config = toml::from_str(
            "concurrency = 2\n[agents.claude]\nmodel = \"haiku\"\n[agents.grok]\npath = \"/opt/grok\"\n",
        )
        .unwrap();
        assert_eq!(c.concurrency, Some(2));
        assert_eq!(c.agents.get(Agent::Claude).model.as_deref(), Some("haiku"));
        assert!(c.agents.get(Agent::Codex).path.is_none());
        assert!(toml::from_str::<Config>("concurency = 2").is_err());
    }
}
