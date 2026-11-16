//! A non-CLI host uses explicit configuration and owns the assembled sessions.

use std::path::Path;

use zx_agent::transcript::{SessionName, SessionQuery};
use zx_cli::application::{Application, Approvals, Overrides, SessionPlace};
use zx_cli::config::{Builtin, Config};

fn configured() -> Config {
    let mut config = Config {
        model: Some("configured-model".into()),
        ..Config::default()
    };
    config.provider.openai.base_url = "http://127.0.0.1:1/v1".into();
    config.provider.api_key_env = "ZX_APPLICATION_TEST_UNUSED_KEY".into();
    config
}

fn role_in(home: &Path) {
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::write(
        home.join("agents/unattended.md"),
        "+++\nextends = \"unattended\"\ntools = []\n[context]\ndiscover = false\n+++\nExplicit host role.\n",
    )
    .unwrap();
}

#[tokio::test]
async fn an_explicit_host_builds_and_reopens_sessions_without_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    role_in(&home);
    let app = Application::from_config(
        configured(),
        Some(home),
        Overrides {
            model: Some("host-model".into()),
            cwd: Some(work.clone()),
            ..Overrides::default()
        },
        Builtin::Unattended,
    )
    .await
    .unwrap();
    assert_eq!(app.model(), "host-model");
    assert!(app.notices().is_empty());
    let place = SessionPlace::directory(&dir.path().join("sessions"));
    let configured = app.build(&place, Approvals::Nobody).unwrap();
    assert_eq!(configured.name(), "unattended");
    let session = configured.create(None).await.unwrap();
    let id = session.id().to_string();
    session
        .set_name(SessionName::new("host session").unwrap())
        .await
        .unwrap();
    assert_eq!(
        configured
            .agent()
            .latest_session(&SessionQuery::default())
            .await
            .unwrap(),
        Some(id.clone())
    );
    assert!(configured.agent().close_session(&id));
    assert!(session.is_closed());
    let reopened = configured.open(&id).await.unwrap();
    reopened
        .with_transcript(|transcript| {
            assert_eq!(transcript.header().model, "host-model");
            assert_eq!(
                transcript.header().cwd,
                work.canonicalize().unwrap().display().to_string()
            );
            assert_eq!(transcript.session_name().unwrap().as_str(), "host session");
        })
        .unwrap();
}

#[tokio::test]
async fn an_explicit_host_gets_sanitized_notices_and_rejects_unusable_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "Q7z").unwrap();
    let mut config = configured();
    config.secret_files.push(secret.clone());
    let app = Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
        .await
        .unwrap();
    assert_eq!(app.notices().len(), 1);
    assert!(app.notices()[0].contains(&secret.display().to_string()));
    assert!(!app.notices()[0].contains("Q7z"));
    std::fs::remove_file(secret).unwrap();
    let mut config = configured();
    config.secret_files.push(dir.path().join("secret.txt"));
    let error = Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("cannot use secret file"));
}

#[tokio::test]
async fn invalid_model_and_budget_are_rejected_before_any_session_is_created() {
    let mut config = configured();
    config.model = None;
    assert!(
        Application::from_config(config, None, Overrides::default(), Builtin::Unattended)
            .await
            .is_err()
    );
    let app = Application::from_config(
        configured(),
        None,
        Overrides {
            cost_budget: Some(1.0),
            ..Overrides::default()
        },
        Builtin::Unattended,
    )
    .await
    .unwrap();
    let error = app.build(&SessionPlace::memory(), Approvals::Nobody).err().unwrap();
    assert!(format!("{error:#}").contains("a cost budget needs the prices"));
}
