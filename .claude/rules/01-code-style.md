---
paths:
  - "**/*.rs"
---

# 01 代码规范

格式和 lint 的要求见 CLAUDE.md。这里只列 rustfmt 和 clippy 检查不出、又容易写错的约定。

- **`#[allow]` 的唯一例外**：`tests/common/mod.rs` 的 `#![allow(dead_code, reason = "...")]`。这个模块被多个测试二进制共享，各自只用其中一部分；改用 `expect` 的话，在恰好用到全部辅助项的测试二进制里会报"预期落空"（`unfulfilled_lint_expectations`）。
- **单位写进名字**：`timeout_secs`、`delay_ms`、`max_bytes`。内部计算用 `Duration`，只在 CLI 和序列化边界用带单位的整数。有含义的数字定义成常量，注释写来源或理由（参照 `src/executor/capture.rs` 的 `PIPE_DRAIN_TIMEOUT`）。
- **导入分三组**，组之间空一行：标准库、外部 crate、本 crate（`crate::`、`super::`）。不用 glob 导入，测试模块的 `use super::*` 和 `mod.rs` 中的再导出除外。
- **可见性最小化**：默认私有，crate 内共享用 `pub(crate)`，只有库的使用者需要的才用 `pub`。
- **函数**：参数超过 5 个就用结构体打包；不用 bool 参数切换行为，改用枚举或拆成两个函数。
- **注释**：`pub` 项写文档注释，说明契约（做什么、何时出错、是否响应取消）；普通注释写"为什么"；沿用 pi 的地方注明出处，如 `// pi: agent-loop.ts:254`；`unsafe` 块前写 `// SAFETY:`；不留被注释掉的代码和调试宏。
- **match**：按变体分别处理的 match（如 `StopReason`、`AgentEvent`、`Message`）写全所有变体，不用 `_ =>` 兜底，这样新增变体时编译器会指出所有要改的地方。
- **格式字符串**：变量直接内联，写 `format!("{path}: {err}")`；只有表达式才用位置参数，如 `format!("{}", path.display())`。clippy 默认不检查这一条。
