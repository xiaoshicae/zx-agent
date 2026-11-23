//! The program `zx-im`: configuration errors, the MCP servers it connects before serving,
//! and the whole path against a fake Slack and a fake model: a mention, an approval card and
//! its click, hidden and redacted Slack tokens, the session file, the operator log, the stop,
//! restarts after a kill (a status replaced, and a message not handled yet answered), and the
//! agent a chat has of its own.

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

/// The first line of the operator log `log` whose `type` is `kind`.
fn logged<'a>(log: &'a [Value], kind: &str) -> &'a Value {
    log.iter()
        .find(|line| line["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} line in {log:?}"))
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

    // The card shows who allowed it; the answer follows the card, and the status message says
    // so.
    let updates = slack.wait_for("chat.update", 2).await;
    let texts: Vec<&Value> = updates.iter().map(|call| &call.body["text"]).collect();
    assert!(texts.contains(&&json!("Allowed (<@U2>)")), "{texts:?}");
    assert!(texts.contains(&&json!("Done; the answer is below.")), "{texts:?}");
    let posts = slack.wait_for("chat.postMessage", 3).await;
    assert_eq!(
        (&posts[2].body["text"], &posts[2].body["thread_ts"]),
        (&json!("done"), &json!("1700000001.000100"))
    );

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
    let approval_line = logged(&log, "approval");
    assert_eq!(
        (
            &approval_line["user"],
            &approval_line["result"],
            &approval_line["thread"]
        ),
        (&json!("U2"), &json!("allow_once"), &json!("1700000001.000100"))
    );
    let run_line = logged(&log, "run");
    assert_eq!(
        (&run_line["work"], &run_line["outcome"], &run_line["modelRequests"]),
        (&json!("prompt"), &json!("completed"), &json!(2))
    );
    assert!(!stdout(&output).contains(BOT_TOKEN));
    assert!(!stderr.contains(BOT_TOKEN) && !stderr.contains(APP_TOKEN));
}

#[cfg(unix)]
#[tokio::test]
async fn a_mention_in_a_channel_not_on_the_list_is_refused_and_logged_with_its_sender() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(Vec::new()).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &VALID.replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base)),
    );
    let child = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    let mut elsewhere = mention("e1", "1700000001.000100", "hello");
    elsewhere["payload"]["event"]["channel"] = json!("C9");
    socket.send(elsewhere).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    let posts = slack.wait_for("chat.postMessage", 1).await;
    assert_eq!(
        (&posts[0].body["channel"], &posts[0].body["text"]),
        (
            &json!("C9"),
            &json!("Not authorized: this chat is not on the service's list.")
        )
    );

    zx_testing::signal(&child, "-TERM").await;
    let output = program::ended_output(child).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let log: Vec<Value> = json_lines(&stdout(&output));
    let line = logged(&log, "unauthorized");
    assert_eq!(
        (&line["platform"], &line["conversation"], &line["thread"], &line["user"]),
        (&json!("slack"), &json!("C9"), &json!("1700000001.000100"), &json!("U1"))
    );
    assert!(model.requests().is_empty());
}

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

/// The instructions of the agent `prod`, which no built-in agent has.
const PROD_INSTRUCTIONS: &str = "You only look at production.";

/// [`VALID`] with the slack:C2 served too, where the agent `prod` answers.
fn with_prod_chat(slack_api: &str) -> String {
    let served = VALID
        .replace("[slack]\n", &format!("[slack]\napiBase = \"{slack_api}\"\n"))
        .replace("[\"slack:C1\"]", "[\"slack:C1\", \"slack:C2\"]");
    format!("{served}\n[[chats]]\nchat = \"slack:C2\"\nagent = \"prod\"\n")
}

#[tokio::test]
async fn a_chat_with_an_agent_of_its_own_is_answered_by_that_agent() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::text("assistant"), Reply::text("prod")]).await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(dir.path(), &with_prod_chat(&slack.api_base));
    let mut command = im(dir.path(), &config, &model.base_url);
    program::write_agent_file(
        &dir.path().join(".zx-agent-home"),
        "prod",
        &program::agent_text("extends = \"assistant\"", PROD_INSTRUCTIONS),
    );
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "hello")).await;
    model.wait_for_requests(1).await;
    let mut in_prod = mention("e2", "1700000002.000100", "hello");
    in_prod["payload"]["event"]["channel"] = json!("C2");
    socket.send(in_prod).await;
    model.wait_for_requests(2).await;

    let system = |index: usize| model.requests()[index].body["messages"][0]["content"].to_string();
    assert!(!system(0).contains(PROD_INSTRUCTIONS), "{}", system(0));
    assert!(system(1).contains(PROD_INSTRUCTIONS), "{}", system(1));
    child.kill().await.unwrap();
}

