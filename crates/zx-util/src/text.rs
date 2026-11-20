//! Text for errors, messages and terminal output: bounded by characters or bytes, and
//! amounts that keep their first digits however small.

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

/// `text` when it fits in `max_bytes`, else its longest start that does, ending on a
/// character boundary, followed by the note `note` gives for the bytes kept and the bytes in
/// all.
pub fn prefix_noted(text: &str, max_bytes: usize, note: impl FnOnce(usize, usize) -> String) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let kept = prefix_within(text, max_bytes);
    format!("{kept}{}", note(kept.len(), text.len()))
}

/// `value` with four decimals, and for a value below a thousandth as many more as its first
/// two digits need, up to eight. A cost, for one: a request to a low-cost model costs about a
/// ten-thousandth of a dollar, which four decimals show as nothing.
pub fn amount(value: f64) -> String {
    let mut decimals = 4;
    // The value in units of the last decimal shown.
    let mut shown = value * 10_000.0;
    while value > 0.0 && shown < 10.0 && decimals < MAX_AMOUNT_DECIMALS {
        shown *= 10.0;
        decimals += 1;
    }
    format!("{value:.decimals$}")
}

/// The most decimals [`amount`] shows.
const MAX_AMOUNT_DECIMALS: usize = 8;

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

    #[test]
    fn a_noted_prefix_keeps_short_text_whole_and_notes_a_cut_with_its_counts() {
        let note = |kept: usize, total: usize| format!(" [cut at {kept} of {total} bytes]");
        assert_eq!(prefix_noted("short", 10, note), "short");
        assert_eq!(prefix_noted("ab中cd", 3, note), "ab [cut at 2 of 7 bytes]");
    }

    #[test]
    fn an_amount_below_a_thousandth_keeps_its_first_two_digits() {
        // One request to a model at $0.10 per million input tokens.
        assert_eq!(amount(0.000_101_1), "0.00010");
        assert_eq!(amount(0.000_059_6), "0.000060");
        assert_eq!(amount(0.000_000_000_001), "0.00000000");
        assert_eq!(amount(0.0), "0.0000");
        // A thousandth and more: four decimals.
        assert_eq!(amount(0.001), "0.0010");
        assert_eq!(amount(0.0123), "0.0123");
        assert_eq!(amount(1.5), "1.5000");
    }
}
