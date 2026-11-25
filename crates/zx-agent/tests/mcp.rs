//! The MCP client against a fake MCP server (`zx_testing::fake_mcp`): both
//! protocol eras, tool selection and naming, calls and their results, errors, timeouts and
//! cancellation; calls once a server that was down or restarted answers again; configured
//! headers; trusted read-only annotations.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::builtin::mcp::{Server, ServerConfig};
use zx_agent::builtin::tools::truncate::MAX_BYTES;
use zx_agent::events::RunOutcome;
use zx_agent::tools::{Effects, Tool, ToolContext, ToolOutput};
use zx_testing::fake_mcp::{Era, FakeMcp, Reply, SESSION, tool};
use zx_testing::{RunnerSetup, config, run_prompt, summarize, tool_context};

const TOKEN: &str = "mcp-token-1234";

fn settings(fake: &FakeMcp, tools: &[&str]) -> ServerConfig {
    ServerConfig {
        token: Some(TOKEN.to_string()),
        ..ServerConfig::new(
            "prom",
            fake.url.clone(),
            tools.iter().map(std::string::ToString::to_string).collect(),
        )
    }
}

async fn connect(fake: &FakeMcp, tools: &[&str]) -> Server {
    Server::connect(settings(fake, tools)).await.unwrap()
}

fn the_tool(server: &Server, name: &str) -> Arc<dyn Tool> {
    server.tools().into_iter().find(|tool| tool.name() == name).unwrap()
}

async fn call(server: &Server, name: &str, args: serde_json::Value) -> Result<ToolOutput, String> {
    the_tool(server, name)
        .execute(args, &tool_context(Path::new(".")))
        .await
}

fn three_tools() -> Vec<serde_json::Value> {
    vec![
        tool("query", "Run an instant query."),
        tool("range", "Run a range query."),
        tool("admin", "Delete series."),
    ]
}

#[tokio::test]
async fn a_current_server_is_discovered_and_only_the_configured_tools_are_offered() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let server = connect(&fake, &["query", "range"]).await;
    assert_eq!(server.protocol_version(), "2026-07-28");
    let definitions: Vec<_> = server.tools().iter().map(|tool| tool.definition()).collect();
    let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["prom__query", "prom__range"]);
    assert_eq!(definitions[0].description, "Run an instant query.");
    assert_eq!(definitions[0].parameters, tool("query", "")["inputSchema"]);
    assert_eq!(server.tool_names()[1], "prom__range");
    assert!(fake.requests("initialize").is_empty());
    // One tool per page: every page was read.
    assert_eq!(fake.requests("tools/list").len(), 3);
}

#[tokio::test]
async fn the_origin_of_a_server_leaves_out_the_path_and_query_of_its_url() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let url = format!("{}?key=secret", fake.url);

    let server = Server::connect(ServerConfig::new("prom", url, vec!["query".into()]))
        .await
        .unwrap();

    assert_eq!(Some(server.origin()), fake.url.strip_suffix("/mcp"));
}

#[tokio::test]
async fn an_older_server_gets_the_initialize_handshake_and_its_session() {
    let fake = FakeMcp::start(Era::Legacy, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    assert_eq!(server.protocol_version(), "2025-11-25");
    call(&server, "prom__query", json!({"q": "up"})).await.unwrap();
    let methods: Vec<String> = fake
        .received()
        .into_iter()
        .filter(|r| r.http_method == "POST")
        .map(|r| r.method)
        .collect();
    assert_eq!(
        methods[..3],
        ["server/discover", "initialize", "notifications/initialized"]
    );
    for request in fake
        .requests("tools/list")
        .into_iter()
        .chain(fake.requests("tools/call"))
    {
        assert_eq!(request.session.as_deref(), Some(SESSION));
        assert_eq!(request.protocol_version.as_deref(), Some("2025-11-25"));
    }
}

#[tokio::test]
async fn every_request_carries_the_token() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        let server = connect(&fake, &["query"]).await;
        call(&server, "prom__query", json!({})).await.unwrap();
        let posts: Vec<_> = fake
            .received()
            .into_iter()
            .filter(|r| r.http_method == "POST")
            .collect();
        assert!(posts.len() >= 4, "{era:?}: {posts:?}");
        for request in posts {
            assert_eq!(
                request.authorization.as_deref(),
                Some(format!("Bearer {TOKEN}").as_str()),
                "{era:?} {}",
                request.method
            );
        }
    }
}

