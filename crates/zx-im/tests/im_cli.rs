//! The program `zx-im`: configuration errors, the MCP servers it connects before serving,
//! and the whole path against a fake Slack and a fake model: a mention, an approval card and
//! its click, hidden and redacted Slack tokens, the session file, the operator log, the stop,
//! and restarts after a kill: a status replaced, and a message not handled yet answered.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use zx_testing::fake_mcp::{Era, FakeMcp, tool};
use zx_testing::fake_slack::{Answer, FakeSlack};
use zx_testing::mock_server::{MockServer, Reply};
use zx_testing::program::{self, Home, fake_model_config, write_agent};
use zx_testing::{json_lines, stderr, stdout};

/// The variable that config.toml names for the fake model's API key.
const KEY_VAR: &str = "ZX_IM_CLI_TEST_KEY";
const APP_TOKEN: &str = "xapp-1-im-test-token";
const BOT_TOKEN: &str = "xoxb-im-test-token";
/// How long a test waits for the program to start serving: longer than [`zx_testing::WAIT`],
/// as it connects its MCP servers first. Only reached when it never does.
const START_WAIT: Duration = Duration::from_secs(20);

/// The error for a `[feishu]` section whose secret is not set: a build without that platform
/// refuses the section itself.
#[cfg(feature = "feishu")]
const FEISHU_SECRET_UNSET: &str = "FEISHU_APP_SECRET must hold";
#[cfg(not(feature = "feishu"))]
const FEISHU_SECRET_UNSET: &str = "this build of zx-im has no such platform";

/// The default variables of the platforms' credentials, which `zx-im` reads besides the
/// variables that [`program::command`] withholds.
const PLATFORM_CREDENTIALS: [&str; 4] = [
    "SLACK_APP_TOKEN",
    "SLACK_BOT_TOKEN",
    "FEISHU_APP_ID",
    "FEISHU_APP_SECRET",
];

/// The compiled `zx-im`, started in `cwd` with the zx-agent home `home` and apart from the
/// developer's environment, as [`program::command`] starts a program, and without the
/// developer's platform credentials.
fn zx_im_in(cwd: &Path, home: Home<'_>) -> Command {
    let mut command = program::command(Path::new(env!("CARGO_BIN_EXE_zx-im")), cwd, home);
    for name in PLATFORM_CREDENTIALS {
        command.env_remove(name);
    }
    command
}

/// `zx-im` with `config`, in `dir`, against `model`; Slack tokens are set, and the zx-agent
/// home in `dir` has a config.toml without retries.
fn im(dir: &Path, config: &Path, model: &str) -> Command {
    let home = dir.join(".zx-agent-home");
    program::write_config(&home, &fake_model_config(KEY_VAR, "", ""));
    let mut command = zx_im_in(dir, Home::Agent(&home));
    command
        .args(["--config", config.to_str().unwrap(), "--model", "mock"])
        .env("ZX_BASE_URL", model)
        .env("SLACK_APP_TOKEN", APP_TOKEN)
        .env("SLACK_BOT_TOKEN", BOT_TOKEN)
        .env("FEISHU_APP_ID", "cli_test")
        .env("FEISHU_APP_SECRET", "secret-test");
    command
}

/// The text of the one session file kept under `sessions`.
fn only_session(sessions: &Path) -> String {
    let files: Vec<_> = std::fs::read_dir(sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .flat_map(|directory| program::session_files(&directory))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    std::fs::read_to_string(&files[0]).unwrap()
}

fn write_im_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("im.toml");
    std::fs::write(&path, text).unwrap();
    path
}

const VALID: &str = r#"
[slack]

[access]
conversations = ["slack:C1"]
approvers = ["slack:U2"]

[sessions]
dir = "sessions"
"#;

/// A Socket Mode envelope: user U1 mentions the bot in C1 with message `ts`.
fn mention(envelope: &str, ts: &str, text: &str) -> Value {
    json!({
        "envelope_id": envelope,
        "type": "events_api",
        "payload": {"type": "event_callback", "event_id": format!("Ev-{envelope}"), "event": {
            "type": "app_mention", "user": "U1", "text": format!("<@UBOT> {text}"), "ts": ts, "channel": "C1"
        }},
    })
}

