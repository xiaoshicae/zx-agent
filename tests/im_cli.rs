//! `zx-agent im`: configuration errors, and the whole path against a fake Slack and a fake
//! model: a mention, an approval card and its click, hidden and redacted Slack tokens, the
//! session file, the operator log, the stop, and a restart after a kill.
mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use common::fake_slack::FakeSlack;
use common::mock_server::{MockServer, Reply};
use common::{fake_model_config, json_lines, stderr, stdout, zx_agent};

const APP_TOKEN: &str = "xapp-1-im-test-token";
const BOT_TOKEN: &str = "xoxb-im-test-token";
/// Only reached when the server never answers.
const WAIT: Duration = Duration::from_secs(20);

/// `zx-agent im` with `config`, in `dir`, against `model`; Slack tokens are set, and the
/// zx-agent home in `dir` has a config.toml without retries.
fn im(dir: &Path, config: &Path, model: &str) -> Command {
    let home = dir.join(".zx-agent-home");
    common::write_config(&home, &fake_model_config("ZX_IM_CLI_TEST_KEY", "", ""));
    let mut command = zx_agent();
    command
        .args(["im", "--config", config.to_str().unwrap(), "--model", "mock"])
        .current_dir(dir)
        .env("ZX_AGENT_HOME", &home)
        .env("ZX_BASE_URL", model)
        .env("SLACK_APP_TOKEN", APP_TOKEN)
        .env("SLACK_BOT_TOKEN", BOT_TOKEN)
        .env("FEISHU_APP_ID", "cli_test")
        .env("FEISHU_APP_SECRET", "secret-test")
        .env_remove("ZX_SECRET_FILES")
        .kill_on_drop(true);
    command
}

fn write_config(dir: &Path, text: &str) -> std::path::PathBuf {
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
            "FEISHU_APP_SECRET must hold",
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
        (VALID, &["--session", "x.jsonl"], None, "--session does not apply to im"),
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
        let path = write_config(dir.path(), config);
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
    let config = write_config(
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

    let session = dir.path().join("sessions/slack/C1/1700000001%2E000100/1.jsonl");
    let recorded = std::fs::read_to_string(&session).unwrap();
    assert!(recorded.contains("run it"));
    assert!(!recorded.contains(BOT_TOKEN));

    let pid = child.id().unwrap().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().await.unwrap();
    assert!(killed.success());
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
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
async fn configured_replies_and_card_labels_replace_the_english_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::bash("c1", "echo hi"), Reply::text("done")]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_config(
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
    let config = write_config(
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
    tokio::time::timeout(WAIT, model.wait_for_requests(1)).await.unwrap();
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
    let pid = second.id().unwrap().to_string();
    let stopped = Command::new("kill").args(["-TERM", &pid]).status().await.unwrap();
    assert!(stopped.success());
    let output = tokio::time::timeout(WAIT, second.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}
