//! Session-host summaries, task request accounting and cancellation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, ContextEdit, Event, HookContext, HookOptions, Hooks};
use zx_agent::agent_session::{ModelRef, SessionError};
use zx_agent::ai::Message;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::compaction::{Config, Outcome};
use zx_agent::runner::{Limits, RunOutcome};
use zx_agent::session::{Entry, SessionStore};

use common::{RunnerSetup, config, fast_retry, header};

fn summary(id: &str) -> String {
    json!({"facts":[{"text":"Old observation","sourceEntryIds":[id]}],"inferences":["A guess"],"plans":["Continue"]})
        .to_string()
}

async fn seeded() -> (SessionStore, String) {
    let mut store = SessionStore::in_memory(header());
    let old = store
        .append_message(Message::user("observed ".repeat(600)))
        .await
        .unwrap();
    store.append_message(Message::user("most recent input")).await.unwrap();
    (store, old)
}

fn budget() -> Config {
    Config {
        keep_recent_bytes: 1,
        timeout: Duration::from_secs(1),
        ..Config::default()
    }
}

#[tokio::test]
async fn manual_compaction_is_a_bounded_tool_free_request_and_survives_reuse() {
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("finished"),
    ]));
    let (runner, events) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let session = AgentSession::from(runner.with_compaction(budget()).unwrap());
    let compact = session.compact().await.unwrap();
    assert_eq!(compact.outcome, Outcome::Completed, "{compact:?}");
    assert_eq!(compact.requests, 1);
    assert_eq!(compact.usage.total_tokens, 15);
    assert!(compact.output_bytes < compact.input_bytes);
    assert_eq!(events.kinds(), ["compaction_start", "request_start", "compaction_end"]);
    let report = session.prompt("continue now").await.unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.turns, 1);
    assert_eq!(report.final_text, "finished");
    let requests = provider.requests();
    assert_eq!(requests[0].max_tokens, Some(2048));
    assert!(requests[0].tools.is_empty());
    let Message::User { content, .. } = requests[0].messages.last().unwrap() else {
        panic!("source");
    };
    let source: Vec<Value> = serde_json::from_str(content).unwrap();
    assert_eq!(source[0]["entryId"], old);
    assert!(source[0]["message"]["content"].as_str().unwrap().contains("observed"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| matches!(message, Message::User{content,..} if content.contains("<context_summary>")))
    );
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| matches!(message, Message::User{content,..} if content.contains("observed observed")))
    );
}

#[tokio::test]
async fn automatic_summary_counts_against_task_request_budget_and_usage() {
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("answer"),
    ]));
    let (runner, events) = RunnerSetup {
        session: store,
        limits: Limits {
            max_turns: Some(2),
            timeout: None,
        },
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.turns, 2);
    assert_eq!(report.usage.total_tokens, 30);
    assert_eq!(provider.requests().len(), 2);
    let messages = runner.session().messages();
    assert_eq!(provider.requests()[1].messages, messages[..messages.len() - 1]);
    // The summary request counts against the budget, but only the task request is a turn.
    let turns: Vec<_> = events
        .events()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnStart { turn } | AgentEvent::TurnEnd { turn } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1, 1]);
    let kinds = events.kinds();
    let summary = kinds.iter().position(|kind| *kind == "compaction_start").unwrap();
    assert_eq!(
        kinds[summary..summary + 3],
        ["compaction_start", "request_start", "compaction_end"]
    );
}

#[tokio::test]
async fn steering_received_during_a_summary_is_persisted_before_the_next_task_request() {
    struct SteerAfterSummary(Arc<zx_agent::agent::MessageQueues>);
    #[async_trait]
    impl Hooks for SteerAfterSummary {
        async fn on_event(&self, event: &Event, _ctx: &HookContext) -> Result<(), String> {
            if matches!(event.event, AgentEvent::CompactionEnd { .. }) {
                self.0.steer("new correction");
            }
            Ok(())
        }
    }
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("answer"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(SteerAfterSummary(config.queues.clone()));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config)
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert!(
        matches!(provider.requests()[1].messages.last(), Some(Message::User { content, .. }) if content == "new correction")
    );
    assert_eq!(report.turns, 2);
}

