# 使用与配置

派发、收结果和对话的示例见 [agent 使用说明](../src/guide.md)。所有命令支持全局选项 `--json`；各命令的参数见 `axiopera <命令> --help`。

## 任务与工作目录

子 agent 在调用方的工作目录中运行，可用 `--cwd` 另行指定。Axiopera 不为任务建立独立的工作副本：同时修改同一仓库的任务，由主 agent 各建一个 Git worktree，再用 `--cwd` 指定。

主 agent 负责检查子 agent 的改动、提交和合并分支。子 agent 的权限由 `--tier` 指定，默认 `work`。各档位在不同 backend 上的具体行为见[权限](backends.md#权限)。

任务 ID 全局唯一，`--name` 指定的别名在同一个调用方下唯一。主 agent 为第 0 层，最多派发到第 2 层，只统计 Axiopera 任务。宿主内置子 agent 归属于所在的 Axiopera 任务，它再派发的任务继续受该任务树的深度和权限限制。子任务的档位不能高于父任务。内置子 agent 的收件隔离见[后端文档](backends.md#会话与消息)。

子 agent 用 `ask` 提问后可以继续工作。若这一轮结束时仍有未回答的问题，任务进入 `waiting`；回答会在原会话中开启下一轮。兄弟任务通过 `tell` 留言，父节点收到抄送。接收方空闲时，留言等到下一轮开始才送入，不会单独开启一轮。

`spawn`、`send`、`reply` 接受位置参数、`-f FILE` 或单独的 `-`。后两种分别读取 UTF-8 文件和标准输入，保留原始换行。三种来源只能选一种，内容不能为空。

## 结果与接管

`wait` 阻塞到有新消息，列出后退出，不取走任何内容；`inbox` 取走消息，从不阻塞。`watch` 每有新消息就输出一行摘要，一直运行到被停止，同样不取走；子 agent 的 `watch` 在子任务都结束、消息都取走 1 分钟后自行退出。每轮结果经 `inbox`、`result` 或宿主推送交付后，再次 `result` 只返回文件路径。需要重新查看正文时用 `result <task> --turn N --force`。

回复长度有上限。等你裁决的审批请求总是先给，其余消息按先后取，一次回复放不下的整条留到下一次 `inbox`，回复末尾注明还剩几条。单条结果超长时只保留末尾，并注明显示的字符数、总字符数和完整文件路径；过长的提问、留言和审批请求给开头和结尾，并注明全文路径。`status` 查看任务状态，运行中的任务附带进度：第几轮、本轮已用时间、工具调用次数，以及最近一次调用和距今时间；一轮结束后进程仍在等后台工作时，注明上一轮结束了多久；没有新消息时，`inbox` 和 `wait` 超时也列出这些进度。`log` 查看事件摘要，每个事件一行，换行显示为 `↵`，命令等自由文本截到 200 个字符并注明省略了多少。加上 `--json` 可以读取完整任务字段和事件字段，包括改动文件列表及后端上报的用量。

`detach <task>` 将任务交给人接管，并打印工作目录和原生 resume 命令。接管前要等这一轮结束，或先 `cancel`。Axiopera 发现原生会话文件被其他进程修改时，也会将任务标为 detached。

`cancel` 取消整个任务子树。`failed` 和 `interrupted` 任务可以用 `send` 继续；已取消或脱管的任务通过原生 CLI 接管。

退出码见 [guide](../src/guide.md)。带消息的输出总以 0 退出：宿主对非零退出的输出截得更短，Windows 上的 Codex 还把所有非零退出码报成 1。

## 配置

数据目录默认为 `~/.axiopera/`，环境变量 `AXIOPERA_HOME` 可以覆盖它。配置文件是该目录下的 `config.toml`，所有字段均可省略。以下三项是默认值：

```toml
concurrency = 50
stall_minutes = 10
approval_timeout_minutes = 2
```

`concurrency` 是同时运行的顶层任务上限，所有会话共用，嵌套任务不计入。它用于防止失控派发：达到上限时新的 `spawn` 直接报错，不排队；已有任务经 `send` 或 `reply` 开始的新一轮不受限制。持续没有事件达到 `stall_minutes` 时，任务标为 stalled 并通知调用方；任务继续运行。`spawn --timeout` 才会在到期时取消任务。

每个 backend 可以设置默认模型和可执行文件路径。例如：

```toml
[agents.claude]
model = "haiku"
# path = "/absolute/path/to/claude"
```

同样支持 `[agents.codex]`、`[agents.grok]`。`--model` 覆盖默认模型。`path` 必须指向真实可执行文件；Windows 上应使用 `.exe`。省略时按[后端的路径查找规则](backends.md#可执行文件与配置继承)定位。

`axiopera models <agent>` 查询模型 ID、名称、默认模型和支持的推理强度。`--cwd` 指定工作目录，默认当前目录。`--json` 还保留后端提供的说明、别名解析后的模型 ID 和禁用状态。与 ID 相同的名称在文本中省略；Axiopera 配置的模型覆盖值单独注明。

查询沿用后端的目录发现和缓存，最多等待 20 秒。模型 ID 可直接用于 `spawn --model`，列表中的推理强度可用于 `--effort`。参数仍按原样传给后端。后端明确报告模型或推理强度无效时，错误后附查询结果；自动查询最多等待 5 秒，失败或返回空列表时附查询命令。认证、权限、额度和网络错误保持原报错。

daemon 启动时读取配置。修改后运行 `axiopera daemon stop`，下一次调用会重新启动。未知配置项会报错。

## Skill 与 Codex 插件

skill 让 agent 在派发任务前先运行 `axiopera guide --host <宿主>`，效果与在项目说明中写明这一点相同。`--host` 只输出该宿主取结果的方式，不带时输出全部宿主的。skill 就是仓库中的 [`plugins/axiopera/skills/axiopera/`](../plugins/axiopera/skills/axiopera) 目录，复制到 agent 的 skills 目录即可，删除该目录即卸载：

| agent | 位置 |
|---|---|
| Claude Code | `~/.claude/skills/axiopera/` |
| Codex | `~/.agents/skills/axiopera/` |
| Grok | 默认也读取上面两个位置；只用 Grok 时放到 `~/.grok/skills/axiopera/` |

每个会话的上下文都会列出 skill 的名称和描述，约占一行。

Codex 插件包含同一个 skill，另带 hook，让宿主在一轮进行中也能得知子任务的消息。装了插件就不必再复制 skill。插件不包含 `axiopera` 程序。

- 安装：`codex plugin marketplace add Victor-Quqi/Axiopera`，再 `codex plugin add axiopera@axiopera`。
- 卸载：`codex plugin remove axiopera@axiopera`，再 `codex plugin marketplace remove axiopera`。

hook 信任后才运行。桌面端在插件页信任；TUI 启动时发现新的或改动过的 hook 会弹出确认，也可以在 `/hooks` 中信任。hook 定义改动后需重新信任；Codex 0.164 起，插件中任何文件改动都需重新信任，包括升级插件。VS Code 扩展官方不支持插件。工具调用之后，如果有新的结果、提问或审批请求，hook 列出每条的一行摘要，提醒宿主运行 `inbox`；一轮结束时仍未取走的，由门铃在本轮结束后提醒。

装了插件后，Codex 每个会话都会启动一个 `axiopera mcp --codex-hooks` 进程。hook 只查询已在运行的 daemon，不会启动它；没有相关任务时立即返回。实测进程启动不到 0.1 秒，每次 hook 调用不到 1 毫秒。删除 `axiopera` 程序前先卸载插件，否则 Codex 无法启动这个进程。卸载不会结束已打开会话中的这个进程，需要时手动结束。

Codex 卸载插件后会留下以下内容，运行 `axiopera plugin cleanup` 清除：

- `config.toml` 中 hook 的信任记录 `[hooks.state."axiopera@<来源>:codex/hooks.json:…"]`。如果保留，重装定义相同的插件时会直接沿用旧的信任。
- `.hook-trust/` 下插件内容的基线（Codex 0.164 起）。Codex 用它核对旧版本写下的信任记录。
- 空目录 `plugins/cache/axiopera`。

数据目录取 `CODEX_HOME`，未设置时为 `~/.codex`。插件仍装着时，清理命令不做任何处理。配置通过 Codex 自己的配置接口修改。

## daemon 与清理

CLI 按需启动 daemon。没有运行中的 agent 和客户端连接时，daemon 空闲 10 分钟后退出。主会话退出不会取消已派发的任务。

`axiopera daemon status` 查看 daemon，`daemon stop` 在没有运行任务时停止它。`daemon stop --force` 会中断正在运行的任务，之后可用 `send` 续接。升级二进制前先停止 daemon。

Axiopera 自动删除 30 天未活动且已停止的任务，以及请求日志 `daemon/requests.jsonl` 和 daemon 日志 `daemon/daemon.log` 中 30 天前的行。`axiopera gc` 立即删除所有已停止的任务和请求日志，`--days N` 保留最近 N 天有活动的。运行中或即将开始运行的任务，以及仍有子任务的父任务，都不会被删除；收到兄弟任务的留言也算一次活动。任务删除后不能再对它 `send`、`result` 或 `log`。

卸载前先停止 daemon。通过 Cargo 安装的执行 `cargo uninstall axiopera`；通过 Release 压缩包安装的删除可执行文件。数据目录按需删除，原生 agent 的会话仍由各自 CLI 管理。
