//! A non-CLI host uses explicit configuration, owns the assembled sessions, and gives the
//! agent parts of its own.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use zx_agent::SessionOptions;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Provider, ToolCall, ToolDef};
use zx_agent::builtin::executor::{Executor, LocalExecutor};
use zx_agent::compaction::{Summarized, Summarizer, SummarySource};
use zx_agent::hooks::{Hook, HookContext, HookResult};
use zx_agent::policy::{PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::{SessionHeader, SessionName, SessionQuery};
use zx_app::config::{Config, ContainerSettings, Credentials, McpServer};
use zx_app::{
    Application, Approvals, Asking, ExecutorFactory, ExecutorSession, Overrides, ProviderFactory, ProviderInput,
    SessionPlace, SessionTools,
};
use zx_testing::EchoTool;
use zx_testing::fake_mcp::{self, Era, FakeMcp};
use zx_testing::mock_server::{Captured, MockServer, Reply};
use zx_testing::program::{agent_text, write_agent_file};
use zx_util::sync::lock;

/// The value of a secret file of the configuration, which tool results must not show.
const SECRET: &str = "s3cret-value-of-the-host";

/// The agent the hosts of these tests build: the built-in one for runs that nobody attends, or
/// the user's file of its name.
const AGENT: &str = "unattended";

fn configured() -> Config {
    let mut config = Config {
        model: Some("configured-model".into()),
        ..Config::default()
    };
    config
        .provider
        .options
        .insert("baseUrl".into(), "http://127.0.0.1:1/v1".into());
    config.provider.api_key = Some("${ZX_APPLICATION_TEST_UNUSED_KEY}".into());
    config
}

/// The user's file of the built-in agent `unattended` in `home`, with `front` in its front
/// matter and instructions of its own.
fn agent_in(home: &Path, front: &str) {
    let front = format!("extends = \"unattended\"\n{front}\n[context]\ndiscover = false");
    write_agent_file(home, "unattended", &agent_text(&front, "Explicit host role.\n"));
}

fn role_in(home: &Path) {
    agent_in(home, "tools = []");
}

/// A host whose agent has `front` in its file, asking `server`, its sessions working in
/// `dir`, which also holds the home and a secret file.
async fn host(dir: &Path, front: &str, server: &MockServer) -> Application {
    host_asking(dir, front, server, Asking::AsConfigured).await
}

/// [`host`], with `asking` over the agent's permissions.
async fn host_asking(dir: &Path, front: &str, server: &MockServer, asking: Asking) -> Application {
    host_configured(dir, front, asking_model(dir, server), asking).await
}

/// The configuration of a host that asks `server`, with a secret file in `dir`.
fn asking_model(dir: &Path, server: &MockServer) -> Config {
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, SECRET).unwrap();
    let mut config = configured();
    config
        .provider
        .options
        .insert("baseUrl".into(), server.base_url.clone().into());
    config.secret_files.push(secret);
    config
}

/// A host with `config`, whose agent has `front` in its file, in the home that `dir` holds,
/// its sessions working in `dir`, with `asking` over the agent's permissions.
async fn host_configured(dir: &Path, front: &str, config: Config, asking: Asking) -> Application {
    let home = dir.join("home");
    agent_in(&home, front);
    let overrides = Overrides {
        cwd: Some(dir.to_path_buf()),
        asking,
        ..Overrides::default()
    };
    Application::from_config(config, Some(home), overrides).await.unwrap()
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
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
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
    request.tool_result_texts().remove(0)
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
    )
    .await
    .unwrap();
    assert_eq!(app.model(), "host-model");
    assert!(app.notices().is_empty());
    let place = SessionPlace::directory(&dir.path().join("sessions"));
    let configured = app.build(AGENT, app.model(), &place, Approvals::Nobody).unwrap();
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
    assert_eq!(reopened.model().unwrap(), "host-model");
    reopened
        .with_transcript(|transcript| {
            assert_eq!(
                transcript.header().cwd,
                work.canonicalize().unwrap().display().to_string()
            );
            assert_eq!(transcript.session_name().unwrap().as_str(), "host session");
        })
        .unwrap();
}

#[tokio::test]
async fn a_notice_about_a_short_secret_names_its_file_but_not_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "Q7z").unwrap();
    let mut config = configured();
    config.secret_files.push(secret.clone());
    let app = Application::from_config(config, None, Overrides::default())
        .await
        .unwrap();
    assert_eq!(app.notices().len(), 1);
    assert!(app.notices()[0].contains(&secret.display().to_string()));
    assert!(!app.notices()[0].contains("Q7z"));
}

