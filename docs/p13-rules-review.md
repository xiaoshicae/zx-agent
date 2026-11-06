# 技术方案 · P13 按规则评审与整理

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-06` |
| 状态 | 已实施，待评审合并 |
| Owner | 待指定 |
| 对应 | `docs/ROADMAP.md`（P13 按规则评审与整理） |
| 代码核对基线 | `ad16f31`（PR #75 的内容并入 main 的合并提交；行号以此为准） |
| 目标 | 按 PR #75 定下的规则（规则 01 至 09 与 `[workspace.lints]`）把整个仓库再评审一遍，并把结论全部实施 |

---

## 1. 背景

PR #71（P11 内核整理与 P12 项目整理）与 PR #75（规则与 lint）之后，用户 2026-11-06 要求按最新的规则再完整评审一次代码并全面优化。本稿记录评审结论、做法、行为变化、度量与验证。

两点说明：

- PR #75 当时合并进了 `claude/project-cleanup` 分支而不是 main（那个分支没有删除，PR 的 base 没有自动改到 main）。它的内容经本 PR 的合并提交 `ad16f31` 进入 main，带来的新 lint 命中（P10 的代码里的 `uninlined_format_args`、`cast_*`、`unreachable_pub` 等）在同一个提交里清掉。
- 评审分五路只读进行（测试辅助、IM、内核、工具与模型层、根 crate），每一条结论都对照代码核实过行号；实施按同样的五个区域进行，测试辅助由作者完成，其余四个区域各在一个独立的 worktree 里实施并通过全部检查，最后合并、再跑一遍全部检查。

## 2. 评审结论

按规则编号归类，只列有代表性的；完整清单在各区域的实施记录里逐条对应。

### 2.1 测试辅助（规则 06、09）

- 11 个驱动程序的测试文件各自写 `String::from_utf8_lossy(&output.stderr)`（共 50 处）、解析 JSON 行的三行链（10 处），而 `tests/common` 已有 `stdout` / `stderr`。
- 请求体里"取用户消息文本 / 系统消息 / 工具结果"的访问器在六个文件里各写一份；`seeded()` 与 `summary(id)` 在三个文件里逐字相同；崩溃留下的工具调用回答在三处手工构造；从会话文件还原请求在三处；会话目录的拼法在两处。
- `tests/im_slack.rs` 与 `tests/im_feishu.rs` 各有一份相同的 `Running` 驱动；`mock_server.rs` 自带一个 HTTP 读取器，而 `tests/common/http.rs` 已有 `read_request`。
- 四处用 `sleep` 等"请求已开始"（`session_branch.rs`、`extension_capabilities.rs`、`agent_loop.rs`、`runner.rs`），只有 `agent_session.rs` 的 `NotifyingProvider` 不依赖计时。
- 同名异义：`tests/container.rs` 的 `stdout(&CommandOutput)` 与公用的 `stdout(&Output)`；`tests/cli.rs` 把公用的 `zx_agent` 改名引入再定义一个同名函数。

### 2.2 IM（规则 03、04、05、09）

- `crates/zx-im-feishu/src/lib.rs` 930 行混着开放平台客户端、长连接与事件；`crates/zx-im/src/server.rs` 675 行把契约、`Server::run` 与分派循环放在一起。
- Slack 的 `call()` 按 `Retry-After` 睡眠，不设上限也不响应取消；`receive()` 没有沉默期限，半开的连接永远不会被判为断开（飞书有 `deadline`）。
- "消息所在或开启的话题"在两个平台里写了四遍；`resolution()` 两份，且对一个不可能出现的变体返回空串；六个常量两边手抄；重连退避两份；`split_text` 保证有第一段却返回 `Vec`，五个调用方写 `unwrap_or_default()`。
- worker 用 `Option<Opened>` 加 `Phase` 守着一个不可能的组合（四处 `if let Some`），`run` 用三个 bool 表示生命周期；`Addressed` 装着总是 `Message` 的 `Inbound` 再重新匹配。
- 平台解析失败处没有注释（规则 03）；`Fragments::add` 的文档与代码不一致；任务失败的上报不带线程键（tokio 1.54 有 `join_next_with_id`）。

### 2.3 内核 `zx-core`（规则 01、02、03、05、09）

- "有 session 就记录并把错误映射成 `Halt::Unrecorded`，否则 `None`"的 match 写了六次；工具调用"执行 → 输出钩子 → `tool_end` → 写结果"的流程三份，`find_tool(..).expect("tool was resolved …")` 三处；`"Invalid arguments for tool {:?}: …"` 四处；`RunTrace::new` 加 `Emit` 三份；`"cannot persist session entry"` 五处；`record_pending_entries` 两份；钩子链里 `entry.call(phase).run(..)` 八处；"列出所有非消息 `Entry` 变体并返回 `None`"三份。
- 两处计价：任务请求按请求的模型名查价，摘要请求按服务端回报的名字查，回报名不同时摘要没有成本。
- `Runner::start` 81 行、`compaction::begin` 80 行、`extensions::register` 75 行、`decide_next_turn` 70 行、`run_loop` 68 行、`candidate` 65 行；`compaction.rs` 837 行、`agent_session.rs` 653 行各混两个主题。
- `PendingInput { requested: Option<Message>, take_follow_ups: bool }` 与 `RunStop { outcome, error: Option<String> }` 都是"`Option` 加 bool / 不该为 `None` 的 `Option`"；`begin` / `validate` / `complete` / `fail` 用 `automatic: bool` 切换行为而 `Trigger` 已存在。
- 无注释的 `unwrap_or_default()` 各自吞掉不同的东西：回合号默认 0、没有超时却报 "0s"、失败回答无 `error_message` 时拼出以 "; " 开头的错误、无 ID 的事实以 `""` 发给模型；摘要校验的错误不写是哪条事实、哪个 id。
- 同名异义 `Prepared`（上下文准备结果 vs 工具调用是否可执行）；一词多义 `summarize`、`attempt`、`Host`、`check` / `validate`；`Emit` 用动词命名一个目的地；`mpsc::channel(32)` 的 32 没有名字；过期文档四处（`SESSION_VERSION` 的历史写到 5 为止、`session_mut` 的用途、`append_context_edit` 对增量模式不成立、钩子阶段说"内核的"策略适配而三者都在 zx-tools）。

### 2.4 工具与模型层（规则 01、03、04、05、09；不变量 8）

- 不变量 8 的两个漏洞：grep / find / ls 失败时把整段 stderr（上限 1 MiB）原样交给模型；MCP 的协议错误用 `Display` 格式化服务端给的 message 与 data，不截断。
- `crates/zx-ai-openai/src/lib.rs` 1308 行分四个主题；按字节截断加标记的写法三份（`edit.rs` 的另一个 `clip`、`mcp.rs` 的 `bounded`、`parse_arguments`）；两个执行器各写一份"是目录 / 不是普通文件 / 超过字节上限"的文本；`SystemUpdates` 与字符串的映射两个方向手写，`"openai-chat-completions"` 两处，请求选项的键名在 `json!` 与 `Options` 各一份；重试表 `NON_RETRYABLE` 重复 `OVERFLOW_TEXT` 的四个短语，`THROTTLING_TEXT` 重复 `RETRYABLE` 的四项，少改一处就会把溢出当成可重试。
- `McpTool::execute` 84 行三件事；`apply_edits` 67 行；`FauxProvider::stream` 63 行；哪个 MCP 超时触发由 `self.max_timeout == Some(timeout)` 猜，两个上限相等时报错的文案是错的。
- `Searched.engine: Option<String>` 装的是 `rg` / `grep` / `find` 三个值；`10 * 1024 * 1024` 写在两个工具里，bash 的 `120` / `600` 是裸字面量，`builtin_tools_with` 为了读一个默认值构造整个 `BashTool`；库的错误文案写着 CLI 的 TOML 键（`toolOutput.directory`、`container.user`）。
- 行级文档引用 pi 的文件路径与 issue 编号；九处文档首段不止一句；六个 `pub` 项没有文档；十一处 `pub` 在 crate 外没有使用者。

### 2.5 根 crate `src/`（规则 01、02、03、05、09）

- `cli` 引用 `sessions`，`sessions` 又引用 `cli::resolve_cwd` 与 `process::EXIT_COMPLETED`：同层循环，因为 `sessions.rs` 既是公用的文件辅助又是 `zx-agent sessions` 子命令。
- `src/cli.rs` 1201 行（clap 定义、追踪导出的包装、会话组装、路径辅助）、`src/otel.rs` 1106 行、`src/acp.rs` 727 行、`src/chat/mod.rs` 1194 行各混三到五个主题；`otel` 的 `apply` 与 `acp` 的 `emit` 带着 `#[expect(too_many_lines, reason = "one arm per …")]`，但分支长达 29 行，不符合规则 09 对这个理由的要求。
- `recorded_call` 与 `emit` 的 `ToolResult` 分支各构造一遍 `ToolCall` 与结果字段；`clip(&describe_call(..), 80)` 三处两个常量名；本地时间格式化的链四份四种兜底；`cli::prepare(SessionOptions { … })` 四个入口各抄一遍；`start_telemetry → sink → run → finish_telemetry` 两份；回答结束的文案两种说法；"环境变量的非空值"四种写法。
- `Host::open(…, replay: bool)`；每个 ACP 会话克隆整个 `BaseArgs` 只为改 `cwd`；`Option<SdkMeterProvider>` 与 `Exports` 互相复述；`Signal.parent` 只对两个变体有意义；`opening` 返回 `(Storage, Option<PathBuf>, Option<String>)`。
- 同名异义：`exit_code`（`process` vs `serve`）、`Live`（ACP 会话 vs 打开的 span）、`resolve`（路径 vs `/branch` 查找）、`PREVIEW_CHARS`（60 vs 120）；文档里 "scene" 与 "scenario" 混用。
- `sessions` 列表直接写 stdout 而不是 `process::print_line`，stdout 关闭时以 `EXIT_USAGE` 退出；`current_dir()?` 与两处 `spawn_blocking` 的错误不写对象；`main.rs` 的文档引用评审稿章节；`secret_files` 用进程级 `Once` 去重通知，因为每个会话都重读一遍。

