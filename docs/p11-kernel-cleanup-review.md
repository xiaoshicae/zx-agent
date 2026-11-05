# 技术方案 · P11 内核整理

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-05` |
| 状态 | 评审中；① 搬家在本稿所在的 PR 中实施 |
| Owner | 待指定 |
| 对应 | `docs/ROADMAP.md`（P11 内核整理） |
| 代码核对基线 | `2367a3a`（PR #66 合并后）；源码行号以此为准，本稿所在 PR 移动过的文件以移动前的位置写 |
| 目标 | 把 `zx-core` 做成轻量的微内核：只含机制，默认实现外移；模块边界清楚、代码简洁、注释精简 |

---

## 1. 背景

用户 2026-11-05 提出两个要求：`zx-core` 要成为轻量级微内核，并质疑内核依赖 `zx-ai`；随后要求内核“设计更优雅、模块更清晰、代码更简洁、注释更精简，便于阅读和维护”。本稿记录评审结论与整理方案。评审由三路并行阅读（session、compaction 与运行层辅助模块、executor 与扩展模块）加核心循环的人工阅读合成，引用的行号逐条核对过。

基线数据（本机 macOS，`2367a3a`）：

| 项目 | 结果 |
|-|-|
| `cargo test` | 944 通过，0 失败，7 跳过（付费测试） |
| `zx-core` 产线代码（去掉单元测试） | 9420 行，其中代码 7178、文档注释 1343、普通注释 203 |
| 注释占代码 | 22% |
| 超过 60 行的函数 | 9 个；最长 `Runner::start` 265 行、`compaction::Host::run` 251 行 |
| clippy pedantic 提示 | `zx-core` 165 条；文档注释首段过长 17 处 |
| `zx-core` 依赖闭包（`cargo tree -e normal` 去重） | 140 个 crate，其中经 reqwest 进来 97 个，jsonschema 的边际增量 64 个；只剩 tokio、futures、serde、anyhow、uuid、libc 时 37 个 |

## 2. 评审结论

### 2.1 分层：内核依赖 `zx-ai` 没有错，错在 `zx-ai` 装了驱动

内核依赖一个只含类型与 trait 的契约 crate 是健康的（tower 依赖 tower-service 就是这种关系）。问题是 `zx-ai` 混装了三种东西：

| 内容 | 行数 | 性质 |
|-|-|-|
| `types`、`provider`、`system_prompt`、`retry` | 1078 | 内核的词汇与 `Provider` 契约 |
| `openai`、`sse` | 1787 | 一个具体驱动，整个 crate 唯一用到 reqwest 的地方 |
| `sync`、`text`（`#[doc(hidden)]`） | 132 | 全 workspace 共用的辅助，因为这是最底层的 crate 才住在这里 |

后果：只想嵌入内核、自己实现 `Provider` 的人，要编译 hyper、rustls 与 aws-lc-sys。`Tool`、`Hooks`、`Executor`、`SessionLog` 都由内核定义，`Provider` 却在内核之下，而且所在 crate 带着 HTTP 客户端。

结论：`openai.rs` 与 `sse.rs` 拆成 `zx-ai-openai`，与 `zx-im-slack` 同构；`zx-ai` 只留词汇与契约（`faux` 与 `retry` 不碰 reqwest，留下）。不把 `zx-ai` 并进 `zx-core`：那样 provider 的实现者要依赖整个内核。

### 2.2 内核里不是机制的部分

按 ROADMAP 原则 1“机制在内核，策略在外层”衡量 `zx-core` 的产线代码：

