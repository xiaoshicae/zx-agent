---
paths:
  - "**/*.rs"
---

# 01 代码规范

格式由 rustfmt 决定；能用 lint 查的条款在 `Cargo.toml` 的 `[workspace.lints]`，阈值在 `clippy.toml`。这里只列工具查不出的约定。

- **`#[allow]` 的唯一例外**：`tests/common/mod.rs` 的 `#![allow(dead_code, reason = "...")]`。这个模块被多个测试二进制共享，各自只用一部分；`expect` 会在用全了的二进制里报"预期落空"。其他地方要例外就用 `#[expect(lint, reason = "...")]`，理由写成能核对的一句话（`one arm per event variant`）。
- **导入分三组**，组之间空一行：标准库、外部 crate、本 crate（`crate::`、`super::`）。
- **可见性最小化**：默认私有，crate 内共享用 `pub(crate)`，只有库的使用者需要的才 `pub`。workspace 内另一个 crate 要用、但不属于 API 的项，见 CLAUDE.md 的"分层由 crate 边界保证"。
- **函数**：参数超过 5 个就打包成结构体；不用 bool 参数切换行为，改用枚举或拆成两个函数。长度见规则 09。
- **命名**：函数名说它返回什么（`executor()`、`host_tools()`）或做什么（`open_session()`）；谓词用 `is_`、`has_` 或一个陈述（`waiting()`）。一个概念一个词，全仓库一致：截短字符串只有 `clip`（按字符，加省略号）、`prefix_within`（按字节，不加标记）与 `prefix_noted`（按字节，由调用方给出说明截掉多少的注记）；一个进程里所有会话共用的叫 `Shared`；读子进程输出叫 `Capture`。同名不同义是错误，改名到各自的含义（两处 `host_tool` 之一改成了 `known_tool`）。不缩写，通用的除外：`cwd`、`id`、`ms`、`secs`、`env`、`args`、`ctx`（工具与钩子的上下文）、`cx`（ACP 连接）、`tx`/`rx`（通道两端）、`acc`（累加器）。
- **单位写进名字**：`timeout_secs`、`delay_ms`、`max_bytes`。内部计算用 `Duration`，只在 CLI 和序列化边界用带单位的整数。有含义的数字定义成常量，注释写来源或理由；跨模块引用常量，不手抄（搜索工具的 `first_lines()` 由 `MAX_LINE_BYTES` 算出）。
- **注释**：`pub` 项写文档注释，首段一句话说明做什么，契约只写出错条件与是否响应取消；普通注释只写代码表达不了的"为什么"，一条规则只在一处讲，别处引用它；历史、日期、路线图与决策编号写进 `docs/`，不进代码；沿用 pi 的地方只在模块文档注明出处（`//! … (pi: packages/agent/src/agent-loop.ts)`），行级只标对应的标识符（`(pi: \`ThinkingLevel\`)`）；`unsafe` 块前写 `// SAFETY:`；不留被注释掉的代码。
- **搬代码连文档一起搬**：移动函数时带上它的文档注释，搬完跑 `cargo doc` 查断链（两处挂错函数的文档都是搬家留下的）。
- **match**：按变体分别处理的 match（如 `StopReason`、`AgentEvent`、`Message`）写全所有变体，不用 `_ =>` 兜底，新增变体时编译器会指出所有要改的地方。