## 3. 实施记录

每个区域几个提交，都在 `ad16f31` 之上；四个区域在各自的 worktree 里实施，经合并提交 `474b572`、`6e17545`、`95e6bd1`、`bb639c0` 进入分支。

| 区域 | 提交 | 做法 |
|-|-|-|
| 测试辅助 | `12bbb37` | `tests/common/mod.rs` 新增 `json_lines`、`signal`、`session_dir`、`session_files`、`tool_call_answer`、`seeded_session`、`summary_json`、`replayed_requests` 与 `NotifyingProvider`（从 `agent_session.rs` 搬入，四处 `sleep` 改为等 `started`）；`tests/common/im.rs` 的 `Running` 与 `run_channel` 为两个 IM 测试共用；`mock_server.rs` 经 `http::read_request` 读请求（`Request::head_text`），`Captured` 提供 `user_texts`、`system_text`、`tool_results`，六个文件的访问器改为委托；`container.rs` 的 `stdout` 改名 `captured_stdout`，`cli.rs` 的本地构造器改名 `run_against`；`openai_mock.rs` 删掉多余的 `cwd` 覆盖 |
| 工具与模型层 | `af9db5a`、`2b694b8`、`7d6a666`、`e37ee6f`、`6f52767` | `zx_ai::text::prefix_noted`，三份截断加标记改用；重试表每个短语只在一处，`is_retryable` 查溢出表；`FauxProvider::stream` 拆出 `event_for` / `respond`；openai 拆成 `config.rs`、`failure.rs`、`request.rs`、`accumulate.rs`（41 个单元测试随代码搬），`SystemUpdates::as_str` + `FromStr`，`Options` 可序列化，`Accumulator::append`；搜索工具的 stderr 经 `truncate_head` 截断并加注记，`McpTool::failed` 像结果一样截断，MCP 调用自己记总期限（`sleep_until` 分支）并按是否有过进度选文案，`McpTool::call` + `output_of`；`search.rs` 拆成 `search/{mod,grep,find,ls}.rs`，`choose_engine`、`first_lines`、`ripgrep_excludes`，`Engine` 枚举；两个执行器共用 `directory_error`、`special_file_error`、`check_read_within`；`DEFAULT_MAX_FILE_BYTES`、`DEFAULT_TIMEOUT`、`MAX_TIMEOUT`；`apply_edits` 拆出 `locate` 与 `check_overlaps`；输出存储与容器的错误文案改说自己的字段，profile 加 `[toolOutput]` 的上下文；文档与可见性收窄 |
| 内核 | `182c8da`、`a547910`、`90c3b2e`、`44d6ea5`、`51d568d` | `NoSession` 让 `SessionHost` 的方法返回 `Option<String>`，`Engine` 持有 `&mut dyn SessionHost`，六处 match 消失；`Checked` / `Runnable` 携带 `&dyn Tool`，`execute_checked` + `record_result` 三条路径共用，`invalid_arguments`、`blocked_by_policy`，`check_tool` 拆成 `validate_arguments` 与 `authorize`；`LoopConfig::trace`、`persist_error`、`RunHost::open_run` / `close_run` / `emit`、`recover_interrupted_calls` 成为 `RunHost` 的方法，`Runner::start` 约 30 行；`record_pending_entries` 一份；`RegisteredHook::run`，`after_tool_in`；`Entry::projected()`；`agent::price` 两处共用；`check_extension`；`begin` → `record_request` + `announce`，`candidate` → `full_candidate` / `incremental_candidate` / `retained`；`decide_next_turn` 的决策矩阵进 `agent_loop/turn.rs`，`Halt::into_outcome`；`compaction/{mod,answer,host}.rs`、`agent_session/events.rs`；`PendingInput`、`RunStop` 改为枚举，`Trigger` 代替 `automatic: bool`；`AssistantMessage::failure_reason`，回合号由 `TurnPreparation::Ready(turn)` 带回；摘要校验的错误写出序号与 id，头部写入的错误带路径；`Summarizer`、`request_summary`、`OpenSummary`、`EventTarget`、`emit_unobserved`、`report_hook_failure`、`MAX_QUEUED_PROGRESS`；`TurnResponse.index` 代替整份克隆；可见性收窄，`overlap` 的测试搬进 `runner/answer.rs`；`AllowAll` 保留并补测试；四处过期文档改正 |
| IM | `a0f9c21`、`e3c3f4e`、`67e00e0`、`59cfd59`、`bcc24d5` | `zx-im`：`router.rs`（`Router::new` / `run`）、`outlet.rs`（`Outlet::answer`、`Opened::leftover_texts`）从 `server.rs` 与 `worker.rs` 分出，`Server::run` 14 行；`Phase::Waiting` / `Running` 携带 `Opened`，`Lifecycle` 枚举，`Worker::new`；`timed()` 底下的 `Shared::bounded` 与 `ImApproval::confirm`，`Router::refuse` 改用；`join_next_with_id` 加 `note_ended`；`MessageRef::thread()`；`platform.rs` 新增 `Backoff`、`pause`、`Keepalive`、`SEEN_CAPACITY`、`CARD_CAPACITY`、`RATE_LIMIT_RETRIES`、`MAX_TEXT_CHARS`、`card_update_refused`；`split_text` 返回 `(String, Vec<String>)`；`StoredStatus`。飞书拆成 `api.rs`、`connection.rs`（`Link`、`deliver`、`later(reason)`）、`events.rs`（`Addressed` 不再装 `Inbound`）；`Fragments::add` 按文档行事并移动 payload。Slack：`MAX_RETRY_AFTER` 封顶、`call_unless_cancelled`，`Keepalive` 给出沉默期限（`SlackConfig::ping_interval`），`events.rs`；两个平台的解析失败各写明原因或上报 |
| 根 crate | `e680d75`、`ead1506`、`77de0cc`、`5733ab9`、`93380a3`、`daca52d` | `src/list_sessions.rs` 是 `zx-agent sessions`，`sessions.rs` 只剩辅助，循环消失；`cli/{mod,shared,prepare,execution}.rs`，`process::with_telemetry` 包住四个入口，`cli::ModeSettings`、`cli::env_value`，`register_hooks` 返回前端共用的自动放行清单，密钥文件在 `Shared::connect` 读一次（`Once` 删除）；`otel/{mod,signal,spans,metrics}.rs`，`Runs::apply` 每个信号一行（分支体是 `OpenRun` 的方法），`Exporters` 枚举，`parent` 只在两个变体里；`acp/{mod,permissions,updates}.rs`，`Updates::call` / `result_fields`，`emit` 每个事件一行，`Host` 的五个协议方法，`prepare_session` → `replay` → `start` 代替 `replay: bool`，`SessionOptions::cwd` 代替克隆 `BaseArgs`；`chat/{storage,turn,info}.rs`，`First` 枚举，`About::of`，`about_line`，`unless_last_error`；`render::call_label`、`local_time`、`answer_end_line`、`progress_line` 返回 `Severity`；`serve_exit_code`、`OpenSpan`、`SignalSink`、`resolve_location` / `tree::target`、`PROMPT_PREVIEW_CHARS` / `LINE_PREVIEW_CHARS`；`print_line`、错误上下文、文档与可见性 |
| 文档 | 本稿所在的提交 | 本稿、ROADMAP、规则 01（`prefix_noted`、`first_lines`）、规则 06（新的测试设施）、`docs/TESTS.md`（新用例与两个遗漏的文件）、README（会话格式 v6） |

