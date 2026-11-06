//! Exact-text replacement for `edit_file`, with a tolerant fallback, and the unified patch of
//! a change (pi: coding-agent/src/core/tools/edit-diff.ts).

use serde::{Deserialize, Serialize};

/// One replacement the model asked for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// The byte order mark, which the model never writes: it is kept aside while matching.
const BOM: char = '\u{feff}';

/// Lines of context around each change in a patch, as `diff -u` writes.
const CONTEXT_LINES: usize = 3;

/// The most line insertions and deletions the patch search looks for. Past it, the changed
/// region is shown as removed and added whole: the search costs time and memory that grow
/// with the square of this number.
const MAX_DIFF_EDITS: usize = 500;

/// `text` without a leading byte order mark, and the mark if there was one.
pub(crate) fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (&text[..BOM.len_utf8()], rest),
        None => ("", text),
    }
}

/// `"\r\n"` when the first line ends with it, otherwise `"\n"` (pi: `detectLineEnding`).
pub(crate) fn line_ending(text: &str) -> &'static str {
    match text.find('\n') {
        Some(index) if index > 0 && text.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Every `\r\n` and lone `\r` as `\n`.
pub(crate) fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `text`, whose lines end with `\n`, with `ending` instead.
pub(crate) fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_string()
    } else {
        text.replace('\n', ending)
    }
}

/// The form two texts are compared in when they do not match exactly: trailing whitespace
/// removed from every line, typographic quotes, dashes and spaces replaced by their ASCII
/// forms. Unlike pi, no Unicode NFKC normalization: the standard library has none. Keeps
/// the number of lines.
fn tolerant(text: &str) -> String {
    let trimmed: Vec<&str> = text.split('\n').map(str::trim_end).collect();
    trimmed
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// One matched replacement: `len` bytes at `start` of the text matched against.
struct Replacement<'a> {
    edit: usize,
    start: usize,
    len: usize,
    new_text: &'a str,
}

/// Where `old` is in `content`: exactly, or else in the tolerant forms of both, which
/// `content` then must already be in.
fn find(content: &str, old: &str) -> Option<(usize, usize)> {
    if let Some(start) = content.find(old) {
        return Some((start, old.len()));
    }
    let old = tolerant(old);
    content.find(&old).map(|start| (start, old.len()))
}

/// Name an edit in an error, as pi does: "the text" for a single edit.
fn which(index: usize, total: usize) -> String {
    if total == 1 {
        "the text".to_string()
    } else {
        format!("edits[{index}]")
    }
}

/// Apply `edits` to `content`, whose lines end with `\n` and which has no byte order mark.
/// Every edit is matched against the original content, must match once, and must not
/// overlap another (pi: `applyEditsToNormalizedContent`). When one only matches in the
/// tolerant form, all are matched in that form; the lines a replacement touches are then
/// written from the tolerant form and every other line keeps its original bytes. Errors
/// tell the model how to fix the call.
pub(crate) fn apply_edits(content: &str, edits: &[Edit], path: &str) -> Result<String, String> {
    if edits.is_empty() {
        return Err("edits must contain at least one replacement".into());
    }
    let edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();
    let total = edits.len();
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("{} has an empty oldText in {path}", which(index, total)));
        }
    }
    let tolerant_content = tolerant(content);
    let needs_tolerance = edits
        .iter()
        .any(|edit| !content.contains(&edit.old_text) && tolerant_content.contains(&tolerant(&edit.old_text)));
    let base = if needs_tolerance {
        tolerant_content.as_str()
    } else {
        content
    };

    let mut replacements = Vec::with_capacity(total);
    for (index, edit) in edits.iter().enumerate() {
        let Some((start, len)) = find(base, &edit.old_text) else {
            return Err(format!(
                "could not find {} in {path}; oldText must match the file exactly, including whitespace and line breaks",
                which(index, total)
            ));
        };
        let occurrences = tolerant_content.matches(&tolerant(&edit.old_text)).count();
        if occurrences > 1 {
            return Err(format!(
                "found {occurrences} occurrences of {} in {path}; oldText must be unique, so include more of the surrounding text",
                which(index, total)
            ));
        }
        replacements.push(Replacement {
            edit: index,
            start,
            len,
            new_text: &edit.new_text,
        });
    }
    replacements.sort_by_key(|replacement| replacement.start);
    for pair in replacements.windows(2) {
        if pair[0].start + pair[0].len > pair[1].start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}; merge them into one edit or target separate regions",
                pair[0].edit, pair[1].edit
            ));
        }
    }

    let changed = replace_touched_lines(content, base, &replacements);
    if changed == content {
        return Err(format!(
            "no changes made to {path}: the replacement produced identical content"
        ));
    }
    Ok(changed)
}

