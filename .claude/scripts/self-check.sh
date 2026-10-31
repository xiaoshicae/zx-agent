#!/usr/bin/env bash
# 提交前自查：列出需要说明理由的写法。每一处命中都要能说出理由；"(无)" 表示没有命中。
# 在任何目录下运行都可以，脚本会切到项目根目录。

cd "$(dirname "$0")/../.." || exit 1

check() {
    local title=$1
    shift
    echo "== $title"
    grep -rn "$@" || echo "(无)"
}

# 只看生产代码：从紧跟在 #[cfg(test)] 或 #[cfg(all(test, ...))] 后面的 mod 开始，到文件结尾都跳过。
# 单独带 #[cfg(test)] 的函数（如 src/session.rs 的 with_writer）不跳过。
production_unwraps() {
    find src -name '*.rs' -print0 | xargs -0 awk '
        FNR == 1 { in_tests = 0; after_cfg_test = 0 }
        /^[[:space:]]*#\[cfg\((all\()?test/ { after_cfg_test = 1; next }
        after_cfg_test && /^[[:space:]]*(pub(\([a-z]+\))? )?mod / { in_tests = 1 }
        { after_cfg_test = 0 }
        !in_tests && /\.unwrap\(\)/ { print FILENAME ":" FNR ":" $0 }
    '
}

check 'Rc / RefCell / Box::leak / static mut / unsafe' 'Rc<\|RefCell\|Box::leak\|static mut\|unsafe ' src/
check '#[allow]' '#\[allow' src/ tests/
echo "== 生产代码里的 unwrap()（已跳过测试模块）"
unwraps=$(production_unwraps)
echo "${unwraps:-(无)}"
check 'let _ =' 'let _ =' src/
check '调试代码' 'dbg!\|todo!\|unimplemented!' src/ tests/