#[tokio::test]
async fn summary_cannot_spend_an_extra_task_request_after_max_turns() {
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("must not be sent"),
    ]));
    let (runner, _) = RunnerSetup {
        session: store,
        limits: Limits {
            max_turns: Some(1),
            timeout: None,
        },
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::MaxTurns);
    assert_eq!(report.turns, 1);
    assert_eq!(report.usage.total_tokens, 15);
    assert!(report.final_text.is_empty(), "a summary is never a task answer");
    assert_eq!(provider.requests().len(), 1);
    assert!(runner.session().latest_compaction().is_some());
}

#[tokio::test]
async fn sdk_defaults_do_not_make_summary_requests() {
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text("normal task answer")]));
    let (mut runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    assert_eq!(
        runner.run("go", &CancellationToken::new()).await.outcome,
        RunOutcome::Completed
    );
    assert_eq!(provider.requests().len(), 1);
    // Request checkpoints are always written; no compaction record may appear.
    assert!(runner.session().entries().iter().all(|entry| matches!(
        entry,
        Entry::Message(_) | Entry::RequestConfig(_) | Entry::RequestStarted(_)
    )));
}

#[tokio::test]
async fn repeated_compaction_uses_originals_instead_of_a_previous_summary() {
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("new observation ".repeat(200)),
        FauxStep::text(summary(&old)),
    ]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner.with_compaction(budget()).unwrap();
    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );
    assert_eq!(runner.compact(&CancellationToken::new()).await.outcome, Outcome::Noop);
    runner.run("new task", &CancellationToken::new()).await;
    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );
    let requests = provider.requests();
    let source = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(source.contains("observed observed"));
    assert!(!source.contains("<context_summary>"));
    assert!(!source.contains("Old observation"));
}

#[tokio::test]
async fn invalid_or_truncated_summaries_never_replace_context_or_retry_as_tasks() {
    for step in [
        FauxStep::text("not JSON"),
        FauxStep::text(summary("unknown")),
        FauxStep::truncated("cut off"),
        FauxStep::tool_call("c", "bash", json!({"command":"echo no"})),
        FauxStep::Fail("temporarily unavailable".into()),
    ] {
        let (store, _) = seeded().await;
        let before = store.raw_projection();
        let provider = Arc::new(FauxProvider::new([step, FauxStep::text("should not execute")]));
        let (runner, _) = RunnerSetup {
            session: store,
            retry: fast_retry(3),
            ..RunnerSetup::new(config(provider.clone(), vec![]))
        }
        .build();
        let mut runner = runner
            .with_compaction(Config {
                trigger_bytes: Some(1024),
                ..budget()
            })
            .unwrap();
        let report = runner.run("continue", &CancellationToken::new()).await;
        assert_eq!(report.outcome, RunOutcome::Error, "{report:?}");
        assert_eq!(report.turns, 1);
        assert_eq!(report.retries, 0);
        assert!(report.final_text.is_empty());
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(&runner.session().raw_projection()[..before.len()], before);
        assert!(runner.session().latest_compaction().is_none());
        assert!(matches!(
            runner.session().entries().last(),
            Some(Entry::CompactionFailed(_))
        ));
    }
}

#[tokio::test]
async fn a_failed_automatic_summary_is_not_paid_for_again_on_the_next_prompt() {
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("not JSON"),
        FauxStep::text("must not be requested"),
    ]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let first = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(first.outcome, RunOutcome::Error, "{first:?}");
    let second = runner.run("try again", &CancellationToken::new()).await;
    assert_eq!(second.outcome, RunOutcome::Error, "{second:?}");
    assert_eq!(second.turns, 0);
    assert!(
        second.error.as_deref().unwrap_or_default().contains("paused"),
        "{second:?}"
    );
    assert_eq!(provider.requests().len(), 1, "the failed summary is not paid for again");
}