#[tokio::test]
async fn container_settings_that_the_execution_does_not_use_are_noticed() {
    let image = || ContainerSettings {
        image: Some("debian:bookworm-slim".into()),
        ..ContainerSettings::default()
    };
    for (execution, notice) in [
        (
            "local",
            Some(
                "[container] the [container] settings of config.toml are not used: execution is \"local\", and commands run on this machine; execution = \"container\" runs them in a container",
            ),
        ),
        (
            "noting",
            Some(
                "[container] the [container] settings of config.toml are not used: execution is \"noting\"; execution = \"container\" runs commands in a container",
            ),
        ),
        ("container", None),
    ] {
        let config = Config {
            container: image(),
            ..executing(execution)
        };
        let app = Application::from_config(config, None, Overrides::default())
            .await
            .unwrap();
        let notices: Vec<&str> = app.notices().iter().map(String::as_str).collect();
        assert_eq!(notices, notice.into_iter().collect::<Vec<_>>(), "{execution}");
    }
    // Without the table there is nothing to say.
    let app = Application::from_config(executing("local"), None, Overrides::default())
        .await
        .unwrap();
    assert!(app.notices().is_empty(), "{:?}", app.notices());
}

#[tokio::test]
async fn a_secret_file_that_cannot_be_used_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = configured();
    config.secret_files.push(dir.path().join("secret.txt"));
    let error = Application::from_config(config, None, Overrides::default())
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
        Application::from_config(config, None, Overrides::default())
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
    )
    .await
    .unwrap();
    let error = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
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
        let refused = Application::from_config(configured(), None, overrides).await;
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
    let refused = Application::from_config(config, Some(dir.path().join("home")), Overrides::default()).await;
    let error = match refused {
        Ok(_) => panic!("an approval that times out at once was accepted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("approvalTimeoutSecs must be greater than zero"),
        "{error}"
    );
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
    assert!(result.starts_with("Not executed: approval denied"), "{result}");
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
async fn the_token_of_an_mcp_server_a_host_configures_is_redacted_from_tool_output() {
    const TOKEN: &str = "t0ken-of-the-mcp-server";
    let dir = tempfile::tempdir().unwrap();
    let mcp = FakeMcp::start(Era::Modern, vec![fake_mcp::tool("query", "Query the metrics.")]).await;
    // The server answers a call with its arguments, so the model reads the token back.
    let server = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", &json!({"q": TOKEN})),
        Reply::text("done"),
    ])
    .await;
    let mut config = asking_model(dir.path(), &server);
    config.mcp.push(McpServer {
        name: "prom".into(),
        url: mcp.url.clone(),
        token: Some(TOKEN.into()),
        headers: BTreeMap::new(),
        tools: vec!["query".into()],
        trust_read_only: false,
        timeout_secs: None,
    });
    let front = "tools = []\n[permissions]\nallow = [\"prom__query\"]";
    let app = host_configured(dir.path(), front, config, Asking::AsConfigured).await;
    ask(&app).await;
    let sent = mcp.requests("tools/call")[0].authorization.clone();
    assert_eq!(sent, Some(format!("Bearer {TOKEN}")));
    let result = first_result(&server.requests()[1]);
    assert!(result.contains("[REDACTED]") && !result.contains(TOKEN), "{result}");
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
async fn skipping_approvals_runs_the_calls_that_would_ask_but_not_those_the_agent_denies() {
    let asked = "tools = [\"probe\"]\n[permissions]\nask = [\"probe\"]";
    let denied = "tools = [\"probe\"]\n[permissions]\ndeny = [\"probe\"]";
    for (front, runs) in [(asked, true), (denied, false)] {
        let dir = tempfile::tempdir().unwrap();
        let server = calling("probe").await;
        let app = host_asking(dir.path(), front, &server, Asking::Never)
            .await
            .add_tool(Arc::new(EchoTool::new("probe")));
        assert!(
            app.notices()[0].starts_with("[danger] approvals are skipped"),
            "{:?}",
            app.notices()
        );
        ask(&app).await;
        let result = first_result(&server.requests()[1]);
        assert_eq!(!result.starts_with("Not executed"), runs, "{front}: {result}");
        if !runs {
            assert!(result.contains("tool denied by the agent"), "{result}");
        }
    }
}

/// `probe`, which answers whom its session is for.
struct SaysWho(String);

#[async_trait]
impl Tool for SaysWho {
    fn name(&self) -> &str {
        "probe"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "probe".into(),
            description: "Whom the session is for.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok(self.0.clone()))
    }
}

