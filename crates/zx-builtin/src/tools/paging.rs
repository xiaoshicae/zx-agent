//! Text that a tool returns a page at a time. Each page but the last ends with a footer that
//! names the tool, the argument that names the text, and the UTF-8 byte offset to read on
//! from. read_output pages the results the output store keeps this way, and a host's own tool
//! can page a file of this machine with [`read_text`] and a [`Pager`], as zx-app's read_skill
//! does.

use std::path::Path;

use super::truncate::{MAX_LINES, truncate_head};
pub use crate::files::Links;
use crate::files::{Unreadable, read_regular};

/// How a tool returns long text a page at a time.
///
/// A page is whole lines while they fit: at most [`MAX_LINES`] of them, and at most
/// `max_bytes` together with the footer. A line too long for a page is cut at a character
/// boundary, and the next page goes on from there. Text that fits is returned whole, without
/// a footer.
#[derive(Clone, Copy, Debug)]
pub struct Pager<'a> {
    /// The tool the model calls to read on, such as `read_output`.
    pub tool: &'a str,
    /// The argument of that tool that names the text, and its value, such as
    /// `("id", "<output ID>")`.
    pub argument: (&'a str, &'a str),
    /// The most bytes of a page, footer included. A page holds at least one character, even
    /// when that and the footer are more.
    pub max_bytes: usize,
}

impl Pager<'_> {
    /// The page of `text` that starts at byte `offset`, with a footer that tells the model how
    /// to read on when more follows. Errors, written for the model: an offset past the end of
    /// `text` or inside a character.
    pub fn page(&self, text: &str, offset: usize) -> Result<String, String> {
        let rest = text.get(offset..).ok_or_else(|| {
            format!(
                "offset {offset} is out of range or inside a UTF-8 character; use the offset from the footer of a page"
            )
        })?;
        let whole = truncate_head(rest, MAX_LINES, self.max_bytes);
        if !whole.truncated {
            return Ok(whole.text);
        }
        // The footer is longest with the largest offset it can name.
        let budget = self.max_bytes.saturating_sub(self.footer(text.len()).len());
        let mut page = truncate_head(rest, MAX_LINES, budget).text;
        if page.is_empty() {
            // Every page moves on, however small the budget.
            page = rest.chars().take(1).collect();
        }
        let footer = self.footer(offset + page.len());
        page.push_str(&footer);
        Ok(page)
    }

    /// The footer that tells the model to read on from byte `next`.
    fn footer(&self, next: usize) -> String {
        let (name, value) = self.argument;
        format!("\n[use {} with {name}={value:?}, offset={next} to continue]", self.tool)
    }
}

