# 验证与依据

[后端文档](backends.md)只写结论。这些结论来自真实 CLI 探针、手动实验或上游源码，受版本限制。本文集中记录已测版本、每条结论的依据和探针用法。

## 已测版本

| 平台 | 已验证范围 | CLI 版本 |
|---|---|---|
| Windows 11 | 三家互相派发、中途消息、续接、取消；read 档 MCP、消息确认、Codex elevated sandbox、宿主通知另有补测 | 基础端到端为 Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52；补测为 Claude Code 2.1.302 及其 VS Code 扩展 2.1.301、Codex 0.163.0（推送所连 daemon 为 0.160.0）、Codex 桌面端内置 0.162.0-alpha.17.2、Grok 1.0.62 |
| Linux，WSL2 Ubuntu 24.04 | 三家原生 Linux CLI 的端到端流程、sandbox 回连、进程组取消和 Codex 宿主推送 | Claude Code 2.1.296、Codex 0.162.1、Grok 1.0.52 |
| macOS | 有平台实现，真实 agent、sandbox 检测及回连尚未实测 | 未验证 |

基础验证在 2026-10-13 至 14 日，补测在 2026-10-17 至 18 日。表中记录已测组合，最低版本尚未确定。

## 依据

脚本名指 `spikes/` 下的探针。

| 结论 | 依据 |
|---|---|
| [已测版本](#已测版本)中的端到端流程：主 agent 仅凭 guide 完成派发、收结果、追问和取消 | `host_e2e.py` |
| [权限](backends.md#权限)：read 档 MCP 工具按注解过滤 | `read_mcp.py` |
| [权限](backends.md#权限)：Grok 权限模式和工具裁剪 | `grok_perms.py`、`grok_profile.py` |
| [权限](backends.md#权限)：回连差异表中 Codex Windows 两行 | `ipc_sandbox.py` |
| [权限](backends.md#权限)：回连差异表中 Linux 三行 | 手动实验：在三家原生 Linux sandbox 内调用 `axiopera` |
| [权限](backends.md#权限)：Codex work 档添加 ACE；MCP 服务器设为 `approve` 后免审批 | 手动实验 |
| [审批](backends.md#审批)：不使用持久化批准选项 | `codex_escalate.py`：Codex 的 amendment 会写入用户规则，`acceptForSession` 只缓存完全相同的命令 |
| [审批](backends.md#审批)：后端自行拒绝时的返回形式 | `native_block.py` |
| [会话与消息](backends.md#会话与消息)：Claude read 档需另加 `--permission-prompt-tool stdio` | `read_mcp.py` |
| [会话与消息](backends.md#会话与消息)：中途消息和消费确认 | `steer.py`、`message_receipts.py`；源码 [Codex tasks/mod.rs](https://github.com/openai/codex/blob/ce0323f2f2d3b25e82806cd555237da3db4e1897/codex-rs/core/src/tasks/mod.rs)、[Grok interjection.rs](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) |
| [会话与消息](backends.md#会话与消息)：Grok 空闲时的消息，以及一次 prompt 内的多段答复 | `grok_idle_interject.py`、`grok_response_boundary.py` |
| [会话与消息](backends.md#会话与消息)：后台命令结束后自行开启一轮 | `background.py` |
| [会话与消息](backends.md#会话与消息)：并发续接 | `resume.py` |
| [宿主通知](backends.md#宿主通知)：Codex 推送通道，以及 TUI 退出后会话约一分钟才卸载 | `codex_host_push.py` |
| [宿主通知](backends.md#宿主通知)：Claude Code VS Code 扩展的后台 `wait` 唤醒；Codex 桌面端和 VS Code 扩展的私有 app-server | 手动实验 |
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