## 4. 行为变化

规则要求的修正，每一项都有先失败后通过的回归测试：

| 变化 | 测试 |
|-|-|
| grep / find / ls 失败时交给模型的 stderr 截断到工具结果的上限并加注记（不变量 8） | `search/mod.rs` 的两个单元测试 |
| MCP 协议错误的文本像结果一样截断（不变量 8） | `tests/mcp.rs` 的 `a_long_protocol_error_is_cut_like_a_result` |
| MCP 调用超时与总上限相等时，没有进度就报"没有应答"，不再报"进度之外没有完成" | `tests/mcp.rs` 的 `equal_bounds_on_a_call_still_report_that_no_answer_came` |
| 所有溢出短语与错误码都不重试（此前只有四个短语在不可重试表里，"prompt is too long" 配上 500 会被重试） | `retry.rs` 的 `an_overflow_is_never_retried_whatever_else_the_error_says` |
| 摘要请求按请求的模型名计价，服务端回报别的名字时成本不再丢失 | `tests/model_meta.rs` 的 `costs_follow_the_requested_model_when_the_server_names_another` |
| 没有 `error_message` 的失败回答统一报 "the request failed without an error message"，不再出现空串或 "0s" | 内核单元测试 |
| Slack：`Retry-After` 最多等 60 秒，等待响应取消；连接沉默超过两个 ping 间隔加余量判为断开并上报后重连；没有正文的消息不转交；非 JSON 的帧与缺 URL 的应答上报 | `tests/im_slack.rs` 的 `stopping_does_not_wait_out_a_long_retry_after`、`a_silent_connection_is_reported_and_opened_again`、`a_message_without_text_is_not_passed_on` |
| 飞书：无法解码的帧与非 JSON 的事件上报后连接继续；分片的校验和不是数字时丢弃该帧（文档说的行为） | `tests/im_feishu.rs` 的 `a_frame_that_cannot_be_decoded_is_reported_and_the_connection_goes_on`、`a_frame_whose_sum_is_not_a_number_is_dropped` |

