# zx-agent 路线图

本文档记录 zx-agent 的设计目标、已定决策、各阶段的状态与待办，只留结论；设计与实施的过程在 PR 描述和 git 历史里。现状与用法见 [README](../README.md)。

## 1. 目标

把 zx-agent 从“面向无人值守排障的内核”演进为通用内核：同一个内核支撑三类场景，场景差异全部放在内核之外。

| 场景 | 例子 | 前端 |
|---|---|---|
| 人机对话 | 在终端里和 Agent 多轮对话，类似 pi | `chat` |
| 无人值守 | 由 cron、Kubernetes Job、告警触发的排障 | `run` |
| 被嵌入 | 在编辑器或其他平台中驱动 Agent | `acp`、Rust SDK |

| 编号 | 目标 | 验收标准 |
|---|---|---|
| G1 | 一个内核支撑三类场景 | 三个前端共用同一个 `AgentSession`；内核中不出现“当前是不是交互模式”的判断 |
| G2 | 不改内核即可适配新场景 | 新增一个场景只需新增 agent 文件、提示词或插件 |
| G3 | 保持可审计与安全 | `.claude/CLAUDE.md` 中的不变量全部保留；每次模型请求都能从会话文件还原 |
| G4 | 渐进迁移 | 保持现有 `run` 行为兼容，测试全部通过；D5 若改变默认执行环境，在 P6 明确迁移方式并单独验收 |

G3 的完整请求重建在 P5 补齐：每次请求前记录 `request_started`，工具定义与发送配置变化时记录 `request_config`。本期不做的事见第 7 节。

## 2. 设计原则

1. 机制在内核，策略在外层：内核只提供循环、会话、钩子和事件；上限、续写、提示词、能否问人都由 agent 与前端决定。
2. 内核不直接面对人：需要人的地方都经过 `Interaction` 接口；没有人时有确定的回退，即拒绝。
3. 会话日志是唯一事实来源：模型可见的内容都先记录；原始历史永不改写，压缩和编辑都以追加条目表达。
4. 前端只消费事件、生产输入：界面、日志、协议适配器都是事件的订阅者。
5. 扩展优先用标准协议：工具扩展用 MCP，嵌入用 ACP。
6. 纵深防御：规则（deny → ask → allow）、执行环境隔离（文件系统和网络）、不可信数据不改变控制流。
7. 简单优先，可回退：没有第二个使用者就不抽象；每一步都能单独测试和回滚。

## 3. 已定决策

