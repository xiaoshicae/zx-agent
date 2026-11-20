//! The chat platforms, each an [`ImChannel`](crate::ImChannel), and what every platform
//! shares: an HTTP client, its connection and the loop that reads it (pings, the silence
//! that means it died, the stop), keeping a delivery's messages before it is acknowledged,
//! reports to the operator, splitting long text, pacing posts to a chat, waiting out a rate
//! limit, and waiting between attempts to reach the platform. A platform uses these and
//! nothing of another platform. Each builds its channel from its im.toml section through a
//! [`PlatformFactory`], registered under the section's name.
//!
//! - `slack`: Slack, over Socket Mode and the Web API
//! - `feishu`: Feishu, over its long connection and the open API (the feature `feishu`)

#[cfg(feature = "feishu")]
mod feishu;
mod slack;

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
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
use zx_util::registry::Registry;
use zx_util::sync::lock;

use crate::{CardLabels, ErrorReport, ImChannel, ImError, Inbound, Inlet, Kept};

#[cfg(feature = "feishu")]
pub use self::feishu::{FeishuChannel, FeishuConfig};
pub use self::slack::{SlackChannel, SlackConfig};

/// The names the built-in platforms are registered under, which their channels go by.
/// Feishu's is known also to a build without it, which refuses its section by name.
const SLACK: &str = "slack";
const FEISHU: &str = "feishu";

/// Builds a platform's channel from the section of im.toml named as the platform is
/// registered ([`Program::register_platform`](crate::Program::register_platform)).
pub trait PlatformFactory: Send + Sync {
    /// The channel `input` describes. Fails on a section it cannot use, the error naming its
    /// keys as im.toml writes them, and on a credential it cannot read.
    fn build(&self, input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel>;
}

/// What a platform's channel is built from.
pub struct PlatformInput<'a> {
    /// The platform's section of im.toml.
    pub section: &'a toml::Table,
    /// The texts of approval cards: `[cardLabels]` over the English defaults.
    pub labels: &'a CardLabels,
    /// Where the channel reports what it goes on after: the operator log, as `failed` lines
    /// with the platform's name.
    pub on_error: ErrorReport,
}

/// A channel a factory built.
pub struct BuiltChannel {
    /// It goes by the name its platform is registered under ([`ImChannel::platform`]).
    pub channel: Arc<dyn ImChannel>,
    /// The variables that hold the platform's credentials: commands do not see them, and
    /// their values are redacted from tool output.
    pub hidden_env: Vec<String>,
}

/// The platforms every program has: Slack, and Feishu, which a build without it refuses.
pub(crate) fn built_in() -> Registry<dyn PlatformFactory> {
    let slack: Arc<dyn PlatformFactory> = Arc::new(slack::Slack);
    #[cfg(feature = "feishu")]
    let feishu: Arc<dyn PlatformFactory> = Arc::new(feishu::Feishu);
    #[cfg(not(feature = "feishu"))]
    let feishu: Arc<dyn PlatformFactory> = Arc::new(NotBuilt);
    Registry::new(
        "IM platform",
        vec![(SLACK.to_string(), slack), (FEISHU.to_string(), feishu)],
    )
}

/// Feishu's place in a build without it, so that its section is refused by name.
#[cfg(not(feature = "feishu"))]
struct NotBuilt;