不带测试的文案变化：chat 里回答因输出 token 上限截断时的那一行改用 run 的措辞（`[model] Length: the response hit the output token limit`）；`zx-agent sessions` 经 `process::print_line` 写 stdout，stdout 关闭时与其他子命令一样处理；库的几条错误文案改说自己的字段（`directory`、`max_file_bytes`、`execution = "container" needs container.image`），CLI 层再加上 TOML 键。CLI 的参数、退出码与文件布局未变，`tests/*_cli.rs` 全部不改即通过。

## 5. 公开 API 的变化

- `zx-ai`：新增 `text::prefix_noted`、`AssistantMessage::failure_reason`；`STREAM_EVENT_BUFFER` 不再公开。
- `zx-ai-openai`：类型从 `config`、`failure`、`request`、`accumulate` 四个模块再导出，原路径不变；`build_body`、`RESERVED_FIELDS` 不再公开。
- `zx-tools`：`CONTINUE_PROMPT`、MCP 的四个默认常量、`truncate_tail`、`files::read_regular_file` 不再公开；搜索工具从 `tools::search` 的子模块导出，路径不变。
- `zx-core`：新增 `agent::MAX_QUEUED_PROGRESS`；`pub` 名称都没有改。
- `zx-im`：`split_text` 返回 `(String, Vec<String>)`；新增 `Backoff`、`pause`、`Keepalive`、`card_update_refused`、`MessageRef::thread`、`SEEN_CAPACITY`、`CARD_CAPACITY`、`RATE_LIMIT_RETRIES`、`MAX_TEXT_CHARS`；`SlackConfig` 新增 `ping_interval`（默认 10 秒）；两个平台的 `DEFAULT_API_BASE` / `DEFAULT_TOKEN_URL` 不再公开。
- 根 crate 的库：`otel` 的四个变量名常量、`Builtin::ALL`、`profile::read_text`、`MAX_FILE_BYTES` 不再公开（workspace、测试与示例里没有使用者）。

