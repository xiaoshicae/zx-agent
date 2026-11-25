//! The `acp` subcommand against the local fake model, driven by a hand-written ACP client:
//! JSON-RPC over the process's stdio, one message per line, so the wire format is what a real
//! client sees. No paid APIs.

mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use zx_testing::fake_mcp::{Era, FakeMcp, Reply as McpReply, tool};
use zx_testing::mock_server::{MockServer, Reply, text_chunk};
use zx_testing::program::{Home, fake_model_config, seed_session, session_dir, write_agent, write_config};
use zx_testing::{stderr, summary_json};

use self::common::zx_agent_in;

const KEY_VAR: &str = "ZX_ACP_TEST_KEY";

/// A client on the other end of the agent's stdio.
struct Acp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    /// Messages read while waiting for something else, in arrival order.
    inbox: VecDeque<Value>,
    /// The agent's home directory, where its sessions go: its own, unless a test shares one
    /// between two processes.
    _home: Option<tempfile::TempDir>,
}

/// The session files in `dir`, by name.
fn session_files(dir: &Path) -> Vec<String> {
    zx_testing::program::session_files(dir)
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// The text of every message in a model request, in order; tool calls have none.
fn texts(request: &Value) -> Vec<&str> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect()
}

impl Acp {
    async fn start(server: &MockServer, cwd: &Path, args: &[&str]) -> Self {
        Self::start_in(server, cwd, args, None).await
    }

    /// Start the agent with `home` as its home directory, or one of its own.
    async fn start_in(server: &MockServer, cwd: &Path, args: &[&str], home: Option<&Path>) -> Self {
        Self::start_configured(server, cwd, args, home, "idleTimeoutSecs = 10").await
    }

    /// [`Acp::start_in`], with `rest` after the provider keys of its config.toml.
    async fn start_configured(server: &MockServer, cwd: &Path, args: &[&str], home: Option<&Path>, rest: &str) -> Self {
        let own = match home {
            Some(_) => None,
            None => Some(tempfile::tempdir().unwrap()),
        };
        let home = home.map_or_else(|| own.as_ref().unwrap().path().to_path_buf(), Path::to_path_buf);
        write_config(&home.join(".zx-agent"), &fake_model_config(KEY_VAR, "", rest));
        let mut child = zx_agent_in(cwd, Home::User(&home))
            .arg("acp")
            .args(["--model", "mock-model"])
            .args(args)
            .env("ZX_BASE_URL", &server.base_url)
            .env(KEY_VAR, "sk-acp-test-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
            inbox: VecDeque::new(),
            _home: own,
        }
    }