| 编号 | 事项 | 结论 | 日期 | 状态 |
|---|---|---|---|---|
| D1 | MSRV | 升级到 1.88 | 2026-10-31 | 已完成：`rust-version = "1.88"` |
| D2 | 新依赖 | 已授权，在用到的阶段引入：tokio 的 `io-std` feature、`toml`、`rmcp`（MCP 官方 Rust SDK）、ACP 官方 Rust SDK、`rustyline`（可选）。此后经用户同意追加：`opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`（2026-11-02，P9 的追踪）；`chrono`（2026-11-04，chat `--debug` 日志的本地时间与时区，只开 `clock`）；`jsonschema`（2026-11-05，按工具声明的 JSON Schema 校验参数，`default-features = false`）；`thiserror`（2026-11-09，派生 `Display` 与 `Error`） | 2026-10-31 | 已引入：`rmcp` 3.8.0（P7，PR #40）；OpenTelemetry 三个 crate 0.33.1（P9，PR #49）；`chrono` 0.4.45；`jsonschema` 0.33.0（`zx-core`）；`agent-client-protocol` 5.0.0（2026-11-05 按 D13，只开 `stdio`），它给 serde_json 打开了 `preserve_order`，JSON 对象从此按插入序输出；`thiserror` 2.0.21（`zx-core`、`zx-im`、`zx-im-feishu`） |
| D3 | profile 文件格式 | TOML | 2026-10-31 | P4 落实；D17 起拆为 `config.toml` 与 agent 文件，agent 的 front matter 仍是 TOML |
| D4 | 会话 API 形态 | 进程内方法调用；暂不实现提交队列 / 事件队列（SQ/EQ） | 2026-10-31 | 已实现（P2） |
| D5 | 无人值守的命令执行环境 | 默认在本机执行，`run` 行为不变；容器执行须显式开启（D17 起写在 `config.toml`；CLI 参数 `--execution`、`--container-image` 于 2026-11-09 移除）。理由：Kubernetes Job 的 Pod 内通常没有 Docker；无网络的容器做不了在线排障，在线访问方案尚未评审。不采用 `troubleshoot` 默认容器、没有 Docker 时拒绝启动 | 2026-11-01 | 已确认；P6 按此实现 |
| D6 | 项目定位 | 改为“通用内核 + 场景配置” | 2026-10-31 | 已完成 |
| D7 | IM 长连接的 WebSocket 客户端 | 已授权 `tokio-tungstenite` 0.30（关掉默认 feature，只开 `connect` 与 `rustls-tls-webpki-roots`） | 2026-11-01 | 已引入（PR #29） |
| D8 | MCP 的范围 | 只做 MCP 客户端：只接工具，只支持 Streamable HTTP；MCP 服务器与领域工具（如 Kubernetes、Prometheus 查询）由外部服务实现 | 2026-11-01 | 已确认；P7 按此实现 |
| D9 | HTTP 客户端与 TLS | reqwest 升到 0.13（`json`、`stream`、`rustls`）；rustls 只用 aws-lc-rs 一个加密后端，HTTPS 根证书用系统信任库。理由：rmcp 只支持 reqwest 0.13，与 0.12 并存时 rustls 同时启用两个后端，`wss://` 连接会 panic | 2026-11-01 | 经用户授权；已完成（PR #36） |
| D10 | 通用，不为场景定制 | 代码只定义接口与通用机制。某个环境、平台或厂商才有的知识（如 kubectl 的只读选项、Kubernetes 的 token 路径、IM 文案的语言）放在 agent、部署清单或配置里；告警等外部系统由外部适配器调用 `run`、SDK 或 IM 接入，zx-agent 不解析其私有格式（如 Alertmanager）；provider 的协议方言显式开启。同日决定：P9 只再做更多 provider（如 Anthropic），另有四项不做（第 7 节）；P8 `acp` 不做 | 2026-11-02 | 用户决定；已有代码中的定制已整理（PR #54）。“P8 `acp` 不做”于 2026-11-05 由 D13 撤回 |
| D11 | 容器执行的权限 | 沙箱内给 agent 最大权限，限制只放在外墙：工作目录（含 `.git`）与根文件系统可写，网络默认开启，审批由 agent 决定、可以一律放行；命令默认以映射的宿主机用户运行，仍不允许 root（以后按需做成显式选项，并同时改造进程清理）。外墙：不加 `--privileged`、不挂 `docker.sock`、不用宿主机的网络与 PID 命名空间、API key 与环境变量不进容器、会话文件在工作目录之外。原严格模式（只读、无网络）保留为可选配置。取代 P6 中“工作目录只读挂载、网络固定为 `none`”与“容器联网已取消”；D5 不变 | 2026-11-05 | 用户决定；已完成（P10 ②，PR #57） |
| D12 | chat 的会话默认落盘 | 不给 `--session` 时，chat 把每次对话存成 `~/.zx-agent/sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（目录名写法与 pi 相同；新建的目录 0700、文件 0600），`--no-session` 只保存在内存；`run` 不变，只在给出 `--session` 时写文件。取代 chat 默认只保存在内存的行为。理由：列出、续接与 fork 会话都需要一个已知的位置，与 pi 一致。D17 去掉 `--session-dir`，D19 去掉 chat 的 `--fork`、`/fork` 与会话内的 `/sessions`、`/resume` | 2026-11-05 | 用户决定（另一选项是只在给出 `--session-dir` 时落盘）；P10 ⑥a 落实 |
| D13 | 人机界面协议 | 只维护内核：ACP 是对人的唯一界面协议，交互式 UI 由实现 ACP 的开源客户端（Zed 等编辑器、Wisp 等终端客户端）提供，zx-agent 只实现 agent 一侧（`acp`）。`chat` 冻结为最小入口（应急使用与 `--output jsonl` 的脚本化调用），不再加功能（D19 据此删去遗留的界面功能）；`serve`（现名 `im`）冻结，只修平台 API 变化导致的故障（2026-11-10 由 D16 解冻）；`run` 保留并维护，ACP 覆盖不了无人值守；SDK 不变 | 2026-11-05 | 用户决定；撤回 D10 的“P8 `acp` 不做”，P8 重启 |
| D14 | 不做 rpc 模式 | 不为非 Rust 程序另做 pi `rpc` 模式那样的进程协议（stdin/stdout 的 JSON 行命令与事件流）。非 Rust 宿主经 ACP（D13），无人值守经 `run`（退出码与 `--output jsonl`），常驻服务经 `im` 与 SDK；pi 的 rpc 本身也没有幂等键 | 2026-11-05 | 按用户授权采纳建议；P10 ⑫ 只做幂等键，幂等键后由 D18 去掉 |
| D15 | 会话格式只保留当前版本 | 项目没有使用者，也没有要保留的会话文件：只读写当前格式（`SESSION_VERSION`，当时为 6，D18 起为 7），头部是其他版本的会话一律拒绝打开，`SessionStore::create` 也不接受其他版本的头部；删除旧格式的升级代码。格式变化时提高版本号，不写升级代码。取代“修改持久化格式前先确认旧文件还能读取”的约定 | 2026-11-09 | 所有者决定；已实施（分支 `simplify-formats`） |
| D16 | IM 的对话模型 | 解冻 `im`（修订 D13），按人和同事说话的直觉重做对话模型：私聊整段一个会话，`/new` 开新会话；群聊一个线程一个会话；机器人参与过的线程里，没 @ 它的消息只记作上下文，刚被它回答的人接着说不必 @；第一次在别人发起的线程里被 @ 时先读已有消息。不读频道历史，不加 `/clear` 别名，不加开关：平台没订阅线程消息时退化为只认 @。行为细节见 README 的“IM 服务” | 2026-11-10 | 用户决定；P15 落实 |
| D17 | 配置拆为 `config.toml` 与 agent | 主流 agent（Claude Code、Codex、Gemini CLI、OpenCode、pi）都自动发现用户配置文件、把模型放在其中、用 allow / ask / deny 表达权限；此前每次都要 `--model` 与 `--profile <路径>`，一个 profile 混着角色与环境，有 8 个权限开关。改为：① 主目录 `~/.zx-agent`（`ZX_AGENT_HOME` 可改）中的 `config.toml` 是环境；不用多个工具共用的 `~/.agents/`；② 模型取 `--model` > `ZX_MODEL` > 文件，服务地址取 `ZX_BASE_URL` > 文件 > 默认，其余原命令行设置只写在文件中，去掉 `--session-dir`、`--queue-mode`；③ 没有项目级配置文件：项目内容不能改变环境或权限，项目的贡献仍是 AGENTS.md；④ agent 是带 TOML front matter 的 Markdown 文件，内置 `assistant`（chat、im、acp）与 `unattended`（run），主目录 `agents/<名字>.md` 替换同名内置 agent，其他文件名不读取，没有 `--agent`；⑤ 只有一条合并规则，没有累加的列表；⑥ `[permissions]` 只改语法、策略引擎不变，去掉命令白名单 `rules.allow`。键与规则见 [CONFIGURATION.md](CONFIGURATION.md) | 2026-11-10 | 所有者决定；已实施（PR #93） |
| D18 | 内核精简 | 去掉没有使用者或与其他机制重复的内核能力：① 命令白名单及提示词段落 `command_policy`（D17 已用 `permissions.allow` 取代，CLI 从未输出该段落）；② 只有测试在用的 `AgentSession` 接口（`subscribe_bounded`、`set_active_tools`、`clear_approvals`、`writes_stopped`、`steer` / `follow_up` 等，改用 `queues()`）；③ `prompt_once` 与幂等键（推翻 P10 ⑫：唯一会重复提交的宿主 IM 从未使用）；④ 增量摘要（两种模式一直没有选定默认值），只留全量模式：每次重读原始记录，摘要源上限 2 MiB（`MAX_SOURCE_BYTES`），超过后要 `/new` 开新会话。会话格式升为 7（D15） | 2026-11-10 | 所有者同意；已实施 |
| D19 | CLI 精简 | ① chat 收回到应急与脚本化入口（D13）：删去 `--debug` 的执行时间线、`--show-thinking`、终端状态行与动画、`/tree`、`/branch`、`--fork`、`/fork`、REPL 内的 `/sessions` 与 `/resume <ID>`、`/queue`（`zx-agent sessions` 与 `chat --resume` 保留）；两个消息队列共用一种取法；会话树、复制与分支留在 SDK。修订 D12、D13 中提到 fork、tree、debug 的部分。② otel 只导出 span，删去指标管线：指标可在收集端从 span 推导，token 与成本仍是 span 的属性。③ 审批等待只有 `config.toml` 顶层的 `approvalTimeoutSecs`（默认 600 秒，chat、acp 与 im 共用），去掉 `[chat]`、`[acp]` 与 IM 配置中的同类键。④ `--session` 只属于 `run` 与 `chat`，acp 与 im 由命令行解析拒绝（退出码 2）。⑤ 去掉 `[[mcp]]` 的 `maxTimeoutSecs`：pi 没有不看进度的总上限，一次调用已由按进度重新计时的 `timeoutSecs`、运行的超时与取消约束 | 2026-11-10 | 所有者批准 |

## 4. 阶段状态

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 场景默认值移出内核 | 已完成 |
| P1 | 钩子、工具调用流水线与人机交互接口 | 已完成（PR #3） |
| P2 | 长期会话 `AgentSession` | 已完成（PR #4） |
| P3 | 终端对话 `chat` | 已完成（PR #5）；按 D13 冻结 |
| P4 | profile 文件、上下文文件与 skills | 已完成（PR #6） |
| P5 | 上下文管理：有界摘要、请求审计、长结果落盘 | 已完成（PR #9）；增量摘要（PR #45）由 D18 去掉 |
| P6 | 执行环境与崩溃恢复 | 进行中：崩溃恢复（PR #12）、执行器接缝（#13）、容器执行（#14；按 D11 改为沙箱，#57）、常驻 Pod 部署与多集群（#19、#20、#22、#24）、IM 服务（#28、#29、#32、#33）已完成；待真实集群与真实 IM 平台验收（第 5 节） |
| P7 | 扩展接口与 MCP 客户端 | 已完成：会话宿主与可替换存储（PR #21）、追踪事件（#23）、扩展 API（#25）、`Extension`（#38）、MCP 客户端（#40、#41）；真实 MCP 服务器的验收见第 5 节 |
| P8 | ACP agent `acp` | 进行中：按 D13 重启，`acp` 已实现；`session/load`（PR #74），`session/list` 与 `session/resume`（#76）；待在 Zed 或 Wisp 上实测 |
| P9 | 可选增强 | 进行中：token 预算（PR #47）、OpenTelemetry（#49）、请求附加字段（#50）、crate 拆分（#52）、通用化整理（#54）已完成；剩更多 provider |
| P10 | 与 pi 的能力对齐 | 已完成（最后为 PR #72）：上下文超长后恢复（#56）、容器沙箱（#57）、edit / write（#58）、grep / find / ls（#59）、推理强度与续跑（#60、#61、#65）、会话目录 / fork / 分支（#67、#68、#72）、扩展能力与消息队列、模型元数据与计量、幂等键（D18 去掉）、MCP 进度、可观测、行为评测框架（已去掉，见第 7 节）；chat 的便利功能按 D13 不做 |
| P11–P13 | 内核与项目整理、按规则评审 | 已完成（PR #70、#71、#75、#77）；模块文件布局（#78） |
| P14 | 对照 pi durable 的小项与无人值守的缺口 | 已完成（PR #80）：工具结果上报用量、撤回排队的消息、定时巡检保留会话文件、被拒调用计数与 `run --fail-on-denied`（退出码 9）、成本预算 `--cost-budget`（退出码 8）；对照基准为 pi `a92283056` 的 `packages/durable`。之后成本字段改为不带币种的 `totalCost`（PR #82；事件 `schemaVersion` 升为 2） |
| P15 | IM 对话模型（D16） | 进行中：实现已提交（PR #92）：消息带会话种类与对象，`ImChannel` 新增读线程历史、查名字、回执三个带默认实现的方法，会话文件改为每个会话一个目录。Slack 实现全部三项；⚑ D:5/CHINA FLAG: 飞书只做消息分类，三项沿用默认（不读历史、用 open_id、不回执），待按需补上。待在真实 Slack 工作区验证线程回复事件带 `thread_ts`（官方事件页未写明） |

## 5. 待办与已知缺口

| 事项 | 状态 | 说明 |
|---|---|---|
| 更多 provider（如 Anthropic Messages API） | 未开始 | ⚑ D:5/CHINA FLAG: DeepSeek 经 OpenAI 兼容接口直接可用，不单独写 provider；服务端的设置经 `provider.requestFields` 传递 |
| 真实 IM 平台验收 | 待凭据 | 在 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次 @、回答与审批；换 HTTP 客户端与 TLS（D9）之后，对这两个平台的 HTTPS 连接没有验证过 |
| 真实集群验收 | 待部署方 | 常驻 Pod 部署包与多集群配置没有在真实集群上执行过；只读规则只看命令字面 |
| 真实 MCP 服务器验收 | 待选定服务器 | 对一个 Streamable HTTP 服务器跑通列出、调用与审批；HTTPS 的 MCP 服务器未测 |
| ACP 客户端实测 | 待做 | 只用手写客户端与 SDK 示例客户端验证过，没有在 Zed 或 Wisp 上用过 |
| IM 服务崩溃时丢失已确认、还没处理的消息 | 已知缺口 | 重启后会替换遗留的“处理中…”（PR #46），消息本身仍丢失。持久化收件需要新的记录格式与重放规则，等多副本或实际丢消息时再评审 |
| 输入被钩子改写时的崩溃窗口 | 已知局限 | 原始输入与替换它的 `context_edit` 分两次追加；其间进程崩溃，原始输入会出现在投影中 |
| 会话写入不 `fsync` | 已知局限 | 只调用 `flush`，断电时已写入的记录可能没有落盘 |
| 打开大会话的耗时 | 暂缓 | 加载时每条请求记录都要构建一次分支路径，耗时高于线性 |
| 保留近期用户消息原文；摘要前把较早的长工具结果换成引用 | 待评估 | 两项上下文管理改进，可推迟全量摘要碰到 2 MiB 上限 |
| 审计中写明自动放行命中的前缀 | 暂缓 | 要改 `PolicyDecision` 的 JSON 契约（`tests/hook_contract.rs`） |
| `cli::prepare` 拆成配置、进程级运行时与每会话三层 | 暂缓 | 改动面大 |
| FauxProvider 在增量之间响应取消、可脚本化带 `ProviderError` 的失败 | 暂缓 | 有测试需要时再做 |
| provider 契约的兜底 | 暂缓 | 取消后给 provider 的宽限期；`Done` / `Error` 与 `stop_reason` 的一致性检查 |
| `JsonlFile::create` 在加锁或写头部失败后留下文件 | 暂缓 | 这两种失败无法在测试中稳定构造 |
| `run` 的内置 agent 是否默认开启自动摘要 | 待用户决定 | unattended（以及扩展它的 `deploy/kubernetes/agents/unattended.md`）默认关闭，长时间排查撞到上下文上限时以退出码 1 结束；开启后可从上下文过长中恢复，代价是摘要丢失信息 |
| 定时巡检的会话文件不自动清理 | 已知局限 | 每次运行一个文件，在卷 `zx-agent-inspect-sessions` 上累积，需要部署方定期删除 |
| 失败运行后排队输入的处置 | 待评估 | pi 的 `afterFailedRun`（`continue` / `withdraw`）；现在由宿主在运行结束后用 `take_queued` 自行处置 |
| 工具触发的 `handoff` | 待评估 | pi 的 `control.handoff`；zx 没有 `reset` 条目，要先定会话格式 |
| 对照 pi durable 的较大项 | 待评审 | 排队消息落盘（同时关掉 IM 丢消息的缺口）、运行中部分结果落盘、后台摘要与摘要钩子、按会话的扩展选择与工作目录、扩展的可读状态、延迟加载的工具与 `tool_search`、持久化的睡眠与定时、宿主侧的环境接口、SQLite 等存储后端 |

## 6. 验证记录

只记录付费或真实环境上的运行；常规测试每个 PR 都跑。

| 日期 | 运行 | 结果 |
|---|---|---|
| 2026-11-01 | `tests/real_api.rs`，⚑ D:5/CHINA FLAG: DeepSeek（`https://api.deepseek.com/v1`，`collapse` 模式），⚑ D:5/CHINA FLAG: `deepseek-flash` 与 `deepseek-chat` 各一次 | 均为 6 项通过；不含压缩用例 |
| 2026-11-02 | `run` 经 `https://api.deepseek.com` 调用 ⚑ D:5/CHINA FLAG: `deepseek-flash`，不带工具，PR #50 的 `--request-field` 取默认、关闭思考、`reasoning_effort="low"` | 均以 0 退出、回答为 `ok`；关闭思考时输入 token 不同，字段生效 |
| 2026-11-02 | 摘要评测 `summaries_over_eight_rounds_of_a_scripted_session`（全量与增量各摘要 8 轮），两个模型各三次 | ⚑ D:5/CHINA FLAG: `deepseek-chat` 三次通过。⚑ D:5/CHINA FLAG: `deepseek-flash` 三次失败：第一次因摘要回答的大小上限（PR #48 修正）；后两次因推理用满默认 2048 token 的输出预算。用 `ZX_E2E_SUMMARY_TOKENS` 调高预算后尚未运行 |

