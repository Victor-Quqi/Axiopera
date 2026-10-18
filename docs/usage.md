# 使用与配置

派发、收结果和对话的示例见 [agent 使用说明](../src/guide.md)。所有命令支持全局选项 `--json`；各命令的参数见 `axiopera <命令> --help`。

## 任务与工作目录

默认情况下，子 agent 使用调用方的工作目录。`spawn --worktree` 从当前仓库的 HEAD 创建分支 `axiopera/<别名或任务 ID>`，并在 Axiopera 的数据目录下检出。当前工作区未提交的改动不会带入。调用方在仓库子目录时，子 agent 使用新 worktree 中对应的子目录。

主 agent 负责检查子 agent 的改动、提交和合并分支。子 agent 的权限由 `--tier` 指定，默认 `work`。各档位在不同 backend 上的具体行为见[权限](backends.md#权限)。

任务 ID 全局唯一，`--name` 指定的别名在同一个父节点下唯一。主 agent 为第 0 层，最多派发到第 2 层。子任务的档位不能高于父任务。

子 agent 用 `ask` 提问后可以继续工作。若这一轮结束时仍有未回答的问题，任务进入 `waiting`；回答会在原会话中开启下一轮。兄弟任务通过 `tell` 留言，父节点收到抄送。接收方空闲时，留言等到下一轮开始才送入，不会单独开启一轮。

`spawn`、`send`、`reply` 接受位置参数、`-f FILE` 或单独的 `-`。后两种分别读取 UTF-8 文件和标准输入，保留原始换行。三种来源只能选一种，内容不能为空。

## 结果与接管

每轮结果经 `wait`、`result` 或宿主推送交付后，再次 `result` 只返回文件路径。需要重新查看正文时用 `result <task> --turn N --force`。`watch` 持续输出事件摘要，不消费结果。

回复长度有上限。正文超长时只保留末尾，并附完整文件路径；多任务结果超出本次回复预算时，未交付的正文可以用 `result` 取得。`status` 查看任务状态，`log` 查看事件摘要。加上 `--json` 可以读取完整任务字段和事件字段，包括改动文件列表及后端上报的用量。

`detach <task>` 将任务交给人接管，并打印工作目录和原生 resume 命令。接管前要等这一轮结束，或先 `cancel`。Axiopera 发现原生会话文件被其他进程修改时，也会将任务标为 detached。

`cancel` 取消整个任务子树。`failed` 和 `interrupted` 任务可以用 `send` 继续；已取消或脱管的任务通过原生 CLI 接管。

退出码见 [guide](../src/guide.md)。同一批事件包含失败和取消时返回失败。

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

## daemon 与清理

CLI 按需启动 daemon。没有运行中的 agent 和客户端连接时，daemon 空闲 10 分钟后退出。主会话退出不会取消已派发的任务。

`axiopera daemon status` 查看 daemon，`daemon stop` 在没有运行任务时停止它。`daemon stop --force` 会中断正在运行的任务，之后可用 `send` 续接。升级二进制前先停止 daemon。

`axiopera gc --days 7` 清理至少 7 天未活动且已停止的任务。存在未提交改动的 worktree 会连同任务保留，分支仅在已合并时删除。仍有子任务的父任务也会保留。

卸载前先处理需要保留的 worktree 改动，运行 `gc --days 0` 清理可删除的任务，再停止 daemon。通过 Cargo 安装的执行 `cargo uninstall axiopera`；通过 Release 压缩包安装的删除可执行文件。数据目录按需删除，原生 agent 的会话仍由各自 CLI 管理。
