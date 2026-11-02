//! Incremental summaries (session format v5): a summary after an earlier one carries that
//! one's facts over unchanged and reads only the records after its boundary, plus earlier
//! records corrected since.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, Request, Usage, UserMessageKind};
use zx_agent::compaction::{Config, Outcome, SummaryMode};
use zx_agent::runner::Runner;
use zx_agent::session::{
    CompactionEntry, CompactionPlan, CompactionStartedEntry, CompactionSummary, Entry, MergedSummary, SessionStore,
    SummaryFact, SummaryRequest,
};

use common::{RunnerSetup, config, header, read_lines};

fn budget(mode: SummaryMode) -> Config {
    Config {
        keep_recent_bytes: 1,
        timeout: Duration::from_secs(1),
        mode,
        ..Config::default()
    }
}

/// An old record of `bytes` bytes and a recent input, which a summary keeps.
async fn seeded(store: &mut SessionStore, bytes: usize) -> String {
    let old = store
        .append_message(Message::user("observed ".repeat(bytes / 9)))
        .await
        .unwrap();
    store.append_message(Message::user("most recent input")).await.unwrap();
    old
}

fn runner(provider: &Arc<FauxProvider>, store: SessionStore, mode: SummaryMode) -> Runner {
    let (runner, _) = RunnerSetup {
        session: store,
        ..RunnerSetup::new(config(provider.clone(), vec![]))
    }
    .build();
    runner.with_compaction(budget(mode)).unwrap()
}

/// The content of a summary request.
fn input(request: &Request) -> &str {
    match &request.messages[1] {
        Message::User { content, .. } => content,
        other => panic!("a summary request ends with its input, not {other:?}"),
    }
}

/// What a summary request offers: the entry IDs of its records, and the IDs of the earlier
/// facts it carries (none for a first summary).
fn offered(request: &Request) -> (Vec<String>, Vec<String>) {
    let input: Value = serde_json::from_str(input(request)).unwrap();
    let records = if input.is_array() {
        input.clone()
    } else {
        input["records"].clone()
    };
    let ids = records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["entryId"].as_str().unwrap().to_string())
        .collect();
    let facts = input
        .get("previousFacts")
        .and_then(Value::as_array)
        .map(|facts| {
            facts
                .iter()
                .map(|fact| fact["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    (ids, facts)
}

/// A summary answer: one fact per text, each citing the last record offered; the first fact
/// supersedes the earlier facts at the given positions among those offered.
fn answer(texts: Vec<String>, supersede: Vec<usize>) -> FauxStep {
    FauxStep::compute(move |request| {
        let (records, earlier) = offered(request);
        let cite = records.last().unwrap().clone();
        let facts: Vec<Value> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let supersedes: Vec<&String> = if index == 0 {
                    supersede.iter().map(|position| &earlier[*position]).collect()
                } else {
                    Vec::new()
                };
                json!({"text": text, "sourceEntryIds": [cite], "supersedes": supersedes})
            })
            .collect();
        FauxStep::text(json!({"facts": facts, "inferences": ["A guess"], "plans": ["Continue"]}).to_string())
    })
}

fn texts(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| text.to_string()).collect()
}

fn attempts(store: &SessionStore) -> Vec<&CompactionStartedEntry> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::CompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect()
}

fn compactions(store: &SessionStore) -> Vec<&CompactionEntry> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Compaction(done) => Some(done),
            _ => None,
        })
        .collect()
}

fn fact_texts(compaction: &CompactionEntry) -> Vec<&str> {
    compaction.summary.facts.iter().map(|fact| fact.text.as_str()).collect()
}

/// The summary message the model sees.
fn summary_in_context(store: &SessionStore) -> String {
    store
        .messages()
        .into_iter()
        .find_map(|message| match message {
            Message::User {
                content,
                kind: UserMessageKind::Summary,
                ..
            } => Some(content),
            _ => None,
        })
        .expect("the context holds a summary")
}

