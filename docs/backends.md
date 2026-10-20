# 后端协议与权限

本文记录 Axiopera 依赖的后端行为，参数映射以 `src/agent/` 的实现为准。这些行为受版本限制，依据和已测版本见[验证与依据](verification.md)。

## 权限

| 档位 | Claude Code | Codex | Grok Build |
|---|---|---|---|
| read | `default`，移除 Edit、Write、NotebookEdit、Agent；通过 stdio 接审批 | `read-only`，`never`，reviewer 为 `user` | ask 模式，移除写入、子 agent、工作流、定时任务、媒体生成和 MCP 入口 |
| work | `auto` | `workspace-write`，`on-request`，reviewer 为 `auto_review` | `autoMode:true`、`yoloMode:false` |
| full | `bypassPermissions` | `danger-full-access`，`on-request`，reviewer 为 `user` | `yoloMode:true` |

新建和续接会话都显式设置权限模式，用户已有的允许和拒绝规则仍参与后端判断。同档位的能力存在差异，`spawn` 回执会报告影响任务的限制。

read 档允许 Claude 和 Codex 使用标注 `readOnlyHint:true` 且没有 `destructiveHint:true` 的 MCP 工具。Claude 适配器在每次调用时查询 `mcp_status`，按完整工具名检查注解；查询失败或 10 秒未返回就拒绝。Codex 使用原生注解判断。Grok 隐藏 `search_tool`、`use_tool`，因为 `agentProfile.disallowedTools` 无法逐个禁用 MCP 工具，ACP 状态也不提供注解。

Grok 的 work 档不限制内置文件工具写入项目外。Windows 上 Claude 和 Grok 没有生效的 OS sandbox；Codex 的限制取决于自身 sandbox 模式。命令规则、MCP 注解和 backend 的自动审查各自决定其负责的调用。

