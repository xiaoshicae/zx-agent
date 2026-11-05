//! Minimal Server-Sent Events parser: yields the `data` payload of each event.

/// Default maximum bytes in one SSE event, including its framing.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Incremental parser with a byte limit per event. Discard it after a parsing error.
pub struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
    event_bytes: usize,
    max_event_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_EVENT_BYTES)
    }
}

impl SseParser {
    /// Limit each event, including unterminated lines and multi-line data fields.
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            data: Vec::new(),
            event_bytes: 0,
            max_event_bytes,
        }
    }

    /// Feed raw bytes; returns the payloads of events completed by them.
    ///
    /// Only whole lines are decoded, and a newline byte never occurs inside a
    /// multi-byte UTF-8 sequence, so characters split across chunks stay intact.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        // Scan each incoming byte once; only a partial line is carried across calls.
        for part in bytes.split_inclusive(|&byte| byte == b'\n') {
            if part.len() > self.max_event_bytes.saturating_sub(self.event_bytes) {
                return Err(format!("SSE event exceeds the {} byte limit", self.max_event_bytes));
            }
            self.event_bytes += part.len();
            self.buffer.extend_from_slice(part);
            if !part.ends_with(b"\n") {
                continue;
            }
            let line = String::from_utf8_lossy(&self.buffer);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
                self.event_bytes = 0;
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // event/id/retry fields and comment lines are not needed here.
            self.buffer.clear();
        }
        Ok(events)
    }

    /// Flush an event left unterminated at end of stream.
    pub fn finish(&mut self) -> Option<String> {
        let tail = std::mem::take(&mut self.buffer);
        self.event_bytes = 0;
        let line = String::from_utf8_lossy(&tail);
        if let Some(rest) = line.trim_end_matches(['\n', '\r']).strip_prefix("data:") {
            self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        if self.data.is_empty() {
            None
        } else {
            let event = self.data.join("\n");
            self.data.clear();
            Some(event)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: {\"a\":").unwrap().is_empty());
        assert_eq!(
            parser.push(b"1}\n\ndata: [DONE]\n\n").unwrap(),
            vec!["{\"a\":1}", "[DONE]"]
        );
    }

    #[test]
    fn keeps_multibyte_characters_split_across_chunks() {
        let bytes = "data: 中文\n\n".as_bytes();
        let mut parser = SseParser::default();
        assert!(parser.push(&bytes[..8]).unwrap().is_empty());
        assert_eq!(parser.push(&bytes[8..]).unwrap(), vec!["中文"]);
    }

    #[test]
    fn ignores_comments_and_handles_crlf_and_unterminated_tail() {
        let mut parser = SseParser::default();
        assert_eq!(
            parser.push(b": keep-alive\r\ndata: x\r\n\r\ndata: y").unwrap(),
            vec!["x"]
        );
        assert_eq!(parser.finish().as_deref(), Some("y"));
    }

    #[test]
    fn bounds_unterminated_lines_across_chunks() {
        let mut parser = SseParser::new(10);
        assert!(parser.push(b"data: 123").unwrap().is_empty());
        assert!(parser.push(b"456").unwrap_err().contains("10 byte limit"));
    }

    #[test]
    fn bounds_multiline_events_and_resets_the_budget_between_events() {
        let mut parser = SseParser::new(9);
        assert_eq!(parser.push(b"data: a\n\ndata: b\n\n").unwrap(), ["a", "b"]);
        assert!(parser.push(b"data: a\n").unwrap().is_empty());
        assert!(parser.push(b"data: b\n").is_err());
    }
}
