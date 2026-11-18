---
paths:
  - "**/*.rs"
---

# 01 代码规范

格式由 rustfmt 决定，能用 lint 查的条款在 `Cargo.toml` 的 `[workspace.lints]`，阈值在 `clippy.toml`。这里只列工具查不出的约定。

- **`#[allow]` 的唯一例外**：程序的测试公共模块 `crates/zx-cli/tests/common/mod.rs` 的 `#![allow(dead_code, reason = "...")]`。它被程序的多个测试二进制共享，各自只用一部分（`pty` 只有 `chat_cli` 与 `run_cli` 用），`expect` 会在用全了的二进制里落空。其他地方用 `#[expect(lint, reason = "...")]`，理由写成能核对的一句话（`one arm per event variant`）。
- **导入分三组**，组之间空一行：标准库、外部 crate、本 crate（`crate::`、`super::`）。
- **可见性最小化**：默认私有，crate 内共享用 `pub(crate)`，只有库的使用者需要的才 `pub`。
- **函数**：参数超过 5 个就打包成结构体；不用 bool 参数切换行为，改用枚举或拆成两个函数。长度见规则 09。
- **命名**：函数名说它返回什么（`executor()`、`hidden_env()`）或做什么（`open_session()`）；谓词用 `is_`、`has_` 或一个陈述（`waiting()`）。一个概念一个词，全仓库一致，例如截短字符串只有 `clip`、`prefix_within` 与 `prefix_noted`。同名不同义是错误，改名到各自的含义。不缩写，通用的除外：`cwd`、`id`、`ms`、`secs`、`env`、`args`、`ctx`（工具与钩子的上下文）、`cx`（ACP 连接）、`tx`/`rx`、`acc`。
- **单位写进名字**：`timeout_secs`、`delay_ms`、`max_bytes`。内部计算用 `Duration`，只在 CLI 和序列化边界用带单位的整数。有含义的数字定义成常量，注释写来源或理由；跨模块引用常量，不手抄。
- **注释**：`pub` 项写文档注释，首段一句话说明做什么，契约只写出错条件与是否响应取消；普通注释只写代码表达不了的"为什么"，一条规则只在一处讲，别处引用它；历史、日期、路线图与决策编号写进 `docs/`，不进代码；沿用 pi 的地方只在模块文档注明出处（`//! … (pi: packages/agent/src/agent-loop.ts)`）；`unsafe` 块前写 `// SAFETY:`；不留被注释掉的代码。搬代码连文档一起搬，搬完跑 `cargo doc` 查断链。
- **match**：按变体分别处理的 match（如 `StopReason`、`EventKind`、`Message`）写全所有变体，不用 `_ =>` 兜底，新增变体时编译器会指出所有要改的地方。

## MSRV 与依赖的 API

- 1.88 起可以用 let chains（`if let Some(x) = a && x > 1`），clippy 的 `collapsible_if` 会建议这种写法。拿不准某个标准库 API 的稳定版本时，查本机源码的 `since` 标注：`grep -rnE 'since = "1\.(89|9[0-9])' ~/.rustup/toolchains/stable-*/lib/rustlib/src/rust/library/`。
- 依赖的 API 以 `Cargo.lock` 锁定的版本（`cargo tree -i reqwest`）和本项目启用的 feature 为准：各 crate 给 reqwest 只启用 `json`、`stream`、`rustls`，代码不用 `reqwest::blocking`（它只在依赖 `zx-app` 的构建里出现，由追踪导出的 `opentelemetry-otlp` 的 `reqwest-blocking-client` 打开）；tokio 没有启用 `io-std`，没有 `tokio::io::stdin`。
- 根目录 `Cargo.toml` 的 `[workspace.dependencies]` 给 serde_json 打开了 `preserve_order`，每个 crate 的 `serde_json::Map` 都按插入序，单独编译一个 crate 时也一样；比较 JSON 字符串的断言按写入顺序写。
