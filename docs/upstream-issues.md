# 上游问题与复测

升级宿主 CLI 时，先在隔离会话中核对对应条目。兼容限制解除时，同步更新[后端文档](backends.md)和[验证依据](verification.md)。

## Antigravity CLI

以下行为在 Windows 11、AGY 1.5.11 上核对。

| 上游行为 | 新版本的通过条件 | 通过后处理 |
|---|---|---|
| 新建 ACP 会话可能忽略交互终端保存的模型和强度。 | 直接启动 AGY 与原生 ACP，在同一目录、同一终端偏好下新建会话，两者都选中相同模型及强度；更换偏好后再测一次。另在新进程中续接旧会话，核对其保留此前实际使用的模型和强度。 | 去掉[适配器](../src/agent/agy.rs)和[模型查询](../src/agent/models.rs)中对终端偏好的匹配与应用，保留显式覆盖。跨进程续接也通过后，可移除续接时重设模型和强度的逻辑。 |
| ACP 会为个人禁令明确禁止的命令请求审批。上游 [#798](https://github.com/google-antigravity/antigravity-cli/issues/798) 跟踪相关的路径规则与优先级问题。 | 同一条命令禁令在 `agy -p` 和 ACP 的适用权限档位都直接拒绝操作。 | 更新[权限说明](backends.md#权限)和发版提醒。 |
| 自定义 agent 的新会话首轮缺少客户端提供的 MCP 工具。相关上游工单见 [#1603](https://github.com/google-antigravity/antigravity-cli/issues/1603)。 | 在默认共享 ACP 进程中新建 read 任务，省去紧接着的 `session/resume`，首轮仍能实际调用 Axiopera 的 `answers`。 | 去掉[新建会话](../src/agent/agy.rs)后的额外 resume，保留已有会话的正常续接。 |
| 自定义只读 agent 即使在工具列表中声明 `list_dir` 和 `find_by_name`，模型仍拿不到这两个工具。 | 在关闭 shell 和任务目录 MCP 的只读会话中，让模型自行找到嵌套目录里一个事先未知的文件名，并读出内容。 | 通过后改用原生工具，移除 Axiopera 的 `list_directory`。 |
| 续接会话沿用创建时的工具列表。上游是否支持更换列表待确认。 | 同一会话 read → work 后能写文件，work → read 后只有只读和任务通信工具；两次切换都保留原会话历史。 | 解除 [`send --tier` 的限制](../src/daemon/mod.rs)，更新 guide 和权限说明。 |
| 主会话的 ACP 事件缺少可用的原生子会话 ID，子会话需要单独载入才能接收审批。 | 仅用 ACP 即可确定两个并发子会话各自的归属，分别允许、拒绝或取消其中一个请求，另一会话的审批和执行结果均正确。 | 在[AGY 适配器](../src/agent/agy.rs)中撤掉协议已覆盖的 ID 查找或子会话载入逻辑。后台轮次与中途消息也使用本地会话记录，需分别验证。 |

## Grok Build

Grok 1.0.67 的源码显示，切换到使用另一 harness 的模型时会重建 agent 并丢失工具配置。该版本的实测模型目录只有一种 harness，Axiopera 的 read 档限制由单元测试覆盖。[验证依据](verification.md#依据)

新版本提供跨 harness 的模型组合后，在尚未开始首轮的 read 会话中切换模型，核对原先隐藏的写入工具和 MCP 入口仍然隐藏。通过后可移除[按模型 harness 拒绝切换](../src/agent/grok.rs)的判断，并更新 read 档的模型选择说明。