    async fn write(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    /// Every line on stdout must be a protocol message.
    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(8), self.lines.next_line())
            .await
            .expect("no message within 8 seconds")
            .unwrap()
            .expect("the agent closed stdout");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("not a JSON-RPC message ({error}): {line}"))
    }

    /// The next message, inbox first.
    async fn next(&mut self) -> Value {
        match self.inbox.pop_front() {
            Some(message) => message,
            None => self.read().await,
        }
    }

    /// Send a request and wait for its response, keeping everything else for `next`.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(id).await
    }

    /// The response to request `id`, from the inbox if it arrived while waiting for something
    /// else; everything read before it is kept for `next`.
    async fn response(&mut self, id: u64) -> Value {
        let answers = |message: &Value| message.get("method").is_none() && message["id"] == json!(id);
        if let Some(at) = self.inbox.iter().position(answers) {
            return self.inbox.remove(at).unwrap();
        }
        loop {
            let message = self.read().await;
            if answers(&message) {
                return message;
            }
            self.inbox.push_back(message);
        }
    }

    /// Close `session_id`; the response.
    async fn close(&mut self, session_id: &str) -> Value {
        self.request("session/close", json!({"sessionId": session_id})).await
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn respond(&mut self, id: &Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
    }

    /// Initialize as a client that presents notices and compactions, as Zed does; the
    /// response, with the agent's capabilities.
    async fn initialize(&mut self) -> Value {
        self.initialize_as(json!({"session": {"notices": {}, "compaction": {}}}))
            .await
    }

    /// Initialize as a client with `capabilities`; the response.
    async fn initialize_as(&mut self, capabilities: Value) -> Value {
        let initialized = self
            .request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
            )
            .await;
        assert_eq!(initialized["result"]["agentInfo"]["name"], "zx-agent", "{initialized}");
        initialized
    }

    /// Initialize and create a session in `cwd`; the session ID.
    async fn session(&mut self, cwd: &Path) -> String {
        self.initialize().await;
        self.create(cwd).await
    }

    /// Create a session in `cwd`; its ID. The agent then offers the session's commands.
    async fn create(&mut self, cwd: &Path) -> String {
        let created = self.request("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
        let id = created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{created}"))
            .to_string();
        self.expect_commands(&id).await;
        id
    }

    /// Load `session_id` in `cwd`; the response. The replayed updates wait in the inbox; the
    /// commands offered after a successful load do not.
    async fn load(&mut self, session_id: &str, cwd: &Path) -> Value {
        let loaded = self
            .request(
                "session/load",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        if loaded.get("error").is_none() {
            self.expect_commands(session_id).await;
        }
        loaded
    }

    /// Resume `session_id` in `cwd`, without a replay; the response.
    async fn resume(&mut self, session_id: &str, cwd: &Path) -> Value {
        let resumed = self
            .request(
                "session/resume",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        if resumed.get("error").is_none() {
            self.expect_commands(session_id).await;
        }
        resumed
    }

    /// The message right after a response that opened `session_id`: the commands the agent
    /// offers for it, `/name` with its input and `/compact` without one.
    async fn expect_commands(&mut self, session_id: &str) {
        let message = self.read().await;
        assert_eq!(message["method"], "session/update", "{message}");
        assert_eq!(message["params"]["sessionId"], session_id, "{message}");
        let update = &message["params"]["update"];
        assert_eq!(update["sessionUpdate"], "available_commands_update", "{message}");
        let commands = update["availableCommands"].as_array().unwrap();
        assert_eq!(commands.len(), 2, "{message}");
        assert_eq!(commands[0]["name"], "name", "{message}");
        assert!(commands[0]["description"].is_string(), "{message}");
        assert!(commands[0]["input"]["hint"].is_string(), "{message}");
        assert_eq!(commands[1]["name"], "compact", "{message}");
        assert!(commands[1]["description"].is_string(), "{message}");
        assert!(commands[1]["input"].is_null(), "{message}");
    }

    /// List the sessions with `params`; the response.
    async fn list(&mut self, params: Value) -> Value {
        self.request("session/list", params).await
    }

    /// The updates that arrived before the last response, in order.
    fn replayed(&mut self) -> Vec<Value> {
        self.inbox
            .drain(..)
            .map(|message| {
                assert_eq!(message["method"], "session/update", "{message}");
                message["params"]["update"].clone()
            })
            .collect()
    }

    /// Forget the updates of the turns so far.
    fn discard_updates(&mut self) {
        self.inbox.clear();
    }

    /// The next `session/update`, skipping nothing: tests assert the order of updates.
    async fn next_update(&mut self) -> Value {
        let message = self.next().await;
        assert_eq!(message["method"], "session/update", "{message}");
        message["params"]["update"].clone()
    }

    /// The next message that is a request from the agent: its ID and params.
    async fn next_request(&mut self, method: &str) -> (Value, Value) {
        let message = self.next().await;
        assert_eq!(message["method"], method, "{message}");
        assert!(message.get("id").is_some(), "{message}");
        (message["id"].clone(), message["params"].clone())
    }

    /// Updates until the response to the prompt, then the response.
    async fn prompt(&mut self, session_id: &str, text: &str) -> Value {
        self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }

    async fn finish(mut self) -> ExitStatus {
        drop(self.stdin);
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("the agent did not exit after stdin closed")
            .unwrap()
    }
}

fn text_of(update: &Value) -> &str {
    update["content"]["text"].as_str().unwrap_or_else(|| panic!("{update}"))
}

#[tokio::test]
async fn a_prompt_streams_the_answer_and_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    let response = acp.prompt(&session, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let update = acp.next_update().await;
    assert_eq!(update["sessionUpdate"], "agent_message_chunk", "{update}");
    assert_eq!(text_of(&update), "hi there");
    assert!(acp.inbox.is_empty(), "{:?}", acp.inbox);
    let requests = server.requests();
    let users: Vec<&Value> = requests[0].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "hello");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_shell_call_asks_the_client_and_runs_once_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c1", "printf approved > marker"), Reply::text("done")]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", "bash"]).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(99, &session, "go")).await;

    let call = acp.next_update().await;
    assert_eq!(call["sessionUpdate"], "tool_call", "{call}");
    assert_eq!(call["toolCallId"], "c1");
    assert_eq!(call["title"], "bash: printf approved > marker");
    assert_eq!(call["kind"], "execute");
    // The default status, pending, is left out on the wire.
    assert!(call.get("status").is_none(), "{call}");
    assert_eq!(call["rawInput"]["command"], "printf approved > marker");

    let (id, params) = acp.next_request("session/request_permission").await;
    assert_eq!(params["sessionId"], session);
    assert_eq!(params["toolCall"]["toolCallId"], "c1");
    assert_eq!(params["toolCall"]["title"], "bash: printf approved > marker");
    // Why it asks, the auto-approval it missed included, until the result takes its place.
    assert_eq!(
        params["toolCall"]["content"][0]["content"]["text"],
        "the agent requires tool approval; not auto-approved: it contains '>', which can change what the shell runs"
    );
    let options: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["allow-once", "allow-session", "reject-once"]);
    assert_eq!(params["options"][0]["kind"], "allow_once");
    assert!(!dir.path().join("marker").exists(), "nothing runs before the answer");
    acp.respond(
        &id,
        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
    )
    .await;

    let running = acp.next_update().await;
    assert_eq!(running["sessionUpdate"], "tool_call_update", "{running}");
    assert_eq!(running["status"], "in_progress");
    let finished = acp.next_update().await;
    assert_eq!(finished["sessionUpdate"], "tool_call_update", "{finished}");
    assert_eq!(finished["toolCallId"], "c1");
    assert_eq!(finished["status"], "completed");
    assert!(
        finished["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("[exit code: 0]"),
        "{finished}"
    );
    let answer = acp.next_update().await;
    assert_eq!(text_of(&answer), "done");
    let response = acp.read().await;
    assert_eq!(response["id"], 99);
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(std::fs::read_to_string(dir.path().join("marker")).unwrap(), "approved");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_rejected_call_never_runs_and_the_model_hears_why() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::bash("c1", "printf no > marker"), Reply::text("ok")]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", "bash"]).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(7, &session, "go")).await;
    let call = acp.next_update().await;
    assert_eq!(call["sessionUpdate"], "tool_call", "{call}");
    let (id, _) = acp.next_request("session/request_permission").await;
    acp.respond(
        &id,
        json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}}),
    )
    .await;
    let failed = acp.next_update().await;
    assert_eq!(failed["sessionUpdate"], "tool_call_update", "{failed}");
    assert_eq!(failed["status"], "failed");
    let answer = acp.next_update().await;
    assert_eq!(text_of(&answer), "ok");
    let response = acp.read().await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(!dir.path().join("marker").exists());
    assert!(
        server.requests()[1].body["messages"]
            .to_string()
            .contains("rejected by the user")
    );
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn cancel_ends_the_turn_as_cancelled_and_the_session_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![stalled_answer(), Reply::text("again")]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(5, &session, "first")).await;
    let chunk = acp.next_update().await;
    assert_eq!(text_of(&chunk), "partial");
    acp.notify("session/cancel", json!({"sessionId": session})).await;
    let response = acp.read().await;
    assert_eq!(response["id"], 5);
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");

    let response = acp.prompt(&session, "second").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(text_of(&acp.next_update().await), "again");
    assert!(acp.finish().await.success());
}

/// A model answer that begins and then never ends: a turn that is not cancelled does not end.
fn stalled_answer() -> Reply {
    Reply::Stall(vec![text_chunk("partial")])
}

/// The request that starts a turn of `session` with `text`, as request `id`.
fn prompt_request(id: u64, session: &str, text: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]}})
}

#[tokio::test]
async fn a_cancel_right_behind_its_prompt_ends_the_turn_as_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![stalled_answer()]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    let prompt = prompt_request(5, &session, "first");
    let cancel = json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}});
    // One write, so that the agent reads the cancellation right behind the prompt, before
    // the turn has made any progress.
    acp.stdin
        .write_all(format!("{prompt}\n{cancel}\n").as_bytes())
        .await
        .unwrap();

    let response = acp.response(5).await;

    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert!(acp.finish().await.success());
}