#[cfg(not(feature = "feishu"))]
impl PlatformFactory for NotBuilt {
    fn build(&self, _input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel> {
        anyhow::bail!("this build of zx-im has no such platform: it was built without the cargo feature `feishu`")
    }
}

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
    // What `push` writes of one character.
    let mut piece = String::new();
    for c in text.chars() {
        piece.clear();
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

/// Longest wait a rate limit is given: a longer one is more likely a bad header than a real
/// wait (Slack's limits, for one, are per minute: Rate limits, "Tiers"), and a send that
/// waits that long is dropped by the server's outbound timeout anyway, a connection attempt
/// by the stop.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// How long a rate-limited call waits before it is tried again: the seconds in the header
/// `name` of `headers`, the answer's, in which the platform says so, but at most
/// [`MAX_RATE_LIMIT_WAIT`]; one second when that header is missing, not text, or not a
/// number of seconds.
fn rate_limit_wait(headers: &HeaderMap, name: impl AsHeaderName) -> Duration {
    let secs = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs).min(MAX_RATE_LIMIT_WAIT)
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

impl Delivery {
    /// Finish the delivery: send its acknowledgement with `acknowledge`, unless it may not be
    /// acknowledged, then pass its events on to the server. They are passed on also when the
    /// acknowledgement cannot be sent: the platform then delivers the messages again, but
    /// the backlog knows them by then, and nobody would pass them on. `Err` says how the
    /// connection ended: lost when the acknowledgement could not be sent, cancelled when the
    /// server no longer reads.
    async fn finish<E: Display>(
        self,
        inlet: &Inlet,
        acknowledge: impl Future<Output = Result<(), E>>,
    ) -> Result<(), Ended> {
        let acknowledged = if self.acknowledge {
            let sent = acknowledge.await;
            sent.map_err(|error| format!("cannot acknowledge a delivery: {error}"))
        } else {
            Ok(())
        };
        for event in self.events {
            if !inlet.pass(event).await {
                return Err(Ended::Cancelled);
            }
        }
        acknowledged.map_err(Ended::Lost)
    }
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

/// How an attempt to open a platform's connection went.
enum Connection<L> {
    /// The connection, boxed: the other variant is a short reason.
    Open(Box<L>),
    /// A failure worth retrying, with the reason, which never shows a secret, such as an
    /// address that carries a ticket.
    Later(String),
}

/// How a platform's connection ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// The server is stopping: `cancel` was triggered, or the server no longer reads.
    Cancelled,
    /// The platform closed it, or asked for another: a new one is opened.
    Closed,
    /// It failed, for this reason: a new one is opened, and the operator hears why.
    Lost(String),
    /// Nothing came over it for this long, not even answers to the pings.
    Silent(Duration),
}

/// What came next over a connection ([`next_message`]).
enum Next {
    /// `cancel` was triggered; the connection was closed.
    Cancelled,
    /// The connection failed or ended without a close frame, or a ping could not be sent:
    /// why.
    Lost(String),
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
                if let Err(error) = socket.send(ping.clone()).await {
                    return Next::Lost(format!("cannot send a ping: {error}"));
                }
                keepalive.pinged();
            }
            () = tokio::time::sleep_until(keepalive.deadline()) => return Next::Silent(keepalive.silence()),
            message = socket.next() => {
                keepalive.alive();
                return match message {
                    Some(Ok(message)) => Next::Message(message),
                    Some(Err(error)) => Next::Lost(format!("cannot read: {error}")),
                    None => Next::Lost("it ended without a close frame".into()),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    use super::*;
    use crate::{Addressee, ChatKind, ConversationId, Destination, MemoryBacklog, MessageId, UserId};

    #[test]
    fn the_first_part_is_there_even_for_empty_text() {
        assert_eq!(split_text("", 4, String::push), (String::new(), Vec::new()));
        assert_eq!(
            split_text("abcdefghij", 4, String::push),
            ("abcd".to_string(), vec!["efgh".to_string(), "ij".to_string()])
        );
    }

    /// The wait after an answer whose `Retry-After` is `value`, or that has none.
    fn retry_after(value: Option<HeaderValue>) -> Duration {
        let mut headers = HeaderMap::new();
        if let Some(value) = value {
            headers.insert(RETRY_AFTER, value);
        }
        rate_limit_wait(&headers, RETRY_AFTER)
    }

    #[test]
    fn a_rate_limit_wait_is_the_seconds_the_platform_names() {
        assert_eq!(retry_after(Some(HeaderValue::from_static("5"))), Duration::from_secs(5));
        assert_eq!(
            retry_after(Some(HeaderValue::from_static(" 5 "))),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn a_rate_limit_wait_is_capped() {
        assert_eq!(
            retry_after(Some(HeaderValue::from_static("100000"))),
            MAX_RATE_LIMIT_WAIT
        );
    }

    #[test]
    fn a_rate_limit_wait_without_a_number_of_seconds_is_one_second() {
        assert_eq!(retry_after(None), Duration::from_secs(1));
        assert_eq!(
            retry_after(Some(HeaderValue::from_static("soon"))),
            Duration::from_secs(1)
        );
        // Not text: as good as missing.
        let not_text = HeaderValue::from_bytes(&[0xff]).unwrap();
        assert_eq!(retry_after(Some(not_text)), Duration::from_secs(1));
        // Only the named header counts.
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(rate_limit_wait(&headers, "x-reset"), Duration::from_secs(1));
    }

    #[test]
    fn a_backoff_doubles_up_to_the_longest_wait() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let waits: Vec<u64> = (0..5).map(|_| backoff.advance().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 5, 5]);
    }

    #[test]
    fn a_reset_backoff_starts_over() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        backoff.advance();
        backoff.advance();
        backoff.reset();
        assert_eq!(backoff.advance(), Duration::from_secs(1));
    }

    #[test]
    fn a_keepalive_pings_at_once() {
        let before = Instant::now();
        let keepalive = Keepalive::new(Duration::from_secs(10));
        assert!(keepalive.next_ping() <= Instant::now());
        assert!(keepalive.deadline() >= before + Duration::from_secs(25));
    }

    #[test]
    fn a_new_ping_interval_counts_from_the_last_ping() {
        let before = Instant::now();
        let mut keepalive = Keepalive::new(Duration::from_secs(10));
        keepalive.pinged();
        let pinged = Instant::now();
        assert!(keepalive.next_ping() >= before + Duration::from_secs(10));
        assert!(keepalive.next_ping() <= pinged + Duration::from_secs(10));
        keepalive.set_interval(Duration::from_secs(3));
        assert!(keepalive.next_ping() <= pinged + Duration::from_secs(3));
    }

    #[test]
    fn the_silence_margin_shrinks_to_a_short_interval() {
        assert_eq!(Keepalive::new(Duration::from_secs(3)).silence(), Duration::from_secs(9));
    }

    #[test]
    fn a_ping_interval_is_capped() {
        assert_eq!(
            Keepalive::new(Duration::MAX).silence(),
            Keepalive::MAX_INTERVAL * 2 + Keepalive::MARGIN
        );
    }

    /// A message `id` someone posted in a group chat, mentioning the bot.
    fn message(id: &str) -> Inbound {
        Inbound::Message {
            id: MessageId(id.into()),
            at: Destination {
                conversation: ConversationId("C1".into()),
                thread: None,
            },
            chat: ChatKind::Group,
            addressee: Addressee::Bot,
            sender: UserId("U1".into()),
            text: "hello".into(),
        }
    }

    #[tokio::test]
    async fn a_kept_message_is_passed_on_when_its_acknowledgement_cannot_be_sent() {
        let (events, mut passed) = tokio::sync::mpsc::channel(4);
        let inlet = Inlet::new("slack", events, Arc::new(MemoryBacklog::default()));

        let delivery = keep(&inlet, vec![message("m1")], None).await;
        let finished = delivery.finish(&inlet, async { Err::<(), _>("closed") }).await;

        assert_eq!(
            finished,
            Err(Ended::Lost("cannot acknowledge a delivery: closed".into()))
        );
        assert_eq!(passed.try_recv().ok(), Some(message("m1")));
        // The platform delivers what was not acknowledged again: known by then, it is not
        // passed on a second time, so the first delivery was its only way to the server.
        let again = keep(&inlet, vec![message("m1")], None).await;
        assert_eq!(again.finish(&inlet, async { Ok::<(), &str>(()) }).await, Ok(()));
        assert!(passed.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_delivery_that_may_not_be_acknowledged_sends_no_acknowledgement() {
        let (events, mut passed) = tokio::sync::mpsc::channel(4);
        let inlet = Inlet::new("slack", events, Arc::new(MemoryBacklog::default()));
        let delivery = Delivery {
            events: vec![message("m1")],
            acknowledge: false,
        };

        let acknowledged = std::cell::Cell::new(false);
        let finished = delivery
            .finish(&inlet, async {
                acknowledged.set(true);
                Ok::<(), &str>(())
            })
            .await;

        assert_eq!(finished, Ok(()));
        assert!(!acknowledged.get(), "an acknowledgement was sent");
        assert_eq!(passed.try_recv().ok(), Some(message("m1")));
    }

    #[tokio::test]
    async fn a_pause_ends_early_when_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(!pause(Duration::from_secs(3600), &cancel).await);
        assert!(pause(Duration::ZERO, &CancellationToken::new()).await);
    }
}
