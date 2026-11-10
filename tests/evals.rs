//! The behaviour evaluation harness (`tests/common/evals.rs`) on scripted models: the plan,
//! paired runs through the compiled binary, the report and the artifacts. Real models need
//! the API owner's consent and run from `tests/real_api.rs`.
mod common;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use common::evals::{
    Artifacts, Flag, Harness, Observation, Status, Task, Variant, Variants, case, evaluate, format_report, plan,
    protocol, sample_cases, sample_variants, summarize,
};
use common::mock_server::{MockServer, Reply};

fn harness(root: PathBuf) -> Harness {
    Harness {
        root,
        model: "mock".to_string(),
        api_key_env: "ZX_EVALS_TEST_KEY".to_string(),
        extra_config: String::new(),
        timeout: Duration::from_secs(60),
    }
}

/// What the scripted model does in each arm: the control never edits and signs off only in
/// its second run; the treatment edits and signs off.
fn script(task: &Task) -> Vec<Reply> {
    match (task.id.as_str(), task.variant.as_str()) {
        ("fix-typo", "control") => vec![Reply::text("I cannot change files here.")],
        ("fix-typo", "treatment") => vec![
            Reply::tool_call(
                "c1",
                "edit_file",
                &json!({"path": "greeting.txt", "edits": [{"oldText": "Helo", "newText": "Hello"}]}),
            ),
            Reply::text("Fixed the typo.\nDONE"),
        ],
        ("sign-off", "control") if task.run == 2 => vec![Reply::text("Deploys happen on Tuesdays.\nDONE")],
        ("sign-off", "control") => vec![Reply::text("Deploys happen on Tuesdays.")],
        ("sign-off", "treatment") => vec![Reply::text("Deploys happen on Tuesdays.\nDONE")],
        other => panic!("no script for {other:?}"),
    }
}

fn comparison<'a>(report: &'a common::evals::Report, set: &str) -> &'a common::evals::Comparison {
    report
        .comparisons
        .iter()
        .find(|comparison| comparison.set == set)
        .unwrap_or_else(|| panic!("no comparison for {set}"))
}

fn observed(set: &str, id: &str, variant: &str, run: u32, score: Option<f64>) -> Observation {
    Observation {
        task: Task {
            set: set.into(),
            id: id.into(),
            variant: variant.into(),
            run,
        },
        status: if score.is_some() {
            Status::Scored
        } else {
            Status::Errored
        },
        score,
        reason: score.is_none().then(|| "timed out after 1 s".to_string()),
        exit_code: score.map(|_| 0),
        duration_ms: 10,
        tokens: Some(12),
        session: None,
    }
}

