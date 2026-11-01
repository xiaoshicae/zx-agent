---
paths:
  - "**/*.rs"
---

# 06 测试

测试原则和分层见 CLAUDE.md 的"测试"一节。这里是写测试时用到的设施和约定。

## 测试设施

| 设施 | 位置 | 用途 |
|---|---|---|
| `FauxProvider` / `FauxStep` | `src/ai/faux.rs` | 按脚本返回文本、工具调用、截断、失败（`Fail`）、挂起直到取消（`Hang`），并记录每个请求 |
| `EchoTool` | `tests/common/mod.rs` | 可设置延迟、失败、panic、terminate，执行时还能注入引导消息或后续消息 |
| `Collect` | `tests/common/mod.rs` | 收集事件；`envelopes()` 返回带信封（`seq`、`runId` 等）的事件，`events()` 返回事件本体，`kinds()` 返回事件类型序列，`tool_end_order()` 返回工具完成的顺序 |
| `summarize` | `tests/common/mod.rs` | 把消息列表压缩成 `"tool:c1=hi"` 这样的短字符串 |
| `run_prompt` | `tests/common/mod.rs` | 用默认参数（10 轮、不取消）跑一次 `run_loop`，返回结果和事件 |
| `RunnerSetup` | `tests/common/mod.rs` | 带测试默认值（不重试、默认上限、内存会话）的 runner 构造器，用结构体更新语法覆盖字段 |
| `fast_retry` / `header` / `read_lines` | `tests/common/mod.rs` | 毫秒级退避的重试策略、会话头部、读取会话文件的每一行 |
| `CancelOnFirstDelta` | `tests/common/mod.rs` | 收到第一段流式增量就取消，测试中途取消而不依赖计时 |
| `FakeFeishu` / `TestFrame` | `tests/common/fake_feishu.rs` | 假 ⚑ D:5/CHINA FLAG: 飞书开放平台：token、长连接地址与消息接口（可按接口安排错误码或 429），长连接由测试逐帧收发；帧编解码独立于被测代码 |
| `read_request` / `response` | `tests/common/http.rs` | 假服务读取一个 HTTP 请求、拼出响应 |
| `FakeSlack` / `Answer` | `tests/common/fake_slack.rs` | 假 Slack：Web API 记录每次调用，可按方法安排错误或 429；Socket Mode 连接由测试逐帧发送、读取确认 |
| `MockServer` / `Reply` | `tests/common/mock_server.rs` | 本地假 HTTP 服务：返回 SSE 流、HTTP 错误，发出部分数据后卡住（`Stall`），或等待测试通知后结束（`Gate`）；记录请求，`wait_for_requests` 等到请求到达 |
| `FailingWriter` / `SessionStore::with_writer` | `src/session.rs`（仅测试编译） | 让会话写入失败，测试不变量 5 和 7 |
| `SessionStore::read` | `src/session.rs` | 读取写入者仍在运行的会话文件（快照，不加锁、不修改文件）；写入者已释放时也可用 `SessionStore::open` |

典型写法：循环测试参照 `tests/agent_loop.rs` 的 `runs_tools_until_the_model_answers`，用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列；运行层测试参照 `tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build()`。

## 测试文件

