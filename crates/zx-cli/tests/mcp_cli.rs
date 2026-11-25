//! The compiled program with MCP servers from config.toml: run and chat connect them at
//! startup; a tool the agent's permissions.allow lists runs, the others ask (and `run`, with no one
//! to ask, refuses them), but those a trusted server says only read; the configured headers
//! go with every request, through the proxy of config.toml when it has one; the token and the
//! header variables are hidden from commands and
//! redacted; a server that cannot be used stops startup. The IM service's test of the same is
//! with its program (`crates/zx-im/tests/im_cli.rs`).

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zx_testing::fake_mcp::{Era, FakeMcp, Reply as McpReply, tool};
use zx_testing::fake_proxy::FakeProxy;
use zx_testing::mock_server::{MockServer, Reply};
use zx_testing::program::{Home, ended_output, fake_model_config, write_agent, write_config};
use zx_testing::{stderr, stdout};

use self::common::zx_agent_in;

const TOKEN_VAR: &str = "ZX_MCP_CLI_TEST_TOKEN";
const TOKEN: &str = "mcp-cli-secret-5678";
const KEY_VAR: &str = "ZX_MCP_CLI_TEST_KEY";
/// The variable holding the value of the server's `X-Api-Key` header.
const HEADER_VAR: &str = "ZX_MCP_CLI_TEST_HEADER";
const HEADER_VALUE: &str = "mcp-cli-header-9012";

async fn prom() -> FakeMcp {
    FakeMcp::start(
        Era::Modern,
        vec![tool("query", "Instant query."), tool("range", "Range query.")],
    )
    .await
}

/// A zx-agent home in `dir` whose agents enable `builtin_tools`, and whose config.toml has one
/// MCP server, where `query` runs without approval and `range` asks.
fn agent_home(dir: &Path, url: &str, builtin_tools: &str) -> PathBuf {
    agent_home_with(dir, url, builtin_tools, "")
}

/// [`agent_home`] with the lines of `more` added to the server's `[[mcp]]` entry.
fn agent_home_with(dir: &Path, url: &str, builtin_tools: &str, more: &str) -> PathBuf {
    let home = dir.join(".zx-agent-home");
    for builtin in ["unattended", "assistant"] {
        write_agent(
            &home,
            builtin,
            &format!(
                "tools = {builtin_tools}\n[context]\ndiscover = false\n[permissions]\nallow = [\"prom__query\"]\n"
            ),
        );
    }
    let server = format!(
        "[[mcp]]\nname = \"prom\"\nurl = \"{url}\"\ntoken = \"${{{TOKEN_VAR}}}\"\ntools = [\"query\", \"range\"]\n{more}"
    );
    write_config(&home, &fake_model_config(KEY_VAR, &server, ""));
    home
}

/// `zx-agent <subcommand>` against `model` with the zx-agent home `home`, the token and the
/// header variable set and no API key.
fn program(subcommand: &str, model: &MockServer, dir: &Path, home: &Path) -> Command {
    // chat keeps sessions under the zx-agent home by default; never the real one.
    let mut command = zx_agent_in(dir, Home::Agent(home));
    command
        .args([subcommand, "--model", "mock"])
        .env("ZX_BASE_URL", &model.base_url)
        .env(TOKEN_VAR, TOKEN)
        .env(HEADER_VAR, HEADER_VALUE);
    command
}

fn run(model: &MockServer, dir: &Path, home: &Path) -> Command {
    let mut command = program("run", model, dir, home);
    command.arg("go").stdin(Stdio::null());
    command
}

/// The tool results in the model's last request.
fn tool_results(model: &MockServer) -> Vec<String> {
    model.requests().last().unwrap().tool_result_texts()
}

fn origin(fake: &FakeMcp) -> &str {
    fake.url.trim_end_matches("/mcp")
}

