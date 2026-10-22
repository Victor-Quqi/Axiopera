# Axiopera

[简体中文](README.zh-CN.md)

Lightweight, minimally invasive collaboration for coding agents.

Bring Claude Code, Codex, and Grok Build together with a single executable. Keep your existing tools and sign-ins, and start delegating tasks with a few commands.

Ask Codex to review Claude's changes, split a task across models, or leave a longer job running after your session ends.

- Send new instructions while a task is running.
- Let agents ask questions and exchange notes.
- Give each task its own model and permissions.
- Continue a task in its original agent CLI when you want to take over.

Each agent starts with the task description you provide. Include the context it needs.

## Install

Install and sign in to the agent CLIs you want to use. Axiopera uses those existing accounts.

Download the archive for your system from [GitHub Releases](https://github.com/Victor-Quqi/Axiopera/releases), extract it, and put `axiopera` or `axiopera.exe` in a directory on your PATH.

Or install from [crates.io](https://crates.io/crates/axiopera) with Rust:

```sh
cargo install axiopera --locked
axiopera --version
```

Windows and Linux have been tested; macOS still needs validation. See the [tested versions](docs/verification.md#已测版本).

## Get started

Give your coding agent the Axiopera skill, so it knows how to delegate: copy the [`plugins/axiopera/skills/axiopera`](plugins/axiopera/skills/axiopera) folder into `~/.claude/skills/` for Claude Code, or into `~/.agents/skills/` for Codex. Grok reads both.

In Codex you can install the plugin instead. Besides the skill, it lets a busy session know when its sub-tasks have news:

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

The [usage guide](docs/usage.md#skill-与-codex-插件) covers the hooks and uninstalling.

Or add this to your agent's project instructions instead:

> When delegating tasks through Axiopera, first run `axiopera guide --host <your harness: claude, codex or grok>`.

Or try a task from your terminal:

```sh
axiopera spawn codex --tier read --name review "Review this repository and report the issues most worth fixing."
axiopera wait review
axiopera inbox review
```

The task starts in the background. `wait` blocks until it has news, `inbox` takes its answer, and `send` lets you follow up in the same conversation.

See the [agent guide](src/guide.md) for more examples, or run `axiopera --help`. The [usage and configuration guide](docs/usage.md) is currently in Chinese.

## License

[Apache-2.0](LICENSE).
