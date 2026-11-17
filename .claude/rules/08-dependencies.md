---
paths:
  - "**/Cargo.toml"
---

# 08 依赖

新增或升级依赖、修改 `rust-version` 与 `edition` 都先征得用户同意（红线 5）；已获授权的依赖见 `docs/ROADMAP.md` 的决策 D2。提议其他依赖时说明四点：标准库和现有依赖为什么做不到；维护状况和传递依赖数量（`cargo tree -p <crate>`）；能否关掉默认 feature；它的 MSRV 是否不高于 1.88。

- **版本只写一处**：外部依赖的版本与 feature 写在根目录 `Cargo.toml` 的 `[workspace.dependencies]`，各 crate 写 `<名字>.workspace = true`，只在需要额外 feature 时写成 `{ workspace = true, features = [...] }`。先把新依赖写进 `[workspace.dependencies]`，再 `cargo add -p <crate> <名字>`。
- **tokio 的 feature 按 crate 写**：`[workspace.dependencies]` 只写版本，各 crate 写自己的代码用到的 feature，只有测试用到的写进 `[dev-dependencies]`。别的依赖也会打开 tokio 的 feature，编译通过不说明写全了，要按代码里的 `tokio::` 路径核对；再单独编译每个 crate：`cargo check -p <crate>`（库与程序）、`cargo check -p <crate> --all-targets`，有 doctest 的再跑 `cargo test -p <crate> --doc`。
- **依赖加在真正用到它的 crate 上**，只在测试里用的放进 `[dev-dependencies]`。核对方法：`RUSTFLAGS="-W unused-crate-dependencies" cargo check --lib --bins`，换一个 `CARGO_TARGET_DIR`，免得冲掉平时的编译缓存。zx-cli 的代码都在库里，它的可执行文件只调用库，在那里报其余依赖"未使用"是正常的。
- **rustls 只能有一个加密后端**：现在只启用 `aws-lc-rs`，来自 reqwest 0.13 的 `rustls` feature。新依赖如果再打开 `ring`，rustls 就选不出进程默认的后端，`wss://` 连接会 panic。引入或升级依赖后，用 `cargo tree -f '{p} {f}' -i rustls` 核对；`crates/zx-agent/tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried` 会因此失败。
- 同意后检查各 `Cargo.toml` 和 `Cargo.lock` 的改动，再跑 CLAUDE.md 中的四条命令。
