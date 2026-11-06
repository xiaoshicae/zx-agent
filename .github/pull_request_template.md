## 变更说明

<!-- 改了什么、为什么；设计有变化时说明路线图中对应的条目。 -->

## 自查（规则 09）

- [ ] 最长的新函数在 80 行内，或带着说明理由的 `#[expect(clippy::too_many_lines, reason = "...")]`
- [ ] 没有新抄的第二份；共用的放在所属层的 `mod.rs`、`zx_im::platform` 或 `tests/common`
- [ ] 新名字与仓库里的同义词一致；没有同名不同义
- [ ] 搬过的代码带着文档一起搬；`pub` 最小

## 验证

- [ ] `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 通过，并附上 `test result` 原始输出
- [ ] 用到新语法或新标准库 API 时，`cargo +1.88.0 check --all-targets` 通过
- [ ] 修 bug 时先补回归测试，并确认它在修复前失败

## 路线图

- [ ] 设计有变化时，已先更新 `docs/ROADMAP.md` 再改代码
- [ ] 本 PR 完成的阶段或修正项，状态已写为“已完成（PR #本 PR 编号）”：合并即完成，避免合并后状态滞后
