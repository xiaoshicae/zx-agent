# 测试文件一览

每个测试文件覆盖什么。写测试用到的设施与约定在 `.claude/rules/06-testing.md`，原则在 `.claude/CLAUDE.md` 的"测试"一节。

| 文件 | 内容 |
|---|---|
| `src/**` 与 `crates/*/src/**` 中的 `mod tests` | 各模块的单元测试，包括会话文件和事件流的 JSON 格式；自动放行按 profile 给出的选项与例外判断，`deploy/kubernetes/profiles/kubernetes.toml` 由 `src/profile` 的单元测试加载并检查。`#[cfg(test)]` 的辅助项（如 `SessionStore::with_writer`）只在所在 crate 的单元测试里可用 |
| `tests/agent_loop.rs` | 循环语义 |
| `tests/runner.rs` | 运行层：重试（含预算用完不重试、省略失败回答的事件）、上限（含 token 预算：FauxProvider 每次回答报告 15 个 token）、截断续写、持久化（含自定义 `SessionLog`、工具结果的 `details`）、恢复会话、排队输入只写一次 |
| `tests/agent_session.rs` | 共享会话：Busy、取消与丢弃后复用、多订阅者、退订、回调重入与 panic、模型切换与审计、推理强度的切换与审计、取出未被消费的排队输入、会话释放后有界订阅结束 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、空闲超时、中途取消、成功状态下的 JSON 正文、带上下文过长错误码的 400 之后摘要并继续 |
| `tests/prepare_request.rs` | 请求前替换设置（`Hooks::prepare_request`）：只对一次请求有效、钩子看到的计划、`request_start` 报告实际的模型、请求审计可还原、钩子按顺序接力、失败与空模型名在请求前结束且不重试、panic |
| `tests/continue_run.rs` | 不加新输入的续跑：回答结尾的输入、崩溃后补写的结果（调用不再执行）、省略结尾失败的回答再答、完成的回答没有可续的（排队的后续消息除外）、空会话、请求可从会话文件还原、运行中返回 Busy |
| `tests/overflow_recovery.rs` | 上下文过长后的恢复：省略失败回答、摘要一次再开始新的尝试（不算重试）、未开启自动摘要时行为不变、恢复后紧接着又过长时结束、回答成功后可再次恢复、摘要失败暂停自动摘要、预算用完与运行超时 |
| `tests/cli.rs` | 原有 run 行为与退出码，P0 起逐字节保持；P6 默认值变更另行验收 |
| `tests/run_cli.rs` | P0 之后新增的 run 约定（如进度与报告行转义终端控制字符、会话文件单写者、零超时被拒绝、stdout 关闭时的退出码、token 预算的退出码 7：假服务每次回答报告 12 个 token、只在设置了 OTLP 地址时导出追踪：测试中的假收集端只看请求，不解码 protobuf、`--request-field` 到达模型并写入请求审计，写错时以退出码 2 结束、`--fill-missing-reasoning` 默认关闭且开启时写入请求审计、`--reasoning` 的三种写法与冲突检查、`--continue` 失败后续跑与参数检查），新的 run 测试放这里，`tests/cli.rs` 不改 |
| `tests/otel.rs` | OpenTelemetry 导出：真实运行经 `Telemetry` 产生的 span 层级与属性；测试用的导出器 `Kept` 在关闭后仍保留 span（SDK 的内存导出器关闭时会清空） |
| `tests/session_fork.rs` | 复制分支成新会话（P10 ⑥b）：只复制选中的分支、条目逐字节相同、头部的 `parentSession` 与格式版本、副本可重新打开并追加、源文件不变、复制来的请求能从副本还原；目标不为空、叶子不存在、格式版本不同时不写入；没有 `parentSession` 的头部保持原格式 |
| `tests/sessions_cli.rs` | 会话目录（P10 ⑥a、⑥b）：chat 默认的位置、文件名与权限，`--no-session`，`--resume` 取本工作目录最近的会话或按 ID 前缀，`--session-dir`，`zx-agent sessions` 的顺序与预览，chat 的 `/sessions`、`/resume` 与 `/new`；`--fork` 按 ID 或文件复制、找不到时不建文件，`/fork` 之后只写副本，`--no-session` 时没有可复制的文件。每个测试有自己的 `HOME` |
| `tests/chat_cli.rs` | chat 的脚本 stdin、审批与过期（含 `y`、`n` 别名，默认 profile 下 write_file 先审批，文件内容与编辑逐项显示）、工具行与结果预览（含 edit_file 的补丁）、`[turn]` 行与 `/status`、`--color`、续接会话的提示、重试错误只报一次、无用量的 `[turn]`、`--show-thinking`、信号与超时（取消只报告一次，空闲时 Ctrl+C 两次退出）、取消后丢弃残留插话、钩子失败的提示、Unicode、模型与推理强度的切换、`/continue`、会话重置和 Unix PTY（插话、粘贴合并、等待时的状态行、回答前后的空行） |
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
| `tests/mcp.rs` | MCP 客户端对假 MCP 服务器：两种协议、只交出配置的工具与命名、每个请求带 token、调用与结果的呈现、执行错误与协议错误、超时与取消后放弃请求、截断、连接失败与配置错误、经循环调用 |
| `tests/mcp_cli.rs` | 编译出的程序带 `[[mcp]]` profile：run 调用放行的工具、拒绝要审批的工具，chat 在审批后调用，serve 在开始服务前连接；命令看不到 token 变量，token 的值被脱敏；用不了的服务器让启动以退出码 2 失败 |
| `tests/im_feishu.rs` | ⚑ D:5/CHINA FLAG: 飞书 channel 对假开放平台：ping 与 pong、确认、分片、去重、重连、群 @ 与私聊、卡片点击、token、话题回复、编辑与改发、卡片、限频与停止 |
| `tests/im_slack.rs` | Slack channel 对假 Slack：@ 与私聊、确认与去重、忽略自己的消息、按钮点击、断线重连、连接失败的上报、`wss://` 握手失败（rustls 只有一个加密后端时报错而不是 panic）、凭据被拒、转义与拆分、卡片、限频与停止 |
| `tests/serve_cli.rs` | `serve` 子命令：配置错误（含 `[replies]` 的未知字段）；配置的 `[replies]` 与 `[cardLabels]` 替换英文默认文案；对假 Slack 与假模型的完整流程（审批卡片、命令看不到 token、脱敏、会话文件、运维日志、SIGTERM）；被 SIGKILL 后重启，替换遗留的"处理中…" |
| `tests/im_server.rs` | IM 服务：内存中的 `FakeChannel` 加脚本化模型，覆盖线程与会话、访问控制、审批卡片（批准、拒绝、非审批人、超时、取消）、运行上限与排队、插话、空闲释放与重开、停止与平台错误、停止时告知没有处理的消息（排队中的、运行没取走的插话、停止期间才收到的：`FakeChannel` 的 `late`）、停止取消的运行写明原因；运行中的状态（内存中的 `Statuses` 把记录与删除也写进 `Sent`，便于断言先后）：先删记录再写回答、启动时替换遗留的状态、平台拒绝或未配置、停止时放弃替换（`FakeChannel` 的 `updates`）、记录 / 删除 / 读取失败（`Broken`） |
| `tests/real_api.rs` | 付费：真实模型 API，默认跳过；摘要评测另需 `ZX_E2E_EVAL=1` |

---

TESTS.md · v0.1 · 2026年11月