/// Run two tasks, so that a later summary has records to read: a long answer to summarize,
/// then a short task that it keeps.
async fn add_tasks(runner: &mut Runner) {
    runner.run("task two", &CancellationToken::new()).await;
    runner.run("task three", &CancellationToken::new()).await;
}

fn task_answers() -> [FauxStep; 2] {
    [FauxStep::text("new observation ".repeat(400)), FauxStep::text("done")]
}

#[tokio::test]
async fn the_first_incremental_summary_reads_the_whole_prefix_and_numbers_its_facts() {
    let mut store = SessionStore::in_memory(header());
    let old = seeded(&mut store, 5400).await;
    let provider = Arc::new(FauxProvider::new([answer(texts(&["Old observation"]), vec![])]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);

    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    assert_eq!(offered(&provider.requests()[0]), (vec![old], Vec::new()));
    let attempt = attempts(runner.session())[0];
    assert_eq!(attempt.mode, SummaryMode::Incremental);
    assert_eq!(attempt.continues, None);
    let facts = &compactions(runner.session())[0].summary.facts;
    assert_eq!(facts[0].id.as_deref(), Some(format!("{}-1", attempt.id).as_str()));
}

#[tokio::test]
async fn a_later_summary_reads_only_the_earlier_facts_and_the_new_records() {
    let mut store = SessionStore::in_memory(header());
    let old = seeded(&mut store, 5400).await;
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        answer(texts(&["Old observation"]), vec![]),
        long,
        short,
        answer(texts(&["New observation"]), vec![]),
    ]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);
    runner.compact(&CancellationToken::new()).await;
    add_tasks(&mut runner).await;

    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    let request = &provider.requests()[3];
    assert!(
        !input(request).contains("observed observed"),
        "the old record was read again"
    );
    let (records, earlier) = offered(request);
    assert!(!records.contains(&old));
    let [first, second] = compactions(runner.session())[..] else {
        panic!("two summaries")
    };
    assert_eq!(earlier, [first.summary.facts[0].id.clone().unwrap()]);
    assert_eq!(attempts(runner.session())[1].continues.as_ref(), Some(&first.id));
    assert_eq!(fact_texts(second), ["Old observation", "New observation"]);
    // The earlier fact is carried over verbatim.
    assert_eq!(second.summary.facts[0], first.summary.facts[0]);
    let context = summary_in_context(runner.session());
    assert!(
        context.contains("Old observation") && context.contains("New observation"),
        "{context}"
    );
}

#[tokio::test]
async fn a_new_fact_can_supersede_an_earlier_one() {
    let mut store = SessionStore::in_memory(header());
    seeded(&mut store, 5400).await;
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        answer(texts(&["The pod keeps restarting"]), vec![]),
        long,
        short,
        answer(texts(&["The pod recovered"]), vec![0]),
    ]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);
    runner.compact(&CancellationToken::new()).await;
    add_tasks(&mut runner).await;

    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    let [first, second] = compactions(runner.session())[..] else {
        panic!("two summaries")
    };
    let restarting = first.summary.facts[0].id.clone().unwrap();
    assert_eq!(fact_texts(second), ["The pod recovered"]);
    assert_eq!(second.superseded, std::slice::from_ref(&restarting));
    assert_eq!(second.summary.facts[0].supersedes, [restarting]);
    assert!(!summary_in_context(runner.session()).contains("keeps restarting"));
    // The superseded fact stays in the earlier record.
    assert_eq!(fact_texts(first), ["The pod keeps restarting"]);
}