各项验证的范围：

- 本地使用 macOS，CI 覆盖 Linux 与 macOS。
- `provider.systemUpdates = "inline"` 只在一个服务上验证过。
- 容器执行的真实 Docker 测试（设置 `ZX_DOCKER_TEST_IMAGE` 运行 `tests/container.rs`）在本机 macOS 的 Docker Desktop 上用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像跑过，原生 Linux 的 Docker 只由 CI 覆盖；没有在 rootless Docker 或 Podman 上验证。

### Kubernetes 常驻部署（`deploy/kubernetes/`）

2026-11-01，结果均为单次观测。集群为本机 Docker Desktop 自带的 Kubernetes v1.36.1，2 个 arm64 节点，默认 StorageClass 为 local-path（ext4），回收策略 `Delete`。完整验证基于 `dd43cce`；rebase 到包含 PR #21（会话文件单写者）的 `5c20655` 后，换用新标签的镜像复验了会话锁、脱敏、`/proc` 保护、chat 审批与定时巡检。清单经一个 overlay 把命名空间换成 `zx-agent-e2e`、镜像换成本地构建的 `zx-agent:e2e`，其余原样；镜像没有推送到镜像仓库。模型是按脚本应答的假服务。

| 项 | 结果 |
|-|-|
| 镜像 | 构建成功：zx-agent 0.1.0、kubectl v1.36.1（校验和检查通过）、jq 1.6。以 uid 10001 运行，PID 1 是 tini |
| 只读账号 | Pod 内 `kubectl get pods` 成功；读 Secret、`rollout restart` 都被拒绝（Forbidden） |
| run | `kubectl exec … zx-agent run` 执行 `kubectl get pods` 并回答，退出码 0，会话文件写在 `/data` |
| chat | 经 `-i` 管道与 `-it` 终端运行。`kubectl get` 直接执行；`rollout restart` 先请求审批，未启用 `operator.yaml` 时批准后被拒绝 |
| 写权限 | 启用 `operator.yaml` 后，`rollout restart`（Deployment、StatefulSet、DaemonSet）、`scale`（Deployment、StatefulSet）、`delete pod` 都成功；`kubectl -v=6` 显示 `scale` 请求 `…/scale` 子资源 |
| 定时巡检 | `create job --from=cronjob` 试跑完成，`kubectl logs` 显示回答。巡检账号 `auth can-i`：`list pods` 为 yes，`patch deployments`、`delete pods` 为 no；巡检 Pod 中执行写命令被拒绝 |
| 脱敏与 `/proc` | 命令读出的 ServiceAccount token 与 PID 1 环境中的 API key 在工具结果里都是 `[REDACTED]`；假模型收到的 8 个请求中不含 token 与 API key。命令读 agent 自身的 `/proc/<pid>/environ` 得到 `Permission denied`；巡检 Pod 的 PID 1 是 zx-agent，读它的环境同样被拒绝（agent 不再把自身设为不可 dump 之后，这两项不再成立，未重新实测） |
| 会话文件锁（PR #21） | chat 运行期间持有会话文件的 `flock`；另一个 `kubectl exec` 中对同一文件的 run 报告 `is in use`，退出码 2；chat 结束后同一文件可再次使用 |
| 清理 | `delete -k` 删除全部对象（含集群级的角色和绑定），从 CronJob 创建的 Job 与卷一并删除；删除命名空间后没有残留 |