#[tokio::test]
async fn configuration_errors_are_reported_before_serving() {
    let cases: [(&str, &[&str], Option<&str>, &str); 8] = [
        (
            "[slack]\n[sessions]\ndir = \"s\"\n",
            &[],
            None,
            "missing field `access`",
        ),
        (VALID, &[], Some("SLACK_APP_TOKEN"), "SLACK_APP_TOKEN must hold"),
        (
            "[feishu]\n[access]\nconversations = []\n[sessions]\ndir = \"s\"\n",
            &[],
            Some("FEISHU_APP_SECRET"),
            FEISHU_SECRET_UNSET,
        ),
        (
            "[access]\nconversations = []\n[sessions]\ndir = \"s\"\n",
            &[],
            None,
            "no IM platform is configured",
        ),
        (
            "[slack]\n[access]\nconversations = [\"teams:C1\"]\n[sessions]\ndir = \"s\"\n",
            &[],
            None,
            "which is not configured",
        ),
        (
            VALID,
            &["--session", "x.jsonl"],
            None,
            "unexpected argument '--session'",
        ),
        (
            "[slack]\ntoken = \"x\"\n[access]\nconversations = []\n[sessions]\ndir = \"s\"\n",
            &[],
            None,
            "unknown field `token`",
        ),
        (
            "[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"s\"\n[replies]\ngreeting = \"hi\"\n",
            &[],
            None,
            "unknown field `greeting`",
        ),
    ];
    for (config, extra, unset, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write_im_config(dir.path(), config);
        let mut command = im(dir.path(), &path, "http://127.0.0.1:9/v1");
        command.args(extra);
        if let Some(variable) = unset {
            command.env_remove(variable);
        }
        let output = command.output().await.unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{config}: {stderr}");
        assert!(stderr.contains(expected), "{config}: expected {expected:?} in {stderr}");
        // Every error comes before the session directory is created; the zx-agent home is
        // the test's.
        let mut created: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        created.sort();
        assert_eq!(created, [".zx-agent-home", "im.toml"], "{config}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn im_answers_a_mention_with_an_approved_command_that_cannot_see_the_slack_tokens() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token.txt"), format!("{BOT_TOKEN}\n")).unwrap();
    let command = "echo app=$SLACK_APP_TOKEN bot=$SLACK_BOT_TOKEN; cat token.txt";
    let model = MockServer::start(vec![Reply::bash("c1", command), Reply::text("done")]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &format!("{VALID}\n[limits]\nidleSecs = 600\n")
            .replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let child = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "run it")).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));

    // The status message, then the approval card, both in the mention's thread.
    let posts = slack.wait_for("chat.postMessage", 2).await;
    assert_eq!(posts[0].body["thread_ts"], json!("1700000001.000100"));
    let card = &posts[1].body;
    let approval = card["blocks"][1]["elements"][0]["value"].as_str().unwrap().to_string();
    socket
        .send(json!({
            "envelope_id": "i1",
            "type": "interactive",
            "payload": {
                "type": "block_actions",
                "user": {"id": "U2"},
                "channel": {"id": "C1"},
                // No thread here: the card's button value carries it.
                "message": {"ts": "1700000000.000002"},
                "actions": [{"action_id": "zx_allow_once", "value": approval, "type": "button"}],
            },
        }))
        .await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "i1"}));

    // The card shows who allowed it, and the status message becomes the answer.
    let updates = slack.wait_for("chat.update", 2).await;
    let texts: Vec<&Value> = updates.iter().map(|call| &call.body["text"]).collect();
    assert!(texts.contains(&&json!("Allowed (<@U2>)")), "{texts:?}");
    assert!(texts.contains(&&json!("done")), "{texts:?}");

    // The command ran without the tokens, and a token it read was redacted.
    model.wait_for_requests(2).await;
    let result = model.requests()[1].body["messages"].to_string();
    assert!(result.contains("app= bot="), "{result}");
    assert!(result.contains("[REDACTED]"), "{result}");
    assert!(!result.contains(APP_TOKEN) && !result.contains(BOT_TOKEN), "{result}");

    // Whatever its file is called, the session's labels say which conversation it is.
    let recorded = only_session(&dir.path().join("sessions"));
    let header: Value = serde_json::from_str(recorded.lines().next().unwrap()).unwrap();
    assert_eq!(
        header["labels"],
        json!({"im.conversation": "C1", "im.platform": "slack", "im.thread": "1700000001.000100"})
    );
    assert!(recorded.contains("run it"));
    assert!(!recorded.contains(BOT_TOKEN));

    zx_testing::signal(&child, "-TERM").await;
    let output = program::ended_output(child).await;
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let log: Vec<Value> = json_lines(&stdout(&output));
    let approval_line = log
        .iter()
        .find(|line| line["type"] == "approval")
        .unwrap_or_else(|| panic!("no approval line in {log:?}"));
    assert_eq!(
        (
            &approval_line["user"],
            &approval_line["result"],
            &approval_line["thread"]
        ),
        (&json!("U2"), &json!("allow_once"), &json!("1700000001.000100"))
    );
    assert!(!stdout(&output).contains(BOT_TOKEN));
    assert!(!stderr.contains(BOT_TOKEN) && !stderr.contains(APP_TOKEN));
}

