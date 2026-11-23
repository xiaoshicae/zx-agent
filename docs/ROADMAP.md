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
| D2 | 已授权的依赖：`toml`、`rmcp`、ACP 官方 Rust SDK（`agent-client-protocol`，只开 `stdio`）、`opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp`、`chrono`（只开 `clock`）、`jsonschema`（`zx-core` 默认开启的 feature `json-schema`）、`thiserror`、`tokio-tungstenite`、`base64`（图片的编码；0.22 已经经 `jsonschema` 编进 `zx-core`）、`image`（只开 `gif`、`jpeg`、`png`、`webp`：图片进入会话前的缩放与重新编码，D36）、`tokio-postgres`（zx-im 多副本的存储与主备选举，D44）与 `tokio-postgres-rustls`（只开 `aws-lc-rs`），以及为它的 TLS 直接依赖的 `rustls` 与 `rustls-platform-verifier`（2026-11-23 用户批准，都在 zx-im 默认开启的 feature `postgres` 后）。reqwest 0.13，rustls 只用 aws-lc-rs 一个加密后端（规则 07）；HTTPS 的根证书用系统信任库，IM 的 WebSocket 长连接用编译进程序的根证书（`tokio-tungstenite` 的 `rustls-tls-webpki-roots`）。serde_json 开启 `preserve_order`（规则 01）。其他新依赖与升级先征得同意 |
| D4 | 会话 API 是进程内方法调用，不做提交队列 / 事件队列（SQ/EQ） |
| D5 | 命令默认在本机执行；容器执行在 `config.toml` 中显式开启。理由：Kubernetes Job 的 Pod 内通常没有 Docker |
| D8 | 只做 MCP 客户端：只接工具，只用 Streamable HTTP；MCP 服务器与领域工具由外部实现 |
| D10 | 通用，不为场景定制：某个环境、平台或厂商的知识放在 agent、部署清单或配置里；外部系统经适配器调用 `run`、SDK 或 IM，zx-agent 不解析其私有格式；provider 的协议方言显式开启。例外：回答中的推理内容原样回传给同一 provider 名与模型，不设开关，因为它只回放该服务自己发来的内容：OpenAI 兼容服务按它到达的字段（`reasoning_content` 或 `reasoning`），Anthropic 是思考块连同签名；两个地址配置了同一 provider 名与模型时会互相回传 |
| D11 | 容器是沙箱：限制放在外墙上，容器内默认可写、联网，命令不以 root 运行，审批由 agent 决定；只读、无网络是可选的严格模式。外墙与默认值见 [SECURITY.md](SECURITY.md#容器执行) |
| D12 | chat 不给 `--session` 时把会话存到主目录的会话目录（路径与权限见 [REFERENCE.md](REFERENCE.md#chat)），`--no-session` 只存在内存；`run` 只在给出 `--session` 时写文件 |
| D13 | 界面交给 ACP 客户端：`acp` 只实现 agent 一侧；`chat` 冻结为应急与脚本化入口，不再加功能；`run` 保留并维护 |
| D14 | 不做 rpc 模式：非 Rust 宿主经 ACP，无人值守经 `run`，常驻服务经 `zx-im` 与 SDK |
| D15 | 只读写当前会话格式（`SESSION_VERSION`），其他版本的会话一律拒绝创建与加载；格式变化时提高版本号，不写升级代码 |
| D16 | IM 按人的对话直觉：私聊整段一个会话，群聊一个线程一个会话；机器人参与过的线程里不 @ 它的消息只记作上下文，刚被它回答的人接着说不必 @；在频道里直接 @ 机器人时，先读入频道里上次 @ 它之后的消息（至多 50 条）再回答，“上次”以平台上的消息为准，不另存游标；不加开关（2026-11-22 用户决定，原为不读频道历史）。细节见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D17 | `config.toml` 是环境，agent 文件是角色，都在主目录；agent 的 front matter 用 TOML。没有项目级配置文件，项目内容不能改变环境或权限；主目录的 `agents/<名字>.md` 替换同名内置 agent，没有 `--agent`；其他名字的文件只在宿主选用时读取，zx-im 由 `im.toml` 按聊天选（D39）。键与规则见 [CONFIGURATION.md](CONFIGURATION.md) |
| D18 | 没有命令白名单、幂等键与增量摘要：摘要每次读取原始记录，上限见 [CONFIGURATION.md](CONFIGURATION.md#上下文管理) |
| D19 | CLI 精简：chat 没有执行时间线、状态行、fork 与会话树命令（会话树与复制在 SDK）；otel 只导出 span；审批等待只有 `config.toml` 的 `approvalTimeoutSecs` 一个设置；`--session` 只属于 `run` 与 `chat`；`[[mcp]]` 没有不看进度的总超时 |
| D20 | 会话可以起名（pi 的 `/name`）：追加 `session_info` 记录，不改头部与文件名，不进入模型上下文，不能清除；只经 ACP 的 `/name` 与 SDK 的 `Session::set_name` 设置，chat 按 D13 不加命令；`session/set_title` 在协议中仍不稳定，不实现。细节见 [REFERENCE.md](REFERENCE.md) |
| D21 | 唯一入口是 `Agent`、`Session` 与由调用方读取的 `Run`；`Runner`、`StepLoop`、`LoopConfig` 与 `Toolbox` 为内核内部类型 |
| D22 | 摘要功能统一叫 compaction：操作本身及其记录、模块、类型与 ID 都叫 compaction；summary 只指 compaction 产出的文本、文本的限额，以及向模型要这段文本的请求（`kind: summary`）；任务请求叫 task（`kind: task`） |
| D23 | 领域词只有 Session、Run、Step、ToolCall 四层：一次任务请求及其工具调用叫 step，不叫 turn；ACP 协议与 chat 界面里的 turn 指一次提问到回答。运行结果用过去分词：`completed`、`truncated`、`failed`、`cancelled`、`timed_out`；预算用完是 `budget_exhausted`，并写明是哪一项预算，因为退出码、IM 回复与 ACP 的停止原因都要逐个区分预算。会话的记录树叫 transcript |
| D24 | 库与接入方式分开：SDK `zx-agent` 只把 `zx-core`、`zx-ai`、`zx-ai-openai`、`zx-ai-anthropic` 与 `zx-builtin` 再导出成一个库，不含命令行解析、ACP、追踪导出与 IM；命令行 `zx-cli`（可执行文件仍叫 `zx-agent`）、IM 服务 `zx-im` 与新的接入方式（如 Web 服务）是建在库上的独立 crate，要共用 `config.toml`、agent 文件、追踪导出与收尾的经应用层 `zx-app`，它不依赖命令行解析、ACP 与 IM。没有业务含义的共用小工具在 `zx-util`，不属于 API；两个 provider 共用的 HTTP 部分（设置检查、发请求、读事件流、请求失败）在 `zx-ai-http`，也不属于 API，协议的写法与失败的归类留在各 provider；测试设施在只作测试依赖的 `zx-testing`，测试替身经 feature 打开、不进生产 API（见 [TESTS.md](TESTS.md)）。各 crate 的职责见 [README](../README.md#架构) |
| D25 | 会话写入失败、超时或写入中被丢弃后自行关闭，由 `Agent` 从存储重开。文件工具按路径排队修改（边界见 [SECURITY.md](SECURITY.md#工具)），队列由宿主传入，一个进程的所有会话共用一个；取消调用不提前让出 |
| D26 | 预算与计数按整次运行累计，重试与溢出恢复共用；续写是否还有预算由内核判断。配置费用预算后，任务请求在钩子选定最终模型后校验单价，自动、手动摘要与溢出恢复也校验；无法计价时在发送前失败 |
| D27 | IM 的路由只分派消息；运行、排队与空闲的会话数以及每个会话待处理消息的数量与文本字节都有上限。打开会话都要先取得运行名额；运行结束时没读到的插话留给下一次运行；第一次为对话建会话前先读它之前的消息：别人发起的线程读线程，在频道里直接 @ 读频道（D16）。行为见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D28 | 宿主自己的会话索引从会话头部建立（`SessionStorage::headers`），JSONL 存储不缓存；IM 服务启动时这样建立对话到会话的索引，此后只由本进程维护。宿主的配额作为输入的凭证（`PendingInput::held`）随消息转移，内核只携带、不计量 |
| D29 | 内核只留机制：内核定义记录格式与存储接口，自带的存储只有 `MemoryStorage`，会话文件（JSONL）的存储在 zx-builtin；只有一个程序或一种存储用的代码也不在内核（如按会话 ID 前缀找会话在 chat）；内核的文档与报错用 Rust 字段名，不提命令行参数与 agent 文件的键；只有内核写的审计记录（请求、摘要、工具调用的开始）没有公开的写入方法。应用层 `zx-app` 只放各程序都要的配置装配与收尾，不打印，要说的作为返回值交给宿主，措辞不提子命令与命令行参数；只有一个程序用的行为留在那个程序里。用法见 [LIBRARY.md](LIBRARY.md#与命令行共用配置) |
| D30 | 宿主经 `SessionSetup::section_names` 声明它给出的提示词段名，内核不知道命令行的段名。自动摘要只在请求得到回答、摘要却不可用时暂停，出错、断流或超时不暂停；随上下文窗口的 token 阈值按会话当时的模型取窗口（同 pi 的 `shouldCompact`）。IM 的每个平台是 `zx-im` 的 `platform` 下的同层模块，互不引用，带着自己的 `im.toml` 段与由段建 channel 的 `PlatformFactory`（D34）；⚑ D:5/CHINA FLAG: 飞书在默认开启的 feature `feishu` 后（关掉时见 [CONFIGURATION.md](CONFIGURATION.md#imtoml)） |
| D31 | 模型请求的失败由 provider 归类：模型层 `zx-ai` 只定义中立的种类（`ProviderErrorKind`），重试与上下文溢出恢复只看种类，没有种类的失败不重试；HTTP 状态、错误码、`finish_reason` 与措辞属于哪一类，是协议与厂商的知识，只写在 provider 的 crate 里。种类与处理见 [REFERENCE.md](REFERENCE.md#模型请求失败) |
| D32 | `zx-builtin` 与内核同一规则（D29）：设置类型自己校验，文档与报错用自己的字段名与语义，`zx-app` 把错误换成 `config.toml` 的键，并给出程序的默认值（如落盘目录）；执行器自己决定 shell，并为提示词描述自己；命令策略的程序清单宿主可以追加，续写的提示语可以替换，都不新增配置键。用法见 [LIBRARY.md](LIBRARY.md) |
| D33 | IM 服务收到的每条消息先写入 backlog（`zx-im` 的 trait `Backlog`；程序用会话目录下的文件实现，不新增依赖），再确认投递，写入会话、已回复或有意丢弃之后才删除；去重也只由它做，处理完的消息仍被认出一段时间（文件实现为 24 小时）。消息的凭证与配额一起作为输入的 hold 交给内核（D28）：`Session::send` 也接收带 hold 的输入，写入会话后才释放。停止时还没写入会话的消息留到下次启动，不再回复；启动时先按到达顺序处理它们。崩溃或停止恰好落在写入会话与删除记录之间时重复处理一次，先接受，不为此改会话格式。多副本时 backlog 与会话存储、对话归属、状态记录一起换成数据库实现。行为见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D34 | 宿主经 `zx_app::Application` 给 agent 加自己的部件，只用三个动词：`add_*` 在装配自带的旁边加一个（工具、钩子、策略、扩展、提示词段落、对命令隐藏的变量），`set_*` 换掉唯一的那个（摘要器、追踪导出），`register_*` 登记由配置按名字选用的实现；会话存储是宿主交给 `build` 的 `SessionPlace`，宿主自己的存储用 `SessionPlace::from_storage`。宿主的部件与自带的同等对待：它的工具像内置工具，由 agent 文件的 `tools` 启用、受 `[permissions]` 约束；它的扩展像 MCP 服务器，工具对所有 agent 提供、默认询问；钩子按阶段执行，脱敏总在宿主的钩子之后。名字与内置的或已有的部件重复时 `build` 报错。内置的实现也经 `register_*` 登记：provider 经 `register_provider`，`config.toml` 的 `provider.api` 写它的名字；执行环境经 `register_executor`，`execution` 写它的名字，它声明是否隔离命令，`permissions.approveAll` 只能用于隔离的执行环境；IM 平台经程序 `zx-im` 的库中 `Program::register_platform`，`im.toml` 中服务自己的段以外，每个段配置以段名注册的平台。自建的 zx-im 程序经 `Program::customize` 用同样的动词给 `Application` 加部件，程序自己的部件在其后：平台的凭据与宿主隐藏的变量合并，不替换；追踪导出归程序。用法见 [LIBRARY.md](LIBRARY.md#与命令行共用配置) |
| D35 | 被取消的运行为整批工具调用都写下结果：还没开始的记为未执行，执行中被打断的按工具有无副作用说明；只有运行没能写下结果时（崩溃、宿主丢弃 `Run`、会话写入失败）与失败或被取消的回答里的调用，才由下一次运行补写（不变量 10）。被取消打断的钩子回调不算钩子失败，不发 `hook_failed`。行为见 [REFERENCE.md](REFERENCE.md) 的“会话文件” |
| D36 | 支持图片输入（2026-11-20 用户决定，原先第 7 节的“工具结果中的图片”不做取消）：用户消息与工具结果在文本旁带 `images`（base64 与媒体类型），不像 pi 那样与文本穿插；`[[models]]` 的 `input` 默认只有文本（沿用 pi），没声明 `"image"` 的模型由 provider 按请求把图片换成一句说明，会话里存原图；图片进入会话前由 `zx_agent::builtin::images` 按 pi 的上限适配。入口：`read_file` 与 MCP 的结果、ACP 提问、`run` 的 `@文件`、IM 附件（Slack 与 ⚑ D:5/CHINA FLAG: 飞书。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块下载用户发送的图片（软件/数据）] Subject to Legal/Export Compliance approval）。音频与视频不收（第 5 节）。行为见 [REFERENCE.md](REFERENCE.md#图片) |
| D37 | zx-im 可以不等人发消息就开始对话（2026-11-22 用户决定，原第 7 节“由 webhook 或告警触发运行”不做取消）：一个触发（`zx_im::Trigger`）在名单里的频道发一条消息开出线程，在线程里问给定的提问，此后回答、审批与接话都照常。来源是 `im.toml` 的 `[[schedules]]`（固定间隔，至少 60 秒，第一次在启动后一个间隔），以及宿主经 `Server::triggers` 或 `Program::on_triggers` 给出的触发。zx-im 仍只需出网，不开监听端口；告警等外部系统按 D10 由宿主的适配器接入。触发计入 `maxConversations`，不进 backlog，停机期间到点的不补。行为见 [REFERENCE.md](REFERENCE.md#im-服务) |
| D38 | MCP 客户端适合常驻（2026-11-23，P19 ②）。断线与会话失效：服务器停机期间的调用以错误结果交给模型，恢复后照常；服务器回 404 表示会话已失效时，rmcp 重新握手、重发这次请求；都不必重启宿主，测试覆盖两代协议。调用失败的错误不带 URL 的路径与查询（可能含凭据），也不带 rmcp 内部的类型名。工具只在启动时列出一次，不重新列出：说明与参数在会话打开时声明，放行规则在装配 agent 时定，中途更换要两处一起改；服务器换了工具后，重启宿主生效。请求头：`[[mcp]]` 的 `headers` 直接写值，`headerEnv` 从环境变量读值，与 `tokenEnv` 一样对命令隐藏、在输出中脱敏；传输自己用的头不能设，设了 `tokenEnv` 时不能再设 `Authorization`。只读标注：`trustReadOnly = true` 时信任该服务器的 `readOnlyHint`，标了只读的工具声明为只读（中断时不报“可能改过状态”），并且不询问；agent 的 `permissions.ask` 与 `deny` 仍可收紧。不设时标注只是提示，工具照旧询问 |
| D39 | zx-im 按聊天选 agent 与审批人（2026-11-23，P19 ③）：一个进程服务的聊天可以各有自己的角色，例如生产群用只读的 agent、只让值班的人审批，测试群用可写的。`im.toml` 的 `[[chats]]` 给名单里的一个聊天指定 `agent` 或 `approvers`：`agent` 是主目录 `agents/<名字>.md` 的名字（只能用字母、数字、`-` 与 `_`），工具、权限、上限与说明都随它；内置 agent 的名字照旧取用户的同名文件或内置的。`approvers` 替换 `access.approvers`，只能是该聊天所在平台的用户。没写的聊天用 `assistant` 与 `access.approvers`。选 agent 的只有部署配置：命令行仍没有 `--agent`（D17），项目内容也选不了。每个 agent 启动时各装配一次、试开一个会话；MCP 连接、会话目录与 `[limits]` 由所有聊天共用，工作目录与模型仍按进程。聊天换了 agent 后，已有对话在原会话里继续，按新 agent 的设置（说明的变化按不变量 6 追加）。库：`zx_im::ServerConfig::chats`（`ChatConfig`；其中的 agent 须把会话存在服务的 agent 的存储里，服务在那里找对话的会话）与 `zx_app::Application::build_named` |
| D40 | zx-im 按天的用量上限（2026-11-23，P19 ④）：`im.toml` 的 `[limits]` 给整个服务、`[[chats]]` 给一个聊天设每天（UTC）的 `dailyTokens`（输入加输出 token，以服务返回的为准）与 `dailyCost`（按 `[[models]]` 的单价，模型须有单价）。当天的用量用完后，新的工作（提问、触发与 `/compact`）不再开始：提问与 `/compact` 回一句 `replies.dailyLimit`，触发只记日志；正在进行的运行照常结束，所以一天可能超出正在进行的几次运行的用量，要硬上限时给 agent 设单次预算（`limits.tokenBudget`、`limits.costBudget`）。用量在每次运行结束时按运行报告计入，失败的尝试与摘要请求都算。只在内存中计数，重启后当天从零计；持久化与多副本共享随 ⑧ 与 backlog 一起做。不在内核里加跨运行的预算：天的边界、按聊天计与回复都属于 IM 服务，单次运行已有内核的预算 |
| D41 | zx-im 不重启就换名单（2026-11-23，P19 ⑤）：收到 SIGHUP 时重读 `im.toml`，服务自己的段按启动时的规则检查（平台以正在运行的为准），通过后 `[access]` 的 `conversations` 与 `approvers` 立即生效，记一条 `reloaded`；其他段（含 `[[chats]]`）的改动重启后才生效，检查它们是为了重启时不失败。读不了或检查不过时保留原名单，记一条 `failed`。移出名单的聊天，进行中的运行照常结束，之后的消息按名单外处理；移出的审批人此后点击无效。只在 Unix 上。用信号而不轮询文件：何时生效由运维决定，不会读到改了一半的文件。库：`Server::access` 给出的 `Access::replace` |
| D42 | zx-im 的运行指标（2026-11-23，P19 ⑦）：每次运行（提问的，或 `/compact` 的摘要）结束时，服务报一个 `ServerEvent::Ran`（`RunRecord`：对话、怎样结束、模型请求数、用量与耗时），程序把它写成操作日志的一行 `run`；运行次数、失败、耗时、token 与花费由日志系统统计。不加依赖，也不开指标端口：zx-im 仍不监听端口（D37）；OpenTelemetry 指标等需要时再加，追踪照旧经 OTLP 导出 |
| D43 | zx-im 发文件（2026-11-23，P19 ⑥，用户选定方案 B）：平台接口 `ImChannel::send_file` 把文件作为附件发进对话，默认实现说这个平台不能发。Slack 经 `files.getUploadURLExternal` 与 `files.completeUploadExternal`，要 `files:write`；⚑ D:5/CHINA FLAG: 飞书先上传到 `im/v1/files` 再发 `file` 消息，要 `im:resource` 或 `im:resource:upload`，单个文件至多 30 MB，图片也作为文件发（⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块向该平台上传文件（数据）] Subject to Legal/Export Compliance approval）。用到它的有两处：比 `[limits]` 的 `longAnswerChars`（默认 8000 字符，约两条消息）更长的回答作为 Markdown 文件发出，状态换成 `replies.answerAttached`，发不出时照旧分段发文字；工具 `send_file` 发模型写的内容（`content`），或会话工作目录里的一个文件（`path`，至多 20 MiB）。后者不经脱敏，文件里的密钥会原样发进聊天，用户知情后选定。工具要知道发到哪个对话：zx-app 新增 `Application::add_session_tools`，宿主按会话的头部（含标签）给每个会话它自己的工具，这些工具在每个会话里都提供，不由 agent 的 `tools` 启用，`permissions.ask` 与 `deny` 照常约束；zx-im 用它给每个 IM 会话一个 `send_file` |
| D44 | zx-im 多副本：主备（2026-11-23，P19 ⑧，用户批准方案与依赖，见 D2）。所有副本都启动，在 Postgres 上争抢一把会话级 advisory lock，拿到的是主：连平台、跑定时、服务对话；其余待命。主退出或与库断开时锁自动释放，备接手后按重启的流程处理 backlog、替换遗留状态、在原会话里接着对话。防脑裂：每任主有递增的任期号，存储的每次写入用单条语句核对它（锁住任期那一行，新主改任期号要等这条写完），被取代的旧主写不进去，持锁的连接一断就停止服务。不做多活：平台把事件随机分给各连接，多活要在副本间转发消息与审批点击、按对话分配归属；吞吐也不是瓶颈。存进 Postgres 的：会话（内核的 `SessionStorage`，记录以会话 ID 与 `seq` 为主键，第二个写入者的追加因此失败）、backlog、运行状态、每天的用量（经新的 `UsageStore`，存在 Postgres 时不再随重启清零，D40），以及任期；表在启动时按版本建好，几个副本同时启动时先拿迁移锁。`im.toml` 的 `[sessions]` 写 `postgresUrlEnv`（存连接串的环境变量，对命令隐藏、在输出中脱敏）代替 `dir`；连接按串里的 `sslmode` 走 TLS，证书用系统信任库校验。会话的工作目录仍在各自的 Pod 里，接手后看不到之前写在那里的文件与落盘的长工具输出。代码在 zx-im 默认开启的 feature `postgres` 后。存储的测试在设了 `ZX_TEST_POSTGRES_URL` 时连真实的 Postgres 跑，CI 的 Linux 任务为此起一个 Postgres 容器（测试任务的矩阵含 macOS，用不了任务级的服务容器）。分两步：先是存储与配置（一个副本），再是选举、防脑裂与 Kubernetes 清单。第二步的做法：锁的键在建表时随机取、存在表里，共用一个库、各用一个 schema 的几套部署互不争抢；选举用一条专用连接，不随存储的连接重连（重连会悄悄丢掉锁）；主每隔几秒在这条连接上读一次任期，读不到、超时或任期变了即失去主的身份，服务端在这条连接上开 TCP keepalive，主所在的网络断开时锁在半分钟左右放出；失去主的身份时服务像收到 SIGTERM 一样停下，进程以退出码 1 结束，由 Kubernetes 重启成备；任期的核对写在库里的函数 `zx_im_fence` 中，不符时以单独的错误码报错，与“消息已收过”等结果分开；加了任期这张表，表的版本升到 2；配了 Postgres 时一个进程也先当选再服务，只有一条路 |

## 4. 当前阶段

P0–P5、P10–P14（P10 为与 pi 的能力对齐）与 P16–P18 已完成，之后几轮评审定下的规则见 D25–D32 与 D35。进行中：

- P6 执行环境、P7 扩展与 MCP、P8 `acp`：已实现，待真实集群、MCP 服务器与 ACP 客户端（Zed 或 Wisp）上的验收，见第 5 节。
- P9 可选增强：Anthropic Messages API 的 provider 已实现（`provider.api = "anthropic"`），只对模拟服务测试过，待真实 API 验收（付费，须用户授权）；⚑ D:5/CHINA FLAG: DeepSeek 经 OpenAI 兼容接口可用，不单独写。
- P15 IM 对话模型（D16）：Slack 实现了读线程与频道、查名字与回执；⚑ D:5/CHINA FLAG: 飞书读话题与群（2026-11-22），名字与回执沿用默认（用 open_id、不回执），待按需补上。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：读取该平台上话题与群的历史消息；补上名字后还会读取用户名称] Subject to Legal/Export Compliance approval。

- P19 常驻 agent 的核心能力（2026-11-22 用户决定，按顺序做）：① 主动发起对话（D37，已实现）；② MCP 客户端的常驻能力（D38，已实现）：断线与会话失效后照常调用、自定义请求头、按服务器信任只读标注；③ 按聊天选 agent 与权限（D39，已实现）：权限随 agent 文件，另可指定审批人；④ 按天的用量上限（D40，已实现）：整个服务与单个聊天；⑤ 名单热加载（D41，已实现）：SIGHUP 时重读 `[access]`；⑥ IM 发文件（D43，已实现）：长回答转文件与工具 `send_file`；⑦ 运行指标（D42，已实现）：每次运行一行 `run` 日志；⑧ 多副本（D44，已实现）：主备，Postgres 存储与 `[sessions] postgresUrlEnv`，选举、防脑裂与 Kubernetes 组件 `im-replicas`。观测、数据库等场景能力由外部实现（D8、D10）。

## 5. 待办与已知缺口

- 真实集群验收：常驻 Pod 部署包与多集群配置没有在真实集群上执行过；只读规则只看命令字面。
- 真实 MCP 服务器验收：对一个 Streamable HTTP 服务器跑通列出、调用与审批；HTTPS 未测。请求头、停机后恢复与会话失效后重新握手（D38）只对假服务器测过。
- zx-im 的会话存在目录时，每天的用量只在内存中计数（D40），重启后当天从零计；存在 Postgres 时记在库里，重启后接着计（D44）。
- zx-im 发文件（D43）只对假的平台测过：Slack 的三步上传与 ⚑ D:5/CHINA FLAG: 飞书的 multipart 上传按官方文档写成，没有在真实平台上跑过；⚑ D:5/CHINA FLAG: 飞书的图片作为文件发出，不走图片消息。
- ACP 客户端实测：只用手写客户端与 SDK 示例客户端验证过。顺带确认客户端是否发 `session/close`（不发时会话留到进程退出，见 [REFERENCE.md](REFERENCE.md#acp)），再考虑像 `zx-im` 一样空闲释放。会话自行关闭之后，`session/prompt` 以“会话未打开”应答，客户端要先 `session/load` 或 `session/resume`；要自动重开，得记下客户端认为打开着的会话编号，未做。
- 会话存在目录时单进程是部署边界：IM 的 backlog（D33）与状态记录只在本进程的会话目录里。存在 Postgres 时可以主备（D44），但同一时间只有一个进程服务，不做多活。对话索引在当选时由会话头部建成，之后只由本进程更新，随机器人参与过的对话增长，不淘汰。
- 主备（D44）只在本机与 CI 的 Postgres 16 上测过：接手、任期核对与连接断开都有测试，Kubernetes 组件 `im-replicas` 只用 kustomize 渲染核对过，没有在真实集群上部署；主所在网络断开时半分钟左右放锁，是按服务端 TCP keepalive 的设置推算的，没有实测。接手的副本看不到上一个主在工作目录里写的文件。
- ⚑ D:5/CHINA FLAG: 飞书群聊中 @ 任意机器人都算 @ 本机器人：事件不指出被 @ 的机器人是否属于本应用。读群的历史时已按本机器人的 open_id（经 `/open-apis/bot/v3/info` 取得）认出 @ 它的消息；事件里 `mentions` 的 `id.open_id` 能否同样比较，未核实。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块的消息路由（软件/代码）] Subject to Legal/Export Compliance approval。
- 输入被钩子改写时，原始输入与替换它的 `context_edit` 分两次追加，其间崩溃时原始输入会留在投影中。
- 会话写入只 `flush`、不 `fsync`，断电时已写入的记录可能没有落盘。
- 模型请求失败的文本照录 reqwest 的错误与服务端的回应：传输失败带请求的完整 URL，HTTP 错误带服务端原文，都不经脱敏写入会话与事件；`baseUrl` 不能带查询串，写在它的路径里的密钥仍会被记录。
- MCP 工具的参数 schema 没有大小上限，原样交给模型；工具名与描述有上限。
- JSONL 存储不缓存：`headers` 每次读取查询范围内每个会话文件的头部，`list` 每次回放匹配的会话；会话很多、ACP 客户端频繁列出时，再引入磁盘索引或数据库。
- 每一步多次重建并复制整个投影（构造请求、比较系统提示词、检查摘要阈值等各取一次）；加载会话时，每条 `compaction_started` 的校验也复制一次它所在分支的消息，校验其实只用到条目编号与消息种类（没有量过耗时）。会话很长时再优化。
- 每开一个会话，`Agent::assemble` 都重建所有会话共用的部分：复制钩子、策略、模型元数据与系统提示词，并在 `Toolbox::new` 里为全部工具重新编译参数的 JSON Schema。开销没有测过；IM 的会话多起来时，再把共用的部分只建一次，会话只加自己的工具（2026-11-19 的评审提出）。
- 两个 HTTP provider 发请求与读事件流的流程仍大体相同，`SystemUpdates` 各定义一份；再下沉到 `zx-ai-http` 涉及 D24 的分层，单独设计（2026-11-19 定为不在 e10a914 评审的修复中做）。
- ACP 在运行中不显示扩展要求显示的消息，回放时显示：zx-agent 里没有部件会在运行中发出这种消息，所以没有端到端测试可写；`acp/updates.rs` 的单元测试要能构造 ACP 连接。
- ACP 不提供 chat 有的三样：扩展注册的斜杠命令（`Session::commands`、`run_command` 只有 chat 调用）、工具的进度更新（`tool_execution_update`），以及扩展记下的状态与恢复时补写的调用结果（`run` 与 chat 各打一行）。扩展点在唯一的界面协议上只通了一部分，做 P7、P8 的验收时一起决定。
- ACP 的通知队列无界：SDK（agent-client-protocol 5.0.0）的 `send_notification` 不等待，放进无界通道；客户端不读 stdout 时，流式更新积在内存里，而内核的事件通道是有界的。要有界得等 SDK 提供可以等待的发送。
- 待评估：保留近期用户消息原文、摘要前把较早的长工具结果换成引用（可推迟碰到 2 MiB 上限）；失败运行后排队输入的处置（pi 的 `afterFailedRun`，现由宿主用 `take_queued` 处置）；工具触发的 `handoff`（pi 的 `control.handoff`，要先定会话格式）；多 Agent 协作（2026-11-22 由第 7 节的“不做”改为待评估：单次运行的上下文或时长成为排查的瓶颈时再评估，在此之前由一个 agent 依次调用工具）。
- 暂缓：审计中写明自动放行命中的前缀（要改 `PolicyDecision` 的 JSON 契约）；FauxProvider 响应增量之间的取消；provider 取消后的宽限期与 `stop_reason` 一致性检查；`JsonlFile::create` 失败后留下文件。
- 待用户决定：`run` 的内置 agent（及部署的 `unattended.md`）是否默认开启自动摘要；现在关闭，撞到上下文上限时以退出码 1 结束，开启的代价是摘要丢失信息。
- 待用户决定：`run -` 从 stdin 读提问不设上限（chat 每行至多 1 MiB）；调度器误把大文件接到 stdin 时，会整个读进内存、记入会话再发给模型。上限取多少待定。
- 待用户决定：⚑ D:5/CHINA FLAG: 飞书在私聊里回复不开话题，靠进程内记下的私聊集合判断；集合只在收到消息时写入，重启后从 backlog 重放的私聊消息用不上它，超长回答的第二段起会按话题回复。根因是 `Destination` 不带聊天种类，要改的是平台接口（让目的地带上 `ChatKind`，或服务对私聊不构造线程目的地），属设计调整。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块的消息发送（软件/代码）] Subject to Legal/Export Compliance approval。
- IM 平台的连接刚建立就被断开时立即重连，退避每次重置，反复如此时重连的节奏只靠平台的限频；断开现在会记一条日志，是否计入退避未定。
- 对照 pi durable 的较大项（待评审）：排队消息落盘（IM 服务丢消息已由 D33 的 backlog 解决）、部分结果落盘、后台摘要（宿主已可用 `Summarizer` 自己产出摘要，钩子仍不提供摘要）、按会话的扩展（按会话的工具、钩子、提示词段落与工作目录已由 `SessionSetup` 给出）、扩展的可读状态、延迟加载的工具与 `tool_search`、持久化的睡眠与定时、宿主侧的环境接口、附带的数据库存储（接口 `SessionStorage` 已有，实现由宿主给出）。
- 对照 pi 的 Discord 接入（待评估）：消息附件的进出、停机期间漏掉的 @ 的补读（按对话的游标读平台历史；backlog 只管已送达的消息）、`/status`。Slack 线程里其他机器人的消息已能听到（见 [REFERENCE.md](REFERENCE.md#im-服务)），其形态依据 Slack 文档，只读 `text` 与附件的文字、不读 `blocks`，没有在真实工作区验证。
- 读频道与话题的历史（D16，2026-11-22）只对模拟服务测试过：Slack 的 `conversations.history`，⚑ D:5/CHINA FLAG: 飞书的取消息、列消息与机器人信息接口。飞书待真实租户核实四点：历史消息里 @ 机器人的 `mentions[].id` 是否就是 `/bot/v3/info` 给的 `open_id`（不是时认不出上次 @，读满 50 条）；富文本在列消息接口里的内容结构是否与事件相同；话题的根消息是否在 `thread` 容器的结果里（现在另取根消息、按 ID 去重）；`end_time` 是否包含边界那一秒（现在多取一秒，再按毫秒去掉 @ 之后的）。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：读取群与话题的历史消息（数据）] Subject to Legal/Export Compliance approval。
- 应用层 `zx-app` 尚无真实 Web 宿主验收。
- 图片（D36）：经 `openai` provider 在 ⚑ D:5/CHINA FLAG: DeepSeek 上验证过（2026-11-20，第 6 节），Anthropic provider 收发图片没有在真实 API 上验证；尺寸与大小都在上限内的 GIF 原样发送：⚑ D:5/CHINA FLAG: DeepSeek 接受，动图答的是第一帧（2026-11-20），其他服务没有验证；每张图片按约 1200 token 估算上下文，不按尺寸。会话里的图片每次请求都重发：图片多时请求体可能超出服务的上限（Anthropic 以 413 `request_too_large` 拒绝），provider 把它归为上下文过长，开启自动摘要时摘要一次再试（摘要不保留图片），没开时运行失败；没有按请求只发最近几张的做法。
- IM 的图片（D36）：读入的线程与频道历史不带图片；图片不计入 `maxPendingBytes`，只受每条至多 4 张、每个文件至多 20 MiB 的限制；⚑ D:5/CHINA FLAG: 飞书只开 `im:message` 时下载消息里的图片、富文本消息里 @ 机器人并附图片，都在真实租户验证过（2026-11-21，第 6 节）；文档列出的另两种权限（`im:message:readonly`、`im:message.history:readonly`）没有试。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书平台模块下载用户发送的图片（软件/数据）] Subject to Legal/Export Compliance approval。
- 音频与视频不收：MCP 结果与 ACP 提问里的音频照旧略去。需要时再定：加一种输入只发给声明能收的模型，或由宿主先转成文字；视频交给工具抽帧。
- 执行环境扩展前，需让执行器提供文件资源身份：共享宿主挂载的同一文件共用键，容器/远程私有文件带环境域，并在目标文件系统解析别名；当前锁按宿主路径协调。
- 集成测试还没有全放进被测的 crate（规则 06）：`crates/zx-cli/tests/` 的 `container.rs`（44 个测试中 33 个）与 `crash_recovery.rs`（14 个中 12 个）有只用库的测试，与运行程序的测试同在一个文件；`container.rs` 的两种测试共用假 docker，CI 用 `--test container` 跑真实 Docker，拆分要先把假 docker 移进 `zx-testing`。
- 容器里的文件写入在调用被丢弃后仍写完，只用假 docker 验证过。
- 落盘目录的符号链接检查与随后的打开之间有竞态：同时在跑的命令可以在两者之间换掉目录。要消除得从工作目录起按目录描述符逐级打开，未做。
- 没有确定性测试的行为：chat 在会话自行关闭后重开（子进程中途写入失败无法确定性地构造，只测了从错误链认出会话已关闭）；尾记录修复在写线程上进行时，卷无响应不阻塞运行时（只测了修复本身）；MCP 放弃通知的 10 秒超时；`acp` 的建会话、载入、续接与列出在派发循环之外运行，慢的会话准备不挡其他请求。

## 6. 验证记录

只记录付费或真实环境上的运行；常规测试每个 PR 都跑，CI 覆盖 Linux 与 macOS，本地使用 macOS。

⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: DeepSeek：下表中写明经 DeepSeek 的各行把请求发往该服务的 API] Subject to Legal/Export Compliance approval

| 日期 | 运行 | 结果 |
|---|---|---|
| 2026-11-01 | `crates/zx-agent/tests/real_api.rs`，⚑ D:5/CHINA FLAG: DeepSeek `https://api.deepseek.com/v1`，⚑ D:5/CHINA FLAG: `deepseek-flash` 与 `deepseek-chat` 各一次 | 均为 6 项通过；只覆盖 `collapse` 模式，不含压缩用例 |
| 2026-11-02 | `run` 经 ⚑ D:5/CHINA FLAG: `https://api.deepseek.com` 调用 `deepseek-flash`，附加请求字段取默认、关闭思考、`reasoning_effort="low"` | 均以 0 退出；关闭思考时输入 token 不同，字段生效 |
| 2026-11-02 | 摘要评测，两个模型各三次 | ⚑ D:5/CHINA FLAG: `deepseek-chat` 三次通过；⚑ D:5/CHINA FLAG: `deepseek-flash` 因推理用满默认 2048 token 的输出预算而失败，调高预算后尚未运行 |
| — | `provider.systemUpdates = "inline"` | 只在一个服务上验证过 |
| — | `provider.api = "anthropic"` | 只对模拟服务（`crates/zx-agent/tests/anthropic_mock.rs`）测试过，没有调用真实 API |
| 2026-11-19 | 经 Anthropic provider 向 `claude-haiku-5-5` 验证 `thinking.block_binding`（计划三次请求：产生思考，再以 `"error"` 与 `"drop_block"` 各重放一次改过开头的对话） | 未能进行：第一个请求返回 400 “credit balance is too low”，没有产生用量，其余两次没有发出；这项配置仍只经单元测试与加载检查 |
| 2026-11-20 | 经 `openai` provider 向 ⚑ D:5/CHINA FLAG: DeepSeek `https://api.deepseek.com` 的 ⚑ D:5/CHINA FLAG: `deepseek-flash`（关闭思考，`maxTokens` 256）：`run` 的纯文本、`read_file` 往返与 `run @图片`，ACP 带图片的一轮；再在 `[[models]]` 声明 `input = ["text", "image"]`，跑 `run @图片` 与 `read_file` 读图 | 均通过。未声明看图时模型收到 `(image omitted: model does not support images)`，会话存原图；ACP 声明 `image: true`；声明后两种图片都答对颜色。8 个请求约 7700 token；事件、会话与 stderr 都不含 API key |
| 2026-11-20 | 经 Anthropic provider 向 `claude-haiku-5-5`（纯文本、图片、`thinking.block_binding`） | 未能进行：第一个请求同样返回 400 “credit balance is too low”，没有用量 |
| 2026-11-20 | 直连 ⚑ D:5/CHINA FLAG: DeepSeek `https://api.deepseek.com` 的 ⚑ D:5/CHINA FLAG: `deepseek-flash`（`openai` provider，关闭思考，清空环境变量运行、只传 key）：`run` 的 bash、策略拒绝加 `--fail-on-denied`、`--token-budget`、`--cost-budget`、输出截断与续写、不存在的模型、`--timeout-secs` 与 Ctrl+C 之后的 `--continue`、一个回答里的两个 `read_file`、超出上限的图片、GIF；开启思考的工具往返；`chat` 的 `/compact`、`--resume` 与 `AGENTS.md`；`acp` 的审批（允许一次、拒绝）与取消；另有 10 个不发请求的场景（配置与参数错误以 2 退出，`sessions` 与 chat 的本地命令以 0 退出） | 均符合预期。退出码依次为 0、9、7、8、6、1、4、5，两次 `--continue` 都续上；bash 读不到 key 所在的变量；两个调用的结果按调用顺序写入；2600×2000、7.9 MB 的 PNG 缩成 2000×1538 的 JPEG 后答对颜色；GIF 被接受，先绿后蓝的动图答“绿”；带推理内容的工具调用回传后，下一次请求成功；ACP 允许后执行，拒绝后调用为 failed，取消后 `stopReason` 为 `cancelled`；`/compact` 的摘要比原对话还长时保留原对话。发现三处显示问题，另行修正：预算报错的花费按四位小数显示（0.000062 显示为 0.0001）、`/compact` 失败时同一行打印两次、摘要请求不计入 `/status`。约 41 个请求、4.3 万 token；输出、会话与 stderr 都不含 API key |
| 2026-11-21 | 直连 ⚑ D:5/CHINA FLAG: DeepSeek 的 ⚑ D:5/CHINA FLAG: `deepseek-flash`（清空环境变量运行、只传 key）：同一道需要推理的题，直接请求时不带 `thinking` 字段、显式开启、显式关闭各 3 次；本地服务记下 zx-agent 不设 `reasoning` 时的请求体；zx-agent 不带这个字段跑 2 次；经本地的 CONNECT 代理（只转发 TLS，不解密）把第一条连接切断两次：一次在收到约 15 KB 时（流的前 18%），一次在 TLS 握手中 | 默认开启思考：不带字段的 3 次都返回推理内容（69–99 个推理 token），与显式开启相同，显式关闭的 3 次都没有；zx-agent 不设 `reasoning` 时请求体不带思考字段，两次运行都记下了思考块。两次切断都在约 2 秒后重试一次并完成（`retries=1`，退出 0）：流中途断开时，会话先记下失败的回答，再记 `context_edit`，用户消息没有重发；握手中断开时，第一个请求没有到达服务。15 个请求约 2 千 token；输出与会话都不含 API key |
| 2026-11-21 | `zx-im` 接真实的 Slack 工作区（Socket Mode），模型为直连的 ⚑ D:5/CHINA FLAG: `deepseek-flash`：频道里 @ 机器人、线程里不 @ 接话、自动放行的只读命令、未授权与已授权的私聊、任务进行中 SIGTERM、停机期间发的私聊、`/cancel`（空闲与运行中）、`/compact`、`/new`、空闲释放后再发消息、审批卡片的放行与随任务超时的取消、@ 时附图片 | 均符合文档：停机期间的消息在重启约 1 分钟后由 Slack 重投并得到回答；`app_mention` 事件带 `files`，bot 缺 `files:read` 时下载图片得到 403，模型收到说明；放行后 bash 读不到两个 Slack token 与 key 所在的变量，会话与日志里没有它们的值。发现两处显示问题与一处日志时间，另行修正：模型答完后才读到插话时，前一个回答不显示；回答排在审批卡片之上；`approval` 日志的时间取在卡片改完之后 |
| 2026-11-21 | `zx-im` 接真实的 ⚑ D:5/CHINA FLAG: 飞书租户（企业自建应用，长连接），模型为直连的 ⚑ D:5/CHINA FLAG: `deepseek-flash`：群里 @ 机器人、话题里不 @ 的回复、话题里 @ 接着问（中间重启一次）、富文本消息里 @ 机器人并附图片、审批卡片的放行、未授权的私聊。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书租户中的验收经该平台收发消息与图片] Subject to Legal/Export Compliance approval。 | 均符合文档：没开“获取群组中所有消息”时话题里不 @ 的回复没有送来；重启后按话题接回原会话；只开 `im:message` 就能下载图片；`approval` 日志的 `endedMs` 等于工具开始执行的时刻；私聊送到后回复了“未授权”。设置时发现：保存“使用长连接接收事件”时要有长连接在线；只选了订阅方式、没添加事件时消息不送来；私聊要 `im:message.p2p_msg:readonly`，原文档没写，已补。拒绝未授权的消息原先不写日志，名单要的 ID 只能从会话记录取，另做改动：拒绝时记一条带发送人的 `unauthorized` 日志。6 个请求约 1.3 万 token |
| — | 真实 Docker（`ZX_DOCKER_TEST_IMAGE`，`crates/zx-cli/tests/container.rs`） | macOS 的 Docker Desktop 上用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像跑过；原生 Linux 只由 CI 覆盖；没有在 rootless Docker 或 Podman 上验证 |
| 2026-11-01 | `deploy/kubernetes/` 在本机 Docker Desktop 的 Kubernetes v1.36.1（2 个 arm64 节点，local-path ext4），假模型 | 镜像、只读账号、run、chat 审批、`operator.yaml` 的每项写操作、定时巡检、脱敏、会话文件锁与清理均符合预期；多集群由同一集群扮演远端，新增集群约 1 分钟后生效。读 agent 自身 `/proc/<pid>/environ` 被拒一项在 agent 不再设为不可 dump 后不成立，未重测 |
| — | IM 组件（当时名为 `serve`，此后改为子命令 `im`、再拆为程序 `zx-im`，都未重测），Slack 指向不可达地址 | 启动、`/proc/1/environ` 中没有 API key、重试日志不含凭据、SIGTERM 后退出码 0、孤儿进程被回收、与 `multicluster` 组件共存 |
| — | 定时巡检的会话卷与 `--fail-on-denied` 的说明 | 只经 `kubectl kustomize` 本地渲染检查，没有在集群上运行 |
| — | `deploy/kubernetes/` 的 `zx-agent-cluster-view`、按命名空间授予的 `operator.yaml`（此前验证的是全集群的 ClusterRoleBinding）、巡检前清理 30 天前的会话文件、`/tmp` 的 1Gi 上限、`part-of` 标签 | 只经 `kubectl kustomize` 本地渲染检查，清理命令只在 macOS 的 sh 与 find 上试过；没有在集群上运行 |

未验证：x86_64 镜像与其他集群、推送到镜像仓库、真实模型下的部署、真正分属两个集群的多集群访问、kubeconfig 的 exec 插件凭据、真实的 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户。

## 7. 已决定不做

- `--output-schema`、OS 级沙箱
- 异步审批（2026-11-22 复核后保留）：审批在运行内等待，计入运行时限；等待跨越运行要把运行的状态存下来，与自动续跑同类
- rpc 模式（D14）；chat 的便利功能（D13）
- durable 的自动续跑与重放（2026-11-22 复核后保留：重启时 zx-im 已把中断的运行标明，没处理的消息由 backlog 保住，再问一次即在原会话里接着做），工具结果中的 `structuredContent` 与 `runToolCall`（见 REFERENCE“与 pi 的差异”），codemode
- MCP 的 stdio 与 OAuth（D8，2026-11-22 复核后保留）：外部服务器由宿主实现，可以提供 Streamable HTTP；常驻部署里管理 stdio 子进程、走 OAuth 的浏览器授权都不合适。工具列表也不重新列出，运行中途与两次运行之间都不换（D38）：服务器换了工具后重启宿主
- 完整 TUI、插件热加载、WASM 插件、云端托管运行时、A/B 行为评测框架
- 事件 JSON 中预算的名字 `model_request_limit` 不改为与设置同名的 `max_model_requests`：事件是对外格式（`schemaVersion`），改名的收益只是名字对齐

---

ROADMAP.md · v0.74 · 2026年11月
