//! `config.toml` through the compiled program: where it is found, that its values are used,
//! and how `--model`, `ZX_MODEL` and `ZX_BASE_URL` go over it. Every test has its own home
//! directory.
mod common;

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;

use common::mock_server::{MockServer, Reply};
use common::{AgentHome, stderr, write_config, zx_agent};

/// `run` in `cwd` without a model on the command line; the caller adds the home.
fn run(cwd: &Path, args: &[&str]) -> Command {
    let mut command = zx_agent();
    command
        .args(["run", "--tools", ""])
        .args(args)
        .arg("go")
        .current_dir(cwd)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn output(mut command: Command) -> Output {
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap()
}

/// The model the only request to `server` named.
fn requested_model(server: &MockServer) -> String {
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    requests[0].body["model"].as_str().unwrap().to_string()
}

fn model_and_url(model: &str, server: &MockServer) -> String {
    format!("model = \"{model}\"\n[provider]\nbaseUrl = \"{}\"\n", server.base_url)
}

#[tokio::test]
async fn config_toml_is_found_in_dot_zx_agent_under_home() {
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    write_config(&home.path().join(".zx-agent"), &model_and_url("home-model", &server));
    let mut command = run(home.path(), &[]);
    command.env("HOME", home.path()).env_remove("ZX_AGENT_HOME");
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(requested_model(&server), "home-model");
}

#[tokio::test]
async fn zx_agent_home_replaces_dot_zx_agent_under_home() {
    let home = tempfile::tempdir().unwrap();
    let ignored = MockServer::start(vec![Reply::text("ignored")]).await;
    write_config(&home.path().join(".zx-agent"), &model_and_url("home-model", &ignored));
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let elsewhere = AgentHome::new(&model_and_url("elsewhere-model", &server));
    let mut command = run(home.path(), &[]);
    command.env("HOME", home.path()).env("ZX_AGENT_HOME", elsewhere.path());
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(requested_model(&server), "elsewhere-model");
    assert!(ignored.requests().is_empty());
}

#[tokio::test]
async fn a_relative_zx_agent_home_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![]).await;
    let mut command = run(dir.path(), &["--model", "m"]);
    command
        .env("ZX_AGENT_HOME", "relative/home")
        .env("ZX_BASE_URL", &server.base_url);
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ZX_AGENT_HOME must be an absolute path"),
        "{}",
        stderr(&output)
    );
    assert!(server.requests().is_empty());
}

/// Which server and model a run used, with the file setting both and `env` and `flags` over it.
async fn chosen(env: &[(&str, &str)], flags: &[&str], file: &MockServer) -> Output {
    let home = AgentHome::new(&model_and_url("file-model", file));
    let dir = tempfile::tempdir().unwrap();
    let mut command = run(dir.path(), flags);
    command.env("ZX_AGENT_HOME", home.path()).envs(env.iter().copied());
    output(command).await
}

#[tokio::test]
async fn the_model_comes_from_the_flag_then_the_variable_then_the_file_and_the_base_url_from_the_variable_then_the_file()
 {
    let file = MockServer::start(vec![Reply::text("file")]).await;
    let output = chosen(&[], &[], &file).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(requested_model(&file), "file-model");

    let file = MockServer::start(vec![]).await;
    let env = MockServer::start(vec![Reply::text("env")]).await;
    let output = chosen(&[("ZX_MODEL", "env-model"), ("ZX_BASE_URL", &env.base_url)], &[], &file).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(requested_model(&env), "env-model");
    assert!(file.requests().is_empty());

    let env = MockServer::start(vec![Reply::text("env")]).await;
    let output = chosen(
        &[("ZX_MODEL", "env-model"), ("ZX_BASE_URL", &env.base_url)],
        &["--model", "flag-model"],
        &file,
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(requested_model(&env), "flag-model");
    assert!(file.requests().is_empty());
}

#[tokio::test]
async fn without_a_model_anywhere_the_run_names_where_to_set_one() {
    let home = AgentHome::new("");
    let dir = tempfile::tempdir().unwrap();
    let mut command = run(dir.path(), &[]);
    command.env("ZX_AGENT_HOME", home.path());
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let expected = format!(
        "no model: pass --model, set ZX_MODEL, or set model in {}",
        home.path().join("config.toml").display()
    );
    assert!(stderr(&output).contains(&expected), "{}", stderr(&output));
}

#[tokio::test]
async fn an_unknown_key_in_config_toml_stops_the_run() {
    let server = MockServer::start(vec![]).await;
    let home = AgentHome::new("modle = \"typo\"\n");
    let dir = tempfile::tempdir().unwrap();
    let mut command = run(dir.path(), &["--model", "m"]);
    command
        .env("ZX_AGENT_HOME", home.path())
        .env("ZX_BASE_URL", &server.base_url);
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("unknown field `modle`"), "{}", stderr(&output));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn the_former_command_line_settings_are_read_from_config_toml() {
    let server = MockServer::start(vec![Reply::text("ok")]).await;
    let home = AgentHome::new(&format!(
        "model = 'file-model'\nmaxTokens = 77\nreasoning = 'low'\n[provider]\nbaseUrl = '{}'\n\
         headers = {{ X-Route = 'blue' }}\nrequestFields = {{ seed = 7 }}\nmaxRetries = 0\n",
        server.base_url
    ));
    let dir = tempfile::tempdir().unwrap();
    let mut command = run(dir.path(), &[]);
    command.env("ZX_AGENT_HOME", home.path());
    let output = output(command).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let request = &server.requests()[0];
    assert_eq!(request.body["max_tokens"], 77);
    assert_eq!(request.body["reasoning_effort"], "low");
    assert_eq!(request.body["seed"], 7);
    assert!(request.headers.contains("x-route: blue"), "{}", request.headers);
}

#[tokio::test]
async fn invalid_settings_exit_2_naming_their_key() {
    for (text, key) in [
        ("toolTimeoutSecs = 0", "toolTimeoutSecs must be at least 1"),
        ("approvalTimeoutSecs = 0", "approvalTimeoutSecs must be at least 1"),
        (
            "[provider]\nidleTimeoutSecs = 0",
            "provider.idleTimeoutSecs must be at least 1",
        ),
        (
            "[provider]\nheaders = { Authorization = 'x' }",
            "provider.headers: Authorization",
        ),
        ("secretFiles = ['missing-token']", "missing-token"),
    ] {
        let server = MockServer::start(vec![]).await;
        let home = AgentHome::new(text);
        let dir = tempfile::tempdir().unwrap();
        let mut command = run(dir.path(), &["--model", "m"]);
        command
            .env("ZX_AGENT_HOME", home.path())
            .env("ZX_BASE_URL", &server.base_url);
        let output = output(command).await;
        assert_eq!(output.status.code(), Some(2), "{text}: {}", stderr(&output));
        assert!(stderr(&output).contains(key), "{text}: {}", stderr(&output));
        assert!(server.requests().is_empty());
    }
}
