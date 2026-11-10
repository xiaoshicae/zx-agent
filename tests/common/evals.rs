//! A behaviour evaluation harness (pi: packages/evals): scenario cases run through the
//! compiled `zx-agent run` under two agents, repeated, paired and compared. A case seeds a
//! workspace, gives the prompt and grades the outcome deterministically; the harness owns the
//! plan, the isolation (a home, a workspace and a session file per run), the artifacts and the
//! report. Synthetic runs script the model with `MockServer`; real models need the API owner's
//! consent (`tests/real_api.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;

/// A scenario: files to seed, the prompt, and a deterministic grade.
pub(crate) struct Case {
    /// The eval set, such as `edit`; lift is reported per set.
    pub set: &'static str,
    pub id: &'static str,
    pub prompt: &'static str,
    /// Files in the workspace before the run, by relative path.
    pub files: &'static [(&'static str, &'static str)],
    /// Pass or fail, with the reason. A low score is data; panics are for broken invariants.
    pub grade: fn(&Outcome) -> Grade,
}

/// How a run is configured: a directory of agent files, copied into the arm's zx-agent home
/// as `agents/`, so that its `unattended.md` replaces the built-in agent `run` uses. Files
/// the agent names, such as context files, go beside it.
pub(crate) struct Variant {
    pub name: &'static str,
    pub agents: PathBuf,
}

/// The two arms of a comparison. Lift is the treatment's pass rate over the control's.
pub(crate) struct Variants {
    pub control: Variant,
    pub treatment: Variant,
}

impl Variants {
    pub(crate) fn named(&self, name: &str) -> &Variant {
        [&self.control, &self.treatment]
            .into_iter()
            .find(|variant| variant.name == name)
            .unwrap_or_else(|| panic!("no variant named {name:?}"))
    }
}

/// One arm: a case under a variant, in run number `run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Task {
    pub set: String,
    pub id: String,
    pub variant: String,
    pub run: u32,
}

/// Every (case, variant, run) arm. Odd runs go control first, even runs treatment first, to
/// reduce order bias (pi: plan.ts).
pub(crate) fn plan(cases: &[Case], variants: &Variants, runs_per_variant: u32) -> Vec<Task> {
    assert!(runs_per_variant >= 1, "runs per variant must be at least 1");
    let mut tasks = Vec::new();
    for case in cases {
        for run in 1..=runs_per_variant {
            let order = if run % 2 == 1 {
                [&variants.control, &variants.treatment]
            } else {
                [&variants.treatment, &variants.control]
            };
            for variant in order {
                tasks.push(Task {
                    set: case.set.to_string(),
                    id: case.id.to_string(),
                    variant: variant.name.to_string(),
                    run,
                });
            }
        }
    }
    tasks
}

pub(crate) fn case<'a>(cases: &'a [Case], task: &Task) -> &'a Case {
    cases
        .iter()
        .find(|case| case.set == task.set && case.id == task.id)
        .unwrap_or_else(|| panic!("no case {}/{}", task.set, task.id))
}

/// What a finished run left behind, for grading.
pub(crate) struct Outcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub workspace: PathBuf,
    pub session: PathBuf,
    pub duration: Duration,
}

impl Outcome {
    /// The final answer, as `run` printed it.
    pub(crate) fn answer(&self) -> &str {
        self.stdout.trim()
    }

    /// A workspace file's content; `None` when it does not exist or is not UTF-8, which a
    /// grade reports as a fact about the run.
    pub(crate) fn file(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.join(relative)).ok()
    }

    /// Tokens the run reported on its report line, when it got far enough to report.
    pub(crate) fn tokens(&self) -> Option<u64> {
        self.stderr.lines().rev().find_map(|line| {
            let fields = line.strip_prefix("[zx-agent] outcome=")?;
            fields
                .split_whitespace()
                .find_map(|field| field.strip_prefix("tokens="))
                .and_then(|count| count.parse().ok())
        })
    }
}

pub(crate) enum Grade {
    Pass,
    Fail(String),
}

/// Whether an arm produced a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Scored,
    /// The run could not start, was killed or timed out: no verdict, and the pair is blocked.
    Errored,
}