#[tokio::test]
async fn a_call_sends_the_arguments_to_the_tool_and_returns_its_text() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    let output = call(&server, "prom__query", json!({"q": "up"})).await.unwrap();
    assert_eq!(output.content, r#"query: {"q":"up"}"#);
    assert!(!output.is_error);
    let sent = fake.requests("tools/call");
    assert_eq!(sent[0].params["name"], "query");
    assert_eq!(sent[0].params["arguments"], json!({"q": "up"}));
    assert_eq!(sent[0].protocol_version.as_deref(), Some("2026-07-28"));
}

/// A PNG of one transparent pixel, in base64.
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[tokio::test]
async fn results_are_rendered_as_text_for_the_model() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    fake.reply(
        "query",
        vec![
            Reply::Result(json!({"content": [
                {"type": "text", "text": "two series"},
                {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
                {"type": "resource_link", "uri": "file:///tmp/report.csv", "name": "report"},
                {"type": "resource", "resource": {"uri": "file:///tmp/a.txt", "text": "line one"}},
                {"type": "resource", "resource": {"uri": "file:///tmp/a.bin", "mimeType": "application/zip", "blob": "UEsDBA=="}},
            ]})),
            Reply::Result(json!({"content": [], "structuredContent": {"series": 2}})),
            Reply::Result(json!({"content": []})),
        ],
    );
    let server = connect(&fake, &["query"]).await;
    let mixed = call(&server, "prom__query", json!({})).await.unwrap();
    let lines: Vec<&str> = mixed.content.lines().collect();
    assert_eq!(lines[0], "two series");
    // Only the PNG's signature: an image that cannot be decoded is a line that says why.
    assert!(
        lines[1].starts_with("[image image/png omitted: cannot decode the image: "),
        "{}",
        lines[1]
    );
    assert_eq!(
        lines[2..].join("\n"),
        "[resource file:///tmp/report.csv (report)]\n[resource file:///tmp/a.txt]\nline one\n\
         [resource file:///tmp/a.bin: binary application/zip omitted]"
    );
    assert!(mixed.images.is_empty());
    let structured = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert_eq!(structured, r#"{"series":2}"#);
    let empty = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert_eq!(empty, "(no content)");
}

#[tokio::test]
async fn images_in_a_result_reach_the_model_as_images() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    fake.reply(
        "query",
        vec![
            Reply::Result(json!({"content": [
                {"type": "text", "text": "the chart"},
                {"type": "image", "data": ONE_PIXEL_PNG, "mimeType": "image/png"},
            ]})),
            Reply::Result(json!({"content": [{"type": "image", "data": ONE_PIXEL_PNG, "mimeType": "image/png"}]})),
        ],
    );
    let server = connect(&fake, &["query"]).await;

    let with_text = call(&server, "prom__query", json!({})).await.unwrap();
    assert_eq!(with_text.content, "the chart");
    assert_eq!(with_text.images.len(), 1);
    assert_eq!(with_text.images[0].mime_type, "image/png");
    assert_eq!(with_text.images[0].data, ONE_PIXEL_PNG);

    let alone = call(&server, "prom__query", json!({})).await.unwrap();
    assert_eq!(alone.content, "", "a result of images alone has no text");
    assert_eq!(alone.images.len(), 1);
}

#[tokio::test]
async fn a_failed_tool_execution_is_an_error_result_for_the_model() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    fake.reply(
        "query",
        vec![Reply::Result(
            json!({"content": [{"type": "text", "text": "parse error at 3"}], "isError": true}),
        )],
    );
    let server = connect(&fake, &["query"]).await;
    let output = call(&server, "prom__query", json!({"q": "up{"})).await.unwrap();
    assert!(output.is_error);
    assert_eq!(output.content, "parse error at 3");
}

#[tokio::test]
async fn a_protocol_error_fails_the_call_naming_the_server_and_not_the_token() {
    let fake = FakeMcp::start(Era::Legacy, three_tools()).await;
    fake.reply("query", vec![Reply::Error(-32602, "unknown argument q2".into())]);
    let server = connect(&fake, &["query"]).await;
    let error = call(&server, "prom__query", json!({"q2": "x"})).await.unwrap_err();
    assert!(
        error.contains("MCP server prom") && error.contains("unknown argument q2"),
        "{error}"
    );
    assert!(!error.contains(TOKEN), "{error}");
}

#[tokio::test]
async fn a_long_protocol_error_is_cut_like_a_result() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let message: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    fake.reply("query", vec![Reply::Error(-32000, message)]);
    let server = connect(&fake, &["query"]).await;
    let error = call(&server, "prom__query", json!({})).await.unwrap_err();
    assert!(error.starts_with("MCP server prom: query failed: "), "{}", &error[..60]);
    assert!(error.len() <= MAX_BYTES + 200, "{} bytes", error.len());
    assert!(
        error.ends_with("[error truncated; showing 2000 of 3000 lines]"),
        "{}",
        &error[error.len() - 60..]
    );
}

