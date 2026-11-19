# 验证与依据

[后端行为](backends.md)的验证范围、依据与探针用法。

## 已测版本

| 平台 | 已验证范围 | CLI 版本 |
|---|---|---|
| Windows 11 | 跨宿主派发、消息、续接与取消 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| Windows 11 | read 档 MCP、Codex elevated sandbox、消费确认、宿主通知和插件 | Claude Code 2.1.302 至 2.1.312、VS Code 扩展 2.1.301；Codex 0.163.0、0.164.0，共享 daemon 0.160.0，桌面端内置 0.162.0-alpha.17.2、0.163.0-alpha.10.2；Grok 1.0.62、1.0.67 |
| Windows 11 | [个人命令禁令、AGY 派发与宿主收件](#antigravity-cli-1511) | Antigravity CLI 1.5.7、1.5.11 |
| Linux，WSL2 Ubuntu 24.04 | 三家原生 Linux CLI 的端到端流程、sandbox 回连、进程组取消和 Codex 宿主推送 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| macOS | 有平台实现，真实 agent、sandbox 检测及回连尚未实测 | 未验证 |

最低支持版本尚未确定。

## 依据

脚本名指 `spikes/` 下的探针。

| 结论 | 依据 |
|---|---|
| [AGY 中途消息](backends.md#会话与消息) | Windows、AGY 1.5.2，接受追加消息时旧 prompt 提前返回 `end_turn`，等待审批时新 prompt 返回 `-32603`。本地协议测试覆盖两种应答、拒收后的持久化和下一轮交付。 |
| [模型查询](backends.md#模型查询)：三家原生目录查询及推理强度字段；Codex 无效强度错误附模型列表；不带 `--model` 的任务按派发默认值运行，带 `--model`、`--effort` 的任务按请求运行，`status` 和每轮结果首行显示实际模型 | Windows，`models.py` 验证原生目录；`models.py codex --errors` 验证非法强度错误；`models.py --tasks` 验证派发默认值及显式覆盖。目录覆盖 Claude 2.1.309、Codex 0.164.0、Grok 1.0.67，派发覆盖 Claude 2.1.316、Codex 0.166.0、Grok 1.0.67。Claude 2.1.312 的 JSON 目录含 `resolvedModel`。Grok 的会话默认模型受服务端 campaign 影响。[campaigns.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/util/config/campaigns.rs#L14-L56)、[session_setup.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/mvp_agent/session_setup.rs#L516-L535)、[resolution.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/remote_config/resolution.rs#L135-L232)、[manager/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/remote_config/manager/mod.rs#L1225-L1375) |
| [会话与消息](backends.md#会话与消息)：实际模型与推理强度的来源 | Windows。Claude 2.1.316 的 `get_settings.applied` 随显式模型、强度变化，可在首轮前读取；`model_fallback` 字段依据二进制。Codex 0.166.0 的 `thread/start` 与 `config/read` 一致，`model/rerouted` 的作用域和强度回退依据 schema 与源码。Grok 1.0.67 用 `model_changed` 报告配置变化，`session/prompt` 的 `_meta.modelId` 与会话文件一致。[session/mod.rs](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/core/src/session/mod.rs#L4348-L4375)、[step_settings.rs](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/core/src/session/step_settings.rs#L76-L87) |
| [会话与消息](backends.md#会话与消息)：Grok ACP 会话忽略 `-m`、`--reasoning-effort`，`session/set_config_option` 生效，无效值返回 `-32602` | Windows、Grok 1.0.67 的 prompt 回复、`model_changed` 和会话文件。环境变量 `GROK_DEFAULT_MODEL` 也不覆盖新会话模型。该版本原生目录只有 `grok-build-plan`，跨 harness 重建 agent 的行为依据源码，read 档拒绝路径由单元测试覆盖。[model_switch.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs#L60-L175)、[mvp_agent/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/mvp_agent/mod.rs#L935-L948) |
| [已测版本](#已测版本)中的端到端流程：主 agent 仅凭 `guide --host` 完成派发、收结果、追问和取消 | `host_e2e.py` |
| [宿主通知](backends.md#宿主通知)：`grok -p` 等 persistent monitor 结束才退出，`--no-wait-for-background` 让它在本轮结束时退出 | `host_e2e.py grok` |
| [权限](backends.md#权限)：read 档 MCP 工具按注解过滤 | `read_mcp.py` |
| [权限](backends.md#权限)：Grok 权限模式和工具裁剪 | `grok_perms.py`、`grok_profile.py` |
| [权限](backends.md#权限)：回连差异表中 Codex Windows 两行 | `ipc_sandbox.py`，elevated 加 `-c windows.sandbox="elevated"`；手动实验：在 elevated sandbox 中运行 `axiopera daemon status` |
| [权限](backends.md#权限)：回连差异表中 Linux 三行 | 手动实验：在三家原生 Linux sandbox 内调用 `axiopera` |
| [权限](backends.md#权限)：Codex work 档添加 ACE；MCP 服务器设为 `approve` 后免审批 | 手动实验 |
| [权限](backends.md#权限)：Codex auto_review 的审查模型，审查失败即拒绝；未配置 Windows sandbox 时 `workspace-write` 降为 `read-only` | 源码 [provider.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/model-provider/src/provider.rs#L91-L93)、[model.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/guardian-reviewer/src/model.rs#L18-L76)、[completion.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/guardian-reviewer/src/completion.rs#L127-L159)、[config_toml.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/config/src/config_toml.rs#L821-L839)；`auto_review_model_override` 的绕法和 `review_model` 无效见 [openai/codex#47916](https://github.com/openai/codex/issues/47916) |
| [权限](backends.md#权限)：Claude auto 分类器的模型选择和失败处理 | 文档 [permission-modes](https://code.claude.com/docs/en/permission-modes#default-classifier-model)、[errors](https://code.claude.com/docs/en/errors#auto-mode-cannot-determine-the-safety-of-an-action)；Claude Code 2.1.302 二进制中的日志字符串 |
| [权限](backends.md#权限)：Grok auto 分类器使用会话模型，失败或超时转为审批，无法解析时拒绝 | 源码 [sampler_turn.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs#L830-L873)、[manager/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs#L1063-L1078)、[auto_mode/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs#L1480-L1499) |
| [审批](backends.md#审批)：不使用持久化批准选项 | `codex_escalate.py`：Codex 的 amendment 会写入用户规则，`acceptForSession` 只缓存完全相同的命令 |
| [审批](backends.md#审批)：后端自行拒绝时的返回形式 | `native_block.py` |
| [审批](backends.md#审批)：Codex 撤回请求 | Windows、Codex 0.167.0。命令审批收到 `decline` 后，`serverRequest/resolved.requestId` 与原 JSON-RPC 请求 ID 一致。网络请求在裁决前结束时的撤回行为依据上游源码。[bespoke_event_handling.rs](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/app-server/src/bespoke_event_handling.rs#L836-L846)、[outgoing_message.rs](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/app-server/src/outgoing_message.rs#L232-L241) |
| [会话与消息](backends.md#会话与消息)：Claude read 档需另加 `--permission-prompt-tool stdio` | `read_mcp.py` |
| [会话与消息](backends.md#会话与消息)：中途消息和消费确认 | `steer.py`、`message_receipts.py`；源码 [Codex tasks/mod.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/tasks/mod.rs)、[Grok interjection.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) |
| [任务与工作目录](usage.md#任务与工作目录)：`send --tier` 保留会话并切换权限 | `tier_resume.py`，Windows、Claude Code 2.1.312、Codex 0.165.0、Grok 1.0.67。从 read 切至 full 再切回 read，会话 ID 一致，只有 full 档能写文件。档位持久化、父子档位约束和运行中切档由单元测试覆盖 |
| [会话与消息](backends.md#会话与消息)：Grok 空闲时的消息，以及一次 prompt 内的多段答复 | `grok_idle_interject.py`、`grok_response_boundary.py` |
| [会话与消息](backends.md#会话与消息)：Grok 工具名、参数摘要及后续更新 | 上游工具事件与 MCP 输入结构。适配器、daemon 和 `top` 单元测试覆盖参数提取、MCP 展开、更新去重和调用计数。[tool_calls.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs)、[use_tool/input.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/use_tool/input.rs) |
| [会话与消息](backends.md#会话与消息)：后台命令结束后自行开启一轮 | `background.py` |
| [结果与接管](usage.md#结果与接管)：审批请求先于更早的结果交出，交出前不能批准；过长的提问和留言只给首尾，全文写入文件；agent 自行开启的一轮从开始就计入进度 | `delivery.py` |
| [会话与消息](backends.md#会话与消息)：并发续接 | `resume.py` |
| [会话与消息](backends.md#会话与消息)：Grok 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `grok_native_identity.py`，Windows、Grok 1.0.67。原生 auto 宿主和 Axiopera work 宿主下，主会话均无法取走子会话的结果 |
| [会话与消息](backends.md#会话与消息)：Codex 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `codex_native_identity.py`，Windows、Codex 0.164.0。原生 full 权限宿主（`danger-full-access`）和 Axiopera work 宿主下，主会话均无法取走子线程的结果 |
| [宿主通知](backends.md#宿主通知)：Claude 和 Grok 在本轮进行中能否收到后台命令的完成通知 | `background.py busy` |
| [宿主通知](backends.md#宿主通知)：Codex 后台状态显示 | `status_watch_*` 测试覆盖后台状态与调用方隔离。Codex CLI 0.166.0 的 `thread/backgroundTerminals/list` 可列出 `/ps` 遗漏的任务；后台终端结束不会唤醒宿主。[启动事件去重](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/app-server/src/bespoke_event_handling.rs#L1075)、[界面登记](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/tui/src/chatwidget/command_lifecycle.rs#L35)、[async_watcher.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/core/src/unified_exec/async_watcher.rs#L155-L243) |
| [宿主通知](backends.md#宿主通知)：Claude 内置子 agent 和 agent team teammate 空闲时能否被唤醒 | Windows、Claude Code 2.1.312。后台 `wait` 完成可唤醒内置子 agent；in-process teammate 需要前台 `wait`，空闲时 Monitor 也无法唤醒它。[claude-code#91503](https://github.com/anthropics/claude-code/issues/91503) |
| [宿主通知](backends.md#宿主通知)：Codex 推送通道，以及 TUI 退出后会话的卸载时间 | `codex_host_push.py`；TUI 退出后的 `thread/loaded/list`，覆盖共享 daemon 0.160.0 和 `--remote` 连接的 app-server 0.164.0。[config_toml.rs](https://github.com/openai/codex/blob/c1382380de69521303b416720a52f42d51af6248/codex-rs/config/src/config_toml.rs#L346-L348)、[config_toml.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/config/src/config_toml.rs#L349-L352) |
| [宿主通知](backends.md#宿主通知)：Codex 用户消息的 `clientUserMessageId` 按 `clientId` 读回 | `codex_client_message_id.py`，Windows、Codex 0.167.0。start、steer 和 queue 消息在轮末可按 `clientId` 逐字读回，跨进程及 resume 后仍有效。steer 刚被接受时可能尚未入列表。字段自 0.136.0 源码起存在。[turn_processor.rs](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/app-server/src/request_processors/turn_processor.rs#L618)、[thread_history.rs](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/app-server-protocol/src/protocol/thread_history.rs#L517) |
| [宿主通知](backends.md#宿主通知)：只开关 feature 的 `-c` 不让 TUI 离开共享 daemon | 源码 Codex 0.164.0 [daemon_startup.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/tui/src/daemon_startup.rs#L90-L126) |
| [宿主通知](backends.md#宿主通知)：Codex 门铃，包括宿主自行 `inbox` 后撤回提醒；app-server 客户端会话的来源为 `vscode` | `codex_doorbell.py`；Windows、Codex 0.169.0 实测：宿主收件后队列提醒被撤回。撤回失败时 ledger 保留队列 ID，daemon 重启后重试；原生队列已无该项时清理本地 ID |
| [宿主通知](backends.md#宿主通知)：Codex 插件 hook 在工具调用之后提醒，隐藏的工具仍可由 hook 调用；不调工具时由本轮之后的门铃提醒 | `codex_hooks.py` 验证 hook 与 MCP 的线程配置；桌面端实测覆盖已安装、已信任插件。自主收件的模型实测使用了服从调用间指令的提示 |
| [宿主通知](backends.md#宿主通知)：Grok monitor 在工具调用之后和空闲时送达 `watch` 的输出，以及它的速率和行长限制；monitor 启动的命令没有 `GROK_SESSION_ID` | `grok_monitor.py`。会话变量依据 monitor 与终端子进程的环境差异；送达实测使用了服从调用间指令的提示；字节数与速率上限依据源码。[Grok monitor rate_limiter.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/monitor/rate_limiter.rs)、[event.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/monitor/event.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：skill 的读取位置 | Grok 在隔离的主目录中用 `grok inspect` 实测；文档 [Claude skills](https://code.claude.com/docs/en/skills)；源码 [Codex host_roots.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/skills/src/host_roots.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：Codex 插件的安装、卸载和卸载后的残留，包括 hook 的信任记录和内容基线 | 手动实验：在临时 `CODEX_HOME` 中和桌面端分别安装、信任和卸载，再运行 `axiopera plugin cleanup`；源码 Codex 0.164.0 [plugin_trust.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_trust.rs#L51-L117) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：卸载不结束已打开会话中的 hook 进程 | 手动实验：桌面端 |
| [宿主通知](backends.md#宿主通知)：VS Code 的 hook 支持与插件加载 | 维护者确认 [openai/codex#17930](https://github.com/openai/codex/issues/17930)；Codex 0.164.0 [插件加载](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/core-plugins/src/manager.rs#L698-L702)、[hook 加载](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/core-plugins/src/manager.rs#L1004-L1023) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：TUI 启动时的 hook 确认，以及需要重新信任的条件 | 源码 Codex 0.163.0 [startup_hooks_review.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/tui/src/startup_hooks_review.rs)、[discovery.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/hooks/src/engine/discovery.rs#L666)；0.164.0 [plugin_trust.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_trust.rs)、[plugin_fingerprint.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_fingerprint.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：hook 的开销 | 手动实验：直接调用 debug 构建的 `axiopera mcp --codex-hooks`，daemon 未运行和运行时各 20 次 |
| [宿主通知](backends.md#宿主通知)：队列的检查间隔和分发规则 | 源码 [Codex queue service.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/queue/src/service.rs#L89-L245) |
| [宿主通知](backends.md#宿主通知)：Claude Code VS Code 扩展的后台 `wait` 唤醒；Codex 桌面端和 VS Code 扩展的私有 app-server；桌面端处理外部排入的队列消息 | 手动实验 |
| [宿主通知](backends.md#宿主通知)：不向 Claude inbox socket 推送 | 手动实验；文档 [Claude 跨会话消息](https://code.claude.com/docs/en/cross-session-messaging) |
| [宿主通知](backends.md#宿主通知)：Windows 上 Codex 的退出码 | 手动实验；源码 [Codex shell.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/shell.rs#L34-L40) |
| [daemon 生命周期](design.md#daemon-生命周期)：Windows 进程存活、Job 取消和命名管道权限 | `daemon_job.py`、`ipc_sandbox.py`、`winproc/` |
| [身份与 IPC](design.md#身份与-ipc)：Codex Windows sandbox 中的进程不能为 daemon 的管道另建实例，抢先创建的管道被客户端拒绝；sandbox 中的新旧客户端仍能连接 | `pipe_squat.py`，Windows、Codex 0.166.0；覆盖服务端进程身份与管道实例权限 |
| [身份与 IPC](design.md#身份与-ipc)：Linux 客户端在 sandbox 中启动 daemon 的条件 | WSL2。Grok 1.0.52 允许工作区内的 daemon 启动并在宿主退出后继续运行。Codex 0.162.1 的 read-only、workspace 模式均阻止 socket 连接。Claude 2.1.296 默认阻止连接；允许 Unix socket 且数据目录在工作区内时，daemon 跟随 PID 命名空间退出。macOS 的 Claude sandbox 启动行为依据用户反馈 |
| [指定调用方](usage.md#指定调用方)、跨机器的[宿主通知](backends.md#宿主通知)：会话变量传不过边界时用 `--caller` 分开收件，Claude 的后台 `wait` 和 Grok monitor 照常送达；子 agent 的 `--caller` 离不开所在任务 | Ubuntu 24.04、Axiopera 0.3.2 的 SSH 用户反馈；Windows Claude 2.1.322 和 Grok 1.0.67 经 WSL2 的实测。显式调用方隔离收件，Claude 后台 wait 可唤醒，Grok monitor 在工具调用之间送达；子任务仍受原任务的身份和档位限制 |

问题回执和回答队列由 `question_receipts_match_the_task_answer_route` 覆盖；Grok 宿主及子任务的收件指引由 `grok_spawn_receipts_use_the_callers_monitor` 覆盖。

## Antigravity CLI 1.5.11

Windows 11 上的原生 CLI 与 Axiopera 任务验证结果如下。

| 范围 | 结果 |
|---|---|
| 个人命令禁令 | 对同一条按名称禁止的命令，`agy -p` 报告命中 `permissions.deny`。`agy acp --process-per-session` 在默认权限模式下，无论有无该规则都发出 `session/request_permission`。探针在两种条件下均回复拒绝；1.5.7 实测命令和文件操作经批准后执行。 |
| 任务权限与续接 | read 可用 `list_directory` 查找未知文件、用 `read_file` 读取，并通过会话 MCP 调用 `answers`。目录工具拒绝指向工作区外的 Windows 目录联接。已有只读会话续接后也能使用目录工具。full 可写文件，可在原会话切至 work；切入 read 需要新任务。运行中的任务可取消。 |
| 中途消息与宿主收件 | work 子任务可在本轮采纳 `send`，轮末未读数为零。宿主的后台 `wait` 可唤醒空闲会话；工作中的宿主可在相邻工具调用间用 `inbox` 取结果。Windows auto 沙箱中的 `wait` 和 `inbox` 需要获准在沙箱外运行。 |
| 原生子会话 | work 父任务能在首轮结束后交付子任务结果。审批包含子会话 ID 和完整命令；双子会话的批准、拒绝和取消各自独立。模拟 ACP 覆盖并发创建部分失败、创建回复与子会话记录乱序、父会话答复先于子会话记录可读时的唤醒。 |
| 模型与强度 | 同一目录下，原生终端使用 Hydrogen / High，原生 ACP 新会话使用 Argon / Medium。Axiopera 的查询及新任务继承可用的终端偏好；显式指定的模型和强度生效。跨进程续接保留任务原值。模拟 ACP 覆盖终端偏好被拒时的启动和目录回退。 |
| 模型目录 | 文本和 JSON 均按模型列出强度。实测目录中，Argon、Flash、Opus、Sonnet 有 low、medium、high，Hydrogen 另有 xhigh，Orchestrator、GPT-OSS 无独立强度菜单。逐模型查询保留派发默认值。 |

## 探针

探针调用真实 CLI，会消耗 token，只在需要验证真实行为时手动运行。各脚本顶部说明参数。

需要 Python 3.12 或更高版本，并安装、登录待测 CLI。探针从 PATH 定位程序；Windows 的 Claude/Codex npm 入口会解析为原生 exe。自定义安装可以通过 `AXIOPERA_PROBE_CLAUDE`、`AXIOPERA_PROBE_CODEX`、`AXIOPERA_PROBE_GROK` 指定可执行文件路径。

调用 Axiopera 的探针使用 `target/debug/axiopera`，Windows 使用对应 `.exe`，先运行 `cargo build --locked`。交互终端探针在 Windows 上需要 `python -m pip install pywinpty`。`daemon_job.py`、`ipc_sandbox.py` 和 `pipe_squat.py` 仅用于 Windows，先构建辅助程序：

```sh
cargo build --release --locked --manifest-path spikes/winproc/Cargo.toml
```

例如：

```sh
python spikes/host_e2e.py claude
python spikes/message_receipts.py codex
python spikes/read_mcp.py grok
```

手动验证 daemon 时，用临时 `AXIOPERA_HOME` 隔离数据目录。测试结束后停止该 daemon，删除测试目录和测试创建的原生会话。`probe.forget_sessions` 清理指定测试工作目录下的 Claude、Grok 会话；Codex 会话需按探针取得的 thread ID 清理，`resume.py` 有 `thread/delete` 示例。

协议排查可在启动测试 daemon 前设置 `AXIOPERA_TRACE_PROTOCOL=1`。原始收发内容写入 `tasks/<id>/protocol.log`，可能包含任务内容和工具参数。测试结束后连同测试数据目录删除。
