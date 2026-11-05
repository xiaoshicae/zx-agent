# 技术方案 · P12 项目整理

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-05` |
| 状态 | 已实施，待评审合并 |
| Owner | 待指定 |
| 对应 | `docs/ROADMAP.md`（P12 项目整理；P11 内核整理 ② 至 ⑥ 在同一 PR 中完成） |
| 代码核对基线 | `a0c400f`（PR #70 的末尾提交）；行号以此为准 |
| 目标 | 按 P11 对内核的做法整理整个项目：模块边界清楚、函数短、重复只留一份、注释只讲代码说不出的"为什么" |

---

## 1. 背景

P11（`docs/p11-kernel-cleanup-review.md`）只看 `zx-core`。用户 2026-11-05 要求按同样的方式看整个项目，列出需要优化的地方并直接实施。本稿记录评审结论、做法与度量；评审由每个区域一路只读阅读加作者复核合成，引用的行号逐条核对过。

## 2. 评审结论

### 2.1 `zx-tools`

- 四个工具各自解析参数、处理 `@` 前缀的路径、判断二进制文件、在取消时放弃等待，写法相同（`tools/read.rs:85`、`edit.rs:146`、`write.rs:75`、`output_store.rs`）。
- 按字节边界截断字符串有四份循环（`tools/truncate.rs`、`edit.rs` 的 `clip`、`mcp.rs` 的 `bounded`、`search.rs` 的 `bound`），而 `zx_ai::text::prefix_within` 已经是公用的。
- `search.rs` 三个工具的收尾（空结果、超出条数、截断说明）各写一遍；`CUT` 常量手抄 `501`，与 `MAX_LINE_BYTES` 无关联。两处错误：`context = 0` 时命中数在 `limit` 与 `2 × limit` 之间不说"还有更多"；stderr 不是 UTF-8 时整段丢弃，引擎标记与错误文本一起丢失。
- `executor/container.rs` 一个文件 1100 行，混着配置校验、探测脚本和执行；`capture.rs` 的读取任务与两种执行器各自组装。
- `policy.rs` 三种策略在一个文件里；`rule_verdict` 用三轮循环表达"Deny 先于 Ask 先于 Allow"。
- `mcp.rs` 的 `connect` 85 行，连接、列举、取工具三件事连写，错误前缀手抄七次。

### 2.2 模型层

- `zx-ai` 的 `text.rs` 含两个读文件的函数，因此依赖 anyhow 与 libc；它们的使用者在 `zx-tools` 与 CLI。
- `zx-ai-openai` 的 `stream_response` 124 行；错误对象转失败的逻辑在正文、流块与 `Accumulator::apply` 各一份；`Accumulator::finish` 与 `failure` 各自构造 `AssistantMessage`；流结束事件按 `stop_reason` 选 `Done` 或 `Error` 的 match 在 faux 与 openai 各一份。
- SSE 解析器 `push` 与 `finish` 重复"取 `data:` 行、合成事件"。
- 行级注释里的 pi 出处与 PR 编号（规则 01 要求只在模块文档注明一次）。

### 2.3 根 crate

- `src/main.rs` 既是入口，又装着 run 子命令、退出码、stdout 行、信号与 JSON 行 sink，chat 的输出模块反过来引用它。
- `src/chat/output.rs` 的 `describe_call`、`progress_line`、`visible` 被 run、acp、sessions 使用，于是三个模块都依赖已冻结的 chat。
- `src/cli.rs` 的 `prepare` 320 行；`max_model_requests`、`timeout_secs`、`token_budget` 三个参数在四个子命令的结构体里各抄一遍；每个入口都重复"读指令文件、连 MCP、填 `SessionOptions`"；两处文档注释挂错了函数（`cli.rs:606-609`）。
- `src/chat/mod.rs` 的 `converse` 243 行，`drive_turn` 155 行，审批等待用 `Option<Ticket>` 加 `bool` 两个变量表示三种状态。
- `src/otel.rs` 的 `Runs::apply` 185 行，六个分支重复"取 span、设属性、按失败设状态、结束"。
- `src/profile/mod.rs` 1077 行，TOML 的原始类型与叠加逻辑和领域类型混在一起；`load` 用字面量匹配三个内置名；"已知工具"的判断写了两遍。
- 代码注释里的决策编号与评审稿章节（`(decision D11)`、`docs/p7-mcp-review.md, 2.3.3`、`docs/serve-im-review.md, section 3.6`）。

### 2.4 IM

- Slack 与飞书两个 crate 各有一份相同的"每个会话的下一次发送时间"与"打开的审批卡片文本"实现，以及相同的结果标签 match。
- `zx-im` 里"带超时的出站调用，失败或超时都上报"写了六遍（状态记录两处、遗留状态两处、发送与更新两处）。
- 飞书 `connect` 返回 `Result<Result<(Socket, i32), String>, ImError>`，调用方要分三层解读。
- worker 的 `Pending` 用 `Option<Sender>` 与 `Option<UserId>` 表示"未决、已决、失效"三种状态。

### 2.5 测试辅助

驱动编译出的程序的 11 个测试文件各自写 `Command::new(env!("CARGO_BIN_EXE_zx-agent"))`，5 个文件各自定义 `stdout` / `stderr`。

## 3. 实施记录

每一项一个提交，都在 `a0c400f` 之上；P11 ② 至 ⑥ 的五个提交在前。

| 项 | 提交 | 做法 |
|-|-|-|
| ① `zx-tools` | `a708397` | `tools/mod.rs` 新增 `parse_args`、`resolve`、`looks_binary`、`unless_cancelled`，四个工具与 `read_output` 改用；四份字节边界循环改用 `prefix_within`；`MAX_PATCH_BYTES` 从 `MAX_DETAILS_BYTES` 推出。`search.rs` 以 `Wording` 与 `finish` 统一三个工具的收尾，`cut_command()` 由 `MAX_LINE_BYTES` 算出，stderr 改为有损解码，没有程序自报身份时结果不带 `details`。`executor/capture.rs` 以 `Capture::spawn` / `finish` 管理读取任务，`await_end` 与 `collect` 由本机与容器两个执行器共用；`container.rs` 拆成 `container/{mod,config,scripts}.rs`，网络成为 `Network` 枚举（`FromStr` 拒绝 `host` 与 `container:*`，`Display` 按 Docker 的写法）；`policy.rs` 拆成 `policy/{mod,rules,command,auto_approve}.rs`，`rule_verdict` 改为 `filter` + `min_by_key`；`mcp.rs` 的 `connect` 拆成 `connect_and_list` 与 `Server::with_tools`，错误由 `fault` 统一加前缀 |
| ② 模型层 | `25102c0` | `StreamEvent::ending` 供所有 provider 选结束事件；读文件的两个函数移到 `zx_tools::files`，`zx-ai` 不再依赖 anyhow 与 libc；`stream_response` 拆成 `send_request`、`http_failure`、`answers_json`、`json_body_failure`、`read_stream`；错误对象统一由 `provider_failure` 转失败，摘录长度一处（`MAX_ERROR_EXCERPT_CHARS`）；`Accumulator::message` 构造每条消息，`apply` 不再检查错误块（`handle_data` 已检查）；`assistant_entry` 构造请求体里的回答；SSE 解析器以 `keep_data` 与 `take_event` 共用两段；`request_options` 的 JSON 展开成多行 |
| ③ 根 crate | `e7fae2f` | `src/run.rs`（run 子命令）、`src/process.rs`（退出码、stdout 行、信号、JSON 行 sink）、`src/render.rs`（调用与事件的文案）从 `main.rs` 与 `chat/output.rs` 分出，chat 之外不再依赖 chat。`cli.rs`：`BudgetArgs` 以 `#[command(flatten)]` 进入四个子命令；`Shared::connect` 读指令文件并连 MCP，`SessionOptions` 带 `&Shared`；`prepare` 拆成 `secret_files`、`load_profile`、`provider`、`host_tools`、`check_session_outside`、`executor`、`hidden_env`、`hidden_values`、`rules_policy`；`resolve_cwd` 移到 `cli.rs`，`SessionFile`、`open_existing`、`create` 移到 `sessions.rs`；两处文档注释归位。chat：`Chat` 持有设置、交互与输出，`build` 与 `switch` 是它的方法；`opening` 选存储与要续接或复制的会话；`drive_turn` 以 `Pending` 枚举表示审批等待，`answer_approval` 处理等待期间的输入，`settle` 取消并报告。acp：`notice` 与 `compaction_update`；serve：`slack_channel`、`feishu_channel`、`exit_code`；otel：`finish` 与 `Open::for_call`；debug：`entry`、`reason_suffix`、`tokens`；profile：TOML 层（原始类型、叠加、`read_text`）在 `profile/file.rs`，`Builtin::ALL` 与 `Builtin::name` 选内置 profile，`known_tool` |
| ④ IM | `c315a51` | `zx-im` 新增 `platform.rs`：`Seen`、`split_text` 搬入，新增 `Pacer` 与 `Cards`，两个平台 crate 改用；`CardLabels::outcome`；`Shared::bounded` 统一带超时的出站调用，状态记录、遗留状态、发送与更新六处改用，遗留状态的处理移到 `status.rs`；worker 的 `Decision` 枚举（`Open` / `By` / `Lapsed`）；飞书 `connect` 返回 `Connection`（`Open` / `Later`），握手拆成 `open_socket`，心跳帧 `ping()`；Slack `open_socket` 上报连接失败 |
| ⑤ 测试辅助 | `1c0e5bb` | `tests/common/mod.rs` 新增 `zx_agent()`、`stdout`、`stderr`，11 个文件改用 |
| ⑥ 注释与文档 | 本提交 | 行级的 pi 文件出处、PR 编号、决策编号与评审稿章节只保留在模块文档；标出 pi 对应标识符的注释（如 `(pi: \`ThinkingLevel\`)`）保留，模块文档说不出这层对应；README、`.claude/CLAUDE.md`、规则 04 的路径与名称随改动更新；本稿与 ROADMAP |