#[tokio::test]
async fn the_oldest_earlier_facts_are_evicted_to_stay_within_the_limits() {
    let mut store = SessionStore::in_memory(header());
    seeded(&mut store, 64 * 1024).await;
    let many: Vec<String> = (1..=128).map(|n| format!("Fact {n}")).collect();
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        answer(many, vec![]),
        long,
        short,
        answer(texts(&["New one", "New two", "New three"]), vec![]),
    ]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);
    runner.compact(&CancellationToken::new()).await;
    add_tasks(&mut runner).await;

    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    let [first, second] = compactions(runner.session())[..] else {
        panic!("two summaries")
    };
    let oldest: Vec<String> = first.summary.facts[..3]
        .iter()
        .map(|fact| fact.id.clone().unwrap())
        .collect();
    assert_eq!(second.evicted, oldest);
    assert_eq!(second.summary.facts.len(), 128);
    assert_eq!(fact_texts(second)[0], "Fact 4");
    assert_eq!(fact_texts(second)[125..], ["New one", "New two", "New three"]);
}

#[tokio::test]
async fn a_model_cannot_number_its_own_facts() {
    let mut store = SessionStore::in_memory(header());
    let old = seeded(&mut store, 5400).await;
    let provider = Arc::new(FauxProvider::new([FauxStep::text(
        json!({"facts": [{"id": "x-1", "text": "Old", "sourceEntryIds": [old]}], "inferences": [], "plans": []})
            .to_string(),
    )]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);

    let report = runner.compact(&CancellationToken::new()).await;

    assert_eq!(report.outcome, Outcome::Failed);
    let error = report.error.unwrap();
    assert!(error.contains("unknown field `id`"), "{error}");
    assert!(compactions(runner.session()).is_empty());
}

/// A first incremental summary with one fact per record of `cited`, written directly.
async fn summarize_directly(store: &mut SessionStore, first_kept: &str, cited: &[(&str, &str)]) -> String {
    let source_entry_ids: Vec<String> = store
        .raw_projection()
        .into_iter()
        .take_while(|item| item.entry_id != first_kept)
        .map(|item| item.entry_id)
        .collect();
    let attempt = store
        .begin_compaction(CompactionPlan {
            first_kept_entry_id: first_kept.into(),
            source_entry_ids,
            request: SummaryRequest {
                provider: "faux".into(),
                model: "m".into(),
                instructions: "Summarize".into(),
                max_tokens: 100,
            },
            input_bytes: 1000,
            mode: SummaryMode::Incremental,
            continues: None,
        })
        .await
        .unwrap();
    let facts = cited
        .iter()
        .enumerate()
        .map(|(index, (text, source))| SummaryFact {
            id: Some(format!("{attempt}-{}", index + 1)),
            text: text.to_string(),
            source_entry_ids: vec![source.to_string()],
            supersedes: Vec::new(),
        })
        .collect();
    let merged = MergedSummary {
        summary: CompactionSummary {
            facts,
            inferences: vec!["A guess".into()],
            plans: vec!["Continue".into()],
        },
        superseded: Vec::new(),
        evicted: Vec::new(),
    };
    store
        .append_merged_compaction(&attempt, merged, Usage::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_correction_drops_only_the_facts_that_cite_the_corrected_record() {
    let mut store = SessionStore::in_memory(header());
    let one = store
        .append_message(Message::user("first record ".repeat(400)))
        .await
        .unwrap();
    let two = store
        .append_message(Message::user("second record ".repeat(400)))
        .await
        .unwrap();
    let kept = store.append_message(Message::user("latest input")).await.unwrap();
    summarize_directly(&mut store, &kept, &[("About one", &one), ("About two", &two)]).await;

    store
        .append_context_edit(&one, Some("first record, corrected".into()))
        .await
        .unwrap();

    let context = summary_in_context(&store);
    assert!(!context.contains("About one"), "{context}");
    assert!(context.contains("About two"), "{context}");
}

#[tokio::test]
async fn the_next_summary_reads_a_corrected_record_again_without_its_old_facts() {
    let mut store = SessionStore::in_memory(header());
    let one = store
        .append_message(Message::user("first record ".repeat(400)))
        .await
        .unwrap();
    let two = store
        .append_message(Message::user("second record ".repeat(400)))
        .await
        .unwrap();
    let kept = store.append_message(Message::user("latest input")).await.unwrap();
    summarize_directly(&mut store, &kept, &[("About one", &one), ("About two", &two)]).await;
    store
        .append_context_edit(&one, Some("first record, corrected".into()))
        .await
        .unwrap();
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        long,
        short,
        answer(texts(&["About one, corrected"]), vec![]),
    ]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);
    add_tasks(&mut runner).await;

    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    let request = &provider.requests()[2];
    let (records, earlier) = offered(request);
    assert_eq!(records[0], one, "the corrected record is read again first");
    assert!(!records.contains(&two));
    assert!(input(request).contains("first record, corrected"));
    assert_eq!(earlier.len(), 1, "only the fact about the second record still holds");
    let context = summary_in_context(runner.session());
    assert!(
        context.contains("About two") && context.contains("About one, corrected"),
        "{context}"
    );
    assert!(!context.contains("About one ["), "{context}");
}

#[tokio::test]
async fn an_incremental_summary_can_continue_a_full_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    seeded(&mut store, 5400).await;
    let provider = Arc::new(FauxProvider::new([answer(texts(&["Old observation"]), vec![])]));
    let mut full = runner(&provider, store, SummaryMode::Full);
    assert_eq!(
        full.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );
    drop(full);
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        long,
        short,
        answer(texts(&["New observation"]), vec![]),
    ]));
    let mut incremental = runner(&provider, SessionStore::open(&path).unwrap(), SummaryMode::Incremental);
    add_tasks(&mut incremental).await;

    assert_eq!(
        incremental.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );

    let [full_attempt, incremental_attempt] = attempts(incremental.session())[..] else {
        panic!("two attempts")
    };
    let [first, second] = compactions(incremental.session())[..] else {
        panic!("two summaries")
    };
    // The full summary's facts have no IDs: they are numbered after its attempt.
    assert_eq!(first.summary.facts[0].id, None);
    let derived = format!("{}-1", full_attempt.id);
    assert_eq!(offered(&provider.requests()[2]).1, std::slice::from_ref(&derived));
    assert_eq!(incremental_attempt.continues.as_ref(), Some(&first.id));
    assert_eq!(fact_texts(second), ["Old observation", "New observation"]);
    assert_eq!(second.summary.facts[0].id.as_deref(), Some(derived.as_str()));
    drop(incremental);
    assert!(SessionStore::read(&path).is_ok(), "the session loads again");
}

