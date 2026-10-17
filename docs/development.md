# 开发

## 代码入口

| 文件 | 职责 |
|---|---|
| `src/main.rs`、`src/guide.md` | CLI 参数、agent 使用说明 |
| `src/client.rs`、`src/ipc.rs` | 客户端和 IPC 报文 |
| `src/daemon/mod.rs`、`task.rs` | 请求处理、任务调度和轮次驱动 |
| `src/daemon/push.rs`、`render.rs` | 宿主推送和输出格式 |
| `src/agent/` | 原生 CLI 协议适配 |
| `src/mcp.rs` | 子 agent 的 ask、tell、inbox 工具 |
| `src/platform/` | 进程树、IPC 和 sandbox 检测 |
| `src/store.rs`、`worktree.rs` | 任务存储和 Git worktree |
| `src/config.rs`、`paths.rs`、`identity.rs` | 配置、路径和调用方身份 |

## 构建与检查

在仓库根目录运行：

```sh
cargo fmt --check
cargo test --locked
cargo build --release --locked
cargo package --locked
```

普通测试使用本地临时数据，不调用模型。当前测试覆盖消息解析、消费确认、任务状态、结果游标、配置和平台辅助函数。CI 在 Windows、Linux 和 macOS 上运行上述检查，并按 `Cargo.toml` 的 `rust-version` 检查最低 Rust 版本。

从源码安装使用 `cargo install --path . --locked`。

## 真实 agent 探针

`spikes/` 调用真实 CLI，会消耗 token，只在验证协议行为时手动运行。各脚本顶部说明参数。已验证的 CLI 版本和平台集中在[后端兼容性](backends.md#兼容性)。

需要 Python 3.12 或更高版本，并安装、登录待测 CLI。探针从 PATH 定位程序；Windows 的 Claude/Codex npm 入口会解析为原生 exe。自定义安装可以通过 `AXIOPERA_PROBE_CLAUDE`、`AXIOPERA_PROBE_CODEX`、`AXIOPERA_PROBE_GROK` 指定可执行文件路径。

调用 Axiopera 的探针使用 `target/debug/axiopera`，Windows 使用对应 `.exe`，先运行 `cargo build --locked`。交互终端探针在 Windows 上需要 `python -m pip install pywinpty`。`daemon_job.py` 和 `ipc_sandbox.py` 仅用于 Windows，先构建辅助程序：

```sh
cargo build --release --locked --manifest-path spikes/winproc/Cargo.toml
```

| 探针 | 验证内容 |
|---|---|
| `host_e2e.py` | 主 agent 仅凭 guide 完成派发、收结果、追问和取消 |
| `steer.py`、`message_receipts.py` | 中途消息、连续输入和最终输出期间的消费确认 |
| `resume.py` | 原生续接、会话身份和并发写入 |
| `background.py` | 后台命令结束后自行开启一轮 |
| `codex_host_push.py` | 交互宿主的空闲、运行和审批推送 |
| `read_mcp.py` | MCP 注解和 read 档工具过滤 |
| `native_block.py` | 原生权限拒绝的返回形式 |
| `grok_perms.py`、`grok_profile.py` | Grok 权限模式和工具裁剪 |
| `grok_idle_interject.py`、`grok_response_boundary.py` | 空闲消息和同一轮内的多段答复 |
| `codex_escalate.py` | Codex 提权应答的作用范围 |
| `daemon_job.py`、`ipc_sandbox.py`、`winproc/` | Windows 进程存活、取消和命名管道权限 |

例如：

```sh
python spikes/host_e2e.py claude
python spikes/message_receipts.py codex
python spikes/read_mcp.py grok
```

手动验证 daemon 时，用临时 `AXIOPERA_HOME` 隔离数据目录。测试结束后停止该 daemon，删除测试目录和测试创建的原生会话。`probe.forget_sessions` 清理指定测试工作目录下的 Claude、Grok 会话；Codex 会话需按探针取得的 thread ID 清理，`resume.py` 有 `thread/delete` 示例。

协议排查可在启动测试 daemon 前设置 `AXIOPERA_TRACE_PROTOCOL=1`。原始收发内容写入 `tasks/<id>/protocol.log`，可能包含任务内容和工具参数。测试结束后连同测试数据目录删除。

## 修改约定

行为说明按用途维护，各处通过链接引用：

- `README.md` 与 `README.zh-CN.md` 负责用户介绍、安装和快速上手，两种语言同步维护。
- `src/guide.md` 是 `axiopera guide` 的输出，面向主 agent，保留完成协作所需的操作说明。
- `docs/usage.md` 负责用户配置、任务操作和维护。
- `docs/backends.md` 负责权限映射、协议差异及版本证据。新增行为事实标注〔实验〕或〔源码〕，附版本与探针或来源；未验证的内容明确标注。
- `docs/design.md` 负责内部状态、持久化和并发机制。

修改命令时同步 guide；修改后端行为时同步后端文档。删除过时表述，把同一事实的条件和例外放在一起。探索记录留在忽略目录 `.local/`。

代码注释用英文，解释代码本身看不出的原因。实现保持直接，优先修复可确认的缺陷。提交使用 Conventional Commits，探针改动使用 `test(spikes)`。