多集群（基于 `e33f6fb` 上的分支，镜像标签 `e2e-mc`）：只有一个集群，由同一集群扮演远端。`remote.yaml` 创建的只读账号作为远端身份，kubeconfig 的 `remote` context 指向同一个 API server、使用这个账号的 token。

| 项 | 结果 |
|-|-|
| 远端准备 | `kubectl apply -f remote.yaml` 后，token Secret 自动填入 token 和 CA；该 token 的 JWT 中没有 `exp` |
| 身份 | 常驻 Pod 中 `kubectl auth whoami`：默认 context 为 `system:serviceaccount:zx-agent-e2e:zx-agent`，`--context remote` 为 `system:serviceaccount:zx-agent-remote:zx-agent`。巡检 Pod 中分别为 `zx-agent-inspect` 与同一个远端账号 |
| run 与脱敏 | run 经 `--context remote` 读到 Pod 列表；命令读出的远端 token 在工具结果中是 `[REDACTED]`（`ZX_SECRET_FILES` 指向 token 目录） |
| chat | `kubectl config get-contexts` 与 `kubectl get pods … --context remote` 都不经审批直接执行 |
| 新增集群 | 用 `create … --dry-run=client -o yaml \| apply` 给 Secret 加一个 key；约 56 秒后新文件出现在 Pod 中并随即被脱敏，Pod 没有重启。用 `--save-config` 创建的对象，之后 `apply` 不再提示缺少注解 |
| 清理 | 本集群按 `deploy/kubernetes/README.md` 的“清理”一节删除；远端执行 `kubectl delete -f remote.yaml --ignore-not-found`，退出码 0。不加该参数时，因 token 已随 Namespace 删除，命令报 NotFound 并以 1 退出 |

