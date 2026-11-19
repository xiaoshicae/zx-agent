//! The chat platforms, each an [`ImChannel`](crate::ImChannel), and what every platform
//! shares: an HTTP client, its connection and the loop that reads it (pings, the silence
//! that means it died, the stop), keeping a delivery's messages before it is acknowledged,
//! reports to the operator, splitting long text, pacing posts to a chat, waiting out a rate
//! limit, and waiting between attempts to reach the platform. A platform uses these and
//! nothing of another platform.
//!
//! - `slack`: Slack, over Socket Mode and the Web API
//! - `feishu`: Feishu, over its long connection and the open API (the feature `feishu`)

#[cfg(feature = "feishu")]
mod feishu;
mod slack;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use reqwest::header::{AsHeaderName, HeaderMap};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use zx_util::env::env_value;
use zx_util::sync::lock;

use crate::{ErrorReport, ImError, Inbound, Inlet, Kept};

#[cfg(feature = "feishu")]
pub(crate) use feishu::FeishuSection;
#[cfg(feature = "feishu")]
pub use feishu::{FeishuChannel, FeishuConfig};
pub(crate) use slack::SlackSection;
pub use slack::{SlackChannel, SlackConfig};

/// Characters per posted message, beyond which a channel splits text with [`split_text`].
/// Slack recommends keeping message text within 4,000 characters (chat.postMessage,
/// "Truncating content"); other platforms take more, but longer messages read badly anywhere.
const MAX_TEXT_CHARS: usize = 4000;
/// How often a rate-limited call is retried after waiting as long as the platform asks.
const RATE_LIMIT_RETRIES: u32 = 3;
/// Limit for each call to a platform and for opening its connection, unless a channel's
/// settings say otherwise: long enough for a slow answer, short enough that a platform that
/// stopped answering is tried again soon.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The connection over which a platform delivers its events: a WebSocket, with TLS for
/// `wss://`.
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// An HTTP client whose calls each give up after `timeout`. Fails when it cannot be built.
fn http_client(timeout: Duration) -> Result<reqwest::Client, ImError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| ImError::Config(format!("cannot build the HTTP client: {error}")))
}

/// Tell the operator about `error`, which a channel goes on after: through `on_error`, the
/// callback of the channel's settings, when there is one.
fn report(on_error: Option<&ErrorReport>, error: &ImError) {
    if let Some(callback) = on_error {
        callback(error);
    }
}

/// The value of the variable `name`, which holds a platform's credential (`what`, for the
/// error): set and not empty, or an error.
fn secret(name: &str, what: &str) -> anyhow::Result<String> {
    env_value(name).with_context(|| format!("{name} must hold {what}"))
}

/// Text in parts of at most `max` characters: the first part, which may be empty, and the
/// rest.
///
/// Each character is written by `push`, which may write one character as several (an
/// entity, for example) but never splits what it writes. A part ends at its last line break
/// when that keeps at least half of it. For a channel that must split text longer than one
/// platform message.
///
/// A channel posts the parts itself: the first to the destination, the rest to the thread
/// of the first. Those few lines are in each platform; sharing them would take a callback
/// with a boxed future, which is harder to read than they are.
fn split_text(text: &str, max: usize, push: fn(&mut String, char)) -> (String, Vec<String>) {
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

/// How long a rate-limited call waits before it is tried again: the seconds in the header
/// `name` of `headers`, the answer's, in which the platform says so, but at most `longest`;
/// one second when that header is missing, not text, or not a number of seconds.
fn rate_limit_wait(headers: &HeaderMap, name: impl AsHeaderName, longest: Duration) -> Duration {
    let secs = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs).min(longest)
}

/// One delivery's events once its messages are kept in the backlog ([`keep`]).
struct Delivery {
    /// What to pass on: the clicks, and the messages new to the backlog.
    events: Vec<Inbound>,
    /// Whether the delivery may be acknowledged: not when one of its messages could not be
    /// kept, so that the platform delivers it again. The messages kept are passed on even
    /// so: delivered again, they are known.
    acknowledge: bool,
}

/// Keep the messages of one delivery, `events`, in the backlog of `inlet`, as a channel does
/// before it acknowledges the delivery; a message that could not be kept is reported through
/// `on_error`.
async fn keep(inlet: &Inlet, events: Vec<Inbound>, on_error: Option<&ErrorReport>) -> Delivery {
    let mut delivery = Delivery {
        events: Vec::with_capacity(events.len()),
        acknowledge: true,
    };
    for event in events {
        match inlet.keep(&event).await {
            Ok(Kept::New) => delivery.events.push(event),
            Ok(Kept::Again) => {}
            Err(error) => {
                report(on_error, &error);
                delivery.acknowledge = false;
            }
        }
    }
    delivery
}

/// When each chat may be posted to next, for a platform that allows so many posts per chat
/// per second. Concurrent sends reserve their turn under the lock and wait without it.
struct Pacer {
    interval: Duration,
    next: Mutex<HashMap<String, Instant>>,
}