#[test]
fn the_plan_alternates_the_variants_between_runs() {
    let tasks = plan(&sample_cases(), &sample_variants(), 3);
    assert_eq!(tasks.len(), 12);
    let order: Vec<(&str, &str, u32)> = tasks
        .iter()
        .take(6)
        .map(|task| (task.id.as_str(), task.variant.as_str(), task.run))
        .collect();
    assert_eq!(
        order,
        [
            ("fix-typo", "control", 1),
            ("fix-typo", "treatment", 1),
            ("fix-typo", "treatment", 2),
            ("fix-typo", "control", 2),
            ("fix-typo", "control", 3),
            ("fix-typo", "treatment", 3),
        ]
    );
    assert!(tasks[6..].iter().all(|task| task.id == "sign-off"));
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a scripted evaluation: the runs, then the report and the artifacts"
)]
async fn paired_runs_through_the_binary_report_the_lift_and_keep_the_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = Artifacts::create(dir.path());
    let cases = sample_cases();
    let variants = sample_variants();
    let tasks = plan(&cases, &variants, 2);
    artifacts.write_protocol(&protocol("mock", &variants, &cases, 2));
    artifacts.write_expected(&tasks);
    let harness = harness(artifacts.dir.clone());

    let mut observations = Vec::new();
    for task in &tasks {
        let server = MockServer::start(script(task)).await;
        let observation = harness
            .run(
                task,
                case(&cases, task),
                variants.named(&task.variant),
                &server.base_url,
            )
            .await;
        artifacts.append_observation(&observation);
        observations.push(observation);
    }
    let report = summarize(&tasks, &observations, &variants);
    artifacts.write_report(&report);

    assert!(
        observations
            .iter()
            .all(|observation| observation.status == Status::Scored),
        "{observations:?}"
    );
    let edit = comparison(&report, "edit");
    assert_eq!((edit.pairs, edit.blocked_pairs), (2, 0));
    assert_eq!(
        (edit.control_pass_rate, edit.treatment_pass_rate),
        (Some(0.0), Some(1.0))
    );
    assert_eq!(edit.lift, Some(1.0));
    assert_eq!(edit.flags, [Flag::TreatmentSaturated]);
    let context = comparison(&report, "context");
    assert_eq!(
        (context.control_pass_rate, context.treatment_pass_rate),
        (Some(0.5), Some(1.0))
    );
    assert_eq!(context.lift, Some(0.5));
    assert_eq!(context.flags, [Flag::TreatmentSaturated, Flag::Flaky]);
    assert!(report.blocked.is_empty(), "{:?}", report.blocked);
    // Every arm reported its usage: the mock model counts 12 tokens per request.
    let control = &report.totals["control"];
    assert_eq!(
        (control.runs, control.scored, control.passed, control.tokens),
        (4, 4, 1, Some(48))
    );
    let treatment = &report.totals["treatment"];
    assert_eq!((treatment.runs, treatment.passed, treatment.tokens), (4, 4, Some(72)));
    // The reasons of the failed arms name what the grade saw.
    let failed: Vec<&str> = observations
        .iter()
        .filter_map(|observation| observation.reason.as_deref())
        .collect();
    assert_eq!(
        failed,
        [
            "greeting.txt is Some(\"Helo, world\\n\")",
            "greeting.txt is Some(\"Helo, world\\n\")",
            "the answer does not end with DONE",
        ]
    );

    let text = format_report(&report);
    assert!(
        text.contains("edit: pass rate +100.0 pp (treatment 100%, control 0%) [treatment-saturated]"),
        "{text}"
    );
    assert!(
        text.contains("context: pass rate +50.0 pp (treatment 100%, control 50%) [treatment-saturated, flaky]"),
        "{text}"
    );
    assert!(
        text.contains("control: 4 run(s), 4 scored, 1 passed, 48 tokens"),
        "{text}"
    );
    for name in [
        "protocol.json",
        "expected-runs.json",
        "observations.jsonl",
        "report.json",
        "report.txt",
    ] {
        assert!(artifacts.dir.join(name).is_file(), "{name}");
    }
    let lines: Vec<Value> = std::fs::read_to_string(artifacts.dir.join("observations.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[0]["variant"], "control");
    assert_eq!(lines[0]["status"], "scored");
    assert_eq!(lines[0]["score"], 0.0);
    assert_eq!(lines[1]["score"], 1.0);
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(artifacts.dir.join("expected-runs.json")).unwrap()).unwrap();
    assert_eq!(expected.as_array().unwrap().len(), 8);
    // Each arm kept its session and its workspace; the treatment's edit is in the file.
    let edited = artifacts.dir.join("treatment/edit-fix-typo-run1");
    assert!(edited.join("session.jsonl").is_file());
    assert_eq!(
        std::fs::read_to_string(edited.join("workspace/greeting.txt")).unwrap(),
        "Hello, world\n"
    );
    assert_eq!(
        std::fs::read_to_string(artifacts.dir.join("control/edit-fix-typo-run1/workspace/greeting.txt")).unwrap(),
        "Helo, world\n"
    );
    // The report on disk is the one in memory.
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(artifacts.dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(written["comparisons"][0]["lift"], 0.5);
    assert_eq!(written["comparisons"][0]["set"], "context");
}

#[tokio::test]
async fn an_arm_that_cannot_start_blocks_its_pair_and_withholds_the_headline() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = Artifacts::create(dir.path());
    let cases: Vec<_> = sample_cases()
        .into_iter()
        .filter(|case| case.id == "sign-off")
        .collect();
    // An agent file without front matter: the program refuses it before any request.
    let broken = dir.path().join("broken-agents");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join("unattended.md"), "no front matter\n").unwrap();
    let variants = Variants {
        control: sample_variants().control,
        treatment: Variant {
            name: "treatment",
            agents: broken,
        },
    };
    let tasks = plan(&cases, &variants, 1);
    let server = MockServer::start(vec![Reply::text("Deploys happen on Tuesdays.")]).await;

    let observations = evaluate(
        &harness(artifacts.dir.clone()),
        &cases,
        &variants,
        &tasks,
        &server.base_url,
        &artifacts,
    )
    .await;

    let treatment = observations
        .iter()
        .find(|observation| observation.task.variant == "treatment")
        .unwrap();
    assert_eq!(
        (treatment.status, treatment.score, treatment.exit_code),
        (Status::Errored, None, Some(2))
    );
    assert!(
        treatment
            .reason
            .as_deref()
            .unwrap()
            .starts_with("zx-agent refused to start:"),
        "{:?}",
        treatment.reason
    );
    let control = observations
        .iter()
        .find(|observation| observation.task.variant == "control")
        .unwrap();
    assert_eq!((control.status, control.score), (Status::Scored, Some(0.0)));
    let report = summarize(&tasks, &observations, &variants);
    let context = comparison(&report, "context");
    assert_eq!((context.pairs, context.blocked_pairs, context.lift), (0, 1, None));
    assert_eq!(
        report.blocked[0].reasons,
        [format!(
            "treatment arm errored: {}",
            treatment.reason.as_deref().unwrap()
        )]
    );
    assert_eq!(
        report.totals["treatment"].tokens,
        Some(0),
        "no scored arm, nothing to sum"
    );
    let text = format_report(&report);
    assert!(
        text.contains("context: pass rate withheld because pairs are blocked (treatment n/a, control n/a)"),
        "{text}"
    );
    assert!(
        text.contains("blocked context/sign-off/run-1: treatment arm errored: zx-agent refused to start:"),
        "{text}"
    );
}

