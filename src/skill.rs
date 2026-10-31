use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::ipc::Agent;

const CONTENT: &[u8] = include_bytes!("../plugins/axiopera/skills/axiopera/SKILL.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    Claude,
    Codex,
    Grok,
    All,
}

pub fn install(host: Host, force: bool, json: bool) -> i32 {
    let home = crate::agent::home_dir();
    install_into(&home, host, force, json)
}

fn install_into(home: &Path, host: Host, force: bool, json: bool) -> i32 {
    let hosts: &[Agent] = match host {
        Host::Claude => &[Agent::Claude],
        Host::Codex => &[Agent::Codex],
        Host::Grok => &[Agent::Grok],
        Host::All => &[Agent::Claude, Agent::Codex, Agent::Grok],
    };
    let mut results = Vec::new();
    let mut code = crate::ipc::exit::OK;
    let mut installed = false;
    for &agent in hosts {
        let result = match install_at(home, agent, force) {
            Ok((path, changed)) => {
                installed = true;
                if !json {
                    let status = if changed {
                        "Installed"
                    } else {
                        "Already installed"
                    };
                    println!("{status}: {}", path.display());
                }
                serde_json::json!({"host": agent, "path": path, "changed": changed})
            }
            Err(error) => {
                code = crate::ipc::exit::ERROR;
                if !json {
                    eprintln!("axiopera: {error:#}");
                }
                serde_json::json!({"host": agent, "error": format!("{error:#}")})
            }
        };
        results.push(result);
    }
    if json {
        let output = if host == Host::All {
            serde_json::Value::Array(results)
        } else {
            results.remove(0)
        };
        println!("{output}");
    } else if installed {
        println!("Start a new agent session to use the skill.");
    }
    code
}

fn install_at(home: &Path, host: Agent, force: bool) -> Result<(PathBuf, bool)> {
    if !home.is_absolute() {
        bail!("cannot locate an absolute user home directory");
    }
    let root = match host {
        Agent::Claude => ".claude",
        Agent::Codex => ".agents",
        Agent::Grok => ".grok",
    };
    let dir = home.join(root).join("skills").join("axiopera");
    let path = dir.join("SKILL.md");
    let exists = match fs::read(&path) {
        Ok(contents) if contents == CONTENT => return Ok((path, false)),
        Ok(_) if !force => bail!(
            "{} already has different contents; run again with --force to replace it",
            path.display()
        ),
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    OpenOptions::new()
        .write(true)
        .create_new(!exists)
        .truncate(exists)
        .open(&path)
        .and_then(|mut file| file.write_all(CONTENT))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok((path, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Home(PathBuf);

    impl Home {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("axiopera skill {}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn installs_once_and_requires_force_to_replace_changed_contents() {
        let home = Home::new();
        for (host, root) in [
            (Agent::Claude, ".claude"),
            (Agent::Codex, ".agents"),
            (Agent::Grok, ".grok"),
        ] {
            let expected = home.0.join(root).join("skills/axiopera/SKILL.md");
            let (path, changed) = install_at(&home.0, host, false).unwrap();
            assert_eq!(path, expected);
            assert!(changed);
            assert_eq!(fs::read(&path).unwrap(), CONTENT);
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            assert!(!install_at(&home.0, host, false).unwrap().1);
            assert!(!install_at(&home.0, host, true).unwrap().1);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

            let extra = path.with_file_name("notes.md");
            fs::write(&extra, "keep").unwrap();
            fs::write(&path, "custom skill").unwrap();
            let error = install_at(&home.0, host, false).unwrap_err();
            assert!(error.to_string().contains("--force"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "custom skill");
            assert!(install_at(&home.0, host, true).unwrap().1);
            assert_eq!(fs::read(&path).unwrap(), CONTENT);
            assert_eq!(fs::read_to_string(&extra).unwrap(), "keep");
        }
    }

    #[test]
    fn all_continues_after_conflicts_and_force_replaces_them() {
        let home = Home::new();
        let conflict = home.0.join(".claude/skills/axiopera/SKILL.md");
        fs::create_dir_all(conflict.parent().unwrap()).unwrap();
        fs::write(&conflict, "custom skill").unwrap();

        assert_eq!(
            install_into(&home.0, Host::All, false, true),
            crate::ipc::exit::ERROR
        );
        assert_eq!(fs::read_to_string(&conflict).unwrap(), "custom skill");
        for root in [".agents", ".grok"] {
            assert_eq!(
                fs::read(home.0.join(root).join("skills/axiopera/SKILL.md")).unwrap(),
                CONTENT
            );
        }
        assert_eq!(
            install_into(&home.0, Host::All, true, false),
            crate::ipc::exit::OK
        );
        assert_eq!(fs::read(&conflict).unwrap(), CONTENT);
        assert_eq!(
            install_into(&home.0, Host::All, false, true),
            crate::ipc::exit::OK
        );
    }

    #[test]
    fn invalid_destinations_fail_without_writing() {
        assert!(install_at(Path::new(""), Agent::Codex, false).is_err());
        assert!(install_at(Path::new("relative"), Agent::Codex, true).is_err());
        let home = Home::new();
        let path = home.0.join(".agents/skills/axiopera/SKILL.md");
        fs::create_dir_all(&path).unwrap();
        for force in [false, true] {
            assert!(install_at(&home.0, Agent::Codex, force).is_err());
            assert!(path.is_dir());
        }
    }
}