/// Gives each session with the label `who` a `probe` that answers with it.
struct Whom;

impl SessionTools for Whom {
    fn names(&self) -> Vec<String> {
        vec!["probe".into()]
    }

    fn tools(&self, header: &SessionHeader) -> Vec<Arc<dyn Tool>> {
        let who = header.labels.get("who").into_iter();
        who.map(|who| Arc::new(SaysWho(who.clone())) as Arc<dyn Tool>).collect()
    }
}

/// Ask the agent of `app` once, in a session for `who`, kept in memory, with nobody to
/// approve.
async fn ask_for(app: &Application, who: &str, cwd: &Path) {
    let configured = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .unwrap();
    let options = SessionOptions {
        labels: BTreeMap::from([("who".to_string(), who.to_string())]),
        ..SessionOptions::new(cwd)
    };
    let session = configured.agent().create_session(options).await.unwrap();
    session.send("go").unwrap().wait().await;
}

#[tokio::test]
async fn a_tool_the_host_gives_each_session_is_offered_whatever_the_agent_enables() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let app = host(dir.path(), "tools = []", &server)
        .await
        .add_session_tools(Arc::new(Whom));
    ask_for(&app, "ops", dir.path()).await;
    let requests = server.requests();
    assert_eq!(offered(&requests[0]), ["probe"]);
    // Made for its session, it knows whom the session is for.
    assert_eq!(first_result(&requests[1]), "ops");
}

#[tokio::test]
async fn permissions_may_deny_a_tool_of_each_session_and_tools_may_not_enable_it() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let denied = "tools = []\n[permissions]\ndeny = [\"probe\"]";
    let app = host(dir.path(), denied, &server)
        .await
        .add_session_tools(Arc::new(Whom));
    ask_for(&app, "ops", dir.path()).await;
    let result = first_result(&server.requests()[1]);
    assert!(result.contains("tool denied by the agent"), "{result}");

    let listed = tempfile::tempdir().unwrap();
    let app = host(listed.path(), "tools = [\"probe\"]", &server)
        .await
        .add_session_tools(Arc::new(Whom));
    let error = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("the host offers \"probe\" in every session"),
        "{error:#}"
    );
}

#[tokio::test]
async fn the_prompt_names_a_tool_of_each_session_that_the_permissions_ask_about() {
    let dir = tempfile::tempdir().unwrap();
    let server = calling("probe").await;
    let asked = "tools = []\n[permissions]\nask = [\"probe\"]";
    let app = host(dir.path(), asked, &server).await.add_session_tools(Arc::new(Whom));
    ask_for(&app, "ops", dir.path()).await;
    let requests = server.requests();
    // Asked about, with nobody to answer: the call is denied, as the prompt said it would be.
    let result = first_result(&requests[1]);
    assert!(result.starts_with("Not executed: approval denied"), "{result}");
    let prompt = requests[0].system_text();
    assert!(
        prompt.contains("so calls of these tools are denied: probe."),
        "{prompt}"
    );
}

/// Gives an application parts of the host's.
type Adding = fn(Application) -> Application;

#[tokio::test]
async fn a_part_of_the_host_named_like_another_fails_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    agent_in(&home, "tools = [\"probe\"]");
    let cases: [(Adding, &str); 7] = [
        (
            |app| app.add_tool(Arc::new(EchoTool::new("bash"))),
            "the host's tool \"bash\" has the name of a built-in tool",
        ),
        (
            |app| app.add_hook("redact_secrets", Arc::new(Leaky)),
            "the host's hook \"redact_secrets\" has a name",
        ),
        (
            |app| app.add_policy("command_policy", Arc::new(ProbeOff)),
            "the host's policy \"command_policy\" has a name",
        ),
        (
            |app| {
                app.add_tool(Arc::new(EchoTool::new("probe")))
                    .add_tool(Arc::new(EchoTool::new("probe")))
            },
            "more than one tool is named \"probe\"",
        ),
        (
            |app| app.add_prompt_section("environment", "Elsewhere."),
            "the host's prompt section \"environment\" has a name",
        ),
        (
            |app| app.add_prompt_section("preamble", "Other instructions."),
            "the host's prompt section \"preamble\" has a name",
        ),
        (
            |app| {
                app.add_prompt_section("notes", "One.")
                    .add_prompt_section("notes", "Two.")
            },
            "the host's prompt section \"notes\" has a name",
        ),
    ];
    for (add, expected) in cases {
        let app = Application::from_config(configured(), Some(home.clone()), Overrides::default())
            .await
            .unwrap();
        let app = add(app);
        let error = app
            .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
    }
}

