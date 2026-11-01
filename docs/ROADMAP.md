# zx-agent 路线图

本文档记录 zx-agent 的设计目标、已定决策与分阶段计划，随实施进度更新。

- 现状与用法见 [README](../README.md)。
- 调研、选型论证和接口草图见[通用内核技术方案](zx-agent-general-kernel-proposal.html)。

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

G1–G4 是验收目标。当前会话日志可重建对话与提示词；G3 所要求的完整请求重建（风险见技术方案第 10.2 节）已在 P5 补齐：每次请求前记录 `request_started`，工具定义与发送配置变化时记录 `request_config`。

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
| D2 | 新依赖 | 已授权：tokio 的 `io-std` feature、`toml`、`rmcp`（MCP 官方 Rust SDK）、ACP 官方 Rust SDK、`rustyline`（可选）。在用到它们的阶段再引入 | 2026-10-31 | 按阶段引入 |
| D3 | profile 文件格式 | TOML | 2026-10-31 | P4 落实 |
| D4 | 会话 API 形态 | 进程内方法调用；暂不实现提交队列 / 事件队列（SQ/EQ） | 2026-10-31 | 已实现：P2 进程内共享会话 API |
| D5 | 无人值守的命令执行环境 | 默认仍在本机执行，`run` 行为不变；容器执行须在 profile 或 CLI 中显式开启。理由：在 Kubernetes Job 中运行时 Pod 内通常没有 Docker；无网络的容器做不了在线排障，而在线访问方案尚未评审。此前的候选方案（`troubleshoot` 默认容器、没有 Docker 时拒绝启动）不采用 | 2026-11-01 | 已确认：P6 执行环境部分按此实现 |
| D6 | 项目定位 | 改为“通用内核 + 场景配置” | 2026-10-31 | 已完成：README、`.claude/CLAUDE.md` 已更新 |
| D7 | IM 长连接的 WebSocket 客户端 | 已授权：`tokio-tungstenite`（0.30，关掉默认 feature，只开 `connect` 与 `rustls-tls-webpki-roots`）。理由与依赖核对见 `docs/serve-im-review.md` 缺口 ① | 2026-11-01 | 已引入（PR #29） |
| D8 | MCP 的范围 | 本项目只做 MCP 客户端：只接工具，只支持 Streamable HTTP。MCP 服务器与领域工具（如 Kubernetes、Prometheus 查询）由外部服务实现 | 2026-11-01 | 已确认：P7 按此评审，见 `docs/p7-mcp-review.md` |
| D9 | HTTP 客户端与 TLS | reqwest 升到 0.13（`json`、`stream`、`rustls`）。rustls 只用 aws-lc-rs 一个加密后端，HTTPS 的根证书改用系统信任库。理由：rmcp 只支持 reqwest 0.13，与 0.12 并存时 rustls 同时启用两个后端，`wss://` 连接会 panic（评审稿缺口 ①）；项目还没有使用者，用户授权按最合理的方式处理 | 2026-11-01 | 已完成（PR #36） |

D2 之外的新依赖、依赖升级，仍需先征得同意。

## 4. 阶段计划

2026-10-31 已完成一轮内核健壮性优化（本轮改动）：

| 项目 | 已实现 |
|---|---|
| 单一运行与取消 | `Runner::run(&mut self)`；丢弃运行 future 时取消子令牌，定时任务、输出读取任务和 Unix 进程组由 RAII 清理 |
| 文件读取 | 只接受普通文件，实际读取量受限，等待响应取消；大分页参数不会溢出 |
| 会话恢复 | 支持 UTF-8 字符中途截断；只修复未完成尾记录，完整损坏记录报错并保留；提供修复字节数 |
| provider 错误 | 可选 `ProviderError`；状态与错误码分类优先，支持数值 `Retry-After`，兼容旧错误文本与旧会话 |
| 流式事件 | 工具参数增量携带调用 ID；事件队列有界，响应与单个 SSE 事件有字节预算 |
| 验证 | 回归覆盖上述边界，原有 CLI 退出码与正常任务行为继续验证；新增资源限制有单独 CLI 用例 |

2026-11-01 复核 P0–P3 后的修正（已完成，PR #7）：

| 项目 | 修正 |
|---|---|
| `read_file` 长行 | 单行超过单页字节上限时只显示前缀，并注明该行被截断、已显示和总字节数，以及 read_file 不能在行内翻页。此前不作提示，或把被截断的行计为已显示 |
| chat 输入 | 超过 1 MiB 或不是 UTF-8 的行被整行丢弃并提示，对话和当前任务继续。此前整个 chat 以退出码 2 结束 |
| 会话写入 | 一次写入失败后不再写入，未写完的记录只会留在文件末尾，下次打开时按尾记录修复。此前后续条目可能接在半行之后，使文件无法再打开 |
| 密钥 | `OpenAiConfig` 的 Debug 输出不含 API key |
| 用量 | 服务端返回的用量按饱和加法累计，异常大的值不再导致溢出 |
| 测试 | 每项修正先补回归测试，并确认修复前失败；补充审批默认 120 秒的断言；改正一项名不副实的 chat 测试名 |

本地 macOS 验证（基于 `07a0b8d`，含 P4）：stable 与 Rust 1.88 各 314 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。本次修正未改动 `src/cli.rs`、`tests/cli.rs`、`Cargo.toml`、`Cargo.lock`；`run --help`、`chat --help` 与 `07a0b8d` 逐字节相同；未新增依赖。

2026-11-01 复核 P4 后的修正（已完成，PR #8）：

| 项目 | 修正 |
|---|---|
| 自动发现的上下文 | 自动发现的 AGENTS.md 解析后须位于 Git 根目录内（无 Git 根时为 cwd），否则报错，不发出请求。此前仓库中的符号链接可让程序在启动时读取仓库外的任意文件，并写进发给模型的系统提示词 |
| `run` 默认上下文 | `troubleshoot` 不再自动发现 AGENTS.md，恢复 P4 前 `run` 的行为；需要时用 `--context-file` 显式指定，或在 profile 中设置 `discover = true`。`chat` 使用的 `assistant` 不变 |
| `--system-prompt-file` | 恢复 P4 前的读取方式：可以是管道，不受 64 KiB 限制。64 KiB 上限只用于 profile 引用的文件 |
| 上下文报错 | 加载失败时提示 `--no-context` |
| 工具名 | 工具名集中为常量（第 5 节整理项） |

本地 macOS 验证（基于 `993350e`）：新增 9 项测试，其中 8 项回归测试在修复前失败；3 项既有测试显式开启 `discover = true`，断言不变。stable 与 Rust 1.88 各 323 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`run --help`、`chat --help` 与 `993350e` 逐字节相同；`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

2026-11-01 复核 P5 后的修正（已完成，PR #11）：

| 项目 | 修正 |
|---|---|
| 结果目录越界 | 默认结果目录（相对工作目录）解析后须位于工作目录内；仓库中的符号链接不能把写入引到外部，越界时不写入。profile 显式配置的目录不受此限制。此前仓库可提交 `.zx-agent` 链接，使结果写到任意可写目录 |
| `run` 默认行为 | `troubleshoot` 默认不启用 `read_output`：工具集与结果内联恢复为 P5 前的行为，也不向工作目录写文件。`chat` 使用的 `assistant` 不变 |
| 写入失败 | 写入失败、超过单文件上限或目录越界时，保留已脱敏的有界结果内联并附原因，并删除写了一半的文件。此前返回 `Tool output withheld`，只读工作目录中所有大于阈值的结果都会丢失 |
| 仓库卫生 | 结果目录中写入内容为 `*` 的 `.gitignore`（已有则不覆盖），避免结果被提交到用户仓库。此前只有本仓库自己的 `.gitignore` 忽略该目录 |
| 自动摘要失败 | 付费的自动摘要请求失败后，本会话暂停自动摘要：之后需要摘要时直接报错并说明原因，不再发请求；手动压缩成功或切换模型后恢复。此前每次提问都会重新付费并以同样的方式失败 |
| 流程 | 新增 PR 模板，提醒在 PR 中回写阶段状态 |

既有测试的调整：两项结果落盘测试按新的写入失败契约改写（失败时保留内联结果，而不是 `Tool output withheld`），修改后在修复前失败；三项依赖 `troubleshoot` 默认启用 `read_output` 的测试改为显式启用，其中一项只统计 UUID 结果文件、不计入新增的 `.gitignore`；验证意图不变。

本地 macOS 验证（基于 `25e5b49`）：新增 6 项测试，其中 5 项回归测试在修复前失败，另 1 项验证手动压缩成功后恢复自动摘要。stable 与 Rust 1.88 各 391 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。另用本地假服务实测：默认 `run` 的工具集与结果内联同 P5 前一致；显式启用 `read_output` 时，只读目录与越界链接都保留内联结果、不写外部目录；结果目录中的 `.gitignore` 经 `git status` 验证生效。

2026-11-01 全局评审后的修正（已完成，PR #16）：

| 项目 | 修正 |
|---|---|
| 压缩保留边界 | 保留边界按条目在分支上的先后计算。此前保留范围的第一条消息被 `context_edit` 省略后，摘要从投影中消失，已摘要的旧记录回到模型上下文 |
| 合成摘要的编辑 | 合成摘要标为 `UserMessageKind::Summary`；编辑、省略或追加它的变换计划在校验时整份拒绝，按钩子失败结束，不写入任何条目。此前计划校验放行，写到这一项才失败：前面的编辑已写入，并按会话写入失败取消运行 |
| 未知 `finish_reason` | 只有 `stop`、`end`、`length`、`tool_calls`、`function_call` 视为正常结束，与 pi 的 `openai-completions.ts`（`a92283056`）相同；其他取值为 provider 错误，错误码为该取值，不重试。以 `stop` 或 `end` 结束但带工具调用时仍按工具调用处理，沿用此前对部分服务端的兼容（pi 没有这一处理）。此前未知取值一律当作正常结束，服务端中断的回答会以退出码 0 报告 |
| bash `timeout_secs: 0` | 在策略检查与审批之前拒绝。此前命令会立即超时 |
| provider 参数错误 | 先构造 provider，再创建会话文件；`--max-response-bytes 0` 等错误不再留下只有头部的会话文件。容器仍在会话文件之后启动，以便把文件隐藏在容器外 |
| 预算参数的默认值 | `--max-turns`、`--timeout-secs`、`--continue-on-length` 省略时取所选 profile 的值，help 不再显示固定默认值；删除 `Explicit` 与解析器默认值。此前 `run --help` 显示 30 / 900 / 1，而 `run --profile assistant` 实际不限轮次和时间 |
| `/new` 与 `--system-prompt-file` | 说明文件只在启动时读取一次，`/new` 沿用。此前 `/new` 重新读取：管道传入时新对话的说明被静默清空，文件被删除时 `/new` 失败 |
| `run` 进度输出 | 工具名、参数、错误文本与报告行经与 chat 相同的转义后输出。此前模型或服务端返回的控制字符原样写到 stderr |
| 文档 | README 的系统提示词组成（说明加四个生成段落，空段不出现；容器执行时环境信息的来源）、"与 pi 的差异"中过期的"上下文压缩未实现"、预算参数与重试说明；PROFILES.md 的会话格式版本；`.claude/CLAUDE.md` 的提示词一行 |

兼容性：`run --help` 与 `chat --help` 中上述预算参数和 `--system-prompt-file` 的说明有意改变；此前各阶段以 `run --help` 逐字节不变作为验收项，本次不再适用于这几项。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

既有测试的调整：`src/cli.rs` 中三项断言解析器默认值与 `Explicit` 的单元测试，改为两项断言"省略即取 profile、显式的 0 或空值仍覆盖"。

本地 macOS 验证（基于 `efe998e`）：新增 10 项测试。其中 8 项回归测试在修复前失败，分别对应压缩保留边界、编辑摘要的计划、未知 `finish_reason`、bash 零超时、provider 参数错误、预算参数的 help、`/new` 与说明文件、`run` 输出转义；另 2 项单元测试覆盖合成摘要的三种编辑和 `finish_reason` 映射。stable 与 Rust 1.88 各 455 项通过（基线 446 项）、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。与基线相比，`run --help`、`chat --help` 只有上述几项说明变化。

2026-11-01 Codex 评审后的修正（已完成，PR #18）：

| 项目 | 修正 |
|---|---|
| 容器内的进程清理 | 保持容器存活的 PID 1 与 `cat` 改以去掉全部 capability 的 root 运行；命令、文件读取与清理都经 `docker exec --user <命令用户>` 执行。每条命令执行前后，以命令用户的身份执行 `kill -s KILL -- -1`，终止该用户的全部进程；同一容器内的命令与文件读取逐条执行，运行 future 被丢弃时，清理任务排队等到这把锁再执行。此前按 tmpfs 中的 PID 文件终止进程组：用 `setsid` 脱离进程组的子进程不受影响；命令删除 `/tmp/.zx-agent` 后不但清理失效，之后的命令也都以 125 失败；并行的命令会在同一容器中重叠 |
| 本机执行的边界 | 接口文档与 README 写明本机只终止进程组，脱离进程组的进程不保证终止，需要保证时使用容器执行。实测 `setsid` 子进程在超时后继续运行；不借助 cgroup 等系统机制无法可靠追踪 |
| 容器用户不得为 root | 配置校验拒绝 UID 0 与 `root`，启动探测按 `/proc/self/status` 检查命令的实际 UID。此前 `user = "0:0"` 时命令以 root 运行，与"非 root"的隔离说明不符 |
| 会话文件与容器 | 去掉 `/dev/null` 遮蔽（删除 `ContainerConfig::hidden_files`）；容器执行时，会话文件位于工作目录内即报配置错误，不创建文件、不启动容器。此前遮蔽挡不住硬链接（实测读出内容），同目录中较早的会话文件也完整可读 |
| 运行中被取消的工具调用 | 区分开始前取消（"未执行"）与执行中被打断：有副作用的调用写明可能已改变状态、先检查再重试，只读调用写明未改变任何状态。此前统一为 `Operation aborted`，下一轮的模型无从判断调用是否已生效 |
| 插话与后续消息 | 循环在处理点逐条从队列取出。前一条被钩子拒绝或运行被丢弃时，其余消息仍留在队列中，供下一次运行使用。此前整批取出到局部变量，随之丢失 |
| 自动摘要的暂停状态 | 状态归 Runner 所有，运行只借用。此前运行取走状态、只在正常结束时放回，丢弃运行 future 后暂停失效，下一次提问会重新发送付费的摘要请求 |
| `run` 的最终回答 | stdout 是终端时，经与 chat 相同的转义输出；输出到管道时保持原文 |
| 技术方案 | 更新 execution 与 D5 的现状说明（v0.11） |

兼容性：公开的 `ContainerConfig::hidden_files` 已删除；容器执行时会话文件须位于工作目录外；容器内的命令改为逐条执行；`docker run` 的用户改为 root（仅 PID 1 与 `cat`）。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

既有测试的调整：`tests/container.rs` 中以假 docker 验证清理的 4 项测试改为断言按用户清理（命令前后各一次）与 `--user` 参数；"取消的命令"改为在命令运行中取消，原先"开始前取消"的情形另立一项，断言不执行任何命令；验证遮蔽会话文件的真实 Docker 测试随功能删除；3 项 CLI 测试把会话文件移到工作目录外。原 chat 测试中的伪终端辅助函数移入 `tests/common`，供 run 的终端输出测试共用。

本地 macOS 验证（基于 `ee15a29`）：新增 14 项测试，删除 1 项。其中 7 项回归测试在修复前实测失败：摘要暂停在丢弃运行后保留、被拒绝的插话之后的消息仍在队列、执行中被取消的有副作用调用的结果、终端中的最终回答，以及真实 Docker 中的 `setsid` 子进程清理、命令无法关掉清理、命令逐条执行。另外 7 项覆盖只读调用被打断、管道输出保持原文、root 用户的配置校验与启动探测、工作目录内的会话文件被拒绝、开始前取消不执行命令。修复前的另两处现象用 Docker 直接复现过（同一套启动参数）：遮蔽的会话文件可经硬链接读出，同目录较早的会话文件可读；`--user 0:0` 时 `id -u` 为 0。stable 与 Rust 1.88 各 468 项通过（基线 455 项）、6 项付费测试跳过；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过（Docker Desktop 29.9.2）；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。

2026-11-01 整体设计评审后的修正（进行中）：

背景：2026-11-01 按模块分五个区对整个代码库做了一次设计评审，共 46 条，逐条核实后按主题分四个 PR 修正：安全、循环与 chat、会话、输出与诊断。

一、安全（已完成，PR #30）：

| 项目 | 修正 |
|---|---|
| 自动放行的字面检查 | 命令只由字母、数字、空白和 `- _ . / : = , @ + \| ^ ~ !` 组成时才可能自动放行；出现引号、`\`、`$`、通配符、花括号、括号等其他字符一律询问。此前"不含 `secret`、URL、连接参数"只检查原始字面，`kubectl get sec""rets`、`"--server=…"`、`"https:"//…` 经 shell 去掉引号后照常生效，不经审批即可执行 |
| 自动放行的 kubectl 参数 | 自动放行的 kubectl 命令只能带白名单中的只读参数：命名空间与上下文、标签与字段选择、输出格式与排序、日志范围等；`--raw`、`--user`、`--cluster`、`--profile`、`--profile-output`、`--cache-dir`、`-v`、`-f`、`-w` 等一律询问。此前只排除 4 个连接参数：全局参数 `--profile-output` 能覆盖任意文件（kubectl v1.36.1 实测），`--raw` 的路径可用 URL 编码绕过 `secret` 检查，`--user` 可把一个集群的 token 用在另一个集群的地址上 |
| 默认危险命令 | 关机、重启与 `mkfs` 等程序名只在命令位置匹配：管道与命令列表的每一段、`sudo` 等包装程序之后；比较前去掉引号与反斜杠，按路径的文件名比较。`grep`、`last`、`journalctl` 等只读程序的参数不参与匹配。`rm -rf /` 等多词模式仍按子串匹配。此前按子串匹配整条命令：`kubectl get pods -n asphalt-prod`、`last reboot`、`journalctl … \| grep -i reboot` 被拒绝，`/sbin/re''boot` 却能通过 |
| `read_file` 的特殊文件 | 先查看路径类型，只打开普通文件；打开后再检查一次，防止中途被替换。此前先打开再判断，FIFO 与设备文件在被拒绝之前已经被打开 |
| `--secret-file` 与 `ZX_SECRET_FILES` | 两者合并登记。此前命令行给出 `--secret-file` 时整体忽略环境变量，部署包用环境变量登记的远端 token 会因此不再脱敏 |
| `RuleMatcher::CommandPrefix` | 与命令白名单一样忽略大小写、合并空白；Allow 规则不匹配含 `;`、`&`、`>`、`<`、反引号、`$(` 或换行的命令。此前 `KUBECTL delete` 能绕过 Deny 前缀，Allow 前缀 `kubectl get` 会放行 `kubectl get pods; rm …` |

