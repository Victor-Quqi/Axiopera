# Axiopera

[简体中文](README.zh-CN.md)

**Lighter, more efficient, easier agent collaboration with minimal setup changes.**

Axiopera connects Claude Code, Codex, and Grok Build.

In Claude Code, ask:

> Have Codex and Grok review these changes, then fix the issues they find.

Advantages:

- One executable + one skill.
- Uses your existing agent sign-ins and usage allowances.
- Leaves agent configuration unchanged by default.
- About one line for skill discovery; instructions load on demand.
- All three sub-agents support Steer: add instructions and change direction while they run.
- All three lead agents can [receive sub-task messages](docs/backends.md#宿主通知) while working and follow up.

## Install

Install and sign in to the agent CLIs you want to use.

**1. Install the executable**

Download and extract your platform's archive from [Releases](https://github.com/Victor-Quqi/Axiopera/releases). Put `axiopera` or `axiopera.exe` on your PATH.

With Rust: `cargo install axiopera --locked`.

Windows and Linux tested; macOS awaits validation. [Test coverage](docs/verification.md#已测版本)

**2. Install the skill (recommended)**

Run the command for your host agent, then start a new session:

| Agent | Command |
|---|---|
| Claude Code | `axiopera skill install claude` |
| Codex | `axiopera skill install codex` |
| Grok Build | `axiopera skill install grok` |

For all supported harnesses: `axiopera skill install all`.

The skill is bundled with the executable and installs offline. [Locations, replacement, and removal](docs/usage.md#skill-与-codex-插件).

<details>
<summary>Codex desktop / VS Code: plugin for mid-turn notifications</summary>

**For Codex TUI, the skill is enough. Axiopera pushes messages into the active turn directly.**

Desktop and VS Code normally receive notifications after the current turn ends. The plugin delivers them after the next tool call through a hook.

The plugin includes the skill. Install through the CLI, trust its hooks, then start a new session:

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

</details>

## Documentation

[Usage and configuration](docs/usage.md) · [Permissions and messaging](docs/backends.md) · [Development](docs/development.md). Currently in Chinese.

## Friends

[linux.do](https://linux.do)

## License

[Apache-2.0](LICENSE).
