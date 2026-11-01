# 场景配置与上下文管理

`run` 默认使用 `troubleshoot`，`chat` 默认使用 `assistant`。两个内置 profile 的 TOML 和说明文件位于 `profiles/`，构建时嵌入二进制，发布后的程序无需额外查找安装目录。

```bash
zx-agent run --model "$ZX_MODEL" --profile troubleshoot "检查磁盘使用情况"
zx-agent chat --model "$ZX_MODEL" --profile assistant
# 从仓库根目录使用完整的只读代码评审示例
zx-agent run --model "$ZX_MODEL" --profile examples/profiles/review.toml "评审 src/profile/ 的实现"
```

## 自定义 profile

`--profile` 接受内置名称或文件路径。文件相对启动目录解析；其中 `instructions`、`context.files`、`skills.path` 均相对 TOML 文件所在目录解析，与 `--cwd` 无关。示例中的全部文件已提交在 [`examples/profiles/`](../examples/profiles/)。

```toml
name = "code-review"
extends = "assistant"
instructions = "review.md"
tools = ["read_file", "read_skill"]
execution = "local"

[rules]
denyTools = ["bash"]

[limits]
maxTurns = 12
timeoutSecs = 300

[extensions]
redaction = true
continueOnLength = 0

[context]
discover = true
files = ["review-context.md"]

[[skills]]
name = "rust-review"
description = "Review Rust ownership, async cancellation, errors and test coverage."
path = "skills/rust-review/SKILL.md"
```

| 配置 | 语义 |
|---|---|
| `name` | 场景名称，省略时继承基线 |
| `extends` | 可选的 `troubleshoot` 或 `assistant`；省略时使用当前前端的默认 profile；不支持文件链式继承 |
| `instructions` | 替换场景说明的 UTF-8 文件；CLI `--system-prompt-file` 优先，它的读取方式与引入 profile 之前一致：可以是管道，不受 64 KiB 限制 |
| `tools` | 替换启用列表，支持 `bash`、`read_file`、`read_skill`、`read_output`；`[]` 禁用全部工具 |
| `execution` | 当前只接受 `local`；容器执行留到 P6，填写未实现后端时报错 |
| `rules.allow` | bash 命令前缀白名单，沿用原管道检查；空列表表示只检查 deny |
| `rules.deny` | 追加 bash 命令子串拒绝规则，原内置拒绝规则始终保留 |
| `rules.askTools` | 替换需审批的工具名列表；run 通过 NoHuman 明确拒绝，chat 交给终端审批 |
| `rules.denyTools` | 追加被拒绝的工具名；启用该工具或批准调用都不能覆盖拒绝 |
| `limits.maxTurns` | 每轮模型请求数；`"unlimited"` 不限，`0` 不请求模型 |
| `limits.timeoutSecs` | 每轮总时间上限，`0` 关闭 |
| `extensions.continueOnLength` | 输出达到 token 上限后的续写次数；每次消耗一次模型请求预算 |
| `extensions.redaction` | 仅接受 `true` 或省略；CLI 的已知认证 key 脱敏不可关闭 |
| `context.discover` | 是否自动发现项目 AGENTS.md；`assistant` 默认开启，`troubleshoot` 默认关闭 |
| `context.files` | 追加显式上下文文件 |
| `skills` | 显式登记 name、description、path 的目录；正文按需读取 |
| `compaction.triggerBytes` | 自动摘要阈值，0 关闭；非零须大于 `keepRecentBytes` |
| `compaction.keepRecentBytes` | 近期消息的序列化字节预算，大于零；完整保留最新输入和全部 system，可因此超出保留预算 |
| `compaction.maxSummaryTokens` | 摘要输出 token 上限，1–16384，默认 2048 |
| `compaction.timeoutSecs` | 摘要流总超时，大于零，默认 120 秒 |
| `toolOutput.thresholdBytes` | 超过此大小的工具结果落盘；0 关闭，否则为 256–maxFileBytes，默认 8192 |
| `toolOutput.maxFileBytes` | 读写文件上限，1–1048576，默认 1 MiB |
| `toolOutput.directory` | 显式路径相对 TOML 文件；省略时为工具 cwd 下的 `.zx-agent/outputs` |

未提供的值继承基线。叠加顺序为 **内置基线 < profile 文件 < 显式命令行参数**；即使 CLI 值恰好等于旧默认值，也按显式覆盖处理。`--allow-cmd` 替换文件的 allow；`--deny-cmd` 追加到文件和内置 deny，不能清除拒绝规则。重复的 `--context-file` 追加文件。

连接地址、模型、认证环境变量名、传输与工具超时、输出模式和会话路径仍使用原 CLI 选项。TOML 不接受这些键，也不接受任意扩展名称；未知字段、错误类型、无效路径或不支持的配置均在请求模型前以退出码 2 报错。

