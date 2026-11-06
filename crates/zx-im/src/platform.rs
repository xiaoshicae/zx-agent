//! Helpers for the duties every platform has: splitting long text, dropping redeliveries,
//! pacing posts to a chat, keeping what an open approval card shows, waiting between
//! attempts to reach the platform, and telling a dead connection from a quiet one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zx_ai::sync::lock;

use super::{ImError, MessageId};

/// Characters per posted message, beyond which a channel splits text with [`split_text`].
/// Slack recommends keeping message text within 4,000 characters (chat.postMessage,
/// "Truncating content"); other platforms take more, but longer messages read badly anywhere.
pub const MAX_TEXT_CHARS: usize = 4000;
/// How often a rate-limited call is retried after waiting as long as the platform asks.
pub const RATE_LIMIT_RETRIES: u32 = 3;
/// Messages a [`Seen`] remembers: platforms redeliver only for a short while, and this
/// covers far more messages than arrive in that time.
pub const SEEN_CAPACITY: usize = 1024;
/// Cards a [`Cards`] remembers: an approval waits for a person, so few are open at once.
pub const CARD_CAPACITY: usize = 256;

/// Text in parts of at most `max` characters: the first part, which may be empty, and the
/// rest.
///
/// Each character is written by `push`, which may write one character as several (an
/// entity, for example) but never splits what it writes. A part ends at its last line break
/// when that keeps at least half of it. For a channel that must split text longer than one
/// platform message.
pub fn split_text(text: &str, max: usize, push: fn(&mut String, char)) -> (String, Vec<String>) {
    let mut first = None;
    let mut rest = Vec::new();
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
            let part = match cut {
                Some(at) => {
                    let after = current.split_off(at + 1);
                    count = after.chars().count();
                    std::mem::replace(&mut current, after)
                }
                None => {
                    count = 0;
                    std::mem::take(&mut current)
                }
            };
            match first {
                None => first = Some(part),
                Some(_) => rest.push(part),
            }
        }
        current.push_str(&piece);
        count += size;
    }
    match first {
        None => (current, rest),
        Some(first) => {
            rest.push(current);
            (first, rest)
        }
    }
}

/// The error of [`ImChannel::update`](super::ImChannel::update) with an
/// [`Outbound::ApprovalRequest`](super::Outbound::ApprovalRequest): a card is only posted,
/// never written over a message, and the server never asks for it. A configuration error,
/// since no later call would succeed.
pub fn card_update_refused() -> ImError {
    ImError::Config("an approval request is posted, not written over a message".into())
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

    /// Keep `request`, the text shown on `card`, forgetting the oldest card beyond the
    /// capacity.
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

/// Waits between attempts to reach a platform: each failed attempt doubles the next wait,
/// up to the longest one.
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// The shortest wait of [`Backoff::default`].
    pub const DEFAULT_MIN: Duration = Duration::from_secs(1);
    /// The longest wait of [`Backoff::default`].
    pub const DEFAULT_MAX: Duration = Duration::from_secs(60);

    /// Waits from `min`, doubling up to `max`.
    pub fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, next: min }
    }

    /// Start again from the shortest wait, after an attempt succeeded.
    pub fn reset(&mut self) {
        self.next = self.min;
    }

    /// The wait after a failed attempt; the next one is twice as long, up to the longest.
    pub fn advance(&mut self) -> Duration {
        let wait = self.next;
        self.next = wait.saturating_mul(2).min(self.max);
        wait
    }

    /// Wait out the current backoff, doubling it; false when `cancel` is triggered meanwhile.
    pub async fn wait(&mut self, cancel: &CancellationToken) -> bool {
        pause(self.advance(), cancel).await
    }
}

impl Default for Backoff {
    /// From one second up to a minute.
    fn default() -> Self {
        Self::new(Self::DEFAULT_MIN, Self::DEFAULT_MAX)
    }
}

/// Wait `wait`; false when `cancel` is triggered meanwhile.
pub async fn pause(wait: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(wait) => true,
    }
}

/// When to ping a connection next, and when its silence means it is dead: a frame is
/// expected within twice the ping interval, plus a margin, of the last one.
pub struct Keepalive {
    interval: Duration,
    last_ping: Instant,
    next_ping: Instant,
    deadline: Instant,
}

impl Keepalive {
    /// Longest ping interval, so that a bad value from a server cannot stop pings or hide a
    /// dead connection for hours.
    pub const MAX_INTERVAL: Duration = Duration::from_secs(3600);
    /// Added to twice the ping interval: an answer may be a few seconds late.
    const MARGIN: Duration = Duration::from_secs(5);

    /// Ping every `interval`, the first at once; the connection counts as alive now.
    pub fn new(interval: Duration) -> Self {
        let interval = interval.min(Self::MAX_INTERVAL);
        let now = Instant::now();
        Self {
            interval,
            last_ping: now,
            next_ping: now,
            deadline: now + Self::silence_for(interval),
        }
    }

    /// When to ping next.
    pub fn next_ping(&self) -> Instant {
        self.next_ping
    }

    /// When the connection counts as dead, unless a frame arrives first.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// How long a silence counts as a dead connection.
    pub fn silence(&self) -> Duration {
        Self::silence_for(self.interval)
    }

    /// A ping went out now.
    pub fn pinged(&mut self) {
        self.last_ping = Instant::now();
        self.next_ping = self.last_ping + self.interval;
    }

    /// A frame arrived now.
    pub fn alive(&mut self) {
        self.deadline = Instant::now() + self.silence();
    }

    /// Ping every `interval` from now on, counted from the last ping.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval.min(Self::MAX_INTERVAL);
        self.next_ping = self.last_ping + self.interval;
    }

    // The interval is at most MAX_INTERVAL, so this cannot overflow.
    fn silence_for(interval: Duration) -> Duration {
        interval * 2 + Self::MARGIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_part_is_there_even_for_empty_text() {
        assert_eq!(split_text("", 4, String::push), (String::new(), Vec::new()));
        assert_eq!(
            split_text("abcdefghij", 4, String::push),
            ("abcd".to_string(), vec!["efgh".to_string(), "ij".to_string()])
        );
    }

    #[test]
    fn a_backoff_doubles_up_to_the_longest_wait_and_starts_over_when_reset() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let waits: Vec<u64> = (0..5).map(|_| backoff.advance().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 5, 5]);
        backoff.reset();
        assert_eq!(backoff.advance(), Duration::from_secs(1));
    }

    #[test]
    fn a_keepalive_pings_at_once_and_counts_a_new_interval_from_the_last_ping() {
        let before = Instant::now();
        let mut keepalive = Keepalive::new(Duration::from_secs(10));
        assert!(keepalive.next_ping() <= Instant::now());
        assert!(keepalive.deadline() >= before + Duration::from_secs(25));
        keepalive.pinged();
        let pinged = Instant::now();
        assert!(keepalive.next_ping() >= before + Duration::from_secs(10));
        assert!(keepalive.next_ping() <= pinged + Duration::from_secs(10));
        keepalive.set_interval(Duration::from_secs(3));
        assert!(keepalive.next_ping() <= pinged + Duration::from_secs(3));
        assert_eq!(keepalive.silence(), Duration::from_secs(11));
        assert_eq!(
            Keepalive::new(Duration::MAX).silence(),
            Keepalive::MAX_INTERVAL * 2 + Keepalive::MARGIN
        );
    }

    #[tokio::test]
    async fn a_pause_ends_early_when_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(!pause(Duration::from_secs(3600), &cancel).await);
        assert!(pause(Duration::ZERO, &CancellationToken::new()).await);
    }
}
