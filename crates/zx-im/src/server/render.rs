//! The text that replaces a run's status message once the conversation's work has ended.

use zx_agent::compaction;
use zx_agent::events::{Budget, RunOutcome, RunReport};

use super::Replies;
use super::outlet::Finished;

/// What ends a piece of work in the conversation.
#[derive(Debug, PartialEq)]
pub(super) enum Ending {
    /// The model's answer, with a note when the run stopped short of finishing it.
    Answer(String),
    /// How the work ended when it gave no answer: cancelled, timed out, failed, without text,
    /// or a compaction's result.
    Notice(String),
}

/// What ends the work that `ended`: its answer, or why it gave none.
pub(super) fn ending_of(ended: Result<Finished, String>, replies: &Replies) -> Ending {
    match ended {
        Ok(Finished::Run(report)) => render(&report, replies),
        Ok(Finished::Compaction(report)) => Ending::Notice(render_compaction(&report, replies)),
        Err(error) => Ending::Notice(format!("{}{error}", replies.failed)),
    }
}

/// What ends a run.
fn render(report: &RunReport, replies: &Replies) -> Ending {
    let answer = report.final_text.trim();
    let noted = |note: &str| {
        if answer.is_empty() {
            Ending::Notice(note.to_string())
        } else {
            Ending::Answer(format!("{answer}\n\n{note}"))
        }
    };
    match report.outcome {
        RunOutcome::Completed if answer.is_empty() => Ending::Notice(replies.no_answer.clone()),
        RunOutcome::Completed => Ending::Answer(answer.to_string()),
        RunOutcome::Truncated => noted(&replies.truncated),
        RunOutcome::BudgetExhausted { budget, truncated } => {
            let reached = match budget {
                Budget::ModelRequests => &replies.model_request_limit,
                Budget::Tokens => &replies.token_budget,
                Budget::Cost => &replies.cost_budget,
            };
            if truncated {
                noted(&format!("{}\n{reached}", replies.truncated))
            } else {
                noted(reached)
            }
        }
        // The last answer in the session may be an earlier run's, so it is not repeated.
        RunOutcome::TimedOut => Ending::Notice(replies.timed_out.clone()),
        RunOutcome::Cancelled => Ending::Notice(replies.cancelled.clone()),
        RunOutcome::Failed => Ending::Notice(failure(report.error.as_deref(), replies)),
    }
}

/// The text that replaces the status message at the end of `/compact`.
fn render_compaction(report: &compaction::Report, replies: &Replies) -> String {
    match report.outcome {
        compaction::Outcome::Completed => replies.compacted.clone(),
        compaction::Outcome::Noop => replies.nothing_to_compact.clone(),
        compaction::Outcome::Cancelled => replies.cancelled.clone(),
        compaction::Outcome::Failed => failure(report.error.as_deref(), replies),
    }
}

/// That the work failed, with `error` when there is one.
fn failure(error: Option<&str>, replies: &Replies) -> String {
    match error {
        Some(error) => format!("{}{error}", replies.failed),
        // The reply leads into the error; without one, drop the separator it ends with,
        // whatever the reply's language.
        None => replies
            .failed
            .trim_end_matches(|c: char| c.is_whitespace() || c == ':' || c == '\u{ff1a}')
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Usage;

    use super::*;

    fn report(outcome: RunOutcome, final_text: &str, error: Option<&str>) -> RunReport {
        RunReport {
            outcome,
            final_text: final_text.into(),
            model_requests: 1,
            retries: 0,
            usage: Usage::default(),
            tool_usage: std::collections::BTreeMap::new(),
            denied_calls: 0,
            error: error.map(String::from),
            session_id: "session".into(),
        }
    }

    #[test]
    fn a_completed_run_posts_its_answer() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::Completed, " done \n", None), &replies),
            Ending::Answer("done".into())
        );
        assert_eq!(
            render(&report(RunOutcome::Completed, "", None), &replies),
            Ending::Notice(replies.no_answer.clone())
        );
    }

    #[test]
    fn an_unfinished_answer_says_why_it_stopped() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::Truncated, "part", None), &replies),
            Ending::Answer(format!("part\n\n{}", replies.truncated))
        );
        let exhausted = |budget, truncated| RunOutcome::BudgetExhausted { budget, truncated };
        assert_eq!(
            render(&report(exhausted(Budget::ModelRequests, false), "", None), &replies),
            Ending::Notice(replies.model_request_limit.clone())
        );
        assert_eq!(
            render(&report(exhausted(Budget::Tokens, false), "part", None), &replies),
            Ending::Answer(format!("part\n\n{}", replies.token_budget))
        );
        // Cut off as well: both are said.
        assert_eq!(
            render(&report(exhausted(Budget::Cost, true), "part", None), &replies),
            Ending::Answer(format!("part\n\n{}\n{}", replies.truncated, replies.cost_budget))
        );
    }

    #[test]
    fn a_run_that_did_not_finish_does_not_repeat_an_earlier_answer() {
        let replies = Replies::default();
        assert_eq!(
            render(&report(RunOutcome::TimedOut, "earlier", None), &replies),
            Ending::Notice(replies.timed_out.clone())
        );
        assert_eq!(
            render(&report(RunOutcome::Cancelled, "earlier", None), &replies),
            Ending::Notice(replies.cancelled.clone())
        );
        assert_eq!(
            render(&report(RunOutcome::Failed, "earlier", Some("HTTP 500")), &replies),
            Ending::Notice(format!("{}HTTP 500", replies.failed))
        );
    }

    #[test]
    fn a_compaction_says_how_it_ended() {
        use compaction::Outcome;

        let replies = Replies::default();
        let ended = |outcome, error: Option<&str>| compaction::Report {
            outcome,
            requests: 1,
            usage: Usage::default(),
            input_bytes: 10,
            output_bytes: 5,
            entry_id: None,
            error: error.map(String::from),
        };
        let shown = |outcome, error| render_compaction(&ended(outcome, error), &replies);
        assert_eq!(shown(Outcome::Completed, None), replies.compacted);
        assert_eq!(shown(Outcome::Noop, None), replies.nothing_to_compact);
        assert_eq!(shown(Outcome::Cancelled, None), replies.cancelled);
        assert_eq!(
            shown(Outcome::Failed, Some("no summary")),
            format!("{}no summary", replies.failed)
        );
        // Without an error the reply does not end with the separator that leads into one.
        assert_eq!(shown(Outcome::Failed, None), "The run failed");
        // A cancelled one is told as a cancelled run is: by the stop when the server stops.
        assert!(Finished::Compaction(ended(Outcome::Cancelled, None)).cancelled());
        assert!(!Finished::Compaction(ended(Outcome::Completed, None)).cancelled());
        assert!(Finished::Run(report(RunOutcome::Cancelled, "", None)).cancelled());
    }
}
