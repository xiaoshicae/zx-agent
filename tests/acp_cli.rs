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
use common::zx_agent;

const KEY_VAR: &str = "ZX_ACP_TEST_KEY";

/// A client on the other end of the agent's stdio.
struct Acp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    /// Messages read while waiting for something else, in arrival order.
    inbox: VecDeque<Value>,
}

impl Acp {
    async fn start(server: &MockServer, cwd: &Path, args: &[&str]) -> Self {
        let mut child = zx_agent()
            .arg("acp")
            .args(["--model", "mock-model", "--base-url", &server.base_url])
            .args([
                "--api-key-env",
                KEY_VAR,
                "--max-retries",
                "0",
                "--idle-timeout-secs",
                "10",
            ])
            .arg("--no-context")
            .args(args)
            .current_dir(cwd)
            .env(KEY_VAR, "sk-acp-test-key")
            .env_remove("ZX_MODEL")
            .env_remove("ZX_BASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
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
        loop {
            let message = self.read().await;
            if message.get("method").is_none() && message["id"] == json!(id) {
                return message;
            }
            self.inbox.push_back(message);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn respond(&mut self, id: &Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
    }

    /// Initialize and create a session in `cwd`; the session ID.
    async fn session(&mut self, cwd: &Path) -> String {
        let initialized = self
            .request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}}))
            .await;
        assert_eq!(initialized["result"]["agentInfo"]["name"], "zx-agent", "{initialized}");
        let created = self.request("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
        created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{created}"))
            .to_string()
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
    let mut acp = Acp::start(&server, dir.path(), &["--auto-approve-cmd", "pwd"]).await;
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