暂不做：在审计中写明自动放行命中的清单前缀，需要改变 PR #25 固定的 `Decision` JSON 契约，另行评估。

实施记录（PR #30，基于 `0984288`，含 PR #28）：

- 按上表实施，补充如下：
  - kubectl 参数白名单按 kubectl v1.36.1 的帮助核对（`get`、`describe`、`logs`、`top`、`events`、`explain`、`api-resources`、`version`、`config get-contexts` 与全局参数）。`--watch`、`--follow` 这类会一直等待的参数也不在其中。
  - 旧规则把 kubectl 短参数中含字母 `s` 的一律当作 `-s`，`-nkube-system` 因此要审批；现在按参数白名单判断，可以自动放行。
  - `DEFAULT_DENIED_PATTERNS` 只保留多词模式，程序名移到新的 `DENIED_PROGRAMS`。`--deny-cmd` 与 profile 的 `rules.deny` 仍按子串匹配。
  - `read_file` 的错误信息不变。
- 公开 API 与 CLI 的变化：`DEFAULT_DENIED_PATTERNS` 去掉程序名，新增 `DENIED_PROGRAMS`；`RuleMatcher::CommandPrefix` 的匹配方式见上表；`--secret-file` 不再以 `ZX_SECRET_FILES` 为缺省值，而是与之合并。`run --help`、`chat --help` 只有 `--auto-approve-cmd` 与 `--secret-file` 的说明改变，其余与 `0984288` 逐字节相同。
- 新增 9 项测试，修复前均实测失败：
  - 策略单元测试 7 项：引号与展开；kubectl 参数在白名单外要询问、在白名单内可放行；危险程序在命令位置被拒绝、作为只读程序的参数不被拒绝；`CommandPrefix` 忽略大小写与空白；Allow 前缀不放行第二条命令。
  - `read_file` 拒绝 FIFO 时不打开它：修复前写端被释放。
  - 命令行与环境变量登记的密钥文件合并：修复前环境变量登记的 token 没有被过滤。
  - 既有测试未改。
- 本机 macOS 验证：stable 与 Rust 1.88.0 各 568 项通过（基线 559 项，在 `0984288` 上实测），6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过，自查脚本的命中项与基线相同；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过。未新增依赖，`Cargo.toml`、`Cargo.lock` 未改动。

二、循环与 chat（已完成，PR #31）：

| 项目 | 修正 |
|---|---|
| 失败回答中的工具调用 | 以 `Error` 或 `Aborted` 结束的回答若仍在投影中，下一次运行开始时由崩溃恢复为其中的调用补写"未执行"结果（这类回答中的调用从不执行）。此前不补写：请求转换只补 "No result provided"，模型无法知道调用根本没有执行，参数不完整的调用还会显示为 `bash({})` |
| 预算用完时的重试 | 没有剩余请求预算时不再安排重试，以 `Error` 结束并保留 provider 的错误。每个回合（含每次尝试的第一个回合）先检查预算与取消，再取出排队的插话。此前会白等一次退避，开出一个不发请求的回合并取走插话，最后报 `MaxTurns`，真实错误丢失 |
| 插话写入与出队 | 输入写入会话后立即出队，再通知观察者。此前要等观察者处理完 `message_end` 才出队，运行 future 在此期间被丢弃时，同一输入会在下一次运行中再写一次 |
| 事件与会话记录 | `tool_call_recovered` 在补写的结果写入之后发出；重试时省略失败回答的 `context_edit` 也发出 `context_edited`（带条目 ID）。此前前者先于写入，后者没有事件 |
| 有界订阅的结束 | 会话释放后 `EventStream::recv` 返回 `None`，`subscribe` 登记的 sink 随会话释放。此前订阅句柄持有 sink 与发送端，`recv` 会一直等待 |
| 钩子追加的 system 消息 | `transform_context` 不能再追加 system 消息，整份计划按钩子失败拒绝；系统提示词只来自配置（P7 的扩展经 `prompt_sections` 加入）。此前可以追加，但下一次尝试或运行开始时会被当作与配置不一致而删除 |
| 钩子与工具的取消令牌 | `HookContext::cancel` 改为只读的 `CancelSignal`（可等待、可查询、可派生子令牌，不能取消运行）；工具拿到的是每次调用的子令牌。此前两者拿到的都是运行本身的令牌，调用 `cancel()` 会中止整个运行，并被报告为调用方取消 |
| 扩展的同步方法 | `Hooks::observes` panic 时按不观察处理并报告 `hook_failed`；运行开始时在 panic 保护下检查每个工具的 `name`、`definition`、`execution_mode` 与 `effects`，并要求 `name` 与声明的函数名一致、声明的函数名不重复。此前这些方法的 panic 会穿出 `prompt`，名字不一致时模型调用会报 "not found" |
| 观察与取消的语义 | 在 `Hooks::on_event`、`Tool::execute` 与 README 中写明：并行执行工具时同一钩子的 `on_event` 可能并发、乱序；运行取消后，挂起的观察者被停止，可能收不到其余事件，需要完整事件流时用 `subscribe_bounded`；运行取消时循环直接丢弃工具 future，必须执行的清理放在 `Drop` 中 |
| chat 中残留的输入 | 每轮结束后取出内核队列中没有被处理的输入：被 Ctrl+C 取消时丢弃并提示条数与内容；正常结束时作为下一轮提问发送。此前取消后残留的插话会在下一轮排在新问题之后，正常结束时迟到的插话也一样 |
| chat 的审批过期 | 审批超时或被取消后立即提示已过期并清除，脚本模式不再吞掉下一条输入；新增 `--approval-timeout-secs`（默认 120）。此前只在下一次循环开头才发现，期间读到的输入被当作"请先审批"丢弃 |

实施记录（PR #31，基于 `ba10d35`，含 PR #29、#30）：

- 按上表实施，补充如下：
  - 失败回答：`StartRecord` 新增 `NeverRun`，对应的恢复状态为 `not_started`，与"会话已记录开始、该调用却没有开始记录"使用同一段"未执行"文本。请求转换不变，旧会话的请求还原不受影响。
  - 预算：重试需要剩余预算；没有时以 `Error` 结束，错误是 provider 的原文。
  - 取消信号：`HookContext::cancel` 的类型由 `CancellationToken` 改为 `CancelSignal`；`HookContext::new` 仍接收 `CancellationToken`。
  - `observes` panic 时，每个事件各报告一次 `hook_failed`。
  - chat：正常结束时没被读到的插话作为下一轮提问发送。这个窗口位于运行结束前最后一次检查队列之后，无法从 CLI 稳定构造，因此没有端到端测试；取出逻辑由 PR #28 的 `take_queued` 测试覆盖。
- 公开 API 与 CLI 的变化：新增 `CancelSignal` 与 `StartRecord::NeverRun`；`HookContext::cancel` 改为 `CancelSignal`；`ContextEdit::Append` 拒绝 system 消息；`LoopSpec::validate` 增加声明名与元数据检查；`tool_call_recovered` 改在结果写入之后发出，重试省略失败回答时发出 `context_edited`；chat 新增 `--approval-timeout-secs`。`run --help`、`serve --help` 与 `ba10d35` 逐字节相同，`chat --help` 只多出这一项。会话文件格式不变。
- 既有测试的调整（对应上表有意改变的行为）：
  - `src/session/recovery.rs` 的 `failed_and_cancelled_answers_never_ran_their_calls`：失败回答中的调用由"不返回"改为返回并标为 `NeverRun`。
  - `tests/hooks.rs` 的 `input_and_context_changes_are_persisted_before_the_model_request`：钩子追加的消息由 system 改为 user，期望值随之改为 `user:extension note`，验证的持久化顺序不变。
  - `tests/hooks.rs` 的 `marked_continuations_join_across_system_updates` 依靠钩子在续写之间追加 system 消息，改为 `src/runner.rs` 的单元测试 `a_final_answer_joins_marked_continuations_across_system_messages`，验证同一条拼接规则；续写拼接的端到端覆盖仍在 `tests/runner.rs`。
  - `tests/hooks.rs` 的 `ContextProbe` 保存的类型由 `CancellationToken` 改为 `CancelSignal`，断言不变。
- 新增 16 项测试，删除上述 1 项，净增 15 项。除替换用的单元测试外，修复前均实测失败：
  - `tests/runner.rs` 4 项：预算用完不重试且保留错误；无预算时排队输入留在队列；重试省略失败回答时发出 `context_edited`；运行在观察者处理时被丢弃，排队输入只写一次。
  - `tests/crash_recovery.rs` 2 项：失败或取消的回答中的调用记为未执行；恢复事件在结果写入之后。
  - `tests/agent_session.rs` 1 项：会话释放后有界订阅结束。
  - `tests/hooks.rs` 5 项：钩子不能追加 system 消息；`observes` 的 panic 被报告且运行继续；声明名不一致的工具、`definition` panic 的工具在请求前被拒绝；工具取消自己的令牌不影响运行。
  - `tests/chat_cli.rs` 2 项：取消后残留的插话被丢弃（修复前第二轮请求的用户消息为 `first`、`second`、`late steering`）；审批过期后下一行作为新提问（只加入 `--approval-timeout-secs` 时实测失败：该行被当作"请先审批"丢弃，随后 `/quit` 中止了运行）。
  - 1 个 `compile_fail` Rustdoc 示例：钩子不能调用 `ctx.cancel.cancel()`（修复前能编译）。
- 本机 macOS 验证：stable 与 Rust 1.88.0 各 616 项通过（基线 601 项，在 `ba10d35` 上实测），6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过，自查脚本的命中项与基线相同；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过。未新增依赖，`Cargo.toml`、`Cargo.lock` 未改动。

三、会话（已完成，PR #34；增量摘要为设计稿，待评审）：

| 项目 | 修正 |
|---|---|
| JSONL 写入的超时 | 记录在会话文件自己的写入线程上写入，存储层的 30 秒追加超时因此对 JSONL 文件同样生效；卡住的写入不再占住运行时线程，也不阻止进程退出。此前写入是异步方法中的同步 I/O，卷卡住时（例如 NFS hard mount）超时无法触发，运行与取消都会一直等待 |
| 停写的原因 | 改为可匹配的 `WritesStopped`（`Interrupted`、`TimedOut`、`Failed`、`ReadOnly`），`SessionStore::writes_stopped` 与 `AgentSession::writes_stopped` 公开查询。此前只是一个字符串，调用方无法区分"需要重新加载"与"只读快照"，长期持有会话的宿主（如 serve）只能看到每次运行都以 `Error` 结束 |
| `context_edit` 的规则 | 写入与加载共用同一组规则：目标是同一分支上更早的消息、不是 system 消息，文本替换不能抹掉工具调用。此前加载时不检查，写入时也不检查最后一条，指向 system 消息的编辑会让投影丢掉系统提示词 |
| 摘要配置 | 拒绝 `triggerBytes - keepRecentBytes` 超过摘要能读取的 2 MiB 的配置。此前能通过校验，到第一次需要摘要时才必然失败 |
| 加载耗时 | 加载时只检查请求记录中可能出错的部分（配置与摘要尝试的引用），不再为每条请求重建一次投影 |
| 长结果落盘的期限 | `SpillOutputs` 给存储设一个低于钩子超时的期限（默认 3 秒），到期按写入失败处理，结果留在原处。此前钩子超时（默认 5 秒）先触发，已脱敏的结果被整条丢弃 |
| 请求还原的版本守护 | 新增一份固定的会话文件及其还原结果（provider 无关的请求与两种发送方式的请求体），测试逐字节比对。此前请求还原总用当前代码重算，投影、摘要渲染或请求转换一改，旧会话的还原会悄悄改变 |

增量摘要（设计稿，待评审）：现在每次摘要都重读全部原始前缀（P5 的决定："重新摘要读取原始投影"），摘要请求随历史线性增长，从第二次起就大于 `triggerBytes`，累计约 2 MiB 后会话只能重开。提议：

1. 新摘要的输入为上一份有效摘要的事实（原样，附其来源条目 ID，标明不得改写）加上一份摘要边界之后到新边界之前的原始记录；推断与计划重新生成。
2. 宿主合并：保留的旧事实原样并入新摘要，模型只写新事实，不改写旧事实，因此不会出现"摘要的摘要"。
3. 事实超过上限（128 条或 64 KiB）时，丢弃最早的旧事实；来源记录仍在会话中，只是不再进入上下文。
4. 旧事实的来源被修正时，与现在一样使整条摘要链失效，回到从原始记录完整摘要。
5. 审计：摘要尝试要记录它接续的上一份摘要，请求审计据此还原增量请求，会话格式随之升为 v5；旧会话仍按现有方式还原。

这会改变 P5 的已定决定与会话格式，评审通过后另行实施。

暂不做：`JsonlFile::create` 在加锁或写头部失败后留下文件。这两种失败无法在测试中稳定构造，按"先补回归测试"的规则暂不修改。

实施记录（PR #34，基于 `b169275`，含 PR #31–#33）：

- 按上表实施，补充如下：
  - 写入线程：每个会话文件一个，按顺序写入；存储层逐条等待结果。超时或运行被丢弃后存储不再写入，此时仍在进行的写入会自行结束。会话释放时，空闲的写入线程立即结束并关闭文件、释放锁；仍卡在写入中的不等待。
  - `WritesStopped` 的 `Display` 与此前的错误文字相同。
  - 加载耗时（本机 release 构建，各一次，会话每轮 1 KiB 提问加 2 KiB 回答）：1000、2000、4000 轮的打开时间由 209.5、836.6、3703.5 毫秒降为 35.7、137.5、563.1 毫秒。剩余的增长来自每条请求记录都要构建一次分支路径，暂不处理。
  - 版本守护：反向验证中把摘要消息的措辞改动一处，测试即失败；修改已还原。
- 公开 API 的变化：新增 `WritesStopped`、`SessionStore::writes_stopped`、`AgentSession::writes_stopped`、`SpillOutputs::write_timeout`。会话文件格式、事件格式与 CLI 参数不变。
- 新增测试：
  - 修复前实测失败：写入卡住时 50 毫秒超时生效（修复前 2 秒后返回 `Ok`）；文本替换不能抹掉工具调用；打开时拒绝指向 system 消息的编辑；无法读完的摘要配置被拒绝。
  - 新接口与新行为：停写原因的四种情况；会话报告停写原因；存储超时后结果留在原处；固定会话的请求还原（`tests/fixtures/replay-v4.jsonl` 与期望文件）。
  - 既有测试未改。
- 本机 macOS 验证：stable 与 Rust 1.88.0 各 659 项通过（基线 651 项，在 `b169275` 上实测），6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过。自查脚本比基线多两处 `let _ =`，都在写入线程中（存储已不再等待时丢弃写入结果；回收线程时忽略 join 的结果），旁边写明了理由。`run --help`、`chat --help`、`serve --help` 与基线逐字节相同。未新增依赖，`Cargo.toml`、`Cargo.lock` 未改动。

四、输出与诊断（已完成，PR #35）：

| 项目 | 修正 |
|---|---|
| bash 的 stdout 与 stderr | 两路输出各自截尾、各带截断说明：两路都长时 stdout 占预算的四分之三、stderr 占四分之一，一路用不完的部分让给另一路；`[stderr]` 标记始终保留。此前先拼接再整体截尾，stderr 较大时 stdout 与标记都被截掉 |
| 文本输出的诊断 | `run` 与 chat 的文本模式打印钩子失败（`[hook] <钩子> <阶段> failed: <原因>`），两份渲染共用同一个函数。此前两者都忽略 `hook_failed`，例如脱敏失败而丢弃输出时，巡检日志里看不到原因 |
| stdout 关闭 | 文本模式把最终回答写到已关闭的 stdout 时忽略写入错误，退出码仍由运行结果决定（JSONL 输出原本如此）。此前 `println!` panic，退出码变为 101 |
| 零超时 | `--tool-timeout-secs`、`--idle-timeout-secs` 至少为 1；`OpenAiCompatible::new` 拒绝为零的空闲与连接超时。此前 0 表示立即超时，每条命令或每个请求都失败 |
| 2xx 的 JSON 响应 | `Content-Type` 为 `application/json` 时读取有界的正文：含 `error` 对象时按 provider 错误报告其信息，否则按协议错误附上正文摘录（服务端可能不支持流式）。其他类型仍按事件流读取，以兼容标注不规范的流式服务。此前一律报 "stream ended without a finish_reason"，真正的原因（例如模型不存在）丢失 |
| 不合法的工具参数 | `argumentsError` 附上模型发出的原文（至多 2 KiB），与 pi 一致，会话与模型都能看到。此前只有 `{}` 与解析错误 |
| `finish_reason` 的重试 | 未知的结束原因以 `finish_reason:<值>` 作为错误码，不再与流内错误码同名，因此按 PR #16 的决定一律不重试。此前 `timeout` 等取值因与可重试的错误码同名而被重试 |
| 容器与会话文件 | 先启动容器，再创建会话文件；容器启动失败时不留下只有头部的会话文件。此前顺序相反（为已经取消的遮蔽而设） |
| 容器执行器 | 执行器声明执行方式（`Executor::execution_mode`，默认并行）：容器执行器为串行，bash 与 read_file 随所用的执行器；容器内的命令总用启动时检查过的 shell。此前工具声明并行而执行器内部排队：整批的开始记录先写完，运行在排队期间被取消时，从未执行的调用被报告为"可能已改变状态"，记录的耗时也包含排队时间；SDK 中 `BashTool::shell` 与容器配置不一致时，启动检查与实际执行用的不是同一个 shell |
| 整理 | `clip` 合为一处；`profile::read_text` 的实现移到中立的 `text` 模块（`read_text_file`，上限由调用方给出），脱敏使用自己的 64 KiB 上限，不再借用 profile 的（消除 `extensions → profile → presets → extensions` 的依赖环）；`BashTool::execute` 拆出可单测的纯函数；更新过期的文档（presets 模块说明、README 中"尚未实现：容器执行"、规则文件中指向旧位置的引用） |

