//! `zx-agent serve`: configuration errors, and the whole path against a fake Slack and a fake
//! model: a mention, an approval card and its click, hidden and redacted Slack tokens, the
//! session file, the operator log and the stop.
mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use common::fake_slack::FakeSlack;
use common::mock_server::{MockServer, Reply};

const APP_TOKEN: &str = "xapp-1-serve-test-token";
const BOT_TOKEN: &str = "xoxb-serve-test-token";
/// Only reached when the server never answers.
const WAIT: Duration = Duration::from_secs(20);

/// `zx-agent serve` with `config`, in `dir`, against `model`; Slack tokens are set.
fn serve(dir: &Path, config: &Path, model: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .args(["serve", "--config", config.to_str().unwrap(), "--model", "mock"])
        .args(["--base-url", model, "--max-retries", "0", "--no-context"])
        .current_dir(dir)
        .env("SLACK_APP_TOKEN", APP_TOKEN)
        .env("SLACK_BOT_TOKEN", BOT_TOKEN)
        .env("FEISHU_APP_ID", "cli_test")
        .env("FEISHU_APP_SECRET", "secret-test")
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        .env_remove("ZX_SECRET_FILES")
        .kill_on_drop(true);
    command
}

fn write_config(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("serve.toml");
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

#[tokio::test]
async fn configuration_errors_are_reported_before_serving() {
    let cases: [(&str, &[&str], Option<&str>, &str); 7] = [
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
        (
            VALID,
            &["--session", "x.jsonl"],
            None,
            "--session does not apply to serve",
        ),
        (
            "[slack]\ntoken = \"x\"\n[access]\nconversations = []\n[sessions]\ndir = \"s\"\n",
            &[],
            None,
            "unknown field `token`",
        ),
    ];
    for (config, extra, unset, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), config);
        let mut command = serve(dir.path(), &path, "http://127.0.0.1:9/v1");
        command.args(extra);
        if let Some(variable) = unset {
            command.env_remove(variable);
        }
        let output = command.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{config}: {stderr}");
        assert!(stderr.contains(expected), "{config}: expected {expected:?} in {stderr}");
        // Every error comes before the session directory is created.
        let created: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(created, ["serve.toml"], "{config}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn serve_answers_a_mention_with_an_approved_command_that_cannot_see_the_slack_tokens() {
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
    let child = serve(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    let mention = json!({
        "envelope_id": "e1",
        "type": "events_api",
        "payload": {"type": "event_callback", "event_id": "Ev1", "event": {
            "type": "app_mention", "user": "U1", "text": "<@UBOT> run it", "ts": "1700000001.000100", "channel": "C1"
        }},
    });
    socket.send(mention).await;
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
    assert!(texts.contains(&&json!("已批准（<@U2>）")), "{texts:?}");
    assert!(texts.contains(&&json!("done")), "{texts:?}");

    // The command ran without the tokens, and a token it read was redacted.
    model.wait_for_requests(2).await;
    let result = model.requests()[1].body["messages"].to_string();
    assert!(result.contains("app= bot="), "{result}");
    assert!(result.contains("[REDACTED]"), "{result}");
    assert!(!result.contains(APP_TOKEN) && !result.contains(BOT_TOKEN), "{result}");

    let session = dir.path().join("sessions/slack/C1/1700000001%2E000100.jsonl");
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let log: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
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
    assert!(!String::from_utf8_lossy(&output.stdout).contains(BOT_TOKEN));
    assert!(!stderr.contains(BOT_TOKEN) && !stderr.contains(APP_TOKEN));
}
