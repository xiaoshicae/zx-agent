# zx-agent 路线图

本文档记录 zx-agent 的设计目标、已定决策、各阶段的状态与待办。设计与实施的过程在 PR 描述和 git 历史里。

- 现状与用法见 [README](../README.md)。

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
| G2 | 不改内核即可适配新场景 | 新增一个场景只需新增 profile 文件、提示词或插件 |
| G3 | 保持可审计与安全 | `.claude/CLAUDE.md` 中的不变量全部保留；每次模型请求都能从会话文件还原 |
| G4 | 渐进迁移 | 保持现有 `run` 行为兼容，测试全部通过；D5 若改变默认执行环境，在 P6 明确迁移方式并单独验收 |

G1–G4 是验收目标。当前会话日志可重建对话与提示词；G3 所要求的完整请求重建已在 P5 补齐：每次请求前记录 `request_started`，工具定义与发送配置变化时记录 `request_config`。

**本期不做**：pi 级别的完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时。

## 2. 设计原则

1. **机制在内核，策略在外层**：内核只提供循环、会话、钩子和事件；上限、续写、提示词、能否问人都由 profile 决定。
2. **内核不直接面对人**：需要人的地方都经过 `Interaction` 接口；没有人时有确定的回退，即拒绝。
3. **会话日志是唯一事实来源**：模型可见的内容都先记录；原始历史永不改写，压缩和编辑都以追加条目表达。
4. **前端只消费事件、生产输入**：界面、日志、协议适配器都是事件的订阅者。
5. **扩展优先用标准协议**：工具扩展用 MCP，嵌入用 ACP。
6. **纵深防御**：规则（deny → ask → allow）、执行环境隔离（文件系统和网络）、不可信数据不改变控制流。
7. **简单优先，可回退**：没有第二个使用者就不抽象；每一步都能单独测试和回滚。

## 3. 已定决策