会话文件、事件与钩子的 JSON 格式未变（`tests/request_audit.rs` 的固定文件比对与 `tests/hook_contract.rs` 通过）。

## 6. 度量

产线代码不含单元测试；函数长度按大括号匹配计。基线为 `ad16f31`。

| 区域 | 产线行 基线 → 现在 | 超过 600 行的文件 基线 → 现在 | 最长函数 基线 → 现在 |
|-|-|-|-|
| `zx-ai` | 1229 → 1283 | 0 → 0 | `FauxProvider::stream` 63 → `respond` 40 |
| `zx-ai-openai` | 1393 → 1481 | 1（`lib.rs` 1308）→ 0 | `Accumulator::apply` 59 → 55 |
| `zx-core` | 9096 → 9469 | 6 → 2（`session.rs` 840、`session/summary.rs` 694，未在结论中） | `Runner::start` 81 → `run_loop` 67 |
| `zx-tools` | 5158 → 5351 | 0 → 0 | `McpTool::execute` 84 → `McpTool::call` 60 |
| `zx-im` | 2024 → 2348 | 1（`server.rs` 675）→ 0 | `worker::run` 75 → `Worker::approval` 47 |
| `zx-im-slack` | 595 → 670 | 0 → 0 | `receive` 55 → `run` 56 |
| `zx-im-feishu` | 1118 → 1206 | 1（`lib.rs` 930）→ 0 | `receive` 78 → 54 |
| 根 crate `src/` | 8526 → 8981 | 4（`cli.rs`、`chat/mod.rs`、`otel.rs`、`acp.rs`）→ 0 | `otel` 的 `apply` 276 → 一行一个信号；保留的按变体展开的 match：`describe` 301、`Signal::of` 112、`progress_line` 99；冻结的 chat：`converse` 224、`drive_turn` 145、`output::emit` 123 |

