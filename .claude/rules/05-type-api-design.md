---
paths:
  - "**/*.rs"
---

# 05 类型与 API 设计

- **不可能出现的状态，让它写不出来**：封闭的取值集合用枚举，不用字符串或 bool；互斥的几种状态用一个枚举。改状态时先列出有几种状态、哪些组合不可能出现：两个 `Option`、`Option` 加 `bool`、`Result<Result<_>>` 一律换成枚举（参照 `crates/zx-builtin/src/executor.rs` 的 `CommandEnd`、`crates/zx-core/src/runner.rs` 的 `Ending`）。路径用 `Path`。
- **不提前抽象**：只有一种实现、也不需要测试替身时，直接写具体类型，出现第二种实现时再定义 trait 或泛型。使用者要扩展的点（CLAUDE.md 的“新增功能放在哪里”）才用 trait。
- **构造**：字段多时用结构体加 `Default` 和结构体更新语法（参照 `OpenAiConfig`、`RetryPolicy`、`RunLimits`）。只有需要校验或分步构造时才用 builder。
- **数值与切片**：用 `try_from` 并处理失败，不用 `as` 截断或换号（`cast_*` lint）；可以证明不会溢出时用 `expect("原因")`。切片可能切在 UTF-8 字符中间或越界，用 `get`、`is_char_boundary`。
- **derive**：`Clone`、`PartialEq`、`Default` 只在语义成立时 derive，不为了让某处能编译而加。持有资源（文件、连接、子进程）的类型通常不应该是 `Clone`。
- **带容器级 `#[serde(default)]` 的结构**（配置文件的表，如 `RawConfig`、`RawPermissions`）：读取它的地方用写出全部字段、不带 `..` 的解构（`let Self { a, b } = self;`），不用的字段写 `字段: _` 并注明原因。serde 为容器级 default 生成的代码会读每个字段，没有代码读取的字段也不触发 dead_code 警告，漏读的键因此被静默忽略；解构漏写字段则编译报错（E0027）。参照 `crates/zx-app/src/config/agent_file.rs` 的 `AgentFile::apply`。
- **持久化和对外的格式**：字段名用 camelCase。带标签的枚举照同类的已有类型写：消息和内容块沿用 pi 的 camelCase（`toolResult`、`toolCall`），会话条目、事件、流式增量用 snake_case（`context_edit`）。格式由 `crates/zx-ai/src/types/tests.rs` 的 `messages_keep_the_session_file_format`、`crates/zx-agent/tests/hook_contract.rs` 与 `crates/zx-agent/tests/request_audit.rs` 对固定会话的还原保护：它们失败说明格式变了；变的是持久化格式时，按不变量 5 提高版本号。
- **公开 API**：现在没有使用者，改公开 API 不算成本。给公开 trait 新增方法时提供默认实现（参照 `Tool::execution_mode`）。