/// How the agent exited once it was sent SIGTERM.
#[cfg(unix)]
async fn terminated(acp: &mut Acp) -> ExitStatus {
    zx_testing::signal(&acp.child, "-TERM").await;
    tokio::time::timeout(Duration::from_secs(5), acp.child.wait())
        .await
        .expect("the agent did not exit after SIGTERM")
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_ends_an_idle_agent() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(Vec::new()).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    acp.session(dir.path()).await;

    let status = terminated(&mut acp).await;

    // An exit code, not a death by the signal: the agent stopped by itself.
    assert_eq!(status.code(), Some(0), "{status:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_cancels_the_turn_in_progress_before_the_agent_exits() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![stalled_answer()]).await;
    let mut acp = Acp::start_in(&server, &cwd, &["--tools", ""], Some(home.path())).await;
    let session = acp.session(&cwd).await;
    acp.write(prompt_request(5, &session, "first")).await;
    assert_eq!(text_of(&acp.next_update().await), "partial");

    let status = terminated(&mut acp).await;

    assert_eq!(status.code(), Some(0), "{status:?}");
    // Cancelled rather than dropped: the session holds the answer as far as it got, marked
    // as aborted, which only a run that is read to its end records.
    let files = zx_testing::program::session_files(&session_dir(home.path(), &cwd));
    assert_eq!(files.len(), 1, "{files:?}");
    let aborted: Vec<Value> = zx_testing::read_lines(&files[0])
        .into_iter()
        .filter(|line| line["message"]["stopReason"] == "aborted")
        .collect();
    assert_eq!(aborted.len(), 1, "{:?}", zx_testing::read_lines(&files[0]));
    assert!(aborted[0]["message"].to_string().contains("partial"), "{aborted:?}");
}

#[tokio::test]
async fn an_unknown_session_is_a_request_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    acp.session(dir.path()).await;
    let response = acp.prompt("nope", "hello").await;
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown session")),
        "{response}"
    );
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_file_write_shows_as_a_diff_and_an_auto_approved_command_asks_nobody() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("w", "write_file", &json!({"path": "notes.txt", "content": "one\n"})),
        Reply::bash("p", "pwd"),
        Reply::text("written"),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    write_agent(
        &home.path().join(".zx-agent"),
        "assistant",
        "[permissions]\nallow = [\"bash(pwd)\"]",
    );
    let mut acp = Acp::start_in(&server, dir.path(), &[], Some(home.path())).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(3, &session, "write")).await;

    let call = acp.next_update().await;
    assert_eq!(call["kind"], "edit", "{call}");
    assert_eq!(call["title"], "write_file: notes.txt");
    let expected_path = dir.path().canonicalize().unwrap().join("notes.txt");
    assert_eq!(call["locations"][0]["path"], json!(expected_path), "{call}");
    let (id, _) = acp.next_request("session/request_permission").await;
    acp.respond(
        &id,
        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
    )
    .await;
    assert_eq!(acp.next_update().await["status"], "in_progress");
    let done = acp.next_update().await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["content"][0]["type"], "diff", "{done}");
    assert_eq!(done["content"][0]["path"], json!(expected_path));
    assert_eq!(done["content"][0]["newText"], "one\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(), "one\n");

    // pwd is auto-approved: the call goes straight to in_progress without a question, in the
    // session's directory rather than the agent's.
    let pwd = acp.next_update().await;
    assert_eq!(pwd["sessionUpdate"], "tool_call", "{pwd}");
    assert_eq!(pwd["title"], "bash: pwd");
    assert_eq!(acp.next_update().await["status"], "in_progress");
    let result = acp.next_update().await;
    assert_eq!(result["status"], "completed", "{result}");
    assert!(
        result["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains(dir.path().canonicalize().unwrap().to_str().unwrap()),
        "{result}"
    );
    assert_eq!(text_of(&acp.next_update().await), "written");
    assert_eq!(acp.read().await["result"]["stopReason"], "end_turn");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_call_on_a_directory_has_no_location_for_the_client_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("l", "ls", &json!({"path": "."})),
        Reply::text("listed"),
    ])
    .await;
    let mut acp = Acp::start_in(&server, dir.path(), &[], None).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(3, &session, "list")).await;

    let call = acp.next_update().await;
    assert_eq!(call["sessionUpdate"], "tool_call", "{call}");
    assert_eq!(call["kind"], "read", "{call}");
    assert!(call["locations"].as_array().is_none_or(Vec::is_empty), "{call}");
    assert_eq!(acp.next_update().await["status"], "in_progress");
    assert_eq!(acp.next_update().await["status"], "completed");
    assert_eq!(text_of(&acp.next_update().await), "listed");
    assert_eq!(acp.read().await["result"]["stopReason"], "end_turn");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn an_edit_shows_as_the_diff_of_each_changed_region() {
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    std::fs::write(cwd.join("plan.md"), "# Plan\n- ship on monday\n").unwrap();
    let edits = json!({"path": "plan.md", "edits": [{"oldText": "monday", "newText": "friday"}]});
    let server = MockServer::start(vec![Reply::tool_call("e", "edit_file", &edits), Reply::text("moved")]).await;
    let mut acp = Acp::start(&server, &cwd, &["--dangerously-skip-approvals"]).await;
    let session = acp.session(&cwd).await;

    let response = acp.prompt(&session, "move it").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = acp.replayed();
    let ended = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed")
        .unwrap_or_else(|| panic!("{updates:?}"));
    let content = ended["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "diff", "{ended}");
    assert_eq!(content[0]["path"], json!(cwd.join("plan.md")));
    assert_eq!(content[0]["oldText"], "# Plan\n- ship on monday\n");
    assert_eq!(content[0]["newText"], "# Plan\n- ship on friday\n");
    assert_eq!(
        std::fs::read_to_string(cwd.join("plan.md")).unwrap(),
        "# Plan\n- ship on friday\n"
    );
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_write_over_a_file_shows_as_the_diff_of_what_changed() {
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    std::fs::write(cwd.join("plan.md"), "# Plan\n- ship on monday\n").unwrap();
    let write = json!({"path": "plan.md", "content": "# Plan\n- ship on friday\n"});
    let server = MockServer::start(vec![Reply::tool_call("w", "write_file", &write), Reply::text("moved")]).await;
    let mut acp = Acp::start(&server, &cwd, &["--dangerously-skip-approvals"]).await;
    let session = acp.session(&cwd).await;

    let response = acp.prompt(&session, "move it").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = acp.replayed();
    let ended = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed")
        .unwrap_or_else(|| panic!("{updates:?}"));
    let content = ended["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "diff", "{ended}");
    assert_eq!(content[0]["oldText"], "# Plan\n- ship on monday\n");
    assert_eq!(content[0]["newText"], "# Plan\n- ship on friday\n");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_tool_shows_its_progress_in_its_call_until_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let mcp = FakeMcp::start(Era::Modern, vec![tool("query", "Instant query.")]).await;
    mcp.reply(
        "query",
        vec![McpReply::Progress {
            steps: vec![json!({"progress": 1, "total": 2, "message": "half the series read"})],
            every: Duration::from_millis(10),
            result: json!({"content": [{"type": "text", "text": "up"}]}),
        }],
    );
    let server = MockServer::start(vec![
        Reply::tool_call("q", "prom__query", &json!({})),
        Reply::text("all up"),
    ])
    .await;
    let config = format!(
        "idleTimeoutSecs = 10\n[[mcp]]\nname = \"prom\"\nurl = \"{}\"\ntools = [\"query\"]",
        mcp.url
    );
    let mut acp = Acp::start_configured(&server, dir.path(), &["--dangerously-skip-approvals"], None, &config).await;
    let session = acp.session(dir.path()).await;

    let response = acp.prompt(&session, "check").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = acp.replayed();
    let calls: Vec<&Value> = updates.iter().filter(|update| update["toolCallId"] == "q").collect();
    let progress = calls
        .iter()
        .position(|update| update["content"][0]["content"]["text"] == "half the series read")
        .unwrap_or_else(|| panic!("{calls:?}"));
    let ended = calls
        .iter()
        .position(|update| update["status"] == "completed")
        .unwrap_or_else(|| panic!("{calls:?}"));
    assert!(progress < ended, "{calls:?}");
    assert_eq!(calls[ended]["content"][0]["content"]["text"], "up", "{calls:?}");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn a_session_is_kept_in_the_directory_of_its_cwd_and_loaded_with_its_history() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.finish().await;
    // One file per session, named after it, in the directory chat uses for this cwd.
    let files = session_files(&session_dir(home.path(), &cwd));
    assert!(
        files.len() == 1 && files[0].ends_with(&format!("_{id}.jsonl")),
        "{files:?}"
    );

    // A later process loads it: the conversation is replayed before the response, and the
    // next turn goes on from there, the model seeing the earlier one.
    let server = MockServer::start(vec![Reply::text("still here")]).await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let initialized = second.initialize().await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["loadSession"], true,
        "{initialized}"
    );
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay: Vec<(String, String)> = second
        .replayed()
        .into_iter()
        .map(|update| {
            (
                update["sessionUpdate"].as_str().unwrap().to_string(),
                text_of(&update).to_string(),
            )
        })
        .collect();
    assert_eq!(
        replay,
        [
            ("user_message_chunk".to_string(), "hello".to_string()),
            ("agent_message_chunk".to_string(), "hi there".to_string()),
        ]
    );
    let response = second.prompt(&id, "and now?").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let requests = server.requests();
    let texts = texts(&requests[0].body);
    assert!(texts.ends_with(&["hello", "hi there", "and now?"]), "{texts:?}");
    second.finish().await;
}

#[tokio::test]
async fn loading_replays_a_tool_call_with_its_result() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    std::fs::write(cwd.join("notes.txt"), "deploys on tuesdays\n").unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("c1", "read_file", &json!({"path": "notes.txt"})),
        Reply::text("noted"),
    ])
    .await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first.prompt(&id, "read it").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.finish().await;

    let server = MockServer::start(Vec::new()).await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay = second.replayed();
    assert_eq!(replay.len(), 3, "{replay:?}");
    assert_eq!(replay[0]["sessionUpdate"], "user_message_chunk");
    assert_eq!(text_of(&replay[0]), "read it");
    // The call comes as one update, with its result: the client never saw it run.
    let call = &replay[1];
    assert_eq!(call["sessionUpdate"], "tool_call", "{call}");
    assert_eq!(call["toolCallId"], "c1");
    assert_eq!(call["status"], "completed");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["rawInput"]["path"], "notes.txt");
    assert_eq!(
        call["locations"][0]["path"],
        cwd.join("notes.txt").display().to_string()
    );
    // Output as a code block in the file's language, which a client renders as it is.
    let shown = call["content"][0]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{call}"));
    assert!(shown.starts_with("```txt\ndeploys on tuesdays\n"), "{shown}");
    assert!(shown.ends_with("\n```"), "{shown}");
    assert_eq!(replay[2]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(text_of(&replay[2]), "noted");
    second.finish().await;
}