#[tokio::test]
async fn run_calls_an_mcp_tool_its_server_allows() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout(&output), "done\n");
    let startup = format!(
        "[mcp] prom: prom__query, prom__range; protocol 2026-07-28; {}; credentials are sent unencrypted",
        origin(&fake)
    );
    assert!(stderr.contains(&startup), "{stderr}");
    assert_eq!(tool_results(&model), [r#"query: {"q":"up"}"#]);
    assert_eq!(fake.requests("tools/call").len(), 1);
}

#[tokio::test]
async fn the_tools_of_a_server_are_offered_before_those_of_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![Reply::text("done")]).await;
    let home = agent_home(dir.path(), &fake.url, "[\"read_file\"]");
    let output = run(&model, dir.path(), &home).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let tools = &model.requests()[0].body["tools"];
    // Every session shares the servers' tools; a session's own, which work in its
    // directory, come after them.
    assert_eq!(tools[0]["function"]["name"], "prom__query");
    assert_eq!(tools[1]["function"]["name"], "prom__range");
    assert_eq!(tools[2]["function"]["name"], "read_file");
    assert_eq!(tools.as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn the_server_is_reached_through_the_proxy_of_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let proxy = FakeProxy::start().await;
    let section = format!("[proxy]\nhttp = \"{}\"\n", proxy.url(None));
    let home = agent_home_with(dir.path(), &fake.url, "[]", &section);

    let output = run(&model, dir.path(), &home).output().await.unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(tool_results(&model), [r#"query: {"q":"up"}"#]);
    let call = format!("POST {}", fake.url);
    let asked = proxy.asked();
    assert!(asked.iter().any(|asked| asked.line == call), "{asked:?}");
}

#[tokio::test]
async fn run_refuses_an_mcp_tool_that_needs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let results = tool_results(&model);
    assert!(results[0].contains("no human is available"), "{results:?}");
    assert!(fake.requests("tools/call").is_empty());
}

#[tokio::test]
async fn run_calls_an_mcp_tool_that_needs_approval_when_approvals_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
        .arg("--dangerously-skip-approvals")
        .output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("[danger] approvals are skipped"), "{stderr}");
    assert_eq!(tool_results(&model), [r#"range: {"q":"up"}"#]);
}

#[tokio::test]
async fn run_calls_a_tool_a_trusted_server_says_only_reads_without_approval() {
    let dir = tempfile::tempdir().unwrap();
    let mut range = tool("range", "Range query.");
    range["annotations"] = json!({"readOnlyHint": true});
    let fake = FakeMcp::start(Era::Modern, vec![tool("query", "Instant query."), range]).await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let home = agent_home_with(dir.path(), &fake.url, "[]", "trustReadOnly = true");
    let output = run(&model, dir.path(), &home).output().await.unwrap();
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("[mcp] prom: prom__query, prom__range (read-only); protocol"),
        "{stderr}"
    );
    assert_eq!(tool_results(&model), [r#"range: {"q":"up"}"#]);
}

#[tokio::test]
async fn a_tool_a_trusted_server_says_only_reads_still_asks_where_the_agent_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut range = tool("range", "Range query.");
    range["annotations"] = json!({"readOnlyHint": true});
    let fake = FakeMcp::start(Era::Modern, vec![tool("query", "Instant query."), range]).await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let home = agent_home_with(dir.path(), &fake.url, "[]", "trustReadOnly = true");
    write_agent(
        &home,
        "unattended",
        "tools = []\n[context]\ndiscover = false\n[permissions]\nask = [\"prom__range\"]\n",
    );
    let output = run(&model, dir.path(), &home).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let results = tool_results(&model);
    assert!(results[0].contains("no human is available"), "{results:?}");
    assert!(fake.requests("tools/call").is_empty());
}

#[tokio::test]
async fn every_mcp_request_carries_the_configured_headers() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", &json!({})),
        Reply::text("done"),
    ])
    .await;
    let more = format!("headers = {{ X-Scope-OrgID = \"tenant-a\", X-Api-Key = \"${{{HEADER_VAR}}}\" }}");
    let home = agent_home_with(dir.path(), &fake.url, "[]", &more);
    let output = run(&model, dir.path(), &home).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let posts: Vec<_> = fake
        .received()
        .into_iter()
        .filter(|request| request.http_method == "POST")
        .collect();
    assert!(posts.iter().any(|request| request.method == "tools/call"));
    for request in posts {
        let mut sent: Vec<(&str, &str)> = request
            .headers
            .iter()
            .filter(|(name, _)| name.starts_with("x-"))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        // The transport keeps the headers in a hash map: their order is not fixed.
        sent.sort_unstable();
        assert_eq!(
            sent,
            [("x-api-key", HEADER_VALUE), ("x-scope-orgid", "tenant-a")],
            "{}",
            request.method
        );
    }
}

