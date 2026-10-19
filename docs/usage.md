# 使用与配置

派发、收结果和对话的示例见 [agent 使用说明](../src/guide.md)。所有命令支持全局选项 `--json`；各命令的参数见 `axiopera <命令> --help`。

## 任务与工作目录

默认情况下，子 agent 使用调用方的工作目录。`spawn --worktree` 从当前仓库的 HEAD 创建分支 `axiopera/<别名或任务 ID>`，并在 Axiopera 的数据目录下检出。当前工作区未提交的改动不会带入。调用方在仓库子目录时，子 agent 使用新 worktree 中对应的子目录。

主 agent 负责检查子 agent 的改动、提交和合并分支。子 agent 的权限由 `--tier` 指定，默认 `work`。各档位在不同 backend 上的具体行为见[权限](backends.md#权限)。

任务 ID 全局唯一，`--name` 指定的别名在同一个父节点下唯一。主 agent 为第 0 层，最多派发到第 2 层。子任务的档位不能高于父任务。

子 agent 用 `ask` 提问后可以继续工作。若这一轮结束时仍有未回答的问题，任务进入 `waiting`；回答会在原会话中开启下一轮。兄弟任务通过 `tell` 留言，父节点收到抄送。接收方空闲时，留言等到下一轮开始才送入，不会单独开启一轮。

`spawn`、`send`、`reply` 接受位置参数、`-f FILE` 或单独的 `-`。后两种分别读取 UTF-8 文件和标准输入，保留原始换行。三种来源只能选一种，内容不能为空。

## 结果与接管

`wait` 阻塞到有新消息，列出后退出，不取走任何内容；`inbox` 取走消息，从不阻塞。`watch` 持续输出事件摘要，同样不取走。每轮结果经 `inbox`、`result` 或宿主推送交付后，再次 `result` 只返回文件路径。需要重新查看正文时用 `result <task> --turn N --force`。

回复长度有上限。一次回复放不下的消息整条留到下一次 `inbox`，回复末尾注明还剩几条。单条结果超长时只保留末尾，并附完整文件路径。`status` 查看任务状态，运行中的任务附带进度：第几轮、本轮已用时间、工具调用次数，以及最近一次调用和距今时间；一轮结束后进程仍在等后台工作时，注明上一轮结束了多久；没有新消息时，`inbox` 和 `wait` 超时也列出这些进度。`log` 查看事件摘要。加上 `--json` 可以读取完整任务字段和事件字段，包括改动文件列表及后端上报的用量。

`detach <task>` 将任务交给人接管，并打印工作目录和原生 resume 命令。接管前要等这一轮结束，或先 `cancel`。Axiopera 发现原生会话文件被其他进程修改时，也会将任务标为 detached。

`cancel` 取消整个任务子树。`failed` 和 `interrupted` 任务可以用 `send` 继续；已取消或脱管的任务通过原生 CLI 接管。

退出码见 [guide](../src/guide.md)。带消息的输出总以 0 退出：宿主对非零退出的输出截得更短，Windows 上的 Codex 还把所有非零退出码报成 1。

## 配置

数据目录默认为 `~/.axiopera/`，环境变量 `AXIOPERA_HOME` 可以覆盖它。配置文件是该目录下的 `config.toml`，所有字段均可省略。以下三项是默认值：

```toml
concurrency = 4
stall_minutes = 10
approval_timeout_minutes = 2
```

`concurrency` 只计算顶层任务，嵌套任务共用父任务的名额。超过上限的任务排队。持续没有事件达到 `stall_minutes` 时，任务标为 stalled 并通知调用方；任务继续运行。`spawn --timeout` 才会在到期时取消任务。

每个 backend 可以设置默认模型和可执行文件路径。例如：

```toml
[agents.claude]
model = "haiku"
# path = "/absolute/path/to/claude"
```

同样支持 `[agents.codex]`、`[agents.grok]`。`--model` 覆盖默认模型。`path` 必须指向真实可执行文件；Windows 上应使用 `.exe`。省略时按[后端的路径查找规则](backends.md#可执行文件与配置继承)定位。

daemon 启动时读取配置。修改后运行 `axiopera daemon stop`，下一次调用会重新启动。未知配置项会报错。

## Skill 与 Codex 插件

skill 让 agent 在派发任务前先运行 `axiopera guide`，效果与在项目说明中写明这一点相同。skill 就是仓库中的 [`plugins/axiopera/skills/axiopera/`](../plugins/axiopera/skills/axiopera) 目录，复制到 agent 的 skills 目录即可，删除该目录即卸载：

| agent | 位置 |
|---|---|
| Claude Code | `~/.claude/skills/axiopera/` |
| Codex | `~/.agents/skills/axiopera/` |
| Grok | 默认也读取上面两个位置；只用 Grok 时放到 `~/.grok/skills/axiopera/` |

每个会话的上下文都会列出 skill 的名称和描述，约占一行。

Codex 插件包含同一个 skill，另带 hook，让宿主在一轮进行中也能得知子任务的消息。装了插件就不必再复制 skill。插件不包含 `axiopera` 程序。

- 安装：`codex plugin marketplace add Victor-Quqi/Axiopera`，再 `codex plugin add axiopera@axiopera`。
- 卸载：`codex plugin remove axiopera@axiopera`，再 `codex plugin marketplace remove axiopera`。

hook 要在桌面端的插件页或 TUI 的 `/hooks` 中信任后才运行，插件更新后需重新信任。VS Code 扩展官方不支持插件。hook 的作用：

- 工具调用之后，如果有新的结果、提问或审批请求，提醒宿主运行 `inbox`。
- 一轮即将结束时，如果仍有未取的内容，让这一轮继续一次，取完再结束。

装了插件后，Codex 每个会话都会启动一个 `axiopera mcp --codex-hooks` 进程。hook 只查询已在运行的 daemon，不会启动它；没有相关任务时立即返回。实测进程启动不到 0.1 秒，每次 hook 调用不到 1 毫秒。删除 `axiopera` 程序前先卸载插件，否则 Codex 无法启动这个进程。

Codex 卸载插件后会留下以下内容，运行 `axiopera plugin cleanup` 清除：

- `config.toml` 中 hook 的信任记录 `[hooks.state."axiopera@<来源>:codex/hooks.json:…"]`。如果保留，重装定义相同的插件时会直接沿用旧的信任。
- 空目录 `plugins/cache/axiopera`。

数据目录取 `CODEX_HOME`，未设置时为 `~/.codex`。插件仍装着时，清理命令不做任何处理。配置通过 Codex 自己的配置接口修改。

## daemon 与清理

CLI 按需启动 daemon。没有运行中的 agent 和客户端连接时，daemon 空闲 10 分钟后退出。主会话退出不会取消已派发的任务。

`axiopera daemon status` 查看 daemon，`daemon stop` 在没有运行任务时停止它。`daemon stop --force` 会中断正在运行的任务，之后可用 `send` 续接。升级二进制前先停止 daemon。

`axiopera gc --days 7` 清理至少 7 天未活动且已停止的任务。存在未提交改动的 worktree 会连同任务保留，分支仅在已合并时删除。仍有子任务的父任务也会保留。

卸载前先处理需要保留的 worktree 改动，运行 `gc --days 0` 清理可删除的任务，再停止 daemon。通过 Cargo 安装的执行 `cargo uninstall axiopera`；通过 Release 压缩包安装的删除可执行文件。数据目录按需删除，原生 agent 的会话仍由各自 CLI 管理。
