# zx-agent 路线图

本文档只记录现状。设计有变化时先更新本文档再改代码；过程在 PR 与 git 历史里。用法见 [README](../README.md)。

## 1. 目标

通用内核加场景配置：一个内核支撑人机对话（`chat`、`zx-im`）、无人值守（`run`）与被嵌入（`acp`、Rust SDK）。

- G1：各前端共用同一个 `Session`，内核中没有“是不是交互模式”的判断。
- G2：新增场景只需新增 agent 文件、提示词或插件，不改内核。
- G3：`.claude/CLAUDE.md` 的不变量全部保留，每次模型请求都能从会话文件还原。

## 2. 设计原则

机制在内核、策略在外层；没有人时拒绝；会话日志是唯一事实来源，只追加；扩展优先用 MCP 与 ACP；纵深防御；没有第二个使用者就不抽象。

## 3. 已定决策

每条是现行规则；细节以所指的文档与 rustdoc 为准。

| 编号 | 现行规则 |
|---|---|
| D2 | 已授权的依赖：`toml`、`rmcp`、ACP 官方 Rust SDK（`agent-client-protocol`，只开 `stdio`）、`opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp`、`chrono`（只开 `clock`）、`jsonschema`（`zx-core` 默认开启的 feature `json-schema`）、`thiserror`、`tokio-tungstenite`。reqwest 0.13，rustls 只用 aws-lc-rs 一个加密后端（规则 07）；HTTPS 的根证书用系统信任库，IM 的 WebSocket 长连接用编译进程序的根证书（`tokio-tungstenite` 的 `rustls-tls-webpki-roots`）。serde_json 开启 `preserve_order`（规则 01）。其他新依赖与升级先征得同意 |
| D4 | 会话 API 是进程内方法调用，不做提交队列 / 事件队列（SQ/EQ） |
| D5 | 命令默认在本机执行；容器执行在 `config.toml` 中显式开启。理由：Kubernetes Job 的 Pod 内通常没有 Docker |
| D8 | 只做 MCP 客户端：只接工具，只用 Streamable HTTP；MCP 服务器与领域工具由外部实现 |
| D10 | 通用，不为场景定制：某个环境、平台或厂商的知识放在 agent、部署清单或配置里；外部系统经适配器调用 `run`、SDK 或 IM，zx-agent 不解析其私有格式；provider 的协议方言显式开启。例外：回答中的推理内容按它到达的字段（`reasoning_content` 或 `reasoning`）原样回传给同一 provider 名与模型，不设开关，因为它只回放该服务自己发来的内容；两个地址配置了同一 provider 名与模型时会互相回传 |
| D11 | 容器是沙箱：限制放在外墙上，容器内默认可写、联网，命令不以 root 运行，审批由 agent 决定；只读、无网络是可选的严格模式。外墙与默认值见 [SECURITY.md](SECURITY.md#容器执行) |
| D12 | chat 不给 `--session` 时把会话存到主目录的会话目录（路径与权限见 [REFERENCE.md](REFERENCE.md#chat)），`--no-session` 只存在内存；`run` 只在给出 `--session` 时写文件 |
| D13 | 界面交给 ACP 客户端：`acp` 只实现 agent 一侧；`chat` 冻结为应急与脚本化入口，不再加功能；`run` 保留并维护 |
| D14 | 不做 rpc 模式：非 Rust 宿主经 ACP，无人值守经 `run`，常驻服务经 `zx-im` 与 SDK |
| D15 | 只读写当前会话格式（`SESSION_VERSION`），其他版本的会话一律拒绝创建与加载；格式变化时提高版本号，不写升级代码 |
| D16 | IM 按人的对话直觉：私聊整段一个会话，群聊一个线程一个会话；机器人参与过的线程里不 @ 它的消息只记作上下文，刚被它回答的人接着说不必 @；不读频道历史，不加开关。细节见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D17 | `config.toml` 是环境，agent 文件是角色，都在主目录；agent 的 front matter 用 TOML。没有项目级配置文件，项目内容不能改变环境或权限；主目录的 `agents/<名字>.md` 替换同名内置 agent，没有 `--agent`。键与规则见 [CONFIGURATION.md](CONFIGURATION.md) |
| D18 | 没有命令白名单、幂等键与增量摘要：摘要每次读取原始记录，上限见 [CONFIGURATION.md](CONFIGURATION.md#上下文管理) |
| D19 | CLI 精简：chat 没有执行时间线、状态行、fork 与会话树命令（会话树与复制在 SDK）；otel 只导出 span；审批等待只有 `config.toml` 的 `approvalTimeoutSecs` 一个设置；`--session` 只属于 `run` 与 `chat`；`[[mcp]]` 没有不看进度的总超时 |
| D20 | 会话可以起名（pi 的 `/name`）：追加 `session_info` 记录，不改头部与文件名，不进入模型上下文，不能清除；只经 ACP 的 `/name` 与 SDK 的 `Session::set_name` 设置，chat 按 D13 不加命令；`session/set_title` 在协议中仍不稳定，不实现。细节见 [REFERENCE.md](REFERENCE.md) |
| D21 | 唯一入口是 `Agent`、`Session` 与由调用方读取的 `Run`；`Runner`、`StepLoop`、`LoopConfig` 与 `Toolbox` 为内核内部类型 |
| D22 | 摘要功能统一叫 compaction：操作本身及其记录、模块、类型与 ID 都叫 compaction；summary 只指 compaction 产出的文本、文本的限额，以及向模型要这段文本的请求（`kind: summary`）；任务请求叫 task（`kind: task`） |
| D23 | 领域词只有 Session、Run、Step、ToolCall 四层：一次任务请求及其工具调用叫 step，不叫 turn；ACP 协议与 chat 界面里的 turn 指一次提问到回答。运行结果用过去分词：`completed`、`truncated`、`failed`、`cancelled`、`timed_out`；预算用完是 `budget_exhausted`，并写明是哪一项预算，因为退出码、IM 回复与 ACP 的停止原因都要逐个区分预算。会话的记录树叫 transcript |
| D24 | 库与接入方式分开：SDK `zx-agent` 只把 `zx-core`、`zx-ai`、`zx-ai-openai` 与 `zx-builtin` 再导出成一个库，不含命令行解析、ACP、追踪导出与 IM；命令行 `zx-cli`（可执行文件仍叫 `zx-agent`）、IM 服务 `zx-im` 与新的接入方式（如 Web 服务）是建在库上的独立 crate，要共用 `config.toml`、agent 文件、追踪导出与收尾的经应用层 `zx-app`，它不依赖命令行解析、ACP 与 IM。没有业务含义的共用小工具在 `zx-util`，不属于 API；测试设施在只作测试依赖的 `zx-testing`，测试替身经 feature 打开、不进生产 API（见 [TESTS.md](TESTS.md)）。各 crate 的职责见 [README](../README.md#架构) |
| D25 | 会话写入失败、超时或写入中被丢弃后自行关闭，由 `Agent` 从存储重开。文件工具按路径排队修改（边界见 [SECURITY.md](SECURITY.md#工具)），队列由宿主传入，一个进程的所有会话共用一个；取消调用不提前让出 |
| D26 | 预算与计数按整次运行累计，重试与溢出恢复共用；续写是否还有预算由内核判断。配置费用预算后，任务请求在钩子选定最终模型后校验单价，自动、手动摘要与溢出恢复也校验；无法计价时在发送前失败 |
| D27 | IM 的路由只分派消息；运行、排队与空闲的会话数以及每个会话待处理消息的数量与文本字节都有上限。打开会话都要先取得运行名额；运行结束时没读到的插话留给下一次运行；第一次为线程建会话前先读线程历史。行为见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D28 | 宿主自己的会话索引从会话头部建立（`SessionStorage::headers`），JSONL 存储不缓存；IM 服务启动时这样建立对话到会话的索引，此后只由本进程维护。宿主的配额作为输入的凭证（`PendingInput::held`）随消息转移，内核只携带、不计量 |
| D29 | 内核只留机制：内核定义记录格式与存储接口，自带的存储只有 `MemoryStorage`，会话文件（JSONL）的存储在 zx-builtin；只有一个程序或一种存储用的代码也不在内核（如按会话 ID 前缀找会话在 chat）；内核的文档与报错用 Rust 字段名，不提命令行参数与 agent 文件的键；只有内核写的审计记录（请求、摘要、工具调用的开始）没有公开的写入方法。应用层 `zx-app` 只放各程序都要的配置装配与收尾，不打印，要说的作为返回值交给宿主，措辞不提子命令与命令行参数；只有一个程序用的行为留在那个程序里。用法见 [LIBRARY.md](LIBRARY.md#与命令行共用配置) |
| D30 | 宿主经 `SessionSetup::section_names` 声明它给出的提示词段名，内核不知道命令行的段名。自动摘要只在请求得到回答、摘要却不可用时暂停，出错、断流或超时不暂停；随上下文窗口的 token 阈值按会话当时的模型取窗口（同 pi 的 `shouldCompact`）。IM 的每个平台是 `zx-im` 的 `platform` 下的同层模块，互不引用，带着自己的 `im.toml` 段；⚑ D:5/CHINA FLAG: 飞书在默认开启的 feature `feishu` 后（关掉时见 [CONFIGURATION.md](CONFIGURATION.md#imtoml)） |
| D31 | 模型请求的失败由 provider 归类：模型层 `zx-ai` 只定义中立的种类（`ProviderErrorKind`），重试与上下文溢出恢复只看种类，没有种类的失败不重试；HTTP 状态、错误码、`finish_reason` 与措辞属于哪一类，是协议与厂商的知识，只写在 provider 的 crate 里。种类与处理见 [REFERENCE.md](REFERENCE.md#模型请求失败) |
| D32 | `zx-builtin` 与内核同一规则（D29）：设置类型自己校验，文档与报错用自己的字段名与语义，`zx-app` 把错误换成 `config.toml` 的键，并给出程序的默认值（如落盘目录）；执行器自己决定 shell，并为提示词描述自己；命令策略的程序清单宿主可以追加，续写的提示语可以替换，都不新增配置键。用法见 [LIBRARY.md](LIBRARY.md) |
| D33 | IM 服务收到的每条消息先写入 backlog（`zx-im` 的 trait `Backlog`；程序用会话目录下的文件实现，不新增依赖），再确认投递，写入会话、已回复或有意丢弃之后才删除；去重也只由它做，处理完的消息仍被认出一段时间（文件实现为 24 小时）。消息的凭证与配额一起作为输入的 hold 交给内核（D28）：`Session::send` 也接收带 hold 的输入，写入会话后才释放。停止时还没写入会话的消息留到下次启动，不再回复；启动时先按到达顺序处理它们。崩溃或停止恰好落在写入会话与删除记录之间时重复处理一次，先接受，不为此改会话格式。多副本时 backlog 与会话存储、对话归属、状态记录一起换成数据库实现。行为见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D34 | 宿主经 `zx_app::Application` 给 agent 加自己的部件，只用两个动词：`add_*` 在装配自带的旁边加一个（工具、钩子、策略、扩展、提示词段落），`set_*` 换掉唯一的那个（摘要器、对命令隐藏的变量、追踪导出）；会话存储是宿主交给 `build` 的 `SessionPlace`，宿主自己的存储用 `SessionPlace::from_storage`。宿主的部件与自带的同等对待：它的工具像内置工具，由 agent 文件的 `tools` 启用、受 `[permissions]` 约束；它的扩展像 MCP 服务器，工具对所有 agent 提供、默认询问；钩子按阶段执行，脱敏总在宿主的钩子之后。名字与内置的或已有的部件重复时 `build` 报错。由配置按名字选实现的 `register_*`（provider、执行环境、IM 平台）还没有做，随第二个 provider 一起做。用法见 [LIBRARY.md](LIBRARY.md#与命令行共用配置) |

## 4. 当前阶段

P0–P5、P10–P14（P10 为与 pi 的能力对齐）与 P16–P18 已完成，之后几轮评审定下的规则见 D25–D32。进行中：

- P6 执行环境、P7 扩展与 MCP、P8 `acp`：已实现，待真实集群、IM 平台、MCP 服务器与 ACP 客户端（Zed 或 Wisp）上的验收，见第 5 节。
- P9 可选增强：剩更多 provider（如 Anthropic Messages API），未开始；⚑ D:5/CHINA FLAG: DeepSeek 经 OpenAI 兼容接口可用，不单独写。
- P15 IM 对话模型（D16）：Slack 实现了读线程、查名字与回执；⚑ D:5/CHINA FLAG: 飞书只做消息分类，这三项沿用默认（不读历史、用 open_id、不回执），待按需补上。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：补上后会读取该平台上的话题历史与用户名称] Subject to Legal/Export Compliance approval。待在真实 Slack 工作区验证线程回复事件带 `thread_ts`（官方事件页未写明）。

## 5. 待办与已知缺口

- 真实 IM 平台验收（待凭据）：在 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次 @、回答与审批；换用 reqwest 0.13 与 rustls 后，对这两个平台的 HTTPS 连接没有验证过。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书租户中的验收经该平台收发消息] Subject to Legal/Export Compliance approval。
- 真实集群验收：常驻 Pod 部署包与多集群配置没有在真实集群上执行过；只读规则只看命令字面。
- 真实 MCP 服务器验收：对一个 Streamable HTTP 服务器跑通列出、调用与审批；HTTPS 未测。
- ACP 客户端实测：只用手写客户端与 SDK 示例客户端验证过。顺带确认客户端是否发 `session/close`（不发时会话留到进程退出，见 [REFERENCE.md](REFERENCE.md#acp)），再考虑像 `zx-im` 一样空闲释放。会话自行关闭之后，`session/prompt` 以“会话未打开”应答，客户端要先 `session/load` 或 `session/resume`；要自动重开，得记下客户端认为打开着的会话编号，未做。
- 单进程是部署边界：IM 的 backlog（D33）、对话索引与状态记录都只在本进程的会话目录里，对话索引只由本进程更新，随机器人参与过的对话增长，不淘汰。多副本之前要换成共享的实现，并定义会话所有权与接管。
- ⚑ D:5/CHINA FLAG: 飞书群聊中 @ 任意机器人都算 @ 本机器人：事件不指出被 @ 的机器人是否属于本应用，能否取得本机器人的 ID 未核实。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块的消息路由（软件/代码）] Subject to Legal/Export Compliance approval。
- 输入被钩子改写时，原始输入与替换它的 `context_edit` 分两次追加，其间崩溃时原始输入会留在投影中。
- 会话写入只 `flush`、不 `fsync`，断电时已写入的记录可能没有落盘。
- 模型请求失败的文本照录 reqwest 的错误与服务端的回应：传输失败带请求的完整 URL，HTTP 错误带服务端原文，都不经脱敏写入会话与事件；`baseUrl` 不能带查询串，写在它的路径里的密钥仍会被记录。
- MCP 工具的参数 schema 没有大小上限，原样交给模型；工具名与描述有上限。
- JSONL 存储不缓存：`headers` 每次读取查询范围内每个会话文件的头部，`list` 每次回放匹配的会话；会话很多、ACP 客户端频繁列出时，再引入磁盘索引或数据库。
- 每一步多次重建并复制整个投影（构造请求、比较系统提示词、检查摘要阈值等各取一次）；会话很长时再优化。
- 待评估：保留近期用户消息原文、摘要前把较早的长工具结果换成引用（可推迟碰到 2 MiB 上限）；失败运行后排队输入的处置（pi 的 `afterFailedRun`，现由宿主用 `take_queued` 处置）；工具触发的 `handoff`（pi 的 `control.handoff`，要先定会话格式）。
- 暂缓：审计中写明自动放行命中的前缀（要改 `PolicyDecision` 的 JSON 契约）；FauxProvider 响应增量之间的取消；provider 取消后的宽限期与 `stop_reason` 一致性检查；`JsonlFile::create` 失败后留下文件。
- 待用户决定：`run` 的内置 agent（及部署的 `unattended.md`）是否默认开启自动摘要；现在关闭，撞到上下文上限时以退出码 1 结束，开启的代价是摘要丢失信息。
- 对照 pi durable 的较大项（待评审）：排队消息落盘（IM 服务丢消息已由 D33 的 backlog 解决）、部分结果落盘、后台摘要（宿主已可用 `Summarizer` 自己产出摘要，钩子仍不提供摘要）、按会话的扩展（按会话的工具、钩子、提示词段落与工作目录已由 `SessionSetup` 给出）、扩展的可读状态、延迟加载的工具与 `tool_search`、持久化的睡眠与定时、宿主侧的环境接口、附带的数据库存储（接口 `SessionStorage` 已有，实现由宿主给出）。
- 对照 pi 的 Discord 接入（待评估）：消息附件的进出、停机期间漏掉的 @ 的补读（按对话的游标读平台历史；backlog 只管已送达的消息）、`/status`。Slack 线程里其他机器人的消息已能听到（见 [REFERENCE.md](REFERENCE.md#im-服务)），其形态依据 Slack 文档，只读 `text` 与附件的文字、不读 `blocks`，没有在真实工作区验证。
- 应用层 `zx-app` 尚无真实 Web 宿主验收。
- 执行环境扩展前，需让执行器提供文件资源身份：共享宿主挂载的同一文件共用键，容器/远程私有文件带环境域，并在目标文件系统解析别名；当前锁按宿主路径协调。
- 集成测试还没有全放进被测的 crate（规则 06）：`crates/zx-cli/tests/` 的 `container.rs`（44 个测试中 33 个）与 `crash_recovery.rs`（11 个中 9 个）有只用库的测试，与运行程序的测试同在一个文件；`container.rs` 的两种测试共用假 docker，CI 用 `--test container` 跑真实 Docker，拆分要先把假 docker 移进 `zx-testing`。
- 容器里的文件写入在调用被丢弃后仍写完，只用假 docker 验证过。
- 落盘目录的符号链接检查与随后的打开之间有竞态：同时在跑的命令可以在两者之间换掉目录。要消除得从工作目录起按目录描述符逐级打开，未做。
- 没有确定性测试的行为：chat 在会话自行关闭后重开（子进程中途写入失败无法确定性地构造，只测了从错误链认出会话已关闭）；尾记录修复在写线程上进行时，卷无响应不阻塞运行时（只测了修复本身）；MCP 放弃通知的 10 秒超时；`acp` 的建会话、载入、续接与列出在派发循环之外运行，慢的会话准备不挡其他请求。

## 6. 验证记录

只记录付费或真实环境上的运行；常规测试每个 PR 都跑，CI 覆盖 Linux 与 macOS，本地使用 macOS。

⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: DeepSeek：下表前三行的运行把请求发往该服务的 API]

| 日期 | 运行 | 结果 |
|---|---|---|
| 2026-11-01 | `crates/zx-agent/tests/real_api.rs`，⚑ D:5/CHINA FLAG: DeepSeek `https://api.deepseek.com/v1`，⚑ D:5/CHINA FLAG: `deepseek-flash` 与 `deepseek-chat` 各一次 | 均为 6 项通过；只覆盖 `collapse` 模式，不含压缩用例 |
| 2026-11-02 | `run` 经 ⚑ D:5/CHINA FLAG: `https://api.deepseek.com` 调用 `deepseek-flash`，附加请求字段取默认、关闭思考、`reasoning_effort="low"` | 均以 0 退出；关闭思考时输入 token 不同，字段生效 |
| 2026-11-02 | 摘要评测，两个模型各三次 | ⚑ D:5/CHINA FLAG: `deepseek-chat` 三次通过；⚑ D:5/CHINA FLAG: `deepseek-flash` 因推理用满默认 2048 token 的输出预算而失败，调高预算后尚未运行 |
| — | `provider.systemUpdates = "inline"` | 只在一个服务上验证过 |
| — | 真实 Docker（`ZX_DOCKER_TEST_IMAGE`，`crates/zx-cli/tests/container.rs`） | macOS 的 Docker Desktop 上用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像跑过；原生 Linux 只由 CI 覆盖；没有在 rootless Docker 或 Podman 上验证 |
| 2026-11-01 | `deploy/kubernetes/` 在本机 Docker Desktop 的 Kubernetes v1.36.1（2 个 arm64 节点，local-path ext4），假模型 | 镜像、只读账号、run、chat 审批、`operator.yaml` 的每项写操作、定时巡检、脱敏、会话文件锁与清理均符合预期；多集群由同一集群扮演远端，新增集群约 1 分钟后生效。读 agent 自身 `/proc/<pid>/environ` 被拒一项在 agent 不再设为不可 dump 后不成立，未重测 |
| — | IM 组件（当时名为 `serve`，此后改为子命令 `im`、再拆为程序 `zx-im`，都未重测），Slack 指向不可达地址 | 启动、`/proc/1/environ` 中没有 API key、重试日志不含凭据、SIGTERM 后退出码 0、孤儿进程被回收、与 `multicluster` 组件共存 |
| — | 定时巡检的会话卷与 `--fail-on-denied` 的说明 | 只经 `kubectl kustomize` 本地渲染检查，没有在集群上运行 |
| — | `deploy/kubernetes/` 的 `zx-agent-cluster-view`、按命名空间授予的 `operator.yaml`（此前验证的是全集群的 ClusterRoleBinding）、巡检前清理 30 天前的会话文件、`/tmp` 的 1Gi 上限、`part-of` 标签 | 只经 `kubectl kustomize` 本地渲染检查，清理命令只在 macOS 的 sh 与 find 上试过；没有在集群上运行 |

未验证：x86_64 镜像与其他集群、推送到镜像仓库、真实模型下的部署、真正分属两个集群的多集群访问、kubeconfig 的 exec 插件凭据、真实的 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户。

## 7. 已决定不做

- `--output-schema`、异步审批、OS 级沙箱、由 webhook 或告警触发运行（D10）
- rpc 模式（D14）；chat 的便利功能（D13）
- durable 的自动续跑与重放，工具结果中的图片、`structuredContent` 与 `runToolCall`（见 REFERENCE“与 pi 的差异”），codemode
- MCP 的 stdio、OAuth 与运行中更新工具列表（D8）
- 完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时、A/B 行为评测框架

---

ROADMAP.md · v0.44 · 2026年11月