#[tokio::test]
async fn commands_do_not_see_a_header_variable_and_its_value_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    fake.reply(
        "query",
        vec![McpReply::Result(
            json!({"content": [{"type": "text", "text": format!("key={HEADER_VALUE}")}]}),
        )],
    );
    let model = MockServer::start(vec![
        Reply::bash("c1", "env"),
        Reply::tool_call("c2", "prom__query", &json!({})),
        Reply::text("done"),
    ])
    .await;
    let more = format!("headers = {{ X-Api-Key = \"${{{HEADER_VAR}}}\" }}");
    let home = agent_home_with(dir.path(), &fake.url, "[\"bash\"]", &more);
    let output = run(&model, dir.path(), &home).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let results = tool_results(&model);
    assert!(results[0].contains("PATH="), "{results:?}");
    assert!(!results[0].contains(HEADER_VAR), "{results:?}");
    assert_eq!(results[1], "key=[REDACTED]");
}

#[tokio::test]
async fn chat_asks_before_an_mcp_tool_that_needs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", &json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let mut child = program("chat", &model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n1\n/quit\n").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let output = ended_output(child).await;
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("[approval 1]") && stderr.contains("prom__range"),
        "{stderr}"
    );
    assert_eq!(fake.requests("tools/call").len(), 1);
    assert_eq!(tool_results(&model), [r#"range: {"q":"up"}"#]);
}

#[tokio::test]
async fn commands_do_not_see_the_mcp_token() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![Reply::bash("c1", "env"), Reply::text("done")]).await;
    let output = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[\"bash\"]"))
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let results = tool_results(&model);
    assert!(results[0].contains("PATH="), "{results:?}");
    assert!(
        !results[0].contains(TOKEN_VAR) && !results[0].contains(TOKEN),
        "{results:?}"
    );
}

#[tokio::test]
async fn the_mcp_token_is_redacted_from_tool_results() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    fake.reply(
        "query",
        vec![McpReply::Result(
            json!({"content": [{"type": "text", "text": format!("token={TOKEN}")}]}),
        )],
    );
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", &json!({})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(tool_results(&model), ["token=[REDACTED]"]);
}

#[tokio::test]
async fn startup_stops_when_an_mcp_server_cannot_be_used() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![Reply::text("never sent")]).await;
    // The token variable is not set.
    let mut unset = run(&model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"));
    unset.env_remove(TOKEN_VAR);
    // Nothing listens on port 9. Each has a home of its own, with its own config.toml.
    let other = tempfile::tempdir().unwrap();
    let unreachable = run(
        &model,
        other.path(),
        &agent_home(other.path(), "http://127.0.0.1:9/mcp", "[]"),
    );
    // The variable of a header is not set.
    let third = tempfile::tempdir().unwrap();
    let more = format!("headers = {{ X-Api-Key = \"${{{HEADER_VAR}}}\" }}");
    let mut no_header = run(
        &model,
        third.path(),
        &agent_home_with(third.path(), &fake.url, "[]", &more),
    );
    no_header.env_remove(HEADER_VAR);
    for (mut command, expected) in [
        (unset, "mcp[0].token: names ${ZX_MCP_CLI_TEST_TOKEN}, which is not set"),
        (unreachable, "MCP server prom:"),
        (
            no_header,
            "mcp[0].headers.X-Api-Key: names ${ZX_MCP_CLI_TEST_HEADER}, which is not set",
        ),
    ] {
        let output = command.output().await.unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(model.requests().is_empty());
}
