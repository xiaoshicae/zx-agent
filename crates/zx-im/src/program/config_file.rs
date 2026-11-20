//! im.toml, the configuration of the IM service: the server's sections, checked, and a section
//! for each platform, which only that platform can check.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use zx_app::config::read_text;

use crate::{CardLabels, ConversationId, Replies, ServerConfig, UserId};

/// The sections of im.toml that are the server's; each of the others configures the platform
/// registered under its name.
const SERVER_SECTIONS: [&str; 5] = ["access", "limits", "sessions", "replies", "cardLabels"];

type Conversations = Vec<(String, ConversationId)>;
type Approvers = Vec<(String, UserId)>;

/// The IM service configuration file: the server's sections, and a section for each platform.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ConfigFile {
    pub(super) access: AccessSection,
    #[serde(default)]
    pub(super) limits: LimitsSection,
    pub(super) sessions: SessionsSection,
    /// Texts the server posts besides answers; each field is optional, the defaults English.
    #[serde(default)]
    pub(super) replies: Replies,
    /// Texts of approval cards on every platform; each field optional, the defaults English.
    #[serde(default)]
    pub(super) card_labels: CardLabels,
    /// The section of each platform, by its name: what is left of the file but the server's.
    #[serde(skip)]
    pub(super) platforms: Vec<(String, toml::Table)>,
}

impl ConfigFile {
    /// The file `text` holds: its server's sections, checked, and the others, which only the
    /// platforms they name can check.
    pub(super) fn parse(text: &str) -> Result<Self> {
        let mut server = toml::Table::new();
        let mut platforms = Vec::new();
        for (key, value) in toml::from_str::<toml::Table>(text)? {
            if SERVER_SECTIONS.contains(&key.as_str()) {
                server.insert(key, value);
                continue;
            }
            let toml::Value::Table(section) = value else {
                bail!("unknown key `{key}`: a platform is configured by a section, such as [slack]");
            };
            platforms.push((key, section));
        }
        let mut file: Self = toml::Value::Table(server).try_into()?;
        file.platforms = platforms;
        Ok(file)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct AccessSection {
    /// Chats the bot answers in, as `platform:id`.
    conversations: Vec<String>,
    /// Users who may approve, as `platform:id`.
    #[serde(default)]
    approvers: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct LimitsSection {
    max_running: Option<NonZeroUsize>,
    max_conversations: Option<NonZeroUsize>,
    max_pending_messages: Option<NonZeroUsize>,
    max_pending_bytes: Option<NonZeroUsize>,
    idle_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SessionsSection {
    pub(super) dir: PathBuf,
}

/// The IM service configuration file at `path`, read as config.toml is: a regular UTF-8 file
/// within a size limit. A startup read; nothing runs concurrently yet.
pub(super) fn read_file(path: &Path) -> Result<ConfigFile> {
    let text = read_text(path)?;
    ConfigFile::parse(&text).with_context(|| format!("invalid IM service configuration {}", path.display()))
}

/// The chats the bot answers in and the users who may approve, each on a configured platform.
pub(super) fn access(section: &AccessSection, platforms: &HashSet<&str>) -> Result<(Conversations, Approvers)> {
    let conversations = entries(&section.conversations, platforms, "access.conversations")?
        .into_iter()
        .map(|(platform, id)| (platform, ConversationId(id)))
        .collect();
    let approvers = entries(&section.approvers, platforms, "access.approvers")?
        .into_iter()
        .map(|(platform, id)| (platform, UserId(id)))
        .collect();
    Ok((conversations, approvers))
}

/// The server's settings that the file gives: its limits over the defaults, its texts, and
/// who may use the server.
pub(super) fn server_config(
    limits: &LimitsSection,
    replies: Replies,
    conversations: Conversations,
    approvers: Approvers,
) -> ServerConfig {
    let mut config = ServerConfig {
        conversations,
        approvers,
        replies,
        ..ServerConfig::default()
    };
    if let Some(running) = limits.max_running {
        config.max_running = running;
    }
    if let Some(limit) = limits.max_conversations {
        config.max_conversations = limit;
    }
    if let Some(limit) = limits.max_pending_messages {
        config.max_pending_messages = limit;
    }
    if let Some(limit) = limits.max_pending_bytes {
        config.max_pending_bytes = limit;
    }
    if let Some(secs) = limits.idle_secs {
        config.idle = Duration::from_secs(secs);
    }
    config
}

/// Split `platform:id` entries, each naming a configured platform.
fn entries(entries: &[String], platforms: &HashSet<&str>, field: &str) -> Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|entry| {
            let (platform, id) = entry
                .split_once(':')
                .filter(|(platform, id)| !platform.is_empty() && !id.is_empty())
                .with_context(|| format!("{field}: {entry:?} is not of the form platform:id"))?;
            anyhow::ensure!(
                platforms.contains(platform),
                "{field}: {entry:?} names {platform:?}, which is not configured"
            );
            Ok((platform.to_string(), id.to_string()))
        })
        .collect()
}

/// The server's sections of a file that configures no platform.
#[cfg(test)]
pub(super) const SERVER: &str = "[access]\nconversations = []\n[sessions]\ndir = \"d\"\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn slack_only() -> HashSet<&'static str> {
        HashSet::from(["slack"])
    }

