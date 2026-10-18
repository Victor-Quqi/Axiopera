# 开发

## 代码入口

| 文件 | 职责 |
|---|---|
| `src/main.rs`、`src/guide.md` | CLI 参数、agent 使用说明 |
| `src/client.rs`、`src/ipc.rs` | 客户端和 IPC 报文 |
| `src/daemon/mod.rs`、`task.rs` | 请求处理、任务调度和轮次驱动 |
| `src/daemon/push.rs`、`render.rs` | 宿主推送和输出格式 |
| `src/agent/` | 原生 CLI 协议适配 |
| `src/mcp.rs` | 子 agent 的 ask、tell、inbox 工具，Codex 插件的 hook |
| `src/plugin.rs` | Codex 插件卸载后的清理 |
| `plugins/axiopera/`、`.agents/plugins/` | skill、Codex 插件和 marketplace |
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

## 修改约定

各文档按用途维护，同一事实只在一处陈述，其他位置链接过去：

- `README.md` 与 `README.zh-CN.md`：用户介绍、安装和快速上手，两种语言同步维护。
- `src/guide.md`：`axiopera guide` 的输出，面向主 agent，只保留完成协作所需的操作说明。修改命令时同步。
- `docs/usage.md`：用户配置、任务操作和维护。
- `docs/backends.md`：权限映射和协议差异。修改适配器前先查，修改后端行为时同步。
- `docs/verification.md`：已测版本、后端结论的依据和探针用法。新增或修改后端结论时在此登记依据，正文不标注来源。
- `docs/design.md`：内部状态、持久化和并发机制。修改状态或交付逻辑时同步。
- `plugins/axiopera/`：skill 只让 agent 去读 `axiopera guide`，说明写在 guide 里。改动 Codex hook 会让用户重新信任；插件在 Codex 中留下的内容有变化时，同步 `src/plugin.rs` 和[使用文档](usage.md#skill-与-codex-插件)。

事实的条件和例外与它写在一起；未验证的结论在正文注明。删除过时表述。探索记录留在忽略目录 `.local/`。

代码注释用英文，解释代码本身看不出的原因。实现保持直接，优先修复可确认的缺陷。提交使用 Conventional Commits，探针改动使用 `test(spikes)`。
