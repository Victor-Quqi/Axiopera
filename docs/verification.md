# 验证与依据

[后端行为](backends.md)的验证范围、依据与探针用法。

## 已测版本

| 平台 | 已验证范围 | CLI 版本 |
|---|---|---|
| Windows 11 | 跨宿主派发、消息、续接与取消 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| Windows 11 | read 档 MCP、Codex elevated sandbox、消费确认、宿主通知和插件 | Claude Code 2.1.302 至 2.1.312、VS Code 扩展 2.1.301；Codex 0.163.0、0.164.0，共享 daemon 0.160.0，桌面端内置 0.162.0-alpha.17.2、0.163.0-alpha.10.2；Grok 1.0.62、1.0.67 |
| Windows 11 | [个人命令禁令、AGY 派发与宿主收件](#antigravity-cli-1511) | Antigravity CLI 1.5.7、1.5.11 |
| Windows 11 | 任务派发、原会话冷续接、宿主 TUI 收件、原生子任务、[模型默认值与 `top` 实际画面](#pi-与-opencode-的模型和任务显示) | Pi 1.6.0、OpenCode 2.3.2 |
| Linux，WSL2 Ubuntu 24.04 | 三家原生 Linux CLI 的端到端流程、sandbox 回连、进程组取消和 Codex 宿主推送 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| Linux，WSL2 Ubuntu 24.04.4 | Pi full、OpenCode read/full 的工具、轮中追加与续接，两个原生 TUI 的忙闲收件，模型默认值与实际请求 | Pi 1.6.0、OpenCode 2.3.2 |
| macOS | 有平台实现，真实 agent、sandbox 检测及回连尚未实测 | 未验证 |

最低支持版本尚未确定。

## 依据

Windows 上，Pi 1.6.0 自带 subagent 示例、`pi-subagents` 0.89.2 和 `@tintinweb/pi-subagents` 0.26.2 的子进程各有自己的 Pi 会话 ID。两个社区插件在父轮结束后继续处理后台子任务，Axiopera 保留进程，记录插件唤醒的后续轮次。`pi-subagents` 的 delegate 按回执在前台等待并取回自己派发的结果。父 TUI 随后派发任务，扩展仍会提醒，父 TUI 只取回自己的结果。Linux 上的 Pi 官方 subagent 示例也验证了子会话的前台等待回执。社区插件探针为 `python spikes/pi_extensions.py <pi> subagents`。

`python spikes/pi_extensions.py <pi> host` 在 Windows 和 Linux 的真实 Pi TUI 中覆盖安装、更新、卸载、冷启动首次派发、`send` 和 `reply` 回执、忙闲收件、重载去重，以及卸载后前台等待并取回结果。Windows 上，扩展在线时模型从提醒中读到收件命令。卸载并重载后，模型从前台 `wait` 的输出读到该命令，再单独调用 `inbox`。无关对话没有收到收件指令，也没有启动 daemon。

Windows 上，Pi full 和 OpenCode read/full 使用 `gpt-6.1-terra` 验证文件读取、full 写入、同会话恢复随机文件内容，以及输出原生接管命令。OpenCode 两档均验证了 Axiopera daemon 重启后的续接。另已实测 OpenCode 的取消、原生子任务唤醒和接管。

`python spikes/pi_rpc.py <pi>` 验证模型目录、权限档位、文件写入、轮中追加、取消和原会话续接。`python spikes/opencode_managed.py <opencode>` 验证 read 的七个工具、full 的单次审批与拒绝、轮中追加和续接。

`python spikes/opencode_host.py <opencode>` 在真实 TUI 中验证后台 `wait` 唤醒空闲宿主、忙时当前前台命令继续执行，以及随后 `inbox` 取回结果。Windows 上，OpenCode 2.3.2 的 `spawn`、`send`、`reply` 回执说明何时等待，模型随后从 shell 完成通知中读到收件命令。`node spikes/pi_watch.mjs` 验证 Pi 监听重连、重放去重、宿主强退回收、daemon 空闲退出后重新发现，以及同一 Pi 会话切换 Axiopera 数据目录后收到首条提醒。

下表脚本名均指 `spikes/` 下的探针。

| 结论 | 依据 |
|---|---|
| [AGY 中途消息](backends.md#会话与消息) | Windows、AGY 1.5.2，接受追加消息时旧 prompt 提前返回 `end_turn`，等待审批时新 prompt 返回 `-32603`。本地协议测试覆盖两种应答、拒收后的持久化和下一轮交付。 |
| [模型查询](backends.md#模型查询)：三家原生目录查询及推理强度字段；Codex 无效强度错误附模型列表；不带 `--model` 的任务按派发默认值运行，带 `--model`、`--effort` 的任务按请求运行，`status` 和每轮结果首行显示实际模型 | Windows，`models.py` 验证原生目录；`models.py codex --errors` 验证非法强度错误；`models.py --tasks` 验证派发默认值及显式覆盖。目录覆盖 Claude 2.1.309、Codex 0.164.0、Grok 1.0.67，派发覆盖 Claude 2.1.316、Codex 0.166.0、Grok 1.0.67。Claude 2.1.312 的 JSON 目录含 `resolvedModel`。Grok 的会话默认模型受服务端 campaign 影响。[campaigns.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/util/config/campaigns.rs#L14-L56)、[session_setup.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/mvp_agent/session_setup.rs#L516-L535)、[resolution.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/remote_config/resolution.rs#L135-L232)、[manager/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/remote_config/manager/mod.rs#L1225-L1375) |
| [会话与消息](backends.md#会话与消息)：实际模型与推理强度的来源 | Windows。Claude 2.1.316 的 `get_settings.applied` 随显式模型、强度变化，可在首轮前读取；`model_fallback` 字段依据二进制。Codex 0.166.0 的 `thread/start` 与 `config/read` 一致，`model/rerouted` 的作用域和强度回退依据 schema 与源码。Grok 1.0.67 用 `model_changed` 报告配置变化，`session/prompt` 的 `_meta.modelId` 与会话文件一致。[session/mod.rs](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/core/src/session/mod.rs#L4348-L4375)、[step_settings.rs](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/core/src/session/step_settings.rs#L76-L87) |
| [会话与消息](backends.md#会话与消息)：Grok ACP 会话忽略 `-m`、`--reasoning-effort`，`session/set_config_option` 生效，无效值返回 `-32602` | Windows、Grok 1.0.67 的 prompt 回复、`model_changed` 和会话文件。环境变量 `GROK_DEFAULT_MODEL` 也不覆盖新会话模型。该版本原生目录只有 `grok-build-plan`，跨 harness 重建 agent 的行为依据源码，read 档拒绝路径由单元测试覆盖。[model_switch.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs#L60-L175)、[mvp_agent/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/mvp_agent/mod.rs#L935-L948) |
| Claude Code、Codex 和 Grok 宿主从 `guide --host` 开始完成派发、收结果、追问和取消 | `host_e2e.py` |
| [宿主通知](backends.md#宿主通知)：`grok -p` 等 persistent monitor 结束才退出，`--no-wait-for-background` 让它在本轮结束时退出 | `host_e2e.py grok` |
| [权限](backends.md#权限)：Claude read 档按注解过滤 MCP 工具，Grok ACP 提供的工具信息和禁用粒度 | `read_mcp.py` |
| [权限](backends.md#权限)：Grok 权限模式和工具裁剪 | `grok_perms.py`、`grok_profile.py` |
| [权限](backends.md#权限)：回连差异表中 Codex Windows 两行 | `ipc_sandbox.py`，elevated 加 `-c windows.sandbox="elevated"`；手动实验：在 elevated sandbox 中运行 `axiopera daemon status` |
| [权限](backends.md#权限)：回连差异表中 Linux 三行 | 手动实验：在三家原生 Linux sandbox 内调用 `axiopera` |
| [权限](backends.md#权限)：Codex work 档添加 ACE；MCP 服务器设为 `approve` 后免审批 | 手动实验 |
| [权限](backends.md#权限)：Codex auto_review 的审查模型，审查失败即拒绝；未配置 Windows sandbox 时 `workspace-write` 降为 `read-only` | 源码 [provider.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/model-provider/src/provider.rs#L91-L93)、[model.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/guardian-reviewer/src/model.rs#L18-L76)、[completion.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/guardian-reviewer/src/completion.rs#L127-L159)、[config_toml.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/config/src/config_toml.rs#L821-L839)；`auto_review_model_override` 的绕法和 `review_model` 无效见 [openai/codex#47916](https://github.com/openai/codex/issues/47916) |
| [权限](backends.md#权限)：Claude auto 分类器的模型选择和失败处理 | 文档 [permission-modes](https://code.claude.com/docs/en/permission-modes#default-classifier-model)、[errors](https://code.claude.com/docs/en/errors#auto-mode-cannot-determine-the-safety-of-an-action)；Claude Code 2.1.302 二进制中的日志字符串 |
| [权限](backends.md#权限)：Grok auto 分类器使用会话模型，失败或超时转为审批，无法解析时拒绝 | 源码 [sampler_turn.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs#L830-L873)、[manager/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs#L1063-L1078)、[auto_mode/mod.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-workspace/src/permission/auto_mode/mod.rs#L1480-L1499) |
| [审批](backends.md#审批)：不使用持久化批准选项 | `codex_escalate.py`：Codex 的 amendment 会写入用户规则，`acceptForSession` 只缓存完全相同的命令 |
| [审批](backends.md#审批)：Claude、Codex 和 Grok 自行拒绝时的返回形式 | `native_block.py` |
| [审批](backends.md#审批)：Codex 撤回请求 | Windows、Codex 0.167.0。命令审批收到 `decline` 后，`serverRequest/resolved.requestId` 与原 JSON-RPC 请求 ID 一致。网络请求在裁决前结束时的撤回行为依据上游源码。[bespoke_event_handling.rs](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/app-server/src/bespoke_event_handling.rs#L836-L846)、[outgoing_message.rs](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/app-server/src/outgoing_message.rs#L232-L241) |
| [会话与消息](backends.md#会话与消息)：Claude read 档需另加 `--permission-prompt-tool stdio` | `read_mcp.py` |
| [会话与消息](backends.md#会话与消息)：中途消息和消费确认 | `steer.py`、`message_receipts.py`；源码 [Codex tasks/mod.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/tasks/mod.rs)、[Grok interjection.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) |
| [任务与工作目录](usage.md#任务与工作目录)：`send --tier` 保留会话并切换权限 | `tier_resume.py`，Windows、Claude Code 2.1.312、Codex 0.165.0、Grok 1.0.67。从 read 切至 full 再切回 read，会话 ID 一致，只有 full 档能写文件。档位持久化、父子档位约束和运行中切档由单元测试覆盖 |
| [会话与消息](backends.md#会话与消息)：Grok 空闲时的消息，以及一次 prompt 内的多段答复 | `grok_idle_interject.py`、`grok_response_boundary.py` |
| [会话与消息](backends.md#会话与消息)：Grok 工具名、参数摘要及后续更新 | 上游工具事件与 MCP 输入结构。适配器、daemon 和 `top` 单元测试覆盖参数提取、MCP 展开、更新去重和调用计数。[tool_calls.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs)、[use_tool/input.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/use_tool/input.rs) |
| [会话与消息](backends.md#会话与消息)：Claude 和 Grok 在后台命令结束后自行开启一轮 | `background.py` |
| [结果与接管](usage.md#结果与接管)：审批请求先于更早的结果交出，交出前不能批准；过长的提问和留言只给首尾，全文写入文件；agent 自行开启的一轮从开始就计入进度 | `delivery.py` |
| [会话与消息](backends.md#会话与消息)：Claude、Codex 和 Grok 的并发续接 | `resume.py` |
| [会话与消息](backends.md#会话与消息)：Grok 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `grok_native_identity.py`，Windows、Grok 1.0.67。原生 auto 宿主和 Axiopera work 宿主下，主会话均无法取走子会话的结果 |
| [会话与消息](backends.md#会话与消息)：Codex 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `codex_native_identity.py`，Windows、Codex 0.164.0。原生 full 权限宿主（`danger-full-access`）和 Axiopera work 宿主下，主会话均无法取走子线程的结果 |
| [会话与消息](backends.md#会话与消息)：Codex 内置子线程先结束时的结果和中途消息 | Windows、Codex 0.171.0，read 档实测。主线程用 `wait_agent` 等待子线程。子线程答完时任务仍在运行，收件箱无结果；此时 `send` 通过 `turn/steer` 进入原轮。该轮只交付一次，答复和用量均来自主线程。单元测试另覆盖子线程输入回显及跨线程同条目 ID 的文件审批。 |
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

## Pi 与 OpenCode 的模型和任务显示

Windows 11 上，用 Axiopera 0.5.0、Pi 1.6.0 和 OpenCode 2.3.2 在同一临时项目分别启动原生 TUI 和 Axiopera 任务。模型服务在本机记录每次请求的模型 ID 和推理强度，再与 `axiopera models`、`status` 和 `inbox` 对照。以下无参数派发指省略模型和强度。权限档位另测：Pi 和 OpenCode 在省略 `--tier` 或指定 `work` 时都返回退出码 1，提示各自支持的档位，四次均未创建任务。

### Pi

`python spikes/pi_models.py <pi>` 把 `models pi` 的文本、JSON 和原生 RPC 逐模型对照。三种模型覆盖常规强度菜单、自定义菜单和无推理模型。无推理模型只列 `off`，实际请求省略推理参数。自定义菜单中的 `max` 按 provider 映射发送为 `maximum`。查询模型列表没有发出推理请求。

保存的默认模型为 `research`、全局强度为 `medium`、该模型单设 `high` 时，新 TUI 和无参数派发都使用 `research/high`。在仍然打开的 TUI 中用 `/model` 和 `/thinking` 切到 `draft/low` 后，该会话实际使用 `draft/low`，新派发仍用 `research/high`。只传 `--model fixture/draft` 或在 Axiopera 配置中指定该模型时，新任务使用 `draft/medium`。后一种情况下 `models pi` 首行只报配置的模型，省略实际生效的 `medium`。只传 `--effort` 和同时传两项时，任务也按指定值运行。[Pi 新会话选择](https://github.com/earendil-works/pi/blob/faac585884ae2c013a6b1400f3e9e77f10b34bac/packages/coding-agent/src/core/sdk.ts#L219-L277)、[TUI 切换模型和强度](https://github.com/earendil-works/pi/blob/faac585884ae2c013a6b1400f3e9e77f10b34bac/packages/coding-agent/src/modes/interactive/interactive-mode.ts#L5386-L5450)

新建无凭据的 Pi 环境中，原生 `--list-models` 报告目录为空。`models pi` 同样返回空目录，但首行把 `get_state` 的占位值 `unknown/unknown, effort off` 写成了派发默认值。实际派发因缺少凭据失败。[Pi 初始状态](https://github.com/earendil-works/pi/blob/faac585884ae2c013a6b1400f3e9e77f10b34bac/packages/agent/src/agent.ts#L58-L68)

使用异步发现模型的 provider 扩展时，原生 `pi --list-models`、刚启动的 RPC 和 `models pi` 都只列出扩展的初始模型。刷新完成后同一 RPC 能选中新发现的模型，可用强度含 `high`。在刷新完成前显式派发该模型，实际请求用了正确的模型 ID，但 Pi 沿用了初始模型的能力，只能用 `off`。另一个 provider 完全依靠异步发现，没有初始模型；对它做同样的派发，任务启动失败，模型服务收到零次请求。[Pi 后台刷新](https://github.com/earendil-works/pi/blob/faac585884ae2c013a6b1400f3e9e77f10b34bac/packages/coding-agent/src/main.ts#L1020-L1032)、[Pi 对未列出模型的处理](https://github.com/earendil-works/pi/blob/faac585884ae2c013a6b1400f3e9e77f10b34bac/packages/coding-agent/src/core/model-resolver.ts#L301-L312)

### OpenCode

`python spikes/opencode_models.py <opencode>` 比对原生 TUI、服务目录和实际派发。原生 `opencode models --standalone` 列出 `fixture/alpha`、`fixture/beta`、`fixture/gamma` 三个 ID。`axiopera models opencode` 列出相同的 ID，各自的 `low`、`high` 也与原生服务目录一致。下表记录省略 `--model` 和 `--effort` 时的结果，模型名省略 `fixture/`。

| 原生设置 | 新 TUI 实际请求 | `models` 首行 | Axiopera 派发实际请求 |
|---|---|---|---|
| 项目指定 `alpha#high` | `alpha/high` | `alpha` | `alpha`，未传强度 |
| 项目指定 `alpha#high`，默认主 agent 指定 `beta#low` | `beta/low` | `alpha` | `alpha`，未传强度 |
| 同上，TUI 还保存了 `beta` 的 `high` | `beta/high` | `alpha` | `alpha`，未传强度 |
| 只保留 TUI 最近选择的 `gamma/high` | `gamma/high` | `alpha` | `alpha`，未传强度 |

OpenCode TUI 创建会话时把选好的模型和 variant 一起写入会话。Axiopera 0.5.0 创建会话时只指定主 agent，`models` 首行取服务的 `/api/model/default`。该接口返回目录中的模型条目。原生服务直接用 `build` 建会话时也会省略项目的 variant。单独指定模型或在 Axiopera 配置中指定模型时，模型生效，实际请求省略 variant；同时指定 `--model` 和 `--effort` 时两项均生效。[TUI 默认值](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/tui/src/context/local.tsx#L173-L253)、[TUI 创建会话](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/tui/src/component/prompt/index.tsx#L1234-L1242)、[服务端选择模型](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/core/src/session/runner/model.ts#L89-L99)

另有一处冷启动时序差异。Axiopera 选用 `/api/agent` 首次返回的可见主 agent。两个独立服务的首个非空列表都以 `build` 开头，等待 `/api/model` 返回后，列表首项都变成项目指定的 `focus`。用 Axiopera 任务的原生会话 ID 核对，read 和 full 均已保存为 `build`。给 `focus` 增加专属提示词后，原生 TUI 和就绪后创建的 `focus` 会话都把它发给模型；Axiopera 的 read 和 full 请求均缺少这条提示词。上游的模型目录接口会等待插件注册，agent 列表接口直接返回当前列表。[agent 列表](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/server/src/handlers/agent.ts#L8-L15)、[模型目录](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/server/src/handlers/model.ts#L10-L24)、[模型请求中的主 agent 指令](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/core/src/session/model-request.ts#L116-L124)

WSL2 Ubuntu 另用 Linux 发布版复测。两个工作目录分别指定 `alpha`、`beta`，查询和派发各自使用项目模型。第三个项目指定 `alpha`，主 agent 指定 `beta/high`，派发使用 `alpha`；只传 `--effort low` 后，实际请求和 `status` 都是 `alpha/low`。同环境下 Pi 保存的 `beta/low` 也与派发请求一致。

### top

Windows 终端中，150 列可看见两家任务的完整模型、强度和状态。80 列时标题优先保留状态。40 列时，有名称的任务详情仍保留名称、ID 和权限档位，Pi 的 `running` 只剩 `run…`，OpenCode 的 `approval` 整个被裁掉。总览和详情正文中仍能看到审批请求。宽屏下也有一处信息缺失：读取同一个 `sample.txt`，Pi 工具行显示 `Read sample.txt`，OpenCode 只显示 `Read`；写文件和运行命令也缺少操作对象。OpenCode 在后续 `session.tool.called` 事件中提供完整参数。[标题布局](../src/top.rs)、[OpenCode 适配器](../src/agent/opencode.rs)、[OpenCode 工具事件](https://github.com/anomalyco/opencode/blob/c0ad18b402eb2827fc1ee71c171b1177b3fc02d1/packages/schema/src/session-event.ts#L458-L496)

OpenCode 后台 shell 和 Pi 的 `@tintinweb/pi-subagents` 0.26.2 都在首轮结束后显示 `running` 及后台仍在工作的提示，完成后自动进入第二轮，最终显示 `idle` 和两轮结果。

## 探针

Python 探针使用 Python 3.12 或更高版本。Pi 和 OpenCode 的 Python 探针以待测 CLI 路径为首个参数，在临时目录中使用本机模型服务，无需模型凭据。`pi_extensions.py` 的 `host` 模式使用 Git，`subagents` 模式会从 npm 下载两个固定版本的插件。`pi_watch.mjs` 只需 Node.js 24 或更高版本和编译好的 Axiopera。

Claude、Codex 和 Grok 探针默认查找本机 CLI，调用在线模型的用例需要已登录的 CLI 或有效模型凭据，并消耗对应额度。可用 `AXIOPERA_PROBE_CLAUDE`、`AXIOPERA_PROBE_CODEX`、`AXIOPERA_PROBE_GROK` 指定可执行文件路径。各脚本顶部列出参数。

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
