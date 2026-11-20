---
paths:
  - "**/*.rs"
---

# 01 代码规范

格式由 rustfmt 决定，能用 lint 查的条款在 `Cargo.toml` 的 `[workspace.lints]`，阈值在 `clippy.toml`。这里只列工具查不出的约定。

- **`#[expect]` 的理由**写成能核对的一句话（`one arm per event variant`）；不用 `#[allow]`（红线 3）。几个测试二进制共用的辅助，只有其中一部分用到的，单放一个文件，由用到的二进制经 `#[path]` 引入（`crates/zx-cli/tests/common/pty.rs`）。
- **导入分三组**，组之间空一行：标准库、外部 crate（工作区的其他 crate 也是）、本 crate（`crate::`、`self::`、`super::`；子模块写成 `self::` 开头）。分组由 `cargo fmt -- --config group_imports=StdExternalCrate` 完成：这个选项还不稳定，写进 `rustfmt.toml` 会被忽略，只能在命令行给出。
- **可见性最小化**：默认私有，crate 内共享用 `pub(crate)`，只有库的使用者需要的才 `pub`。
- **函数**：参数超过 5 个就打包成结构体；不用 bool 参数切换行为，改用枚举或拆成两个函数。
- **命名**：函数名说它返回什么（`executor()`、`hidden_env()`）或做什么（`open_session()`）；谓词用 `is_`、`has_` 或一个陈述（`waiting()`）。一个概念一个词，全仓库一致，例如截短字符串只有 `clip`、`prefix_within` 与 `prefix_noted`。同名不同义是错误，改名到各自的含义。不缩写，通用的除外：`cwd`、`id`、`ms`、`secs`、`env`、`args`、`ctx`（工具与钩子的上下文）、`cx`（ACP 连接）、`tx`/`rx`、`acc`。
- **单位写进名字**：`timeout_secs`、`delay_ms`、`max_bytes`。内部计算用 `Duration`，只在 CLI 和序列化边界用带单位的整数。有含义的数字定义成常量，注释写来源或理由；跨模块引用常量，不手抄。
- **注释**：`pub` 项写文档注释，首段一句话说明做什么，契约只写出错条件与是否响应取消；普通注释只写代码表达不了的"为什么"，一条规则只在一处讲，别处引用它；历史、日期、路线图与决策编号写进 `docs/`，不进代码；沿用 pi 的地方在模块文档注明出处（`//! … (pi: packages/agent/src/agent-loop.ts)`），单个项只写 pi 中对应的名字（``(pi: `appendEntry`)``）；`unsafe` 块前写 `// SAFETY:`；不留被注释掉的代码。搬代码连文档一起搬，搬完跑 `cargo doc` 查断链。
- **match**：按变体分别处理的 match（如 `StopReason`、`EventKind`、`Message`）写全所有变体，不用 `_ =>` 兜底。

## MSRV 与依赖的 API

- let chains 在 1.88 稳定，可以用（clippy 的 `collapsible_if` 会建议）。
- 依赖的 API 以 `Cargo.lock` 锁定的版本（`cargo tree -i reqwest`）和本项目启用的 feature 为准：各 crate 给 reqwest 只启用 `json`、`stream`、`rustls`，代码不用 `reqwest::blocking`（只在依赖 `zx-app` 的构建里由 `opentelemetry-otlp` 打开）；tokio 没有启用 `io-std`，没有 `tokio::io::stdin`。
- serde_json 在根目录 `Cargo.toml` 开了 `preserve_order`，单独编译一个 crate 时 `serde_json::Map` 也按插入序；比较 JSON 字符串的断言按写入顺序写。