IM 服务（基于 PR #32 的 `c272f41` 加当时的改动，镜像标签 `serve-d1`）：当时子命令、组件、ConfigMap 与挂载路径名为 `serve`、`zx-agent-serve`、`/etc/zx-agent-serve/serve.toml`，现为 `im`、`zx-agent-im-config`、`/etc/zx-agent-im/im.toml`；下表保留当时的原样，改名后未重新实测。没有真实平台的凭据：`serve.toml` 中 Slack 的 `apiBase` 指向拒绝连接的 `127.0.0.1:9`，Secret 中是假 token。清单原样，只在 `kustomization.yaml` 中启用 `serve` 组件、改了镜像标签；`deploy/kubernetes/README.md` 的“IM 服务”一节中的每条命令都执行过。

| 项 | 结果 |
|-|-|
| 进程 | PID 1 为 `/pause`，`zx-agent serve --config /etc/zx-agent-serve/serve.toml` 为 7 号进程 |
| `/proc` 保护 | `/proc/1/environ` 中没有 `OPENAI_API_KEY`；读 serve 的 `/proc/7/environ` 得到 `Permission denied`（agent 不再把自身设为不可 dump 之后，后一项不再成立，未重新实测） |
| 孤儿进程 | 执行 `(sleep 1 &)` 后 3 秒，没有状态为 `Z` 的进程 |
| chat | serve 运行期间，`kubectl exec -i` 启动 chat 并输入 `/quit`，退出码 0 |
| 日志 | stderr 有启动行 `[serve] platforms=slack; sessions in /data/sessions`；连不上 Slack 时每次重试一行 `{"type":"failed","platform":"slack",...}`，间隔 1、2 秒递增；日志中没有任何凭据值 |
| 停止 | 向 serve 发送 SIGTERM 后，容器以退出码 0 结束（`Completed`）并重启 |
| 重启 | `kubectl rollout restart` 完成 |
| 两个组件 | 同时启用 `serve` 与 `multicluster` 渲染：挂载 `/etc/zx-agent-serve` 与 `/etc/zx-agent`，两组环境变量都在，不冲突 |
| 清理 | 按 `deploy/kubernetes/README.md` 的“清理”一节删除后，命名空间与两个 ClusterRoleBinding 都已不存在 |