#[tokio::test]
async fn a_call_without_an_answer_times_out_and_is_abandoned() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        fake.reply("query", vec![Reply::Hang]);
        let server = Server::connect(ServerConfig {
            call_timeout: Duration::from_millis(200),
            ..settings(&fake, &["query"])
        })
        .await
        .unwrap();
        let error = call(&server, "prom__query", json!({})).await.unwrap_err();
        assert!(error.contains("did not answer within"), "{era:?}: {error}");
        // The client closed the request; an older server is also told with a notification.
        fake.wait_abandoned(1).await;
        if era == Era::Legacy {
            fake.wait_for("notifications/cancelled", 1).await;
        }
    }
}

#[tokio::test]
async fn cancelling_a_call_stops_waiting_and_abandons_the_request() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    fake.reply("query", vec![Reply::Hang]);
    let server = connect(&fake, &["query"]).await;
    let tool = the_tool(&server, "prom__query");
    let ctx = tool_context(Path::new("."));
    let cancel = ctx.cancel.clone();
    let running = tokio::spawn(async move { tool.execute(json!({}), &ctx).await });
    fake.wait_for("tools/call", 1).await;
    cancel.cancel();
    let error = running.await.unwrap().unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    fake.wait_abandoned(1).await;
}

#[tokio::test]
async fn cancelling_a_run_gives_up_the_call_it_was_waiting_for() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        fake.reply("query", vec![Reply::Hang]);
        let server = connect(&fake, &["query"]).await;
        let provider = Arc::new(FauxProvider::new([FauxStep::tool_call(
            "c1",
            "prom__query",
            json!({"q": "up"}),
        )]));
        let config = config(provider, server.tools());
        let (runner, _) = RunnerSetup::new(config).build().await;

        let report = {
            let run = runner.run("is it up?");
            tokio::pin!(run);
            tokio::select! {
                report = &mut run => panic!("{era:?}: the run ended before its call was sent: {report:?}"),
                _ = fake.wait_for("tools/call", 1) => {}
            }
            // The loop stops waiting at once and drops the call, which never sees the token.
            runner.cancel();
            run.await
        };

        assert_eq!(report.outcome, RunOutcome::Cancelled, "{era:?}");
        // As when a call times out: the request is closed, and an older server is also told.
        fake.wait_abandoned(1).await;
        if era == Era::Legacy {
            fake.wait_for("notifications/cancelled", 1).await;
        }
    }
}

#[tokio::test]
async fn long_results_are_truncated() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let text: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    fake.reply(
        "query",
        vec![Reply::Result(json!({"content": [{"type": "text", "text": text}]}))],
    );
    let server = connect(&fake, &["query"]).await;
    let output = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert!(output.starts_with("line 0\n"), "{}", &output[..20]);
    assert!(
        output.ends_with("[result truncated; showing 2000 of 3000 lines]"),
        "{}",
        &output[output.len() - 60..]
    );
}

#[tokio::test]
async fn arguments_must_be_a_json_object() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    // The loop rejects non-objects before any tool sees them; a direct call is refused too.
    let refused = call(&server, "prom__query", json!(["up"])).await.unwrap_err();
    assert_eq!(refused, "the arguments must be a JSON object");
    assert!(fake.requests("tools/call").is_empty());
}

#[tokio::test]
async fn a_configured_tool_the_server_lacks_fails_the_connection() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let error = Server::connect(settings(&fake, &["query", "delete"]))
        .await
        .err()
        .unwrap();
    assert!(
        error.contains("has no tool named \"delete\"") && error.contains("query, range, admin"),
        "{error}"
    );
}

#[tokio::test]
async fn tools_that_would_share_a_name_fail_the_connection() {
    let fake = FakeMcp::start(Era::Modern, vec![tool("a.b", ""), tool("a_b", "")]).await;
    let error = Server::connect(settings(&fake, &["a.b", "a_b"])).await.err().unwrap();
    assert!(error.contains("like another of its tools"), "{error}");
}

