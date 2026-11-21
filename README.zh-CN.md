# Axiopera

[English](README.md)

**更轻量、更高效、更易用，低侵入性的 agent 协作工具**

Axiopera 支持 Claude Code、Codex、Grok Build、Antigravity CLI、Pi 和 OpenCode v2 互相派发任务。

你可以在 Claude Code 中说：

> 让 Codex 和 Grok 审查这次改动，再根据审查结果修复问题。

优势：

- 一个可执行文件 + 一个 Skill。
- 沿用现有 agent 登录状态和额度。
- 默认不改 agent 配置。
- 平时约一行 Skill 简介，操作说明按需加载。
- 子 agent 运行中可以追加要求、调整方向。
- 在主 agent 会话中[接收子任务消息](docs/backends.md#宿主通知)。
- 另开终端运行 `axiopera top`，实时查看全部子任务，不影响主 agent。

## 安装

先安装并登录要使用的 agent CLI。

**1. 安装程序**

从 [Releases](https://github.com/Victor-Quqi/Axiopera/releases) 下载并解压，将 `axiopera` 或 `axiopera.exe` 放入 PATH。

有 Rust：`cargo install axiopera --locked`。

Windows、Linux 已实测，macOS 待验证。[验证范围](docs/verification.md#已测版本)

**2. 安装 Skill（推荐）**

按主 agent 执行对应命令，新开会话即可使用：

| Agent | 命令 |
|---|---|
| Claude Code | `axiopera skill install claude` |
| Codex | `axiopera skill install codex` |
| Grok Build | `axiopera skill install grok` |
| Antigravity CLI | `axiopera skill install agy` |
| Pi | `axiopera skill install pi` |
| OpenCode v2 | `axiopera skill install opencode` |

为所有支持的 Harness 安装：`axiopera skill install all`。

Skill 内置于程序，可离线安装。[安装位置、覆盖与卸载](docs/usage.md#skill-与-codex-插件)。

用 `axiopera spawn pi --tier full "…"` 派发 Pi。OpenCode v2 可选 `--tier read` 或 `--tier full`。Pi TUI 需要自动收件时，运行 `pi install git:github.com/Victor-Quqi/Axiopera`，在已打开的会话中输入 `/reload`。[更新与卸载](docs/usage.md#pi-扩展)

<details>
<summary>Codex 桌面端 / VS Code：中途提醒插件</summary>

**只用 Codex TUI，安装 Skill 即可，中途消息由 Axiopera 直接推送。**

桌面端和 VS Code 默认等本轮结束才收到提醒。插件通过 hook，在下一次工具调用后送入提醒。

插件已包含 Skill。通过 CLI 安装并信任 hook，再新开会话：

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

</details>

## 文档

[使用与配置](docs/usage.md) · [权限与消息](docs/backends.md) · [开发](docs/development.md)

## 友链

[linux.do](https://linux.do)

## 许可证

[Apache-2.0](LICENSE)。