暂不做：
- `cli::prepare` 拆分为配置、进程级运行时与每会话三层：改动面大，等 acp 接入时一起设计。
- FauxProvider 在增量之间响应取消、可脚本化带 `ProviderError` 的失败。
- 目录登记的密钥文件中出现无法读取的文件时整体丢弃输出：这是"无法过滤时宁可丢弃"的取舍，是否改为跳过需要确认。
- 审计中写明自动放行的前缀（见一、安全）。
- provider 契约的兜底（取消后给 provider 的宽限期、`Done`/`Error` 与 `stop_reason` 的一致性检查）。

实施记录（PR #35，基于 `66e1b7b`，含 PR #34）：

- 按上表实施，补充如下：
  - 钩子失败行中的钩子名是注册时给的名称；未命名时是类型名，例如 `zx_agent::extensions::RedactSecrets`。`run` 与 chat 的其余进度行改由同一个函数生成，输出不变；`[model]` 行仍各自输出，措辞不变。
  - 2xx 的 JSON 正文与 HTTP 错误的正文按同样的上限读取（8 KiB 与 `--max-response-bytes` 中较小者），读取失败或超时只损失摘录。
  - 容器执行器：修复前实测（假 docker，第一条命令挂起时取消运行），同一批中排在后面的调用从未执行，却被写成 "Interrupted: … It may have changed state"；修复后它没有开始记录，下一次运行恢复时记为 "Not executed"。
  - `ShellCommand::shell` 对容器执行器不再生效。CLI 中两处的值原本都是 `bash`，只有 SDK 调用方可能配置成不同的值。
- 公开 API 的变化：新增 `Executor::execution_mode`（有默认实现，`Parallel`）；`OpenAiCompatible::new` 拒绝为零的 `idle_timeout` 与 `connect_timeout`；未知 `finish_reason` 的错误码由 `<值>` 改为 `finish_reason:<值>`；`argumentsError` 附上原文；新增 `#[doc(hidden)]` 的 `text` 模块，供命令行程序使用，不属于公开 API。会话文件格式与事件格式不变。
- CLI：`--tool-timeout-secs 0`、`--idle-timeout-secs 0` 改为参数错误（退出码 2）。`run --help`、`chat --help`、`serve --help` 与基线相比，各自只有这两个参数的说明多了 "; at least 1"。
- 新增测试 18 项，没有删除测试：
  - 修复前实测失败 14 项：零超时在请求前被拒绝（CLI）与 provider 拒绝零超时；未知 `finish_reason` 不论取值都不重试；不合法的参数附上原文；成功状态下的错误对象与整份 JSON 回答；stderr 很长时 stdout 的回答仍在；容器无法启动时不留会话文件；容器中的工具串行、排队的调用在取消后记为未执行、命令用启动时检查过的 shell；`run` 与 chat 打印钩子失败；stdout 关闭时退出码仍为 0。其中 provider 拒绝零超时与不合法参数两项以反向验证确认：去掉修复后测试即失败，修改已还原；钩子失败的两项在修复前实测失败，断言改写后又做了一次同样的反向验证。
  - 新接口与新函数 4 项：工具的执行方式随执行器；stdout 与 stderr 的预算分配；`clip` 与 `prefix_within`。
  - 既有测试的调整：两处期望的错误码由 `<值>` 改为 `finish_reason:<值>`（`openai.rs` 的 `only_known_finish_reasons_succeed`、`tests/openai_mock.rs` 的 `an_unknown_finish_reason_is_a_provider_error_not_a_completed_answer`），属于上表中有意的变化；其余既有测试的断言未改。
  - 偶发失败的测试（单独提交）：PR #34 新增的 `a_result_not_stored_in_time_stays_inline` 在本 PR 的 Linux CI 上失败一次：结果被存成了文件。它假设零时限总在存储的第一个文件操作完成前到期，但 Tokio 先 poll 被包裹的 future、再看计时器，计时器最早在下一个毫秒刻度触发（`tokio-1.54.1` 的 `time/timeout.rs`、`runtime/time/source.rs`），磁盘快时写入先完成。本机实测：零时限下经阻塞线程池的打开、写入与 flush，300 次中 239 次完成；旧测试在 8 路并发各 100 次的压力下失败 1 次。改为在只有一个阻塞线程的运行时中占住该线程直到钩子返回，存储确定无法按时完成；并给钩子设 10 秒上限，时限失效时测试失败而不是挂住。同样压力下新版 0 次失败；反向验证中把存储时限改为 1 小时，新版在 10 秒时失败。断言与生产代码未改。
- 本机 macOS 验证：stable 与 Rust 1.88.0 各 677 项通过（基线 659 项，在 `66e1b7b` 上实测），6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 34 项通过，其中 12 项在真实 Docker 上运行。自查脚本的命中与基线相同（`write_line` 中的 `let _ =` 移入新的 `print_line`，理由不变）。未新增依赖，`Cargo.toml`、`Cargo.lock` 未改动。

下面的 P0–P9 继续按各自验收标准推进。

状态取值：未开始、进行中、已实现待 PR review、已完成。每个阶段完成时勾选任务，并记录完成日期和提交。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 场景默认值移出内核 | 已完成 |
| P1 | 钩子、工具调用流水线与人机交互接口 | 已完成（PR #3） |
| P2 | 长期会话 `AgentSession` | 已完成（PR #4） |
| P3 | 终端对话 `chat` | 已完成（PR #5） |
| P4 | profile 文件、上下文文件与 skills | 已完成（PR #6） |
| P5 | 上下文管理 | 已完成（PR #9） |
| P6 | 执行环境与崩溃恢复 | 进行中：崩溃恢复（PR #12）、执行环境接缝（PR #13）、离线容器执行（PR #14）已完成；在线访问评审稿已合入（PR #17），一期已完成：只读命令自动放行（PR #19）、环境变量保护与 ServiceAccount token 脱敏（PR #20）、常驻 Pod 部署包（PR #22）、多集群（PR #24）；容器联网已取消，改为常驻 Pod 部署。待部署方在真实集群上验收；服务模式与 IM 接入的评审稿已合入（PR #27），模块 A（IM 接口与服务，PR #28）、B（Slack 与 `serve`，PR #29）、C（⚑ D:5/CHINA FLAG: 飞书，PR #32）、D（部署，PR #33）已完成，真实平台验收待提供凭据 |
| P7 | 插件与 MCP | 评审中：范围按 D8 收窄，评审稿 `docs/p7-mcp-review.md`（草稿）；前置设计中 A1（会话宿主拆分与可替换的会话存储）已完成（PR #21），B（追踪事件）已完成（PR #23），A2–A8（扩展 API）已完成（PR #25） |
| P8 | 嵌入协议 `acp` | 未开始 |
| P9 | 可选增强 | 未开始 |

### P0 场景默认值移出内核

目标：内核的默认值保持中性，无人值守的取值集中到一个预设里。不引入新依赖。

实现约定：`Limits::max_turns` 与 `run_loop` 的轮次预算使用 `Option<u32>`；`None` 表示不限，`Some(0)` 表示不调用模型。`run --max-turns 0` 保留原有的零轮次语义。预设先放在 Rust 模块 `presets::troubleshoot`，提示词组装与运行机制不依赖该模块。

- [x] 内置说明 `DEFAULT_INSTRUCTIONS` 移到 `troubleshoot` 预设（先用 Rust 代码表达，P4 改为文件）
- [x] `Limits::default()` 改为不限轮次、不限时间、不续写；`troubleshoot` 预设设置 30 轮、15 分钟、续写 1 次
- [x] `run` 命令默认使用 `troubleshoot` 预设