#[cfg(unix)]
#[tokio::test]
async fn im_tells_the_model_how_the_conversation_reaches_people() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::text("done")]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &VALID.replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let mut child = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "hello")).await;
    model.wait_for_requests(1).await;

    // A section of the prompt, after what the assembly says of the machine: only the last
    // answer of a run is posted, so the model is told to make it stand alone.
    let system = &model.requests()[0].body["messages"][0];
    assert_eq!(system["role"], json!("system"));
    let prompt = system["content"].as_str().unwrap();
    let (before, section) = prompt.split_once("<conversation>").expect(prompt);
    assert!(before.contains("<environment>"), "{prompt}");
    assert!(section.contains("Only your final answer"), "{prompt}");
    assert!(section.contains("sender's name"), "{prompt}");
    child.kill().await.unwrap();
}

#[tokio::test]
async fn configured_replies_and_card_labels_replace_the_english_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::bash("c1", "echo hi"), Reply::text("done")]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &format!(
            "{VALID}\n[replies]\nworking = \"处理中…\"\n[cardLabels]\ntitle = \"需要审批\"\nallowed = \"已批准\"\n"
        )
        .replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let _child = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "run it")).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    let posts = slack.wait_for("chat.postMessage", 2).await;
    assert_eq!(posts[0].body["text"], json!("处理中…"));
    let card = posts[1].body.to_string();
    assert!(card.contains("需要审批"), "{card}");
    // A label the file leaves out keeps its default.
    assert!(card.contains("Allow once"), "{card}");

    let approval = posts[1].body["blocks"][1]["elements"][0]["value"]
        .as_str()
        .unwrap()
        .to_string();
    socket
        .send(json!({
            "envelope_id": "i1",
            "type": "interactive",
            "payload": {
                "type": "block_actions",
                "user": {"id": "U2"},
                "channel": {"id": "C1"},
                "message": {"ts": "1700000000.000002"},
                "actions": [{"action_id": "zx_allow_once", "value": approval, "type": "button"}],
            },
        }))
        .await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "i1"}));
    let updates = slack.wait_for("chat.update", 2).await;
    let texts: Vec<&Value> = updates.iter().map(|call| &call.body["text"]).collect();
    assert!(texts.contains(&&json!("已批准 (<@U2>)")), "{texts:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_status_left_by_a_killed_server_is_replaced_when_it_starts_again() {
    let dir = tempfile::tempdir().unwrap();
    // The model never answers, so the run is still going when the server is killed.
    let model = MockServer::start(vec![Reply::Stall(Vec::new())]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &VALID.replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let mut first = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "wait")).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    // The status is kept before the run asks the model.
    model.wait_for_requests(1).await;
    let status = "1700000000.000001";
    assert_eq!(slack.calls("chat.postMessage").len(), 1);
    first.kill().await.unwrap();
    let running = dir.path().join("sessions/.running");
    let left: Vec<_> = std::fs::read_dir(&running)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["slack.C1.1700000000%2E000001.json"]);

    let second = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let updates = slack.wait_for("chat.update", 1).await;
    assert_eq!(
        (
            &updates[0].body["channel"],
            &updates[0].body["ts"],
            &updates[0].body["text"]
        ),
        (
            &json!("C1"),
            &json!(status),
            &json!("The service restarted: this task did not finish; please send it again.")
        )
    );
    zx_testing::signal(&second, "-TERM").await;
    let output = program::ended_output(second).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[cfg(unix)]
