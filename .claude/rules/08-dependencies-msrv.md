---
paths:
  - "**/*.rs"
  - "Cargo.toml"
---

# 08 依赖与 MSRV

## MSRV 1.85（edition 2024）

本机工具链比 1.85 新，用了新特性也能编译通过，所以要靠规则避免：

- **标准库 API**：clippy 的 `incompatible_msrv` 默认开启，会对照 `rust-version` 报警（已实测：`Vec::pop_if` 报 "stable since 1.86.0"）。
- **语言特性**：clippy 查不出，当前工具链也能编译（已实测 let chains）。常见的有：

| 特性 | 稳定版本 | 1.85 下的写法 |
|---|---|---|
| let chains：`if let Some(x) = a && x > 1` | 1.88 | 嵌套 `if let`，或者 `if let Some(x) = a.filter(\|x\| *x > 1)` |
| trait 向上转型：`&dyn Sub` 转 `&dyn Super` | 1.86 | 在 trait 里加一个返回 `&dyn Super` 的方法 |

- 拿不准时，查本机标准库源码的 `since` 标注：`grep -rn 'since = "1\.8[6-9]' ~/.rustup/toolchains/stable-*/lib/rustlib/src/rust/library/`。彻底验证要 `rustup toolchain install 1.85 && cargo +1.85 check --all-targets`，这需要网络，本机还没有安装。
- edition 2024 中 `std::env::set_var` 和 `remove_var` 是 `unsafe`，不要使用。

## crate API 以锁定的版本为准

- 版本看 `Cargo.lock`（`cargo tree -i reqwest`）。拿不准就查 `~/.cargo/registry/src/*/<crate>-<版本>/` 的源码和 `Cargo.toml` 的 `[features]`，不要猜。
- 启用的 feature 以本项目的 `Cargo.toml` 为准：reqwest 只启用了 `json`、`stream`、`rustls-tls`，所以没有 `reqwest::blocking`；tokio 没有启用 `io-std`，所以没有 `tokio::io::stdin`。

## 新增或升级依赖

先征得用户同意（红线 5）。提议时说明四点：标准库和现有依赖为什么做不到；维护状况和传递依赖数量（`cargo tree -p <crate>`）；能否关掉默认 feature；它的 MSRV 是否不高于 1.85。同意后用 `cargo add` 添加，检查 `Cargo.toml` 和 `Cargo.lock` 的改动，再跑 CLAUDE.md 中的三条命令。
