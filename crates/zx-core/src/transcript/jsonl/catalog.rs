//! Rebuildable, bounded file metadata caches and the activity time at a log's tail.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::file;
use crate::transcript::{Entry, SessionHeader};

/// A bounded least-recently-used cache. Eviction changes read cost, never stored facts.
pub(super) struct Cache<T> {
    entries: HashMap<PathBuf, (u64, T)>,
    capacity: usize,
    clock: u64,
}

impl<T: Clone> Cache<T> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            clock: 0,
        }
    }

    pub(super) fn get(&mut self, path: &Path) -> Option<T> {
        let entry = self.entries.get_mut(path)?;
        self.clock = self.clock.wrapping_add(1);
        entry.0 = self.clock;
        Some(entry.1.clone())
    }

    pub(super) fn insert(&mut self, path: PathBuf, value: T) {
        if self.entries.len() >= self.capacity
            && !self.entries.contains_key(&path)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| used)
                .map(|(path, _)| path.clone())
        {
            self.entries.remove(&oldest);
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries.insert(path, (self.clock, value));
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&PathBuf, &T)> {
        self.entries.iter().map(|(path, (_, value))| (path, value))
    }
}

/// Read the last complete record, growing the tail window only when a record spans it.
/// This selects a candidate; opening or listing still validates the whole transcript.
pub(super) fn last_activity(path: &Path, header: &SessionHeader) -> Result<u64> {
    const INITIAL_TAIL_BYTES: u64 = 8192;
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let end = file.metadata()?.len();
    let mut window = INITIAL_TAIL_BYTES.min(end);
    loop {
        let start = end - window;
        file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        (&mut file).take(window).read_to_end(&mut tail)?;
        if let Some(activity) = activity_in(&tail, start == 0, header)? {
            return Ok(activity);
        }
        if start == 0 {
            bail!("{} has no complete session header or record", path.display());
        }
        window = window.saturating_mul(2).min(end);
    }
}

fn activity_in(tail: &[u8], starts_at_header: bool, header: &SessionHeader) -> Result<Option<u64>> {
    let mut lines = tail.rsplit(|&byte| byte == b'\n').peekable();
    let mut final_fragment = true;
    while let Some(line) = lines.next() {
        let unterminated = final_fragment;
        final_fragment = false;
        // The first fragment of a window may be cut inside a record.
        if lines.peek().is_none() {
            return if starts_at_header {
                Ok(Some(header.timestamp))
            } else {
                Ok(None)
            };
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if unterminated && file::incomplete_tail(line) {
            continue;
        }
        let entry: Entry = serde_json::from_slice(line)?;
        return Ok(Some(entry.timestamp()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_keeps_recently_used_metadata_and_allows_rebuilding() {
        let mut cache = Cache::new(2);
        cache.insert("a".into(), 1);
        cache.insert("b".into(), 2);
        assert_eq!(cache.get(Path::new("a")), Some(1));
        cache.insert("c".into(), 3);
        assert_eq!(cache.get(Path::new("b")), None);
        assert_eq!(cache.get(Path::new("a")), Some(1));
        cache.insert("b".into(), 4);
        assert_eq!(cache.get(Path::new("b")), Some(4));
        assert_eq!(cache.iter().count(), 2);
    }
}