/// A file-backed session with two incremental summaries, and the requests it sent.
async fn two_summaries_in(path: &Path) -> Vec<Request> {
    let mut store = SessionStore::create(path, header()).unwrap();
    seeded(&mut store, 5400).await;
    let [long, short] = task_answers();
    let provider = Arc::new(FauxProvider::new([
        answer(texts(&["Old observation"]), vec![]),
        long,
        short,
        answer(texts(&["New observation"]), vec![]),
    ]));
    let mut runner = runner(&provider, store, SummaryMode::Incremental);
    runner.compact(&CancellationToken::new()).await;
    add_tasks(&mut runner).await;
    assert_eq!(
        runner.compact(&CancellationToken::new()).await.outcome,
        Outcome::Completed
    );
    provider.requests()
}

#[tokio::test]
async fn incremental_summary_requests_are_rebuilt_from_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let sent = two_summaries_in(&path).await;

    let store = SessionStore::read(&path).unwrap();
    let rebuilt: Vec<Request> = store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(store.replay_request(&started.id).unwrap().1),
            _ => None,
        })
        .collect();

    assert_eq!(rebuilt, sent);
    assert_eq!(store.header().version, 5);
}

#[tokio::test]
async fn loading_rejects_a_summary_that_breaks_the_incremental_rules() {
    let dir = tempfile::tempdir().unwrap();
    let valid = dir.path().join("valid.jsonl");
    two_summaries_in(&valid).await;
    let lines = read_lines(&valid);
    let last = lines.iter().rposition(|line| line["type"] == "compaction").unwrap();
    // The second summary carries the first one's fact, then adds its own.
    assert_eq!(lines[last]["summary"]["facts"].as_array().unwrap().len(), 2);
    let mut rewritten = lines[last].clone();
    rewritten["summary"]["facts"][0]["text"] = json!("Rewritten observation");
    let mut evicted_unknown = lines[last].clone();
    evicted_unknown["evicted"] = json!(["nope-1"]);
    let mut uncited = lines[last].clone();
    uncited["summary"]["facts"][1]["sourceEntryIds"] = json!([lines[1]["id"]]);
    let mut renumbered = lines[last].clone();
    renumbered["summary"]["facts"][1]["id"] = json!(format!("{}-2", lines[last]["attemptId"].as_str().unwrap()));
    let mut unknown_superseded = lines[last].clone();
    unknown_superseded["summary"]["facts"][1]["supersedes"] = json!(["nope-1"]);
    unknown_superseded["superseded"] = json!(["nope-1"]);
    for (name, tampered, reason) in [
        ("rewritten", rewritten, "earlier facts must be carried over unchanged"),
        (
            "evicted",
            evicted_unknown,
            "evicted facts must be the oldest earlier facts",
        ),
        ("uncited", uncited, "is not in the source messages"),
        ("renumbered", renumbered, "must be numbered"),
        (
            "superseded",
            unknown_superseded,
            "which is not an earlier fact that still holds",
        ),
    ] {
        let path = dir.path().join(format!("{name}.jsonl"));
        let mut changed = lines.clone();
        changed[last] = tampered;
        let text: Vec<String> = changed.iter().map(Value::to_string).collect();
        std::fs::write(&path, text.join("\n") + "\n").unwrap();

        let error = SessionStore::read(&path).err().expect("a tampered summary is rejected");

        assert!(format!("{error:#}").contains(reason), "{name}: {error:#}");
    }
}

