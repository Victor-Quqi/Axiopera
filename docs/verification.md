# 验证与依据

[后端文档](backends.md)只写结论。这些结论来自真实 CLI 探针、手动实验或上游源码，受版本限制。本文集中记录已测版本、每条结论的依据和探针用法。

## 已测版本

| 平台 | 已验证范围 | CLI 版本 |
|---|---|---|
| Windows 11 | 三家互相派发、中途消息、续接、取消；read 档 MCP、消息确认、Codex elevated sandbox、宿主通知和 Codex 插件另有补测；Codex 0.164.0 还补测了作为宿主的端到端流程和中途消息 | 基础端到端为 Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52；补测为 Claude Code 2.1.302 至 2.1.309 及其 VS Code 扩展 2.1.301、Codex 0.163.0 和 0.164.0（推送所连 daemon 为 0.160.0）、Codex 桌面端内置 0.162.0-alpha.17.2 和 0.163.0-alpha.10.2、Grok 1.0.62 和 1.0.67 |
| Linux，WSL2 Ubuntu 24.04 | 三家原生 Linux CLI 的端到端流程、sandbox 回连、进程组取消和 Codex 宿主推送 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| macOS | 有平台实现，真实 agent、sandbox 检测及回连尚未实测 | 未验证 |

基础验证在 2026-10-13 至 14 日，补测在 2026-10-17 至 26 日。表中记录已测组合，最低版本尚未确定。

## 依据

脚本名指 `spikes/` 下的探针。