| 编号 | 事项 | 结论 | 日期 | 状态 |
|---|---|---|---|---|
| D1 | MSRV | 升级到 1.88 | 2026-10-31 | 已完成：`rust-version = "1.88"`，已用 1.88.0 工具链编译并通过测试 |
| D2 | 新依赖 | 已授权：tokio 的 `io-std` feature、`toml`、`rmcp`（MCP 官方 Rust SDK）、ACP 官方 Rust SDK、`rustyline`（可选）。在用到它们的阶段再引入。2026-11-02 追加 `opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`（P9 的 OpenTelemetry 追踪）。2026-11-04 追加 `chrono`（chat `--debug` 日志显示本地时间与时区；`default-features = false`，只开 `clock`）。2026-11-05 经用户确认追加 `jsonschema`（循环按工具声明的 JSON Schema 校验参数；`default-features = false`）。2026-11-09 经用户同意追加 `thiserror`（错误类型派生 `Display` 与 `Error`） | 2026-10-31 | 按阶段引入；`rmcp` 3.8.0 已于 P7 引入（PR #40）；三个 OpenTelemetry crate 0.33.1 已于 P9 引入（PR #49）；`chrono` 0.4.45 原为 `rmcp` 的间接依赖，2026-11-04 改为 `zx-agent` 的直接依赖；`jsonschema` 0.33.0 是 `zx-core` 的依赖，`Cargo.lock` 因此新增 35 个包，其 `rust-version` 为 1.71.1，未引入新的 rustls 加密后端；`agent-client-protocol` 5.0.0（连同 `-schema` 1.15.1 与 `-derive`）于 2026-11-05 按 D13 引入为 `zx-agent` 的依赖，只开 `stdio` feature，`Cargo.lock` 新增 54 个包，其 `rust-version` 为 1.88.0，未引入新的 rustls 加密后端；它给 serde_json 打开了 `preserve_order`，整个 workspace 的 JSON 对象从此按插入序而不是键序输出，两处比较 JSON 字符串的断言随之调整；`thiserror` 2.0.21 原为 opentelemetry、rmcp、tungstenite 的间接依赖，2026-11-09 改为 `zx-core`、`zx-im`、`zx-im-feishu` 的直接依赖，`Cargo.lock` 没有新增包，其 `rust-version` 为 1.77 |
| D3 | profile 文件格式 | TOML | 2026-10-31 | P4 落实 |
| D4 | 会话 API 形态 | 进程内方法调用；暂不实现提交队列 / 事件队列（SQ/EQ） | 2026-10-31 | 已实现：P2 进程内共享会话 API |
| D5 | 无人值守的命令执行环境 | 默认仍在本机执行，`run` 行为不变；容器执行须在 profile 中显式开启（原有的 CLI 参数 `--execution`、`--container-image` 于 2026-11-09 移除，与 profile 重复）。理由：在 Kubernetes Job 中运行时 Pod 内通常没有 Docker；无网络的容器做不了在线排障，而在线访问方案尚未评审。此前的候选方案（`troubleshoot` 默认容器、没有 Docker 时拒绝启动）不采用 | 2026-11-01 | 已确认：P6 执行环境部分按此实现 |
| D6 | 项目定位 | 改为“通用内核 + 场景配置” | 2026-10-31 | 已完成：README、`.claude/CLAUDE.md` 已更新 |
| D7 | IM 长连接的 WebSocket 客户端 | 已授权：`tokio-tungstenite`（0.30，关掉默认 feature，只开 `connect` 与 `rustls-tls-webpki-roots`） | 2026-11-01 | 已引入（PR #29） |
| D8 | MCP 的范围 | 本项目只做 MCP 客户端：只接工具，只支持 Streamable HTTP。MCP 服务器与领域工具（如 Kubernetes、Prometheus 查询）由外部服务实现 | 2026-11-01 | 已确认：P7 按此实现 |
| D9 | HTTP 客户端与 TLS | reqwest 升到 0.13（`json`、`stream`、`rustls`）。rustls 只用 aws-lc-rs 一个加密后端，HTTPS 的根证书改用系统信任库。理由：rmcp 只支持 reqwest 0.13，与 0.12 并存时 rustls 同时启用两个后端，`wss://` 连接会 panic；项目还没有使用者，用户授权按最合理的方式处理 | 2026-11-01 | 已完成（PR #36） |
| D10 | 通用，不为场景定制 | zx-agent 是通用 Agent：代码只定义接口与通用机制，外部来适配。某个环境、平台或厂商才有的知识（如 kubectl 的只读选项、Kubernetes 的 token 路径、IM 文案的语言）放在 profile、部署清单或配置里；告警等外部系统由外部适配器调用 `run`、SDK 或 IM 接入，zx-agent 不解析它们的私有格式（如 Alertmanager）。provider 对协议方言的处理显式开启。同日决定：P9 只再做更多 provider（如 Anthropic），`--output-schema`、异步审批、OS 级沙箱不做，“由 webhook 或告警触发运行”取消；P8 `acp` 不做 | 2026-11-02 | 用户决定；已有代码中的定制已整理（PR #54）。2026-11-05 撤回其中“P8 `acp` 不做”一项，见 D13 |
| D11 | 容器执行的权限 | 沙箱内给 agent 最大权限，限制只放在沙箱的外墙上：工作目录（含 `.git`）与根文件系统可写，网络默认开启，审批由 profile 决定、可以一律放行；命令默认以映射的宿主机用户运行，仍不允许 root（root 以后按需做成显式选项，并同时改造进程清理）。外墙不变：不加 `--privileged`、不挂 `docker.sock`、不用宿主机的网络与 PID 命名空间、API key 与环境变量不进容器、会话文件在工作目录之外。原严格模式（只读、无网络）保留为可选配置。取代 P6 中“工作目录只读挂载、网络固定为 `none`”的约定与状态表中的“容器联网已取消”；D5（默认在本机执行）不变 | 2026-11-05 | 用户决定；已完成（P10 ②，PR #57） |
| D12 | chat 的会话默认落盘 | 不给 `--session` 时，chat 把每次对话存成 `~/.zx-agent/sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（目录名的写法与 pi 相同；新建的目录 0700、文件 0600），`--session-dir` 换目录，`--no-session` 只保存在内存。`run` 不变：只在给出 `--session` 时写文件。取代此前 chat 默认只保存在内存的行为。理由：列出会话、续接最近一次、fork 都需要一个已知的位置，与 pi 一致 | 2026-11-05 | 用户决定（另一选项是只在给出 `--session-dir` 时落盘）；P10 ⑥a 落实 |
| D13 | 人机界面协议 | 只维护内核，界面交给别人：ACP 是内核对人的唯一界面协议，交互式 UI 由实现 ACP 的开源客户端（Zed 等编辑器、Wisp 等终端客户端）提供，zx-agent 只实现 agent 一侧（`acp` 子命令）。`chat` 冻结为最小入口（没装客户端时的应急使用与 `--output jsonl` 的脚本化调用），不再加功能；`serve` 冻结，只修平台 API 变化导致的故障，是否拆出或下线等 ACP 用过一段时间再定；`run` 保留并维护，无人值守场景 ACP 覆盖不了；SDK 不变。与 D10 的通用原则一致：协议是开放标准，zx-agent 不解析任何客户端的私有格式 | 2026-11-05 | 用户决定；P8 据此重启。编号 D13 是因为同日另一分支先用了 D12 |
| D14 | 不做 rpc 模式 | 不为非 Rust 程序另做进程协议（pi 的 `rpc` 模式：stdin/stdout 的 JSON 行命令与事件流）。非 Rust 的宿主经 ACP（D13）接入；无人值守经 `run`（退出码与 `--output jsonl`），常驻服务经 `serve` 与 SDK；pi 的 rpc 本身也没有幂等键 | 2026-11-05 | 按用户授权采纳建议（用户要求按建议推进，不逐项确认）；P10 ⑫ 只做幂等键 |

D2 之外的新依赖、依赖升级，仍需先征得同意。

## 4. 阶段状态

各阶段的设计与实施过程在对应 PR 和 git 历史里，本文只留结论。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 场景默认值移出内核 | 已完成 |
| P1 | 钩子、工具调用流水线与人机交互接口 | 已完成（PR #3） |
| P2 | 长期会话 `AgentSession` | 已完成（PR #4） |
| P3 | 终端对话 `chat` | 已完成（PR #5）；按 D13 冻结 |
| P4 | profile 文件、上下文文件与 skills | 已完成（PR #6） |
| P5 | 上下文管理：有界摘要、请求审计、长结果落盘 | 已完成（PR #9）；增量摘要为可选模式（PR #45） |
| P6 | 执行环境与崩溃恢复 | 进行中：崩溃恢复（PR #12）、执行器接缝（PR #13）、容器执行（PR #14；按 D11 改为沙箱，PR #57）、常驻 Pod 部署与多集群（PR #19、#20、#22、#24）、IM 服务 `serve`（PR #28、#29、#32、#33，按 D13 冻结）已完成；待真实集群与真实 IM 平台验收（第 5 节） |
| P7 | 扩展接口与 MCP 客户端 | 已完成：会话宿主与可替换存储（PR #21）、追踪事件（PR #23）、扩展 API（PR #25）、`Extension`（PR #38）、MCP 客户端（PR #40、#41）；真实 MCP 服务器的验收待选定服务器 |
| P8 | ACP agent `acp` | 进行中：按 D13 重启，`acp` 子命令已实现；`session/load`（PR #74），`session/list` 与 `session/resume`（PR #76）；待在 Zed 或 Wisp 上实测 |
| P9 | 可选增强 | 进行中：token 预算（PR #47）、OpenTelemetry（PR #49）、请求附加字段（PR #50）、crate 拆分（PR #52）、通用化整理（PR #54）已完成；剩更多 provider |
| P10 | 与 pi 的能力对齐 | 已完成（最后一个是 PR #72）：上下文超长后恢复（PR #56）、容器沙箱（PR #57）、edit / write（PR #58）、grep / find / ls（PR #59）、推理强度与续跑（PR #60、#61、#65）、会话目录 / fork / 分支（PR #67、#68、#72）、扩展能力与消息队列、模型元数据与计量、幂等键、MCP 进度、可观测、行为评测框架（PR #72）；chat 的便利功能按 D13 不做 |
| P11–P13 | 内核与项目整理、按规则评审 | 已完成（PR #70、#71、#75、#77）；模块文件布局（PR #78） |
| P14 | 对照 pi durable 的小项与无人值守的缺口 | 已完成（PR #80）：工具结果上报用量（`ToolOutput::usage`，按工具名汇总进 `RunReport::tool_usage`，不计入 token 预算）、撤回排队的消息（`MessageQueues::withdraw`）、定时巡检保留会话文件、被拒调用计数（`deniedCalls`）与 `run --fail-on-denied`（退出码 9）、成本预算 `--cost-budget`（退出码 8，单位与 `[[models]]` 的单价相同）；对照基准为 pi `a92283056` 的 `packages/durable`。之后成本字段改为不带币种的 `totalCost`（PR #82；事件 `schemaVersion` 升为 2） |

## 5. 待办与已知缺口

| 事项 | 状态 | 说明 |
|---|---|---|
| 更多 provider（如 Anthropic Messages API） | 未开始 | ⚑ D:5/CHINA FLAG: DeepSeek 经 OpenAI 兼容接口直接可用，不单独写 provider；服务端的设置经 `--request-field` 传递 |
| 行为评测的真实运行 | 待授权 | 框架在 `tests/common/evals.rs`，合成运行已通过；付费运行需用户设置 `ZX_E2E_AUTHORIZED` |
| 真实 IM 平台验收 | 待凭据 | 在 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次 @、回答与审批；换 HTTP 客户端与 TLS（D9）之后，对这两个平台的 HTTPS 连接没有验证过 |
| 真实集群验收 | 待部署方 | 常驻 Pod 部署包与多集群配置没有在真实集群上执行过；只读规则只看命令字面 |
| 真实 MCP 服务器验收 | 待选定服务器 | 对一个 Streamable HTTP 服务器跑通列出、调用与审批；HTTPS 的 MCP 服务器未测 |
| ACP 客户端实测 | 待做 | 只用手写客户端与 SDK 示例客户端验证过，没有在 Zed 或 Wisp 上用过 |
| 增量摘要是否改为默认 | 待定 | 默认仍为全量模式 |
| 增量摘要丢失仍成立的细节 | 不修复（用户决定） | 旧事实只能整条保留或整条取代；一条事实把仍成立的细节和会变化的对象写在一起，后者变化时细节随之离开上下文。评测三次都出现 |
| IM 服务崩溃时丢失已确认、还没处理的消息 | 已知缺口 | 重启后会替换遗留的"处理中…"（PR #46）；消息本身仍丢失。持久化收件需要新的记录格式与重放规则，等多副本或实际丢消息时再评审 |
| 输入被钩子改写时的崩溃窗口 | 已知局限 | 原始输入与替换它的 `context_edit` 分两次追加；其间进程崩溃，原始输入会出现在投影中 |
| 会话写入不 `fsync` | 已知局限 | 只调用 `flush`，断电时已写入的记录可能没有落盘 |
| 打开大会话的耗时 | 暂缓 | 加载时每条请求记录都要构建一次分支路径，耗时高于线性 |
| 保留近期用户消息原文；摘要前把较早的长工具结果换成引用 | 待评估 | 增量摘要之外的两项上下文管理改进 |
| 审计中写明自动放行命中的前缀 | 暂缓 | 要改 `PolicyDecision` 的 JSON 契约（`tests/hook_contract.rs`） |
| `cli::prepare` 拆成配置、进程级运行时与每会话三层 | 暂缓 | 改动面大 |
| FauxProvider 在增量之间响应取消、可脚本化带 `ProviderError` 的失败 | 暂缓 | 有测试需要时再做 |
| provider 契约的兜底 | 暂缓 | 取消后给 provider 的宽限期；`Done` / `Error` 与 `stop_reason` 的一致性检查 |
| `JsonlFile::create` 在加锁或写头部失败后留下文件 | 暂缓 | 这两种失败无法在测试中稳定构造 |
| `run` 的内置 profile 是否默认开启自动摘要 | 待用户决定 | unattended 与 troubleshoot 默认关闭，长时间排查撞到上下文上限时以退出码 1 结束；开启后可从上下文过长中恢复，代价是摘要丢失信息 |
| 定时巡检的会话文件不自动清理 | 已知局限 | 每次运行一个文件，在卷 `zx-agent-inspect-sessions` 上累积，需要部署方定期删除 |
| 失败运行后排队输入的处置 | 待评估 | pi 的 `afterFailedRun`（`continue` / `withdraw`）；现在由宿主在运行结束后用 `take_queued` 自行处置 |
| 工具触发的 `handoff` | 待评估 | pi 的 `control.handoff`；zx 没有 `reset` 条目，要先定会话格式 |
| 对照 pi durable 的较大项 | 待评审 | 排队消息落盘（同时关掉 IM 丢消息的缺口）、运行中部分结果落盘、后台摘要与摘要钩子、按会话的扩展选择与工作目录、扩展的可读状态、延迟加载的工具与 `tool_search`、持久化的睡眠与定时、宿主侧的环境接口、SQLite 等存储后端 |

## 6. 验证记录

只记录付费或真实环境上的运行；常规测试每个 PR 都跑。

| 日期 | 运行 | 结果 |
|---|---|---|
| 2026-11-01 | `tests/real_api.rs`，⚑ D:5/CHINA FLAG: DeepSeek（`https://api.deepseek.com/v1`，`collapse` 模式），⚑ D:5/CHINA FLAG: `deepseek-flash` 与 `deepseek-chat` 各一次 | 均为 6 项通过；不含压缩用例 |
| 2026-11-02 | `run` 经 `https://api.deepseek.com` 调用 ⚑ D:5/CHINA FLAG: `deepseek-flash`（PR #50 的 `--request-field`：默认、关闭思考、`reasoning_effort="low"`），不带工具 | 均以 0 退出、回答为 `ok`；关闭思考时输入 token 不同，说明字段生效 |
| 2026-11-02 | 摘要评测 `summaries_over_eight_rounds_of_a_scripted_session`（全量与增量各摘要 8 轮），两个模型各三次 | ⚑ D:5/CHINA FLAG: `deepseek-chat` 三次通过。⚑ D:5/CHINA FLAG: `deepseek-flash` 三次失败：第一次因摘要回答的大小上限（PR #48 修正）；后两次因推理用满默认 2048 token 的输出预算。用 `ZX_E2E_SUMMARY_TOKENS` 调高预算后尚未运行 |

## 7. 已决定不做

- `--output-schema`、异步审批（`WebhookInteraction`）、OS 级沙箱、由 webhook 或告警触发运行（D10）
- rpc 模式（D14）；chat 的便利功能（D13）
- durable 的自动续跑与重放、codemode 与 `runToolCall`、工具结果中的图片与 `structuredContent`（README"与 pi 的差异"）
- MCP 的 stdio、OAuth 与运行中更新工具列表（D8）
- 完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时

## 8. 推进方式

- 设计有变化时，先更新本文档，再改代码。
- 阶段或待办完成时，在 PR 里直接把状态写成"已完成（PR #编号）"。过程记录写在 PR 描述里，不写进本文档。
- 检查命令见 `.claude/CLAUDE.md`；`tests/cli.rs` 中 `run` 的测试不改即通过，以此保证兼容。
- 需要新依赖时，先确认在 D2 的授权范围内；范围之外的要先征得同意。

---

ROADMAP.md · v0.13 · 2026年11月
