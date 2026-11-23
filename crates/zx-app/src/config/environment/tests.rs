use std::time::Duration;

use zx_agent::ai::openai::{MaxTokensField, ReasoningFormat, SystemUpdates};

use super::*;
use crate::config::provider::{OPENAI, openai_config};

fn load_text(text: &str) -> Result<Config> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    Config::load(&path)
}

fn error(text: &str) -> String {
    format!("{:#}", load_text(text).unwrap_err())
}

#[test]
fn a_missing_file_is_an_empty_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::load(&path).unwrap();
    assert_eq!(config.path.as_deref(), Some(path.as_path()));
    assert_eq!(config.model, None);
    assert_eq!(config.provider.api, OPENAI);
    assert_eq!(config.provider.api_key_env, None);
    assert_eq!(config.provider.retry.max_retries, 3);
    assert_eq!(config.execution, LOCAL);
    assert_eq!(config.tool_output.threshold_bytes, Some(8 * 1024));
    // In each session's working directory.
    assert_eq!(config.tool_output.directory, Path::new(".zx-agent/outputs"));
    assert!(config.mcp.is_empty() && config.models.is_empty());
}

#[test]
fn the_model_and_its_provider_are_read_from_the_file() {
    let config = load_text(
        "model = 'm1'\nreasoning = 'high'\nmaxTokens = 512\ntoolTimeoutSecs = 30\napprovalTimeoutSecs = 60\n\
         [provider]\nbaseUrl = 'http://localhost:8000/v1'\n\
         apiKeyEnv = 'LOCAL_KEY'\nsystemUpdates = 'inline'\nreasoningFormat = 'thinking'\n\
         maxTokensField = 'max_completion_tokens'\n\
         fillMissingReasoning = true\npromptCacheKey = true\nidleTimeoutSecs = 15\nmaxRetries = 0\n\
         requestFields = { metadata = { user_id = 'u' }, top_k = 5 }\n\
         headers = { X-Route = 'eu' }",
    )
    .unwrap();
    assert_eq!(config.model.as_deref(), Some("m1"));
    assert_eq!(config.reasoning, Some(Reasoning::High));
    assert_eq!((config.max_tokens, config.tool_timeout_secs), (Some(512), Some(30)));
    let mut fields = serde_json::Map::new();
    fields.insert("metadata".into(), serde_json::json!({"user_id": "u"}));
    fields.insert("top_k".into(), serde_json::json!(5));
    let provider = &config.provider;
    let openai = openai_config(&provider.options, config.reasoning).unwrap();
    assert_eq!(openai.base_url, "http://localhost:8000/v1");
    assert_eq!(provider.api_key_env.as_deref(), Some("LOCAL_KEY"));
    assert_eq!(openai.body.system_updates, SystemUpdates::Inline);
    assert_eq!(openai.body.reasoning_format, ReasoningFormat::Thinking);
    assert_eq!(openai.body.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    assert!(openai.body.fill_missing_reasoning && openai.body.prompt_cache_key);
    assert_eq!(openai.body.extra_fields, fields);
    assert_eq!(openai.headers, [("X-Route".to_string(), "eu".to_string())]);
    assert_eq!(openai.idle_timeout, Duration::from_secs(15));
    assert_eq!(provider.retry.max_retries, 0);
    assert_eq!(config.approval_timeout_secs, Some(60));
}

#[test]
fn secret_files_are_relative_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "secretFiles = ['token', '/run/secrets/other']").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.secret_files,
        [dir.path().join("token"), PathBuf::from("/run/secrets/other")]
    );
}