#[tokio::test]
async fn the_pause_survives_a_run_whose_future_is_dropped() {
    /// Holds the input "hold" forever, after telling the test it is waiting.
    struct Hold(Arc<tokio::sync::Notify>);
    #[async_trait]
    impl Hooks for Hold {
        async fn on_input(&self, text: String, _ctx: &HookContext) -> Result<String, String> {
            if text == "hold" {
                self.0.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(text)
        }
    }
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text("not JSON"),
        FauxStep::text("must not be requested"),
    ]));
    let waiting = Arc::new(tokio::sync::Notify::new());
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push_with(
        Hold(waiting.clone()),
        HookOptions {
            timeout: Duration::from_secs(3600),
            ..HookOptions::default()
        },
    );
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config)
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let first = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(first.outcome, RunOutcome::Error, "{first:?}");
    {
        // Dropped while its input waits in a hook, before it reaches the summary step.
        let cancel = CancellationToken::new();
        let held = runner.run("hold", &cancel);
        tokio::pin!(held);
        tokio::select! {
            _ = &mut held => panic!("the held run must not finish"),
            () = waiting.notified() => {}
        }
    }
    let third = runner.run("try again", &CancellationToken::new()).await;
    assert_eq!(third.outcome, RunOutcome::Error, "{third:?}");
    assert!(
        third.error.as_deref().unwrap_or_default().contains("paused"),
        "{third:?}"
    );
    assert_eq!(provider.requests().len(), 1, "the failed summary is not paid for again");
}

#[tokio::test]
async fn a_successful_manual_summary_resumes_automatic_summaries() {
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        // The automatic attempt fails and pauses automatic summaries.
        FauxStep::text("not JSON"),
        // A manual summary succeeds.
        FauxStep::text(summary(&old)),
        // A long answer grows the context over the trigger again.
        FauxStep::text("y".repeat(3000)),
        // The next automatic attempt is made instead of being skipped.
        FauxStep::text("not JSON"),
    ]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(runner.run("continue", &cancel).await.outcome, RunOutcome::Error);
    let compact = runner.compact(&cancel).await;
    assert_eq!(compact.outcome, Outcome::Completed, "{compact:?}");
    let answered = runner.run("next", &cancel).await;
    assert_eq!(answered.outcome, RunOutcome::Completed, "{answered:?}");
    let resumed = runner.run("again", &cancel).await;
    assert_eq!(resumed.outcome, RunOutcome::Error, "{resumed:?}");
    assert_eq!(provider.requests().len(), 4);
}

#[tokio::test]
async fn an_input_that_cannot_fit_is_not_silently_truncated_or_sent_to_the_task_model() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("must not execute")]));
    let (runner, _) = RunnerSetup::new(config(provider.clone(), vec![])).build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let input = "界".repeat(1000);
    let report = runner.run(&input, &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Error);
    assert_eq!(report.turns, 0);
    assert!(provider.requests().is_empty());
    assert!(matches!(&runner.session().messages()[0], Message::User{content,..} if content == &input));
}

#[tokio::test]
async fn context_hooks_are_persisted_before_a_summary_sees_the_source() {
    struct Correct;
    #[async_trait]
    impl Hooks for Correct {
        async fn transform_context(
            &self,
            messages: &[Message],
            _ctx: &HookContext,
        ) -> Result<Vec<ContextEdit>, String> {
            Ok(
                if matches!(&messages[0], Message::User{content,..} if content.starts_with("observed")) {
                    vec![ContextEdit::ReplaceText {
                        index: 0,
                        text: "corrected ".repeat(600),
                    }]
                } else {
                    vec![]
                },
            )
        }
    }
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("answer"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(Correct);
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config)
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    assert_eq!(
        runner.run("go", &CancellationToken::new()).await.outcome,
        RunOutcome::Completed
    );
    let source = serde_json::to_string(&provider.requests()[0].messages).unwrap();
    assert!(source.contains("corrected corrected"));
    assert!(!source.contains("observed observed"));
    let entries = runner.session().entries().to_vec();
    assert!(matches!(entries[3], Entry::ContextEdit(_)));
    assert!(matches!(entries[4], Entry::CompactionStarted(_)));
}

