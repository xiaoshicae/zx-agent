//! Helpers for the duties every platform has: splitting long text, dropping redeliveries,
//! pacing posts to a chat and keeping what an open approval card shows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use zx_ai::sync::lock;

use super::MessageId;

/// Text in parts of at most `max` characters, each written by `push`, which may write one
/// character as several (an entity, for example) but never splits what it writes. A part
/// ends at its last line break when that keeps at least half of it. Empty text is one empty
/// part. For a channel that must split text longer than one platform message.
pub fn split_text(text: &str, max: usize, push: fn(&mut String, char)) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in text.chars() {
        let mut piece = String::new();
        push(&mut piece, c);
        let size = piece.chars().count();
        if count + size > max && count > 0 {
            let cut = current
                .rfind('\n')
                .filter(|at| current[..*at].chars().count() >= max / 2);
            match cut {
                Some(at) => {
                    let rest = current.split_off(at + 1);
                    parts.push(std::mem::replace(&mut current, rest));
                    count = current.chars().count();
                }
                None => {
                    parts.push(std::mem::take(&mut current));
                    count = 0;
                }
            }
        }
        current.push_str(&piece);
        count += size;
    }
    parts.push(current);
    parts
}

/// Messages a channel has passed on recently, to drop redeliveries; the oldest are
/// forgotten first.
pub struct Seen<K> {
    order: VecDeque<K>,
    keys: HashSet<K>,
    capacity: usize,
}

impl<K: Clone + Eq + Hash> Seen<K> {
    /// Remember the last `capacity` keys.
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            keys: HashSet::new(),
            capacity,
        }
    }

    /// Record `key`; false if it was seen already.
    pub fn insert(&mut self, key: &K) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key.clone());
        if self.order.len() > self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.keys.remove(&oldest);
        }
        true
    }
}

/// When each chat may be posted to next, for a platform that allows so many posts per chat
/// per second. Concurrent sends reserve their turn under the lock and wait without it.
pub struct Pacer {
    interval: Duration,
    next: Mutex<HashMap<String, Instant>>,
}

impl Pacer {
    /// Posts to one chat at least `interval` apart.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::default(),
        }
    }

    /// Wait for the next turn to post to `conversation`.
    pub async fn turn(&self, conversation: &str) {
        let at = {
            let mut next = lock(&self.next);
            let now = Instant::now();
            let at = next.get(conversation).copied().filter(|at| *at > now).unwrap_or(now);
            next.insert(conversation.to_string(), at + self.interval);
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

/// The request texts of open approval cards, by card message, so that a closed card can
/// still show what was asked; the oldest are forgotten first.
pub struct Cards {
    capacity: usize,
    cards: Mutex<VecDeque<(MessageId, String)>>,
}

impl Cards {
    /// Remember at most `capacity` cards.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            cards: Mutex::default(),
        }
    }

    pub fn remember(&self, card: &MessageId, request: String) {
        let mut cards = lock(&self.cards);
        cards.push_back((card.clone(), request));
        if cards.len() > self.capacity {
            cards.pop_front();
        }
    }

    /// The request text of `card`, forgotten with this; `None` for an unknown card.
    pub fn forget(&self, card: &MessageId) -> Option<String> {
        let mut cards = lock(&self.cards);
        let index = cards.iter().position(|(id, _)| id == card)?;
        cards.remove(index).map(|(_, request)| request)
    }
}