#[tokio::test]
async fn loading_an_unknown_session_is_a_request_error() {
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(Vec::new()).await;
    let mut acp = Acp::start(&server, &cwd, &[]).await;
    acp.initialize().await;
    let loaded = acp.load("no-such-session", &cwd).await;
    assert_eq!(loaded["error"]["code"], -32602, "{loaded}");
    let message = loaded["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("unknown session no-such-session: no such session in "),
        "{message}"
    );
    acp.finish().await;
}

#[tokio::test]
async fn a_session_started_in_another_directory_is_neither_loaded_nor_resumed() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let other = tempfile::tempdir().unwrap();
    let elsewhere = other.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.finish().await;

    let server = MockServer::start(Vec::new()).await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let unknown = format!(
        "unknown session {id}: no such session in {}",
        session_dir(home.path(), &elsewhere).display()
    );
    // Asked twice: a refused session is not left open, where the next request would find it
    // whatever directory it names.
    for _ in 0..2 {
        for refused in [second.load(&id, &elsewhere).await, second.resume(&id, &elsewhere).await] {
            assert_eq!(refused["error"]["code"], -32602, "{refused}");
            assert_eq!(refused["error"]["message"], unknown.as_str(), "{refused}");
        }
    }
    assert!(second.replayed().is_empty(), "nothing of it is replayed");
    // In the directory it started in, it is loaded: the refusals left its file free.
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay: Vec<String> = second
        .replayed()
        .iter()
        .map(|update| text_of(update).to_string())
        .collect();
    assert_eq!(replay, ["hello", "hi there"]);
    second.finish().await;
}

#[tokio::test]
async fn a_live_session_is_neither_loaded_nor_resumed_for_another_directory() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let other = tempfile::tempdir().unwrap();
    let elsewhere = other.path().canonicalize().unwrap();
    let server = MockServer::start(Vec::new()).await;
    let mut acp = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = acp.session(&cwd).await;
    let unknown = format!(
        "unknown session {id}: no such session in {}",
        session_dir(home.path(), &elsewhere).display()
    );
    // Open in this process or not, a session belongs to the directory it works in.
    for refused in [acp.load(&id, &elsewhere).await, acp.resume(&id, &elsewhere).await] {
        assert_eq!(refused["error"]["code"], -32602, "{refused}");
        assert_eq!(refused["error"]["message"], unknown.as_str(), "{refused}");
    }
    acp.finish().await;
}

#[tokio::test]
async fn no_session_keeps_no_file_and_offers_no_loading() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("gone soon")]).await;
    let mut acp = Acp::start_in(&server, &cwd, &["--no-session"], Some(home.path())).await;
    let initialized = acp.initialize().await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["loadSession"], false,
        "{initialized}"
    );
    let capabilities = &initialized["result"]["agentCapabilities"]["sessionCapabilities"];
    assert!(
        capabilities.get("list").is_none_or(Value::is_null) && capabilities.get("resume").is_none_or(Value::is_null),
        "{initialized}"
    );
    // Closing is offered all the same: it frees a session in memory too.
    assert_eq!(capabilities["close"], json!({}), "{initialized}");
    let id = acp.create(&cwd).await;
    let response = acp.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        !home.path().join(".zx-agent/sessions").exists(),
        "no session file is written"
    );
    let loaded = acp.load("another", &cwd).await;
    assert_eq!(loaded["error"]["code"], -32602, "{loaded}");
    assert!(
        loaded["error"]["message"].as_str().unwrap().contains("--no-session"),
        "{loaded}"
    );
    let listed = acp.list(json!({"cwd": cwd})).await;
    assert_eq!(listed["result"]["sessions"], json!([]), "{listed}");
    acp.finish().await;
}

