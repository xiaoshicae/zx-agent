---
paths:
  - "**/*.rs"
  - "Cargo.toml"
---

# 08 依赖与 MSRV

## MSRV 1.88（edition 2024）

本机默认工具链比 1.88 新，用了更新的特性也能编译通过，所以要专门检查：

- **标准库 API**：clippy 的 `incompatible_msrv` 默认开启，会对照 `rust-version` 报警（已实测：`str::floor_char_boundary` 报 "stable since 1.91.0"）。
- **语言特性**：clippy 查不出。用本机已安装的 1.88.0 工具链验证：`cargo +1.88.0 check --all-targets`。
- let chains（`if let Some(x) = a && x > 1`）从 1.88 起可用；clippy 的 `collapsible_if` 会建议把嵌套的 `if let` 合并成这种写法。
- 拿不准某个 API 的稳定版本时，查本机标准库源码的 `since` 标注：`grep -rnE 'since = "1\.(89|9[0-9])' ~/.rustup/toolchains/stable-*/lib/rustlib/src/rust/library/`。
- edition 2024 中 `std::env::set_var` 和 `remove_var` 是 `unsafe`，不要使用。

## crate API 以锁定的版本为准

- 版本看 `Cargo.lock`（`cargo tree -i reqwest`）。拿不准就查 `~/.cargo/registry/src/*/<crate>-<版本>/` 的源码和 `Cargo.toml` 的 `[features]`，不要猜。
- 启用的 feature 以本项目的 `Cargo.toml` 为准：reqwest 只启用了 `json`、`stream`、`rustls`，所以没有 `reqwest::blocking`；tokio 没有启用 `io-std`，所以没有 `tokio::io::stdin`。

## 新增或升级依赖

先征得用户同意（红线 5）。

- **rustls 只能有一个加密后端**：现在只启用 `aws-lc-rs`，来自 reqwest 0.13 的 `rustls` feature。新依赖如果再打开 `ring`，rustls 就选不出进程默认的后端，`wss://` 连接会 panic（tokio-tungstenite 使用进程默认的后端，`rustls-0.23.46/src/crypto/mod.rs:243-257`）。引入或升级依赖后，用 `cargo tree -f '{p} {f}' -i rustls` 核对；`tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried` 会因此失败。已获授权、按阶段引入的依赖见 `docs/ROADMAP.md` 的决策 D2，引入时仍要说明用在哪个阶段。提议其他依赖时说明四点：标准库和现有依赖为什么做不到；维护状况和传递依赖数量（`cargo tree -p <crate>`）；能否关掉默认 feature；它的 MSRV 是否不高于 1.88。同意后用 `cargo add` 添加，检查 `Cargo.toml` 和 `Cargo.lock` 的改动，再跑 CLAUDE.md 中的三条命令。
