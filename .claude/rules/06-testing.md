---
paths:
  - "**/*.rs"
---

# 06 测试

测试原则和分层见 CLAUDE.md 的"测试"一节。这里是写测试时用到的设施和约定。

## 测试设施

| 设施 | 位置 | 用途 |
|---|---|---|
| `FauxProvider` / `FauxStep` | `crates/zx-ai/src/faux.rs` | 按脚本返回文本、工具调用、截断、失败（`Fail`）、挂起直到取消（`Hang`），或按请求内容现场决定（`FauxStep::compute`，例如引用摘要请求给出的条目 ID），并记录每个请求 |
| `EchoTool` | `tests/common/mod.rs` | 可设置延迟、失败、panic、terminate，执行时还能注入引导消息或后续消息、报告进度（`progress`）、返回 `details` |
| `Collect` | `tests/common/mod.rs` | 收集事件；`envelopes()` 返回带信封（`seq`、`runId` 等）的事件，`events()` 返回事件本体，`kinds()` 返回事件类型序列，`tool_end_order()` 返回工具完成的顺序 |
| `summarize` | `tests/common/mod.rs` | 把消息列表压缩成 `"tool:c1=hi"` 这样的短字符串 |
| `run_prompt` | `tests/common/mod.rs` | 用默认参数（10 轮、不取消）跑一次 `run_loop`，返回结果和事件 |
| `RunnerSetup` | `tests/common/mod.rs` | 带测试默认值（不重试、默认上限、内存会话）的 runner 构造器，用结构体更新语法覆盖字段 |
| `fast_retry` / `header` / `read_lines` | `tests/common/mod.rs` | 毫秒级退避的重试策略、会话头部、读取会话文件的每一行 |
| `CancelOnFirstDelta` | `tests/common/mod.rs` | 收到第一段流式增量就取消，测试中途取消而不依赖计时 |
| `summary_eval` | `tests/common/summary_eval.rs` | 摘要评测：按脚本合成的 8 轮排障会话（24 条埋入的事实，其中 6 条后来被取代），`evaluate` 每轮之后摘要并评分（仍成立的事实写到了几条；被取代的事实是否只剩旧值，另计写着旧值的条数；从会话还原的摘要请求大小）；付费评测与常规测试共用 |
| `FakeFeishu` / `TestFrame` | `tests/common/fake_feishu.rs` | 假 ⚑ D:5/CHINA FLAG: 飞书开放平台：token、长连接地址与消息接口（可按接口安排错误码或 429），长连接由测试逐帧收发；帧编解码独立于被测代码 |
| `read_request` / `response` | `tests/common/http.rs` | 假服务读取一个 HTTP 请求、拼出响应 |
| `evals` | `tests/common/evals.rs` | 行为评测框架（P10 ⑪，pi 的 `packages/evals`）：`Case`（工作目录文件、提示词、确定性评分）、`Variants`（控制与处理各一个 profile 文件）、`plan`（按轮次交替变体）、`Harness::run`（经编译出的程序运行一个 arm，家目录与工作目录隔离，超时记为 errored）、`summarize`（配对、每组的通过率与提升、阻塞的配对扣住标题数字、无提升 / 负提升 / 饱和 / 不稳定的标记、各变体的合计）、`Artifacts`（`protocol.json`、`expected-runs.json`、逐条追加的 `observations.jsonl`、`report.json` / `report.txt`，每个 arm 的目录在 `<变体>/<组>-<用例>-run<N>/`）；样例用例 `sample_cases` 与 `tests/fixtures/evals/` 的两个 profile，合成运行用 `MockServer` 按 arm 脚本化，真实运行在 `tests/real_api.rs` |
| `FakeMcp` / `Reply` | `tests/common/fake_mcp.rs` | 假 MCP 服务器（Streamable HTTP），按规范手写、不用 rmcp：可选 2026-07-28（`server/discover`）或 2025-11-25（`initialize` 与会话）；工具一页一个；按工具安排结果、JSON-RPC 错误、不应答（`Hang`）或以 SSE 流按间隔发进度通知再给结果（`Progress`，令牌取自请求的 `_meta.progressToken`），并记录每个请求的方法、参数与请求头、被客户端放弃的调用 |
| `FakeSlack` / `Answer` | `tests/common/fake_slack.rs` | 假 Slack：Web API 记录每次调用，可按方法安排错误、429 或指定的 JSON 应答；Socket Mode 连接由测试逐帧发送、读取确认 |
| `MockServer` / `Reply` | `tests/common/mock_server.rs` | 本地假 HTTP 服务：返回 SSE 流（`Reply::text`、`Reply::bash`、任意工具调用 `Reply::tool_call`）、HTTP 错误，发出部分数据后卡住（`Stall`），或等待测试通知后结束（`Gate`）；记录请求，`wait_for_requests` 等到请求到达 |
| `FailingWriter` / `SessionStore::with_writer` | `crates/zx-core/src/session.rs`（仅测试编译） | 让会话写入失败，测试不变量 5 和 7 |
| `SessionStore::read` | `crates/zx-core/src/session.rs` | 读取写入者仍在运行的会话文件（快照，不加锁、不修改文件）；写入者已释放时也可用 `SessionStore::open` |