/// The next line of the operator log `log` whose `type` is `kind`.
#[cfg(unix)]
async fn next_log_line(log: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>, kind: &str) -> Value {
    let found = tokio::time::timeout(START_WAIT, async {
        while let Some(line) = log.next_line().await.unwrap() {
            let line: Value = serde_json::from_str(&line).unwrap();
            if line["type"] == kind {
                return Some(line);
            }
        }
        None
    });
    match found.await {
        Ok(Some(line)) => line,
        Ok(None) => panic!("the log ended without a {kind} line"),
        Err(_) => panic!("no {kind} line in time"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_sighup_reads_the_lists_again_and_a_broken_file_keeps_them() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![Reply::text("hello from C2")]).await;
    let mut slack = FakeSlack::start().await;
    let served = VALID.replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base));
    let config = write_im_config(dir.path(), &served);
    let mut child = im(dir.path(), &config, &model.base_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut log = BufReader::new(child.stdout.take().unwrap()).lines();
    // Connected once serving: the signal is listened for by then.
    let mut socket = slack.socket().await;
    let in_c2 = |envelope: &str, ts: &str| {
        let mut mention = mention(envelope, ts, "hello");
        mention["payload"]["event"]["channel"] = json!("C2");
        mention
    };
    socket.send(in_c2("e1", "1700000001.000100")).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    let refused = slack.wait_for("chat.postMessage", 1).await;
    assert_eq!(
        refused[0].body["text"],
        json!("Not authorized: this chat is not on the service's list.")
    );

    std::fs::write(&config, "[access\n").unwrap();
    zx_testing::signal(&child, "-HUP").await;
    let failed = next_log_line(&mut log, "failed").await;
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("the lists stay as they were"),
        "{failed}"
    );

    std::fs::write(
        &config,
        served.replace("[\"slack:C1\"]", "[\"slack:C1\", \"slack:C2\"]"),
    )
    .unwrap();
    zx_testing::signal(&child, "-HUP").await;
    let reloaded = next_log_line(&mut log, "reloaded").await;
    assert_eq!(
        (&reloaded["conversations"], &reloaded["approvers"]),
        (&json!(2), &json!(1))
    );
    socket.send(in_c2("e2", "1700000002.000100")).await;
    model.wait_for_requests(1).await;
    child.kill().await.unwrap();
}

#[tokio::test]
async fn settings_the_zx_agent_home_cannot_serve_stop_the_start() {
    let cases = [
        // No agents/prod.md.
        (
            with_prod_chat("http://127.0.0.1:9/api"),
            ["chats \"slack:C2\": agent \"prod\": ", "prod.md does not exist"],
        ),
        // No prices for the model in config.toml.
        (
            VALID.replace("[sessions]", "[limits]\ndailyCost = 1.0\n\n[sessions]"),
            ["dailyCost needs the prices of the model \"mock\"", "[[models]]"],
        ),
    ];
    for (config, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write_im_config(dir.path(), &config);
        let output = im(dir.path(), &path, "http://127.0.0.1:9/v1").output().await.unwrap();
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{config}: {stderr}");
        for part in expected {
            assert!(stderr.contains(part), "{config}: expected {part:?} in {stderr}");
        }
    }
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

/// The variable that names the Postgres of the tests; without it, the tests that need one are
/// skipped.
#[cfg(feature = "postgres")]
const TEST_POSTGRES_VAR: &str = "ZX_TEST_POSTGRES_URL";
/// The variable that im.toml names for the connection string of Postgres.
#[cfg(feature = "postgres")]
const POSTGRES_URL_VAR: &str = "ZX_IM_CLI_TEST_POSTGRES_URL";

/// The connection string of a new schema of its own in the Postgres of the tests; `None`, said
/// on stderr, when there is none.
#[cfg(feature = "postgres")]
async fn postgres_schema() -> Option<String> {
    let Some(url) = std::env::var(TEST_POSTGRES_VAR).ok().filter(|url| !url.is_empty()) else {
        eprintln!("{TEST_POSTGRES_VAR} is not set: this test of zx-im with Postgres is skipped");
        return None;
    };
    let schema = format!("zx_cli_{:016x}", zx_util::random::random_u64());
    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("CREATE SCHEMA {schema}")).await.unwrap();
    let separator = if url.contains('?') { '&' } else { '?' };
    Some(format!("{url}{separator}options=-c%20search_path%3D{schema}"))
}

#[cfg(all(unix, feature = "postgres"))]
#[tokio::test]
async fn a_conversation_kept_in_postgres_goes_on_after_a_restart_and_commands_cannot_see_its_url() {
    let Some(url) = postgres_schema().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let model = MockServer::start(vec![
        Reply::bash("c1", &format!("echo url=[${POSTGRES_URL_VAR}]")),
        Reply::text("first answer"),
        Reply::text("second answer"),
    ])
    .await;
    let mut slack = FakeSlack::start().await;
    let config = write_im_config(
        dir.path(),
        &VALID
            .replace("[slack]\n", &format!("[slack]\napiBase = \"{}\"\n", slack.api_base))
            .replace(
                "dir = \"sessions\"",
                &format!("postgresUrlEnv = \"{POSTGRES_URL_VAR}\""),
            ),
    );
    let start = || {
        im(dir.path(), &config, &model.base_url)
            .env(POSTGRES_URL_VAR, &url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };

    let first = start();
    let mut socket = slack.socket().await;
    socket.send(mention("e1", "1700000001.000100", "run it")).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    let posts = slack.wait_for("chat.postMessage", 2).await;
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
    let posts = slack.wait_for("chat.postMessage", 3).await;
    assert_eq!(posts[2].body["text"], json!("first answer"));
    // The command ran without the variable that holds the connection string.
    let result = model.requests()[1].body["messages"].to_string();
    assert!(result.contains("url=[]"), "{result}");
    zx_testing::signal(&first, "-TERM").await;
    let output = program::ended_output(first).await;
    let said = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{said}");
    assert!(said.contains("sessions in Postgres;"), "{said}");
    assert!(!said.contains(&url), "{said}");
    assert!(!dir.path().join("sessions").exists());

    // Started again, the server goes on in the conversation's session, which Postgres kept.
    let second = start();
    let mut socket = slack.socket().await;
    let mut reply = mention("e2", "1700000002.000100", "and again");
    reply["payload"]["event"]["thread_ts"] = json!("1700000001.000100");
    socket.send(reply).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e2"}));
    model.wait_for_requests(3).await;
    let request = model.requests()[2].body["messages"].to_string();
    assert!(
        request.contains("run it") && request.contains("first answer"),
        "{request}"
    );
    zx_testing::signal(&second, "-TERM").await;
    let output = program::ended_output(second).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}
