# Axiopera

[简体中文](README.zh-CN.md)

**Lighter, more efficient, easier agent collaboration with minimal setup changes.**

Axiopera connects Claude Code, Codex, Grok Build, Antigravity CLI, Pi, and OpenCode v2.

In Claude Code, ask:

> Have Codex and Grok review these changes, then fix the issues they find.

Advantages:

- One executable + one skill.
- Uses your existing agent sign-ins and usage allowances.
- Leaves agent configuration unchanged by default.
- About one line for skill discovery; instructions load on demand.
- Add instructions while a sub-agent is working.
- [Receive sub-task messages](docs/backends.md#宿主通知) in your lead agent's session.
- Run `axiopera top` in another terminal to watch every sub-task live without affecting the lead agent.

## Install

Install and sign in to the agent CLIs you want to use.

**1. Install the executable**

Download and extract your platform's archive from [Releases](https://github.com/Victor-Quqi/Axiopera/releases). Put `axiopera` or `axiopera.exe` on your PATH.

You can also install it with `cargo install axiopera --locked`.

**2. Install the skill (recommended)**

```sh
axiopera skill install all
```

Start a new session after installation. To install for just one agent, replace `all` with `claude`, `codex`, `grok`, `agy` (Antigravity CLI), `pi`, or `opencode`.

**These sub-task notifications require a plugin**

<details>
<summary>Mid-turn updates in Codex desktop and VS Code</summary>

Both clients normally notify Codex after its turn ends. With the plugin, Codex can act on sub-task updates after its next tool call returns. The regular Codex terminal already receives mid-turn updates.

The plugin includes the skill. Run these commands, trust the plugin's hooks in Codex, then start a new session.

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

[Managing and removing the plugin](docs/usage.md#skill-与-codex-插件)

</details>

<details>
<summary>Waking an idle Pi terminal</summary>

When a task you delegated finishes, an idle Pi normally waits for your next input. The extension lets results and questions wake Pi. If Pi is still working, the update arrives after a tool call returns.

```sh
pi install git:github.com/Victor-Quqi/Axiopera
```

Enter `/reload` in any open Pi session to activate the extension. [Updates and removal](docs/usage.md#pi-扩展)

</details>

## Documentation

[Usage and configuration](docs/usage.md) · [Permissions and messaging](docs/backends.md) · [Development](docs/development.md). Currently in Chinese.

## Friends

[linux.do](https://linux.do)

## License

[Apache-2.0](LICENSE).