#[tokio::test]
async fn a_message_a_killed_server_had_not_handled_is_answered_when_it_starts_again() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::text("late answer")]).await;
    let mut slack = FakeSlack::start().await;
    // The first server waits out a rate limit reading the thread, before it makes the
    // conversation's session: the message is kept, and nothing has handled it yet.
    slack.script(
        "conversations.replies",
        vec![Answer::RateLimited { retry_after_secs: 60 }],
    );
    let config = write_im_config(
        dir.path(),
        &VALID.replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let mut first = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut socket = slack.socket().await;
    // A mention in a thread someone else started, whose first session reads the thread first.
    socket
        .send(json!({
            "envelope_id": "e1",
            "type": "events_api",
            "payload": {"type": "event_callback", "event_id": "Ev-e1", "event": {
                "type": "app_mention", "user": "U1", "text": "<@UBOT> still there?",
                "ts": "1700000001.000200", "thread_ts": "1700000001.000100", "channel": "C1"
            }},
        }))
        .await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    slack.wait_for("conversations.replies", 1).await;
    first.kill().await.unwrap();
    let pending = dir.path().join("sessions/.backlog/pending");
    let left = |dir: &Path| -> Vec<_> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    };
    assert_eq!(left(&pending), ["slack.C1.1700000001%2E000200.json"]);

    let second = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let updates = slack.wait_for("chat.update", 1).await;
    assert_eq!(
        (&updates[0].body["channel"], &updates[0].body["text"]),
        (&json!("C1"), &json!("late answer"))
    );
    zx_testing::signal(&second, "-TERM").await;
    let output = program::ended_output(second).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(left(&pending).is_empty(), "{:?}", left(&pending));
}

const MCP_TOKEN_VAR: &str = "ZX_IM_CLI_TEST_MCP_TOKEN";
const MCP_TOKEN: &str = "mcp-im-secret-5678";

/// A zx-agent home in `dir` whose assistant agent enables no built-in tools, and whose
/// config.toml has one MCP server at `url`, where `query` runs without approval and `range`
/// asks.
fn mcp_home(dir: &Path, url: &str) -> PathBuf {
    let home = dir.join(".zx-agent-home");
    write_agent(
        &home,
        "assistant",
        "tools = []\n[context]\ndiscover = false\n[permissions]\nallow = [\"prom__query\"]\n",
    );
    let server = format!(
        "[[mcp]]\nname = \"prom\"\nurl = \"{url}\"\ntokenEnv = \"{MCP_TOKEN_VAR}\"\ntools = [\"query\", \"range\"]"
    );
    program::write_config(&home, &fake_model_config(KEY_VAR, &server, ""));
    home
}

#[tokio::test]
async fn im_connects_the_mcp_servers_before_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeMcp::start(
        Era::Modern,
        vec![tool("query", "Instant query."), tool("range", "Range query.")],
    )
    .await;
    let model = MockServer::start(Vec::new()).await;
    // Nothing listens on port 9, so the Slack connection keeps failing; this test stops first.
    let config = write_im_config(
        dir.path(),
        "[slack]\napiBase = \"http://127.0.0.1:9/api\"\n[access]\nconversations = [\"slack:C1\"]\n\
         approvers = [\"slack:U2\"]\n[sessions]\ndir = \"sessions\"\n",
    );
    let mut child = zx_im_in(dir.path(), Home::Agent(&mcp_home(dir.path(), &fake.url)))
        .args(["--config", config.to_str().unwrap(), "--model", "mock"])
        .env("ZX_BASE_URL", &model.base_url)
        .env(MCP_TOKEN_VAR, MCP_TOKEN)
        .env("SLACK_APP_TOKEN", "xapp-test")
        .env("SLACK_BOT_TOKEN", "xoxb-test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = Vec::new();
    tokio::time::timeout(START_WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let serving = line.starts_with("[im] platforms=");
            seen.push(line);
            if serving {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("zx-im did not start: {seen:?}"));
    child.kill().await.unwrap();
    let mcp = seen.iter().position(|line| line.starts_with("[mcp] prom: prom__query"));
    assert!(mcp.is_some_and(|index| index < seen.len() - 1), "{seen:?}");
    assert!(fake.requests("tools/list").len() >= 2, "{:?}", fake.received());
}