#[test]
fn an_invalid_value_is_an_error_that_names_its_key() {
    for (text, expected) in [
        ("toolTimeoutSecs = 0", "toolTimeoutSecs must be greater than zero"),
        ("maxTokens = 0", "maxTokens must be greater than zero"),
        (
            "[provider]\nidleTimeoutSecs = 0",
            "provider.idleTimeoutSecs must be greater than zero",
        ),
        (
            "approvalTimeoutSecs = 0",
            "approvalTimeoutSecs must be greater than zero",
        ),
        (
            "[provider]\nheaders = { Authorization = 'Bearer x' }",
            "provider.headers: Authorization",
        ),
        (
            "[provider]\nheaders = { ' ' = 'x' }",
            "provider.headers: a header has no name",
        ),
        (
            "[provider]\nrequestFields = { ' ' = 1 }",
            "provider.requestFields: a field has no name",
        ),
        ("[provider]\nrequestFields = { seed = 1, seed = 2 }", "duplicate key"),
        ("[provider]\nrequestFields = 'seed=1'", "invalid type"),
        ("[provider]\nmaxRetries = -1", "maxRetries"),
        ("secretFiles = ['']", "secretFiles: an entry must not be empty"),
        ("maxTokens = 'many'", "maxTokens"),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn unknown_keys_and_invalid_values_are_errors_that_name_the_file() {
    for (text, expected) in [
        ("typo = true", "unknown field `typo`"),
        ("[provider]\napiKey = 'sk'", "unknown field `apiKey`"),
        ("model = ' '", "model must not be empty"),
        ("reasoning = 'hard'", "unknown reasoning level"),
        ("[provider]\nsystemUpdates = 'merge'", "provider.systemUpdates"),
        ("[provider]\nreasoningFormat = 'x'", "provider.reasoningFormat"),
        (
            "reasoning = 'low'\n[provider]\nrequestFields = { reasoning_effort = 'high' }",
            "provider.requestFields.reasoning_effort conflicts with reasoning",
        ),
        (
            "[provider]\nmaxTokensField = 'max_output_tokens'",
            "provider.maxTokensField",
        ),
        ("[provider]\nbaseUrl = ''", "provider.baseUrl must not be empty"),
        ("execution = ' '", "execution must not be empty"),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
        assert!(error.contains("config.toml"), "{text}: {error}");
    }
}

#[test]
fn container_execution_reads_its_settings_and_defaults_to_local() {
    let config = load_text(
        "execution = 'container'\n[container]\nimage = 'debian:bookworm-slim'\nuser = '1000:1000'\nmemory = '512m'\n\
         pidsLimit = 64\nnetwork = 'none'\nworkspace = 'readOnly'\nroot = 'readOnly'",
    )
    .unwrap();
    assert_eq!(config.execution, CONTAINER);
    assert_eq!(
        config.container,
        ContainerSettings {
            image: Some("debian:bookworm-slim".into()),
            user: Some("1000:1000".into()),
            memory: Some("512m".into()),
            pids_limit: Some(64),
            network: Some("none".into()),
            workspace: Some(Access::ReadOnly),
            root: Some(Access::ReadOnly),
        }
    );
    // Unset, the sandbox settings are left to the executor, whose defaults are open.
    let open = load_text("execution = 'container'\n[container]\nimage = 'x'").unwrap();
    assert_eq!(
        (open.container.network, open.container.workspace, open.container.root),
        (None, None, None)
    );
}

#[test]
fn invalid_container_settings_are_configuration_errors() {
    for text in [
        "execution = 'container'",
        "execution = 'container'\n[container]\nimage = ' '",
        "[container]\nimage = 'x'\npidsLimit = 0",
        "[container]\nimage = 'x'\nuser = ''",
        "[container]\nimage = 'x'\nnetwork = 'host'",
        "[container]\nimage = 'x'\nnetwork = 'container:other'",
        "[container]\nimage = 'x'\nnetwork = ' '",
        "[container]\nimage = 'x'\nworkspace = 'ro'",
        "[container]\nimage = 'x'\nroot = 'writable'",
    ] {
        assert!(load_text(text).is_err(), "{text}");
    }
}

#[test]
fn the_executors_refusals_of_container_settings_name_their_keys() {
    for (text, expected) in [
        (
            "[container]\nimage = 'x'\nuser = 'root'",
            "container.user \"root\" is root; commands must run as an unprivileged user",
        ),
        (
            "execution = 'container'\n[container]\nimage = 'x'\nuser = '0:0'",
            "container.user \"0:0\" is root",
        ),
        (
            "[container]\nimage = 'x'\npidsLimit = 0",
            "container.pidsLimit must be greater than zero",
        ),
        (
            "[container]\nimage = 'x'\nnetwork = 'host'",
            "container.network \"host\" would share a network namespace outside the sandbox",
        ),
        (
            "execution = 'container'",
            "execution = \"container\" needs container.image",
        ),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn an_output_directory_is_relative_to_the_file_and_settings_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[toolOutput]\ndirectory = 'results'\nthresholdBytes = 0").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.tool_output.directory, dir.path().join("results"));
    assert_eq!(config.tool_output.threshold_bytes, None);
    for text in [
        "[toolOutput]\ndirectory = ''",
        "[toolOutput]\nthreshold_bytes = 256",
        "[toolOutput]\nthresholdBytes = 1",
        "[toolOutput]\nmaxFileBytes = 1048577",
    ] {
        assert!(load_text(text).is_err(), "{text}");
    }
}

#[test]
fn the_stores_refusals_of_output_settings_name_their_keys() {
    for (text, expected) in [
        (
            "[toolOutput]\nthresholdBytes = 1",
            "toolOutput.thresholdBytes must be 0, which keeps results inline, or 256..=1048576 (toolOutput.maxFileBytes)",
        ),
        (
            "[toolOutput]\nmaxFileBytes = 1000\nthresholdBytes = 2000",
            "or 256..=1000 (toolOutput.maxFileBytes)",
        ),
        (
            "[toolOutput]\nmaxFileBytes = 1048577",
            "toolOutput.maxFileBytes must be 1..=1048576",
        ),
        ("[toolOutput]\ndirectory = ''", "toolOutput.directory must not be empty"),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
        assert!(!error.contains("threshold_bytes") && !error.contains("None"), "{error}");
    }
}

#[test]
fn models_are_known_by_id_and_their_prices_checked() {
    let config = load_text("[[models]]\nid = 'm'\ncontextWindow = 1000\ncost = { input = 1.0, output = 2.0 }").unwrap();
    assert_eq!(config.models["m"].context_window, Some(1000));
    for (text, expected) in [
        ("[[models]]\nid = ' '", "an entry has no id"),
        (
            "[[models]]\nid = 'm'\ncontextWindow = 0",
            "contextWindow must be greater than zero",
        ),
        (
            "[[models]]\nid = 'm'\ncost = { input = -1.0, output = 0.0 }",
            "prices must be zero or more",
        ),
        (
            "[[models]]\nid = 'm'\ncost = { input = 1.0, output = 2.0, cache_read = 0.5 }",
            "unknown field `cache_read`",
        ),
        (
            "[[models]]\nid = 'm'\ncontextWindow = 1000\n[[models]]\nid = 'm'\ncontextWindow = 2000",
            "more than one [[models]] entry has the id \"m\"",
        ),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn a_model_takes_images_only_when_its_input_says_so() {
    let config = load_text(
        "[[models]]\nid = 'plain'\n[[models]]\nid = 'text'\ninput = ['text']\n\
         [[models]]\nid = 'seeing'\ninput = ['text', 'image']",
    )
    .unwrap();
    assert!(!config.models["plain"].takes_images, "text alone by default, as in pi");
    assert!(!config.models["text"].takes_images);
    assert!(config.models["seeing"].takes_images);
    for (text, expected) in [
        (
            "[[models]]\nid = 'm'\ninput = ['image']",
            "models.m: input must include \"text\"",
        ),
        (
            "[[models]]\nid = 'm'\ninput = ['text', 'video']",
            "unknown variant `video`",
        ),
    ] {
        let error = error(text);
        assert!(error.contains(expected), "{text}: {error}");
    }
}

const PROM: &str = "[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntokenEnv = 'PROM_TOKEN'\n\
                    tools = ['query', 'range']\ntimeoutSecs = 30\n";

#[test]
fn mcp_servers_are_read_from_the_file() {
    let config = load_text(PROM).unwrap();
    assert_eq!(
        config.mcp,
        [McpServer {
            name: "prom".into(),
            url: "https://prom.example/mcp".into(),
            token_env: Some("PROM_TOKEN".into()),
            headers: BTreeMap::new(),
            header_env: BTreeMap::new(),
            tools: vec!["query".into(), "range".into()],
            trust_read_only: false,
            timeout_secs: Some(30),
        }]
    );
    assert_eq!(config.mcp_tool_names().unwrap(), ["prom__query", "prom__range"]);
}

#[test]
fn an_mcp_server_sends_its_headers_and_trusts_its_read_only_tools_when_told() {
    let text = format!(
        "{PROM}headers = {{ X-Scope-OrgID = 'tenant-a' }}\nheaderEnv = {{ X-Api-Key = 'PROM_KEY' }}\ntrustReadOnly = true\n"
    );
    let config = load_text(&text).unwrap();
    let server = &config.mcp[0];
    assert!(server.trust_read_only);
    // Written values first, then those of the variables, read when connecting.
    let sent = server
        .sent_headers(|header, variable| Ok(format!("{header} from {variable}")))
        .unwrap();
    assert_eq!(
        sent,
        [
            ("X-Scope-OrgID".to_string(), "tenant-a".to_string()),
            ("X-Api-Key".to_string(), "X-Api-Key from PROM_KEY".to_string()),
        ]
    );
}

#[test]
fn malformed_mcp_settings_are_refused() {
    let server = |fields: &str| format!("[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\n{fields}\n");
    for (text, expected) in [
        (server("tools = []"), "tools lists no tool"),
        (
            server("tools = ['query']\nallowTools = ['query']"),
            "unknown field `allowTools`",
        ),
        (
            server("tools = ['query']\ntimeoutSecs = 0"),
            "timeoutSecs must be greater than zero",
        ),
        (
            server("tools = ['query']\nmaxTimeoutSecs = 600"),
            "unknown field `maxTimeoutSecs`",
        ),
        (
            server("tools = ['query']\ntokenEnv = ' '"),
            "tokenEnv must not be empty",
        ),
        (server("tools = ['a.b', 'a_b']"), "like another of its tools"),
        (server(&format!("tools = ['{}']", "x".repeat(64))), "longer than 64"),
        (
            format!("{}{}", server("tools = ['query']"), server("tools = ['range']")),
            "more than one MCP server",
        ),
        (
            "[[mcp]]\nname = 'Prom'\nurl = 'https://prom.example/mcp'\ntools = ['query']\n".to_string(),
            "must be ASCII lowercase",
        ),
        (
            server("tools = ['query']\nheaders = { Accept = 'text/plain' }"),
            "header \"Accept\" is set by the transport",
        ),
        (
            server("tools = ['query']\ntokenEnv = 'T'\nheaderEnv = { Authorization = 'A' }"),
            "header \"Authorization\" is set by the token",
        ),
        (
            server("tools = ['query']\nheaders = { X-A = '1' }\nheaderEnv = { x-a = 'A' }"),
            "header \"x-a\" is given more than once",
        ),
        (
            server("tools = ['query']\nheaders = { 'X A' = '1' }"),
            "header \"X A\" is not a valid header name",
        ),
        (
            server("tools = ['query']\nheaderEnv = { X-Api-Key = ' ' }"),
            "headerEnv.X-Api-Key must not be empty",
        ),
        (server("tools = ['query']\ntrustReadOnly = 'yes'"), "invalid type"),
    ] {
        let error = error(&text);
        assert!(error.contains(expected), "{text}\n{error}");
    }
}

#[test]
fn the_file_is_read_within_bounds() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Config::load(dir.path()).is_err());
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("# {}", "x".repeat(64 * 1024))).unwrap();
    assert!(format!("{:#}", Config::load(&path).unwrap_err()).contains("exceeds"));
}