#[tokio::test]
async fn a_live_session_is_replayed_in_place() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("two"), Reply::text("four")]).await;
    let mut acp = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = acp.session(&cwd).await;
    let response = acp.prompt(&id, "one").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let files = session_files(&session_dir(home.path(), &cwd));
    assert!(
        files.len() == 1 && files[0].ends_with(&format!("_{id}.jsonl")),
        "{files:?}"
    );
    // A session this process holds is replayed as it is; its file stays open once.
    acp.discard_updates();
    let loaded = acp.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replayed = acp.replayed();
    let kinds: Vec<&str> = replayed
        .iter()
        .map(|update| update["sessionUpdate"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["user_message_chunk", "agent_message_chunk"], "{replayed:?}");
    assert_eq!(text_of(&replayed[1]), "two");
    let response = acp.prompt(&id, "three").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let requests = server.requests();
    let texts = texts(&requests[1].body);
    assert!(texts.ends_with(&["one", "two", "three"]), "{texts:?}");
    acp.finish().await;
}

#[tokio::test]
async fn a_single_session_file_is_refused_before_the_connection() {
    let work = tempfile::tempdir().unwrap();
    let output = zx_agent_in(work.path(), Home::User(work.path()))
        .args(["acp", "--model", "m", "--session"])
        .arg(work.path().join("one.jsonl"))
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let errors = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{errors}");
    assert!(errors.contains("unexpected argument '--session'"), "{errors}");
}

#[tokio::test]
async fn sessions_are_listed_most_recent_first_with_their_first_prompt() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    // Session files that say when they were last written: the order of the listing does not
    // depend on when the test runs. By their IDs alone the order would be the other one.
    let dir = session_dir(home.path(), &cwd);
    seed_session(&dir, "aaaa0001", &cwd, 1_000, "first question");
    seed_session(&dir, "bbbb0002", &cwd, 2_000, "second question");
    let server = MockServer::start(Vec::new()).await;
    let mut acp = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let initialized = acp.initialize().await;
    let capabilities = &initialized["result"]["agentCapabilities"]["sessionCapabilities"];
    assert_eq!(
        (capabilities["list"].clone(), capabilities["resume"].clone()),
        (json!({}), json!({})),
        "{initialized}"
    );

    let listed = acp.list(json!({"cwd": cwd})).await;
    let sessions = listed["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"));
    let listed_ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(listed_ids, ["bbbb0002", "aaaa0001"], "{listed}");
    assert_eq!(sessions[0]["title"], "second question");
    assert_eq!(sessions[1]["title"], "first question");
    assert_eq!(sessions[0]["cwd"], cwd.display().to_string());
    assert_eq!(sessions[0]["updatedAt"], "1970-01-01T00:00:02.000Z", "{listed}");
    assert!(listed["result"]["nextCursor"].is_null(), "{listed}");
    // Without a directory the agent's own is listed; another directory has none.
    let all = acp.list(json!({})).await;
    assert_eq!(all["result"]["sessions"].as_array().unwrap().len(), 2, "{all}");
    let none = acp.list(json!({"cwd": elsewhere.path().canonicalize().unwrap()})).await;
    assert_eq!(none["result"]["sessions"], json!([]), "{none}");
    // A listing is one page: no cursor of ours exists.
    let paged = acp.list(json!({"cwd": cwd, "cursor": "page-2"})).await;
    assert_eq!(paged["error"]["code"], -32602, "{paged}");
    acp.finish().await;
}

#[tokio::test]
async fn a_prompt_without_text_is_refused_before_any_request() {
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("unused")]).await;
    let mut acp = Acp::start(&server, &cwd, &[]).await;
    let id = acp.session(&cwd).await;

    let response = acp.prompt(&id, " \n ").await;

    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(server.requests().is_empty());
    acp.finish().await;
}

#[tokio::test]
async fn a_listing_names_the_session_files_it_cannot_read_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let dir = session_dir(home.path(), &cwd);
    seed_session(&dir, "aaaa0001", &cwd, 1_000, "first question");
    std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();
    write_config(&home.path().join(".zx-agent"), &fake_model_config(KEY_VAR, "", ""));
    // The harness passes stderr through, so this agent is driven by hand.
    let mut child = zx_agent_in(&cwd, Home::User(home.path()))
        .args(["acp", "--model", "mock-model"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // Each request waits for its answer: the protocol takes no other request before
    // `initialize` is answered.
    let mut ask = async |id: u64, method: &str, params: Value| -> Value {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        stdin.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(8), lines.next_line())
                .await
                .expect("no message within 8 seconds")
                .unwrap()
                .expect("the agent closed stdout");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message;
            }
        }
    };
    ask(1, "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let listed = ask(2, "session/list", json!({"cwd": cwd})).await;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("the agent did not exit after stdin closed")
        .unwrap();

    let sessions = listed["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(sessions.len(), 1, "{listed}");
    let errors = stderr(&output);
    assert!(errors.contains("[acp]") && errors.contains("broken.jsonl"), "{errors}");
}

#[tokio::test]
async fn resume_continues_a_session_without_replaying_it() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.finish().await;

    let server = MockServer::start(vec![Reply::text("still here")]).await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let resumed = second.resume(&id, &cwd).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    assert!(second.replayed().is_empty(), "nothing is replayed");
    let response = second.prompt(&id, "and now?").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let requests = server.requests();
    let texts = texts(&requests[0].body);
    assert!(texts.ends_with(&["hello", "hi there", "and now?"]), "{texts:?}");
    // Resuming a session this process holds changes nothing; an unknown one is refused.
    let again = second.resume(&id, &cwd).await;
    assert!(again.get("error").is_none(), "{again}");
    let unknown = second.resume("no-such-session", &cwd).await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    second.finish().await;
}

#[tokio::test]
async fn the_name_command_names_the_session_without_a_model_request() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.discard_updates();

    let response = first.prompt(&id, "/name  排查 nginx 502 ").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let update = first.next_update().await;
    assert_eq!(
        update,
        json!({"sessionUpdate": "session_info_update", "title": "排查 nginx 502"})
    );
    assert!(first.inbox.is_empty(), "{:?}", first.inbox);
    assert_eq!(server.requests().len(), 1, "the command is no prompt");
    // Listed by its name instead of its first prompt, also by a later process: the name is
    // in the session file.
    let listed = first.list(json!({"cwd": cwd})).await;
    assert_eq!(listed["result"]["sessions"][0]["title"], "排查 nginx 502", "{listed}");
    first.finish().await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let listed = second.list(json!({"cwd": cwd})).await;
    assert_eq!(listed["result"]["sessions"][0]["title"], "排查 nginx 502", "{listed}");
    second.finish().await;
}