| 模块 | 产线行数 | 判断 |
|-|-|-|
| `agent`、`runner`、`session` 核心、`agent_session`、`interaction` | 约 5100 | 机制，留 |
| `executor`（含 Docker） | 约 1100 | 默认实现；内核自己从不调用 `Executor`，两个分支上都只有 `prompt.rs` 引用它的 `Platform`。消费者是 `zx-tools` 的工具与 CLI。`docker run`、`docker exec` 的知识写在内核（`executor/container.rs:184`、`:223`），与 D10 相悖 |
| `compaction.rs` 摘要宿主 | 669 | 机制，但 `compaction.rs:26`、`:28` 写死两段摘要提示词 |
| `extensions.rs` 的 `ContinueOnLength`、`RedactSecrets` | 约 270 | 策略；`extensions.rs` 实际是三个模块（注册 20–176 行，续写 183–211 行，脱敏 213–438 行），导入语句两半互不相用 |
| `output_store.rs`（含 `read_output` 工具） | 349 | 一个工具加一个钩子 |
| `session/file.rs` JSONL 后端 | 329 | 默认实现，放这里合理 |
| `prompt.rs` | 150 | `COMMAND_POLICY`、`PROJECT_CONTEXT`、`AVAILABLE_SKILLS` 是上层功能的段落名，内核替它们占位（`extensions.rs:172` 据此拒绝扩展） |

### 2.3 模块与命名

- 三对同名文件：`compaction.rs`、`recovery.rs`、`session_runtime.rs` 各有两份，导航靠记忆。
- `.claude/CLAUDE.md` 第 55 行把循环的会话宿主叫作 `agent::SessionHost`，代码里是 `SessionRuntime`。
- `session/compaction.rs` 装着不属于摘要的东西：九种条目的校验分发 `validate_entry`（398–465 行）、格式版本 `format_version` 与 `ensure_format`（283–327 行）、升级记录类型（153–161 行）。投影被切成两半：原始投影的实现在 `session.rs:515`，公开包装在 `compaction.rs:607`；`projection()` 反过来调用 `compaction.rs:756` 的压缩投影。
- `jsonschema` 是内核第二重的依赖（2026-11-05 经用户确认加入，D2），建议放在默认开启的 cargo feature 后面，或做成 Policy 阶段的内置钩子。

### 2.4 代码

长函数与重复（每条都有覆盖它的测试）：

| 位置 | 问题 |
|-|-|
| `runner.rs:194-458` `Runner::start`，265 行 | 开始阶段（恢复中断调用、续跑输入，228–244）、尝试循环（249–384）、收尾（386–457）写在一个函数里；筛选本次新消息的 20 行（423–442）属于 `SessionStore` |
| `runner.rs:108`、`:259`、`:323`；`runner/session_runtime.rs:14`；`runner/overflow.rs:14` | 三处同样八个字段的借用包（session、hooks、to、provider、model、compaction、compaction_pause、attempt）；暂停状态的读写散在 `session_runtime.rs:79-88`、`113-120`，`overflow.rs:66-71`，`runner.rs:121-124`、`139`、`309-314` |
| `compaction.rs:342-592` `Host::run`，251 行 | 两个立即调用的闭包（354–371、509–535）；返回值 `(Report, Option<Vec<Message>>)` 的第二项只是 `session.messages()`，`runner.rs:120` 用 `.0` 丢掉 |
| `compaction.rs:331-335`、`520-524`；`session/compaction.rs:781-791`；`request.rs:197-202` | 摘要读取范围与压缩后投影在宿主与 session 各算一遍，边界第三次在 request 里算 |
| `compaction.rs:189-202` 与 `session/compaction.rs:572-589` | `merge` 与 `validate_merged` 是同一段循环加同一句报错 |
| `request.rs:205-213` 与 `compaction.rs:443-453` | 摘要请求的消息在两处各构造一遍 |
| `agent_loop.rs:504-549` | `prepare_turn` 里“取消或预算用尽就停”检查了四次；`decide_next_turn`（412–490）把算 disposition 与应用决定混在一起 |
| `agent_loop/tools.rs:158-177`、`323-341` | 工具事件的 id 与 name 在四个方法里各克隆一次 |
| `agent/session_runtime.rs:35-54` | `ContextPreparation` 三个变体各带 requests 与 usage，`runner/session_runtime.rs` 五处构造 |
| `session.rs:154-214`、`423-439` 等 9 处 | 条目头 id、parent、timestamp 的字面量与四个九臂 match |
| `session/file.rs:103-107`、`226-233` | 修复状态用 `Option` 加 `bool` 表示三种情况（规则 05） |
| `session.rs:238-255` | `without_details` 与 `records_without_details` 是同一函数写两遍 |
| `session/compaction.rs:432` | 217 字符的一行条件，rustfmt 放弃；`output_store.rs:321` 435 字符同样 |
| `executor/local.rs:78-101` 与 `container.rs:426-449` | “等结束、收尾、收集输出”逐字相同，只有中间一行不同 |
| `container.rs:217`、`:683`；`prompt.rs:57` | `network` 三处规范化不一致：校验时转小写，传给 Docker 时不转，提示词按小写比对。按读代码推断，`"None"` 能过校验、原样传 Docker、提示词却说有网；未运行验证 |
| `output_store.rs:55-64`、`:152`；`container.rs:640`、`:645` | 内核报错里写着 profile 的键名，内核不该知道 CLI 的键 |