`run` 始终是无人值守前端，`chat` 始终保留 bash 的终端审批；`--profile` 不改变前端。例如 `run --profile assistant` 遇到 bash 的 Ask 会拒绝，`chat --profile troubleshoot` 仍会询问 shell 审批。

## 项目上下文

开启自动发现时，从最近 Git 根目录到工具 cwd，依次加载各层 `AGENTS.md`。Git worktree 的 `.git` 文件也被识别为边界；没有 Git 根目录时只检查 cwd。不会隐式读取用户主目录里的其他 AI 配置。

`chat` 默认使用的 `assistant` 开启自动发现；`run` 默认使用的 `troubleshoot` 关闭，无人值守运行不读取仓库里的说明。`run` 需要项目说明时，用 `--context-file AGENTS.md` 显式指定，或在自定义 profile 中设置 `[context] discover = true`。

自动发现的 `AGENTS.md` 可以是符号链接，但解析后的路径必须位于 Git 根目录内（没有 Git 根目录时为 cwd），否则在请求模型前以退出码 2 报错。这样仓库内容就不能让程序读取并发送仓库外的文件。`--context-file` 与 profile 中的上下文文件由使用者显式指定，不受这条限制。

项目说明会进入系统提示词，可能引导模型的行为，但不能改变工具和规则。在不可信的仓库中运行时，建议使用 `--no-context`。

```bash
zx-agent run --model "$ZX_MODEL" --context-file ./extra-context.md "分析当前项目"
zx-agent chat --model "$ZX_MODEL" --no-context
```

`--context-file` 相对启动目录解析，即使工具 cwd 通过 `--cwd` 改到别处。`--no-context` 同时关闭自动发现和 profile 中的显式上下文文件，不能与 `--context-file` 同时使用。相同 canonical 路径只加载一次。

上下文追加到生成的 `project_context` 提示词段落，沿用现有系统提示词持久化和更新机制。只读普通 UTF-8 文件，每个文件最多 64 KiB；上下文总量包含路径标题和分隔符，最多 256 KiB。超限报错，不静默截断指令；报错会提示可以用 `--no-context` 跳过项目上下文。

## 按需加载 skills

P4 使用 profile 中显式登记的目录，不自动扫描全局 skills，也不解析 SKILL.md 的 YAML frontmatter。已有 SKILL.md 可以直接作为正文，名称和简介以 TOML 为准；skill 里的脚本不会自动执行。

启动只检查登记项及文件元数据，并向 `available_skills` 加入目录。目录包含每个 skill 正文文件的路径；该路径与 `<environment>` 中的工作目录一样，会随系统提示词发送给模型服务方。正文只有模型调用 `read_skill` 时才读取，作为普通工具结果先持久化、再发给模型：

```text
profile 的目录信息 → 系统提示词 → 模型选择 read_skill(name)
                                      ↓
                              参数校验 → 策略 / 审批
                                      ↓
                         按需读取正文 → 脱敏 → 审计 → 模型
```

每个目录最多 64 项，name 使用 1–64 个小写字母、数字或连字符，description 为 1–1024 字节。正文文件最多 64 KiB；单次工具输出沿用 2000 行、50 KiB 的截断限制。输出被分页时返回下一次调用的 UTF-8 字节 `offset`，可继续读取，包括一条超长行的剩余部分。

`read_skill` 仅接受已登记名称和可选 offset，不能通过参数指定任意路径。内置 profile 预留该工具；目录为空时不注册。`--tools` 或 profile 的 tools 列表未启用它时，工具和目录均不加入请求。skill 调用与其他工具使用同一条校验、钩子、审批、取消、脱敏和审计流水线。