    #[test]
    fn the_file_is_read_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("im.toml");
        let file = "[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"/data/sessions\"\n";
        std::fs::write(&path, format!("{file}# {}", "x".repeat(64 * 1024))).unwrap();
        assert!(format!("{:#}", read_file(&path).err().unwrap()).contains("exceeds"));
    }

    #[test]
    fn the_deployment_example_is_a_valid_configuration() {
        // The im.toml that the Kubernetes deployment starts from, read as --config is read.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/kubernetes/im/im.example.toml");
        if let Err(error) = read_file(&path) {
            panic!("{error:#}");
        }
    }

    #[test]
    fn entries_split_at_the_first_colon() {
        let parsed = entries(&["slack:C1".into(), "slack:a:b".into()], &slack_only(), "f").unwrap();
        assert_eq!(parsed, [("slack".into(), "C1".into()), ("slack".into(), "a:b".into())]);
    }

    #[test]
    fn entries_without_a_platform_or_an_id_are_refused() {
        for entry in ["C1", ":C1", "slack:"] {
            let error = entries(&[entry.into()], &slack_only(), "access.conversations").unwrap_err();
            assert!(error.to_string().contains("platform:id"), "{entry}: {error}");
        }
    }

    #[test]
    fn entries_for_an_unconfigured_platform_are_refused() {
        let error = entries(&["teams:C1".into()], &slack_only(), "access.approvers").unwrap_err();
        assert!(error.to_string().contains("not configured"), "{error}");
    }

    #[test]
    fn request_limit_reply_can_be_configured() {
        let config = format!("{SERVER}[replies]\nmodelRequestLimit = \"limit\"\n");
        let file = ConfigFile::parse(&config).unwrap();
        assert_eq!(file.replies.model_request_limit, "limit");
    }

    #[test]
    fn a_configuration_needs_access_and_a_session_directory() {
        assert!(ConfigFile::parse("[slack]\n").is_err());
        let file =
            ConfigFile::parse("[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"/data/sessions\"\n").unwrap();
        assert_eq!(file.platforms, [("slack".to_string(), toml::Table::new())]);
        assert_eq!(file.sessions.dir, Path::new("/data/sessions"));
    }

    #[test]
    fn an_unknown_key_before_the_first_section_is_refused() {
        // After a section, it would be that section's.
        let error = format!("{:#}", ConfigFile::parse(&format!("typo = 1\n{SERVER}")).err().unwrap());
        assert!(error.contains("unknown key `typo`"), "{error}");
    }

    #[test]
    fn a_limit_of_zero_is_refused() {
        for name in [
            "maxRunning",
            "maxConversations",
            "maxPendingMessages",
            "maxPendingBytes",
        ] {
            assert!(ConfigFile::parse(&format!("{SERVER}[limits]\n{name} = 0\n")).is_err());
        }
    }

    #[test]
    fn the_approval_timeout_is_not_a_limit_of_im_toml() {
        // It is config.toml's, shared with chat and acp.
        assert!(ConfigFile::parse(&format!("{SERVER}[limits]\napprovalTimeoutSecs = 60\n")).is_err());
    }
}