修正的两处错误各有先失败后通过的回归测试：`tests/search_tools.rs` 的 `grep_notes_more_matches_when_just_over_the_limit`，`search.rs` 单元测试 `the_engine_marker_is_read_from_error_output_that_is_not_utf8`。另新增单元测试：`network_names_are_read_case_insensitively_and_written_back_as_docker_expects`（容器配置）、`an_error_chunk_ends_the_stream_as_a_provider_failure`（openai）、`builtin_names_are_spelt_as_extends_takes_them`（profile）。

公开 API 的变化：`zx_ai::text` 不再含 `read_text_file` 与 `read_regular_file`，它们在 `zx_tools::files`（根包以 `#[doc(hidden)]` 再导出为 `zx_agent::files`）；`zx_agent::executor::Network` 新增，`ContainerConfig::network` 由 `String` 改为 `Network`；`zx_im` 新增 `Pacer`、`Cards` 与 `CardLabels::outcome`；`StreamEvent::ending` 新增；`Builtin::ALL` 与 `Builtin::name` 新增。会话文件、事件与钩子的 JSON 格式未变（`tests/request_audit.rs` 的固定文件比对与 `tests/hook_contract.rs` 通过）。

## 4. 度量

产线代码不含单元测试（`#[cfg(test)] mod tests` 之前）。"超过 60 行"按大括号匹配计函数长度。

