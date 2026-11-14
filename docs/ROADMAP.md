# zx-agent 路线图

本文档只记录现状。设计有变化时先更新本文档再改代码；过程在 PR 与 git 历史里。用法见 [README](../README.md)。

## 1. 目标

通用内核加场景配置：一个内核支撑人机对话（`chat`）、无人值守（`run`）与被嵌入（`acp`、Rust SDK）。

- G1：三个前端共用同一个 `Session`，内核中没有“是不是交互模式”的判断。
- G2：新增场景只需新增 agent 文件、提示词或插件，不改内核。
- G3：`.claude/CLAUDE.md` 的不变量全部保留，每次模型请求都能从会话文件还原。

## 2. 设计原则

机制在内核、策略在外层；没有人时拒绝；会话日志是唯一事实来源，只追加；扩展优先用 MCP 与 ACP；纵深防御；没有第二个使用者就不抽象。

## 3. 已定决策

| 编号 | 现行规则 |
|---|---|
| D2 | 已授权的依赖：`toml`、`rmcp`、ACP 官方 Rust SDK（`agent-client-protocol`，只开 `stdio`）、`opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp`、`chrono`（只开 `clock`）、`jsonschema`（`zx-core` 默认开启的 feature `json-schema`）、`thiserror`、`tokio-tungstenite`。reqwest 0.13，rustls 只用 aws-lc-rs 一个加密后端，根证书用系统信任库。ACP SDK 打开了 serde_json 的 `preserve_order`。其他新依赖与升级先征得同意 |
| D4 | 会话 API 是进程内方法调用，不做提交队列 / 事件队列（SQ/EQ） |
| D5 | 命令默认在本机执行；容器执行在 `config.toml` 中显式开启。理由：Kubernetes Job 的 Pod 内通常没有 Docker |
| D8 | 只做 MCP 客户端：只接工具，只用 Streamable HTTP；MCP 服务器与领域工具由外部实现 |
| D10 | 通用，不为场景定制：某个环境、平台或厂商的知识放在 agent、部署清单或配置里；外部系统经适配器调用 `run`、SDK 或 IM，zx-agent 不解析其私有格式；provider 的协议方言显式开启 |
| D11 | 容器是沙箱：内部可写、默认联网、审批由 agent 决定，命令以映射的宿主机用户运行，不允许 root；外墙：不加 `--privileged`、不挂 `docker.sock`、不用宿主机的网络与 PID 命名空间、API key 与环境变量不进容器、会话文件在工作目录之外。只读、无网络是可选的严格模式 |
| D12 | chat 不给 `--session` 时把会话存到主目录的 `sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（新建的目录 0700、文件 0600），`--no-session` 只存在内存；`run` 只在给出 `--session` 时写文件 |
| D13 | 界面交给 ACP 客户端：`acp` 只实现 agent 一侧；`chat` 冻结为应急与脚本化入口，不再加功能；`run` 保留并维护 |
| D14 | 不做 rpc 模式：非 Rust 宿主经 ACP，无人值守经 `run`，常驻服务经 `im` 与 SDK |
| D15 | 只读写当前会话格式（`SESSION_VERSION` 为 11），其他版本的会话一律拒绝创建与加载；格式变化时提高版本号，不写升级代码 |
| D16 | IM 按人的对话直觉：私聊整段一个会话，群聊一个线程一个会话；机器人参与过的线程里不 @ 它的消息只记作上下文，刚被它回答的人接着说不必 @；不读频道历史，不加开关。细节见 `docs/REFERENCE.md` 的“IM 服务” |
| D17 | `config.toml` 是环境，agent 文件是角色，都在主目录（`~/.zx-agent`，`ZX_AGENT_HOME` 可改）；agent 的 front matter 用 TOML。没有项目级配置文件，项目内容不能改变环境或权限；主目录的 `agents/<名字>.md` 替换同名内置 agent，没有 `--agent`；权限用 ask / allow / deny。键与规则见 [CONFIGURATION.md](CONFIGURATION.md) |
| D18 | 没有命令白名单、幂等键与增量摘要；摘要只用全量模式，摘要源上限 2 MiB，超过后要 `/new` 开新会话 |
| D19 | CLI 精简：chat 没有执行时间线、状态行、fork 与会话树命令（会话树与复制在 SDK）；otel 只导出 span；审批等待只有 `config.toml` 的 `approvalTimeoutSecs`（默认 600 秒，chat、acp 与 im 共用）；`--session` 只属于 `run` 与 `chat`；`[[mcp]]` 没有不看进度的总超时 |
| D20 | 会话可以起名（pi 的 `/name`）：追加 `session_info` 记录，不改头部与文件名；以文件中最后一条为准，与分支无关，不进入模型上下文，复制分支时随分支上的记录复制。名称只有一行（控制字符换成空格、去掉首尾空白），不能为空，也不能清除。`zx-agent sessions` 与 `session/list` 有名称时显示名称。起名只经 ACP 的 `/name <名称>`（`available_commands_update` 声明，`session_info_update` 通知标题，不请求模型）与 SDK 的 `Session::set_name`；chat 按 D13 不加命令；协议中由客户端改标题的 `session/set_title` 仍不稳定，不实现 |
| D21 | 对外入口是 `Agent`（所有会话共用的角色 `AgentConfig`；创建、打开、查找、关闭会话）、`Session`（可克隆的会话句柄）与它返回的 `Run`（一次执行：事件流与结果，由调用方驱动）；循环的配置 `LoopConfig`、工具集合 `Toolbox`、`Runner` 与循环 `StepLoop` 是 crate 内部类型：一次运行每次尝试（首次，以及每次重试或上下文溢出恢复）运行一遍，不提供单独运行一次循环的接口，因为生产代码没有这种用法。预算与计数只有一份：运行持有 `RunAccounting`，借给每次尝试的循环，请求、用量、步数、续写与被拒绝的调用都直接记在运行上，预算按运行的累计检查，不在每次尝试前后复制。attempt 只表示尝试的序号（首次为 1，重试与上下文溢出恢复都会加一），如 `attempt_start`、`request_start` 事件的 `attempt` 与追踪属性 `zx.attempt`；重试的序号叫 retry（首次重试为 1），如 `retry_scheduled` 事件的 `retry` 与追踪属性 `zx.retry`，与 `max_retries`、`RunReport.retries` 一致 |
| D22 | 摘要功能统一叫 compaction：这次操作本身（触发、开始、结果、失败）及其记录、模块、类型、函数与 ID 都叫 compaction（`transcript::compaction`、`compaction::Compactor`，记录中指向 `compaction_started` 的字段叫 `compactionId`）；summary 只指 compaction 产出的文本、文本的限额，以及向模型要这段文本的请求（`request_start` 的 `kind: summary`、会话中的 `RequestSource::Summary`）；attempt 只用于运行的尝试（D21） |
| D23 | 领域词只有 Session、Run、Step、ToolCall 四层：一次任务请求及其工具调用叫 step（事件 `step_start` / `step_end`、钩子 `finish_step`、追踪中的 `step` span），不再叫 turn；ACP 协议与 chat 界面里的 turn 指一次提问到回答，保持原样。attempt 仍只表示运行的尝试（D21），其事件叫 `attempt_start` / `attempt_end`。运行结果用过去分词：`completed`、`truncated`、`failed`、`cancelled`、`timed_out`；三个上限沿用配置里的名字 `model_request_limit`、`token_budget`、`cost_budget`，因为退出码、IM 回复与 ACP 的停止原因都要逐个区分它们，且结果在 JSON 中保持为一个字符串。会话的记录树叫 transcript（模块 `transcript`、类型 `Transcript`）。事件格式版本当时为 4，P16 第五期提高到 5 |
| D24 | 库与接入方式分开：根包 `zx-agent` 是 SDK，只把 `zx-core`、`zx-ai`、`zx-ai-openai` 与 `zx-tools` 再导出成一个库，不依赖命令行解析、ACP、追踪导出与 IM。命令行是 `zx-cli`（可执行文件仍叫 `zx-agent`，带 `config.toml`、agent 文件与追踪导出），IM 服务是 `zx-im`；新的接入方式（如 Web 服务）是建在库上的独立 crate，不进库。集成测试按是否需要编译出的程序分两处：只用库的在根目录 `tests/`，驱动程序的在 `crates/zx-cli/tests/`，不按 crate 再分 |
| D25 | 评审之后的修正（P17）。内核：会话的记录写不进去时（写入失败、超时，或 `Run` 在写入途中被丢弃）自行关闭，由 `Agent` 从存储重新打开；摘要的边界可以落在最近一轮之内，那一轮的提问原样留在摘要之前（`compaction_started` 的 `keptInputEntryId`；会话格式版本仍是 11，它还没有随任何版本发布）；扩展在建 `Agent` 时展开并校验一次，所有会话共用；按 JSON Schema 校验工具参数是 `zx-core` 默认开启的 feature，关掉后内核只依赖 40 个 crate；`MemoryStorage::forgetting()` 在会话关闭后不再保留它。工具：`read_output` 不经符号链接读写；文件写入一旦开始就写完，调用被丢弃也一样；自动放行的前缀区分大小写；落盘的结果在原处留下第一页，`read_file` 不落盘；`provider.maxTokensField` 选择输出上限的字段名。接入：IM 的 `/new` 立即建会话；IM 的运行上限写在部署清单与示例里（100 次模型请求、3600 秒），内置 agent 不变；`acp` 处理 SIGINT 与 SIGTERM，`--no-session` 关闭的会话不留在内存；`zx-cli` 的代码都在库里。为省几行重复而引入回调或装箱 future 的共用函数不做（规则 05、09） |

## 4. 当前阶段

P0–P5 与 P10–P14（P10 为与 pi 的能力对齐）已完成。进行中：

- P6 执行环境：已实现，待真实集群与真实 IM 平台验收（第 5 节）。
- P7 扩展与 MCP：已实现，待真实 MCP 服务器验收。
- P8 `acp`：已实现，待在 Zed 或 Wisp 上实测。
- P9 可选增强：剩更多 provider。
- P15 IM 对话模型（D16）：Slack 实现了读线程、查名字与回执；⚑ D:5/CHINA FLAG: 飞书只做消息分类，这三项沿用默认（不读历史、用 open_id、不回执），待按需补上。待在真实 Slack 工作区验证线程回复事件带 `thread_ts`（官方事件页未写明）。
- P16 入口与接口重整：`Agent`、`Session`、`Run` 三个入口对象，模型、工具、会话存储、策略、钩子与摘要各有接口并可替换，命令行从库中拆出。七期均已实现，待合并；决策见 D21–D24。
- P17 评审之后的修正：对 P16 的四路评审列出的缺陷、简化与优化，已实现，待合并；决策见 D25，留下的缺口在第 5 节。

## 5. 待办与已知缺口

- 更多 provider（如 Anthropic Messages API）：未开始。⚑ D:5/CHINA FLAG: DeepSeek 经 OpenAI 兼容接口可用，不单独写。
- 真实 IM 平台验收（待凭据）：在 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次 @、回答与审批；换用 reqwest 0.13 与 rustls 后，对这两个平台的 HTTPS 连接没有验证过。
- 真实集群验收：常驻 Pod 部署包与多集群配置没有在真实集群上执行过；只读规则只看命令字面。
- 真实 MCP 服务器验收：对一个 Streamable HTTP 服务器跑通列出、调用与审批；HTTPS 未测。
- ACP 客户端实测：只用手写客户端与 SDK 示例客户端验证过。顺带确认客户端是否发 `session/close`：不发时，`acp` 建过的会话留在内存中、文件保持锁定到进程退出，再考虑像 `im` 一样空闲释放。
- IM 服务崩溃时丢失已确认、未处理的消息（重启后只替换遗留的“处理中…”）；持久化收件等多副本或实际丢消息时再评审。
- 输入被钩子改写时，原始输入与替换它的 `context_edit` 分两次追加，其间崩溃时原始输入会留在投影中。
- 会话写入只 `flush`、不 `fsync`，断电时已写入的记录可能没有落盘。
- 打开大会话的耗时高于线性：每条请求记录都要构建一次分支路径（暂缓）。
- 待评估：保留近期用户消息原文、摘要前把较早的长工具结果换成引用（可推迟碰到 2 MiB 上限）；失败运行后排队输入的处置（pi 的 `afterFailedRun`，现由宿主用 `take_queued` 处置）；工具触发的 `handoff`（pi 的 `control.handoff`，要先定会话格式）。
- 暂缓：审计中写明自动放行命中的前缀（要改 `PolicyDecision` 的 JSON 契约）；FauxProvider 响应增量之间的取消、可脚本化 `ProviderError`；provider 取消后的宽限期与 `stop_reason` 一致性检查；`JsonlFile::create` 失败后留下文件。
- 待用户决定：`run` 的内置 agent（及部署的 `unattended.md`）是否默认开启自动摘要；现在关闭，撞到上下文上限时以退出码 1 结束，开启的代价是摘要丢失信息。
- 定时巡检的会话文件不自动清理，需要部署方定期删除。
- 对照 pi durable 的较大项（待评审）：排队消息落盘（同时解决 IM 丢消息）、部分结果落盘、后台摘要（宿主已可用 `Summarizer` 自己产出摘要，钩子仍不提供摘要）、按会话的扩展（按会话的工具、钩子、提示词段落与工作目录已由 `SessionSetup` 给出）、扩展的可读状态、延迟加载的工具与 `tool_search`、持久化的睡眠与定时、宿主侧的环境接口、附带的数据库存储（接口 `SessionStorage` 已有，实现由宿主给出）。
- `config.toml`、agent 文件、追踪导出与按配置装配 agent 的那一层（`crates/zx-cli/src/cli/`）都在 `zx-cli` 里：嵌入方或新的接入方式（如 Web 服务）要复用它们得依赖整个命令行的 crate；有这样的使用者时再拆成独立 crate。
- 测试适配层经隐藏的 `Agent::adopt_session` 把测试给的 `Transcript` 变成会话。要删掉它，得让测试经存储建会话：约 80 处建会话的写法与 160 处 `build()` 要改成异步，收益只是少一个隐藏的方法，未做。
- 容器里的文件写入在调用被丢弃后仍写完，只用假 docker 验证过；`real_` 测试要真实 Docker，这一轮没有运行。
- 落盘目录的符号链接检查与随后的打开之间有竞态：同时在跑的命令可以在两者之间换掉目录。要消除得从工作目录起按目录描述符逐级打开，未做。
- 会话自行关闭之后，ACP 的 `session/prompt` 以“会话未打开”应答，客户端要先 `session/load` 或 `session/resume`；要让它自动重开，得记下客户端认为打开着的会话编号，未做。

## 6. 验证记录

只记录付费或真实环境上的运行；常规测试每个 PR 都跑，CI 覆盖 Linux 与 macOS，本地使用 macOS。

| 日期 | 运行 | 结果 |
|---|---|---|
| 2026-11-01 | `tests/real_api.rs`，⚑ D:5/CHINA FLAG: DeepSeek `https://api.deepseek.com/v1`，⚑ D:5/CHINA FLAG: `deepseek-flash` 与 `deepseek-chat` 各一次 | 均为 6 项通过；只覆盖 `collapse` 模式，不含压缩用例 |
| 2026-11-02 | `run` 经 `https://api.deepseek.com` 调用 ⚑ D:5/CHINA FLAG: `deepseek-flash`，附加请求字段取默认、关闭思考、`reasoning_effort="low"` | 均以 0 退出；关闭思考时输入 token 不同，字段生效 |
| 2026-11-02 | 摘要评测，两个模型各三次 | ⚑ D:5/CHINA FLAG: `deepseek-chat` 三次通过；⚑ D:5/CHINA FLAG: `deepseek-flash` 因推理用满默认 2048 token 的输出预算而失败，调高预算后尚未运行 |
| — | `provider.systemUpdates = "inline"` | 只在一个服务上验证过 |
| — | 真实 Docker（`ZX_DOCKER_TEST_IMAGE`，`crates/zx-cli/tests/container.rs`） | macOS 的 Docker Desktop 上用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像跑过；原生 Linux 只由 CI 覆盖；没有在 rootless Docker 或 Podman 上验证 |
| 2026-11-01 | `deploy/kubernetes/` 在本机 Docker Desktop 的 Kubernetes v1.36.1（2 个 arm64 节点，local-path ext4），假模型 | 镜像、只读账号、run、chat 审批、`operator.yaml` 的每项写操作、定时巡检、脱敏、会话文件锁与清理均符合预期；多集群由同一集群扮演远端，新增集群约 1 分钟后生效。读 agent 自身 `/proc/<pid>/environ` 被拒一项在 agent 不再设为不可 dump 后不成立，未重测 |
| — | IM 组件（当时名为 `serve`，改名为 `im` 后未重测），Slack 指向不可达地址 | 启动、`/proc/1/environ` 中没有 API key、重试日志不含凭据、SIGTERM 后退出码 0、孤儿进程被回收、与 `multicluster` 组件共存 |
| — | 定时巡检的会话卷与 `--fail-on-denied` 的说明 | 只经 `kubectl kustomize` 本地渲染检查，没有在集群上运行 |

未验证：x86_64 镜像与其他集群、推送到镜像仓库、真实模型下的部署、真正分属两个集群的多集群访问、kubeconfig 的 exec 插件凭据、真实的 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户。

## 7. 已决定不做

- `--output-schema`、异步审批、OS 级沙箱、由 webhook 或告警触发运行（D10）
- rpc 模式（D14）；chat 的便利功能（D13）
- durable 的自动续跑与重放、codemode 与 `runToolCall`、工具结果中的图片与 `structuredContent`（README“与 pi 的差异”）
- MCP 的 stdio、OAuth 与运行中更新工具列表（D8）
- 完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时、A/B 行为评测框架

---

ROADMAP.md · v0.23 · 2026年11月
