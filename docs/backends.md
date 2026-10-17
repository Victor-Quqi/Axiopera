# 后端兼容性与权限

本文集中记录 Axiopera 使用的后端行为。参数映射以 `src/agent/` 的实现为准。标为〔实验〕的内容来自真实 CLI 探针，〔源码〕来自所列版本的上游实现；二者都受版本限制。探针运行方式见[开发文档](development.md#真实-agent-探针)。

## 兼容性

| 平台 | 已验证范围 | CLI 版本 |
|---|---|---|
| Windows 11 | 三家互相派发、中途消息、续接、取消；read 档 MCP、消息确认、Codex elevated sandbox 与宿主推送另有补测 | 基础端到端为 Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52；补测为 Claude Code 2.1.302、Codex 0.163.0、Grok 1.0.62 |
| Linux，WSL2 Ubuntu 24.04 | 三家原生 Linux CLI 的端到端流程、sandbox 回连和进程组取消 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| macOS | 有平台实现，真实 agent、sandbox 检测及回连尚未实测 | 未验证 |

以上为〔实验〕，基础验证在 2026-10-13 至 14 日，补测在 2026-10-17 日。版本表记录已测组合，最低版本尚未确定。

## 权限

| 档位 | Claude Code | Codex | Grok Build |
|---|---|---|---|
| read | `default`，移除 Edit、Write、NotebookEdit、Agent；通过 stdio 接审批 | `read-only`，`never`，reviewer 为 `user` | ask 模式，移除写入、子 agent、工作流、定时任务、媒体生成和 MCP 入口 |
| work | `auto` | `workspace-write`，`on-request`，reviewer 为 `auto_review` | `autoMode:true`、`yoloMode:false` |
| full | `bypassPermissions` | `danger-full-access`，`on-request`，reviewer 为 `user` | `yoloMode:true` |

新建和续接会话都显式设置权限模式，用户已有的允许和拒绝规则仍参与后端判断。同档位的能力存在差异，`spawn` 回执会报告影响任务的限制。

read 档允许 Claude 和 Codex 使用标注 `readOnlyHint:true` 且没有 `destructiveHint:true` 的 MCP 工具。Claude 适配器在每次调用时查询 `mcp_status`，按完整工具名检查注解；查询失败或 10 秒未返回就拒绝。Codex 使用原生注解判断。Grok 隐藏 `search_tool`、`use_tool`，因为 `agentProfile.disallowedTools` 无法逐个禁用 MCP 工具，ACP 状态也不提供注解。〔实验〕`read_mcp.py`

Grok 的 work 档不限制内置文件工具写入项目外。Windows 上 Claude 和 Grok 没有生效的 OS sandbox；Codex 的限制取决于自身 sandbox 模式。命令规则、MCP 注解和 backend 的自动审查各自决定其负责的调用。

回连 daemon 的差异如下。受限客户端能连接时，仍受[IPC 操作限制](design.md#身份与-ipc)约束。

| 后端环境 | 回连方式 |
|---|---|
| Codex Windows unelevated | 命名管道 DACL 允许受限令牌中的登录会话 SID，shell 可以连接 |
| Codex Windows elevated | shell 以单独账户运行，无法连接该管道 |
| Codex Linux 受限网络 | `connect()` 被拦截，shell 无法连接 Unix socket |
| Claude Linux 启用 sandbox | socket 连接被拦截 |
| Grok Linux 启用 sandbox | 可以连接，派发请求被 daemon 拒绝 |

以上为〔实验〕。Codex 子 agent 始终通过 Axiopera 注入的 MCP server 调用 `ask`、`tell`、`inbox`，其进程在 Codex sandbox 外。MCP 配置显式携带任务 ID 和数据目录。其他 shell 调用需要依宿主提示提权重跑。

Codex work 档会为工作目录或 worktree 添加文件系统 ACE，这是原生 Windows sandbox 的行为。`full` 使用 `on-request`，使原生危险操作判断可以发出审批。线程配置将已配置 MCP 服务器的 `default_tools_approval_mode` 设为 `approve`，单个工具显式设置的 prompt 仍生效。〔实验〕

## 审批

read 档按上述只读规则裁决，其他需要批准的请求直接拒绝。work 和 full 档转发以下请求，路由和超时见[内部设计](design.md#审批路由)。

| 后端 | 转发的请求 | 单次应答 |
|---|---|---|
| Claude | `can_use_tool` | allow 或 deny，不写 `updatedPermissions` |
| Codex | `item/commandExecution/requestApproval`、`item/fileChange/requestApproval`、`item/permissions/requestApproval` | accept、decline 或仅本轮的 permissions |
| Grok | `session/request_permission` | allow-once 或 reject-once |

Claude 其他 control request 返回 unsupported。Codex 的 MCP elicitation 返回 decline，`requestUserInput` 返回空 answers。Grok 其他请求直接拒绝。上述情况均记为 `blocked`，由轮末汇总。

Codex decline 不能带拒绝理由，适配器另用 steer 送入理由。Grok 通过 `_meta.followup_message` 传理由。各家要求持久化规则的批准选项均不使用。

后端自己拒绝且没有请求批准的操作，按原生错误返回给子 agent。适配器通过 Claude 的 `permission_denials`、Codex 的 auto-review 结果、Grok 工具错误中的 `Auto mode blocked this action` 识别并记录。〔实验〕`native_block.py`

## 可执行文件与配置继承

显式 `agents.<name>.path` 优先。Grok 其次查找 `GROK_HOME/bin`，省略 home 时使用 `~/.grok/bin`；其他情况查调用方 PATH。Windows 在 PATH 下寻找 `.exe`，遇到 Claude 或 Codex 的 npm `.cmd` 入口时，按包布局定位原生 `.exe`。Unix 使用 PATH 中的命令。

子 agent 使用原生 CLI 的登录、模型、MCP 和规则配置。Axiopera 只通过单次参数及线程配置覆盖需要的选项：

- Claude 每档附加 `--allowedTools "Bash(axiopera *)"`，不清空 setting sources。未在原生客户端批准的项目 `.mcp.json` 服务器，通过单次 `disabledMcpjsonServers` 关闭。
- Grok 设置 `GROK_CLAUDE_{MCPS,HOOKS,SKILLS,RULES,AGENTS}_ENABLED=0`，关闭这些 Claude 配置导入。Grok 自身权限配置仍生效。
- Codex 在线程中设置 reviewer、sandbox 和 MCP；读取配置时只取所需的 MCP 服务器名。

Axiopera 不向用户的 agent 配置写入设置。

## 会话与消息

| 接口 | Claude | Codex | Grok |
|---|---|---|---|
| 进程协议 | `-p`，stream-json 输入输出，启用 verbose、replay-user-messages 和 permission-prompts host | `app-server` JSON-RPC | `agent --no-leader stdio` ACP |
| 新建 / 续接 | `--session-id` / `--resume` | `thread/start` / `thread/resume` | `session/new` / `session/load` |
| 注入任务协议 | `--append-system-prompt` | `developerInstructions` | `_meta.rules` |
| 开始一轮 | stdin user 消息 | `turn/start` | `session/prompt` |
| 中途消息 | stdin user 消息 | `turn/steer` | `_x.ai/interject` |
| 中断 | interrupt control request | `turn/interrupt` | `session/cancel` |
| 最终结果 | `result.result` | 最后一条 `agentMessage` | 最后一段答复中，最后一次工具调用之后的文字 |

Claude read 档另加 `--permission-prompt-tool stdio`，实测只加 `host` 收不到该档位的审批请求。〔实验〕`read_mcp.py`

Claude 的 user 回显可能合并多条消息，需要按 text block 匹配。轮末仍有未回显输入时继续持有进程。后台命令结束也可能让模型自行开启下一轮，通过 `background_tasks_changed` 和 `task_notification` 跟踪。〔实验〕`message_receipts.py`、`background.py`

Codex 的 `userMessage` 可能在轮末落盘，后续模型响应才构成输入被消费的证据。daemon 将未确认输入带入下一轮。`turn/steer` 返回无活跃轮次时，先处理旧轮末事件再开启新轮。〔实验/源码〕`message_receipts.py`；[tasks/mod.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/tasks/mod.rs)

Grok 的 `_x.ai/session/interjection` 只确认入队。轮末输出注明消费确认缺口，Grok 把晚到消息排入后续 prompt。一次 prompt 内可以出现多段答复，`response_completed` 分隔答复；下一段实际输出文字后才替换已有结果。适配器在轮末保留 1 秒等待，并跟踪后台任务和 `runningPromptId`。`session/load` 的历史回放全部忽略。〔实验/源码〕`message_receipts.py`、`grok_response_boundary.py`、`background.py`；[interjection.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs)

并发续接同一会话时，Claude 和 Grok 可以出现各自写入、互相看不到新轮次的情况，Codex 则拒绝第二个 writer。因此 Axiopera 在空闲时释放进程，并在续接前核对会话文件。〔实验〕`resume.py`

## 宿主通知

Claude 和 Grok 使用后台 `wait` 的退出通知或 monitor。Axiopera 不向 Claude inbox socket 推送，因为 Linux 上脱离宿主进程树的 daemon 会触发入站批准，而且该接口没有送达回执。〔实验/源码〕[Claude 跨会话消息](https://code.claude.com/docs/en/cross-session-messaging)

Codex 交互式会话挂在用户级 app-server daemon 上时，Axiopera 经 `codex app-server proxy` 连接其 WebSocket JSON-RPC。用 `thread/loaded/list` 确认会话已加载，再按轮次状态调用 `turn/steer` 或 `turn/start`。`codex exec`、`--no-daemon`、带 `-c` 配置覆盖及 Windows 管理员终端中的会话不走此通道，需要轮询 `wait`。〔实验〕`codex_host_push.py`

TUI 退出后，Codex daemon 约一分钟才卸载会话，这段时间到达的事件仍可能开启一轮。Windows 推送验证使用 daemon 0.160.0、TUI 0.163.0；Linux 使用 0.162.1。推送去重和重试见[内部设计](design.md#codex-宿主推送)。