#[tokio::test]
async fn the_name_command_needs_a_name_and_other_slashes_reach_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let mut acp = Acp::start(&server, dir.path(), &[]).await;
    let session = acp.session(dir.path()).await;

    for without in ["/name", "/name \t "] {
        let response = acp.prompt(&session, without).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert_eq!(
            response["error"]["message"], "give the session a name: /name <name>",
            "{response}"
        );
    }
    assert!(acp.inbox.is_empty(), "no title changed: {:?}", acp.inbox);
    let response = acp.prompt(&session, "/named things").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(texts(&requests[0].body).last(), Some(&"/named things"));
    acp.finish().await;
}

/// The ID of the record that holds the prompt `text`, in the one session file of `dir`.
fn prompt_record(dir: &Path, text: &str) -> String {
    let file = &zx_testing::program::session_files(dir)[0];
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["message"]["role"] == "user" && record["message"]["content"] == text)
        .map(|record| record["id"].as_str().unwrap().to_string())
        .unwrap()
}

#[tokio::test]
async fn the_compact_command_summarizes_and_a_load_still_replays_every_turn() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    // Everything before the latest prompt is old enough to summarize.
    write_agent(
        &home.path().join(".zx-agent"),
        "assistant",
        "[compaction]\nkeepRecentBytes = 1",
    );
    let server = MockServer::start(vec![Reply::text("first answer"), Reply::text("second answer")]).await;
    let mut first = Acp::start_in(&server, &cwd, &["--tools", ""], Some(home.path())).await;
    let id = first.session(&cwd).await;
    for prompt in ["one", "two"] {
        let response = first.prompt(&id, prompt).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    }
    first.finish().await;
    // The summary cites a record of the session, as the model's does.
    let cited = prompt_record(&session_dir(home.path(), &cwd), "one");
    let server = MockServer::start(vec![Reply::text(&summary_json(&cited))]).await;
    let mut second = Acp::start_in(&server, &cwd, &["--tools", ""], Some(home.path())).await;
    second.initialize().await;
    let resumed = second.resume(&id, &cwd).await;
    assert!(resumed.get("error").is_none(), "{resumed}");

    let response = second.prompt(&id, "/compact").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(server.requests().len(), 1, "the summary is the only request");
    let shown: Vec<(String, String)> = second
        .replayed()
        .iter()
        .map(|update| {
            (
                update["sessionUpdate"].as_str().unwrap().to_string(),
                update["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let compaction = |status: &str| ("compaction_update".to_string(), status.to_string());
    assert_eq!(shown, [compaction("in_progress"), compaction("completed")]);
    // The summary stands in for the first turn in what the model reads, not in what the
    // person is shown: a load replays every turn.
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay: Vec<String> = second
        .replayed()
        .iter()
        .map(|update| text_of(update).to_string())
        .collect();
    assert_eq!(replay, ["one", "first answer", "two", "second answer"]);
    second.finish().await;
}

#[tokio::test]
async fn the_compact_command_says_when_nothing_is_old_enough_to_summarize() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;

    let response = acp.prompt(&session, "/compact").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let update = acp.next_update().await;
    assert_eq!(update["sessionUpdate"], "notice", "{update}");
    assert!(acp.inbox.is_empty(), "{:?}", acp.inbox);
    assert!(server.requests().is_empty());
    acp.finish().await;
}

#[tokio::test]
async fn a_client_that_presents_no_notices_or_compactions_is_sent_none() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    write_agent(
        &home.path().join(".zx-agent"),
        "assistant",
        "[compaction]\nkeepRecentBytes = 1",
    );
    let server = MockServer::start(vec![Reply::text("first answer"), Reply::text("second answer")]).await;
    let mut first = Acp::start_in(&server, &cwd, &["--tools", ""], Some(home.path())).await;
    first.initialize_as(json!({})).await;
    let id = first.create(&cwd).await;

    // Nothing is old enough yet: the answer to the command is an agent message.
    let response = first.prompt(&id, "/compact").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = first.replayed();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["sessionUpdate"], "agent_message_chunk", "{updates:?}");
    assert_eq!(
        text_of(&updates[0]),
        "Nothing to summarize: no part of the conversation is old enough yet."
    );
    for prompt in ["one", "two"] {
        let response = first.prompt(&id, prompt).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    }
    first.finish().await;

    // A compaction that runs sends no compaction updates.
    let cited = prompt_record(&session_dir(home.path(), &cwd), "one");
    let server = MockServer::start(vec![Reply::text(&summary_json(&cited))]).await;
    let mut second = Acp::start_in(&server, &cwd, &["--tools", ""], Some(home.path())).await;
    second.initialize_as(json!({})).await;
    let resumed = second.resume(&id, &cwd).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let response = second.prompt(&id, "/compact").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(server.requests().len(), 1, "the summary ran");
    let updates = second.replayed();
    assert!(updates.is_empty(), "{updates:?}");
    second.finish().await;
}

#[tokio::test]
async fn closing_a_session_frees_its_file_for_another_process() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("hi there")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let initialized = first.initialize().await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["sessionCapabilities"]["close"],
        json!({}),
        "{initialized}"
    );
    let id = first.create(&cwd).await;
    let response = first.prompt(&id, "hello").await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    // While the first process holds the session, a second cannot open its file.
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let refused = second.load(&id, &cwd).await;
    let reason = refused["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{refused}"));
    assert!(reason.contains("is in use"), "{reason}");

    let closed = first.close(&id).await;

    assert_eq!(closed["result"], json!({}), "{closed}");
    let gone = first.prompt(&id, "still there?").await;
    assert_eq!(gone["error"]["code"], -32602, "{gone}");
    // The second process opens it now, with the conversation so far.
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay: Vec<String> = second
        .replayed()
        .iter()
        .map(|update| text_of(update).to_string())
        .collect();
    assert_eq!(replay, ["hello", "hi there"]);
    assert_eq!(server.requests().len(), 1, "closing makes no request");
    first.finish().await;
    second.finish().await;
}

#[tokio::test]
async fn closing_a_session_cancels_its_turn() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![stalled_answer()]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(50, &session, "first")).await;
    assert_eq!(text_of(&acp.next_update().await), "partial");

    let closed = acp.close(&session).await;

    assert_eq!(closed["result"], json!({}), "{closed}");
    // The turn's own response may come before or after the close's.
    let answer = acp.response(50).await;
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    let gone = acp.prompt(&session, "again").await;
    assert_eq!(gone["error"]["code"], -32602, "{gone}");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn closing_a_session_the_agent_does_not_hold_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(Vec::new()).await;
    let mut acp = Acp::start(&server, dir.path(), &[]).await;
    let session = acp.session(dir.path()).await;
    assert_eq!(acp.close(&session).await["result"], json!({}));

    // Closed already, or never opened: there is nothing to free.
    for id in [session.as_str(), "no-such-session"] {
        let closed = acp.close(id).await;
        assert_eq!(closed["result"], json!({}), "{closed}");
    }
    assert!(acp.inbox.is_empty(), "{:?}", acp.inbox);
    assert!(acp.finish().await.success());
}