未验证：x86_64 镜像与其他集群（如 GKE）、推送到镜像仓库、真实模型、真正分属两个集群的多集群访问、kubeconfig 中 exec 插件（如云厂商认证插件）方式的凭据、真实的 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户（IM 服务只对连不上的地址验证了启动、日志与停止）。

## 7. 已决定不做

- `--output-schema`、异步审批（`WebhookInteraction`）、OS 级沙箱、由 webhook 或告警触发运行（D10）
- rpc 模式（D14）；chat 的便利功能（D13）
- durable 的自动续跑与重放、codemode 与 `runToolCall`、工具结果中的图片与 `structuredContent`（README“与 pi 的差异”）
- MCP 的 stdio、OAuth 与运行中更新工具列表（D8）
- 完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时
- A/B 行为评测框架（pi 的 `packages/evals`，PR #72 的 `tests/common/evals.rs`）：2026-11-10 去掉，所有者批准；没有对真实模型运行过。理由：没有计划比较 agent 文件，要比较时再重建

## 8. 推进方式

- 设计有变化时，先更新本文档，再改代码。
- 阶段或待办完成时，在 PR 里直接把状态写成“已完成（PR #编号）”。过程记录写在 PR 描述里，不写进本文档。
- 检查命令见 `.claude/CLAUDE.md`。
- 需要新依赖时，先确认在 D2 的授权范围内；范围之外的要先征得同意。

---

ROADMAP.md · v0.14 · 2026年11月
