## 变更说明

<!-- 改了什么、为什么；设计有变化时写明路线图中对应的条目。 -->

## 自查（规则 01、09）

- [ ] 超过 80 行的新函数属于规则 09 允许 `#[expect(clippy::too_many_lines)]` 的三种情况
- [ ] 没有新抄的第二份
- [ ] 新名字与仓库里的同义词一致，没有同名不同义
- [ ] 搬过的代码带着文档；`pub` 最小

## 验证

- [ ] `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 通过，附上 `test result` 原始输出
- [ ] 用到新语法或新标准库 API 时，`cargo +1.88.0 check --all-targets` 通过
- [ ] 修 bug 时先补了回归测试，并确认它在修复前失败

## 路线图

- [ ] 设计有变化时，先更新了 `docs/ROADMAP.md` 再改代码
- [ ] 完成的阶段已在 `docs/ROADMAP.md` 的“当前阶段”中改为已完成，留下的缺口记在“待办与已知缺口”
