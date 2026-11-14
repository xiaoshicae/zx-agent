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

use common::mock_server::{MockServer, Reply};
use common::{Home, fake_model_config, session_dir, stderr, write_agent, write_config, zx_agent_in};

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
    common::session_files(dir)
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
        let own = match home {
            Some(_) => None,
            None => Some(tempfile::tempdir().unwrap()),
        };
        let home = home.map_or_else(|| own.as_ref().unwrap().path().to_path_buf(), Path::to_path_buf);
        write_config(
            &home.join(".zx-agent"),
            &fake_model_config(KEY_VAR, "", "idleTimeoutSecs = 10"),
        );
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

    /// Initialize; the response, with the agent's capabilities.
    async fn initialize(&mut self) -> Value {
        let initialized = self
            .request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}}))
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
    /// offers for it, which is `/name` alone.
    async fn expect_commands(&mut self, session_id: &str) {
        let message = self.read().await;
        assert_eq!(message["method"], "session/update", "{message}");
        assert_eq!(message["params"]["sessionId"], session_id, "{message}");
        let update = &message["params"]["update"];
        assert_eq!(update["sessionUpdate"], "available_commands_update", "{message}");
        let commands = update["availableCommands"].as_array().unwrap();
        assert_eq!(commands.len(), 1, "{message}");
        assert_eq!(commands[0]["name"], "name", "{message}");
        assert!(commands[0]["description"].is_string(), "{message}");
        assert!(commands[0]["input"]["hint"].is_string(), "{message}");
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
    acp.write(json!({"jsonrpc": "2.0", "id": 99, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}}))
        .await;

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
    acp.write(json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}}))
        .await;
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
    let partial = json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]});
    let server = MockServer::start(vec![Reply::Stall(vec![partial]), Reply::text("again")]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    acp.write(json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": "first"}]}}))
        .await;
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
    acp.write(json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": "write"}]}}))
        .await;

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
    let shown = call["content"][0]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{call}"));
    assert!(shown.contains("deploys on tuesdays"), "{shown}");
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
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().canonicalize().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let mut acp = Acp::start(&server, &cwd, &[]).await;
    let initialized = acp.initialize().await;
    let capabilities = &initialized["result"]["agentCapabilities"]["sessionCapabilities"];
    assert_eq!(
        (capabilities["list"].clone(), capabilities["resume"].clone()),
        (json!({}), json!({})),
        "{initialized}"
    );
    let mut ids = Vec::new();
    for prompt in ["first question", "second question"] {
        // Listed by last activity, which has millisecond resolution.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let id = acp.create(&cwd).await;
        let response = acp.prompt(&id, prompt).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        ids.push(id);
    }

    let listed = acp.list(json!({"cwd": cwd})).await;
    let sessions = listed["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"));
    let listed_ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(listed_ids, [ids[1].as_str(), ids[0].as_str()], "{listed}");
    assert_eq!(sessions[0]["title"], "second question");
    assert_eq!(sessions[1]["title"], "first question");
    assert_eq!(sessions[0]["cwd"], cwd.display().to_string());
    let updated = sessions[0]["updatedAt"].as_str().unwrap_or_else(|| panic!("{listed}"));
    assert!(updated.contains('T') && updated.ends_with('Z'), "{updated}");
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
    let partial = json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]});
    let server = MockServer::start(vec![Reply::Stall(vec![partial])]).await;
    let mut acp = Acp::start(&server, dir.path(), &["--tools", ""]).await;
    let session = acp.session(dir.path()).await;
    acp.write(json!({"jsonrpc": "2.0", "id": 50, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": "first"}]}}))
        .await;
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
