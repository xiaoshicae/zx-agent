//! A summarizer of the host's own: it makes the summary of a compaction in place of the
//! kernel's request to the model, and the kernel still decides when, records the start and
//! the result, checks the summary and bounds the wait.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use zx_agent::agent::RunLimits;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, Usage};
use zx_agent::compaction::{Config, Outcome, Summarized, Summarizer, SummarySource};
use zx_agent::events::RunOutcome;
use zx_agent::transcript::{CompactionSummary, Entry, SummarizedBy, SummaryFact, Transcript};

use zx_testing::{Collect, Driven, RunnerSetup, config, refusal, seeded_session};

/// What a scripted summarizer answers a call with.
type Answer = dyn Fn(&SummarySource<'_>) -> Result<Summarized, String> + Send + Sync;

/// A summarizer that answers as scripted and remembers what it was asked.
struct Scripted {
    name: &'static str,
    answer: Box<Answer>,
    /// Waits for ever instead of answering.
    hangs: bool,
    /// The entry IDs of the records of each call, with the token budget it was given.
    asked: Mutex<Vec<(Vec<String>, u32)>>,
    /// Notified when a call has begun.
    began: Notify,
}

impl Scripted {
    fn answering(answer: impl Fn(&SummarySource<'_>) -> Result<Summarized, String> + Send + Sync + 'static) -> Self {
        Self {
            name: "notes",
            answer: Box::new(answer),
            hangs: false,
            asked: Mutex::new(Vec::new()),
            began: Notify::new(),
        }
    }

    /// One that never answers.
    fn hanging() -> Self {
        Self {
            hangs: true,
            ..Self::answering(citing_first)
        }
    }
}

#[async_trait]
impl Summarizer for Scripted {
    fn name(&self) -> &str {
        self.name
    }

    async fn summarize(&self, source: &SummarySource<'_>) -> Result<Summarized, String> {
        let ids = source.records.iter().map(|record| record.entry_id.clone()).collect();
        self.asked.lock().unwrap().push((ids, source.max_summary_tokens));
        self.began.notify_one();
        if self.hangs {
            std::future::pending::<()>().await;
        }
        (self.answer)(source)
    }
}

/// A summary with one fact that cites `id`, which used 7 input and 3 output tokens.
fn citing(id: &str) -> Summarized {
    Summarized {
        summary: CompactionSummary {
            facts: vec![SummaryFact {
                text: "Old observation".into(),
                source_entry_ids: vec![id.into()],
            }],
            inferences: Vec::new(),
            plans: vec!["Continue".into()],
        },
        usage: Usage {
            input: 7,
            output: 3,
            total_tokens: 10,
            ..Usage::default()
        },
    }
}

/// A summary that cites the first record it was given.
fn citing_first(source: &SummarySource<'_>) -> Result<Summarized, String> {
    Ok(citing(&source.records[0].entry_id))
}

fn budget() -> Config {
    Config {
        keep_recent_bytes: 1,
        timeout: Duration::from_secs(5),
        ..Config::default()
    }
}

/// A session with an old record worth summarizing, whose summaries `summarizer` makes with
/// the budgets of `compaction` and whose model answers with `steps`. Returns the session,
/// the events of its runs, the model, and the old record's ID.
async fn summarized_by(
    summarizer: Arc<Scripted>,
    compaction: Config,
    steps: Vec<FauxStep>,
) -> (Driven, Arc<Collect>, Arc<FauxProvider>, String) {
    let (store, old) = seeded_session().await;
    let provider = Arc::new(FauxProvider::new(steps));
    let mut config = config(provider.clone(), vec![]);
    config.summarizer = Some(summarizer);
    let (session, events) = RunnerSetup {
        session: store.into(),
        limits: RunLimits {
            max_model_requests: Some(2),
            ..RunLimits::default()
        },
        compaction,
        ..RunnerSetup::new(config)
    }
    .build()
    .await;
    (session, events, provider, old)
}

/// Who the starts of the session's compactions say makes their summaries, and how many
/// requests the session records.
fn recorded(transcript: &Transcript) -> (Vec<SummarizedBy>, usize) {
    let mut starts = Vec::new();
    let mut requests = 0;
    for entry in transcript.entries() {
        match entry {
            Entry::CompactionStarted(started) => starts.push(started.by.clone()),
            Entry::RequestStarted(_) => requests += 1,
            _ => {}
        }
    }
    (starts, requests)
}

#[tokio::test]
async fn a_summarizer_makes_the_summary_and_the_model_is_not_asked() {
    let summarizer = Arc::new(Scripted::answering(citing_first));
    let steps = vec![FauxStep::text("finished")];
    let (session, events, provider, old) = summarized_by(summarizer.clone(), budget(), steps).await;

    let compact = session.compact().await.unwrap();

    assert_eq!(compact.outcome, Outcome::Completed, "{compact:?}");
    assert_eq!((compact.requests, compact.usage.total_tokens), (1, 10));
    assert!(compact.output_bytes < compact.input_bytes);
    assert_eq!(events.kinds(), ["compaction_start", "compaction_end"]);
    assert!(provider.requests().is_empty(), "the model is not asked for the summary");
    assert_eq!(*summarizer.asked.lock().unwrap(), [(vec![old], 2048)]);
    // The start names the summarizer, and the session holds no request for the summary.
    let (starts, requests) = session.with_transcript(recorded).unwrap();
    assert_eq!(starts, [SummarizedBy::Summarizer("notes".into())]);
    assert_eq!(requests, 0);
    // The model then sees the summary in place of what it summarizes.
    let report = session.prompt("continue now").await.unwrap();
    assert_eq!(report.final_text, "finished");
    let seen = &provider.requests()[0].messages;
    assert!(
        seen.iter()
            .any(|message| matches!(message, Message::User { content, .. } if content.contains("<context_summary>")))
    );
    assert!(
        !seen
            .iter()
            .any(|message| matches!(message, Message::User { content, .. } if content.contains("observed observed")))
    );
}

#[tokio::test]
async fn an_automatic_summary_by_a_summarizer_counts_as_a_request_and_its_usage_as_the_runs() {
    let summarizer = Arc::new(Scripted::answering(citing_first));
    let compaction = Config {
        trigger_bytes: Some(1024),
        ..budget()
    };
    let steps = vec![FauxStep::text("answer")];
    let (session, events, provider, _) = summarized_by(summarizer, compaction, steps).await;

    let report = session.prompt("continue").await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    // The summary and the task request: the limit of two is reached, not passed.
    assert_eq!(report.model_requests, 2);
    assert_eq!(report.usage.total_tokens, 10 + 15);
    assert_eq!(provider.requests().len(), 1);
    let kinds = events.kinds();
    let start = kinds.iter().position(|kind| *kind == "compaction_start").unwrap();
    assert_eq!(kinds[start..start + 2], ["compaction_start", "compaction_end"]);
}

#[tokio::test]
async fn a_summarizer_that_fails_fails_the_compaction_and_the_session_records_why() {
    let summarizer = Arc::new(Scripted::answering(|_| Err("no capacity".into())));
    let (session, events, _, _) = summarized_by(summarizer, budget(), Vec::new()).await;
    let before = session.with_transcript(Transcript::messages).unwrap();

    let compact = session.compact().await.unwrap();

    assert_eq!(compact.outcome, Outcome::Failed);
    assert_eq!(compact.error.as_deref(), Some("summarizer notes failed: no capacity"));
    assert_eq!((compact.requests, compact.output_bytes), (1, compact.input_bytes));
    assert_eq!(events.kinds(), ["compaction_start", "compaction_failed"]);
    let (failure, messages) = session
        .with_transcript(|transcript| {
            let failure = transcript.entries().iter().find_map(|entry| match entry {
                Entry::CompactionFailed(failed) => Some(failed.error.clone()),
                _ => None,
            });
            (failure, transcript.messages())
        })
        .unwrap();
    assert_eq!(failure.as_deref(), Some("summarizer notes failed: no capacity"));
    assert_eq!(messages, before, "the context is as it was");
}

#[tokio::test]
async fn a_summary_that_cites_a_record_the_compaction_did_not_read_is_refused() {
    let summarizer = Arc::new(Scripted::answering(|_| Ok(citing("invented"))));
    let (session, _, _, _) = summarized_by(summarizer, budget(), Vec::new()).await;

    let compact = session.compact().await.unwrap();

    assert_eq!(compact.outcome, Outcome::Failed);
    let error = compact.error.unwrap();
    assert!(error.contains("\"invented\" is not in the source messages"), "{error}");
    // What the summarizer spent is counted even so.
    assert_eq!(compact.usage.total_tokens, 10);
}

#[tokio::test]
async fn a_summarizer_that_does_not_answer_is_given_up_after_the_compaction_timeout() {
    let compaction = Config {
        timeout: Duration::from_millis(20),
        ..budget()
    };
    let (session, _, _, _) = summarized_by(Arc::new(Scripted::hanging()), compaction, Vec::new()).await;

    let compact = session.compact().await.unwrap();

    assert_eq!(compact.outcome, Outcome::Failed);
    assert_eq!(compact.error.as_deref(), Some("summarizer notes timed out after 20ms"));
}

#[tokio::test]
async fn cancelling_the_run_stops_waiting_for_the_summarizer() {
    let summarizer = Arc::new(Scripted::hanging());
    let (session, _, _, _) = summarized_by(summarizer.clone(), budget(), Vec::new()).await;

    let cancel_once_asked = async {
        summarizer.began.notified().await;
        session.cancel();
    };
    let (compact, ()) = tokio::join!(session.compact(), cancel_once_asked);

    let compact = compact.unwrap();
    assert_eq!(compact.outcome, Outcome::Cancelled);
    assert_eq!(compact.error.as_deref(), Some("compaction cancelled"));
}

#[tokio::test]
async fn a_summarizer_that_panics_fails_the_compaction_and_not_the_process() {
    let summarizer = Arc::new(Scripted::answering(|_| panic!("summarizer bug")));
    let (session, _, _, _) = summarized_by(summarizer, budget(), Vec::new()).await;

    let compact = session.compact().await.unwrap();

    assert_eq!(compact.outcome, Outcome::Failed);
    assert_eq!(compact.error.as_deref(), Some("summarizer notes panicked"));
}

#[test]
fn a_summarizer_without_a_name_refuses_the_agent() {
    let mut config = config(Arc::new(FauxProvider::new(Vec::new())), vec![]);
    config.summarizer = Some(Arc::new(Scripted {
        name: " ",
        ..Scripted::answering(citing_first)
    }));

    assert_eq!(refusal(config), "the summarizer has no name");
}
