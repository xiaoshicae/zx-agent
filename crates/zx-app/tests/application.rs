//! A non-CLI host uses explicit configuration, owns the assembled sessions, and gives the
//! agent parts of its own.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;
use zx_agent::ai::ToolCall;
use zx_agent::compaction::{Summarized, Summarizer, SummarySource};
use zx_agent::extensions::Extension;
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::policy::{PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::tools::{Tool, ToolOutput};
use zx_agent::transcript::{SessionName, SessionQuery};
use zx_app::config::{Builtin, Config};
use zx_app::{Application, Approvals, Overrides, SessionPlace};
use zx_testing::EchoTool;
use zx_testing::mock_server::{Captured, MockServer, Reply};

/// The value of a secret file of the configuration, which tool results must not show.
const SECRET: &str = "s3cret-value-of-the-host";

fn configured() -> Config {
    let mut config = Config {
        model: Some("configured-model".into()),
        ..Config::default()
    };
    config.provider.openai.base_url = "http://127.0.0.1:1/v1".into();
    config.provider.api_key_env = "ZX_APPLICATION_TEST_UNUSED_KEY".into();
    config
}

/// The user's file of the built-in agent `unattended` in `home`, with `front` in its front
/// matter.
fn agent_in(home: &Path, front: &str) {
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::write(
        home.join("agents/unattended.md"),
        format!("+++\nextends = \"unattended\"\n{front}\n[context]\ndiscover = false\n+++\nExplicit host role.\n"),
    )
    .unwrap();
}

fn role_in(home: &Path) {
    agent_in(home, "tools = []");
}

/// A host whose agent has `front` in its file, asking `server`, its sessions working in
/// `dir`, which also holds the home and a secret file.
async fn host(dir: &Path, front: &str, server: &MockServer) -> Application {
    let home = dir.join("home");
    agent_in(&home, front);
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, SECRET).unwrap();
    let mut config = configured();
    config.provider.openai.base_url = server.base_url.clone();
    config.secret_files.push(secret);
    let overrides = Overrides {
        cwd: Some(dir.to_path_buf()),
        ..Overrides::default()
    };
    Application::from_config(config, Some(home), overrides, Builtin::Unattended)
        .await
        .unwrap()
}

/// A model that calls `tool` once with the text `found it`, then answers.
async fn calling(tool: &str) -> MockServer {
    MockServer::start(vec![
        Reply::tool_call("c1", tool, &json!({"text": "found it"})),
        Reply::text("done"),
    ])
    .await
}

/// Ask the agent of `app` once, in a session kept in memory, with nobody to approve.
async fn ask(app: &Application) {
    let configured = app
        .build(app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .unwrap();
    let session = configured.create(None).await.unwrap();
    session.send("go").unwrap().wait().await;
}

/// The names of the tools offered with `request`.
fn offered(request: &Captured) -> Vec<String> {
    let tools = request.body["tools"].as_array().into_iter().flatten();
    tools
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

/// What the first tool result of `request` says.
fn first_result(request: &Captured) -> String {
    request.tool_results()[0]["content"].as_str().unwrap().to_string()
}

/// Puts the secret into every tool result, as a careless hook might.
struct Leaky;

#[async_trait]
impl Hook for Leaky {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        output.content = format!("{} and {SECRET}", output.content);
        Ok(output)
    }
}

/// Refuses every call of `probe`.
struct ProbeOff;

#[async_trait]
impl ToolPolicy for ProbeOff {
    async fn check(&self, call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        if call.name == "probe" {
            PolicyDecision::Deny("the host keeps probe off".into())
        } else {
            PolicyDecision::Allow
        }
    }
}

/// An extension named `probes` with one tool, named `.0`.
struct Probes(&'static str);

impl Extension for Probes {
    fn name(&self) -> &str {
        "probes"
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(EchoTool::new(self.0))]
    }
}

/// A summarizer without a name, which the kernel refuses.
struct Nameless;

#[async_trait]
impl Summarizer for Nameless {
    fn name(&self) -> &str {
        ""
    }

    async fn summarize(&self, _source: &SummarySource<'_>) -> Result<Summarized, String> {
        Err("never asked".into())
    }
}

#[tokio::test]
async fn an_explicit_host_builds_and_reopens_sessions_without_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    role_in(&home);
    let app = Application::from_config(
        configured(),
        Some(home),
        Overrides {
            model: Some("host-model".into()),
            cwd: Some(work.clone()),
            ..Overrides::default()
        },
        Builtin::Unattended,
    )
    .await
    .unwrap();
    assert_eq!(app.model(), "host-model");
    assert!(app.notices().is_empty());
    let place = SessionPlace::directory(&dir.path().join("sessions"));
    let configured = app.build(app.model(), &place, Approvals::Nobody).unwrap();
    assert_eq!(configured.name(), "unattended");
    let session = configured.create(None).await.unwrap();
    let id = session.id().to_string();
    session
        .set_name(SessionName::new("host session").unwrap())
        .await
        .unwrap();
    let headers = configured
        .agent()
        .session_headers(&SessionQuery::default())
        .await
        .unwrap();
    assert_eq!(
        headers.iter().map(|header| header.id.as_str()).collect::<Vec<_>>(),
        [id.as_str()]
    );
    assert!(configured.agent().close_session(&id));
    assert!(session.is_closed());
    let reopened = configured.open(&id).await.unwrap();
    reopened
        .with_transcript(|transcript| {
            assert_eq!(transcript.header().model, "host-model");
            assert_eq!(
                transcript.header().cwd,
                work.canonicalize().unwrap().display().to_string()
            );
            assert_eq!(transcript.session_name().unwrap().as_str(), "host session");
        })
        .unwrap();
}