impl Pacer {
    /// Posts to one chat at least `interval` apart.
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::default(),
        }
    }

    /// Wait for the next turn to post to `conversation`.
    async fn turn(&self, conversation: &str) {
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

/// Waits between attempts to reach a platform: each failed attempt doubles the next wait,
/// up to the longest one.
struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// The shortest wait of [`Backoff::default`].
    const DEFAULT_MIN: Duration = Duration::from_secs(1);
    /// The longest wait of [`Backoff::default`].
    const DEFAULT_MAX: Duration = Duration::from_secs(60);

    /// Waits from `min`, doubling up to `max`.
    fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, next: min }
    }

    /// Start again from the shortest wait, after an attempt succeeded.
    fn reset(&mut self) {
        self.next = self.min;
    }

    /// The wait after a failed attempt; the next one is twice as long, up to the longest.
    fn advance(&mut self) -> Duration {
        let wait = self.next;
        self.next = wait.saturating_mul(2).min(self.max);
        wait
    }

    /// Wait out the current backoff, doubling it; false when `cancel` is triggered meanwhile.
    async fn wait(&mut self, cancel: &CancellationToken) -> bool {
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
async fn pause(wait: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(wait) => true,
    }
}

/// When to ping a connection next, and when its silence means it is dead: a frame is
/// expected within twice the ping interval, plus a margin, of the last one.
struct Keepalive {
    interval: Duration,
    last_ping: Instant,
    next_ping: Instant,
    deadline: Instant,
}

impl Keepalive {
    /// Longest ping interval, so that a bad value from a server cannot stop pings or hide a
    /// dead connection for hours.
    const MAX_INTERVAL: Duration = Duration::from_secs(3600);
    /// Added to twice the ping interval: an answer may be a few seconds late. At most the
    /// interval itself, so that a short interval, as tests use, does not wait seconds for a
    /// dead connection.
    const MARGIN: Duration = Duration::from_secs(5);

    /// Ping every `interval`, the first at once; the connection counts as alive now.
    fn new(interval: Duration) -> Self {
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
    fn next_ping(&self) -> Instant {
        self.next_ping
    }

    /// When the connection counts as dead, unless a frame arrives first.
    fn deadline(&self) -> Instant {
        self.deadline
    }

    /// How long a silence counts as a dead connection.
    fn silence(&self) -> Duration {
        Self::silence_for(self.interval)
    }

    /// A ping went out now.
    fn pinged(&mut self) {
        self.last_ping = Instant::now();
        self.next_ping = self.last_ping + self.interval;
    }

    /// A frame arrived now.
    fn alive(&mut self) {
        self.deadline = Instant::now() + self.silence();
    }

    /// Ping every `interval` from now on, counted from the last ping. Only a platform whose
    /// server changes the interval while connected needs it: Feishu's.
    #[cfg(any(feature = "feishu", test))]
    fn set_interval(&mut self, interval: Duration) {
        self.interval = interval.min(Self::MAX_INTERVAL);
        self.next_ping = self.last_ping + self.interval;
    }

    // The interval is at most MAX_INTERVAL, so this cannot overflow.
    fn silence_for(interval: Duration) -> Duration {
        interval * 2 + Self::MARGIN.min(interval)
    }
}

/// What came next over a connection ([`next_message`]).
enum Next {
    /// `cancel` was triggered; the connection was closed.
    Cancelled,
    /// The connection failed or ended without a close frame, or a ping could not be sent.
    Lost,
    /// Nothing came for this long, not even answers to the pings: the connection counts as
    /// dead.
    Silent(Duration),
    /// A message: data, a control frame, or the platform's close.
    Message(Message),
}

/// The next message over `socket`, which `keepalive` keeps: `ping` goes out whenever it says
/// so, and its deadline passing in silence gives the connection up. Any message counts as
/// life. When `cancel` is triggered, the socket is closed.
///
/// `ping` is cloned for each ping; its payload is shared, not copied.
async fn next_message(
    socket: &mut Socket,
    keepalive: &mut Keepalive,
    ping: &Message,
    cancel: &CancellationToken,
) -> Next {
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                // The server is stopping; a failed close changes nothing.
                socket.close(None).await.ok();
                return Next::Cancelled;
            }
            () = tokio::time::sleep_until(keepalive.next_ping()) => {
                if socket.send(ping.clone()).await.is_err() {
                    return Next::Lost;
                }
                keepalive.pinged();
            }
            () = tokio::time::sleep_until(keepalive.deadline()) => return Next::Silent(keepalive.silence()),
            message = socket.next() => {
                keepalive.alive();
                return match message {
                    Some(Ok(message)) => Next::Message(message),
                    Some(Err(_)) | None => Next::Lost,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderValue, RETRY_AFTER};

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
    fn a_rate_limit_wait_is_capped_and_defaults_to_one_second() {
        let longest = Duration::from_secs(60);
        let wait = |value: Option<HeaderValue>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(RETRY_AFTER, value);
            }
            rate_limit_wait(&headers, RETRY_AFTER, longest)
        };
        assert_eq!(wait(None), Duration::from_secs(1));
        assert_eq!(wait(Some(HeaderValue::from_static("soon"))), Duration::from_secs(1));
        // Not text: as good as missing.
        assert_eq!(
            wait(Some(HeaderValue::from_bytes(&[0xff]).unwrap())),
            Duration::from_secs(1)
        );
        assert_eq!(wait(Some(HeaderValue::from_static("5"))), Duration::from_secs(5));
        assert_eq!(wait(Some(HeaderValue::from_static(" 5 "))), Duration::from_secs(5));
        assert_eq!(wait(Some(HeaderValue::from_static("100000"))), longest);
        // Only the named header counts.
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(rate_limit_wait(&headers, "x-reset", longest), Duration::from_secs(1));
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
        // The margin shrinks to a short interval.
        assert_eq!(keepalive.silence(), Duration::from_secs(9));
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
