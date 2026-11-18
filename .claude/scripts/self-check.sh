#!/usr/bin/env bash
# 提交前自查：列出需要说明理由的写法。每一处命中都要能说出理由；"(无)" 表示没有命中。
# 在任何目录下运行都可以，脚本会切到项目根目录。生产代码在 crates/*/src/，测试设施 crates/zx-testing 除外；
# 集成测试在 crates/zx-agent/tests/、crates/zx-app/tests/、crates/zx-cli/tests/ 与 crates/zx-im/tests/。

cd "$(dirname "$0")/../.." || exit 1

check() {
    local title=$1
    shift
    echo "== $title"
    grep -rn "$@" || echo "(无)"
}

# 生产代码中匹配 awk 正则 $1 的行。从紧跟在 #[cfg(test)] 或 #[cfg(all(test, ...))] 后面的内联 mod（`mod tests {`）
# 开始，到文件结尾都跳过；整个 `tests.rs` 与 `test_support.rs` 也跳过，它们只以 `#[cfg(test)] mod tests;` 或
# `mod test_support;` 引入（规则 08）。这样的声明本身不跳过任何内容：它可以写在文件开头，后面还是生产代码（如
# crates/zx-core/src/transcript.rs）。单独带 #[cfg(test)] 的函数（如 crates/zx-core/src/transcript.rs 的 with_writer）
# 不跳过。测试设施 crates/zx-testing 是测试代码，整个跳过。正则经 awk -v 传入，反斜杠会被当作转义，所以用 [.] 这样的写法。
production() {
    find crates/*/src -name '*.rs' -not -path 'crates/zx-testing/*' -print0 | xargs -0 awk -v pattern="$1" '
        FNR == 1 { in_tests = (FILENAME ~ /(^|\/)(tests|test_support)\.rs$/); after_cfg_test = 0 }
        /^[[:space:]]*#\[cfg\((all\()?test/ { after_cfg_test = 1; next }
        after_cfg_test && /^[[:space:]]*(pub(\([a-z]+\))? )?mod [A-Za-z0-9_]+[[:space:]]*\{/ { in_tests = 1 }
        { after_cfg_test = 0 }
        !in_tests && $0 ~ pattern { print FILENAME ":" FNR ":" $0 }
    '
}

# 测试代码可以 unwrap（红线 2），也可以不检查结果。
check_production() {
    echo "== 生产代码里的 $1（已跳过测试模块）"
    local hits
    hits=$(production "$2")
    echo "${hits:-(无)}"
}

check 'Rc / RefCell / Box::leak / static mut / unsafe' 'Rc<\|RefCell\|Box::leak\|static mut\|unsafe ' crates/*/src/
check '#[allow] / #![allow]' -e '#\[allow' -e '#!\[allow' crates/
check_production 'unwrap()' '[.]unwrap[(][)]'
check_production 'let _ =' 'let _ ='
check '调试代码' 'dbg!\|todo!\|unimplemented!' crates/