典型写法：循环测试参照 `tests/agent_loop.rs` 的 `runs_tools_until_the_model_answers`，用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列；运行层测试参照 `tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build()`。

## 测试文件

| 文件 | 内容 |
|---|---|
| `src/**` 与 `crates/*/src/**` 中的 `mod tests` | 各模块的单元测试，包括会话文件和事件流的 JSON 格式；`src/chat/tree.rs` 的树渲染（当前分支优先、离开的分支缩进、不列出的记录、预览）与 ID 解析；`src/chat/commands.rs` 的扩展命令解析；`agent::queue` 的队列模式与扩展消息，`agent::command` 的命令名检查；自动放行按 profile 给出的选项与例外判断，`deploy/kubernetes/profiles/kubernetes.toml` 由 `src/profile` 的单元测试加载并检查。`#[cfg(test)]` 的辅助项（如 `SessionStore::with_writer`）只在所在 crate 的单元测试里可用 |
| `tests/agent_loop.rs` | 循环语义 |
| `tests/runner.rs` | 运行层：重试（含预算用完不重试、省略失败回答的事件）、上限（含 token 预算：FauxProvider 每次回答报告 15 个 token）、截断续写、持久化（含自定义 `SessionLog`、工具结果的 `details`）、恢复会话、排队输入只写一次 |
| `tests/agent_session.rs` | 共享会话：Busy、取消与丢弃后复用、多订阅者、退订、回调重入与 panic、模型切换与审计、推理强度的切换与审计、取出未被消费的排队输入、会话释放后有界订阅结束 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、空闲超时、中途取消、成功状态下的 JSON 正文、带上下文过长错误码的 400 之后摘要并继续、自定义请求头与每次请求取 key、key 来源失败不发请求不重试、`prompt_cache_key` 只在开启时发送并可从审计还原 |
| `tests/prepare_request.rs` | 请求前替换设置（`Hooks::prepare_request`）：只对一次请求有效、钩子看到的计划、`request_start` 报告实际的模型、请求审计可还原、钩子按顺序接力、失败与空模型名在请求前结束且不重试、panic |
| `tests/continue_run.rs` | 不加新输入的续跑：回答结尾的输入、崩溃后补写的结果（调用不再执行）、省略结尾失败的回答再答、完成的回答没有可续的（排队的后续消息除外）、空会话、请求可从会话文件还原、运行中返回 Busy |
| `tests/overflow_recovery.rs` | 上下文过长后的恢复：省略失败回答、摘要一次再开始新的尝试（不算重试）、未开启自动摘要时行为不变、恢复后紧接着又过长时结束、回答成功后可再次恢复、摘要失败暂停自动摘要、预算用完与运行超时 |
| `tests/cli.rs` | 原有 run 行为与退出码，P0 起逐字节保持；P6 默认值变更另行验收 |
| `tests/run_cli.rs` | P0 之后新增的 run 约定（如进度与报告行转义终端控制字符、会话文件单写者、零超时被拒绝、stdout 关闭时的退出码、token 预算的退出码 7：假服务每次回答报告 12 个 token、只在设置了 OTLP 地址时导出追踪与指标：测试中的假收集端只看请求，不解码 protobuf，`TRACEPARENT` 接续调用方的 trace（按 ID 的字节查找）且不合法时提示、`--request-field` 到达模型并写入请求审计，写错时以退出码 2 结束、`--fill-missing-reasoning` 默认关闭且开启时写入请求审计、`--reasoning` 的三种写法与冲突检查、`--continue` 失败后续跑与参数检查），新的 run 测试放这里，`tests/cli.rs` 不改 |
| `tests/otel.rs` | OpenTelemetry 导出：真实运行经 `Telemetry` 产生的 span 层级与属性；经 `with_providers` 的指标（SDK 的 `InMemoryMetricExporter` 关闭时不清空，周期读取器关闭时再导出一次）；`sink_under` 接续给定的 traceparent；测试用的导出器 `Kept` 在关闭后仍保留 span（SDK 的内存导出器关闭时会清空） |
| `tests/idempotency.rs` | 幂等提交（P10 ⑫）：键先于提示词写入且同键第二次不发请求、空键被拒、重新打开的会话仍认得键、失败的提交同键被拒后用 `continue_run` 接着做 |
| `tests/model_meta.rs` | 模型元数据与计量（P10 ⑧）：按单价给回复算成本并在报告与会话文件中相加、没有单价时没有成本、token 阈值触发与不触发自动摘要、上下文 token 的估算、按窗口推导阈值的条件、缓存键进入 provider 与请求审计 |
| `tests/extension_capabilities.rs` | 扩展的运行期能力（P10 ⑦）与队列模式（⑨）：钩子在运行中发消息与记状态（记录顺序、事件、投影）、运行最后的事件中记的状态在报告前写入、宿主限制工具且审计记录每次请求声明的工具、钩子改下一次请求的工具、扩展命令经会话运行且其消息开始一次运行、命令名的检查、空闲时直接写入与运行中 Busy、`one-at-a-time` 每条排队消息各占一个回合 |
| `tests/custom_records.rs` | 扩展的会话记录（格式 v6）：`custom_message` 投影为用户消息且可被 `context_edit` 省略、`custom` 不进投影、旧会话先升级到 v6 且文件中的字段写法、没有升级的记录被拒绝、空类型不写入 |
| `tests/session_branch.rs` | 会话内切换分支（P10 ⑥c）：`AgentSession::branch` 到更早的回答后继续且离开的分支仍在文件里、接在带未答调用的回答后把调用记为未执行且不再执行、回到根重新开始、运行中返回 Busy、未知条目被拒且位置不变、`inspect` 期间会话被占用 |
| `tests/session_fork.rs` | 复制分支成新会话（P10 ⑥b）：只复制选中的分支、条目逐字节相同、头部的 `parentSession` 与格式版本、副本可重新打开并追加、源文件不变、复制来的请求能从副本还原；目标不为空、叶子不存在、格式版本不同时不写入；没有 `parentSession` 的头部保持原格式 |
| `tests/sessions_cli.rs` | 会话目录（P10 ⑥a、⑥b）：chat 默认的位置、文件名与权限，`--no-session`，`--resume` 取本工作目录最近的会话或按 ID 前缀，`--session-dir`，`zx-agent sessions` 的顺序与预览，chat 的 `/sessions`、`/resume` 与 `/new`；`--fork` 按 ID 或文件复制、找不到时不建文件，`/fork` 之后只写副本，`--no-session` 时没有可复制的文件；`/tree` 的分支与标记、`/branch` 接在条目之后或自己的消息之前、`/fork` 复制到移动后的位置、ID 不唯一或不存在时的提示（固定 ID 的种子会话 `seed_tree`）。每个测试有自己的 `HOME`；启动 chat 的其他测试构造器同样把 `HOME` 设到临时目录，新的构造器也要这样做，否则会写进真实的家目录 |
| `tests/chat_cli.rs` | chat 的脚本 stdin、审批与过期（含 `y`、`n` 别名，默认 profile 下 write_file 先审批，文件内容与编辑逐项显示）、工具行与结果预览（含 edit_file 的补丁）、`[turn]` 行与 `/status`、`--color`、续接会话的提示、重试错误只报一次、无用量的 `[turn]`、`--show-thinking`、`--queue-mode` 与 `/queue`、`[[models]]` 的单价与 `[turn]` / `/status` 的成本、`--header` 与 `--prompt-cache-key`、`--context-window` 推导的 token 阈值、信号与超时（取消只报告一次，空闲时 Ctrl+C 两次退出）、取消后丢弃残留插话、钩子失败的提示、Unicode、模型与推理强度的切换、`/continue`、会话重置和 Unix PTY（插话、粘贴合并、等待时的状态行、回答前后的空行） |
| `tests/acp_cli.rs` | `acp`：手写的 JSON-RPC 客户端经子进程 stdio 驱动：初始化与建会话、流式回答、工具调用与权限请求（批准、拒绝）、取消、未知会话、`write_file` 的 diff 与自动放行；stdout 的每一行都必须是协议消息 |
| `tests/profiles_cli.rs` | profile/CLI 覆盖与 deny、项目上下文范围、默认值与越界链接、`--system-prompt-file` 兼容、skill 懒加载、工具策略和审计 |
| `tests/session_compaction.rs`、`tests/compaction.rs` | 追加升级、分支 / 引用与原始投影、自动 / 手动摘要、摘要源不含工具结果的 `details`、计费（摘要请求计入请求与 token 预算）、steering、失败、取消与超时 |
| `tests/incremental_summary.rs` | 增量摘要（v5）：接续与只读新增记录、接续全量摘要、取代、淘汰、修正后的作废与重读、加载校验、请求还原、请求大小不随历史增长；摘要评测的脚本会话自检，以及用知道答案的假模型验证评分 |
| `tests/request_audit.rs` | 请求审计（G3）：从会话文件还原每次请求、配置变化才追加、旧会话升级时机；上下文过长后恢复的三个请求；固定会话 `tests/fixtures/replay-v4.jsonl` 与含增量摘要的 `replay-v5.jsonl` 的还原结果与期望文件逐字节比对，改变还原结果须同时升级转换版本或会话格式 |
| `tests/executor.rs` | 执行器接缝：工具只经自身执行器运行命令和读写文件（grep 的脚本、shell 与隐藏变量），结束方式的报告，执行方式随执行器，不能写文件的执行器 |
| `tests/search_tools.rs` | grep、find、ls 在本机：有无 ripgrep 时断言都要成立（本机没有 rg，CI 可能有）；注入的模式与路径不被执行 |
| `tests/file_tools.rs` | edit_file 与 write_file 在本机：新建、覆盖、多处替换、BOM 与 CRLF、patch，每类失败后文件不变，取消，旧参数写法；同一批调用改同一文件（`SlowReads` 执行器让两次读取都先于写入）时两处改动都生效 |
| `tests/container.rs` | 容器执行：假 docker 程序（在测试目录里记录参数、按文件应答）覆盖启动（默认放开与严格模式）、命令、读写文件、清理、逐个执行（排队的调用在取消后记为未执行）、所用 shell 与 CLI（含 `rules.approveAll` 下 chat 不询问 bash、run 执行 `allowTools` 以外的 MCP 工具、本机执行时报错）；名字以 `real_` 开头的测试在设置 `ZX_DOCKER_TEST_IMAGE`（本地已有、带 bash 的镜像）时对真实 Docker 运行，否则打印提示后返回；工作目录要让容器用户可读（见 `real_workspace`） |
| `tests/crash_recovery.rs` | 崩溃恢复：开始记录先于执行、中断调用的四种恢复结果且不重放、失败或取消的回答中的调用记为未执行、恢复事件在结果写入之后、旧格式会话、丢弃 future 与 SIGKILL 后恢复 |
| `tests/output_store.rs` | 脱敏顺序、UUID 与文件边界、Unix 权限、UTF-8 分页、容量、取消、存储超时与错误回退 |
| `tests/hooks.rs`、`tests/hooks_cli.rs` | 六个钩子、钩子上下文、具名注册与阶段顺序、审批与失败回退、变换持久化（钩子不能追加 system 消息）、`observes` 的 panic 隔离、工具的声明名与元数据检查、工具取消自己的令牌、已知密钥脱敏接线（含进度与 `details`）、钩子失败的文本输出、密钥文件的有损解码匹配与启动检查、内置钩子的固定名称、短值提示 |
| `tests/hook_contract.rs` | 钩子输入输出的 JSON 契约（含 `RequestSettings`），改格式会破坏钩子进程与审批转发 |
| `tests/extensions.rs` | `extensions::register`：扩展的工具、钩子与提示词段落加入循环；钩子以扩展名报告失败；拒绝非 `Extension` 阶段的钩子、重名的工具与扩展、保留或已有的段落名，出错时不注册任何内容 |
| `tests/mcp.rs` | MCP 客户端对假 MCP 服务器：两种协议、只交出配置的工具与命名、每个请求带 token、调用与结果的呈现、执行错误与协议错误、超时与取消后放弃请求、截断、连接失败与配置错误、经循环调用；进度通知到达工具上下文并重置超时（两种协议）、总上限不看进度 |
| `tests/mcp_cli.rs` | 编译出的程序带 `[[mcp]]` profile：run 调用放行的工具、拒绝要审批的工具，chat 在审批后调用，serve 在开始服务前连接；命令看不到 token 变量，token 的值被脱敏；用不了的服务器让启动以退出码 2 失败 |
| `tests/im_feishu.rs` | ⚑ D:5/CHINA FLAG: 飞书 channel 对假开放平台：ping 与 pong、确认、分片、去重、重连、群 @ 与私聊、卡片点击、token、话题回复、编辑与改发、卡片、限频与停止 |
| `tests/im_slack.rs` | Slack channel 对假 Slack：@ 与私聊、确认与去重、忽略自己的消息、按钮点击、断线重连、连接失败的上报、`wss://` 握手失败（rustls 只有一个加密后端时报错而不是 panic）、凭据被拒、转义与拆分、卡片、限频与停止 |
| `tests/serve_cli.rs` | `serve` 子命令：配置错误（含 `[replies]` 的未知字段）；配置的 `[replies]` 与 `[cardLabels]` 替换英文默认文案；对假 Slack 与假模型的完整流程（审批卡片、命令看不到 token、脱敏、会话文件、运维日志、SIGTERM）；被 SIGKILL 后重启，替换遗留的“处理中…” |
| `tests/im_server.rs` | IM 服务：内存中的 `FakeChannel` 加脚本化模型，覆盖线程与会话、访问控制、审批卡片（批准、拒绝、非审批人、超时、取消）、运行上限与排队、插话、空闲释放与重开、停止与平台错误、停止时告知没有处理的消息（排队中的、运行没取走的插话、停止期间才收到的：`FakeChannel` 的 `late`）、停止取消的运行写明原因；运行中的状态（内存中的 `Statuses` 把记录与删除也写进 `Sent`，便于断言先后）：先删记录再写回答、启动时替换遗留的状态、平台拒绝或未配置、停止时放弃替换（`FakeChannel` 的 `updates`）、记录 / 删除 / 读取失败（`Broken`） |
| `tests/evals.rs` | 行为评测框架对脚本化模型：计划的交替、经程序的配对运行与报告（提升、饱和、不稳定、合计 token）、产物文件与每个 arm 的会话和工作目录、起不来的 arm 阻塞配对并扣住标题、手工构造的观测触发无提升 / 负提升 / 缺失与重复 arm 的标记 |
| `tests/real_api.rs` | 付费：真实模型 API，默认跳过；摘要评测与行为评测另需 `ZX_E2E_EVAL=1` |

只能在 Unix 上运行的测试（如进程组、信号相关的）加 `#[cfg(unix)]`。

## 写付费的真实 API 测试

运行它们需要用户授权（红线 9）。新增时遵守：

- 只放在 `tests/real_api.rs`。每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、轮次和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。
- 用量远高于其余测试的（如摘要评测），再要求一个专门的变量（摘要评测为 `ZX_E2E_EVAL=1`），未设置时只打印跳过，免得运行整个文件时连带产生这部分费用。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
