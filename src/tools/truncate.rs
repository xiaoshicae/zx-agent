//! Output truncation (pi: truncateHead / truncateTail). Tools must bound what they return,
//! or large outputs overflow the model context.

/// Default line limit for tool output (pi: DEFAULT_MAX_LINES).
pub const MAX_LINES: usize = 2000;
/// Default byte limit for tool output (pi: DEFAULT_MAX_BYTES).
pub const MAX_BYTES: usize = 50 * 1024;

/// Truncated text and what was cut.
#[derive(Debug, PartialEq, Eq)]
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub total_lines: usize,
    /// Lines included, counting a line that was cut short.
    pub kept_lines: usize,
}

/// Keep the beginning. Suits file reads and search results.
pub fn truncate_head(text: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut kept = 0;
    let mut cut = false;
    for line in &lines {
        if kept >= max_lines {
            break;
        }
        if out.len() + line.len() > max_bytes {
            if kept == 0 {
                out.push_str(prefix_within(line, max_bytes));
                kept = 1;
                cut = true;
            }
            break;
        }
        out.push_str(line);
        kept += 1;
    }
    Truncated {
        text: out,
        truncated: cut || kept < lines.len(),
        total_lines: lines.len(),
        kept_lines: kept,
    }
}

/// Keep the end. Suits command output and logs, where the latest lines matter most.
pub fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut kept_rev: Vec<&str> = Vec::new();
    let mut bytes = 0;
    let mut cut = false;
    for line in lines.iter().rev() {
        if kept_rev.len() >= max_lines {
            break;
        }
        if bytes + line.len() > max_bytes {
            if kept_rev.is_empty() {
                kept_rev.push(suffix_within(line, max_bytes));
                cut = true;
            }
            break;
        }
        bytes += line.len();
        kept_rev.push(line);
    }
    let kept = kept_rev.len();
    kept_rev.reverse();
    Truncated {
        text: kept_rev.concat(),
        truncated: cut || kept < lines.len(),
        total_lines: lines.len(),
        kept_lines: kept,
    }
}

fn prefix_within(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn suffix_within(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_and_tail_respect_line_limits() {
        let text = "1\n2\n3\n4\n";
        let head = truncate_head(text, 2, 100);
        assert_eq!(
            (head.text.as_str(), head.truncated, head.kept_lines),
            ("1\n2\n", true, 2)
        );
        let tail = truncate_tail(text, 2, 100);
        assert_eq!(
            (tail.text.as_str(), tail.truncated, tail.kept_lines),
            ("3\n4\n", true, 2)
        );
        assert!(!truncate_head(text, 10, 100).truncated);
    }

    #[test]
    fn byte_limits_keep_whole_lines_and_char_boundaries() {
        assert_eq!(truncate_tail("aaaa\nbb\n", 10, 4).text, "bb\n");
        let long = "中".repeat(10);
        let head = truncate_head(&long, 10, 7);
        assert_eq!(head.text, "中中");
        assert!(head.truncated);
        assert_eq!(truncate_tail(&long, 10, 7).text, "中中");
    }
}