#[tokio::test]
async fn an_unreachable_or_silent_server_fails_the_connection() {
    let refused = Server::connect(ServerConfig::new(
        "prom",
        "http://127.0.0.1:9/mcp",
        vec!["query".to_string()],
    ))
    .await
    .err()
    .unwrap();
    assert!(refused.starts_with("MCP server prom:"), "{refused}");
    // Accepts connections and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            held.push(stream);
        }
    });
    let error = Server::connect(ServerConfig {
        connect_timeout: Duration::from_millis(300),
        ..ServerConfig::new("prom", url, vec!["query".to_string()])
    })
    .await
    .err()
    .unwrap();
    assert!(error.contains("no answer within"), "{error}");
}

#[tokio::test]
async fn malformed_settings_are_refused_before_connecting() {
    let tools = vec!["query".to_string()];
    for (settings, expected) in [
        (
            ServerConfig::new("Prom", "http://127.0.0.1:9/mcp", tools.clone()),
            "must be ASCII lowercase",
        ),
        (
            ServerConfig::new("prom", "ftp://127.0.0.1/mcp", tools.clone()),
            "http:// or https://",
        ),
        (ServerConfig::new("prom", "not a url", tools.clone()), "invalid url"),
        (
            ServerConfig::new("prom", "http://127.0.0.1:9/mcp", Vec::new()),
            "tools lists no tool",
        ),
    ] {
        let error = Server::connect(settings).await.err().unwrap();
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn the_model_calls_a_server_tool_through_the_loop() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "prom__query", json!({"q": "up"})),
        FauxStep::text("done"),
    ]));
    let config = config(provider.clone(), server.tools());
    let mut context = Vec::new();
    run_prompt(config, &mut context, "is it up?").await;
    let requests = provider.requests();
    assert_eq!(requests[0].tools[0].name, "prom__query");
    assert!(
        summarize(&requests[1].messages).contains(&r#"tool:c1=query: {"q":"up"}"#.to_string()),
        "{:?}",
        summarize(&requests[1].messages)
    );
}

/// A tool context whose progress updates are collected.
fn observed() -> (ToolContext, tokio::sync::mpsc::Receiver<String>) {
    let (progress, updates) = tokio::sync::mpsc::channel(16);
    let mut ctx = tool_context(Path::new("."));
    ctx.progress = Some(progress);
    (ctx, updates)
}

fn progress_steps() -> Vec<serde_json::Value> {
    vec![
        json!({"progress": 1, "total": 3, "message": "step 1"}),
        json!({"progress": 2, "total": 3}),
        json!({"progress": 3, "total": 3, "message": "done"}),
    ]
}

#[tokio::test]
async fn progress_notifications_reach_the_tool_context_and_re_arm_the_timeout() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        // Three notifications and then the answer, 150 ms apart: 600 ms in all, longer than
        // the 450 ms a call may go without news, while each of them comes well within it.
        fake.reply(
            "query",
            vec![Reply::Progress {
                steps: progress_steps(),
                every: Duration::from_millis(150),
                result: json!({"content": [{"type": "text", "text": "finished"}]}),
            }],
        );
        let server = Server::connect(ServerConfig {
            call_timeout: Duration::from_millis(450),
            ..settings(&fake, &["query"])
        })
        .await
        .unwrap();
        let (ctx, mut updates) = observed();

        let output = the_tool(&server, "prom__query").execute(json!({}), &ctx).await.unwrap();

        assert_eq!(output.content, "finished", "{era:?}");
        drop(ctx);
        let mut seen = Vec::new();
        while let Some(update) = updates.recv().await {
            seen.push(update);
        }
        assert_eq!(seen, ["step 1", "progress 2/3", "done"], "{era:?}");
        // The request carried a progress token, which the notifications named.
        let call = &fake.requests("tools/call")[0];
        assert!(
            !call.params["_meta"]["progressToken"].is_null(),
            "{era:?}: {:?}",
            call.params
        );
    }
}

/// The JSON-RPC method and session of each POST the server received, in order.
fn posts(fake: &FakeMcp) -> Vec<(String, Option<String>)> {
    fake.received()
        .into_iter()
        .filter(|request| request.http_method == "POST")
        .map(|request| (request.method, request.session))
        .collect()
}