完成日期：2026-10-31。实施提交：[50a2773](https://github.com/xiaoshicae/zx-agent/commit/50a2773e8cb1c030c2bef5ae0df29b616457d378)。
本地 macOS 验证：stable 与 Rust 1.88 各 160 项测试通过、6 项付费 API 测试跳过；格式、clippy、全目标构建、MSRV 检查与严格 Rustdoc 构建通过。
`tests/cli.rs`、`run --help` 和排障说明字符串均与实施前逐字节一致；未新增依赖。

验收：CLI 测试不改即通过；新增测试覆盖内核默认值。

### P1 钩子、工具调用流水线与人机交互接口

目标：扩展点到位，现有的策略检查和截断续写改为钩子实现。不引入新依赖。

- [x] `Hooks` trait 与钩子链：`on_input`、`transform_context`、`before_tool`、`after_tool`、`finish_turn`、`on_event`；可失败的钩子通过 `Result` 返回错误
- [x] 组合规则：按注册顺序执行；`before_tool` 取最严格结果（Deny > Ask > Allow）；策略类钩子出错按拒绝处理；每个钩子有超时
- [x] `Decision` 增加 `Ask`；规则求值顺序 deny → ask → allow，首个匹配生效
- [x] `Interaction` trait 与 `NoHuman`（Ask 等同拒绝）
- [x] `CommandPolicy` 与截断续写迁移为钩子；`run_loop` 参数整理（静态配置与钩子分开）
- [x] 脱敏扩展：`after_tool` 中去掉密钥和令牌（原路线图“执行后钩子”）
- [x] 输入与上下文变换先以追加消息或 `context_edit` 记录，写入成功后再请求模型；P5 加入压缩时沿用这一约束

实现提交：`ea147e4`。

验收（2026-11-01）：原有 `tests/cli.rs` 与 `run --help` 均保持不变；覆盖六个钩子的组合、失败处理、超时与取消、精确审批范围、变换先落盘、脱敏与续写。稳定版与 Rust 1.88 均为 196 项通过，6 项付费 API 测试跳过。命令与原始测试摘要附在本阶段 PR 中。

实现接口：`LoopSpec` 保存静态配置，`LoopConfig` 持有钩子与交互宿主，`LoopControl` 借用运行状态。`PolicyHook` 适配原策略；`ContinueOnLength` 替代 `Limits::continue_on_length`；SDK 迁移见 README。终端审批与配置文件仍属 P3、P4。

### P2 长期会话 `AgentSession`

目标：现有 `Runner` 已支持顺序多次运行并保留会话；在此基础上提供统一的 `AgentSession` API、多订阅者、模型切换和共享调用互斥。不引入新依赖。

- [x] `prompt`、`steer`、`follow_up`、`abort`、`subscribe`、`set_model`
- [x] 同一时刻只有一个运行；运行中 `prompt` 返回忙碌错误，插话须显式调用 `steer` 或 `follow_up`
- [x] `abort` 只取消当前运行，会话保留；多个订阅者都收到事件
- [x] `prompt` 返回 `Result<RunReport, SessionError>`；无人值守仍可一行调用：`session.prompt(task).await?` 取得 `RunReport`

P2 接口与所有权约定（2026-11-01）：

- `AgentSession` 在 `Runner` 外提供共享的 `&self` 方法；`Runner` 保留为独占的运行、重试与审计实现，兼容现有 SDK。
- 状态为“空闲并持有 Runner”或“运行中并持有取消令牌”。`prompt` 原子地取出 Runner，RAII guard 在完成、panic 或 future 被丢弃后归还；锁不跨越异步等待，也不包住订阅回调。
- `prompt(task).await` 返回 `Result<RunReport, SessionError>`；`prompt_with_cancel` 接受外部取消令牌。并发 `prompt` 或运行中 `set_model` 立即返回 `Busy`，不排队。
- `abort` 只取消当前运行，空闲时无操作；`steer`、`follow_up` 沿用现有队列。每次运行创建独立的子令牌，已取消的运行不会影响下一次。
- `subscribe` 返回 RAII `Subscription`；持有期间接收后续事件，drop 后退订，已分发快照中的回调可能完成。构造时的原有 sink 保留。单个订阅 panic 被隔离并移除，通过订阅句柄与会话失败计数报告。
- `set_model(ModelRef)` 同步切换 provider 与模型 ID，空模型 ID 报错；保留历史、提示词、工具、钩子、上限和审批缓存；空闲时可通过 `clear_approvals` 撤销授权。API 不需要 I/O，因此将方案草图中的 async 改为同步方法；不会改写会话头部记录的初始模型。
- `run` 与库示例接入 `AgentSession`；CLI 参数和 `tests/cli.rs` 保持不变。终端对话仍属 P3。

实现提交：`8831ec0`。

本地验证（2026-11-01）：stable 与 Rust 1.88 各 221 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 和 MSRV 全目标编译通过。

验收：新增 24 项共享会话行为测试与 1 个 Rustdoc 编译示例，覆盖忙碌错误、取消与丢弃后复用、并发调用、订阅和退订、回调重入与 panic、模型切换、审批撤销及审计顺序。原有 `tests/cli.rs` 和 `run --help` 逐字节不变；未新增依赖。完整检查与原始测试输出附在本阶段 PR 中。

### P3 终端对话 `chat`

目标：最小可用的终端对话。本阶段采用专用输入线程，不新增 crate 或启用 io-std；D2 中已授权的 io-std 与可选 rustyline 本阶段无需引入。

- [x] 流式显示回答与工具调用
- [x] `TerminalInteraction`：展示完整命令、依据与影响范围，可选“允许一次 / 本会话允许 / 拒绝”
- [x] 斜杠命令：`/new`、`/model`、`/compact`（P5 已接入）、`/quit`
- [x] Ctrl+C 只中止当前运行，不退出程序

P3 实施约定（2026-11-01）：

- 新增 `chat` 子命令，采用通用交互说明；每次提问默认不限轮次、不限总时间、不自动续写，外部等待仍受 provider 和审批超时约束。`run` 的测试、帮助文本和默认行为保持兼容。
- 共享连接、工具和会话选项，两个前端都组装同一个 `AgentSession`。chat 流式显示文本；可选 JSONL 沿用既有事件格式。
- 所有 bash 调用在通过原命令策略后请求确认，read_file 沿用原读取能力。完整工具名、参数、工作目录和审批理由先展示；用 `/approve <id> once`、`/approve <id> session`、`/deny <id> [reason]` 回答。会话允许仍只匹配完全相同的工具和参数，明确拒绝不能被审批绕过。
- 单一终端输入所有者协调普通输入与审批，审批通过有界请求通道和一次性回复传递，审批答案必须匹配当前请求编号，取消后丢弃过期请求，旧答案不能授权下一条命令。TTY 运行中普通输入作为 steering；管道脚本逐轮处理，只有待审批时读取审批命令，避免预读的 /quit 取消前一条任务。
- 按 Tokio stdin 源码对交互终端的建议，使用进程生命周期的专用阻塞读取线程和有界通道；单行输入限制 1 MiB。无需启用 io-std 或新增 crate；退出不会等待 Tokio 无法取消的 stdin 读取。
- `/new` 创建新会话并清空授权；启用文件会话时创建唯一的新文件，保留旧日志。`/model [id]` 查看或在空闲时切换同一 provider 的模型；运行中拒绝切换。P3 为 `/compact` 预留入口，P5 已接入共享会话压缩 API。
- `/quit` 退出；Ctrl+C 只取消当前运行或待审批，不退出会话，空闲时保留会话；SIGTERM 取消并退出。审批输入结束时拒绝并结束当前运行，避免无人可答时继续等待。
- 不引入 rustyline；P3 先交付逐行 REPL，行编辑与完整 TUI 不在本阶段。

实现提交：`a5f94a8`。完成日期：2026-11-01（PR #5 合并）。

验收（2026-11-01）：新增 15 项单元测试、26 项 CLI 测试，其中 3 项使用真实 Unix PTY。stable 与 Rust 1.88 各 262 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。另用本地假服务在实际终端走通中文对话、拒绝审批、模型切换、/new、/compact 提示和退出。真实模型 API 试用未运行。

兼容性：`tests/cli.rs`、`run --help`、`Cargo.toml`、`Cargo.lock` 与 P2 基线逐字节相同；原始测试摘要附在本阶段 PR 中。

### P4 profile 文件、上下文文件与 skills

目标：不写代码即可定义场景。依赖：`toml`（D2 已授权；上游 [Cargo.toml](https://github.com/toml-rs/toml/blob/main/Cargo.toml) 在 2026-10-31 核对的 MSRV 为 1.85，接入时按选定版本复核）。

P4 实施约定（2026-11-01）：

- `run` 默认加载嵌入二进制的 `profiles/troubleshoot.toml`，`chat` 默认加载 `profiles/assistant.toml`；说明放在相邻 Markdown 文件。保留现有预设 SDK 接口，运行时配置以 TOML 为准。
- `--profile` 接受内置名称或 TOML 路径；自定义文件可用 `extends` 选择一个内置基线，不支持任意文件继承。叠加顺序为内置基线、文件、显式 CLI 参数；显式传入与旧默认值相同的参数也必须覆盖文件。原有 `tests/cli.rs` 不改，帮助仅新增配置选项。
- TOML 字段使用 camelCase，拒绝未知字段和无效类型。规则包含命令前缀 allow、命令子串 deny、askTools 与 denyTools；deny 累加，不能被 CLI 工具开关或 allow 覆盖。run 始终使用 NoHuman，chat 保留终端审批，profile 不切换前端。
- 配置支持轮次、总时间、截断续写与已知密钥脱敏；`maxTurns = "unlimited"` 表示不限，0 仍表示不调用模型。当前只支持 `execution = "local"`，脱敏不可关闭；未实现的容器、任意扩展等配置直接报错。
- 自动读取最近 Git 根目录到 cwd 的 AGENTS.md（无 Git 根时仅 cwd），不隐式读取用户主目录配置（2026-11-01 调整：`troubleshoot` 默认关闭自动发现，发现的文件须位于项目根目录内，见上方 P4 复核修正）；profile 可添加上下文文件，路径相对 profile 文件。CLI `--context-file` 相对启动目录，`--no-context` 可禁用上下文加载。
- skills 通过 TOML 的 `[[skills]]` 显式登记 name、description、path，兼容把现有 SKILL.md 当正文文件；不解析 YAML frontmatter、不自动扫描全局 skill 目录。启动只向提示词加入目录信息；`read_skill` 工具按名称加载正文，经过原钩子、策略、取消、脱敏与持久化流程。未启用该工具时不加入目录。
- 配置、说明、单个上下文和 skill 正文均有 64 KiB 上限；上下文总量 256 KiB，skills 最多 64 项。只读普通 UTF-8 文件，超限或无效内容明确报错。上下文与 skill 文本不能改变已组装的工具及策略。
- 引入已授权的 `toml 1.1.8`，仅启用 parse、serde、std；其 crate 声明 MSRV 1.85，仍用 Rust 1.88 验证。

- [x] profile 文件格式（TOML）：说明、工具、规则、上限、扩展、执行环境
- [x] 叠加顺序：内置默认 < profile 文件 < 命令行参数；deny 规则只增不减
- [x] 内置两个 profile：`troubleshoot`（P0 的预设）与 `assistant`（通用对话）
- [x] 读取 AGENTS.md 等上下文文件；skills 按需加载
- [x] 命令行增加 `--profile`；配置错误时退出码 2

验收：两个内置 profile 均由文件定义；配置错误的测试。

实现提交：`7fb3124`。完成日期：2026-11-01（PR #6 合并）。

本地验收（2026-11-01）：新增 24 项单元测试、18 项 CLI 测试；stable 与 Rust 1.88 各 304 项通过、6 项付费 API 测试跳过。格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。两个内置 profile 及完整评审示例通过零请求离线试用；真实模型 API 未运行。

兼容性：原 `tests/cli.rs` 和两份预设说明逐字节不变，原 SDK 场景辅助接口保留；`run --help` 仅增加 profile 和上下文选项。新增配置覆盖、deny、上下文边界与预算、skill 懒加载和审计测试；上下文分隔符预算及非 UTF-8 原生路径的回归均先失败后修复。配置说明与完整示例见 [PROFILES.md](PROFILES.md)。

### P5 上下文管理

目标：长对话不超出上下文窗口，且不丢失关键信息（原路线图“上下文压缩”）。不引入新依赖。

开工前置（2026-11-01 补充）：经用户授权，先运行一次 `tests/real_api.rs`，确认目标服务的流式格式、工具调用与用量统计。provider 的自动化测试只使用本地假服务，而压缩依赖真实模型的行为与用量。

开工前置补跑记录（2026-11-01，经用户授权，基于 `0c37a66`，在实现完成后补跑）：DeepSeek（`https://api.deepseek.com/v1`，默认 `collapse`）的 `deepseek-flash` 与 `deepseek-chat` 各运行一次 `tests/real_api.rs`，均为 6 项通过。覆盖流式文本与用量统计、bash 工具往返、策略拒绝传回模型、输出上限截断（`Truncated`）、恢复会话后的提示词增量更新、无效 key 不重试。实测用量：`deepseek-flash` 3045 token，`deepseek-chat` 2810 token，单次回答最多 76 个输出 token。该服务的 `/v1/models` 只列出 `deepseek-flash` 与 `deepseek-v4-pro`；`deepseek-chat` 的请求同样成功，原因未核实。这组测试不含压缩与请求审计用例，`inline` 模式未在该服务上运行。

P5 实施约定（2026-11-01）：

- 新建会话使用 v3；旧 v1/v2 日志在首次写入压缩记录前追加 `format_upgrade`，不改写头部。新增 `compaction_started`、`compaction`、`compaction_failed`：请求先落盘，成功摘要或失败结果随后追加，取消或崩溃留下的开始记录不改变投影。
- 成功条目记录 firstKeptEntryId、摘要和请求记录引用。投影保留全部 system 消息、摘要及保留范围；原消息与 context_edit 不改写，分支各自投影。摘要模型使用该分支的原始投影及条目 ID，不输入旧摘要，避免摘要的摘要。
- 摘要分 facts（逐条 sourceEntryIds）、inferences、plans。校验引用属于本次源条目、字段与字节上限；拒绝无来源事实、截断回答、工具调用或无效 JSON。引用校验不代表模型事实的语义已被证明。
- 在现有输入与上下文钩子持久化后，Runner 的宿主准备步骤可执行自动压缩，再请求任务模型；循环不判断交互模式。自动摘要调用计入该轮模型请求预算和 usage，失败不伪装成任务回答、不自动重试。
- chat 默认在序列化上下文字节数超过 256 KiB 时触发，保留最近约 64 KiB 及最后用户输入；run 默认关闭自动压缩。阈值是可配置的字节预算，不声称等同模型 token 上限。保留内容无法缩到阈值时明确停止；系统提示词和最新输入不静默截断。（2026-11-01 调整：付费的自动摘要请求失败后，本会话暂停自动摘要，见上方 P5 复核修正。）
- 摘要源最多 2 MiB，正文最多 64 KiB，单次摘要最多 2048 输出 token、120 秒；profile 可调整触发与保留预算。手动 `AgentSession::compact` 与 prompt 共用独占运行权，支持 Busy、取消和丢弃后复用；chat 的 `/compact` 接入同一 API。
- 大工具结果通过最后的 after_tool 扩展，在已知密钥脱敏后写入普通 UTF-8 文件，只将引用送入会话与模型；保存的是原工具返回的有界内容，既有截断/分页边界不变。默认阈值 8 KiB、文件上限 1 MiB，文件名为 UUID，Unix 新目录/文件权限为 0700/0600。
- 注册 `read_output` 按 UUID 与 UTF-8 字节 offset 分页读取结果，单页不超过落盘阈值；读取结果和 read_skill 不再次落盘，避免递归引用。只有启用 read_output 时安装落盘扩展，显式 CLI 工具集合仍生效。结果目录默认 cwd/.zx-agent/outputs，文件不自动删除并可在同目录续用；加入 gitignore。（2026-11-01 调整：`troubleshoot` 默认不启用 `read_output`；写入失败时结果保持内联；默认目录须位于工作目录内，并在目录中写入 `.gitignore`，见上方 P5 复核修正。）
- profile 使用 `[compaction]` 与 `[toolOutput]`；CLI 可覆盖自动触发阈值或关闭自动压缩。默认 SDK 仍不自动压缩、不落盘，已有构造器保持兼容；可在 Runner 上显式配置后交给 AgentSession。
- 用 FauxProvider、本地假 HTTP 服务、会话兼容/分支/失败写入测试验收，不运行付费 API 测试。
- 请求审计（G3，2026-11-01 补充）：每次任务请求和摘要请求调用 `Provider::stream` 前，先追加 `request_started`；provider、模型、工具定义、`max_tokens` 或 provider 转换参数与当前分支上最近一条 `request_config` 不同时，先追加新的 `request_config`。`request_started` 只记录配置引用和消息来源（会话投影，或摘要尝试的源条目与请求时间戳），不复制整份对话；`SessionStore::replay_request` 据此还原与 provider 无关的 `Request`。写入前校验待发消息与持久化投影一致；任何一步写入失败都不发请求，并按不变量 7 取消运行。默认 SDK 也记录，这是会话文件约定的变化：会话中除 message、context_edit、压缩记录外，还会出现这两类条目。旧 v1/v2 日志在首次写入请求或压缩记录前追加 `format_upgrade`，不再只在首次压缩前。不记录 API key、URL 与 HTTP 头；`Provider::request_options` 只返回非机密的转换参数，默认 `null`，OpenAI 兼容 provider 记录格式版本与 `systemUpdates`，可用 `openai::replay_body` 重建请求体。`request_started` 只表示准备发送，不能证明服务端已接受；没有对应回答说明发送结果未知。

- [x] 会话条目 `compaction`：记录保留的第一条条目与摘要（沿用 pi 的结构）
- [x] 首次写入 `compaction` 前升级会话格式，兼容读取旧版本；版本号按当时的格式递增，不延后到 P6
- [x] 同一次格式升级加入请求配置条目：记录工具定义、模型与发送参数，变化时追加，使每次请求都能从会话文件还原（G3）
- [x] 摘要分“环境事实（附原始条目出处）”与“推断和计划”两部分；原始历史不改写
- [x] 触发方式：达到阈值自动触发，或在 `chat` 中用 `/compact` 手动触发；无人值守的短任务默认关闭
- [x] 工具侧预算：大结果写入文件，只返回路径

实现提交：`f78cbcb`、`e9be6a9`。完成日期：2026-11-01（PR #9 合并）。PR 的 CI（macOS、Ubuntu、Rust 1.88、格式 / clippy / 文档）在 `0c37a66` 上全部通过。

验收（2026-11-01）：FauxProvider、本地 HTTP、会话 v1/v2/v3 与分支、四种压缩写入失败、取消 / 超时 / 丢弃、请求计费与摘要期间 steering、CLI profile 与 Unix PTY、长结果脱敏与分页均通过。stable 与 Rust 1.88.0 各 **356 项通过（含 2 项 doctest），6 项付费测试跳过**；fmt、clippy、严格 Rustdoc 均通过。`tests/cli.rs` 逐字节保留，Cargo 依赖未变。

请求审计验收（2026-11-01）：每次任务请求与摘要请求都能从重新打开的会话文件还原，并与实际发送的 `Request` 逐项相等；配置只在变化时追加（含切换模型）；v1/v2 会话在首次请求前只升级一次且原有字节不变；`request_config` 或 `request_started` 写入失败时不发请求；OpenAI 兼容 provider 的还原请求体与实际请求体在两种 `systemUpdates` 模式下一致，且不含 API key。原有 4 项测试按新的会话文件约定更新了预期：3 项只统计 message 行，1 项允许出现请求审计条目，原本要验证的行为不变。stable 与 Rust 1.88.0 各 **385 项通过，6 项付费测试跳过**；fmt、clippy、严格 Rustdoc、MSRV 全目标检查均通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动。

未验证：真实模型的摘要准确率、信息保真和 token 窗口适配；压缩与请求审计未在真实模型上运行（付费测试只覆盖上面开工前置补跑记录所列项）。配置预算及限制见 [PROFILES.md](PROFILES.md#上下文管理)。

### P6 执行环境与崩溃恢复

目标：命令可以放进隔离环境执行；进程崩溃后能正确恢复。依赖：无（调用本机 `docker` 命令）。D5 已于 2026-11-01 确认：默认本机执行，容器显式开启。

实施拆分（2026-11-01）：先做崩溃恢复（`effects()`、`tool_call_started`、恢复、会话 v4），不依赖执行环境；再做执行环境（`Executor` 接缝、离线 `ContainerExecutor`、profile 容器选项），`Executor` 接缝与容器设计一起定稿；在线访问的网络与凭据范围评审后再实施。

崩溃恢复实施约定（2026-11-01）：

- `Tool::effects()` 返回 `Effects::ReadOnly` 或 `Effects::SideEffects`，默认 `SideEffects`，未声明的工具按有副作用处理。内置工具中 bash 有副作用，read_file、read_output、read_skill 只读。
- 有副作用的调用通过检查与审批后、执行前，追加 `tool_call_started`（`toolCallId`、`toolName`）。并行批次先按调用顺序写完全部开始记录，再并发执行。写入失败时该调用不执行，并按不变量 7 取消运行。只读工具不写开始记录。
- 新建会话为 v4。旧 v1–v3 会话在首次写入 `tool_call_started` 前追加 `format_upgrade`（version 4），不改写头部；请求与压缩记录仍只要求 v3。只支持到 v3 的程序会拒绝打开 v4 会话。
- 恢复：Runner 每次运行开始、追加提示词之前，检查当前分支末尾的 assistant 消息。其中没有结果记录的工具调用，各追加一条 `isError` 的工具结果，并发出 `tool_call_recovered` 事件；不重放调用。按记录分四种：有开始记录为 `started`（结果未知，可能已改变状态）；会话已按 v4 记录开始、工具有副作用、没有开始记录为 `not_started`（未执行）；只读工具为 `read_only`；旧格式或当前未配置该工具为 `start_unknown`（是否开始未知，按可能已改变状态处理）。
- 只处理分支末尾的调用。之后已有其他消息、已被 `context_edit` 省略或已被摘要替代的调用不补结果，provider 转换时补占位的行为不变。`Error` / `Aborted` 回答里的调用从未执行，不处理。恢复结果写入失败时，按不变量 7 取消运行。
- 用 FauxProvider、构造的崩溃会话文件、写入失败、丢弃 future，以及对正在执行命令的 `run` 进程发 SIGKILL 的测试验收；不涉及执行环境，`tests/cli.rs` 不改。

执行环境实施约定（2026-11-01，经用户确认）：

- 分两个 PR：先加 `Executor` 接缝与 `LocalExecutor`，行为不变；再加离线 `ContainerExecutor` 与配置。
- `Executor` 接缝（`src/executor/`）提供两个操作：`run` 在给定目录执行一条 shell 命令，响应超时与取消，返回有上限的 stdout / stderr 与结束方式；`read_file` 只读取不超过上限的普通文件。bash 负责参数、结果格式与截断，read_file 负责分页；两者各持有一个执行器，默认 `LocalExecutor`。`LocalExecutor` 是原有实现搬迁而来：进程组、超时与取消后终止、输出捕获上限、只读普通文件与实际读取量上限。read_skill 读取宿主机上的 skill 文件，固定使用 `LocalExecutor`。
- 容器生命周期：每个进程一个容器（`run` 对应一次任务，`chat` 对应整个会话），在启动时创建（要先读取容器的系统信息写进提示词，配置错误也在启动时暴露），进程退出时删除；容器带 `zx-agent.session` 标签，便于识别。
- 工作目录只读挂载进容器；命令只能写容器内的 tmpfs `/tmp`。网络这一轮固定为 `none`，不提供配置项，在线访问方案评审后再加。
- 超时与取消：2026-11-01 在本机 Docker 29.9.2 上实测，SIGKILL 或 SIGTERM 结束本机的 `docker exec` 客户端（不带 `-t`）后，容器内的命令仍在运行（单次观测，机制未从源码确认）。因此容器执行器在容器内终止命令，不依赖结束客户端（2026-11-01 Codex 评审后由按进程组改为按命令用户终止，见上方修正记录）。
- 容器模式下，系统提示词的 `<environment>` 描述容器而不是宿主机。
- 测试：默认 `cargo test` 用记录参数的假 docker 程序，不访问网络；真实 Docker 测试仅在设置 `ZX_DOCKER_TEST_IMAGE` 时运行，并在 CI 的 Linux 任务中拉取镜像后开启。

容器执行器实现细节（2026-11-01，依据同日在本机 Docker 29.9.2、Ubuntu 系镜像上的实测，均为单次观测）：

- 容器以 `docker run -i --rm --init --sig-proxy=false` 启动，PID 1 读取 agent 持有的 stdin。实测关闭 stdin、或用 SIGKILL 结束 `docker run` 客户端后，容器在 5 秒内被删除；因此正常退出（结束客户端）和 agent 崩溃（stdin 随之关闭）都会删除容器，不需要在析构时同步调用 `docker rm`。docker 客户端放在独立进程组，终端发给前台进程组的信号只送达 agent，由它决定停止什么；另外实测即使不放进独立进程组，`docker run --sig-proxy=false` 客户端收到 SIGINT（默认处置）后仍在运行、容器保留，所以 chat 中按 Ctrl-C 不会删除容器（机制未从 Docker 源码确认）。
- 加固参数：`--network none --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges --pids-limit --memory --user`，实测均被接受；默认用户 `65534:65534`、内存 1g、进程数 256，profile 可调。镜像不自动拉取（`--pull never`），缺少时提示先 `docker pull`。
- 启动检查：实测 `docker exec` 启动的进程既是进程组组长也是会话首进程，但这只是对一个运行时的观测，所以启动时在容器内核对；不成立、缺少配置的 shell 与 `cat`、`head`、`sleep`、`mkdir`、`rm`、`uname`、或容器用户不能读取工作目录时，报配置错误。`docker run` 不设工作目录（PID 1 不需要），否则不可读的工作目录会让容器直接启动失败，只留下 Docker 自己的报错。（2026-11-01 调整：按用户清理后不再依赖进程组，启动检查改为核对命令不以 root 运行、镜像有 shell 与 `cat`、`head`、`uname`、工作目录可读。）
- 命令：包装脚本先把自身 PID（即进程组号）写到 tmpfs，再 `exec` 配置的 shell。超时或取消时另起一次 `docker exec`，先 SIGSTOP 再 SIGKILL 整个进程组（实测以非 root 用户可行）；命令正常结束后也清理一次进程组，结束后台子进程。实测主进程退出但后台子进程仍占用输出时，`docker exec` 约 2 秒后返回。命令被信号终止时，退出码沿用 `docker exec` 的 128+信号编号（实测 SIGKILL 为 137），与本机执行的“terminated by a signal”不同。运行中的 future 被丢弃时，尽力在后台发起同样的清理。（2026-11-01 调整：不再写 PID 文件；命令执行前后按用户清理，命令与读文件逐条执行，见上方修正记录。）
- 会话文件：实测在 Docker Desktop 上，只读挂载的文件对容器用户显示为其所有，权限为 0600 的文件也能读到。因此会话文件位于工作目录内时，用 `/dev/null` 覆盖挂载到同一路径（实测容器内读到空文件，宿主机文件不变）。工作目录中的其他文件（含 `.zx-agent/outputs` 中已脱敏的工具结果）对容器只读可见。原生 Linux 的 Docker 保留宿主机权限，默认用户 65534 只能读取对其他用户可读的文件，工作目录本身也须可读（`tempfile` 创建的 0700 目录不行，测试因此使用 0755 的工作目录），可用 `container.user` 改用其他用户。（2026-11-01 调整：遮蔽挡不住硬链接与同目录的其他会话文件，已改为要求会话文件位于工作目录外，见上方修正记录。）
- read_file 在容器内读取，目录、不存在、特殊文件、无读权限分别报错，读取量上限同本机；单次读取 30 秒超时。
- 配置：profile 的 `execution` 取 `local`（默认）或 `container`；`[container]` 表含 `image`（容器模式必填）、`user`、`memory`、`pidsLimit`。CLI 的 `--execution`、`--container-image` 覆盖 profile；在本机模式下给出 `--container-image` 视为配置错误。

- [x] `Executor` 接缝：`LocalExecutor`（现有行为）与 `ContainerExecutor`（接缝与 `LocalExecutor`：PR #13；`ContainerExecutor`：PR #14）
- [x] profile 的 `execution` 增加容器选项（D5：显式开启，默认仍为本机；PR #14）
- [x] `ContainerExecutor`：每个进程创建一个沙箱容器（2026-11-01 由“每次运行”调整，见上方约定；网络默认拒绝，在线任务按评审方案限制访问范围；只读根文件系统、非 root、去掉全部 capability、进程数与内存上限）；每条命令通过 `docker exec` 执行；`read_file` 也在容器内读取；运行结束删除容器（离线部分已完成，PR #14；在线访问随下一项评审）
- [x] API key 和会话文件只留在 Agent 进程，不进入容器；工具需要的凭据以只读方式挂载（API key 与会话文件：PR #14。容器中的凭据挂载随容器联网一起取消；改为常驻 Pod 部署后，各集群的凭据以只读卷挂载：PR #22、#24）
- [x] 列出在线排障所需的 API 地址、网络与凭据范围；无网络的容器只用于离线排障，在线场景的受限访问方案在实施前评审（评审稿 `docs/p6-online-access-review.md` 已合入，PR #17；按用户确认，先定机制、地址由部署方配置；一期已实施，见下方“在线访问一期”）
- [x] `Tool` trait 增加 `effects()`：只读或有副作用
- [x] 有副作用的工具执行前追加 `tool_call_started` 条目；恢复时把“已开始、无结果”的调用标记为结果未知，不自动重放
- [x] 首次写入 `tool_call_started` 前再次升级会话格式，兼容读取 P5 及更早版本

在线访问一期（2026-11-01 起；评审稿合入后由用户要求推进，按其第 6 节分 PR 实施）：

- [x] chat 只读命令自动放行：`rules.autoApprove` 与 `--auto-approve-cmd`，assistant 内置只读清单（已完成，PR #19）
- ~~容器联网与 kubeconfig~~：已取消（2026-11-01，见下方“部署方向”）
- [x] Linux 上保护 agent 的环境变量（`PR_SET_DUMPABLE`），ServiceAccount token 登记脱敏（已完成，PR #20）
- [x] 常驻 Pod 部署包（单集群）：Deployment、RBAC、Secret 与会话存储卷的模板和文档（由“Kubernetes 部署说明”调整；已完成，PR #22）
- [x] 多集群：多个 context 的 kubeconfig，以及其他集群 token 文件的脱敏登记（已完成，PR #24）

部署方向（2026-11-01，用户确认）：Agent 以常驻 Pod 部署，kubectl 在 Pod 内以本机执行方式运行，凭据来自 ServiceAccount，因此取消容器执行的联网与 kubeconfig。用户确认的任务来源：kubectl exec 进 Pod 对话、定时巡检、告警 Webhook、IM 机器人。用户希望增加的功能：写操作（需审批）、专用查询工具 / MCP、多集群访问、记住历史排障。其中告警 Webhook 与 IM 机器人需要新的服务模式，另行评审后实施。评审稿见 `docs/serve-im-review.md`（已合入，PR #27）：一期为 `zx-agent serve`、IM 接口与两个平台的实现，告警 Webhook 放在二期。

IM 接入一期（2026-11-01 起；评审稿合入后由用户要求推进，按其第 6 节分 PR 实施）：

- [x] A. IM 接口与服务：`im` 模块（`ImChannel`、`Server`）与 `AgentSession::take_queued`，用内存实现测试（已完成，PR #28）
- [x] B. Slack：Socket Mode、`zx-agent serve` 子命令与配置文件、IM 凭据的隐藏与脱敏（引入 `tokio-tungstenite`，决策 D7；已完成，PR #29）
- [x] C. ⚑ D:5/CHINA FLAG: 飞书：长连接、token、发送与回复、卡片（Subject to Legal/Export Compliance approval；已完成，PR #32）
- [x] D. 部署：`serve` 组件（共享进程命名空间、主进程改为 serve、配置与凭据）、部署说明与本机集群验收（已完成，PR #33）
- [ ] 真实平台验收：在 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次 @、回答与审批（需要测试应用的凭据）

自动放行实施记录（2026-11-01，PR #19）：

- 实现：`policy::AutoApprove` 判断命令能否不经审批执行，四个条件见评审稿 3.1。`RulePolicy::auto_approve` 把该策略对清单覆盖命令的 Ask 降为 Allow，Deny 不变。多个策略钩子取最严格的结论（`src/agent/hooks.rs`），所以 chat 自带的 bash 确认规则与 profile `askTools` 生成的规则都接入同一份清单。profile 的 `rules.autoApprove` 与 CLI 的 `--auto-approve-cmd` 都是替换语义，与 `allow` 一致。
- 行为变化：chat（assistant）中清单内的只读命令不再请求审批，`profiles/assistant.md` 的说明随之调整；run（troubleshoot）没有 `askTools`，行为不变。
- 新增 9 项测试：
  - 策略单元测试 5 项：清单覆盖的管道；清单外的命令与 shell 语法；`secret`、URL 与连接参数；只把 bash 的 Ask 降为 Allow；不推翻拒绝。
  - profile 单元测试 1 项：文件替换清单。
  - `tests/chat_cli.rs` 3 项：只读命令不经审批；含 `secret` 或重定向的命令仍要审批；`--auto-approve-cmd` 替换清单。
  - 原有 2 项 profile 测试补充了断言：内置清单；空规则报错。
- 反向验证：分别去掉 chat 侧或 profile 侧的接入时，`tests/chat_cli.rs` 中两项自动放行测试都失败，另一项不受影响；修改已还原。
- 本地 macOS 基于 `533b6d7`：stable 与 Rust 1.88 各 477 项通过（基线 468 项）、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。
- 未验证：清单中的 kubectl 命令没有在真实集群上执行，测试只用 `grep`、`head`、`printf`；规则只看字面，局限见评审稿缺口 ②。

环境变量保护与 ServiceAccount token 脱敏实施记录（2026-11-01，PR #20）：

- 实现：
  - Linux 上，`main` 在解析参数前调用 `prctl(PR_SET_DUMPABLE, 0)`；调用失败时在 stderr 报告并继续运行。依据是评审稿附录 9.2 的 E1–E4 实测：同一用户可读，设置后被拒绝，带 `CAP_SYS_PTRACE` 时仍可读。
  - `RedactSecrets::with_secret_files` 登记文件，每个文件存放一个密钥，每次过滤前重新读取，最近 32 个值继续过滤。文件不存在或无权读取时跳过；其他读取失败使钩子失败并丢弃输出。
  - CLI 登记 `/var/run/secrets/kubernetes.io/serviceaccount/token`，即 client-go `rest.InClusterConfig` 读取的路径。
- 新增 4 项测试：
  - `tests/hooks.rs` 3 项：文件重新读取且旧值继续过滤；缺失或空文件不影响其他过滤；不是普通文本的文件使钩子失败。
  - `tests/run_cli.rs` 1 项，只在 Linux 上编译：命令读取 `/proc/$PPID/environ` 得到 `Permission denied`，读不到 agent 环境中的探针变量。
- 本地 macOS 基于 `3af5ef1`：stable 与 Rust 1.88 各 480 项通过（基线 477 项；Linux 专用的 1 项在 macOS 上不编译），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`Cargo.toml`、`Cargo.lock` 未改动，`libc` 原本就是 Unix 依赖。
- 本机 Docker 中的 Linux 验证（`rust:slim-bookworm`，rustc 1.99.1，aarch64，以 root 和 Docker 默认 capability 运行）：
  - clippy 全目标无警告；新增 4 项测试全部通过。
  - 反向验证：把 `prctl` 调用换成空操作后，`/proc` 探针测试失败。修改只作用于容器内的源码副本。
  - 以非 root 用户运行的 Linux 结果以 CI 为准。
- 未验证：真实 Pod 中的效果（评审稿缺口 ④；2026-11-01 已在本机集群的 Pod 中确认，见下方 PR #22 的记录）；ServiceAccount token 的实际轮换周期。

常驻 Pod 部署包实施记录（2026-11-01，PR #22，单集群）：

- 内容：
  - `deploy/kubernetes/` 下的 Dockerfile：镜像包含 zx-agent、bash、kubectl、curl、jq、tini，以 uid 10001 运行。
  - kustomize 清单：两个账号、`view` 绑定、可选的 `operator.yaml` 写权限、会话存储卷、常驻 Deployment、定时巡检 CronJob。
  - 部署说明；仓库根目录新增 `.dockerignore`。
- 设计取舍：
  - 常驻 Pod 的主进程是 tini 与 sleep，它们环境中的 API key 能被同一用户的命令读到，读到的值会被脱敏。巡检 Pod 的主进程就是 zx-agent，不受影响。服务模式作为主进程之后再收紧。
  - 定时巡检使用独立的只读账号。写权限只绑定常驻 Pod 的账号，默认不启用。
  - ClusterRoleBinding 的 subjects 不写命名空间。实测 kustomize 覆盖命名空间时，不会改写 subjects 中已写明的命名空间；留空时则会填入。因此这两个文件只能用 `kubectl apply -k` 应用。
- 验证：
  - 在本机 Docker Desktop 的 Kubernetes v1.36.1 上逐项实测，结果见 `deploy/kubernetes/README.md` 的“验证记录”。
  - 评审稿缺口 ④ 的三项在该集群上得到确认：Pod 内 kubectl 使用 ServiceAccount；`view` 的实际权限；Pod 内的 `prctl`。
  - 完整验证基于 `dd43cce`。rebase 到包含 PR #21 的 `5c20655` 之后复验：
    - 会话锁在存储卷上生效：chat 打开会话期间，另一个 `kubectl exec` 中的 run 报告 `is in use`，退出码 2；
    - 脱敏、`/proc` 保护、chat 审批与定时巡检不变。
  - 复验时发现，重复使用同一镜像标签，节点会沿用缓存的旧镜像；部署说明据此要求每次构建使用新标签。
  - PR #22 的首次 CI 中，`tests/container.rs` 的 `a_dropped_run_still_stops_its_command` 在 ubuntu 上以 `Text file busy (os error 26)` 失败：假 docker 脚本写出后立即执行。本 PR 未改动 Rust 代码与测试，只重跑失败的任务后通过。这是已有的偶发问题，另行处理。
  - 已修复（PR #26）：`FakeDocker::new` 写出脚本后先以 `--zx-ready` 试运行一次（脚本直接退出，不记为调用），遇到 `ExecutableFileBusy` 时等 5 毫秒重试，最多 200 次。原因是另一测试线程恰在脚本写入期间 fork，子进程在 exec 之前持有写描述符的副本，这时执行脚本得到 ETXTBSY；试运行成功之后，任何进程都不再持有该文件的写描述符。在 Linux 容器（`rust:slim-bookworm`）中实测：
    - 写出后立即执行，另一线程同时 fork 时 300 次中 2 次 ETXTBSY，不 fork 时 0 次；
    - 容器测试以 16 个线程各跑 40 次：不加试运行时 1 次失败，为 `Text file busy`；加上后 0 次失败。次数少，后者只说明这 40 次没有复现。
- 未改动 Rust 代码，也未改动 `Cargo.toml`、`Cargo.lock`。
- 未验证：x86_64 镜像与其他集群；推送到镜像仓库；真实模型。

多集群实施记录（2026-11-01，PR #24）：

- 部署方式：
  - 常驻 Pod 与定时巡检挂载一份 kubeconfig（ConfigMap），各集群的 token 放在 Secret 中，kubeconfig 用 `tokenFile` 引用，本身不含密钥。`local` context 沿用 Pod 自己的账号。
  - `deploy/kubernetes/multicluster/` 是可选的 kustomize 组件。`remote.yaml` 在每个远端集群执行一次，创建只读账号（`view`）与长期 token。
- 代码：
  - 新增 `--secret-file`（可重复，默认取逗号分隔的 `ZX_SECRET_FILES`）。登记的路径在启动时必须存在，否则以退出码 2 报配置错误。
  - `RedactSecrets::with_secret_files` 接受目录，表示其中名字不以点开头的文件，跳过子目录与 Secret 卷的 `..data` 链接，之后新增的文件也会读入。
  - 修正旧值上限：原实现中当前值与旧值共用 32 个的上限，登记的文件超过 32 个时，部分当前值会不再过滤。现在当前值总是过滤，上限只约束轮换下来的旧值。
  - assistant 的自动放行清单加入 `kubectl config get-contexts` 与 `kubectl config current-context`。
- 新增 4 项测试：
  - `tests/hooks.rs` 2 项：目录中的可见文件与后来新增的文件被过滤，隐藏文件与子目录不受影响；40 个文件的当前值全部过滤。
  - `tests/hooks_cli.rs` 2 项：`ZX_SECRET_FILES` 以逗号登记目录与文件；路径不存在时为配置错误且不发请求。
  - 既有测试调整 1 项：`a_secret_file_that_cannot_be_read_as_text_fails_the_hook` 原先用目录充当“不能按文本读取的文件”。目录现在有了含义，改用非 UTF-8 内容的文件，断言不变。
- 反向验证：恢复旧的上限逻辑后，40 个文件的用例失败；修改已还原。
- 本地 macOS：基于 `e33f6fb` 时，stable 与 Rust 1.88 各 499 项通过（基线 495 项）；rebase 到含 PR #23 的 `23eabb3` 后，各 515 项通过（基线 511 项）。均有 6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`Cargo.toml`、`Cargo.lock` 未改动。
- 集群验证：只有一个本机集群，由它同时扮演远端，结果见 `deploy/kubernetes/README.md` 的“验证记录”。验证内容包括：
  - 身份切换；
  - 远端 token 脱敏；
  - chat 中直接执行 `--context` 读命令；
  - 不重启新增集群：新文件约 56 秒后出现；
  - 巡检访问远端；
  - 清理。远端清理需要 `--ignore-not-found`。
- 未验证：真正分属两个集群的访问；kubeconfig 中 exec 插件方式的凭据。

IM 接入模块 A 实施记录（2026-11-01，PR #28）：

- 内容：
  - 公开模块 `im`：平台接口 `ImChannel` 与评审稿 3.1 节的类型；`Server`、`ServerConfig`、`SessionFactory`（宿主为一个线程打开会话）、`ServerSink` 与 `ServerEvent`（审批记录、空闲释放、只有运维看得到的失败）、`Replies`（固定文本，默认中文）、`SessionKey::file_in`（会话文件路径，各段按字节编码）。
  - 结构：路由任务做访问控制并按线程分派；每个线程一个任务，独占会话、运行、审批卡片与空闲计时；任务之间只经 channel 通信。运行在独立任务中进行，所以发消息的等待不会拖住模型请求。运行上限用 tokio 的公平信号量。
  - 缺口 ③ 采用 a)：新增 `AgentSession::take_queued`，空闲时取出未被消费的排队输入，运行中返回 `Busy`；线程任务在每次运行结束后调用，非空就再运行一次。
  - 缺口 ④ 采用 a)：审批记录由 `ServerSink` 收到的 `ServerEvent::Approval` 给出，公开的 `Approval` 与事件格式不变。
  - 空闲释放先关闭会话、再请路由结束线程任务；路由只在这期间没有新消息时才结束它，否则由同一个任务重新打开会话，所以同一会话文件不会被两个任务打开。
- 相对评审稿的补充：`MessageRef` 定为“发往位置 + 平台消息 ID”；`Destination.thread` 是线程根消息的 ID；`ImChannel::run` 须在停止转交之后才返回。
- 新增 28 项测试：
  - `tests/im_server.rs` 16 项，覆盖：
    - 线程与会话：开线程、同线程续接、不同线程分开；
    - 访问控制：名单外的聊天；
    - 审批：批准、拒绝、非审批人点击、超时、取消；
    - 运行上限与排队、插话；
    - 空闲释放与重开；
    - 停止：取消运行并发出结果；
    - 出错：平台返回错误、会话打不开后重试、平台重名。
  - `tests/agent_session.rs` 2 项（`take_queued`）。
  - 模块内 10 项：卡片摘要、会话路径编码、结束文本。
- 反向验证：每次只破坏一处，再还原，对应测试都失败。被破坏的五处是：审批人检查、空闲释放、`take_queued`、卡片收尾、插话。
- 本地 macOS：基于 `75d0bc9`，stable 与 Rust 1.88.0 各 559 项通过（基线 531 项），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；`tests/im_server.rs` 连续运行 30 次均通过。`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。
- 未实现（模块 B–D）：`serve` 子命令、配置文件与 IM 凭据的隐藏和脱敏、任何真实平台。
- 未覆盖：运行结束瞬间到达的插话。会话层的 `take_queued` 有确定性测试；这一时序在服务层无法确定地构造。

IM 接入模块 B 实施记录（2026-11-01，PR #29）：

- 依赖：按决策 D7 引入 `tokio-tungstenite` 0.30（关掉默认 feature，只开 `connect` 与 `rustls-tls-webpki-roots`），`Cargo.lock` 新增 11 个包，与评审稿缺口 ① 的核对一致。`wss://` 握手用项目的依赖解析实测过：连接 Slack 的 WebSocket 地址时 TLS 握手成功，Slack 以 `400 Bad Request` 拒绝无效的 ticket。
- `im::slack::SlackChannel`：
  - 接收：启动时调用 `auth.test`，校验 bot token 并取得机器人自己的用户 ID；再经 `apps.connections.open` 连接 Socket Mode。每个信封先确认再处理，按（频道, ts）去重。只转交 `app_mention` 与私聊消息，去掉对机器人的 @，还原 `&amp;`、`&lt;`、`&gt;`；忽略机器人自己的消息和带 `subtype` 的消息（编辑、删除等）。
  - 重连：收到 `disconnect` 或连接断开时立即重连；连接失败按 1 秒起、最长 60 秒的退避重试；token 被拒等无法靠重试解决的错误使 `run` 以配置错误结束。
  - 发送：转义 `&`、`<`、`>`；超过 4000 字符的文本拆成多条，余下部分发在同一线程；同一聊天两次发送至少间隔 1 秒；429 按 `Retry-After` 等待后重试。
  - 卡片：两个按钮的 `value` 是审批 ID。关闭卡片时保留原请求，按钮换成结果与审批人；卡片文字默认中文（`CardLabels`）。
- `zx-agent serve --config <文件>`：
  - 配置文件为 TOML，含 `[slack]`、`[access]`、`[limits]`、`[sessions]`，未知键报错。
  - 启动检查：凭据变量必须非空；访问名单只能引用已配置的平台；创建会话目录；用内存会话准备一次，模型与 profile 的配置错误在启动时以退出码 2 报出。
  - 会话按 chat 的方式组装：assistant profile，bash 需审批，自动放行清单照常生效。IM 凭据变量从命令环境中删除，其值登记脱敏（`SessionOptions::hidden_env`）。
  - stdout 每行一个 JSON：审批记录、空闲释放与失败。SIGINT 或 SIGTERM 时各线程发出结果后以 0 退出。
  - chat 的 bash 审批规则提取为 `PreparedSession::require_bash_approval`，与 serve 共用。
- 新增 33 项测试：
  - `tests/im_slack.rs` 18 项（对 `tests/common/fake_slack.rs` 的假 Slack）；
  - `tests/serve_cli.rs` 2 项：配置错误 6 种；对假 Slack 与假模型的完整流程，见下一条；
  - 模块内 13 项：Slack 的转义、拆分、去重与卡片长度，serve 配置的解析，`serve` 的参数。
- 完整流程测试：@ 机器人，模型请求执行 `echo app=$SLACK_APP_TOKEN bot=$SLACK_BOT_TOKEN; cat token.txt`，审批人点击“批准一次”后：
  - 命令输出 `app= bot=`，两个变量不在命令环境中；
  - 从文件读出的 token 在下一次模型请求里是 `[REDACTED]`；
  - 卡片显示审批人，状态消息替换为回答，会话文件写在 `sessions/slack/C1/<线程>.jsonl`；
  - stdout 有审批记录；SIGTERM 后退出码为 0。
- 测试先于修复发现的问题：关闭卡片时审批人的 `<@U…>` 提及被一并转义，显示为文字而不是提及。已改为只转义标签文字。
- 按 Slack 官方文档核对后的取舍（原文见评审稿附录 9.1）：
  - `block_actions` 的字段说明里没有消息所在的线程，所以卡片按钮的 `value` 写成“线程/审批 ID”，点击时从中取线程；`value` 中没有线程时再读 `message` 或 `container` 的 `thread_ts`。完整流程测试中的点击不带线程字段。
  - 只替换文字时不传 `blocks`：文档写明此时 Slack 去掉原有的块。
  - Socket Mode 页称信封中的 `payload` 与 Events API 相同（带外层），而同页示例是事件本身，两种都接受。
  - 文档没有 `retry_attempt` 的说明，去重只依据（频道, ts），不依赖该字段。
  - 文档没有写 Socket Mode 的 ping；依据 Slack 的 SDK 推断服务端会发 ping，由 tungstenite 自动应答，未验证。
- 反向验证：每次只破坏一处并还原，对应测试都失败。破坏的六处：不确认信封、放过机器人自己的消息、放过重投、忽略按钮 `value` 中的线程、命令能看到 IM token、不对 IM token 脱敏。
- 本地 macOS（基于 PR #28 的 `1c39ce6`）：stable 与 Rust 1.88.0 各 592 项通过（基线 559 项），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/im_slack.rs` 连续运行 20 次，`tests/serve_cli.rs` 与 `tests/im_server.rs` 各 10 次，均通过。
- 未验证：真实的 Slack 工作区（需要测试应用的凭据）。

IM 接入模块 C 实施记录（2026-11-01，PR #32）：⚑ D:5/CHINA FLAG: 飞书。⚠ COMPLIANCE REVIEW: [对话内容、命令输出与用户 ID 发往该平台；出站长连接] Subject to Legal/Export Compliance approval。

- 依据：
  - 长连接没有公开文档，按官方 Go SDK（`larksuite/oapi-sdk-go`，`v3_main` 分支的 `db9f52d0`，`ws` 包）实现。
  - 消息、卡片与 token 按开放平台文档实现。
  - 原文与出处见评审稿附录 9.2。
- `im::feishu::FeishuChannel`：
  - 长连接：
    - 经 `/callback/ws/endpoint` 取得地址与连接设置，地址中带 ticket，不显示。
    - 帧为手写的 protobuf 编解码（`pbbp2.Frame`，proto2，字段 1–9 都写出）。
    - 心跳：连上即发 ping，此后按服务端给的间隔；pong 带来新间隔时从上次 ping 起重新计时；静默超过两倍间隔加 5 秒即重连。
    - 分片：按 `message_id` 收齐后拼接，只确认补齐的那一片。
    - 确认：回写原帧并加 `biz_rt`。与 SDK 不同，收到即确认，不等处理完：文档要求 3 秒内确认。
    - 去重：按 `message_id`。
    - 错误：地址请求返回不可重试的错误码，或握手被拒（403，或连接数超限）时，以配置错误结束；其他失败按服务端给的间隔或 1–60 秒退避重连。
  - 收：
    - 只转交人发的文本：私聊；群里只转交 @ 了机器人的消息。
    - 机器人的 @ 去掉，其他 @ 写作 `@名字`；回复以 `root_id` 为线程。
    - 卡片按钮的点击以 `card.action.trigger` 事件到达，按钮的回传值中带审批 ID、聊天与线程。
  - 发：
    - token 用文档推荐的 `accounts.feishu.cn/oauth/v3/token`，到期前 5 分钟更新；密钥被拒为配置错误。
    - 有线程时以话题回复，私聊中不开话题；没有线程时发新消息。
    - 状态消息用编辑接口改为回答；编辑被拒（一条消息最多编辑 20 次，可编辑时长由管理员设定）时，改为发一条回复。
    - 卡片为 JSON 2.0、`update_multi`，用 PATCH 关闭，关闭时保留原请求。
    - 限频：同一聊天两次发送间隔 200 毫秒（群内机器人共享每秒 5 条）；429、`99991400`、`230020` 等待后重试。
- `serve` 增加 `[feishu]` 段（`appIdEnv`、`appSecretEnv`，以及代理或测试用的 `apiBase`、`tokenUrl`）；App Secret 所在的变量从命令环境中删除并登记脱敏。
- 共用部分：`CardLabels`、文本拆分与去重集合移到 `im` 模块，两个平台共用。`tests/common/http.rs` 供两个假服务读写 HTTP。
- 新增 33 项测试：
  - `tests/im_feishu.rs` 19 项，对 `tests/common/fake_feishu.rs` 的假开放平台；假服务的帧编解码是独立写的，不复用被测代码。
  - 模块内 14 项：帧编解码 5 项；分片、确认帧、提及、`service_id`、连接设置、抖动 8 项；serve 的 `[feishu]` 默认值 1 项。
  - `tests/serve_cli.rs` 的配置错误中增加缺少 App Secret 的情形，测试项数不变。
- 本地 macOS（基于 `ba10d35`，含 PR #29、#30）：stable 与 Rust 1.88.0 各 634 项通过（基线 601 项），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/im_feishu.rs` 连续运行 20 次，`tests/im_slack.rs`、`tests/serve_cli.rs` 与 `tests/im_server.rs` 各 5 次，均通过。
- 测试先于修复发现的问题：token 请求被拒时的错误在 API 调用中被当作临时错误，已改为保持配置错误。
- 反向验证：每次只破坏一处并还原，对应测试都失败。破坏的八处：不确认、放过重投、交出不完整的分片、放过没有 @ 机器人的群消息、pong 不重新计时、编辑被拒后丢失回答、私聊开话题、密钥被拒被当作临时错误。
- 未验证：
  - 真实的飞书租户。
  - `{"code":200,"headers":null,"data":null}` 作为卡片回调的确认能否被接受：文档写的是返回 `{}`，Go SDK 发的是 `null` 的 base64，Python SDK 只发 `{"code":200}`。
  - 私聊中的话题回复是否可用：现在私聊不开话题。

IM 接入模块 D 实施记录（2026-11-01，PR #33）：

- 部署：`deploy/kubernetes/serve/` 是可选的 kustomize 组件，与 `multicluster` 并列。基础 Deployment 不变，仍是 `tini -- sleep infinity`，只用 `kubectl exec` 的部署不需要 IM 配置。组件包括：
  - `shareProcessNamespace: true`，主进程改为 `zx-agent serve --config /etc/zx-agent-serve/serve.toml`；
  - 配置来自 ConfigMap `zx-agent-serve`，挂在 `/etc/zx-agent-serve`，因为 `/etc/zx-agent` 已由多集群组件使用；
  - 凭据来自 Secret `zx-agent-im`，四个键都是可选的，只写用到的平台；
  - `serve.example.toml` 是配置示例。
- 可观察性：验收前发现，平台连不上时 serve 静默重试，日志中没有任何输出。
  - 为两个平台的配置增加 `on_error`（类型 `im::ErrorReport`），每次连接失败都调用；serve 把它写成 `failed` 行。
  - 新增 2 项测试，Slack 与 ⚑ D:5/CHINA FLAG: 飞书各 1 项：一次可重试的失败被上报，下一次连接成功。反向验证：两处上报分别改为不调用时，对应测试都失败，修改已还原。
- 本机集群验收：Docker Desktop 的 Kubernetes v1.36.1。没有真实平台凭据，Slack 指向拒绝连接的地址。结果：
  - PID 1 为 pause，serve 为普通进程。
  - `/proc/1/environ` 中没有 API key，读取 serve 的环境被拒绝。
  - 孤儿进程被回收。
  - serve 运行期间，`kubectl exec` 启动的 chat 正常。
  - 日志有启动行与 `failed` 行，不含凭据。
  - SIGTERM 后以 0 退出；`rollout restart` 正常。
  - 两个组件同时启用时，挂载不冲突。
  - 详见 `deploy/kubernetes/README.md` 的“验证记录”。
- 文档：
  - 部署说明新增“IM 服务”一节：创建 Secret 与 ConfigMap、启用组件、重启、日志、出网。
  - 安全说明中“常驻 Pod 中的 API key”一条，改为启用 serve 后不再存在。
  - 根目录 README 指向部署说明。
- 本地 macOS（基于含 PR #31、#32 的 `0eb4b96`）：stable 与 Rust 1.88.0 各 651 项通过（基线 649 项），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；⚑ D:5/CHINA FLAG: `tests/im_feishu.rs` 连续运行 20 次，`tests/im_slack.rs`、`tests/serve_cli.rs` 与 `tests/im_server.rs` 各 5 次，均通过。集群验收所用镜像基于 `c272f41` 加本次改动构建，合入 PR #31 之后没有重建镜像复验。
- 未验证：真实的 Slack 工作区与 ⚑ D:5/CHINA FLAG: 飞书租户；两种 WebSocket 主机，需要真实连接；x86_64 镜像。

执行环境接缝验收（2026-11-01，已完成，PR #13）：`src/executor/` 提供 `Executor`、`ShellCommand`、`CommandOutput`、`CommandEnd` 与 `LocalExecutor`；bash 与 read_file 新增 `executor` 字段，默认 `LocalExecutor`，`..Default::default()` 的构造方式不受影响，按字段完整列出的字面量需补上新字段。本机执行代码从 bash 与 read_file 原样搬迁，bash 与 read_file 原有测试不改断言即通过（仅 read_file 测试中两处字面量补 `..ReadFileTool::default()`，bash 测试模块补一行 `use tokio::process::Command;`）。新增 `tests/executor.rs` 3 项，用记录请求的替身执行器验证：bash 把命令、目录、超时与隐藏变量交给执行器，并正确报告 6 种结束方式；read_file 经执行器读取解析后的路径与上限。反向验证：让工具绕过自身执行器时，3 项全部失败，修改已还原。本地 macOS 基于 `bb91974`，stable 与 Rust 1.88.0 各 416 项通过、6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动。

离线容器执行验收（2026-11-01，已完成，PR #14；在线访问未开始）：

- 新增测试：模块内 6 项（`docker run` 参数与校验、启动探测输出的解析、容器的 `<environment>`、profile 的容器设置）；`tests/container.rs` 在 macOS 上 24 项、在 Linux 上 25 项。其中 14 项用假 docker 程序，始终运行：启动参数与失败报告、命令与读文件的转发、超时 / 取消 / 丢弃后的清理、CLI 接线与配置错误。其余以 `real_` 开头的测试只在设置 `ZX_DOCKER_TEST_IMAGE` 时运行：用户与只读工作目录、无网络、整个进程组的终止、后台进程清理、隐藏会话文件、容器内读文件、chat 中 Ctrl-C 后容器仍在、zx-agent 正常结束或被 SIGKILL 后容器删除、不可读工作目录在启动时报错（后者只在 Linux 上编译）。
- 本地真实 Docker：macOS Docker Desktop（Docker 29.9.2）上分别用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像运行，全部通过，未跳过；测试后没有遗留容器或 docker 客户端进程。CI 在 Linux 任务中拉取 `debian:bookworm-slim` 后运行同一文件，以 CI 结果为准；workflow 的改动未在本地用 actionlint 检查，由 CI 的检查步骤校验。
- 反向验证：容器内只终止进程组组长、不终止整个组时，整棵命令树与后台进程两项真实测试失败；去掉 docker 客户端的独立进程组时，Ctrl-C 测试仍通过，说明它验证的是“Ctrl-C 不删除容器”这一行为，不能证明独立进程组的必要性（见上方实现细节）。修改均已还原。
- 修正一项测试自身的问题：读文件测试曾按 `read_dir` 的顺序取第一次调用，而该顺序未定义，约半数运行失败；改为与顺序无关的断言后连续 10 次通过。
- PR #14 第一次 CI 在 Linux 上发现另一项测试问题：Ctrl-C 测试用外部 `kill -INT -<进程组>` 发信号且未检查结果，而 procps-ng 4.0.4 的 `kill` 不接受这种写法（在 Ubuntu 系镜像中实测返回 1），信号没有发出，测试等待 60 秒后失败；其余 24 项在 Linux 上通过。PR #14 合并时尚未包含修复，合并后 `main`（`20b9809`）的 CI 在同一项失败。PR #15 改用 shell 内置的 `kill` 并断言发送成功。
- 原有测试的调整：profile 测试中表示“不支持的后端”的取值由 `container`（现已支持）改为 `kubernetes`，仍验证未知后端报错。
- 本地 macOS 基于 `f45fd98`：stable 与 Rust 1.88.0 各 446 项通过、6 项付费测试跳过（未设置 `ZX_DOCKER_TEST_IMAGE`，真实 Docker 测试只打印提示）；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

未验证：原生 Linux 的 Docker 只由 CI 覆盖，本地没有运行；rootless Docker 与 Podman；Windows；`docker exec` 在后台进程占用输出时约 2 秒返回，以及 `docker run` 客户端在 SIGINT 后继续运行，都只是单次观测。

崩溃恢复验收（2026-11-01，已完成，PR #12；执行环境部分未开始）：

- 新增 22 项测试：会话层 10 项（条目格式、引用校验、调用 ID 复用、旧会话升级到 v4、打开时的格式检查、中断调用判定）；运行层 2 项（开始记录写入失败时不执行、恢复结果写入失败时不发请求）；事件 1 项（`tool_call_recovered` 的 JSON 与四种 `state`）；`tests/crash_recovery.rs` 8 项（内置工具声明、串行与并行批次的开始记录时序、四种恢复结果且不重放、旧格式会话、丢弃 future 后恢复、`run` 的输出，以及 Unix 上 SIGKILL 掉正在执行命令的 `run` 后由下一次 `run` 恢复）；`tests/chat_cli.rs` 1 项。
- 反向验证（整套测试）：不写开始记录时，4 项集成测试和 1 项单元测试失败；去掉恢复步骤时，6 项集成测试和 1 项单元测试失败；其余测试不受影响。两次修改都已还原。
- 原有测试中 2 项按 v4 更新预期：新会话版本号由 3 改为 4；拒绝“未来版本升级”的用例由 4 改为 `SESSION_VERSION + 1`。
- 本地 macOS（基于 `4ccb1bb`，含 PR #11）：stable 与 Rust 1.88.0 各 **413 项通过，6 项付费测试跳过**；fmt、clippy、严格 Rustdoc、MSRV 全目标检查均通过。反向验证的结果在 rebase 前后各测一次，相同。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

未验证：Windows 上的进程终止场景；断电等导致已写入的记录未落到磁盘的情况（会话写入只调用 `flush`，不调用 `fsync`）。

验收：离线任务在无网络的容器中执行命令；在线任务按已评审的网络与凭据范围访问目标 API；崩溃恢复测试。D5 若改变 `run` 的默认执行环境，补充迁移说明和相应 CLI 测试。

### P7 前置：扩展 API 与追踪事件（2026-11-01，已完成：A1 PR #21，B PR #23，A2–A8 PR #25）

背景：P7 将公开 `Extension` trait（技术方案 5.5 节：`name`、`tools`、`prompt_sections`、`hooks`）；技术方案把长期记忆规划为"`transform_context` + 外部存储"的扩展；P9 计划 OpenTelemetry 追踪。公开 trait 有了外部实现后，再改签名就是不兼容变更，所以先定稿下列接口。下表是最初的草案；各项已定稿并实施，见后面三个小节。

A. 扩展 API

| # | 现状 | 草案 |
|---|---|---|
| A1 | `EventSink` 既接收事件，又承担会话宿主职责：`edit_context`、`record_request`、`record_tool_call_started`、`prepare_context`（`src/agent/events.rs`）。后四个方法只在直接调用 `run_loop` 时生效；经 `Runner::new` 或 `AgentSession::subscribe` 传入的 sink 不会被调用（`PersistingSink` 不转发，`EventHub` 只实现 `emit`），2026-11-01 已用实验确认 | `EventSink` 只保留 `emit`；会话宿主协议拆成单独的接口，由 Runner 实现，不暴露给订阅者。持久化失败统一以 `Result` 返回，不再经"记录失败并取消令牌"的旁路 |
| A2 | 钩子方法没有统一的上下文参数（只有 `finish_turn` 的 `TurnSummary` 带轮次），拿不到 session ID、run ID、工作目录与取消令牌；`Tool::execute` 已有 `ToolContext` | 每个钩子方法增加 `&HookContext`，字段只增不删 |
| A3 | 钩子名取自 `std::any::type_name::<H>()`（`src/agent/hooks.rs`）。方案中 `Extension::hooks()` 返回 `Arc<dyn Hooks>`，按此接入后所有扩展钩子会同名，`hook_failed` 与追踪中无法区分 | 注册时给出名称，默认仍为类型名；扩展注册的钩子使用 `Extension::name()` |
| A4 | 安全相关钩子的先后靠注册顺序手工维持：`src/main.rs` 与 `src/chat/mod.rs` 各自按"策略、续写、脱敏、落盘"注册 | 改为固定阶段：`before_tool` 先执行内置策略；`after_tool` 中扩展钩子之后依次是脱敏、落盘。扩展只能注册到中间段 |
| A5 | 工具重名不检测，`find_tool` 取第一个（`src/agent/agent_loop.rs`） | 组装时拒绝重名；MCP 工具名加服务器前缀，分隔符在 P7 实施时按 provider 的函数名约束确定 |
| A6 | run 与 chat 各自组装运行环境 | 抽出唯一的组装函数；扩展的工具、提示词段落与钩子在此注册一次，P8 的 `acp` 复用 |
| A7 | 工具放行有两套 API：`ToolPolicy`（经 `PolicyHook` 适配）与 `Hooks::before_tool`；`agent::policy` 还再导出外层的 `CommandPolicy` | 扩展统一使用 `before_tool`；`ToolPolicy` 保留为内置策略的适配；移除内核模块中的兼容再导出 |
| A8 | 钩子的输入输出类型没有派生 serde：`Decision`、`ToolOutput`、`ContextEdit`、`TurnDecision`、`ApprovalRequest` | 定义稳定的 JSON 契约，供技术方案中的钩子进程（L2）与 P8 的审批转发使用 |

B. 追踪事件

| # | 现状 | 草案 |
|---|---|---|
| B1 | 没有运行级事件：`RunReport` 只作为返回值；每次重试各发一对 `AgentStart`/`AgentEnd`；中断调用的恢复事件发生在 `AgentStart` 之前 | 增加 `RunStart`、`RunEnd`（带报告） |
| B2 | 请求发出时没有事件，`request_started` 只写入会话；`MessageStart` 在收到成功的响应头之后才发（`src/ai/openai.rs`），HTTP 失败的请求没有起点 | 增加 `RequestStart`（请求条目 ID、模型、第几次尝试） |
| B3 | `ToolExecutionStart` 早于参数校验、策略与审批，`duration_ms` 只含执行；策略结论与审批不发事件 | 增加 `ToolDecision`（allow、deny、ask 与理由）和 `ApprovalRequested`、`ApprovalResolved`（含等待时长） |
| B4 | 自动摘要请求表现为没有回答的回合（只有 `TurnStart`、`TurnEnd`） | 摘要单独成段，不借用回合事件；预算计数不变 |
| B5 | 事件不带时间、序号、session ID 与 run ID；`MessageEnd` 不带会话条目 ID | 统一信封 `{seq, atMs, sessionId, runId, event}`；`MessageEnd` 带条目 ID，可回链审计日志 |
| B6 | `emit` 同步内联执行；`on_event` 对每个事件、每个钩子 await，默认超时 5 秒 | 为导出器提供有界通道订阅（满时丢弃并计数）；`on_event` 可声明关注的事件类型，跳过流式增量 |
| B7 | 事件含原始工具参数、用户输入与模型输出；脱敏只覆盖工具输出 | 导出默认只含元数据，内容须显式开启 |
| B8 | JSONL 事件没有版本号 | 信封带 `schemaVersion` |

依赖：B 部分只改事件模型，不需要新依赖；P9 的 OTel 导出器所需依赖不在 D2 授权范围内，届时另行征得同意。会话文件已有时间戳与 `duration_ms`，也可以不依赖 OTel，由会话文件与 JSONL 离线还原时间线。

已决定（2026-11-01）：
- A1：会话宿主接口留在 crate 内部；可替换的是更底层的会话存储，定稿见下一节。
- B7：追踪要能导出完整的调用细节（导出方式与默认范围随 OTel 导出器另行设计）。
- 公开 API 与 JSONL 事件格式可以改变：目前没有外部使用者，按最佳实践设计，不为兼容保留旧接口。

#### A1 定稿：会话宿主拆分与可替换的会话存储（2026-11-01，已完成，PR #21）

目标：会话可以保存到文件以外的地方，例如数据库；所有会话不变量仍由内核保证。同时修复 Codex 评审中的两项：钩子未完成时取消的输入会进入投影；同一会话文件可被多个实例同时写入。

可替换的边界放在最底层、最窄的一层，即"一个会话的有序记录如何保存与读出"；校验、先写日志再改内存、写失败后停止写入、投影、压缩、请求审计与崩溃恢复都留在 `SessionStore`。

1. **`SessionLog`（公开 trait）**：`append(&mut self, record: &LogRecord) -> Result<()>`，记录落盘（提交）后才返回。`LogRecord` 含 `seq`（会话内从 1 开始的序号）、条目 ID、父条目 ID、条目类型、时间戳与 `json`（条目的规范 JSON，与 JSONL 文件中的一行逐字节相同），其余字段便于数据库建索引。实现方必须在别的写入者已占用同一 `seq` 时报错而不是覆盖，数据库可用 `(session_id, seq)` 唯一约束实现。打开会话由各后端自己的构造函数负责，返回 `LoadedSession`（头部、按序的记录、修复掉的尾部字节数），再交给 `SessionStore::load` 校验与重建。
2. **JSONL 文件后端 `JsonlFile`**：现有行为（0600 权限、修复写到一半的尾记录、每条记录 flush）加单写者约束：打开时对文件加非阻塞的独占 `flock`，已被其他进程持有时报错；锁随进程退出释放。
3. **写入改为异步**：`SessionStore` 的写方法改为 `async`，读方法不变。每次追加前先置"写入进行中"，成功后清除：future 在等待写入时被丢弃，结果未知，会话随即停止写入，以存储中的实际内容为准重新打开。每次追加有超时（30 秒），超时同样按结果未知处理。
4. **会话宿主拆分（A1）**：`EventSink` 只保留 `emit`。持久化消息、上下文编辑、请求审计、工具开始记录与请求前的自动压缩，改由 crate 内部的会话宿主接口提供，由 Runner 实现；写入失败以 `Result` 返回，循环立即停止（不变量 7），不再经"记录失败并取消令牌"的旁路。Runner 直接持有 `SessionStore`，不再包一层 `Mutex`；`Runner::session()` 返回 `&SessionStore`。公开的 `run_loop` 使用只在内存中的宿主。
5. **输入先过钩子再写入**：`on_input` 完成后才写入输入：被改写时依次追加原始输入与替换它的 `context_edit`，被拒绝时追加原始输入与省略它的 `context_edit`（审计不变），钩子被取消或运行被丢弃时什么也不写；从队列取出的插话或后续消息此时放回队首。局限：两次追加之间进程崩溃，原始输入会出现在投影中；彻底消除需要新的条目类型，暂不做。
6. **不变**：会话文件格式与版本、事件格式、CLI 参数；`tools::read_output` 的长结果文件仍保存在本机。

验收：自定义的内存日志后端能完整往返（创建、运行、按记录重新加载后投影一致）；第二个进程打开同一会话文件时报错；写入被丢弃或超时后会话停止写入；钩子被取消或运行被丢弃时，原始输入不出现在后续请求中，取出的插话放回队列。

实施记录（PR #21，基于 `3af5ef1`，其后合并 `dd43cce`，含 PR #18–#20）：

- 按上述 1–6 实施，与定稿的差异如下：
  - JSONL 后端留在 crate 内部（`src/session/file.rs`），文件会话仍经 `SessionStore::create` / `open` 使用。公开的是 `SessionLog`、`LogRecord`、`LoadedSession` 与 `SessionStore::with_log` / `load`。
  - 新增 `SessionStore::read`：不加锁、不修改文件的只读快照，对快照的任何追加都会失败。加锁之后，测试和其他读者需要在写入者运行时查看会话文件。
  - 打开文件时先校验全部记录，再截掉未完成的尾记录，与此前一致；"已读取、未修复"是单独的类型，修复之前无法追加。
  - `flock` 返回"已被占用"以外的错误（例如文件系统不支持）时，打开失败，不降级为无锁写入；Unix 以外的平台不加锁。
  - 锁已被占用时，打开最多重试 0.5 秒（间隔从 1 毫秒起逐次加倍，单次不超过 50 毫秒）再报错。原因：本进程正在启动的子进程在运行自己的程序之前持有父进程文件描述符的副本，刚关闭的会话因此可能短暂保持锁定。PR #21 的首次 CI 在 macOS 上因此失败（`tests/crash_recovery.rs` 关闭会话后立即重新打开，同一测试进程中的其他测试正在启动子进程）。本机实验（各 20000 次"加锁、关闭、立即重新加锁"）：不启动子进程时失败 0 次；另一线程不断启动子进程时失败 21–39 次，强制 fork 再 exec 时 18–232 次，重试到成功的等待最长 0.52 毫秒。修复前新增的回归测试三次运行各有 234–317 次（共 3000 次）重新打开失败，修复后通过。
  - 会话写不进去时，运行以 `Error` 结束，即使同时被取消；自动压缩写入失败时，已发生的请求和用量照常计入（`ContextPreparation::Unrecorded`）。
  - 用户输入的 `message_start` / `message_end` 事件改在 `on_input` 完成之后发出，此前在钩子之前。事件的 JSON 格式不变。
- 公开 API 的变化：`SessionStore` 的写方法改为 `async`；`Runner::session()` 返回 `&SessionStore`；`EventSink` 只剩 `emit`；`ContextPreparation` 不再公开；`LoopFailure::Provider` 改为 `{ message, entry_id }`；新增 `Entry::kind` / `timestamp`。会话文件格式（v4）与 CLI 参数不变，`run --help`、`chat --help` 与 `dd43cce` 逐字节相同。
- `tests/cli.rs` 改动一行：准备数据时的 `append_message(...)` 加 `.await`，这是写方法改为异步的直接结果，断言未改；该文件自 `d2a6a6c` 以来首次改动。其他既有测试的调整：写方法加 `.await`；写入者仍在运行时读取会话文件的 9 处改用 `SessionStore::read`；`src/session.rs` 中 2 项测试在重新打开文件前先释放写入者。
- 本机 macOS 验证：新增 15 项测试（含 1 个 Rustdoc 示例）；合并 `dd43cce` 后，stable 与 Rust 1.88.0 各 495 项通过、6 项付费测试跳过。其中 4 项回归测试在修复前失败：在 `e350c8f` 上，取消输入钩子后原始输入仍在会话中（2 项），第二个 `run` 写入同一会话文件并以 0 退出（1 项）；在加入重试之前的本分支上，关闭会话后立即重新打开报 `is in use`（1 项）。另用独立进程（Python `fcntl.flock`）持锁实测：`run` 与 `chat` 均以退出码 2 结束并报告 `is in use`，文件不变；释放后 `chat` 正常打开。未新增依赖。完整检查结果与原始测试摘要附在 PR 中。

#### B 定稿：追踪事件（2026-11-01，已完成，PR #23）

目标：只看事件就能还原一次运行的时间线——运行、每次尝试、回合、每个模型请求、工具的策略结论与审批、摘要——并能逐条对应到会话中的记录，为 P9 的 OTel 导出器备好数据。本阶段不引入导出器，也不新增依赖。

1. **统一信封（B5、B8）**：订阅者收到 `agent::Event`，即事件本身加上 `schemaVersion`、`seq`、`atMs`、`sessionId`、`runId`。JSON 中信封字段与事件字段同层，`type` 仍在顶层。`schemaVersion` 从 1 开始，信封或事件出现不兼容变化时加一。`seq` 在一次运行内从 1 开始连续递增；`runId` 是每次运行新生成的 UUID；`sessionId` 是会话头部的 ID，直接调用 `run_loop` 时没有。手动压缩不属于任何运行，使用自己的 `runId`。`EventSink::emit` 与 `Hooks::on_event` 改为接收 `&Event`。
2. **运行级事件（B1）**：`Runner::run` 以 `run_start`（模型、provider）开始，以 `run_end`（完整的 `RunReport`）结束，中断调用的恢复事件位于两者之间。每次尝试仍由 `agent_start` / `agent_end` 包围，`agent_start` 增加 `attempt`（从 1 开始，每次重试加一）。`run` 与 `chat` 的 `--output jsonl` 不再单独打印 `run_end` 行，改为输出该事件，行内的 `type` 与 `report` 不变。`RunReport`、`RunOutcome` 移到 agent 层，`runner` 再导出原路径。
3. **请求起点（B2）**：每个模型请求在调用 provider 之前发 `request_start`：`kind`（`task` 或 `summary`）、`entryId`（会话中对应的 `request_started` 条目，没有会话时为空）、`model`、`attempt`。HTTP 失败的请求因此也有起点。
4. **策略结论与审批（B3）**：工具策略得出结论后发 `tool_decision`：`allow`、`deny` 或 `ask` 及理由；策略钩子出错按 `deny` 报告。需要询问时发 `approval_requested`，得到结果后发 `approval_resolved`，结果为 `allow_once`、`allow_session`、`denied`、`timed_out`、`cancelled`、`failed` 之一，附理由与等待毫秒数。会话内已授权过的同一调用不再询问，只发 `approval_resolved`（`remembered`，等待 0）。工具不存在、参数无效等不经过策略的情况不发 `tool_decision`，仍体现在工具结果中。
5. **摘要单独成段（B4）**：自动摘要请求不再发 `turn_start` / `turn_end`，由 `compaction_start`、`request_start`（`summary`）与 `compaction_end` 或 `compaction_failed` 表示。`turn` 只给任务回合编号，回合事件成对出现；摘要发生在回合内时，位于该回合的任务请求之前。轮次预算仍按模型请求计数（含摘要请求），`RunReport.turns` 的含义不变。回合是为了发出请求：预算已用完或运行已取消时，不再开启新回合，排队的插话与后续消息留给下一次运行。
6. **回链审计记录（B5）**：`message_end` 带消息条目的 `entryId`，`context_edited` 带编辑条目的 `entryId`；没有会话时为空。
7. **订阅与观察（B6）**：`AgentSession::subscribe_bounded(capacity)` 返回有界通道：通道已满时丢弃新事件并计数（`dropped()`），不阻塞运行，`seq` 的空缺同样反映丢失。`Hooks` 新增 `observes(&AgentEvent) -> bool`，默认不观察流式增量 `message_update`，需要时覆盖；`on_event` 只收到观察的事件。
8. **内容范围（B7，已决定）**：事件保留完整内容：用户输入、工具参数、经 `after_tool` 处理后的工具输出与模型输出。导出时的默认范围随 OTel 导出器另行设计。
9. **不变**：会话文件格式与版本、CLI 参数、文本模式的输出。

兼容性：`EventSink`、`Hooks::on_event` 的签名与 JSONL 事件行的格式改变，在已授权的范围内。JSONL 的第一行从 `agent_start` 变为 `run_start`。

验收：一次运行中 `seq` 从 1 连续递增，所有事件的 `runId`、`sessionId` 相同；`run_start` 最先、`run_end` 最后，`run_end` 的报告与返回值一致；每个模型请求之前恰有一条 `request_start`，其 `entryId` 与会话中的 `request_started` 一致，重试时 `attempt` 加一；策略的三种结论、会话内授权、审批超时与取消各有对应事件；自动摘要期间没有回合事件，回合编号连续，预算计数不变；`message_end`、`context_edited` 的 `entryId` 指向会话中的对应条目；有界订阅在通道满时丢弃并计数且不阻塞运行；钩子默认不观察增量。

实施记录（PR #23，基于 `e33f6fb`）：

- 按上述 1–9 实施，与定稿的差异与补充如下：
  - `LoopResult` 新增 `requests`（模型请求数，含摘要请求，预算按它计算），`turns` 改为任务回合数；`LoopControl::turn_offset` 改为此前各次尝试的任务回合数。`RunReport.turns` 仍是模型请求数。
  - 预算用完或已取消时不开空回合（第 5 项末句）。此前循环会先开启新回合、把排队的输入写入会话，再以 `MaxTurns` 或 `Aborted` 结束。
  - `seq` 的空缺也可能来自订阅者自己的过滤，例如 `--output jsonl` 未加 `--deltas` 时省略的增量；`Event::seq` 的文档与 README 已写明。
  - 订阅接口为 `AgentSession::subscribe_bounded(NonZeroUsize) -> EventStream`，提供 `recv`、`try_recv` 与 `dropped`。
- 公开 API 的变化：`EventSink::emit` 与 `Hooks::on_event` 改为接收 `&Event`，新增 `Hooks::observes`；`AgentEvent` 新增 `RunStart`、`RunEnd`、`RequestStart`、`ToolDecision`、`ApprovalRequested`、`ApprovalResolved`，`AgentStart` 带 `attempt`，`MessageEnd`、`ContextEdited` 带 `entry_id`；`RunReport`、`RunOutcome` 移到 `agent`，`runner` 再导出；新增 `Event`、`EVENT_SCHEMA_VERSION`、`RequestKind`、`ToolVerdict`、`ApprovalResult` 与 `agent_session::EventStream`。
- 既有测试的调整：`tests/cli.rs` 中 JSONL 第一行的期望由 `agent_start` 改为 `run_start`（该文件第二次改动）；`tests/compaction.rs` 中手动压缩的事件序列多出 `request_start`，自动摘要的回合编号由两个回合 `[1, 2]` 变为一个回合的 `turn_start` / `turn_end`；测试中的 sink 与观察钩子改为接收 `&Event`，需要观察全部事件的钩子覆盖 `observes`。
- 本机 macOS 验证：新增 16 项测试；stable 与 Rust 1.88.0 各 511 项通过（基线 495 项）、6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过。"预算用完不开空回合"的测试在改动前失败（任务回合数为 2，期望 1）。`run --help`、`chat --help` 不变，未新增依赖。

#### A2–A8 定稿：扩展 API（2026-11-01，已完成，PR #25）

目标：P7 公开 `Extension` 之前，把扩展会用到的钩子接口、注册方式、执行顺序与数据格式定下来；之后只做兼容的增改。

1. **钩子上下文（A2）**：每个钩子方法最后增加一个参数 `&HookContext`，含 `session_id`、`run_id`（与事件信封的 `runId` 相同）、`cwd` 与运行的 `cancel` 令牌。结构标为 `#[non_exhaustive]`，字段只增不删；库外用 `HookContext::new` 构造。运行上下文只有一份：同时给事件盖信封、给钩子提供上下文。`HookChain` 的公开方法改为接收 `&HookContext`，不再单独接收取消令牌。
2. **钩子名称（A3）**：注册时可以给出名称，用于 `hook_failed` 与追踪；不给时仍用类型名。注册参数收拢为 `HookOptions { name, timeout }`（`Default` 加结构体更新语法），`push_with(hook, options)` 取代 `push_with_timeout`。P7 中扩展注册的钩子以 `Extension::name()` 命名。
3. **固定阶段（A4）**：钩子分为四个阶段，每个钩子方法都按 `Policy`（内置工具策略）、`Extension`（默认）、`Redaction`（脱敏）、`Storage`（长结果落盘）的顺序执行，同一阶段内按注册顺序。阶段由钩子自己声明（`Hooks::stage()`，默认 `Extension`）：`PolicyHook`、`RedactSecrets`、`SpillOutputs` 各自声明所属阶段，不再依赖宿主的注册顺序。P7 的 `Extension` 接口只允许 `Extension` 阶段。
4. **工具重名（A5）**：`LoopSpec::validate` 拒绝重名的工具；`run_loop` 开始时先检查，重名则不发请求、以 `Error` 结束；CLI 在启动时检查，按配置错误退出。MCP 工具名的前缀与分隔符随 P7 确定。
5. **唯一组装入口（A6）**：`run` 与 `chat` 共用的钩子（续写、脱敏、长结果落盘）在 `cli::prepare` 中注册，Runner 与 `AgentSession` 也由同一个函数构造；两种模式只各自提供事件输出、交互宿主与 chat 的审批规则。P7 的扩展与 P8 的 `acp` 都在这里接入。
6. **放行接口（A7）**：扩展统一使用 `Hooks::before_tool`；`ToolPolicy` 保留为内置策略经 `PolicyHook` 接入的方式。内核模块 `agent` 不再再导出外层的 `CommandPolicy`，改从 `policy` 引用。
7. **JSON 契约（A8）**：钩子的输入输出类型派生 serde，格式由测试固定：`Decision` 为 `{"decision":"allow"}`、`{"decision":"deny","reason":…}`；`TurnDecision` 为 `{"decision":"continue_answer","text":…}` 等；`ContextEdit` 为 `{"edit":"append","message":…}`、`{"edit":"replace_text","index":…,"text":…}`、`{"edit":"omit","index":…}`（`Append` 因此改为带字段的形式）；`Approval` 为 `{"approval":"deny","reason":…}` 等；`ToolOutput`、`ApprovalRequest` 为 camelCase 对象，`ToolOutput` 的 `content` 必填，`isError`、`terminate` 缺省为 `false`。`HookContext`（不含取消令牌）、`TurnSummary` 可序列化，供钩子进程（L2）与 P8 的审批转发使用；协议本身随 L2 设计。
8. **不变**：钩子的语义（失败、超时、取消的处理与最严结论）、事件格式、会话文件格式、CLI 参数。

验收：每个钩子方法都能读到会话 ID、运行 ID、工作目录与取消令牌，且运行 ID 与事件一致；具名钩子的名字出现在 `hook_failed` 中；无论注册顺序如何，`after_tool` 中扩展先于脱敏、脱敏先于落盘，`before_tool` 中内置策略最先；重名工具在发请求前被拒绝；`run` 与 `chat` 注册的公共钩子相同；`agent` 不再导出 `CommandPolicy`；每个契约类型的 JSON 与上文一致并能往返。

实施记录（PR #25，基于 `090dbec`，含 PR #24）：

- 按上述 1–8 实施，与定稿的差异与补充如下：
  - `HookContext` 只派生 `Clone` 与 `Serialize`，钩子进程只需读取它。直接调用 `run_loop` 时 `sessionId` 为空，`runId` 为新生成的 UUID。钩子拿到的取消令牌与此前 `HookChain` 收到的相同：`Runner::run` 中是本次运行的令牌，手动压缩中是压缩自己的子令牌。
  - `HookOptions` 未标 `#[non_exhaustive]`，库外才能用结构体更新语法构造。`HookChain::push` 与 `with` 不变，仍以类型名注册。
  - `on_event` 同样按阶段、再按注册顺序调用观察钩子。
  - 阶段由 `Hooks::stage` 声明，SDK 中实现 `Hooks` 的类型目前可以声明任意阶段；"扩展只能使用 `Extension` 阶段"随 P7 的 `Extension` 接口实施。
  - chat 的 bash 审批规则在 `cli::prepare` 之后注册，`PolicyHook` 声明 `Policy` 阶段，因此仍排在续写、脱敏与落盘之前。两种模式的钩子顺序与此前相同。
  - `cli::prepare` 中的重名检查对内置工具不会触发（`tools::builtin_tools_with` 已跳过重复的名字），只防以后在此注册的工具，因此没有 CLI 测试；失败时与其他启动错误一样以退出码 2 结束。
  - `ToolOutput` 的 `content` 必填（第 7 项已补充）：钩子进程漏写 `content` 时解析失败，不会以空文本替换工具结果。
  - `ToolCall` 也派生 serde，字段与消息中 `toolCall` 块的字段同名（`argumentsError` 为空时省略），`ApprovalRequest` 因此可以往返。
- 公开 API 的变化：
  - `Hooks` 的六个回调增加最后一个参数 `&HookContext`，新增 `Hooks::stage`。
  - `HookChain` 的六个回调方法改为接收 `&HookContext`，不再接收 `&CancellationToken`；`push_with_timeout` 由 `push_with(hook, HookOptions)` 取代。
  - 新增 `HookContext`、`HookOptions`、`HookStage` 与 `LoopSpec::validate`。
  - `ContextEdit::Append(Message)` 改为 `ContextEdit::Append { message }`。
  - `agent` 不再再导出 `CommandPolicy`，`agent::policy` 不再再导出 `CommandPolicy` 与 `DEFAULT_DENIED_PATTERNS`，改从 `policy` 引用。
  - `Decision`、`TurnDecision`、`ContextEdit`、`ToolOutput`、`ToolCall`、`ApprovalRequest`、`Approval` 派生 `Serialize` 与 `Deserialize`；`HookContext`、`TurnSummary`、`TurnDisposition` 派生 `Serialize`。
  - 事件格式、会话文件格式与 CLI 参数不变，`run --help`、`chat --help` 与 `090dbec` 逐字节相同。
- 既有测试的调整只涉及调用方式，断言未改：
  - 测试中的钩子实现增加 `_ctx: &HookContext` 参数；直接调用钩子或 `HookChain` 时传入 `tests/common` 新增的 `hook_context()`，`cancellation_skips_writes_and_reads` 传入带已取消令牌的上下文。
  - `push_with_timeout` 改为 `push_with`，`ContextEdit::Append(...)` 改为带字段的形式。
  - `tests/real_api.rs` 与 `examples/library.rs` 改从 `policy` 引入 `CommandPolicy`。
  - `tests/cli.rs` 未改。
- 本机 macOS 验证：
  - 新增 16 项测试：`tests/hooks.rs` 5 项（钩子上下文、具名钩子、阶段顺序 2 项、重名工具），`tests/hook_contract.rs` 10 项（JSON 契约），1 个 Rustdoc 示例（`compile_fail`：`agent` 不再导出 `CommandPolicy`）。
  - stable 与 Rust 1.88.0 各 531 项通过（基线 515 项），6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过，自查脚本的命中项与基线相同；设置 `ZX_DOCKER_TEST_IMAGE=debian:bookworm-slim` 时容器测试 30 项通过。
  - 反向验证：让钩子忽略阶段、按注册顺序执行时，阶段顺序的 2 项测试失败，其中注册在前的脱敏没有处理扩展追加的密钥；去掉 `run_loop` 中的重名检查时，重名工具的测试失败，运行以 `Completed` 结束。修改均已还原。`ToolOutput` 缺少 `content` 的测试在修复前失败。
  - 未新增依赖，`Cargo.toml`、`Cargo.lock` 未改动。

### P7 插件与 MCP

目标：第三方能力可以不改内核接入（原路线图“MCP 客户端，以及只读的领域工具”）。范围按 D8 收窄（2026-11-01）：本项目只做 MCP 客户端。评审稿见 `docs/p7-mcp-review.md`（草稿）。依赖：`rmcp`（D2 已授权；3.8.0 声明 MSRV 1.88），模块 B 引入；它需要的 reqwest 0.13 已先行升级（D9）。

- [x] 前置设计（上节 A、B 两部分）评审通过并实施（A1：PR #21；B：PR #23；A2–A8：PR #25）
- [ ] 公开 `Extension` trait：注册工具、钩子、提示词段落（命令暂不做，见评审稿 1.3）
- [ ] MCP 客户端扩展：只接工具，只支持 Streamable HTTP（D8；stdio 不做）
- [ ] MCP 工具同样受规则约束；服务器与工具都要在配置中显式列出；无人值守只执行显式放行的工具
- ~~只读的领域工具（如 Kubernetes、Prometheus 查询）以扩展或 MCP 服务器提供~~：由外部服务以 MCP 服务器提供（D8）

验收：MCP 工具可以列出、调用，并受规则约束。

reqwest 0.13 升级记录（2026-11-01，PR #36）：

- 起因：P7 评审中发现，rmcp 只支持 reqwest 0.13，与项目的 0.12.28 并存时，rustls 同时启用 ring 与 aws-lc-rs 两个后端，`wss://` 连接会 panic（`docs/p7-mcp-review.md` 缺口 ①）。项目还没有使用者，用户授权按最合理的方式处理，见决策 D9。
- 改动：`Cargo.toml` 中 reqwest 改为 0.13（`json`、`stream`、`rustls`），源码无需修改。
  - `Cargo.lock` 由 183 个包变为 210 个：新增 30 个，移除 base64 0.22.1、ryu、serde_urlencoded；reqwest 0.12.28 升为 0.13.5，wasm-streams 0.4.2 升为 0.5.0。
  - 本机（macOS）新编译 12 个包：aws-lc-rs、aws-lc-sys 及其构建工具 cmake、dunce、fs_extra、jobserver、pkg-config，rustls-platform-verifier，security-framework 与 core-foundation 各两个；其余是其他平台的条目。
  - rustls 的特性由 `ring,std,tls12` 变为 `aws-lc-rs,aws_lc_rs,std,tls12`；本机的重复依赖只剩原有的 webpki-roots。
- 行为变化：
  - HTTPS 的根证书改用系统信任库；Linux 上没有 CA 证书时，程序启动即报错，已在镜像中实测（评审稿附录 9.4），README 已写明。
  - 构建需要 C 编译器（aws-lc-sys）；项目的 Dockerfile 不改即可构建（linux/arm64 实测）。镜像实测基于 `b169275` 加本次改动；之后合入的 PR #34、#35 没有改动依赖，也没有改动 HTTP 客户端的构建方式。
  - 代理仍读取 `HTTPS_PROXY`、`HTTP_PROXY`、`NO_PROXY`。
- 测试：新增 `tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried`，假 Slack 新增 `Answer::Json`。反向验证：给测试构建加入启用 `ring` 的依赖后，该测试因 rustls 在 `crypto/mod.rs:249` panic 而失败，修改已还原。`.claude/rules/08-dependencies-msrv.md` 记下“rustls 只能有一个加密后端”。
- 本地 macOS（基于含 PR #35 的 `a345b39`）：stable 与 Rust 1.88.0 各 678 项通过（其中 1 项为本 PR 新增），6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过；`tests/openai_mock.rs`、`tests/im_slack.rs`、⚑ D:5/CHINA FLAG: `tests/im_feishu.rs` 与 `tests/serve_cli.rs` 各连续运行 5 次，均通过。
- 未验证：x86_64 镜像的构建；对真实模型服务、Slack 与 ⚑ D:5/CHINA FLAG: 飞书 的 HTTPS 连接（镜像中只连过 example.com）。

### P8 嵌入协议 `acp`

目标：可被编辑器和其他平台驱动。依赖：ACP 官方 Rust SDK（D2 已授权，要求 MSRV 1.88）。

- [ ] `acp` 子命令：通过 stdio 收发 ACP 消息
- [ ] `AcpInteraction`：审批请求转发给客户端

验收：可被 ACP 客户端驱动，审批经客户端完成。

### P9 可选增强

按需求排序，每项单独评估：

- [ ] 结构化最终报告：`--output-schema`，按 JSON Schema 约束最终回答
- [ ] 无人值守的 token 预算：profile 上限增加 token 总量，超出时结束运行；现有退出码 0–6 的含义不变（2026-11-01 补充）
- [ ] 更多 provider：例如 Anthropic Messages API
- [ ] 异步审批：`WebhookInteraction`，高风险命令发 webhook 请求批准，超时则拒绝
- [ ] OS 级沙箱的 Executor：Linux 的 bubblewrap + seccomp、macOS 的 Seatbelt
- [ ] 由 webhook 或告警触发运行
- [ ] OpenTelemetry 追踪（事件模型见"P7 前置"B 部分）
- [ ] 拆分为多个 crate（`zx-ai`、`zx-core`、`zx-tools`、`zx-cli`），用编译器保证分层

## 5. 随各阶段处理的整理项

这些整理不改变行为，在改到相关代码时顺带完成：

- [x] `run_loop` 参数过多（P1）：整理为 4 个参数，运行状态通过 `LoopControl` 借用
- [x] 工具名字符串散落在多个文件（P1 或 P4）：2026-11-01 集中为 `tools::BASH`、`tools::READ_FILE` 与 `profile::skills::TOOL_NAME`
- [ ] `clip` 有两份实现（任意阶段）
- [ ] `BashTool::execute` 过长（P6）

## 6. 推进方式

- 每个阶段可以拆成多个提交；完成后在本文档中勾选任务、更新状态，并记录日期和提交。
- PR 合并后，在本文档中把阶段状态改为“已完成”，并记录 PR 编号与合并日期。也可以在 PR 中直接写成“已完成（PR #编号）”，合并后即准确；`.github/pull_request_template.md` 中有对应检查项。
- 每个阶段结束时：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 通过；用到新语法或新标准库 API 时，`cargo +1.88.0 check --all-targets` 也要通过。
- 从 P0 起，`tests/cli.rs` 中 `run` 的测试不改即通过，以此保证兼容性；D5 若改变默认执行环境，仅在 P6 按明确的迁移规则更新相关测试，其余行为仍须保持兼容。
- 需要新依赖时，先确认在 D2 的授权范围内；范围之外的要先征得同意。
- 设计有变化时，先更新本文档，再改代码。

---

ROADMAP.md · v0.7 · 2026年11月
