//! Git worktrees, for sub-agents that should not share the caller's working tree.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A worktree made for a task.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Worktree {
    /// Top of the caller's repository.
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// The commit it started from, abbreviated.
    pub base: String,
}

/// Runs git with the caller's environment, which is where its PATH and identity come from.
fn git(dir: &Path, env: &[(String, String)], args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Make `<home>/worktrees/<repo>-<hash>/<id>` on a new branch `axiopera/<name>` from HEAD of the
/// repository holding `cwd`. Also returns the directory in it that corresponds to `cwd`.
pub fn create(
    home: &Path,
    cwd: &Path,
    id: &str,
    name: &str,
    env: &[(String, String)],
) -> Result<(Worktree, PathBuf)> {
    let repo = PathBuf::from(
        git(cwd, env, &["rev-parse", "--show-toplevel"])
            .context("--worktree needs a git repository")?,
    );
    let prefix = git(cwd, env, &["rev-parse", "--show-prefix"])?;
    let base = git(cwd, env, &["rev-parse", "--short", "HEAD"])
        .context("the repository has no commit to start the worktree from")?;
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let hash = crate::paths::fnv1a(repo.to_string_lossy().to_lowercase().as_bytes()) as u32;
    let path = home
        .join("worktrees")
        .join(format!("{repo_name}-{hash:08x}"))
        .join(id);
    let branch = format!("axiopera/{name}");
    let path_arg = path.to_string_lossy();
    if let Err(e) = git(
        &repo,
        env,
        &["worktree", "add", "-b", &branch, &path_arg, "HEAD"],
    ) {
        // A failing post-checkout hook leaves both behind. The path is new to this task, so if
        // it exists, the branch was created here too.
        if path.exists() {
            let _ = git(&repo, env, &["worktree", "remove", "--force", &path_arg]);
            let _ = git(&repo, env, &["branch", "-D", &branch]);
        }
        return Err(e.context("creating the worktree"));
    }
    let cwd = path.join(prefix);
    Ok((
        Worktree {
            repo,
            path,
            branch,
            base,
        },
        cwd,
    ))
}

/// Remove a worktree, but only if nothing in it is uncommitted; then its branch, if merged.
/// Returns a note on what was kept.
pub fn remove(wt: &Worktree, env: &[(String, String)]) -> Result<Option<String>> {
    if wt.path.exists() {
        git(
            &wt.repo,
            env,
            &["worktree", "remove", &wt.path.to_string_lossy()],
        )?;
    }
    Ok(git(&wt.repo, env, &["branch", "-d", &wt.branch])
        .err()
        .map(|_| format!("branch {} kept: not merged", wt.branch)))
}