/// The normalized outcome of one arm, as written to `observations.jsonl`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Observation {
    #[serde(flatten)]
    pub task: Task,
    pub status: Status,
    /// 1 for a pass, 0 for a fail; absent when the arm errored.
    pub score: Option<f64>,
    pub reason: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Absent when the run did not report usage; never taken as zero.
    pub tokens: Option<u64>,
    pub session: Option<String>,
}

/// Runs arms through the compiled binary, each in its own directory under `root`.
pub(crate) struct Harness {
    pub root: PathBuf,
    pub model: String,
    pub api_key_env: String,
    /// Added at the top of every arm's config.toml, such as `maxTokens = 1024`.
    pub extra_config: String,
    /// Longest an arm may take; a run still going is killed and recorded as errored.
    pub timeout: Duration,
}

impl Harness {
    /// Where an arm's home, workspace and session live.
    pub(crate) fn dir(&self, task: &Task) -> PathBuf {
        self.root
            .join(&task.variant)
            .join(format!("{}-{}-run{}", task.set, task.id, task.run))
    }

    /// The program for one arm, with its own `HOME` and workspace, so that nothing of the
    /// user's environment is used; the variant's agents are copied into that home.
    fn command(
        &self,
        case: &Case,
        variant: &Variant,
        base_url: &str,
        workspace: &Path,
        home: &Path,
        session: &Path,
    ) -> Command {
        install_agents(&variant.agents, &home.join(".zx-agent/agents"));
        super::write_config(
            &home.join(".zx-agent"),
            &super::fake_model_config(&self.api_key_env, &self.extra_config, ""),
        );
        let mut command = super::zx_agent();
        command
            .arg("run")
            .args(["--model", &self.model])
            .arg("--session")
            .arg(session)
            .arg(case.prompt)
            .current_dir(workspace)
            .env("HOME", home)
            .env_remove("ZX_AGENT_HOME")
            .env("ZX_BASE_URL", base_url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Run one arm against the model at `base_url` and grade it.
    pub(crate) async fn run(&self, task: &Task, case: &Case, variant: &Variant, base_url: &str) -> Observation {
        let dir = self.dir(task);
        let workspace = dir.join("workspace");
        let home = dir.join("home");
        let session = dir.join("session.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        for (path, content) in case.files {
            let path = workspace.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        let started = Instant::now();
        let errored = |reason: String, exit_code: Option<i32>, duration: Duration| Observation {
            task: task.clone(),
            status: Status::Errored,
            score: None,
            reason: Some(reason),
            exit_code,
            duration_ms: millis(duration),
            tokens: None,
            session: None,
        };
        let mut command = self.command(case, variant, base_url, &workspace, &home, &session);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return errored(format!("cannot start zx-agent: {error}"), None, started.elapsed()),
        };
        // Dropping the future on timeout kills the child (`kill_on_drop`).
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return errored(format!("cannot wait for zx-agent: {error}"), None, started.elapsed()),
            Err(_) => {
                return errored(
                    format!("timed out after {} s", self.timeout.as_secs()),
                    None,
                    started.elapsed(),
                );
            }
        };
        let duration = started.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let Some(exit_code) = output.status.code() else {
            return errored("killed by a signal".to_string(), None, duration);
        };
        // A usage error means the run never started: it says nothing about the model.
        if exit_code == 2 {
            let last = stderr.lines().last().unwrap_or("no output").to_string();
            return errored(format!("zx-agent refused to start: {last}"), Some(exit_code), duration);
        }
        let outcome = Outcome {
            exit_code,
            stdout,
            stderr,
            workspace,
            session: session.clone(),
            duration,
        };
        let (score, reason) = match (case.grade)(&outcome) {
            Grade::Pass => (1.0, None),
            Grade::Fail(reason) => (0.0, Some(reason)),
        };
        Observation {
            task: task.clone(),
            status: Status::Scored,
            score: Some(score),
            reason,
            exit_code: Some(exit_code),
            duration_ms: millis(duration),
            tokens: outcome.tokens(),
            session: Some(session.display().to_string()),
        }
    }
}

/// Run every arm of `tasks` in order against one model, recording each observation as it
/// comes, so that an interrupted evaluation keeps what it did.
pub(crate) async fn evaluate(
    harness: &Harness,
    cases: &[Case],
    variants: &Variants,
    tasks: &[Task],
    base_url: &str,
    artifacts: &Artifacts,
) -> Vec<Observation> {
    let mut observations = Vec::with_capacity(tasks.len());
    for task in tasks {
        let observation = harness
            .run(task, case(cases, task), variants.named(&task.variant), base_url)
            .await;
        artifacts.append_observation(&observation);
        observations.push(observation);
    }
    observations
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What the report says about one eval set.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Comparison {
    pub set: String,
    /// Pairs with a verdict on both arms.
    pub pairs: usize,
    pub blocked_pairs: usize,
    /// Over the pairs that have both verdicts.
    pub control_pass_rate: Option<f64>,
    pub treatment_pass_rate: Option<f64>,
    /// Treatment minus control, as a fraction; withheld (absent) while any pair of the set is
    /// blocked, or when there is no pair.
    pub lift: Option<f64>,
    pub flags: Vec<Flag>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Flag {
    NoLift,
    NegativeDelta,
    ControlSaturated,
    TreatmentSaturated,
    /// A case passed in some runs of a variant and failed in others.
    Flaky,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BlockedPair {
    pub set: String,
    pub id: String,
    pub run: u32,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VariantTotals {
    pub runs: usize,
    pub scored: usize,
    pub passed: usize,
    /// The sum over the arms, only when every scored arm reported its usage.
    pub tokens: Option<u64>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    pub comparisons: Vec<Comparison>,
    pub blocked: Vec<BlockedPair>,
    pub totals: BTreeMap<String, VariantTotals>,
}

/// Pair the arms of `expected` and compare the variants. A pair needs exactly one scored
/// observation per variant; anything else blocks it, and a blocked pair withholds its set's
/// headline lift (pi: report.ts).
pub(crate) fn summarize(expected: &[Task], observations: &[Observation], variants: &Variants) -> Report {
    let mut groups: BTreeMap<(String, String, u32), Vec<&Observation>> = BTreeMap::new();
    for task in expected {
        groups.entry((task.set.clone(), task.id.clone(), task.run)).or_default();
    }
    for observation in observations {
        let task = &observation.task;
        groups
            .entry((task.set.clone(), task.id.clone(), task.run))
            .or_default()
            .push(observation);
    }
    let mut sets: BTreeMap<String, SetPairs> = BTreeMap::new();
    let mut blocked = Vec::new();
    for ((set, id, run), arms) in &groups {
        let mut reasons = Vec::new();
        let mut scores = [None, None];
        for (slot, variant) in [&variants.control, &variants.treatment].into_iter().enumerate() {
            let own: Vec<&&Observation> = arms.iter().filter(|arm| arm.task.variant == variant.name).collect();
            match own.as_slice() {
                [] => reasons.push(format!("no {} arm", variant.name)),
                [arm] => match arm.score {
                    Some(score) => scores[slot] = Some(score),
                    None => reasons.push(format!(
                        "{} arm errored: {}",
                        variant.name,
                        arm.reason.as_deref().unwrap_or("no reason")
                    )),
                },
                many => reasons.push(format!("{} {} arms", many.len(), variant.name)),
            }
        }
        let pairs = sets.entry(set.clone()).or_default();
        match scores {
            [Some(control), Some(treatment)] if reasons.is_empty() => {
                pairs.pairs.push((id.clone(), control, treatment));
            }
            _ => {
                pairs.blocked += 1;
                blocked.push(BlockedPair {
                    set: set.clone(),
                    id: id.clone(),
                    run: *run,
                    reasons,
                });
            }
        }
    }
    let comparisons = sets.into_iter().map(|(set, pairs)| pairs.compare(set)).collect();
    let mut totals: BTreeMap<String, VariantTotals> = BTreeMap::new();
    for variant in [&variants.control, &variants.treatment] {
        totals.insert(variant.name.to_string(), VariantTotals::default());
    }
    for observation in observations {
        let total = totals.entry(observation.task.variant.clone()).or_default();
        total.runs += 1;
        total.duration_ms += observation.duration_ms;
        if observation.status == Status::Scored {
            total.scored += 1;
            if observation.score == Some(1.0) {
                total.passed += 1;
            }
        }
    }
    for (name, total) in &mut totals {
        let scored = observations
            .iter()
            .filter(|observation| observation.task.variant == *name && observation.status == Status::Scored);
        total.tokens = scored.map(|observation| observation.tokens).sum();
    }
    Report {
        comparisons,
        blocked,
        totals,
    }
}

#[derive(Default)]
struct SetPairs {
    /// Case ID, control score, treatment score.
    pairs: Vec<(String, f64, f64)>,
    blocked: usize,
}

impl SetPairs {
    fn compare(self, set: String) -> Comparison {
        let mean = |scores: &dyn Fn(&(String, f64, f64)) -> f64| {
            (!self.pairs.is_empty()).then(|| self.pairs.iter().map(scores).sum::<f64>() / self.pairs.len() as f64)
        };
        let control = mean(&|pair| pair.1);
        let treatment = mean(&|pair| pair.2);
        let mut flags = Vec::new();
        if let (Some(control), Some(treatment)) = (control, treatment) {
            if control == treatment {
                flags.push(Flag::NoLift);
            } else if treatment < control {
                flags.push(Flag::NegativeDelta);
            }
            if control == 1.0 {
                flags.push(Flag::ControlSaturated);
            }
            if treatment == 1.0 {
                flags.push(Flag::TreatmentSaturated);
            }
        }
        let mut by_case: BTreeMap<(&str, usize), Vec<f64>> = BTreeMap::new();
        for (id, control, treatment) in &self.pairs {
            by_case.entry((id, 0)).or_default().push(*control);
            by_case.entry((id, 1)).or_default().push(*treatment);
        }
        if by_case
            .values()
            .any(|scores| scores.iter().any(|score| *score != scores[0]))
        {
            flags.push(Flag::Flaky);
        }
        let lift = match (control, treatment) {
            (Some(control), Some(treatment)) if self.blocked == 0 => Some(treatment - control),
            _ => None,
        };
        Comparison {
            set,
            pairs: self.pairs.len(),
            blocked_pairs: self.blocked,
            control_pass_rate: control,
            treatment_pass_rate: treatment,
            lift,
            flags,
        }
    }
}

/// The report as text, for the terminal and `report.txt`.
pub(crate) fn format_report(report: &Report) -> String {
    let mut text = String::new();
    let pairs: usize = report.comparisons.iter().map(|comparison| comparison.pairs).sum();
    text.push_str(&format!(
        "Behaviour evals: {pairs} pair(s), {} blocked\n",
        report.blocked.len()
    ));
    for comparison in &report.comparisons {
        let rates = |label: &str, rate: Option<f64>| match rate {
            Some(rate) => format!("{label} {:.0}%", rate * 100.0),
            None => format!("{label} n/a"),
        };
        let headline = match comparison.lift {
            Some(lift) => format!("pass rate {:+.1} pp", lift * 100.0),
            None if comparison.blocked_pairs > 0 => "pass rate withheld because pairs are blocked".to_string(),
            None => "pass rate withheld because there is no pair".to_string(),
        };
        let flags: Vec<String> = comparison
            .flags
            .iter()
            .map(|flag| serde_json::to_value(flag).unwrap().as_str().unwrap().to_string())
            .collect();
        text.push_str(&format!(
            "  {}: {headline} ({}, {}){}\n",
            comparison.set,
            rates("treatment", comparison.treatment_pass_rate),
            rates("control", comparison.control_pass_rate),
            if flags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", flags.join(", "))
            }
        ));
    }
    for (variant, total) in &report.totals {
        let tokens = total
            .tokens
            .map_or("tokens n/a".to_string(), |tokens| format!("{tokens} tokens"));
        text.push_str(&format!(
            "  {variant}: {} run(s), {} scored, {} passed, {tokens}, {:.1} s\n",
            total.runs,
            total.scored,
            total.passed,
            total.duration_ms as f64 / 1000.0
        ));
    }
    for blocked in &report.blocked {
        text.push_str(&format!(
            "  blocked {}/{}/run-{}: {}\n",
            blocked.set,
            blocked.id,
            blocked.run,
            blocked.reasons.join("; ")
        ));
    }
    text
}

/// One evaluation's directory: the protocol, the planned cohort, the observations as they
/// come, the report, and each arm's home, workspace and session under `<variant>/`.
pub(crate) struct Artifacts {
    pub dir: PathBuf,
}

impl Artifacts {
    /// A new directory under `parent`, named by the time and the process.
    pub(crate) fn create(parent: &Path) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let dir = parent.join(format!("{}_{}", now.as_millis(), std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    pub(crate) fn write_protocol(&self, protocol: &Value) {
        self.write("protocol.json", &serde_json::to_string_pretty(protocol).unwrap());
    }

    pub(crate) fn write_expected(&self, tasks: &[Task]) {
        self.write("expected-runs.json", &serde_json::to_string_pretty(tasks).unwrap());
    }

    pub(crate) fn append_observation(&self, observation: &Observation) {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("observations.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(observation).unwrap()).unwrap();
    }

    pub(crate) fn write_report(&self, report: &Report) {
        self.write("report.json", &serde_json::to_string_pretty(report).unwrap());
        self.write("report.txt", &format_report(report));
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).unwrap();
    }
}

/// What an evaluation was: the model, the variants, the cases and the plan's size.
pub(crate) fn protocol(model: &str, variants: &Variants, cases: &[Case], runs_per_variant: u32) -> Value {
    let variant = |variant: &Variant| json!({"name": variant.name, "agents": variant.agents.display().to_string()});
    json!({
        "model": model,
        "variants": {"control": variant(&variants.control), "treatment": variant(&variants.treatment)},
        "cases": cases.iter().map(|case| json!({"set": case.set, "id": case.id})).collect::<Vec<_>>(),
        "runsPerVariant": runs_per_variant,
        "tasks": cases.len() * 2 * runs_per_variant as usize,
    })
}

/// The sample cases: an edit the model has to make, and an instruction the project notes
/// give. The treatment agent adds the notes; the control is otherwise the same.
pub(crate) fn sample_cases() -> Vec<Case> {
    vec![
        Case {
            set: "edit",
            id: "fix-typo",
            prompt: "Fix the typo in greeting.txt so that it reads \"Hello, world\". Change nothing else.",
            files: &[("greeting.txt", "Helo, world\n")],
            grade: fixed_typo,
        },
        Case {
            set: "context",
            id: "sign-off",
            prompt: "Read notes.md and tell me on which day deploys happen.",
            files: &[("notes.md", "Deploys happen on Tuesdays.\n")],
            grade: signed_off,
        },
    ]
}

/// The agents under `tests/fixtures/evals/`, a directory for each arm.
pub(crate) fn sample_variants() -> Variants {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/evals");
    Variants {
        control: Variant {
            name: "control",
            agents: fixtures.join("control"),
        },
        treatment: Variant {
            name: "treatment",
            agents: fixtures.join("treatment"),
        },
    }
}

/// Copy the files of `from` into `to`, the `agents/` directory of an arm's zx-agent home.
fn install_agents(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, to.join(path.file_name().unwrap())).unwrap();
    }
}

fn fixed_typo(outcome: &Outcome) -> Grade {
    match outcome.file("greeting.txt").as_deref() {
        Some("Hello, world\n") => Grade::Pass,
        other => Grade::Fail(format!("greeting.txt is {other:?}")),
    }
}

fn signed_off(outcome: &Outcome) -> Grade {
    let answer = outcome.answer();
    if !answer.to_lowercase().contains("tuesday") {
        return Grade::Fail("the answer does not name Tuesday".to_string());
    }
    if answer.lines().last().is_some_and(|line| line.trim() == "DONE") {
        Grade::Pass
    } else {
        Grade::Fail("the answer does not end with DONE".to_string())
    }
}