项目上下文与 skill 正文是提示词和工具数据，不能修改已组装的工具或规则。认证 key 的脱敏边界仍见 [README 安全模型](../README.md#安全模型)；上下文和说明属于输入，不在工具输出过滤范围内。

## 上下文管理

`assistant` 默认在序列化上下文超过 256 KiB 时自动摘要，`troubleshoot` 默认关闭。触发值继承所选 profile，与前端名称无硬编码关联。手动 `/compact` 和 SDK `AgentSession::compact()` 共用独占运行权；没有旧消息可压缩时返回 Noop，不调用模型。

```toml
[compaction]
triggerBytes = 262144      # 0 禁用自动摘要
keepRecentBytes = 65536
maxSummaryTokens = 2048
timeoutSecs = 120
```

```bash
zx-agent chat --model "$ZX_MODEL" --no-auto-compact
zx-agent run --model "$ZX_MODEL" --compact-threshold-bytes 262144 "处理长任务"
```

两个选项互斥；`--compact-threshold-bytes 0` 也关闭自动触发。`--no-auto-compact` 不影响手动 `/compact`。SDK 默认不自动压缩，原构造器不变；宿主可在 Runner 上调用 `with_compaction(Config)` 校验并启用。

自动步骤在输入和上下文钩子的改动持久化之后、任务模型请求之前执行。一次摘要占用一次 `--max-turns`，其 usage 与失败成本也计入本轮报告，并受整轮时间限制。摘要的工具列表为空；失败不重试成任务请求。手动摘要单独返回 `compaction_report`，不占下一轮 prompt 的预算。

预算按内部消息 JSON 的字节数估算，不是 tokenizer，也不包含工具定义或 HTTP 封装；不能据此保证适合每个模型的 token 窗口。保留最近约 64 KiB、完整的最新用户输入、全部 system 更新，边界不拆开工具调用与结果。保留范围会向前扩到最新输入，并保留它之后的全部消息；单个任务过长也可能超过预算。若保留部分无法缩到阈值内，明确停止，不截断指令或最新输入。摘要源最多 2 MiB，摘要响应和结构化正文最多 64 KiB；源过大时需新建会话，无法无限压缩同一份原始历史。

审计流程如下：

```text
原始投影 + entryId → compaction_started（先写入）→ 同一 provider/model 的摘要请求
                                                        ↓
旧日志：先追加 format_upgrade v3              校验 JSON / 引用 / 大小 / 停止原因
                                                        ↓
原始消息始终保留                 compaction 或 compaction_failed（再追加）
                                                        ↓
                              全部 system + 成功摘要 + 保留范围 → 任务请求
```

新文件使用 v3，旧 v1/v2 文件的头部不改写。开始记录保存源 ID、保留边界、摘要请求配置；成功记录保存摘要、用量和开始记录的引用。失败或进程退出后未完成的尝试不改变投影。重压缩始终读取应用过 `context_edit` 的原始投影，不读取旧摘要；各分支独立。

摘要字段为 `facts: [{text, sourceEntryIds}]`、`inferences: [string]`、`plans: [string]`。每条事实至少引用一个本次源条目；无效引用、无效 JSON、工具调用、输出截断或超限均拒绝提交。引用存在只证明出处可定位，**不证明模型的陈述正确或摘要无信息损失**。system 和合成摘要不能直接通过 `context_edit` 改写；修正被摘要覆盖的原消息后，旧摘要失效，投影恢复到已应用修正的原始记录。

`compact` 与 prompt 重叠时返回 Busy，运行中也不能切模型或清授权。取消、超时或丢弃 future 会释放运行权，未完成记录保留供审计。TTY 手动压缩期间可暂存一个普通输入，成功或失败返回后执行；Ctrl+C 清除暂存输入并报告。管道脚本仍按操作顺序消费。

## 工具结果落盘

两个内置 profile 默认启用 `read_output`。工具返回的文本超过 8 KiB 时，在最后一个 `after_tool` 钩子中脱敏后写入文件，会话和模型只收到 `{outputId, path, bytes, readWith}` 引用。SDK 默认不安装这一钩子；显式 `--tools` 未启用 `read_output` 时也不安装，保留工具本来的有界输出。

```toml
[toolOutput]
thresholdBytes = 8192      # 0 保持内联，仍可读取已有结果
maxFileBytes = 1048576
# directory = "results"   # 相对本 TOML；省略则使用 cwd/.zx-agent/outputs
```

保存的是 `bash`、`read_file` 等工具**已经返回的有界文本**，不会恢复此前已截掉的 stdout、stderr 或文件内容。文件写入失败时，按既有钩子契约返回 `Tool output withheld` 错误结果供模型恢复，不泄出原文本，也不编造文件引用。

模型调用 `read_output({"id":"<outputId>","offset":0})` 读取，offset 为 UTF-8 字节位置。正文加续读提示不超过配置阈值；默认 8 KiB，关闭落盘后读取仍使用 8 KiB 页。严格校验 UUID 和参数，只在配置目录读取 UUID 文件；Unix 拒绝末级文件符号链接和特殊文件。`read_output` 和 `read_skill` 在 CLI 中不再次落盘，避免递归引用。

新目录及 UUID 文件在 Unix 上分别使用 0700、0600；已有目录权限不改动，UUID 文件用 `create_new` 防止覆盖。文件不自动删除；取消可能留下未被日志引用的部分文件。续跑应使用同一结果目录，并一并保存 JSONL 和结果文件；每文件受上限约束，总磁盘占用需由宿主管理。默认目录已加入 `.gitignore`。

SDK 宿主注册同配置的 `OutputStore`、`ReadOutputTool` 和 `SpillOutputs`，写入钩子必须在 `RedactSecrets` 之后，并把分页工具名加入豁免列表。落盘不扩大已知密钥过滤范围，未知凭证和其他敏感数据仍需由宿主处理。

---

PROFILES.md · v0.3 · 2026年11月