/// The sizes of the summary requests over `rounds` rounds of two tasks and a summary.
async fn summary_request_sizes(mode: SummaryMode, rounds: usize) -> Vec<usize> {
    let mut store = SessionStore::in_memory(header());
    seeded(&mut store, 5400).await;
    let mut steps = vec![answer(texts(&["Round 0"]), vec![])];
    for round in 1..=rounds {
        let [long, short] = task_answers();
        steps.extend([long, short, answer(vec![format!("Round {round}")], vec![])]);
    }
    let provider = Arc::new(FauxProvider::new(steps));
    let mut runner = runner(&provider, store, mode);
    runner.compact(&CancellationToken::new()).await;
    for _ in 1..=rounds {
        add_tasks(&mut runner).await;
        assert_eq!(
            runner.compact(&CancellationToken::new()).await.outcome,
            Outcome::Completed
        );
    }
    provider
        .requests()
        .iter()
        .filter(|request| request.tools.is_empty() && request.messages.len() == 2)
        .map(|request| input(request).len())
        .collect()
}

#[tokio::test]
async fn incremental_summary_requests_stay_bounded_while_full_ones_grow() {
    let full = summary_request_sizes(SummaryMode::Full, 6).await;
    let incremental = summary_request_sizes(SummaryMode::Incremental, 6).await;

    assert_eq!((full.len(), incremental.len()), (7, 7));
    // Each round adds about 6.5 KiB of records: a full summary reads them all again.
    assert!(full[6] > full[1] * 3, "{full:?}");
    // An incremental one reads only the last round, plus a line per earlier fact.
    assert!(
        incremental[1..].iter().all(|size| *size < incremental[1] * 2),
        "{incremental:?}"
    );
    assert!(incremental[6] * 3 < full[6], "{incremental:?} {full:?}");
}