| 结论 | 依据 |
|---|---|
| [模型查询](backends.md#模型查询)：三家原生目录查询及推理强度字段；Codex 无效强度错误附模型列表 | `models.py`、`models.py codex --errors`，Windows，Claude Code 2.1.309、Codex 0.164.0、Grok 1.0.67；查询不发推理请求，错误探针使用 Codex 在 `turn/start` 拒绝的非法强度。分页、空列表、禁用项、超时回收及错误分类另由单元测试覆盖 |
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
| [会话与消息](backends.md#会话与消息)：Claude read 档需另加 `--permission-prompt-tool stdio` | `read_mcp.py` |
| [会话与消息](backends.md#会话与消息)：中途消息和消费确认 | `steer.py`、`message_receipts.py`；源码 [Codex tasks/mod.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/tasks/mod.rs)、[Grok interjection.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) |
| [会话与消息](backends.md#会话与消息)：Grok 空闲时的消息，以及一次 prompt 内的多段答复 | `grok_idle_interject.py`、`grok_response_boundary.py` |
| [会话与消息](backends.md#会话与消息)：后台命令结束后自行开启一轮 | `background.py` |
| [结果与接管](usage.md#结果与接管)：审批请求先于更早的结果交出，交出前不能批准；过长的提问和留言只给首尾，全文写入文件；agent 自行开启的一轮从开始就计入进度 | `delivery.py` |
| [会话与消息](backends.md#会话与消息)：并发续接 | `resume.py` |
| [会话与消息](backends.md#会话与消息)：Grok 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `grok_native_identity.py`，Windows、Grok 1.0.67，2026-10-26：直接启动使用 auto，经 Axiopera 启动使用 work，再各派一个原生子 agent；原生子 agent 实际派发只读 Axiopera 任务，主代理先 `inbox` 未取走结果，子 agent 随后成功收取；另用采集的身份与预置结果检查原任务及子会话的归属 |
| [会话与消息](backends.md#会话与消息)：Codex 原生子 agent 在直接及 Axiopera 混合嵌套中的收件隔离 | `codex_native_identity.py`，Windows、Codex 0.164.0，2026-10-26：直接启动使用 `danger-full-access`，经 Axiopera 启动使用 full，再各派一个原生子 agent；原生子 agent 实际派发只读 Axiopera 任务，主代理先 `inbox` 未取走结果，子 agent 随后成功收取；另用采集的身份与预置结果检查原任务及子线程的归属 |
| [宿主通知](backends.md#宿主通知)：Claude 和 Grok 在本轮进行中能否收到后台命令的完成通知 | `background.py busy` |
| [宿主通知](backends.md#宿主通知)：Codex 推送通道，以及 TUI 退出后会话的卸载时间 | `codex_host_push.py`；手动实验：TUI 退出后轮询 `thread/loaded/list`，daemon 0.160.0 和 `--remote` 连接的 0.164.0 app-server 各一次；源码 0.162.0 [config_toml.rs](https://github.com/openai/codex/blob/c1382380de69521303b416720a52f42d51af6248/codex-rs/config/src/config_toml.rs#L346-L348)、0.164.0 [config_toml.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/config/src/config_toml.rs#L349-L352) |
| [宿主通知](backends.md#宿主通知)：只开关 feature 的 `-c` 不让 TUI 离开共享 daemon | 源码 Codex 0.164.0 [daemon_startup.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/tui/src/daemon_startup.rs#L90-L126) |
| [宿主通知](backends.md#宿主通知)：Codex 门铃，包括宿主自行 `inbox` 后撤回提醒；app-server 客户端会话的来源为 `vscode` | `codex_doorbell.py` |
| [宿主通知](backends.md#宿主通知)：Codex 插件 hook 在工具调用之后提醒，隐藏的工具仍可由 hook 调用；不调工具时由本轮之后的门铃提醒 | `codex_hooks.py`：hook 和 MCP 服务器经线程配置提供；手动实验：在桌面端安装并信任插件后新建会话复测，缓存中插件的 `mcp.json` 临时直接设了 `AXIOPERA_HOME` |
| [宿主通知](backends.md#宿主通知)：Grok monitor 在工具调用之后和空闲时送达 `watch` 的输出，以及它的速率和行长限制；monitor 启动的命令没有 `GROK_SESSION_ID` | `grok_monitor.py`；手动实验：同一命令经 monitor 和终端工具各运行一次；源码 [Grok monitor rate_limiter.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/monitor/rate_limiter.rs)、[event.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/monitor/event.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：skill 的读取位置 | Grok 在隔离的主目录中用 `grok inspect` 实测；文档 [Claude skills](https://code.claude.com/docs/en/skills)；源码 [Codex host_roots.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/skills/src/host_roots.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：Codex 插件的安装、卸载和卸载后的残留，包括 hook 的信任记录和内容基线 | 手动实验：在临时 `CODEX_HOME` 中和桌面端分别安装、信任和卸载，再运行 `axiopera plugin cleanup`；源码 Codex 0.164.0 [plugin_trust.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_trust.rs#L51-L117) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：卸载不结束已打开会话中的 hook 进程 | 手动实验：桌面端 |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：TUI 启动时的 hook 确认，以及需要重新信任的条件 | 源码 Codex 0.163.0 [startup_hooks_review.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/tui/src/startup_hooks_review.rs)、[discovery.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/hooks/src/engine/discovery.rs#L666)；0.164.0 [plugin_trust.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_trust.rs)、[plugin_fingerprint.rs](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/hooks/src/engine/plugin_fingerprint.rs) |
| [Skill 与 Codex 插件](usage.md#skill-与-codex-插件)：hook 的开销 | 手动实验：直接调用 debug 构建的 `axiopera mcp --codex-hooks`，daemon 未运行和运行时各 20 次 |
| [宿主通知](backends.md#宿主通知)：队列的检查间隔和分发规则 | 源码 [Codex queue service.rs](https://github.com/openai/codex/blob/83086188ae0614bd882818ff1aa23f3194962012/codex-rs/ext/queue/src/service.rs#L89-L245) |
| [宿主通知](backends.md#宿主通知)：Claude Code VS Code 扩展的后台 `wait` 唤醒；Codex 桌面端和 VS Code 扩展的私有 app-server；桌面端处理外部排入的队列消息 | 手动实验 |
| [宿主通知](backends.md#宿主通知)：不向 Claude inbox socket 推送 | 手动实验；文档 [Claude 跨会话消息](https://code.claude.com/docs/en/cross-session-messaging) |
| [宿主通知](backends.md#宿主通知)：Windows 上 Codex 的退出码 | 手动实验；源码 [Codex shell.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/shell.rs#L34-L40) |
| [daemon 生命周期](design.md#daemon-生命周期)：Windows 进程存活、Job 取消和命名管道权限 | `daemon_job.py`、`ipc_sandbox.py`、`winproc/` |

## 探针

探针调用真实 CLI，会消耗 token，只在需要验证真实行为时手动运行。各脚本顶部说明参数。

需要 Python 3.12 或更高版本，并安装、登录待测 CLI。探针从 PATH 定位程序；Windows 的 Claude/Codex npm 入口会解析为原生 exe。自定义安装可以通过 `AXIOPERA_PROBE_CLAUDE`、`AXIOPERA_PROBE_CODEX`、`AXIOPERA_PROBE_GROK` 指定可执行文件路径。

调用 Axiopera 的探针使用 `target/debug/axiopera`，Windows 使用对应 `.exe`，先运行 `cargo build --locked`。交互终端探针在 Windows 上需要 `python -m pip install pywinpty`。`daemon_job.py` 和 `ipc_sandbox.py` 仅用于 Windows，先构建辅助程序：

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
