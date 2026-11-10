//! The compiled program with MCP servers from config.toml: run, chat and im connect them at
//! startup; a tool the agent's permissions.allow lists runs, the others ask (and `run`, with no one
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
use common::{stderr, stdout, write_agent, write_config, zx_agent};

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

/// A zx-agent home in `dir` whose agents enable `builtin_tools`, and whose config.toml has one
/// MCP server, where `query` runs without approval and `range` asks.
fn agent_home(dir: &Path, url: &str, builtin_tools: &str) -> PathBuf {
    let home = dir.join(".zx-agent");
    for builtin in ["unattended", "assistant"] {
        write_agent(
            &home,
            builtin,
            &format!(
                "tools = {builtin_tools}\n[context]\ndiscover = false\n[permissions]\nallow = [\"prom__query\"]\n"
            ),
        );
    }
    write_config(
        &home,
        &format!(
            "[[mcp]]\nname = \"prom\"\nurl = \"{url}\"\ntokenEnv = \"{TOKEN_VAR}\"\n\
             tools = [\"query\", \"range\"]\n\
             [provider]\napiKeyEnv = \"{KEY_VAR}\"\nmaxRetries = 0\n"
        ),
    );
    home
}

/// `zx-agent <mode>` against `model` with the zx-agent home `home`, the token set and no API
/// key.
fn program(mode: &str, model: &MockServer, dir: &Path, home: &Path) -> Command {
    let mut command = zx_agent();
    command
        .args([mode, "--model", "mock"])
        .current_dir(dir)
        .env("ZX_BASE_URL", &model.base_url)
        // chat keeps sessions under the zx-agent home by default; never the real one.
        .env("ZX_AGENT_HOME", home)
        .env(TOKEN_VAR, TOKEN)
        .env_remove(KEY_VAR)
        .env_remove("ZX_SECRET_FILES")
        .kill_on_drop(true);
    command
}

fn run(model: &MockServer, dir: &Path, home: &Path) -> Command {
    let mut command = program("run", model, dir, home);
    command.arg("go").stdin(Stdio::null());
    command
}

/// The tool results in the model's last request.
fn tool_results(model: &MockServer) -> Vec<String> {
    let requests = model.requests();
    requests
        .last()
        .unwrap()
        .tool_results()
        .iter()
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
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
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
    for (mut command, expected) in [(unset, "must hold its token"), (unreachable, "MCP server prom:")] {
        let output = command.output().await.unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
    }
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn im_connects_the_mcp_servers_before_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let fake = prom().await;
    let model = MockServer::start(Vec::new()).await;
    let config = dir.path().join("im.toml");
    // Nothing listens on port 9, so the Slack connection keeps failing; this test stops first.
    std::fs::write(
        &config,
        "[slack]\napiBase = \"http://127.0.0.1:9/api\"\n[access]\nconversations = [\"slack:C1\"]\n\
         approvers = [\"slack:U2\"]\n[sessions]\ndir = \"sessions\"\n",
    )
    .unwrap();
    let mut child = program("im", &model, dir.path(), &agent_home(dir.path(), &fake.url, "[]"))
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
            let serving = line.starts_with("[im] platforms=");
            seen.push(line);
            if serving {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("im did not start: {seen:?}"));
    child.kill().await.unwrap();
    let mcp = seen.iter().position(|line| line.starts_with("[mcp] prom: prom__query"));
    assert!(mcp.is_some_and(|index| index < seen.len() - 1), "{seen:?}");
    assert!(fake.requests("tools/list").len() >= 2, "{:?}", fake.received());
}