行数多在拆分后略增：新增的小函数各带一句文档注释，IM 的 `Keepalive` 与 `Backoff` 是新机制。测试从 1063 个增加到 1086 个（付费的 8 个始终跳过）。

## 7. 验证

本机 macOS，在合并后的分支上：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（见下）、`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`、`cargo +1.88.0 check --all-targets` 与 `.claude/scripts/self-check.sh` 通过；自查脚本的命中只有既有的 `unsafe`（都带 `// SAFETY:`）与带注释的 `let _ =`。未新增依赖，`Cargo.lock` 未变。真实 Docker 的 `real_` 测试与付费 e2e 测试本次没有运行。

`cargo test` 汇总：1086 通过、0 失败、8 跳过。

## 8. 不做的事与遗留

- `Keepalive` 的测试用真实时间的上下界（tokio 没有开 `test-util`），间隔取得很短。
- 四个子命令共用的参数没有合成一个 `ModeArgs`：clap 按声明顺序列出选项，`run` / `chat` / `acp` 在共用组之间各有自己的参数，合并会改变 `--help` 的顺序。
- `cli/execution.rs` 仍克隆 `ContainerConfig`：`ContainerExecutor::start` 消耗它，而环境段落之后还要用；要去掉这份克隆得给 `ContainerExecutor` 加 `config()`。同类：`RedactSecrets::with_secret_files` 按值取 `Vec<PathBuf>`，每个会话克隆一次密钥文件清单。
- `LoopControl.session` 仍是 `Option`：`LoopControl::new` 是公开 API，没有宿主引用时只能给 `None`；循环内部已经不再有 `Option`。
- `session.rs`（840 行）与 `session/summary.rs`（694 行）超过 600 行，不在这次的结论里，留待下次。
- `Entry::projected` 是 `pub(crate)`；`src/chat/tree.rs` 与 `src/acp/updates.rs` 各有一份同样的匹配，公开它就能共用。
- 飞书的 `MAX_INTERVAL`（1 小时）仍远高于 Slack 的 60 秒 `Retry-After` 上限。
- `profile/mod.rs` 的文档仍从库里提到 `run` / `chat` / `serve` 与 `--tool-timeout-secs`；`Profile::validate` 61 行。
- 库与 CLI 各检查一次 `execution = "container"` 需要镜像：库返回自己的文案，CLI 加 `--container-image` 的提示。
- `zx-im` 没有引入 reqwest，两个平台各自构造 HTTP 客户端的几行保留。
- 不改事件、钩子与会话文件的 JSON；chat 只做不改行为的整理；不删扩展点，理由同 P11。

---

p13-rules-review.md · v0.1 · 2026年11月
