# Axiopera

[English](README.md)

**更轻量、更高效、更易用，低侵入性的 agent 协作工具**

Axiopera 为 Claude Code、Codex 和 Grok Build 提供跨工具协作。

你可以在 Claude Code 中说：

> 让 Codex 和 Grok 审查这次改动，再根据审查结果修复问题。

优势：

- 一个可执行文件 + 一个 Skill。
- 沿用现有 agent 登录状态和额度。
- 默认不改 agent 配置。
- 平时约一行 Skill 简介，操作说明按需加载。
- 三家子 agent 都支持 Steer，运行中追加要求、调整方向。
- 三家主 agent 都能边工作边[接收子任务消息](docs/backends.md#宿主通知)，及时跟进。

## 安装

先安装并登录要使用的 agent CLI。

**1. 安装程序**

从 [Releases](https://github.com/Victor-Quqi/Axiopera/releases) 下载并解压，将 `axiopera` 或 `axiopera.exe` 放入 PATH。

有 Rust：`cargo install axiopera --locked`。

Windows、Linux 已实测，macOS 待验证。[验证范围](docs/verification.md#已测版本)

**2. 安装 Skill（推荐）**

复制 [`axiopera` 目录](plugins/axiopera/skills/axiopera) 到对应位置，新开会话即可使用：

| Agent | Skill 位置 |
|---|---|
| Claude Code | `~/.claude/skills/axiopera/` |
| Codex | `~/.agents/skills/axiopera/` |
| Grok Build | `~/.grok/skills/axiopera/` |

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