/// `original` with the lines that `replacements`, matched in `base`, touch rewritten from
/// `base`; every other line keeps its original bytes. `base` is `original` itself or its
/// tolerant form, which has the same lines (pi: `applyReplacementsPreservingUnchangedLines`).
fn replace_touched_lines(original: &str, base: &str, replacements: &[Replacement<'_>]) -> String {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in base.split_inclusive('\n') {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    // Groups of whole base lines, each with the replacements inside it.
    let mut groups: Vec<(usize, usize, Vec<&Replacement<'_>>)> = Vec::new();
    for replacement in replacements {
        let end = replacement.start + replacement.len;
        let first = spans
            .iter()
            .position(|&(start, stop)| replacement.start >= start && replacement.start < stop)
            .unwrap_or(spans.len().saturating_sub(1));
        let last = spans[first..]
            .iter()
            .position(|&(_, stop)| stop >= end)
            .map_or(spans.len(), |index| first + index + 1);
        match groups.last_mut() {
            Some(group) if first < group.1 => {
                group.1 = group.1.max(last);
                group.2.push(replacement);
            }
            _ => groups.push((first, last, vec![replacement])),
        }
    }
    let mut result = String::with_capacity(original.len());
    let mut next_line = 0;
    for (first, last, group) in groups {
        for line in &original_lines[next_line.min(first)..first] {
            result.push_str(line);
        }
        let (group_start, group_end) = (spans[first].0, spans[last - 1].1);
        let mut cursor = group_start;
        for replacement in group {
            result.push_str(&base[cursor..replacement.start]);
            result.push_str(replacement.new_text);
            cursor = replacement.start + replacement.len;
        }
        result.push_str(&base[cursor..group_end]);
        next_line = last;
    }
    for line in original_lines.iter().skip(next_line) {
        result.push_str(line);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// A line-by-line edit script from `old` to `new`, with the line each operation concerns.
fn line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(Op, &'a str)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);
    let middle = myers(a, b).unwrap_or_else(|| {
        a.iter()
            .map(|line| (Op::Delete, *line))
            .chain(b.iter().map(|line| (Op::Insert, *line)))
            .collect()
    });
    old[..prefix]
        .iter()
        .map(|line| (Op::Equal, *line))
        .chain(middle)
        .chain(old[old.len() - suffix..].iter().map(|line| (Op::Equal, *line)))
        .collect()
}

/// `n` as a signed index. A slice has at most `isize::MAX` elements, so the lengths of the
/// inputs and every index into them fit.
fn signed(n: usize) -> isize {
    isize::try_from(n).expect("a slice length fits in isize")
}

/// `n` as an index; the diagonal bounds keep it at or above zero.
fn unsigned(n: isize) -> usize {
    usize::try_from(n).expect("the diagonal bounds keep indices at or above zero")
}

/// The shortest edit script by Myers' greedy algorithm, or `None` when it needs more than
/// [`MAX_DIFF_EDITS`] insertions and deletions.
fn myers<'a>(a: &[&'a str], b: &[&'a str]) -> Option<Vec<(Op, &'a str)>> {
    let (n, m) = (a.len(), b.len());
    let limit = MAX_DIFF_EDITS.min(n + m);
    // Diagonal k = x - y lies in [-limit - 1, limit + 1]; index it from zero.
    let offset = limit + 1;
    let index = |k: isize| -> usize {
        // k is never below -(limit + 1), so the sum is never negative.
        unsigned(k + signed(offset))
    };
    let mut v = vec![0usize; 2 * limit + 3];
    let mut trace = Vec::new();
    for d in 0..=limit {
        trace.push(v.clone());
        let d = signed(d);
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) {
                v[index(k + 1)]
            } else {
                v[index(k - 1)] + 1
            };
            // On diagonal k, y = x - k, and the bounds above keep it within 0..=m.
            let mut y = unsigned(signed(x) - k);
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[index(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(a, b, &trace, index));
            }
            k += 2;
        }
    }
    None
}

/// Walk `trace` back from the end to the start, collecting the operations.
fn backtrack<'a>(
    a: &[&'a str],
    b: &[&'a str],
    trace: &[Vec<usize>],
    index: impl Fn(isize) -> usize,
) -> Vec<(Op, &'a str)> {
    let (mut x, mut y) = (a.len(), b.len());
    let mut ops = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = signed(d);
        let k = signed(x) - signed(y);
        let previous_k = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = v[index(previous_k)];
        let previous_y = signed(previous_x) - previous_k;
        while x > previous_x && signed(y) > previous_y {
            ops.push((Op::Equal, a[x - 1]));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == previous_x {
                ops.push((Op::Insert, b[y - 1]));
            } else {
                ops.push((Op::Delete, a[x - 1]));
            }
        }
        x = previous_x;
        y = unsigned(previous_y.max(0));
    }
    ops.reverse();
    ops
}

/// A unified patch from `old` to `new` with [`CONTEXT_LINES`] lines of context, headed by
/// `path`, and the first changed line of `new` (1-based). `None` when nothing changed.
pub(crate) fn unified_patch(path: &str, old: &str, new: &str) -> Option<(String, usize)> {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = line_diff(&old_lines, &new_lines);
    let changes: Vec<usize> = (0..ops.len()).filter(|&i| ops[i].0 != Op::Equal).collect();
    let (&first_change, _) = changes.split_first()?;
    // Lines of each side before each operation.
    let mut positions = Vec::with_capacity(ops.len() + 1);
    let (mut old_line, mut new_line) = (0, 0);
    for (op, _) in &ops {
        positions.push((old_line, new_line));
        match op {
            Op::Equal => {
                old_line += 1;
                new_line += 1;
            }
            Op::Delete => old_line += 1,
            Op::Insert => new_line += 1,
        }
    }
    positions.push((old_line, new_line));

    let mut patch = format!("--- {path}\n+++ {path}\n");
    let mut cluster_start = 0;
    for i in 0..changes.len() {
        let last = i + 1 == changes.len();
        if !last && changes[i + 1] - changes[i] <= 2 * CONTEXT_LINES + 1 {
            continue;
        }
        let start = changes[cluster_start].saturating_sub(CONTEXT_LINES);
        let end = (changes[i] + CONTEXT_LINES + 1).min(ops.len());
        let hunk = &ops[start..end];
        let old_count = hunk.iter().filter(|(op, _)| *op != Op::Insert).count();
        let new_count = hunk.iter().filter(|(op, _)| *op != Op::Delete).count();
        // An empty side names the line before it, as `diff -u` does.
        let old_start = positions[start].0 + usize::from(old_count > 0);
        let new_start = positions[start].1 + usize::from(new_count > 0);
        patch.push_str(&format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"));
        for (op, line) in hunk {
            patch.push(match op {
                Op::Equal => ' ',
                Op::Delete => '-',
                Op::Insert => '+',
            });
            patch.push_str(line);
            if !line.ends_with('\n') {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
        cluster_start = i + 1;
    }
    Some((patch, positions[first_change].1 + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn exact_replacements_are_matched_against_the_original() {
        let content = "alpha\nbeta\ngamma\n";
        let changed = apply_edits(content, &[edit("gamma", "GAMMA"), edit("alpha", "beta")], "f").unwrap();
        // The second edit's new text equals an existing line; matching stays on the original.
        assert_eq!(changed, "beta\nbeta\nGAMMA\n");
    }

    #[test]
    fn missing_duplicate_empty_and_overlapping_edits_are_refused() {
        let content = "one\ntwo\ntwo\nthree\n";
        let cases = [
            (vec![edit("four", "x")], "could not find the text in f"),
            (vec![edit("two", "x")], "found 2 occurrences of the text in f"),
            (vec![edit("one", "x"), edit("", "y")], "edits[1] has an empty oldText"),
            (
                vec![edit("one\ntwo\ntwo", "x"), edit("two\nthree", "y")],
                "edits[0] and edits[1] overlap",
            ),
            (vec![edit("one", "one")], "no changes made to f"),
            (vec![], "at least one replacement"),
        ];
        for (edits, expected) in cases {
            let error = apply_edits(content, &edits, "f").unwrap_err();
            assert!(error.contains(expected), "{expected:?}: {error}");
        }
    }

    #[test]
    fn tolerant_matching_rewrites_only_the_lines_it_touches() {
        // Trailing spaces and typographic quotes in the file, plain ones in the edit.
        let content = "keep  \nsay \u{201c}hi\u{201d}   \nalso keep\u{a0}\n";
        let changed = apply_edits(content, &[edit("say \"hi\"", "say \"bye\"")], "f").unwrap();
        assert_eq!(changed, "keep  \nsay \"bye\"\nalso keep\u{a0}\n");
    }

    #[test]
    fn duplicates_are_counted_in_the_tolerant_form() {
        let content = "x = 1  \nx = 1\n";
        let error = apply_edits(content, &[edit("x = 1", "x = 2")], "f").unwrap_err();
        assert!(error.contains("found 2 occurrences"), "{error}");
    }

    #[test]
    fn edits_with_crlf_match_lf_content() {
        let changed = apply_edits("a\nb\n", &[edit("a\r\nb", "c\r\nd")], "f").unwrap();
        assert_eq!(changed, "c\nd\n");
    }

    #[test]
    fn line_endings_and_byte_order_marks_round_trip() {
        let raw = "\u{feff}one\r\ntwo\r\n";
        let (bom, text) = split_bom(raw);
        assert_eq!(bom, "\u{feff}");
        let ending = line_ending(text);
        assert_eq!(ending, "\r\n");
        let changed = apply_edits(&normalize_to_lf(text), &[edit("two", "2")], "f").unwrap();
        assert_eq!(
            format!("{bom}{}", restore_line_endings(&changed, ending)),
            "\u{feff}one\r\n2\r\n"
        );
        assert_eq!(line_ending("no newline"), "\n");
        assert_eq!(split_bom("plain"), ("", "plain"));
    }

    #[test]
    fn multibyte_text_is_replaced_at_character_boundaries() {
        let changed = apply_edits("名字：旧\n下一行\n", &[edit("旧", "新")], "f").unwrap();
        assert_eq!(changed, "名字：新\n下一行\n");
    }

    #[test]
    fn a_patch_shows_each_change_with_context_and_the_first_changed_line() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 5\n", "line five\n").replace("line 18\n", "");
        let (patch, first) = unified_patch("src/a.rs", &old, &new).unwrap();
        assert_eq!(first, 5);
        assert_eq!(
            patch,
            "--- src/a.rs\n+++ src/a.rs\n\
             @@ -2,7 +2,7 @@\n line 2\n line 3\n line 4\n-line 5\n+line five\n line 6\n line 7\n line 8\n\
             @@ -15,6 +15,5 @@\n line 15\n line 16\n line 17\n-line 18\n line 19\n line 20\n"
        );
    }

    #[test]
    fn nearby_changes_share_a_hunk_and_a_missing_final_newline_is_marked() {
        let (patch, first) = unified_patch("f", "a\nb\nc\nd", "A\nb\nc\nD").unwrap();
        assert_eq!(first, 1);
        assert_eq!(
            patch,
            "--- f\n+++ f\n@@ -1,4 +1,4 @@\n-a\n+A\n b\n c\n-d\n\\ No newline at end of file\n+D\n\\ No newline at end of file\n"
        );
        assert!(unified_patch("f", "same\n", "same\n").is_none());
    }

    #[test]
    fn an_added_file_and_a_cleared_file_name_the_line_before_the_empty_side() {
        let (patch, _) = unified_patch("f", "", "new\n").unwrap();
        assert!(patch.contains("@@ -0,0 +1,1 @@\n+new\n"), "{patch}");
        let (patch, _) = unified_patch("f", "old\n", "").unwrap();
        assert!(patch.contains("@@ -1,1 +0,0 @@\n-old\n"), "{patch}");
    }

    #[test]
    fn a_change_too_large_to_search_is_shown_removed_and_added_whole() {
        let old: String = (0..MAX_DIFF_EDITS).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..MAX_DIFF_EDITS).map(|i| format!("new {i}\n")).collect();
        let (patch, first) = unified_patch("f", &old, &new).unwrap();
        assert_eq!(first, 1);
        assert_eq!(patch.matches("\n-old").count(), MAX_DIFF_EDITS);
        assert_eq!(patch.matches("\n+new").count(), MAX_DIFF_EDITS);
    }

    /// The length of the longest common subsequence, by the textbook table.
    fn lcs(a: &[&str], b: &[&str]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                table[i][j] = if a[i - 1] == b[j - 1] {
                    table[i - 1][j - 1] + 1
                } else {
                    table[i - 1][j].max(table[i][j - 1])
                };
            }
        }
        table[a.len()][b.len()]
    }

    #[test]
    fn the_diff_is_minimal_and_rebuilds_both_sides_for_many_inputs() {
        // A fixed linear congruential sequence: the same cases on every run.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        let alphabet = ["a\n", "b\n", "c\n", "d"];
        for _ in 0..2000 {
            let old: Vec<&str> = (0..next(9))
                .map(|_| alphabet[usize::try_from(next(4)).unwrap()])
                .collect();
            let new: Vec<&str> = (0..next(9))
                .map(|_| alphabet[usize::try_from(next(4)).unwrap()])
                .collect();
            let ops = line_diff(&old, &new);
            let side = |skip: Op| -> Vec<&str> { ops.iter().filter(|(op, _)| *op != skip).map(|(_, l)| *l).collect() };
            assert_eq!(side(Op::Insert), old, "{old:?} -> {new:?}");
            assert_eq!(side(Op::Delete), new, "{old:?} -> {new:?}");
            let edits = ops.iter().filter(|(op, _)| *op != Op::Equal).count();
            assert_eq!(edits, old.len() + new.len() - 2 * lcs(&old, &new), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn the_diff_finds_the_shortest_script() {
        let old = ["a\n", "b\n", "c\n", "a\n", "b\n", "b\n", "a\n"];
        let new = ["c\n", "b\n", "a\n", "b\n", "a\n", "c\n"];
        let ops = line_diff(&old, &new);
        let edits = ops.iter().filter(|(op, _)| *op != Op::Equal).count();
        // The classic example from Myers' paper: five insertions and deletions.
        assert_eq!(edits, 5);
        let rebuilt: Vec<&str> = ops
            .iter()
            .filter(|(op, _)| *op != Op::Delete)
            .map(|(_, l)| *l)
            .collect();
        assert_eq!(rebuilt, new);
        let original: Vec<&str> = ops
            .iter()
            .filter(|(op, _)| *op != Op::Insert)
            .map(|(_, l)| *l)
            .collect();
        assert_eq!(original, old);
    }
}