| 区域 | 产线行 基线 → 现在 | 超过 60 行的函数 基线 → 现在 | 最长函数 基线 → 现在 |
|-|-|-|-|
| `zx-ai` | 1149 → 1113 | 1 → 0 | `FauxProvider::stream` 63 → 无 |
| `zx-ai-openai` | 1132 → 1160 | 2 → 0 | `stream_response` 124 → 无 |
| `zx-core`（P11 ② 至 ⑥） | 7828 → 8025 | 9 → 5 | `Runner::start` 265 → 153 |
| `zx-tools` | 4836 → 5030 | 4 → 1 | `Server::connect` 85 → `apply_edits` 67 |
| `zx-im` | 1927 → 2009 | 2 → 2 | `worker::run` 75（未动） |
| `zx-im-slack` | 632 → 595 | 1 → 0 | `receive` 69 → 无 |
| `zx-im-feishu` | 1113 → 1109 | 1 → 1 | `receive` 84 → 70 |
| 根 crate `src/` | 6886 → 7075 | 13 → 13 | `cli::prepare` 320 → 113；`chat/debug.rs` 的 `describe` 319 → 311（一个按变体展开的 match，未拆） |

行数多在拆分后略增：新增的小函数各带一句文档注释。整个仓库的测试函数 987 → 992 个。

## 5. 验证

本机 macOS，`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（990 通过、0 失败、7 项付费测试跳过）、`RUSTDOCFLAGS=-D warnings cargo doc --no-deps`、`cargo +1.88.0 check --all-targets` 通过。自查脚本的命中与基线相同，只是 `src/main.rs` 的一处 `let _ =` 随代码移到 `src/process.rs`。未新增依赖；`Cargo.lock` 只少了 `zx-ai` 对 anyhow 与 libc 的两条边。真实 Docker 的 `real_` 测试与付费 e2e 测试本次没有运行。

## 6. 不做的事

- 不改事件、钩子与会话文件的 JSON。
- `chat/debug.rs` 的 `describe` 与 `otel.rs` 的 `Signal::of` 各是一个按事件变体展开的 match，拆开反而难读，保留。
- `chat` 已冻结：只做不改行为的整理，所有 `tests/chat_cli.rs` 的断言未动。
- 不删扩展点，理由同 P11。
