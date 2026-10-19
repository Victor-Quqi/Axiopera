# Axiopera

[English](README.md)

轻量、低侵入的编程 agent 协作工具。

一个可执行文件，让 Claude Code、Codex 和 Grok Build 协同工作。沿用已有工具和登录状态，几条命令即可派发任务。

可以让 Codex 审查 Claude 的改动，把任务分给不同模型，或让长任务在主会话结束后继续运行。

- 在任务运行中追加要求。
- 让 agent 提问、互相留言。
- 为每个任务选择模型和权限，也可以使用独立的工作目录。
- 需要亲自接手时，通过原来的 agent CLI 继续会话。

每个 agent 从你提供的任务描述开始工作，请在描述中交代所需上下文。

## 安装

先安装并登录要使用的 agent CLI，Axiopera 使用已有账号。

从 [GitHub Releases](https://github.com/Victor-Quqi/Axiopera/releases) 下载对应系统的压缩包，解压后把 `axiopera` 或 `axiopera.exe` 放到 PATH 中的目录。

也可以安装 Rust 后，从 [crates.io](https://crates.io/crates/axiopera) 安装：

```sh
cargo install axiopera --locked
axiopera --version
```

独立工作目录功能需要 Git。Windows 和 Linux 已有实测，macOS 待验证，详见[已测版本](docs/verification.md#已测版本)。

## 开始使用

给编程 agent 装上 Axiopera 的 skill，让它知道如何派发任务：把 [`plugins/axiopera/skills/axiopera`](plugins/axiopera/skills/axiopera) 目录复制到 `~/.claude/skills/`（Claude Code）或 `~/.agents/skills/`（Codex）。Grok 两处都会读取。

Codex 也可以改装插件。除了 skill，插件还能让正在工作的会话得知子任务的消息：

```sh
codex plugin marketplace add Victor-Quqi/Axiopera
codex plugin add axiopera@axiopera
```

hook 和卸载方法见[使用与配置](docs/usage.md#skill-与-codex-插件)。

也可以不装 skill，在 agent 的项目说明中加入：

> 通过 Axiopera 派发子任务前，先运行 `axiopera guide`。

也可以在终端尝试：

```sh
axiopera spawn codex --tier read --name review "审查当前仓库，报告最需要修复的问题。"
axiopera wait review
axiopera inbox review
```

任务在后台运行。`wait` 等到它有新消息，`inbox` 取回结果，`send` 可以在同一会话中继续追问。

更多示例见 [agent 使用说明](src/guide.md)，命令帮助用 `axiopera --help` 查看。工作目录、接管、清理等操作见[使用与配置](docs/usage.md)。

## 许可证

[Apache-2.0](LICENSE)。
