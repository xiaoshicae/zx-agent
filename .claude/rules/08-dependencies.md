---
paths:
  - "**/Cargo.toml"
---

# 08 依赖

提议新依赖（红线 5）时说明四点：标准库和现有依赖为什么做不到；维护状况和传递依赖数量（`cargo tree -p <crate>`）；能否关掉默认 feature；它的 MSRV 是否不高于 1.88。

- **版本只写一处**：外部依赖的版本与 feature 写在根目录 `Cargo.toml` 的 `[workspace.dependencies]`，各 crate 写 `<名字>.workspace = true`，需要额外 feature 时写成 `{ workspace = true, features = [...] }`。先把新依赖写进 `[workspace.dependencies]`，再 `cargo add -p <crate> <名字>`。
- **tokio 的 feature 按 crate 写**：`[workspace.dependencies]` 只写版本，各 crate 写自己的代码用到的 feature，只有测试用到的写进 `[dev-dependencies]`。别的依赖也会打开 tokio 的 feature，编译通过不说明写全了，要按代码里的 `tokio::` 路径核对，并单独编译每个 crate：`cargo check -p <crate>`、`cargo check -p <crate> --all-targets`，有 doctest 的再跑 `cargo test -p <crate> --doc`。
- **依赖加在真正用到它的 crate 上**，只在测试里用的放进 `[dev-dependencies]`。核对：`RUSTFLAGS="-W unused-crate-dependencies" cargo check --lib --bins`，另设 `CARGO_TARGET_DIR`，免得冲掉平时的编译缓存；zx-cli 与 zx-im 的可执行文件只调用库，在那里报的"未使用"可以忽略。
- **rustls 只能有一个加密后端**：现在只有 reqwest 0.13 的 `rustls` feature 带来的 `aws-lc-rs`。新依赖再打开 `ring` 时，rustls 选不出进程默认的后端，`wss://` 连接会 panic，`crates/zx-im/tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried` 随之失败。引入或升级依赖后用 `cargo tree -f '{p} {f}' -i rustls` 核对。
- 获得同意后检查各 `Cargo.toml` 和 `Cargo.lock` 的改动，再跑 CLAUDE.md 中的四条命令。
