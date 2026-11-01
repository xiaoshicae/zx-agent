//! The compiled program with MCP servers from a profile: run, chat and serve connect them at
//! startup; a tool the server's allowTools lists runs, the others ask (and `run`, with no one
//! to ask, refuses them); the token is hidden from commands and redacted; a server that
//! cannot be used stops startup.
mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use common::fake_mcp::{Era, FakeMcp, Reply as McpReply, tool};
use common::mock_server::{MockServer, Reply};

const TOKEN_VAR: &str = "ZX_MCP_CLI_TEST_TOKEN";
const TOKEN: &str = "mcp-cli-secret-5678";
const KEY_VAR: &str = "ZX_MCP_CLI_TEST_KEY";
/// Only reached when the program never prints what is expected.
const WAIT: Duration = Duration::from_secs(20);

async fn prom() -> FakeMcp {
    FakeMcp::start(
        Era::Modern,
        vec![tool("query", "Instant query."), tool("range", "Range query.")],
    )
    .await
}

/// A profile with one MCP server, where `query` runs without approval and `range` asks.
fn profile(dir: &Path, url: &str, builtin_tools: &str) -> PathBuf {
    let path = dir.join("mcp.toml");
    let text = format!(
        "tools = {builtin_tools}\n[[mcp]]\nname = \"prom\"\nurl = \"{url}\"\ntokenEnv = \"{TOKEN_VAR}\"\n\
         tools = [\"query\", \"range\"]\nallowTools = [\"query\"]\n"
    );
    std::fs::write(&path, text).unwrap();
    path
}

/// `zx-agent <mode>` against `model` with `profile`, the token set and no API key.
fn program(mode: &str, model: &MockServer, dir: &Path, profile: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .args([mode, "--model", "mock", "--base-url", &model.base_url])
        .args(["--max-retries", "0", "--api-key-env", KEY_VAR, "--no-context"])
        .args(["--profile", profile.to_str().unwrap()])
        .current_dir(dir)
        .env(TOKEN_VAR, TOKEN)
        .env_remove(KEY_VAR)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .env_remove("ZX_SECRET_FILES")
        .kill_on_drop(true);
    command
}

fn run(model: &MockServer, dir: &Path, profile: &Path) -> Command {
    let mut command = program("run", model, dir, profile);
    command.arg("go").stdin(Stdio::null());
    command
}

/// The tool results in the model's last request.
fn tool_results(model: &MockServer) -> Vec<String> {
    let requests = model.requests();
    requests.last().unwrap().body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect()
}

fn origin(fake: &FakeMcp) -> &str {
    fake.url.trim_end_matches("/mcp")
}

#[tokio::test]
async fn run_calls_an_mcp_tool_its_server_allows() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__query", json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &profile(dir.path(), &fake.url, "[]"))
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
    let startup = format!(
        "[mcp] prom: prom__query, prom__range (asks); protocol 2026-07-28; {}; the token is sent unencrypted",
        origin(&fake)
    );
    assert!(stderr.contains(&startup), "{stderr}");
    assert_eq!(tool_results(&model), [r#"query: {"q":"up"}"#]);
    assert_eq!(fake.requests("tools/call").len(), 1);
}

#[tokio::test]
async fn run_refuses_an_mcp_tool_that_needs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &profile(dir.path(), &fake.url, "[]"))
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let results = tool_results(&model);
    assert!(results[0].contains("no human is available"), "{results:?}");
    assert!(fake.requests("tools/call").is_empty());
}

#[tokio::test]
async fn chat_asks_before_an_mcp_tool_that_needs_approval() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(vec![
        Reply::tool_call("c1", "prom__range", json!({"q": "up"})),
        Reply::text("done"),
    ])
    .await;
    let mut child = program("chat", &model, dir.path(), &profile(dir.path(), &fake.url, "[]"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n/approve 1 once\n/quit\n").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
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
    let output = run(&model, dir.path(), &profile(dir.path(), &fake.url, "[\"bash\"]"))
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
        Reply::tool_call("c1", "prom__query", json!({})),
        Reply::text("done"),
    ])
    .await;
    let output = run(&model, dir.path(), &profile(dir.path(), &fake.url, "[]"))
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
    let mut unset = run(&model, dir.path(), &profile(dir.path(), &fake.url, "[]"));
    unset.env_remove(TOKEN_VAR);
    // Nothing listens on port 9.
    let unreachable = run(&model, dir.path(), &profile(dir.path(), "http://127.0.0.1:9/mcp", "[]"));
    for (mut command, expected) in [(unset, "must hold its token"), (unreachable, "MCP server prom:")] {
        let output = command.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn serve_connects_the_mcp_servers_before_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(Vec::new()).await;
    let config = dir.path().join("serve.toml");
    // Nothing listens on port 9, so the Slack connection keeps failing; this test stops first.
    std::fs::write(
        &config,
        "[slack]\napiBase = \"http://127.0.0.1:9/api\"\n[access]\nconversations = [\"slack:C1\"]\n\
         approvers = [\"slack:U2\"]\n[sessions]\ndir = \"sessions\"\n",
    )
    .unwrap();
    let mut child = program("serve", &model, dir.path(), &profile(dir.path(), &fake.url, "[]"))
        .args(["--config", config.to_str().unwrap()])
        .env("SLACK_APP_TOKEN", "xapp-test")
        .env("SLACK_BOT_TOKEN", "xoxb-test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let serving = line.starts_with("[serve] platforms=");
            seen.push(line);
            if serving {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("serve did not start: {seen:?}"));
    child.kill().await.unwrap();
    let mcp = seen.iter().position(|line| line.starts_with("[mcp] prom: prom__query"));
    assert!(mcp.is_some_and(|index| index < seen.len() - 1), "{seen:?}");
    assert!(fake.requests("tools/list").len() >= 2, "{:?}", fake.received());
}