#[tokio::test]
async fn a_hook_and_a_policy_of_the_host_may_share_a_name() {
    let app = Application::from_config(configured(), None, Overrides::default())
        .await
        .unwrap()
        .add_hook("audit", Arc::new(Leaky))
        .add_policy("audit", Arc::new(ProbeOff));

    let built = app.build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody);

    assert!(built.is_ok(), "{:#}", built.err().unwrap());
}

#[tokio::test]
async fn the_summarizer_a_host_sets_is_the_agents() {
    let app = Application::from_config(configured(), None, Overrides::default())
        .await
        .unwrap()
        .set_summarizer(Arc::new(Nameless));
    let error = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .err()
        .unwrap();
    // The kernel refuses it as it makes the agent: it is the one the agent was given.
    assert!(format!("{error:#}").contains("the summarizer has no name"), "{error:#}");
}

/// A provider a host registers: a scripted model that answers with the `answer` of its keys.
struct Scripted;

impl ProviderFactory for Scripted {
    fn api_key_env(&self) -> &str {
        "ZX_APPLICATION_TEST_SCRIPTED_KEY"
    }

    fn build(&self, input: ProviderInput<'_>) -> anyhow::Result<Arc<dyn Provider>> {
        let answer = input.options["answer"].as_str().unwrap_or_default();
        Ok(Arc::new(FauxProvider::new(vec![FauxStep::text(answer)])))
    }
}

/// `configured()` whose provider is `api`, with the keys of `text`.
fn with_provider(api: &str, text: &str) -> Config {
    let mut config = configured();
    config.provider.api = api.into();
    config.provider.options = toml::from_str(text).unwrap();
    config
}

/// The answer of the agent of `app` to one prompt.
async fn answer_of(app: Application) -> String {
    let agent = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .unwrap();
    let session = agent.create(None).await.unwrap();
    session.send("hi").unwrap().wait().await.final_text
}

#[tokio::test]
async fn config_toml_names_the_provider_a_host_registers_and_gives_it_its_keys() {
    let config = with_provider("scripted", "answer = 'scripted hello'");
    let app = Application::from_config(config, None, Overrides::default())
        .await
        .unwrap()
        .register_provider("scripted", Arc::new(Scripted));
    assert_eq!(answer_of(app).await, "scripted hello");
}