#[test]
fn the_summary_flags_no_lift_negative_deltas_saturation_and_missing_arms() {
    let variants = sample_variants();
    let tasks: Vec<Task> = [("a", "same", 1), ("b", "worse", 1), ("c", "lost", 1), ("c", "twice", 1)]
        .into_iter()
        .flat_map(|(set, id, run)| {
            ["control", "treatment"].map(|variant| Task {
                set: set.into(),
                id: id.into(),
                variant: variant.into(),
                run,
            })
        })
        .collect();
    let observations = vec![
        observed("a", "same", "control", 1, Some(1.0)),
        observed("a", "same", "treatment", 1, Some(1.0)),
        observed("b", "worse", "control", 1, Some(1.0)),
        observed("b", "worse", "treatment", 1, Some(0.0)),
        observed("c", "lost", "control", 1, Some(1.0)),
        observed("c", "twice", "control", 1, Some(1.0)),
        observed("c", "twice", "treatment", 1, Some(1.0)),
        observed("c", "twice", "treatment", 1, Some(0.0)),
    ];

    let report = summarize(&tasks, &observations, &variants);

    let a = comparison(&report, "a");
    assert_eq!(
        (a.lift, a.flags.as_slice()),
        (
            Some(0.0),
            &[Flag::NoLift, Flag::ControlSaturated, Flag::TreatmentSaturated][..]
        )
    );
    let b = comparison(&report, "b");
    assert_eq!(
        (b.lift, b.flags.as_slice()),
        (Some(-1.0), &[Flag::NegativeDelta, Flag::ControlSaturated][..])
    );
    let c = comparison(&report, "c");
    assert_eq!((c.pairs, c.blocked_pairs, c.lift), (0, 2, None));
    let reasons: Vec<&[String]> = report
        .blocked
        .iter()
        .map(|blocked| blocked.reasons.as_slice())
        .collect();
    assert_eq!(
        reasons,
        [
            &["no treatment arm".to_string()][..],
            &["2 treatment arms".to_string()][..]
        ]
    );
    let text = format_report(&report);
    assert!(
        text.contains("b: pass rate -100.0 pp (treatment 0%, control 100%) [negative-delta, control-saturated]"),
        "{text}"
    );
}
