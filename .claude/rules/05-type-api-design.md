---
paths:
  - "**/*.rs"
---

# 05 类型与 API 设计

- **不可能出现的状态，让它写不出来**：封闭的取值集合用枚举，不用字符串或 bool；互斥的几种状态用一个枚举。改状态时先列出有几种状态、哪些组合不可能出现：两个 `Option`、`Option` 加 `bool`、`Result<Result<_>>` 一律换成枚举（参照 `executor.rs` 的 `CommandEnd`、chat 的 `Pending`、worker 的 `Decision`、飞书的 `Connection`）。时间用 `Duration`，路径用 `Path`。
- **id 的新类型**：新代码中跨模块传递的标识，可以用新类型包一层（如 `EntryId(String)`）。现有代码的 id 是 `String`，要修改先征得用户同意。
- **不提前抽象**：只有一种实现、也不需要测试替身时，直接写具体类型；等出现第二种实现时再定义 trait 或泛型。需要让使用者扩展的（provider、工具、策略、事件订阅）才用 trait。
- **构造**：字段多时用结构体加 `Default` 和结构体更新语法（参照 `OpenAiConfig`、`RetryPolicy`、`Limits`）。只有需要校验或分步构造时才用 builder。
- **数值与切片**：不用 `as` 截断或换号（`cast_*` lint 会报），用 `try_from` 并处理失败；可以证明不会溢出时用 `expect("原因")`（切片长度总在 `isize` 内）；要的就是低位时按字节取（`to_le_bytes`），换号用 `cast_unsigned`。切片可能切在 UTF-8 字符中间或越界，用 `get`、`is_char_boundary`。
- **derive**：`Clone`、`PartialEq`、`Default` 只在语义成立时 derive，不为了让某处能编译而加。持有资源（文件、连接、子进程）的类型通常不应该是 `Clone`。
- **持久化和对外的格式**：字段名用 camelCase。带标签的枚举照同类的已有类型写：消息和内容块沿用 pi 的 camelCase（`toolResult`、`toolCall`），会话条目、事件、流式增量用 snake_case（`context_edit`）。会话只读写当前格式（`SESSION_VERSION`，决策 D15）：改变已持久化的格式时提高版本号，此前的文件随之被拒绝，不写升级或兼容读取的代码。新会话的字节由 `crates/zx-ai/src/types/tests.rs` 的 `messages_keep_the_session_file_format`、`tests/hook_contract.rs` 与固定会话的还原测试保护，这些测试失败就是格式变了。
- **公开 API**：只公开使用者需要的。给公开 trait 新增方法时提供默认实现（参照 `Tool::execution_mode`），否则就是不兼容的变更。
