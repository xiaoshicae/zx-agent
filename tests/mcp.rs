//! The MCP client against a fake MCP server (`tests/common/fake_mcp.rs`): both protocol eras,
//! tool selection and naming, calls and their results, errors, timeouts and cancellation.
mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::fake_mcp::{Era, FakeMcp, Reply, SESSION, tool};
use common::{config, run_prompt, summarize};
use zx_agent::agent::{Tool, ToolContext, ToolOutput};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::extensions::{Extension, register};
use zx_agent::mcp::{Server, ServerConfig};

const TOKEN: &str = "mcp-token-1234";

fn settings(fake: &FakeMcp, tools: &[&str]) -> ServerConfig {
    ServerConfig {
        token: Some(TOKEN.to_string()),
        ..ServerConfig::new("prom", fake.url.clone(), tools.iter().map(|t| t.to_string()).collect())
    }
}

async fn connect(fake: &FakeMcp, tools: &[&str]) -> Server {
    Server::connect(settings(fake, tools)).await.unwrap()
}

fn context() -> ToolContext {
    ToolContext {
        cwd: PathBuf::from("."),
        cancel: CancellationToken::new(),
    }
}

fn the_tool(server: &Server, name: &str) -> Arc<dyn Tool> {
    server.tools().into_iter().find(|tool| tool.name() == name).unwrap()
}

async fn call(server: &Server, name: &str, args: serde_json::Value) -> Result<ToolOutput, String> {
    the_tool(server, name).execute(args, &context()).await
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
    assert_eq!(server.tool_names()[1], ("range".to_string(), "prom__range".to_string()));
    assert!(fake.requests("initialize").is_empty());
    // One tool per page: every page was read.
    assert_eq!(fake.requests("tools/list").len(), 3);
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
    let mixed = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert_eq!(
        mixed,
        "two series\n[image image/png omitted]\n[resource file:///tmp/report.csv (report)]\n\
         [resource file:///tmp/a.txt]\nline one\n[resource file:///tmp/a.bin: binary application/zip omitted]"
    );
    let structured = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert_eq!(structured, r#"{"series":2}"#);
    let empty = call(&server, "prom__query", json!({})).await.unwrap().content;
    assert_eq!(empty, "(no content)");
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
    fake.reply("query", vec![Reply::Error(-32602, "unknown argument q2")]);
    let server = connect(&fake, &["query"]).await;
    let error = call(&server, "prom__query", json!({"q2": "x"})).await.unwrap_err();
    assert!(
        error.contains("MCP server prom") && error.contains("unknown argument q2"),
        "{error}"
    );
    assert!(!error.contains(TOKEN), "{error}");
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
    let ctx = context();
    let cancel = ctx.cancel.clone();
    let running = tokio::spawn(async move { tool.execute(json!({}), &ctx).await });
    fake.wait_for("tools/call", 1).await;
    cancel.cancel();
    let error = running.await.unwrap().unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    fake.wait_abandoned(1).await;
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
    let tool = the_tool(&server, "prom__query");
    assert!(tool.validate_arguments(&json!({"q": "up"})).is_ok());
    assert!(tool.validate_arguments(&json!(["up"])).is_err());
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
            "no tools are configured",
        ),
    ] {
        let error = Server::connect(settings).await.err().unwrap();
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn the_model_calls_a_server_tool_through_the_loop() {
    let fake = FakeMcp::start(Era::Modern, three_tools()).await;
    let server: Arc<dyn Extension> = Arc::new(connect(&fake, &["query"]).await);
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "prom__query", json!({"q": "up"})),
        FauxStep::text("done"),
    ]));
    let mut config = config(provider.clone(), Vec::new());
    register(&mut config, &[server]).unwrap();
    let mut context = Vec::new();
    run_prompt(&config, &mut context, "is it up?").await;
    let requests = provider.requests();
    assert_eq!(requests[0].tools[0].name, "prom__query");
    assert!(
        summarize(&requests[1].messages).contains(&r#"tool:c1=query: {"q":"up"}"#.to_string()),
        "{:?}",
        summarize(&requests[1].messages)
    );
}