#[tokio::test]
async fn an_explicit_host_gets_sanitized_notices_and_rejects_unusable_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "Q7z").unwrap();
    let mut config = configured();
    config.secret_files.push(secret.clone());
    let app = Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
        .await
        .unwrap();
    assert_eq!(app.notices().len(), 1);
    assert!(app.notices()[0].contains(&secret.display().to_string()));
    assert!(!app.notices()[0].contains("Q7z"));
    std::fs::remove_file(secret).unwrap();
    let mut config = configured();
    config.secret_files.push(dir.path().join("secret.txt"));
    let error = Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("cannot use secret file"));
}

#[tokio::test]
async fn invalid_model_and_budget_are_rejected_before_any_session_is_created() {
    let mut config = configured();
    config.model = None;
    assert!(
        Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
            .await
            .is_err()
    );
    let app = Application::from_config(
        configured(),
        None,
        Overrides {
            cost_budget: Some(1.0),
            ..Overrides::default()
        },
        Builtin::Unattended,
    )
    .await
    .unwrap();
    let error = app
        .build(app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("a cost budget needs the prices"));
}

#[tokio::test]
async fn a_host_that_gives_a_negative_or_unbounded_cost_budget_gets_an_error() {
    for amount in [-1.0, f64::NAN, f64::INFINITY] {
        let overrides = Overrides {
            cost_budget: Some(amount),
            ..Overrides::default()
        };
        let refused = Application::from_config(configured(), None, overrides, Builtin::Unattended).await;
        let error = match refused {
            Ok(_) => panic!("the cost budget {amount} was accepted"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("a cost budget must be zero (none) or a positive amount"),
            "{amount}: {error}"
        );
    }
}

#[tokio::test]
async fn an_explicit_configuration_is_checked_as_a_file_would_be() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        approval_timeout_secs: Some(0),
        ..configured()
    };
    let refused = Application::from_config(
        config,
        Some(dir.path().join("home")),
        Overrides::default(),
        Builtin::Unattended,
    )
    .await;
    let error = match refused {
        Ok(_) => panic!("an approval that times out at once was accepted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("approvalTimeoutSecs must be at least 1"), "{error}");
}

#[tokio::test]
async fn a_tool_the_host_adds_is_offered_only_where_the_agent_enables_it() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let app = host(dir.path(), "tools = [\"probe\"]", &server)
        .await
        .add_tool(Arc::new(EchoTool::new("probe")));
    ask(&app).await;
    let requests = server.requests();
    assert_eq!(offered(&requests[0]), ["probe"]);
    assert_eq!(first_result(&requests[1]), "found it");

    let elsewhere = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("done")]).await;
    let app = host(elsewhere.path(), "tools = []", &server)
        .await
        .add_tool(Arc::new(EchoTool::new("probe")));
    ask(&app).await;
    assert!(offered(&server.requests()[0]).is_empty());
}