#[tokio::test]
async fn the_anthropic_provider_is_built_in() {
    let server = MockServer::start(vec![Reply::Sse(vec![
        json!({"type": "message_start", "message": {"id": "m", "model": "configured-model", "usage": {"input_tokens": 3}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "from claude"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ])])
    .await;
    let root = server.base_url.trim_end_matches("/v1");
    let config = with_provider("anthropic", &format!("baseUrl = '{root}'"));
    let app = Application::from_config(config, None, Overrides::default())
        .await
        .unwrap();
    assert_eq!(answer_of(app).await, "from claude");
    let request = &server.requests()[0];
    assert_eq!(request.headers.lines().next(), Some("post /v1/messages"));
    assert_eq!(request.body["model"], json!("configured-model"));
}

/// An executor a host registers: commands run on this machine, as with `local`, and each
/// session it starts for is noted with the variables its commands must not see. It isolates
/// commands when it says so.
#[derive(Default)]
struct Noting {
    isolates: bool,
    started: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl ExecutorFactory for Noting {
    fn isolates(&self) -> bool {
        self.isolates
    }

    async fn start(&self, session: ExecutorSession<'_>) -> anyhow::Result<Arc<dyn Executor>> {
        lock(&self.started).push((session.id.to_string(), session.hidden_env.to_vec()));
        Ok(Arc::new(LocalExecutor {
            hidden_env: session.hidden_env.to_vec(),
        }))
    }
}

/// `configured()` whose commands run where the executor registered under `name` says.
fn executing(name: &str) -> Config {
    Config {
        execution: name.into(),
        ..configured()
    }
}

#[tokio::test]
async fn config_toml_names_the_executor_a_host_registers_and_it_starts_each_session() {
    let noting = Arc::new(Noting::default());
    let app = Application::from_config(executing("noting"), None, Overrides::default())
        .await
        .unwrap()
        .register_executor("noting", noting.clone());
    let agent = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .unwrap();
    let session = agent.create(None).await.unwrap();
    let started = lock(&noting.started).clone();
    assert_eq!(started.len(), 1, "{started:?}");
    let (id, hidden_env) = &started[0];
    assert_eq!(*id, session.id());
    // Its commands must not see the API key, wherever they run.
    assert!(
        hidden_env.contains(&"ZX_APPLICATION_TEST_UNUSED_KEY".to_string()),
        "{hidden_env:?}"
    );
}

/// Credentials taken from `variables`, whose values the test does not need.
fn from_variables(variables: &[&str]) -> Credentials {
    Credentials {
        variables: variables.iter().map(ToString::to_string).collect(),
        values: Vec::new(),
    }
}

#[tokio::test]
async fn the_variables_of_the_credentials_a_host_names_are_hidden_beside_the_api_keys() {
    let noting = Arc::new(Noting::default());
    // Named in the configuration the host builds, and added to the application.
    let config = Config {
        credentials: from_variables(&["ZX_APPLICATION_TEST_MCP_TOKEN"]),
        ..executing("noting")
    };
    let app = Application::from_config(config, None, Overrides::default())
        .await
        .unwrap()
        .register_executor("noting", noting.clone())
        .add_credentials(from_variables(&["ZX_APPLICATION_TEST_TOKEN"]))
        .add_credentials(from_variables(&["ZX_APPLICATION_TEST_SECRET"]));
    let agent = app
        .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
        .unwrap();
    agent.create(None).await.unwrap();
    let started = lock(&noting.started).clone();
    let hidden_env = &started[0].1;
    for name in [
        "ZX_APPLICATION_TEST_UNUSED_KEY",
        "ZX_APPLICATION_TEST_MCP_TOKEN",
        "ZX_APPLICATION_TEST_TOKEN",
        "ZX_APPLICATION_TEST_SECRET",
    ] {
        assert!(hidden_env.contains(&name.to_string()), "{name}: {hidden_env:?}");
    }
}

#[tokio::test]
async fn the_value_of_a_credential_a_host_adds_is_redacted_from_tool_output() {
    const TOKEN: &str = "t0ken-of-the-hosts-platform";
    let dir = tempfile::tempdir().unwrap();
    // The tool answers with its argument, so the model reads the token back.
    let server = MockServer::start(vec![
        Reply::tool_call("c1", "probe", &json!({"text": TOKEN})),
        Reply::text("done"),
    ])
    .await;
    let app = host(dir.path(), "tools = [\"probe\"]", &server)
        .await
        .add_tool(Arc::new(EchoTool::new("probe")))
        .add_credentials(Credentials {
            variables: Vec::new(),
            values: vec![TOKEN.into()],
        });
    ask(&app).await;
    assert_eq!(first_result(&server.requests()[1]), "[REDACTED]");
}

#[tokio::test]
async fn approving_every_call_needs_an_executor_that_isolates_commands() {
    let dir = tempfile::tempdir().unwrap();
    agent_in(dir.path(), "[permissions]\napproveAll = true\nask = []");
    for (name, isolates, built) in [
        ("local", false, false),
        ("plain", false, false),
        ("sandbox", true, true),
    ] {
        let mut app = Application::from_config(executing(name), Some(dir.path().to_path_buf()), Overrides::default())
            .await
            .unwrap();
        if name != "local" {
            app = app.register_executor(
                name,
                Arc::new(Noting {
                    isolates,
                    ..Noting::default()
                }),
            );
        }
        let result = app.build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody);
        match result {
            Ok(_) => assert!(built, "{name}"),
            Err(error) => {
                assert!(!built, "{name}: {error:#}");
                assert!(
                    format!("{error:#}").contains("permissions.approveAll needs execution = \"container\""),
                    "{name}: {error:#}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_name_no_factory_is_registered_under_or_two_are_fails_the_build() {
    let cases: [(Config, Adding, &str); 4] = [
        (
            with_provider("nowhere", ""),
            |app| app,
            "provider.api: no provider is registered under the name \"nowhere\"; use one of openai, anthropic",
        ),
        (
            executing("kubernetes"),
            |app| app,
            "execution: no executor is registered under the name \"kubernetes\"; use one of local, container",
        ),
        (
            configured(),
            |app| app.register_provider("openai", Arc::new(Scripted)),
            "more than one provider is registered under the name \"openai\"",
        ),
        (
            configured(),
            |app| app.register_executor("container", Arc::new(Noting::default())),
            "more than one executor is registered under the name \"container\"",
        ),
    ];
    for (config, register, expected) in cases {
        let app = Application::from_config(config, None, Overrides::default())
            .await
            .unwrap();
        let app = register(app);
        let error = app
            .build(AGENT, app.model(), &SessionPlace::memory(), Approvals::Nobody)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}