公开面：`leaf_id`（`session.rs:416`）、`branch`（`:486`）、`SessionStore::read`（`:367`）、`append_compaction`（`session/compaction.rs:330`）、`format_version`（`:284`）、`replay_request`（`request.rs:116`）在产线无调用，只有测试在用；要决定哪些是 SDK 承诺，其余降为 `pub(crate)`。

### 2.5 注释

总量不是问题，重复与错位才是：

- 一条规则讲多遍：flock 与尾记录修复在五处（`session.rs:8-10`、`259-264`、`348-354`；`file.rs:19-20`、`135-138`）；“落盘钩子要在脱敏之后注册”在 `output_store.rs` 四遍（`3-5`、`231-232`、`240`、`261`）。
- 历史与编号进了代码：`container.rs:3-11` 与 `:90` 的测试日期与 Docker 版本、`session/compaction.rs:40-42` 的 P5 标记、`executor/mod.rs:95-96` 的“早期执行器”。
- 跨模块手抄数字：`extensions.rs:178-180` 写死 50 KiB（`zx-tools` 的截断值）、`session/compaction.rs:40-42` 写死 2 MiB、`:99` 写死 65536。
- 复述代码：`agent_loop.rs:338`、`runner.rs:155`、`session.rs:155`、`:170`、`:200`、`capture.rs:32`、`container.rs:285`、`tools.rs:22`。
- 行级 pi 出处 24 处，README 架构一节已有完整对应表。

必须留的（代码表达不了的不变量）：`local.rs:31-39` 的 SAFETY 与先 SIGSTOP 再 SIGKILL；`session.rs:550-556`、`566-567`（不变量 5 与 7）；`file.rs:145`、`320-321`；`tools.rs:236-237`（先记开始再执行）；`hooks.rs:397-398`（观察者与操作钩子对取消的不同优先级）；`compaction.rs:274-275`；`session/compaction.rs:740-743`。

## 3. 目标布局

```
crates/
  zx-ai            词汇与契约：types、provider、system_prompt、retry、faux、sync、text
  zx-ai-openai     OpenAI 兼容 provider 与 SSE 解析（唯一依赖 reqwest 的模型层 crate）
  zx-core          内核：agent、session、runner、agent_session、compaction、interaction、prompt、extensions
  zx-tools         内置工具、策略、执行环境（executor）、长结果落盘（output_store）、续写与脱敏钩子、MCP 客户端
  zx-im、zx-im-slack、zx-im-feishu  不变
  zx-agent（根）   CLI；库以原有模块名再导出各 crate
```

`zx-core` 内部的目标布局（三对同名文件改名；`session` 与 `runner` 的子模块都是私有的，改文件名不改公开路径）：