#[tokio::test]
async fn a_context_plan_that_edits_a_summary_is_rejected_before_any_edit_is_written() {
    struct EditInputThenSummary;
    #[async_trait]
    impl Hooks for EditInputThenSummary {
        async fn transform_context(
            &self,
            messages: &[Message],
            _ctx: &HookContext,
        ) -> Result<Vec<ContextEdit>, String> {
            let input = messages
                .iter()
                .position(|m| matches!(m, Message::User{content,..} if content == "go"));
            let summary = messages
                .iter()
                .position(|m| matches!(m, Message::User{content,..} if content.contains("<context_summary>")));
            Ok(match (input, summary) {
                (Some(input), Some(summary)) => vec![
                    ContextEdit::ReplaceText {
                        index: input,
                        text: "go, corrected".into(),
                    },
                    ContextEdit::ReplaceText {
                        index: summary,
                        text: "rewritten summary".into(),
                    },
                ],
                _ => vec![],
            })
        }
    }
    let (store, old) = seeded().await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("answer"),
    ]));
    let mut config = config(provider.clone(), vec![]);
    config.hooks.push(EditInputThenSummary);
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config)
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Error);
    // A hook rejection, decided while validating the plan, not a failed session write.
    let error = report.error.unwrap();
    assert!(error.contains("transform_context"), "{error}");
    assert!(!error.contains("cannot persist"), "{error}");
    assert_eq!(provider.requests().len(), 1, "only the summary request was sent");
    let entries = runner.session().entries().to_vec();
    assert!(
        !entries.iter().any(|entry| matches!(entry, Entry::ContextEdit(_))),
        "no edit of the rejected plan may be written"
    );
}

#[tokio::test]
async fn pending_summary_shares_busy_cancel_and_drop_reuse_with_prompt() {
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::Hang, FauxStep::text("reused")]));
    let (runner, events) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let session = AgentSession::from(runner.with_compaction(budget()).unwrap());
    let parent = CancellationToken::new();
    let mut compact = Box::pin(session.compact_with_cancel(&parent));
    tokio::select! { biased; result = &mut compact => panic!("unexpected completion: {result:?}"), _ = tokio::task::yield_now() => {} }
    assert!(events.kinds().contains(&"compaction_start"));
    assert!(matches!(session.prompt("busy").await, Err(SessionError::Busy)));
    assert!(matches!(session.compact().await, Err(SessionError::Busy)));
    assert_eq!(
        session.set_model(ModelRef {
            provider: provider.clone(),
            model: "other".into()
        }),
        Err(SessionError::Busy)
    );
    drop(compact);
    assert!(!parent.is_cancelled());
    assert_eq!(session.prompt("after drop").await.unwrap().final_text, "reused");
}

#[tokio::test]
async fn aborting_summary_records_failure_and_the_session_is_immediately_reusable() {
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::Hang, FauxStep::text("reused")]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let session = AgentSession::from(runner.with_compaction(budget()).unwrap());
    let compact = session.compact();
    tokio::pin!(compact);
    tokio::select! { biased; result = &mut compact => panic!("unexpected completion: {result:?}"), _ = tokio::task::yield_now() => {} }
    assert!(session.abort());
    assert_eq!(compact.await.unwrap().outcome, Outcome::Aborted);
    assert_eq!(session.prompt("next").await.unwrap().final_text, "reused");
}