/// A PNG of one transparent pixel, in base64.
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// The provider keys of the harness and a model that takes images.
const SEEING: &str = "idleTimeoutSecs = 10\n[[models]]\nid = \"mock-model\"\ninput = [\"text\", \"image\"]";

/// A prompt block of an image whose base64 is `data`.
fn image_block(data: &str) -> Value {
    json!({"type": "image", "data": data, "mimeType": "image/png"})
}

/// The content part an OpenAI-compatible request carries for [`ONE_PIXEL_PNG`].
fn image_part() -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{ONE_PIXEL_PNG}")}})
}

#[tokio::test]
async fn images_in_a_prompt_reach_a_model_that_takes_them() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("a dot"), Reply::text("the same dot")]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, SEEING).await;
    let initialized = acp.initialize().await;
    let prompts = &initialized["result"]["agentCapabilities"]["promptCapabilities"];
    assert_eq!(prompts["image"], true, "{initialized}");
    assert_ne!(prompts["audio"], true, "{initialized}");
    let session = acp.create(dir.path()).await;

    let with_text = acp
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "what is this?"}, image_block(ONE_PIXEL_PNG)]}),
        )
        .await;
    assert_eq!(with_text["result"]["stopReason"], "end_turn", "{with_text}");
    // An image alone is a prompt too.
    let alone = acp
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [image_block(ONE_PIXEL_PNG)]}),
        )
        .await;
    assert_eq!(alone["result"]["stopReason"], "end_turn", "{alone}");

    let requests = server.requests();
    let first = requests[0].body["messages"].as_array().unwrap();
    assert_eq!(
        first.last().unwrap()["content"],
        json!([image_part(), {"type": "text", "text": "what is this?"}])
    );
    let second = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(second.last().unwrap()["content"], json!([image_part()]));
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn an_image_that_cannot_be_used_is_refused_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("unused")]).await;
    let mut acp = Acp::start(&server, dir.path(), &[]).await;
    let session = acp.session(dir.path()).await;

    // Only the PNG's signature: there is no picture to decode.
    let broken = acp
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "look"}, image_block("iVBORw0KGgo=")]}),
        )
        .await;
    assert_eq!(broken["error"]["code"], -32602, "{broken}");
    let message = broken["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("image 1 of the prompt cannot be used: cannot decode the image"),
        "{message}"
    );
    let command = acp
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "/compact"}, image_block(ONE_PIXEL_PNG)]}),
        )
        .await;
    assert_eq!(command["error"]["code"], -32602, "{command}");
    assert_eq!(command["error"]["message"], "a command takes no images", "{command}");

    assert!(server.requests().is_empty());
    assert!(acp.inbox.is_empty(), "{:?}", acp.inbox);
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn loading_replays_the_images_of_a_prompt() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let server = MockServer::start(vec![Reply::text("a dot")]).await;
    let mut first = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    let id = first.session(&cwd).await;
    let response = first
        .request(
            "session/prompt",
            json!({"sessionId": id, "prompt": [{"type": "text", "text": "what is this?"}, image_block(ONE_PIXEL_PNG)]}),
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.finish().await;

    let server = MockServer::start(Vec::new()).await;
    let mut second = Acp::start_in(&server, &cwd, &[], Some(home.path())).await;
    second.initialize().await;
    let loaded = second.load(&id, &cwd).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay = second.replayed();

    assert_eq!(replay.len(), 3, "{replay:?}");
    assert_eq!(replay[0]["sessionUpdate"], "user_message_chunk");
    assert_eq!(text_of(&replay[0]), "what is this?");
    assert_eq!(replay[1]["sessionUpdate"], "user_message_chunk");
    assert_eq!(
        replay[1]["content"],
        json!({"type": "image", "data": ONE_PIXEL_PNG, "mimeType": "image/png"})
    );
    assert_eq!(text_of(&replay[2]), "a dot");
    second.finish().await;
}

#[tokio::test]
async fn an_image_a_tool_reads_shows_in_its_call() {
    use base64::Engine;

    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let png = base64::engine::general_purpose::STANDARD.decode(ONE_PIXEL_PNG).unwrap();
    std::fs::write(cwd.join("dot.png"), png).unwrap();
    let server = MockServer::start(vec![
        Reply::tool_call("c1", "read_file", &json!({"path": "dot.png"})),
        Reply::text("a dot"),
    ])
    .await;
    let mut acp = Acp::start_configured(&server, &cwd, &[], None, SEEING).await;
    let session = acp.session(&cwd).await;

    let response = acp.prompt(&session, "look at dot.png").await;

    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = acp.replayed();
    let ended = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed")
        .unwrap_or_else(|| panic!("{updates:?}"));
    let content = ended["content"].as_array().unwrap();
    assert_eq!(content[0]["content"]["text"], "Read image file [image/png]", "{ended}");
    assert_eq!(
        content[1]["content"],
        json!({"type": "image", "data": ONE_PIXEL_PNG, "mimeType": "image/png"}),
        "{ended}"
    );
    // The model saw the image, after the result's text, in a message of its own.
    let requests = server.requests();
    let sent = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(
        sent.last().unwrap()["content"],
        json!([{"type": "text", "text": "Attached image(s) from tool result:"}, image_part()])
    );
    assert!(acp.finish().await.success());
}

/// Two models, the second with a context window, after the provider keys of config.toml.
const TWO_MODELS: &str =
    "idleTimeoutSecs = 10\n[[models]]\nid = \"mock-model\"\n[[models]]\nid = \"next-model\"\ncontextWindow = 2000";

/// The reasoning levels the reasoning option offers.
const LEVELS: [&str; 7] = ["default", "off", "minimal", "low", "medium", "high", "xhigh"];