/// The text of the file at `path` on this machine, read to be paged: a regular UTF-8 file of
/// at most `max_bytes`, opened without waiting for the writer of a FIFO, and with a symbolic
/// link in its place followed or refused as `links` says. Refuse links for a file in a
/// directory that commands can write.
///
/// Errors, written for the model, name the path; the caller says what it was reading, as in
/// `cannot read tool output: <error>`. Callers that must respond to cancellation race this
/// future against their token.
pub async fn read_text(path: &Path, max_bytes: u64, links: Links) -> Result<String, String> {
    let bytes = read_regular(path, max_bytes, links)
        .await
        .map_err(|unreadable| match unreadable {
            Unreadable::Io(error) => format!("{}: {error}", path.display()),
            Unreadable::Link => format!("{} is a symbolic link, which is not followed", path.display()),
            Unreadable::Directory | Unreadable::Special => format!("{} is not a regular file", path.display()),
            Unreadable::Over => format!("{} is over the {max_bytes} byte limit", path.display()),
        })?;
    String::from_utf8(bytes).map_err(|error| format!("{} is not UTF-8 text: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager(max_bytes: usize) -> Pager<'static> {
        Pager {
            tool: "read_text",
            argument: ("name", "notes"),
            max_bytes,
        }
    }

    /// The text of `page` and the offset its footer names, if it has one.
    fn split(page: &str) -> (&str, Option<usize>) {
        match page.split_once("\n[use read_text with name=\"notes\", offset=") {
            Some((text, footer)) => (text, footer.split(' ').next().and_then(|next| next.parse().ok())),
            None => (page, None),
        }
    }

    #[test]
    fn text_that_fits_is_returned_whole_without_a_footer() {
        assert_eq!(pager(100).page("one\ntwo\n", 0).unwrap(), "one\ntwo\n");
        assert_eq!(pager(100).page("one\ntwo\n", 4).unwrap(), "two\n");
        assert_eq!(pager(100).page("one", 3).unwrap(), "");
    }

    #[test]
    fn pages_are_whole_lines_that_fit_with_their_footer_and_read_on_without_loss() {
        let text: String = (0..40).map(|line| format!("line {line}\n")).collect();
        let pager = pager(120);
        let (mut joined, mut offset) = (String::new(), 0);
        loop {
            let page = pager.page(&text, offset).unwrap();
            assert!(page.len() <= 120, "{} bytes: {page}", page.len());
            let (lines, next) = split(&page);
            joined.push_str(lines);
            let Some(next) = next else { break };
            assert!(lines.ends_with('\n'), "{lines:?}");
            assert_eq!(next, offset + lines.len());
            offset = next;
        }
        assert_eq!(joined, text);
    }

    #[test]
    fn a_line_longer_than_a_page_is_cut_at_a_character_boundary() {
        let text = format!("{}\nend", "界".repeat(100));
        let page = pager(120).page(&text, 0).unwrap();
        let (cut, next) = split(&page);
        assert!(text.starts_with(cut) && !cut.is_empty(), "{cut:?}");
        assert_eq!(next, Some(cut.len()));
        assert!(page.len() <= 120, "{} bytes", page.len());
    }

    #[test]
    fn a_page_holds_at_most_the_line_limit() {
        let text = "\n".repeat(MAX_LINES + 1);
        let page = pager(usize::MAX).page(&text, 0).unwrap();
        assert_eq!(split(&page), ("\n".repeat(MAX_LINES).as_str(), Some(MAX_LINES)));
    }

    #[test]
    fn every_page_moves_on_however_small_the_budget() {
        let page = pager(1).page("界界", 0).unwrap();
        assert_eq!(split(&page), ("界", Some(3)));
    }

    #[test]
    fn an_offset_past_the_end_or_inside_a_character_is_refused() {
        for offset in [1, 7] {
            let error = pager(100).page("界\n", offset).unwrap_err();
            assert!(error.contains("UTF-8 character"), "{offset}: {error}");
        }
    }

    // Opening a directory to inspect it, as the reader does, is Unix behavior.
    #[cfg(unix)]
    #[tokio::test]
    async fn files_that_cannot_be_paged_are_refused_with_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let large = dir.path().join("large.txt");
        std::fs::write(&large, "x".repeat(11)).unwrap();
        let binary = dir.path().join("binary.txt");
        std::fs::write(&binary, [0xff]).unwrap();
        for (path, expected) in [
            (dir.path().join("missing.txt"), "missing.txt: "),
            (dir.path().to_path_buf(), "is not a regular file"),
            (large, "large.txt is over the 10 byte limit"),
            (binary, "binary.txt is not UTF-8 text"),
        ] {
            let error = read_text(&path, 10, Links::Follow).await.unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_is_followed_or_refused_as_asked() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "linked").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_text(&link, 10, Links::Follow).await.unwrap(), "linked");
        let error = read_text(&link, 10, Links::Refuse).await.unwrap_err();
        assert_eq!(
            error,
            format!("{} is a symbolic link, which is not followed", link.display())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        crate::test_support::make_fifo(&fifo);

        // A read that waited for a writer would never end: nothing writes.
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), read_text(&fifo, 10, Links::Follow)).await;

        let error = read.expect("the read waited for a writer").unwrap_err();
        assert!(error.ends_with("is not a regular file"), "{error}");
    }
}
