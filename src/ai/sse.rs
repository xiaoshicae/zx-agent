//! Minimal Server-Sent Events parser: yields the `data` payload of each event.

/// Incremental parser that accepts arbitrary byte chunks.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed raw bytes; returns the payloads of events completed by them.
    ///
    /// Only whole lines are decoded, and a newline byte never occurs inside a
    /// multi-byte UTF-8 sequence, so characters split across chunks stay intact.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // `event:`, `id:`, `retry:` and `:` comment lines are not needed here.
        }
        events
    }

    /// Flush an event left unterminated at end of stream.
    pub fn finish(&mut self) -> Option<String> {
        let tail = std::mem::take(&mut self.buffer);
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
        assert!(parser.push(b"data: {\"a\":").is_empty());
        assert_eq!(parser.push(b"1}\n\ndata: [DONE]\n\n"), vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn keeps_multibyte_characters_split_across_chunks() {
        let bytes = "data: 中文\n\n".as_bytes();
        let mut parser = SseParser::default();
        assert!(parser.push(&bytes[..8]).is_empty());
        assert_eq!(parser.push(&bytes[8..]), vec!["中文"]);
    }

    #[test]
    fn ignores_comments_and_handles_crlf_and_unterminated_tail() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b": keep-alive\r\ndata: x\r\n\r\ndata: y"), vec!["x"]);
        assert_eq!(parser.finish().as_deref(), Some("y"));
    }
}