/// The ID, current value and choices of each configuration option in `options`.
fn options_of(options: &Value) -> Vec<(String, String, Vec<String>)> {
    let options = options.as_array().unwrap_or_else(|| panic!("no options: {options}"));
    options
        .iter()
        .map(|option| {
            assert_eq!(option["type"], "select", "{option}");
            let choices = option["options"].as_array().unwrap();
            (
                option["id"].as_str().unwrap().to_string(),
                option["currentValue"].as_str().unwrap().to_string(),
                choices
                    .iter()
                    .map(|choice| choice["value"].as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect()
}

impl Acp {
    /// Set option `id` of `session` to `value`; the response.
    async fn set_option(&mut self, session: &str, id: &str, value: &str) -> Value {
        self.request(
            "session/set_config_option",
            json!({"sessionId": session, "configId": id, "value": value}),
        )
        .await
    }
}

#[tokio::test]
async fn a_session_offers_the_model_and_the_reasoning_level_as_options() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, TWO_MODELS).await;
    acp.initialize().await;
    let created = acp
        .request("session/new", json!({"cwd": dir.path(), "mcpServers": []}))
        .await;
    let session = created["result"]["sessionId"].as_str().unwrap().to_string();
    acp.expect_commands(&session).await;

    let options = &created["result"]["configOptions"];
    assert_eq!(
        options_of(options),
        [
            (
                "model".to_string(),
                "mock-model".to_string(),
                vec!["mock-model".to_string(), "next-model".to_string()]
            ),
            (
                "reasoning".to_string(),
                "default".to_string(),
                LEVELS.map(str::to_string).to_vec()
            ),
        ]
    );
    assert_eq!(options[0]["category"], "model", "{options}");
    assert_eq!(options[1]["category"], "thought_level", "{options}");
    // A session opened again shows its options too.
    let resumed = acp.resume(&session, dir.path()).await;
    assert_eq!(options_of(&resumed["result"]["configOptions"]).len(), 2, "{resumed}");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn changing_an_option_changes_the_next_requests_and_keeps_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two"), Reply::text("three")]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, TWO_MODELS).await;
    let session = acp.session(dir.path()).await;
    acp.prompt(&session, "first").await;

    let changed = acp.set_option(&session, "model", "next-model").await;
    assert_eq!(
        options_of(&changed["result"]["configOptions"])[0].1,
        "next-model",
        "{changed}"
    );
    let changed = acp.set_option(&session, "reasoning", "high").await;
    assert_eq!(
        options_of(&changed["result"]["configOptions"])[1].1,
        "high",
        "{changed}"
    );
    acp.prompt(&session, "second").await;
    let changed = acp.set_option(&session, "reasoning", "default").await;
    assert_eq!(
        options_of(&changed["result"]["configOptions"])[1].1,
        "default",
        "{changed}"
    );
    acp.prompt(&session, "third").await;

    let requests = server.requests();
    let models: Vec<&Value> = requests.iter().map(|request| &request.body["model"]).collect();
    assert_eq!(
        models,
        [&json!("mock-model"), &json!("next-model"), &json!("next-model")]
    );
    let efforts: Vec<Value> = requests
        .iter()
        .map(|request| request.body.get("reasoning_effort").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(efforts, [Value::Null, json!("high"), Value::Null]);
    let users: Vec<&str> = texts(&requests[2].body)
        .into_iter()
        .filter(|text| ["first", "second", "third"].contains(text))
        .collect();
    assert_eq!(users, ["first", "second", "third"]);
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn an_option_outside_its_choices_is_refused_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("done")]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, TWO_MODELS).await;
    let session = acp.session(dir.path()).await;
    for (id, value, says) in [
        ("model", "other-model", "unknown model \"other-model\""),
        ("reasoning", "bogus", "unknown reasoning level \"bogus\""),
        ("mode", "auto", "unknown option \"mode\""),
    ] {
        let refused = acp.set_option(&session, id, value).await;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{message}");
    }
    let refused = acp
        .request(
            "session/set_config_option",
            json!({"sessionId": session, "configId": "model", "type": "boolean", "value": true}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    let unknown = acp.set_option("nope", "model", "next-model").await;
    assert!(
        unknown["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown session")),
        "{unknown}"
    );

    acp.prompt(&session, "go").await;
    let request = &server.requests()[0].body;
    assert_eq!(request["model"], "mock-model");
    assert!(request.get("reasoning_effort").is_none(), "{request}");
    assert!(acp.finish().await.success());
}

#[tokio::test]
async fn an_option_does_not_change_while_a_turn_runs() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![stalled_answer(), Reply::text("after")]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, TWO_MODELS).await;
    let session = acp.session(dir.path()).await;
    acp.write(prompt_request(50, &session, "first")).await;
    assert_eq!(text_of(&acp.next_update().await), "partial");

    let refused = acp.set_option(&session, "model", "next-model").await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("a turn is running"), "{message}");

    acp.notify("session/cancel", json!({"sessionId": session})).await;
    assert_eq!(acp.response(50).await["result"]["stopReason"], "cancelled");
    acp.prompt(&session, "second").await;
    assert_eq!(server.requests()[1].body["model"], "mock-model");
    assert!(acp.finish().await.success());
}

/// Three models: two with a context window, and one without.
const WINDOWED: &str = "idleTimeoutSecs = 10\n[[models]]\nid = \"mock-model\"\ncontextWindow = 1000\n\
     [[models]]\nid = \"next-model\"\ncontextWindow = 2000\n[[models]]\nid = \"unmeasured\"";

#[tokio::test]
async fn the_usage_follows_the_context_of_a_model_with_a_window() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("hi"), Reply::text("unmeasured")]).await;
    let mut acp = Acp::start_configured(&server, dir.path(), &["--tools", ""], None, WINDOWED).await;
    let session = acp.session(dir.path()).await;

    // Right after the commands: the system prompt, estimated.
    let opened = acp.next_update().await;
    assert_eq!(opened["sessionUpdate"], "usage_update", "{opened}");
    assert_eq!(opened["size"], 1000, "{opened}");
    assert!(opened["used"].as_u64().unwrap() > 0, "{opened}");
    assert!(opened.get("cost").is_none(), "{opened}");

    acp.prompt(&session, "hello").await;
    let updates = acp.replayed();
    let kinds: Vec<&str> = updates
        .iter()
        .map(|update| update["sessionUpdate"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["agent_message_chunk", "usage_update", "usage_update"],
        "{updates:?}"
    );
    // The answer measured its request: 10 prompt and 2 completion tokens.
    let answered = &updates[1];
    assert_eq!(
        (answered["used"].as_u64(), answered["size"].as_u64()),
        (Some(12), Some(1000))
    );
    // The end of the turn adds an estimate from the bytes of the messages after the answer:
    // there are none, but even an empty list serializes to a few bytes.
    let ended = &updates[2];
    let used = ended["used"].as_u64().unwrap();
    assert!((12..20).contains(&used), "{ended}");
    assert_eq!(ended["size"], 1000, "{ended}");

    acp.set_option(&session, "model", "next-model").await;
    let switched = acp.next_update().await;
    assert_eq!(switched["sessionUpdate"], "usage_update", "{switched}");
    assert_eq!(
        (switched["used"].as_u64(), switched["size"].as_u64()),
        (Some(used), Some(2000))
    );
    // Without a window there is nothing to measure against.
    acp.set_option(&session, "model", "unmeasured").await;
    acp.prompt(&session, "again").await;
    let updates = acp.replayed();
    assert!(
        updates.iter().all(|update| update["sessionUpdate"] != "usage_update"),
        "{updates:?}"
    );
    assert!(acp.finish().await.success());
}
