//! Bounding text for errors, messages and terminal output. Shared by every layer, and depends
//! on nothing else in the crate.

/// The first `max_chars` characters of `text`, followed by "..." when more were left out.
pub fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

/// The longest start of `text` that fits in `max_bytes` and ends on a character boundary.
pub fn prefix_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_characters_and_marks_what_it_leaves_out() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("界面ab", 2), "界面...");
    }

    #[test]
    fn a_prefix_never_splits_a_character() {
        assert_eq!(prefix_within("abc", 10), "abc");
        // "界" is three bytes: four bytes hold one of them, not one and a third.
        assert_eq!(prefix_within("界界", 4), "界");
        assert_eq!(prefix_within("界", 2), "");
    }
}
