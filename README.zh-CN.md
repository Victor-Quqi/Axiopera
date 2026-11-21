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

也可以运行 `cargo install axiopera --locked` 安装。

**2. 安装 Skill（推荐）**

```sh
axiopera skill install all
```

装好后新开会话即可使用。只给某个 agent 安装时，把 `all` 换成 `claude`、`codex`、`grok`、`agy`（Antigravity CLI）、`pi` 或 `opencode`。

**以下两种子任务提醒需要插件**

<details>
<summary>Codex 桌面端 / VS Code 的轮中提醒</summary>

这两个客户端默认在本轮结束后才提醒 Codex。装插件后，下一次工具调用返回时就能处理子任务的消息。普通 Codex 终端已支持轮中提醒。

插件自带 Skill。在终端运行以下命令，然后在 Codex 中信任插件的 hook，再新开会话。

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

[插件管理与卸载](docs/usage.md#skill-与-codex-插件)

</details>

<details>
<summary>唤醒空闲的 Pi 终端</summary>

即使派出的任务已经完成，空闲的 Pi 也会等你再次输入。装扩展后，结果或提问会自动唤醒 Pi。如果 Pi 还在工作，提醒会在工具返回后送达。

```sh
pi install git:github.com/Victor-Quqi/Axiopera
```

在已打开的 Pi 会话中输入 `/reload` 即可生效。[更新与卸载](docs/usage.md#pi-扩展)

</details>

## 文档

[使用与配置](docs/usage.md) · [权限与消息](docs/backends.md) · [开发](docs/development.md)

## 友链

[linux.do](https://linux.do)

## 许可证

[Apache-2.0](LICENSE)。
