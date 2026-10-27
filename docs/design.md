# 内部设计

用户可见的操作和配置见[使用文档](usage.md)，各家协议及权限映射见[后端文档](backends.md)。

实现范围是本机 CLI 编排，采用单二进制分发，不注册系统服务。远程主机、云端执行、桌面界面、多租户和 PR/CI 工作流由其他工具负责。接入新的 backend 前，需验证其在一轮运行中接收消息的能力。

## 进程与任务

一个二进制包含 CLI、daemon 和供 Codex 子 agent 使用的 MCP server。CLI 每次请求连接 daemon，daemon 持有 agent 进程并独占写入任务状态。子 agent 通过 CLI 或 MCP 回话。

采用按需启动的 daemon，是为了让任务脱离宿主的进程树并持续运行。每个任务只在运行轮次时持有原生进程，空闲后释放，下次通过原生 resume 继续。会话的并发写入差异见[后端文档](backends.md#会话与消息)。

任务状态如下：

| 状态 | 含义 |
|---|---|
| queued | 已派发或收到新输入，进程尚未启动 |
| running | 正在运行一轮 |
| idle | 本轮成功结束，可以继续发消息 |
| waiting | 本轮结束，仍有未回答的问题 |
| failed | 启动或本轮执行失败，可以续接 |
| interrupted | daemon 停止或异常退出导致中断，可以续接 |
| cancelled | 已取消，不能通过 Axiopera 续接 |

`stalled` 和 `detached` 是附加标记。调度上限、嵌套深度和接管操作见[使用文档](usage.md)。

轮末先处理已排队的输入和协议事件，再依据适配器的 busy 状态释放进程。释放后记录原生会话文件的路径、大小和修改时间，续接前核对。指纹变化时标为 detached。

取消先调用后端中断接口，2 秒后杀掉进程树。启动握手期间同样处理取消和硬超时。取消父任务会取消全部后代，命令等子树停止后才返回。

## 存储

数据目录的选择见[配置](usage.md#配置)。目录结构为：

```text
~/.axiopera/
  config.toml
  daemon/
    lock
    launch.lock
    info.json
    daemon.log
    requests.jsonl
  tasks/<id>/
    task.json
    events.jsonl
    turns/<n>.md
    approvals/<请求 ID>.txt、questions/<提问 ID>.txt、notes/<编号>.txt
  ledger.json
```

`task.json` 记录状态、工作目录和原生会话，写临时文件后原子替换。`events.jsonl` 只追加。`ledger.json` 保存游标、结果交付标记、Codex 推送目标、待确认批次和已排入的门铃。事件序号在整个 daemon 内单调递增，重启后继续；清理前先把当前序号记入 ledger，任务全被删除后也不会回退。

提问和答案随任务保存。审批应答通道、尚未送出的兄弟留言和消息消费跟踪 ID 只保存在内存中。

`requests.jsonl` 每个请求记一行：时间、调用方类别（宿主、子 agent 或 `user`）、命令、给出的参数名、退出码、错误类别和输出的字节数，不记参数值和输出内容。客户端先断开时退出码为空。hook 的 `pending` 请求、`daemon status` 和 `daemon stop` 不记录。

daemon 启动时和连续运行期间每 24 小时自动清理一次。清理任务时，在同一把状态锁内检查和删除，防止与续接并发。先处理子任务，再处理父任务。`daemon.log` 只在启动时、打开文件之前裁剪，运行期间文件一直打开。清理条件见[使用文档](usage.md#daemon-与清理)。

## daemon 生命周期

连接失败后，CLI 取得 `launch.lock` 并重新连接，仍失败才启动 daemon，最多等 5 秒。daemon 取得 `daemon/lock` 的独占锁后才创建 IPC 端点。

Windows 上，不在 Job 中就直接分离启动；在 Job 中先尝试 `CREATE_BREAKAWAY_FROM_JOB`，遇到 `ERROR_ACCESS_DENIED` 时通过 PowerShell 的 `Invoke-CimMethod` 调用 WMI。新进程不继承句柄，以免持有宿主的输出管道。Unix 使用 `setsid`，标准输入输出重定向到 `/dev/null`。

连接第一条消息携带协议版本。版本不一致且没有活跃 agent 时，旧 daemon 退出，由客户端重启；有活跃 agent 时返回错误。daemon 启动时把遗留的 running、queued 任务改为 interrupted。

Windows 为每个 agent 建立 `KILL_ON_JOB_CLOSE` Job，禁止 breakaway。先挂起创建进程，加入 Job 后恢复线程。Unix 为每个 agent 创建进程组，停止时向整组发送 SIGKILL。

## 身份与 IPC

调用方没有 `AXIOPERA_TASK_ID` 时，使用宿主会话身份 `<host>:<session>`，没有宿主身份则回落到终端用户 `user`。带有任务 ID 时，同时传递原生身份 `task:<id>/<host>:<session>`；daemon 核对任务记录中的后端和原生会话，将任务自身会话归一为 `task:<id>`，其他原生会话保留完整身份，各自收件。只有任务 ID 的调用（如注入的 MCP server）仍使用 `task:<id>`。原生子 agent 共享宿主会话 ID 时仍共用收件箱，使用约定见[后端文档](backends.md#会话与消息)。

完整调用方身份决定任务别名、收件游标和消息归属；其中的任务 ID 决定深度、权限上限、审批向上路由、整树取消和清理依赖。原生子 agent 属于所在的 Axiopera 任务，其派发的 Axiopera 任务继续计入原任务树。

`watch --caller` 直接指定调用方，供不传会话变量的 Grok monitor 使用。多个宿主变量同时存在时，沿祖先进程选择最近的宿主；无法匹配时按 `identity::HOST_VARS` 的顺序选择。

Windows 使用命名管道，DACL 放行当前用户 SID 和登录会话 SID，启用 `PIPE_REJECT_REMOTE_CLIENTS`；首个实例使用 `FILE_FLAG_FIRST_PIPE_INSTANCE`。Unix 使用数据目录下的 `daemon/sock`，目录权限为 0700，接入后核对对端 uid。

一个连接处理一个请求。CLI 发送一行 JSON，带协议版本、调用方和 cwd；`spawn`、`send`、`reply`、`models` 还带环境。daemon 输出若干行，最后一行携带退出码或错误。

daemon 在受限客户端上拒绝 `spawn`、`models`、`gc` 和 `daemon stop`。Windows 客户端在启动 daemon 或执行这些命令前也检查自身令牌。

- Windows 在收到第一行数据后模拟客户端身份，用 `IsTokenRestricted` 检查令牌；读取失败也按受限处理。
- Linux 比较对端与 daemon 的 `NoNewPrivs`、`Seccomp_filters`，任一项更高就按受限处理。已带 `NoNewPrivs` 的容器中，只叠加 Landlock 而不增加 seccomp 的限制无法这样识别。
- macOS 使用 `sandbox_check(pid, NULL, 0)`。

socket 连接返回 EPERM 时，客户端提示到 sandbox 外重跑。各后端的回连能力见[权限](backends.md#权限)。身份用于同一用户内的任务路由，持有相同用户权限的进程可以修改身份变量。

## 环境与配置继承

daemon 用调用方经 IPC 传来的环境启动子 agent。环境只留在内存；daemon 重启后的续接使用新一次 `send` 的环境。受限客户端传来的环境不用于启动进程。

启动前清除 `identity::STRIP_VARS` 列出的宿主身份、inbox 凭据和 Codex 客户端标识，注入任务 ID 与数据目录，并把 daemon 所在目录加入 PATH。各 backend 的配置覆盖见[后端文档](backends.md#可执行文件与配置继承)。

`models` 使用同样的路径定位和环境处理，查询进程不注入任务 ID。查询在连接处理协程中完成，不创建任务；调用方断开时取消查询。模型参数错误触发的自动查询使用原任务目录和环境，结果附在原错误后交付。

## 事件与交付

每个事件包含 `seq`、`ts`、`task`、`kind` 和类型相关的数据。以下事件由 `inbox` 交给父节点，审批请求交给审批方：

| 事件 | 内容 |
|---|---|
| turn_ended | 本轮状态、结果、未处理输入或确认缺口、拒绝次数、适配器的提示和未回答的问题 |
| question | 子 agent 的提问 |
| approval_request | 只交给审批方的请求 |
| stalled、failed、cancelled、interrupted、detached | 状态变化 |
| message | 兄弟留言的父节点抄送 |

其中兄弟留言抄送、交接通知和调用方自己发起的取消不算新消息：不让 `wait` 返回，不触发推送和提醒，随下一次 `inbox` 一起交出。超时或其他调用方的取消仍算新消息。

spawned、turn_started、steered、input_read、tool、file_changed、answered、approved、denied、blocked、notice 只写事件日志。`blocked` 在轮末汇总次数，避免每次拒绝都提醒主 agent。`notice` 是适配器发现的后端设置问题，原文附在下一条轮末结果上。改动文件从工具事件提取，共享目录下的 Git diff 无法归属到某个任务。

`wait` 只等不取，`inbox` 只取不等。游标按调用方和任务保存，只有 `inbox` 和推送推进游标；指定任务时不会消费其他任务的事件。结果正文的交付标记独立于游标，正文实际写给客户端后才标记。再次取正文的规则见[使用文档](usage.md#结果与接管)。

单次回复的预算为 4,500 个估算 token，按 Grok 保留的 2 万字符命令输出定，多个任务共享。每 4 个 ASCII 字符或每个其他字符计 1 token。一次回复先放还在等裁决、尚未交给调用方的审批请求，再按事件先后取整条事件，放不下的留到下一次，回复末尾注明还剩几条；第一条总会交出，结果过长时截短。子 agent 写的提问和留言与审批请求一样，超过 3,500 个估算 token 时只给开头和结尾，全文写入任务目录下的 `questions/` 或 `notes/` 并给出路径。审批请求不随游标交付：游标要求每个任务的事件按序交出，审批插队会越过同一任务里更早的事件。事件序列中的审批请求只推进游标，不再显示，已经交出或已有裁决的都不重复。同一次回复内按紧急程度排列：审批请求、提问、失败，然后是结果和抄送。子 agent 写的结果、提问和留言包在 `<result>`、`<question>`、`<note>` 标签里，正文中同名的闭合标签加反斜杠转义，避免被读成 Axiopera 自己的输出。截短的表现见[使用文档](usage.md#结果与接管)。

## 宿主通知

宿主通知遵循两条原则：

1. 正确性只依赖能力最弱的宿主。任何宿主只靠 `wait` 和 `inbox` 都能拿到全部结果。推送确认送达后才算交付，未确认的内容仍由 `inbox` 交出；门铃和可选集成只提醒宿主来取。提醒遗漏或重复不改变交付语义，宿主之间只有送达延迟不同。
2. 可选集成（插件、hook、skill 等）只用于加速或说明，并满足：
   - 由用户通过该后端自己的安装机制启用。Axiopera 不写入用户的 agent 配置，唯一例外是用户显式执行的清理命令，它只删除本集成留下的条目。
   - 移除后只退化为更慢，不出错。
   - 不改变无关会话的行为。需要在所有会话中运行的部分，在没有相关任务时立即返回，不启动 daemon，不访问网络；额外开销实测后写入文档。
   - 默认卸载不留痕。后端机制必然留下的痕迹逐项记录位置和含义，并由清理命令移除。

`spawn` 回执告诉宿主适用哪种取结果方式。各宿主的能力见[后端文档](backends.md#宿主通知)。

## Codex 推送与门铃

`spawn` 依次尝试注册推送和门铃，回执告诉主 agent 结果怎样送达；两者都不可用时说明原因：找不到 Codex、Codex 5 秒内没有应答，或会话既不在共享 daemon 上，也不在桌面端或 VS Code 扩展中。查询出错按后一种处理，错误写入 daemon 日志。连接方式和适用的会话见[后端文档](backends.md#宿主通知)。

推送与 `inbox` 按调用方串行，从读取事件到推进游标都持有同一把锁。发出消息但未收到应答时，ledger 保存批次标记、事件序号和结果轮次。下次连接用 `thread/turns/list` 读回完整用户消息，找到标记才确认，推进游标并把批次中的审批请求记为已交付；找不到则重发。读回失败时按已送达处理并记日志。`inbox` 交付前也核对该批次。

推送合并 1 秒内的事件，一条消息的内容与一次 `inbox` 回复相同，放不下的随下一条消息发出。失败后从 30 秒起指数退避，最长 10 分钟，新事件可以提前触发重试。只有新消息才会推送。

Codex 插件的 hook 用 `pending` 请求取得调用方新消息的摘要，daemon 没有运行时直接返回，不启动它；版本不同的 daemon 对该请求回空，不因此重启；推送宿主得到空回复。MCP 进程按会话记住已提醒到的事件序号，工具调用之后只在有更新的消息时提醒，轮末交给门铃。hook 同样只提醒，交付仍由 `inbox` 完成。

提醒的摘要每条新消息一行，写明任务、发生了什么和一行预览，提问和审批附 ID，最多 10 行，末行给出 `axiopera inbox`。每行不超过 400 字节，`wait` 列出新消息时也用这种格式。`watch` 每条新消息输出一行，距上一行不足 2 秒时合并到下一行，注明合并了几条，以免超过 Grok monitor 的速率限制；等待期间已被取走的不再输出；它一直运行到被停止，不取走消息。子 agent 的 `watch` 例外：没有在跑的子任务、也没有未取的新消息满 60 秒后，以退出码 4 结束。否则它所在的 monitor 一直算作 Grok 子 agent 的后台任务，子 agent 始终处于忙碌，进程永远不被释放。

门铃附上排入时的新消息摘要，交付仍由 `inbox` 完成，因此不需要读回确认。上一条提醒仍在队列中时不再排入；提醒已被处理而宿主没有 `inbox` 时，新事件会再排入一条。宿主 `inbox` 后，若已没有新消息，就撤回仍在队列中的提醒，避免空开一轮。排入失败时与推送一样退避重试。

推送目标随 ledger 持久化，包含 Codex 路径、home、thread ID 和送达方式。只有 `spawn` 注册目标；`gc` 删除已无任务的目标。

## 审批路由

适配器把一行摘要、完整请求和一次性应答通道交给 daemon，并在消息循环之外等待裁决。完整请求包括命令全文、工作目录和理由，文件改动带每个文件的 diff 和增删行数，其他工具调用列出全部参数。可转发的请求类型和 backend 的应答形式见[后端文档](backends.md#审批)。

`inbox` 和推送给出完整请求。超过 3,500 个估算 token 时只给开头和结尾，注明省略了多少，全文写入 `tasks/<id>/approvals/<请求 ID>.txt` 并给出路径。摘要只出现在单行场合，例如日志和提醒。审批方是 agent 时，请求经 `inbox` 或推送交给它之后才能批准；拒绝和终端用户不受此限制。请求在交给审批方之前已超时或已有裁决的，不再交出；`wait`、`watch` 和提醒只把等待裁决、尚未交出的请求算作新消息。

审批交给最近的 full 档祖先任务或主会话。work 档任务无法判断外部操作是否超出自身权限，因此请求越过它向上转发。事件只唤醒实际审批方。任务只能回答路由给自己的请求，主会话和终端用户可以回答任何请求。

超时即拒绝，时长由[配置](usage.md#配置)决定。默认值较短，因为部分宿主要到本轮结束才看到请求；子 agent 被拒后跳过该操作，并在报告中说明需要什么。请求进程退出时，其待决审批作废；应答通道断开也按拒绝处理。