```
crates/zx-core/src
  lib.rs
  agent/                 回合循环与扩展点，结构不变
    agent_loop/…
    hooks/mod.rs         Hooks trait 与类型
    hooks/chain.rs       HookChain 与 HookCall，从 hooks.rs 拆出
    host.rs              原 session_runtime.rs，trait 改名 SessionHost
    events.rs  tool.rs  policy.rs  queue.rs  report.rs
  session/
    mod.rs               原 session.rs：条目树、追加、校验分发、格式版本表
    projection.rs        新文件：原始投影、压缩后投影、去 details
    summary.rs           原 session/compaction.rs 剩下的部分：摘要记录与增量校验
    jsonl.rs             原 file.rs
    recovery.rs  request.rs  log.rs
  runner/
    mod.rs               原 runner.rs
    host.rs              原 runner/session_runtime.rs，并入 overflow.rs
    interrupted.rs       原 runner/recovery.rs
    accounting.rs  answer.rs
  compaction.rs          摘要宿主，位置不变，内部拆函数
  agent_session.rs  interaction.rs  prompt.rs
  extensions.rs          只留 Extension 与 register
```

## 4. 任务与顺序

每项一个 PR；每个 PR 跑 `cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test`，改到 session 格式的加跑 `cargo +1.88.0 check --all-targets` 与 `tests/request_audit.rs` 的固定文件比对；完成后在 ROADMAP 记录。

1. **立项**：本稿；ROADMAP 开 P11；规则 01 的注释条款改为第 5 节的规则。
2. **搬家**：拆 `zx-ai-openai`；`executor`、`output_store`、`ContinueOnLength` 与 `RedactSecrets` 移到 `zx-tools`；`container_environment_section` 随容器执行器走，`prompt.rs` 只保留通用的事实函数。纯移动的 diff 最好评审，之后的整理不再碰内核以外的文件。
3. **机械清理**：`cargo clippy --fix` 处理 pedantic 里可自动修的（`Self` 重复、可 derive `Eq`、单臂 match）；`const fn` 建议用 1.88 工具链复核后再采纳；两处超长行；三对同名文件改名；`SessionHost` 命名统一。
4. **运行层**：拆 `Runner::start`；三处借用包合成 `RunContext`，提供 `host()`、`prepare_context()`、`recover_overflow()`；暂停状态封成 `CompactionPause`；`overflow.rs` 消失。
5. **摘要宿主**：拆 `Host::run`，返回值只留 `Report`；摘要的两端交给 session（`summary_source_on`、`with_summary`）；`carry(previous, new)` 合并 `merge` 与 `validate_merged`；`SummaryRequest::messages` 合并两处请求构造。
6. **会话**：校验分发与版本表搬回 `session.rs`；`EntryHead` 加 `next_head()`；查找辅助；`enum Repair`；`without_details` 合成泛型；公开面收口。
7. **循环小改与注释**：`checkpoint`、`decide_next_turn` 拆分、`ToolRef`（`#[serde(flatten)]`，JSON 不变）；注释随每步触及的文件一起改，没触及的最后一个 PR 收尾。

各项估计合计约减 450 行产线代码；行数不是目标，长函数与重复才是。

## 5. 注释规则

1. 文档注释首段一句话；契约只写出错条件与是否响应取消，其余删。
2. 一条规则只讲一次：在模块文档讲，别处引用。
3. 历史、日期、路线图与决策编号不进代码，写进 `docs/`。
4. pi 出处只在模块文档保留一处，行级 `// pi: file:line` 删除。
5. 跨模块的数字引用常量，不手抄。

## 6. 不做的事

- 不删扩展点：`Provider`、`Tool`、`ToolPolicy`、`Hooks`、`Interaction`、`EventSink`、`Executor`、`SessionLog`、`ImChannel`、`StatusStore`、`Extension` 都是有意提前建设的。
- 不新增依赖；不改会话文件格式（`EntryHead` 若用 `serde(flatten)` 须先用固定文件验证磁盘格式不变，否则不用）。
- 不改事件与钩子的 JSON（`tests/hook_contract.rs`、`agent/events.rs` 的单元测试固定它们）。