#[tokio::test]
async fn summary_timeout_is_bounded_and_keeps_original_context() {
    let (store, _) = seeded().await;
    let before = store.messages();
    let provider = Arc::new(FauxProvider::new([FauxStep::Hang]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            timeout: Duration::from_millis(10),
            ..budget()
        })
        .unwrap();
    let report = runner.compact(&CancellationToken::new()).await;
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.error.unwrap().contains("timed out"));
    assert_eq!(runner.session().messages(), before);
    assert!(matches!(
        runner.session().entries().last(),
        Some(Entry::CompactionFailed(_))
    ));
}

#[tokio::test]
async fn source_and_summary_byte_limits_stop_unbounded_requests_and_responses() {
    let (mut store, _) = seeded().await;
    store
        .append_message(Message::user("x".repeat(2 * 1024 * 1024)))
        .await
        .unwrap();
    store.append_message(Message::user("latest")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("should not be sent")]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner.with_compaction(budget()).unwrap();
    let report = runner.compact(&CancellationToken::new()).await;
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.error.unwrap().contains("source exceeds"));
    assert!(provider.requests().is_empty());

    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text("x".repeat(65 * 1024))]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let mut runner = runner.with_compaction(budget()).unwrap();
    let report = runner.compact(&CancellationToken::new()).await;
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(report.error.unwrap().contains("exceeds 65536"));
    assert!(runner.session().latest_compaction().is_none());
}

#[test]
fn invalid_budgets_are_rejected_before_use() {
    for invalid in [
        Config {
            trigger_bytes: Some(1),
            ..budget()
        },
        Config {
            keep_recent_bytes: 0,
            ..budget()
        },
        Config {
            max_summary_tokens: 0,
            ..budget()
        },
        Config {
            timeout: Duration::ZERO,
            ..budget()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
}

#[test]
fn a_trigger_whose_first_summary_cannot_be_read_is_rejected() {
    // A summary reads what precedes the kept recent messages, at most 2 MiB.
    let keep = 64 * 1024;
    let readable = Config {
        trigger_bytes: Some(2 * 1024 * 1024 + keep),
        keep_recent_bytes: keep,
        ..budget()
    };
    let unreadable = Config {
        trigger_bytes: Some(3 * 1024 * 1024),
        keep_recent_bytes: keep,
        ..budget()
    };

    assert!(readable.validate().is_ok());
    let error = unreadable.validate().unwrap_err();
    assert!(error.to_string().contains("triggerBytes"), "{error}");
}

#[tokio::test]
async fn the_run_timeout_also_bounds_automatic_compaction() {
    let (store, _) = seeded().await;
    let provider = Arc::new(FauxProvider::new([FauxStep::Hang]));
    let (runner, _) = RunnerSetup {
        session: store,
        limits: Limits {
            max_turns: None,
            timeout: Some(Duration::from_millis(10)),
        },
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(1024),
            ..budget()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Timeout);
    assert_eq!(report.turns, 1);
    assert_eq!(provider.requests().len(), 1);
    assert!(matches!(
        runner.session().entries().last(),
        Some(Entry::CompactionFailed(_))
    ));
}

#[tokio::test]
async fn a_single_large_old_message_does_not_force_the_entire_history_to_be_retained() {
    let mut store = SessionStore::in_memory(header());
    let old = store
        .append_message(Message::user("x".repeat(300 * 1024)))
        .await
        .unwrap();
    store.append_message(Message::user("recent input")).await.unwrap();
    let provider = Arc::new(FauxProvider::new([
        FauxStep::text(summary(&old)),
        FauxStep::text("answer"),
    ]));
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider, vec![]))
    }
    .build();
    let mut runner = runner
        .with_compaction(Config {
            trigger_bytes: Some(256 * 1024),
            ..Config::default()
        })
        .unwrap();
    let report = runner.run("continue", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.turns, 2);
}