#[tokio::test]
async fn a_call_after_an_older_server_restarted_is_sent_again_in_a_new_session() {
    let fake = FakeMcp::start(Era::Legacy, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    call(&server, "prom__query", json!({"q": "before"})).await.unwrap();
    fake.restart();

    let output = call(&server, "prom__query", json!({"q": "after"})).await.unwrap();

    assert_eq!(output.content, r#"query: {"q":"after"}"#);
    let session = |name: &str| Some(name.to_string());
    let after_listing = posts(&fake).split_off(6);
    assert_eq!(
        after_listing,
        [
            ("tools/call".to_string(), session(SESSION)),
            // Refused with 404 for the lost session: a new one, and the call again.
            ("tools/call".to_string(), session(SESSION)),
            ("initialize".to_string(), None),
            ("notifications/initialized".to_string(), session("session-2")),
            ("tools/call".to_string(), session("session-2")),
        ]
    );
}

#[tokio::test]
async fn a_call_while_the_server_is_down_fails_and_calls_succeed_once_it_is_back() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        let server = connect(&fake, &["query"]).await;
        fake.go_down();

        let error = call(&server, "prom__query", json!({})).await.unwrap_err();
        fake.come_back();
        let output = call(&server, "prom__query", json!({"q": "up"})).await.unwrap();

        // The transport's own words, with the URL shown as its origin and none of rmcp's types.
        let origin = fake.url.trim_end_matches("/mcp");
        assert!(
            error.starts_with("MCP server prom: query failed: ") && error.contains(&format!("({origin})")),
            "{era:?}: {error}"
        );
        assert!(!error.contains("/mcp") && !error.contains("rmcp::"), "{era:?}: {error}");
        assert_eq!(output.content, r#"query: {"q":"up"}"#, "{era:?}");
    }
}

#[tokio::test]
async fn an_older_server_that_restarted_while_down_gets_a_new_session_once_back() {
    let fake = FakeMcp::start(Era::Legacy, three_tools()).await;
    let server = connect(&fake, &["query"]).await;
    fake.go_down();
    call(&server, "prom__query", json!({})).await.unwrap_err();
    fake.restart();
    fake.come_back();

    for q in ["back", "again"] {
        let output = call(&server, "prom__query", json!({"q": q})).await.unwrap();
        assert_eq!(output.content, format!(r#"query: {{"q":"{q}"}}"#));
    }

    let calls = fake.requests("tools/call");
    let last: Vec<Option<&str>> = calls.iter().rev().take(2).map(|call| call.session.as_deref()).collect();
    assert_eq!(last, [Some("session-2"), Some("session-2")]);
    assert_eq!(fake.requests("initialize").len(), 2);
}

#[tokio::test]
async fn every_request_carries_the_configured_headers() {
    for era in [Era::Modern, Era::Legacy] {
        let fake = FakeMcp::start(era, three_tools()).await;
        let server = Server::connect(ServerConfig {
            headers: vec![
                ("X-Scope-OrgID".to_string(), "tenant-a".to_string()),
                ("X-Api-Key".to_string(), "key-1".to_string()),
            ],
            ..settings(&fake, &["query"])
        })
        .await
        .unwrap();
        call(&server, "prom__query", json!({})).await.unwrap();
        let posts: Vec<_> = fake
            .received()
            .into_iter()
            .filter(|request| request.http_method == "POST")
            .collect();
        assert!(posts.len() >= 4, "{era:?}: {posts:?}");
        for request in posts {
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .filter(|(key, _)| key == name)
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>()
            };
            assert_eq!(header("x-scope-orgid"), ["tenant-a"], "{era:?} {}", request.method);
            assert_eq!(header("x-api-key"), ["key-1"], "{era:?} {}", request.method);
            // Beside the token, not instead of it.
            assert_eq!(header("authorization"), [format!("Bearer {TOKEN}").as_str()]);
        }
    }
}

/// `tools` with the `readOnlyHint` given for each, by name.
fn annotated(hints: &[(&str, Option<bool>)]) -> Vec<serde_json::Value> {
    hints
        .iter()
        .map(|(name, hint)| {
            let mut described = tool(name, "");
            if let Some(hint) = hint {
                described["annotations"] = json!({"readOnlyHint": hint});
            }
            described
        })
        .collect()
}

#[tokio::test]
async fn a_tool_is_read_only_only_when_its_server_is_trusted_and_says_so() {
    let fake = FakeMcp::start(
        Era::Modern,
        annotated(&[("query", Some(true)), ("range", Some(false)), ("admin", None)]),
    )
    .await;
    let tools = ["query", "range", "admin"];
    let effects = |server: &Server| server.tools().iter().map(|tool| tool.effects()).collect::<Vec<_>>();

    let trusted = Server::connect(ServerConfig {
        trust_read_only: true,
        ..settings(&fake, &tools)
    })
    .await
    .unwrap();
    let untrusted = connect(&fake, &tools).await;

    assert_eq!(
        effects(&trusted),
        [Effects::ReadOnly, Effects::SideEffects, Effects::SideEffects]
    );
    assert_eq!(effects(&untrusted), [Effects::SideEffects; 3]);
}