| 文件 | 内容 |
|---|---|
| `src/**` 中的 `mod tests` | 各模块的单元测试，包括会话文件和事件流的 JSON 格式 |
| `tests/agent_loop.rs` | 循环语义 |
| `tests/runner.rs` | 运行层：重试（含预算用完不重试、省略失败回答的事件）、上限、截断续写、持久化（含自定义 `SessionLog`）、恢复会话、排队输入只写一次 |
| `tests/agent_session.rs` | 共享会话：Busy、取消与丢弃后复用、多订阅者、退订、回调重入与 panic、模型切换与审计、取出未被消费的排队输入、会话释放后有界订阅结束 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、空闲超时、中途取消、成功状态下的 JSON 正文 |
| `tests/cli.rs` | 原有 run 行为与退出码，P0 起逐字节保持；P6 默认值变更另行验收 |
| `tests/run_cli.rs` | P0 之后新增的 run 约定（如进度与报告行转义终端控制字符、会话文件单写者、零超时被拒绝、stdout 关闭时的退出码），新的 run 测试放这里，`tests/cli.rs` 不改 |
| `tests/chat_cli.rs` | chat 的脚本 stdin、审批与过期、信号与超时、取消后丢弃残留插话、钩子失败的提示、Unicode、模型切换、会话重置和 Unix PTY |
| `tests/profiles_cli.rs` | profile/CLI 覆盖与 deny、项目上下文范围、默认值与越界链接、`--system-prompt-file` 兼容、skill 懒加载、工具策略和审计 |
| `tests/session_compaction.rs`、`tests/compaction.rs` | 追加升级、分支 / 引用与原始投影、自动 / 手动摘要、计费、steering、失败、取消与超时 |
| `tests/request_audit.rs` | 请求审计（G3）：从会话文件还原每次请求、配置变化才追加、旧会话升级时机；固定会话 `tests/fixtures/replay-v4.jsonl` 的还原结果与期望文件逐字节比对，改变还原结果须同时升级转换版本或会话格式 |
| `tests/executor.rs` | 执行器接缝：工具只经自身执行器运行命令和读文件，结束方式的报告，执行方式随执行器 |
| `tests/container.rs` | 容器执行：假 docker 程序（在测试目录里记录参数、按文件应答）覆盖启动、命令、读文件、清理、逐个执行（排队的调用在取消后记为未执行）、所用 shell 与 CLI；名字以 `real_` 开头的测试在设置 `ZX_DOCKER_TEST_IMAGE`（本地已有、带 bash 的镜像）时对真实 Docker 运行，否则打印提示后返回；工作目录要让容器用户可读（见 `real_workspace`） |
| `tests/crash_recovery.rs` | 崩溃恢复：开始记录先于执行、中断调用的四种恢复结果且不重放、失败或取消的回答中的调用记为未执行、恢复事件在结果写入之后、旧格式会话、丢弃 future 与 SIGKILL 后恢复 |
| `tests/output_store.rs` | 脱敏顺序、UUID 与文件边界、Unix 权限、UTF-8 分页、容量、取消、存储超时与错误回退 |
| `tests/hooks.rs`、`tests/hooks_cli.rs` | 六个钩子、钩子上下文、具名注册与阶段顺序、审批与失败回退、变换持久化（钩子不能追加 system 消息）、`observes` 的 panic 隔离、工具的声明名与元数据检查、工具取消自己的令牌、已知密钥脱敏接线、钩子失败的文本输出 |
| `tests/hook_contract.rs` | 钩子输入输出的 JSON 契约，改格式会破坏钩子进程与审批转发 |
| `tests/im_feishu.rs` | ⚑ D:5/CHINA FLAG: 飞书 channel 对假开放平台：ping 与 pong、确认、分片、去重、重连、群 @ 与私聊、卡片点击、token、话题回复、编辑与改发、卡片、限频与停止 |
| `tests/im_slack.rs` | Slack channel 对假 Slack：@ 与私聊、确认与去重、忽略自己的消息、按钮点击、断线重连、凭据被拒、转义与拆分、卡片、限频与停止 |
| `tests/serve_cli.rs` | `serve` 子命令：配置错误；对假 Slack 与假模型的完整流程（审批卡片、命令看不到 token、脱敏、会话文件、运维日志、SIGTERM） |
| `tests/im_server.rs` | IM 服务：内存中的 `FakeChannel` 加脚本化模型，覆盖线程与会话、访问控制、审批卡片（批准、拒绝、非审批人、超时、取消）、运行上限与排队、插话、空闲释放与重开、停止与平台错误 |
| `tests/real_api.rs` | 付费：真实模型 API，默认跳过 |

只能在 Unix 上运行的测试（如进程组、信号相关的）加 `#[cfg(unix)]`。

## 写付费的真实 API 测试

运行它们需要用户授权（红线 9）。新增时遵守：

- 只放在 `tests/real_api.rs`。每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、轮次和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
