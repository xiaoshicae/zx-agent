//! Who a run answers: the session's labels and the actor of each input reach the policies
//! and the hooks, and the actor is recorded with the input.
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Message, ToolCall};
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::input::Input;
use zx_agent::policy::{PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::runner::RunOutcome;
use zx_agent::tools::ToolOutput;
use zx_agent::transcript::{CustomMessage, Entry, SessionHeader, Transcript};

use common::{Driven, EchoTool, RunnerSetup, config, header};

/// What a policy or a hook was told about the run: the labels and the actor.
type Told = (BTreeMap<String, String>, Option<String>);

/// Remembers what it was told at each call it was asked about.
#[derive(Default)]
struct RecordingPolicy(Mutex<Vec<Told>>);

#[async_trait]
impl ToolPolicy for RecordingPolicy {
    async fn check(&self, _call: &ToolCall, ctx: &PolicyContext) -> PolicyDecision {
        self.0.lock().unwrap().push((ctx.labels.clone(), ctx.actor.clone()));
        PolicyDecision::Allow
    }
}

/// A callback of a hook, and the actor it was told there.
type Heard = (&'static str, Option<String>);

/// Remembers the actor it was told at each input and after each tool call.
#[derive(Clone, Default)]
struct RecordingHook(Arc<Mutex<Vec<Heard>>>);

#[async_trait]
impl Hook for RecordingHook {
    async fn on_input(&self, text: String, ctx: &HookContext) -> HookResult<String> {
        self.0.lock().unwrap().push(("on_input", ctx.actor.clone()));
        Ok(text)
    }

    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, ctx: &HookContext) -> HookResult<ToolOutput> {
        self.0.lock().unwrap().push(("after_tool", ctx.actor.clone()));
        Ok(output)
    }
}

fn sent_by(actor: &str, text: &str) -> Input {
    Input::User {
        text: text.into(),
        actor: Some(actor.into()),
    }
}

/// The model's part of one run: it calls `echo`, then answers.
fn one_call() -> [FauxStep; 2] {
    [
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]
}

/// A session over `transcript` whose model calls `echo` once in each of `runs` runs, with a
/// policy and a hook that remember what they were told.
fn observed(transcript: Transcript, runs: usize) -> (Driven, Arc<RecordingPolicy>, RecordingHook) {
    let provider = Arc::new(FauxProvider::new((0..runs).flat_map(|_| one_call())));
    let policy = Arc::new(RecordingPolicy::default());
    let hook = RecordingHook::default();
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.policies.push("recording", policy.clone());
    config.hooks.push(hook.clone());
    let (runner, _) = RunnerSetup {
        session: transcript,
        ..RunnerSetup::new(config)
    }
    .build();
    (runner, policy, hook)
}

/// The actor recorded with each message of `transcript` that a person or a hook sent.
fn recorded_actors(transcript: &Transcript) -> Vec<Option<String>> {
    transcript
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(entry) if matches!(entry.message, Message::User { .. }) => Some(entry.actor.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_policy_is_told_the_labels_of_the_session_and_who_sent_the_input() {
    let labels = BTreeMap::from([("tenant".to_string(), "acme".to_string())]);
    let transcript = Transcript::in_memory(SessionHeader {
        labels: labels.clone(),
        ..header()
    });
    let (session, policy, _) = observed(transcript, 1);

    let report = session.prompt(sent_by("alice", "go")).await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(*policy.0.lock().unwrap(), [(labels, Some("alice".to_string()))]);
    let actors = session.with_transcript(recorded_actors).unwrap();
    assert_eq!(actors, [Some("alice".to_string())]);
}

#[tokio::test]
async fn hooks_and_policies_are_told_the_sender_of_the_latest_input_a_person_sent() {
    let (session, policy, hook) = observed(Transcript::in_memory(header()), 1);
    // Taken before the first request: Bob's words, then a note from an extension.
    session.queues().steer_message(sent_by("bob", "and this"));
    session.queues().steer_message(Input::Custom(CustomMessage {
        custom_type: "note".into(),
        content: "a reminder".into(),
        display: false,
        details: None,
    }));

    session.prompt(sent_by("alice", "go")).await.unwrap();

    let bob = Some("bob".to_string());
    assert_eq!(
        *hook.0.lock().unwrap(),
        [
            ("on_input", Some("alice".to_string())),
            ("on_input", bob.clone()),
            // The extension's note is nobody's input: the run still answers Bob.
            ("after_tool", bob.clone()),
        ]
    );
    let told = policy.0.lock().unwrap();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].1, bob);
    let actors = session.with_transcript(recorded_actors).unwrap();
    assert_eq!(actors, [Some("alice".to_string()), bob]);
}

#[tokio::test]
async fn an_input_that_names_nobody_is_answered_for_nobody() {
    let (session, policy, _) = observed(Transcript::in_memory(header()), 2);
    session.prompt(sent_by("alice", "go")).await.unwrap();

    session.prompt("and again").await.unwrap();

    let alice = Some("alice".to_string());
    let told: Vec<Option<String>> = policy
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(_, actor)| actor.clone())
        .collect();
    assert_eq!(told, [alice.clone(), None]);
    assert_eq!(session.with_transcript(recorded_actors).unwrap(), [alice, None]);
}

#[tokio::test]
async fn a_run_without_new_input_answers_whoever_sent_the_last_one() {
    let mut transcript = Transcript::in_memory(header());
    transcript
        .append_message_from(Message::user("go"), Some("alice".into()))
        .await
        .unwrap();
    let (session, policy, hook) = observed(transcript, 1);

    let report = session.continue_run().await.unwrap();

    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(policy.0.lock().unwrap()[0].1.as_deref(), Some("alice"));
    assert_eq!(*hook.0.lock().unwrap(), [("after_tool", Some("alice".to_string()))]);
}