回连 daemon 的差异如下。受限客户端能连接时，仍受[IPC 操作限制](design.md#身份与-ipc)约束。

| 后端环境 | 回连方式 |
|---|---|
| Codex Windows unelevated | 命名管道 DACL 允许受限令牌中的登录会话 SID，shell 可以连接 |
| Codex Windows elevated | shell 以单独账户运行，无法连接该管道 |
| Codex Linux 受限网络 | `connect()` 被拦截，shell 无法连接 Unix socket |
| Claude Linux 启用 sandbox | socket 连接被拦截 |
| Grok Linux 启用 sandbox | 可以连接，派发请求被 daemon 拒绝 |

Codex 子 agent 始终通过 Axiopera 注入的 MCP server 调用 `ask`、`tell`、`answers`，其进程在 Codex sandbox 外。MCP 配置显式携带任务 ID 和数据目录。其他 shell 调用需要依宿主提示提权重跑。

Codex work 档会为工作目录或 worktree 添加文件系统 ACE，这是原生 Windows sandbox 的行为。`full` 使用 `on-request`，使原生危险操作判断可以发出审批。线程配置将已配置 MCP 服务器的 `default_tools_approval_mode` 设为 `approve`，单个工具显式设置的 prompt 仍生效。

work 档需要判定的操作交给各后端自己的自动判定，判定请求经会话所用的 API 发出。以下表现来自源码、文档和二进制字符串，未用第三方 API 实测：

- Codex 的 auto_review 使用固定的审查模型：以 API key 登录时为 `gpt-5.6-luna`，否则为 `codex-auto-review`。会话模型的目录条目设置了 `auto_review_model_override` 时用它；provider 的模型目录中没有首选模型时才改用会话模型。自定义 provider 未配置模型目录时使用 Codex 内置目录，其中包含这两个模型。接口不提供审查模型时审查失败，操作被拒，不转为审批。可以让中转映射该模型名，或在 `model_catalog_json` 中为会话模型设置指向自身的 `auto_review_model_override`；顶层 `review_model` 不影响审查。
- Windows 上未配置 Codex 的 Windows sandbox 时，Codex 把 `workspace-write` 降为 `read-only` 且不报错，没有命中规则的命令都要经过审查。
- Claude 的分类器默认使用 Sonnet 5，首次请求失败后改用会话模型；失败为 HTTP 401 时不改用，需要审查的操作全部被拒，不转为询问。会话模型不支持 auto 时，会话以 `default` 启动，非只读操作都经 `can_use_tool` 转来审批。
- Grok 的分类器使用会话模型和端点。请求失败或超时时转为审批；返回内容无法解析时拒绝。

Codex 的审查失败和 sandbox 降级由适配器识别，在下一条轮末结果上提示主 agent，每个进程各提示一次。

## 审批

read 档按上述只读规则裁决，其他需要批准的请求直接拒绝。work 和 full 档转发以下请求，路由和超时见[内部设计](design.md#审批路由)。

| 后端 | 转发的请求 | 单次应答 |
|---|---|---|
| Claude | `can_use_tool` | allow 或 deny，不写 `updatedPermissions` |
| Codex | `item/commandExecution/requestApproval`、`item/fileChange/requestApproval`、`item/permissions/requestApproval` | accept、decline 或仅本轮的 permissions |
| Grok | `session/request_permission` | allow-once 或 reject-once |

Claude 其他 control request 返回 unsupported。Codex 的 MCP elicitation 返回 decline，`requestUserInput` 返回空 answers。Grok 其他请求直接拒绝。上述情况均记为 `blocked`，由轮末汇总。

Codex decline 不能带拒绝理由，适配器另用 steer 送入理由。Grok 通过 `_meta.followup_message` 传理由。各家要求持久化规则的批准选项均不使用。

后端自己拒绝且没有请求批准的操作，按原生错误返回给子 agent。适配器通过 Claude 的 `permission_denials`、Codex 的 auto-review 结果、Grok 工具错误中的 `Auto mode blocked this action` 识别并记录。

## 可执行文件与配置继承

显式 `agents.<name>.path` 优先。Grok 其次查找 `GROK_HOME/bin`，省略 home 时使用 `~/.grok/bin`；其他情况查调用方 PATH。Windows 在 PATH 下寻找 `.exe`，遇到 Claude 或 Codex 的 npm `.cmd` 入口时，按包布局定位原生 `.exe`。Unix 使用 PATH 中的命令。

子 agent 使用原生 CLI 的登录、模型、MCP 和规则配置。Axiopera 不写入用户的 agent 配置，只通过单次参数及线程配置覆盖需要的选项：

- Claude 每档附加 `--allowedTools "Bash(axiopera *)"`，不清空 setting sources。未在原生客户端批准的项目 `.mcp.json` 服务器，通过单次 `disabledMcpjsonServers` 关闭。
- Grok 设置 `GROK_CLAUDE_{MCPS,HOOKS,SKILLS,RULES,AGENTS}_ENABLED=0`，关闭这些 Claude 配置导入。Grok 自身权限配置仍生效。
- Codex 在线程中设置 reviewer、sandbox 和 MCP；读取配置时只取所需的 MCP 服务器名。

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

Claude read 档另加 `--permission-prompt-tool stdio`，只加 `host` 时收不到该档位的审批请求。

Claude 的 user 回显可能合并多条消息，需要按 text block 匹配。轮末仍有未回显输入时继续持有进程。后台命令结束也可能让模型自行开启下一轮，通过 `background_tasks_changed` 和 `task_notification` 跟踪。

Codex 的 `userMessage` 可能在轮末落盘，后续模型响应才构成输入被消费的证据。daemon 将未确认输入带入下一轮。`turn/steer` 返回无活跃轮次时，先处理旧轮末事件再开启新轮。

Grok 的 `_x.ai/session/interjection` 只确认入队。轮末输出注明消费确认缺口，Grok 把晚到消息排入后续 prompt。一次 prompt 内可以出现多段答复，`response_completed` 分隔答复；下一段实际输出文字后才替换已有结果。适配器在轮末保留 1 秒等待，并跟踪后台任务和 `runningPromptId`。`session/load` 的历史回放全部忽略。

并发续接同一会话时，Claude 和 Grok 可以出现各自写入、互相看不到新轮次的情况，Codex 则拒绝第二个 writer。因此 Axiopera 在空闲时释放进程，并在续接前核对会话文件。

## 宿主通知

| 宿主 | 本轮结束后 | 本轮进行中 | 方式 |
|---|---|---|---|
| Claude Code，含 VS Code 扩展 | 自动开启一轮 | 下一次工具调用前送达 | 后台运行 `wait`，退出后 `inbox` |
| Grok | 自动开启一轮 | 用 monitor 时下一次工具调用后送达；后台命令等本轮结束 | 用 monitor 运行 `watch`，再 `inbox` |
| Codex TUI | 自动开启一轮 | 送入当前轮 | 推送 |
| Codex 桌面端、VS Code 扩展 | 约 10 秒内开启一轮 | 等本轮结束；桌面端装了[插件](usage.md#skill-与-codex-插件)时，下一次工具调用后提醒 | 门铃，插件 hook |
| 其他 Codex 会话 | 不唤醒 | 不送达 | 轮询 `wait`，再 `inbox` |

Claude 和 Grok 由宿主进程在后台命令退出时唤醒，与界面无关。Grok 的 monitor 把命令的每行输出作为一个事件：宿主忙时在当前这次工具调用之后送入本轮，空闲时开启一轮。monitor 每行保留 500 字节，最多连发 10 个事件，之后每 2 秒补 1 个，超出的丢弃；命令自行退出时，最后几行可能丢失。monitor 启动的命令没有 `GROK_SESSION_ID`，因此 `watch` 用 `--caller grok:<会话>` 指明调用方；`guide` 直接给出填好调用方的命令：Grok 会话填会话 ID，Grok 子 agent 填任务 ID。无人值守的 `grok -p` 默认等所有后台任务结束才退出，而 persistent monitor 不会自行结束，因此这样运行 Grok 时加 `--no-wait-for-background`，该选项不在 `--help` 中列出。Axiopera 不向 Claude inbox socket 推送，因为 Linux 上脱离宿主进程树的 daemon 会触发入站批准，而且该接口没有送达回执。

Codex 交互式会话挂在用户级 app-server daemon 上时，Axiopera 经 `codex app-server proxy` 连接其 WebSocket JSON-RPC。用 `thread/loaded/list` 确认会话已加载，再按轮次状态调用 `turn/steer` 或 `turn/start`。`codex exec`、`--no-daemon`、带 `-c` 配置覆盖、Windows 管理员终端中的会话不在该 daemon 上；桌面端和 VS Code 扩展的会话也不在，它们各自以 stdio 启动私有 app-server。

不在共享 daemon 上的会话，由 Axiopera 另起的私有 `codex app-server` 用 `thread/read` 读取来源。来源为 `vscode` 表示 app-server 客户端的会话，桌面端和 VS Code 扩展都属于此类，Axiopera 对其使用门铃：

- 用持久化队列的 `thread/queue/add` 排入一条提醒，让宿主运行 `inbox`。队列属于实验性 API，需在 `initialize` 中声明 `experimentalApi`。
- 加载了该会话的 app-server 约每 10 秒检查一次队列。会话空闲时用提醒开启一轮，运行中则等本轮结束。用户中断上一轮后，要等下一轮开始才继续处理。会话未打开时提醒一直保留。
- 提醒在界面上显示为一条用户消息。
- 宿主运行 `inbox` 后，Axiopera 用 `thread/queue/delete` 撤回尚未处理的提醒。

`codex exec` 跑完一轮就退出，排入的提醒要到下次打开该会话时才触发，因此不用门铃。

Codex 插件的 hook 用 `mcp_tool` 处理器调用插件自带的 MCP 服务器，hook 的调用不受工具审批影响。PostToolUse 返回的 `additionalContext` 在这次工具调用之后作为 developer 消息写入当前轮。命令运行超过 shell 工具的单次等待、转入后台时，要等之后某次轮询看到它退出才触发 PostToolUse。只跑长命令或长时间不调工具时，提醒由本轮结束后的门铃送达。工具的 `_meta.ui.visibility` 不含 `model` 时，模型看不到该工具，hook 仍可调用。

Windows 上 Codex 经 PowerShell `-Command` 执行命令，外部程序的非零退出码一律报告为 1。

TUI 退出后，Codex daemon 约一分钟才卸载会话，这段时间到达的事件仍可能开启一轮。推送去重、门铃和重试见[内部设计](design.md#codex-推送与门铃)。
