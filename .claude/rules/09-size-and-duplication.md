---
paths:
  - "**/*.rs"
---

# 09 尺寸与重复

## 尺寸

- **函数一屏**：目标 60 行以内，一个函数做一件事；写不出一句话文档的函数在做两件事。拆出来的函数按步骤命名（`load_agent`、`open_session`、`set_up`），不叫 `helper`、`do_x`。
- **超过 80 行（clippy 的 `too_many_lines`）只有三种情况可以带 `#[expect(clippy::too_many_lines, reason = "...")]`**：按变体展开、每个分支一两行的 match（`one arm per event variant`）；一张数据表（`one event of every variant`）；测试里一段脚本化的场景（`a scripted conversation, one round per arm`）。
- **文件一个主题**：一个文件讲几件事，就按职责拆成几个子模块（`transcript/compaction.rs` 与 `transcript/compaction/{records,source}.rs`），不论多长；不按类型拆（不把类型和它的 impl 分进两个文件）。入口文件只分派。产线代码（不含测试）超过 600 行时，确认它只讲一个主题。
- **父模块的文档是地图**：有子模块的入口文件，在 `//!` 里每个子模块写一行，说它管什么（参照 `crates/zx-core/src/transcript.rs`、`runner.rs`）。新增、改名或删除子模块时同步改这几行。
- **测试跟着被测的代码走**：单元测试写在它所测代码的模块里，不集中在父模块。一个模块的测试超过约 300 行时，移到 `foo/tests.rs`，`foo.rs` 里只留 `#[cfg(test)] mod tests;`。几个子模块的测试共用的辅助放进父模块的 `test_support.rs`。

## 重复

- **同样的 5 行以上出现第二次就提取**，第三次必须提取，前提是提取出来的比重复的更好懂：为此要引入回调、装箱的 future 或每处一层转调时，保留重复并在一处注明。提取出来的放在它所属那一层的入口文件：工具共用的在 `tools.rs`（`parse_args`、`resolve`、`unless_cancelled`），平台共用的在 zx-im 的 `platform.rs`（`Pacer`、`Seen`、`rate_limit_wait`、`next_message`），没有业务含义、几个 crate 都用的在 `zx-util`（准入条件见 CLAUDE.md 的 crate 表），测试共用的见规则 06。
- **一处定义，别处引用**：常量、错误前缀（`fault`）、超时加上报（`Shared::bounded`）、结束事件（`StreamEvent::ending`）都只写一份。
- **不横向引用**：同一层的模块互不依赖，如 zx-cli 的各子命令、zx-im 的各平台（`platform::slack` 与 ⚑ D:5/CHINA FLAG: `platform::feishu`，内部的项只对本模块可见，共用的在 `platform.rs`）；要共用就下沉到共同的下层。需要引用上层时，说明分层或接口该改了。

提交前对照 diff 过一遍 `.github/pull_request_template.md` 的“自查”。