#[tokio::test]
async fn the_agents_permissions_ask_about_a_tool_the_host_adds() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let probe = Arc::new(EchoTool::new("probe"));
    let front = "tools = [\"probe\"]\n[permissions]\nask = [\"probe\"]";
    let app = host(dir.path(), front, &server).await.add_tool(probe.clone());
    ask(&app).await;
    // Asked about, with nobody to answer.
    let result = first_result(&server.requests()[1]);
    assert!(result.starts_with("Approval denied"), "{result}");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn what_a_hook_of_the_host_adds_to_a_tool_result_is_still_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let app = host(dir.path(), "tools = [\"probe\"]", &server)
        .await
        .add_tool(Arc::new(EchoTool::new("probe")))
        .add_hook("leaky", Arc::new(Leaky));
    ask(&app).await;
    assert_eq!(first_result(&server.requests()[1]), "found it and [REDACTED]");
}

#[tokio::test]
async fn a_policy_of_the_host_refuses_a_call_the_agent_allows() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let probe = Arc::new(EchoTool::new("probe"));
    let app = host(dir.path(), "tools = [\"probe\"]", &server)
        .await
        .add_tool(probe.clone())
        .add_policy("probe_off", Arc::new(ProbeOff));
    ask(&app).await;
    let result = first_result(&server.requests()[1]);
    assert!(result.contains("the host keeps probe off"), "{result}");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_tools_of_an_extension_of_the_host_ask_as_those_of_mcp_servers_do() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("ext_probe").await;
    let app = host(dir.path(), "tools = []", &server)
        .await
        .add_extension(Arc::new(Probes("ext_probe")));
    ask(&app).await;
    let requests = server.requests();
    assert_eq!(offered(&requests[0]), ["ext_probe"]);
    // Asked about, with nobody to answer.
    let result = first_result(&requests[1]);
    assert!(result.starts_with("Approval denied"), "{result}");
}

/// Gives an application parts of the host's.
type Adding = fn(Application) -> Application;

#[tokio::test]
async fn a_part_of_the_host_named_like_another_fails_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    agent_in(&home, "tools = [\"probe\"]");
    let cases: [(Adding, &str); 6] = [
        (
            |app| app.add_tool(Arc::new(EchoTool::new("bash"))),
            "the host's tool \"bash\" has the name of a built-in tool",
        ),
        (
            |app| app.add_extension(Arc::new(Probes("read_file"))),
            "the host's tool \"read_file\" has the name of a built-in tool",
        ),
        (
            |app| app.add_hook("redact_secrets", Arc::new(Leaky)),
            "the host's hook or policy \"redact_secrets\" has a name",
        ),
        (
            |app| {
                app.add_hook("audit", Arc::new(Leaky))
                    .add_policy("audit", Arc::new(ProbeOff))
            },
            "the host's hook or policy \"audit\" has a name",
        ),
        (
            |app| {
                app.add_extension(Arc::new(Probes("ext_probe")))
                    .add_policy("probes", Arc::new(ProbeOff))
            },
            "the host's hook or policy \"probes\" has a name",
        ),
        (
            |app| {
                app.add_tool(Arc::new(EchoTool::new("probe")))
                    .add_tool(Arc::new(EchoTool::new("probe")))
            },
            "more than one tool is named \"probe\"",
        ),
    ];
    for (add, expected) in cases {
        let app = Application::from_config(
            configured(),
            Some(home.clone()),
            Overrides::default(),
            Builtin::Unattended,
        )
        .await
        .unwrap();
        let app = add(app);
        let error = app
            .build(app.model(), &SessionPlace::memory(), Approvals::Nobody)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
    }
}

#[tokio::test]
async fn the_summarizer_a_host_sets_is_the_agents() {
    let app = Application::from_config(configured(), None, Overrides::default(), Builtin::Unattended)
        .await
        .unwrap()
        .set_summarizer(Arc::new(Nameless));
    let error = app
        .build(app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .err()
        .unwrap();
    // The kernel refuses it as it makes the agent: it is the one the agent was given.
    assert!(format!("{error:#}").contains("the summarizer has no name"), "{error:#}");
}
