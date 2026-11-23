//! im.toml, the configuration of the IM service: the server's sections, checked, and a section
//! for each platform, which only that platform can check.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use zx_app::config::read_text;

use crate::{CardLabels, ConversationId, Replies, ServerConfig, Trigger, UserId};

/// The sections of im.toml that are the server's; each of the others configures the platform
/// registered under its name.
const SERVER_SECTIONS: [&str; 7] = [
    "access",
    "limits",
    "sessions",
    "replies",
    "cardLabels",
    "schedules",
    "chats",
];
/// The shortest interval of a schedule: each time it posts in the chat and runs the model, so
/// more often than once a minute is noise and cost rather than watch.
const MIN_SCHEDULE_SECS: u64 = 60;

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
    /// Conversations the server starts at fixed intervals, `[[schedules]]`.
    #[serde(default)]
    pub(super) schedules: Vec<ScheduleSection>,
    /// Chats with an agent or approvers of their own, `[[chats]]`.
    #[serde(default)]
    pub(super) chats: Vec<ChatSection>,
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

/// One of `[[schedules]]`: a conversation started in `chat` every `every_secs`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ScheduleSection {
    /// Unique among the schedules: the source of its conversations is `schedule:<name>`.
    name: String,
    /// A served chat, as `platform:id`.
    chat: String,
    every_secs: u64,
    /// What the model is asked.
    prompt: String,
    /// What the post that opens each thread says; the name by default.
    title: Option<String>,
}

/// One of `[[chats]]`: what a chat the server serves has of its own.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ChatSection {
    /// A served chat, as `platform:id`, given once among the chats.
    chat: String,
    /// The agent of its conversations, by name: `agents/<agent>.md` in the zx-agent home, or
    /// a built-in agent.
    agent: Option<String>,
    /// Who may approve in it, as `platform:id` on its platform, in place of
    /// `access.approvers`.
    approvers: Option<Vec<String>>,
}

/// A chat of `[[chats]]`, checked: what it has of its own, its agent by name.
#[derive(Debug, PartialEq)]
pub(super) struct OwnChat {
    pub(super) platform: String,
    pub(super) chat: ConversationId,
    pub(super) agent: Option<String>,
    pub(super) approvers: Option<Vec<UserId>>,
}

/// The chats of the file with settings of their own, checked: each names a chat among
/// `conversations`, on one of `platforms`, once, and gives an agent or approvers, those on
/// the chat's platform. Whether the agent exists is known once it is built.
pub(super) fn chats(
    sections: &[ChatSection],
    conversations: &Conversations,
    platforms: &HashSet<&str>,
) -> Result<Vec<OwnChat>> {
    let mut given = HashSet::new();
    let mut chats = Vec::with_capacity(sections.len());
    for section in sections {
        let (platform, chat) = served_chat(&section.chat, conversations, platforms, "chats")?;
        let field = format!("chats {:?}", section.chat);
        ensure!(
            given.insert((platform.clone(), chat.clone())),
            "{field}: the chat is given more than once"
        );
        ensure!(
            section.agent.is_some() || section.approvers.is_some(),
            "{field}: give agent, approvers or both"
        );
        let approvers = match &section.approvers {
            Some(listed) => Some(approvers_on(
                listed,
                &platform,
                platforms,
                &format!("{field}.approvers"),
            )?),
            None => None,
        };
        chats.push(OwnChat {
            platform,
            chat,
            agent: section.agent.clone(),
            approvers,
        });
    }
    Ok(chats)
}

/// The users of the `platform:id` entries `listed` for `field`, each on `platform`.
fn approvers_on(listed: &[String], platform: &str, platforms: &HashSet<&str>, field: &str) -> Result<Vec<UserId>> {
    let mut users = Vec::with_capacity(listed.len());
    for (on, id) in entries(listed, platforms, field)? {
        ensure!(
            on == platform,
            "{field}: \"{on}:{id}\" is on another platform than the chat"
        );
        users.push(UserId(id));
    }
    Ok(users)
}

/// The schedules of the file, checked: each with its interval and the trigger it asks for.
/// Each names a chat among `conversations`, on one of `platforms`, and has a unique name, a
/// prompt and an interval of at least a minute.
pub(super) fn schedules(
    sections: &[ScheduleSection],
    conversations: &Conversations,
    platforms: &HashSet<&str>,
) -> Result<Vec<(Duration, Trigger)>> {
    let mut names = HashSet::new();
    let mut schedules = Vec::with_capacity(sections.len());
    for section in sections {
        let name = section.name.as_str();
        ensure!(!name.trim().is_empty(), "schedules: a schedule has an empty name");
        ensure!(
            names.insert(name),
            "schedules: more than one schedule is named {name:?}"
        );
        let field = format!("schedules {name:?}");
        let (platform, chat) = served_chat(&section.chat, conversations, platforms, &format!("{field}.chat"))?;
        ensure!(
            section.every_secs >= MIN_SCHEDULE_SECS,
            "{field}.everySecs: at least {MIN_SCHEDULE_SECS}"
        );
        ensure!(!section.prompt.trim().is_empty(), "{field}.prompt is empty");
        let trigger = Trigger {
            platform,
            chat,
            title: section.title.clone().unwrap_or_else(|| name.to_string()),
            prompt: section.prompt.clone(),
            source: format!("schedule:{name}"),
        };
        schedules.push((Duration::from_secs(section.every_secs), trigger));
    }
    Ok(schedules)
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

/// The chat the `platform:id` entry `chat` names for `field`: one among `conversations`, on
/// one of `platforms`.
fn served_chat(
    chat: &str,
    conversations: &Conversations,
    platforms: &HashSet<&str>,
    field: &str,
) -> Result<(String, ConversationId)> {
    let mut split = entries(&[chat.to_string()], platforms, field)?;
    let (platform, id) = split.pop().expect("entries gives one pair for each entry");
    let served = (platform, ConversationId(id));
    ensure!(
        conversations.contains(&served),
        "{field}: {chat:?} is not in access.conversations"
    );
    Ok(served)
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

    /// The schedules of a file that serves slack:C1, with `schedules` after its server's
    /// sections.
    fn checked_schedules(schedules: &str) -> Result<Vec<(Duration, Trigger)>> {
        let config = format!("[access]\nconversations = [\"slack:C1\"]\n[sessions]\ndir = \"d\"\n{schedules}");
        let file = ConfigFile::parse(&config)?;
        let (conversations, _) = access(&file.access, &slack_only())?;
        super::schedules(&file.schedules, &conversations, &slack_only())
    }

    const HOURLY: &str =
        "[[schedules]]\nname = \"hourly\"\nchat = \"slack:C1\"\neverySecs = 3600\nprompt = \"look around\"\n";

    #[test]
    fn a_schedule_asks_for_its_prompt_in_its_chat_with_its_name_as_title_and_source() {
        let schedules = checked_schedules(HOURLY).unwrap();
        let trigger = Trigger {
            platform: "slack".into(),
            chat: ConversationId("C1".into()),
            title: "hourly".into(),
            prompt: "look around".into(),
            source: "schedule:hourly".into(),
        };
        assert_eq!(schedules, [(Duration::from_secs(3600), trigger)]);
        let titled = checked_schedules(&format!("{HOURLY}title = \"Hourly check\"\n")).unwrap();
        assert_eq!(titled[0].1.title, "Hourly check");
    }

    #[test]
    fn a_schedule_that_cannot_run_as_written_is_refused() {
        let cases = [
            (HOURLY.replace("slack:C1", "slack:C2"), "is not in access.conversations"),
            (HOURLY.replace("slack:C1", "teams:C1"), "not configured"),
            (HOURLY.replace("3600", "59"), "everySecs: at least 60"),
            (HOURLY.replace("look around", " "), "prompt is empty"),
            (HOURLY.replace("\"hourly\"", "\"\""), "empty name"),
            (
                format!("{HOURLY}{HOURLY}"),
                "more than one schedule is named \"hourly\"",
            ),
            (format!("{HOURLY}cron = \"0 * * * *\"\n"), "unknown field `cron`"),
        ];
        for (schedules, expected) in cases {
            let error = format!("{:#}", checked_schedules(&schedules).unwrap_err());
            assert!(
                error.contains(expected),
                "{schedules}: expected {expected:?} in {error}"
            );
        }
    }

    /// The chats of a file that serves slack:C1 and feishu:oc_1, with `chats` after its
    /// server's sections.
    fn checked_chats(chats: &str) -> Result<Vec<OwnChat>> {
        let platforms = HashSet::from(["slack", "feishu"]);
        let config =
            format!("[access]\nconversations = [\"slack:C1\", \"feishu:oc_1\"]\n[sessions]\ndir = \"d\"\n{chats}");
        let file = ConfigFile::parse(&config)?;
        let (conversations, _) = access(&file.access, &platforms)?;
        super::chats(&file.chats, &conversations, &platforms)
    }

    const PROD: &str = "[[chats]]\nchat = \"slack:C1\"\nagent = \"prod\"\napprovers = [\"slack:U9\"]\n";

    #[test]
    fn a_chat_has_an_agent_and_approvers_of_its_own() {
        let prod = OwnChat {
            platform: "slack".into(),
            chat: ConversationId("C1".into()),
            agent: Some("prod".into()),
            approvers: Some(vec![UserId("U9".into())]),
        };
        assert_eq!(checked_chats(PROD).unwrap(), [prod]);
        // Either is enough: the chat keeps the server's other.
        let agent_only = checked_chats(&PROD.replace("approvers = [\"slack:U9\"]\n", "")).unwrap();
        assert_eq!(
            (agent_only[0].agent.as_deref(), &agent_only[0].approvers),
            (Some("prod"), &None)
        );
    }

    #[test]
    fn a_chat_that_cannot_be_used_as_written_is_refused() {
        let cases = [
            (
                PROD.replace("slack:C1", "slack:C2"),
                "chats: \"slack:C2\" is not in access.conversations",
            ),
            (PROD.replace("slack:C1", "teams:C1"), "which is not configured"),
            (
                format!("{PROD}{PROD}"),
                "chats \"slack:C1\": the chat is given more than once",
            ),
            (
                "[[chats]]\nchat = \"slack:C1\"\n".to_string(),
                "chats \"slack:C1\": give agent, approvers or both",
            ),
            (
                PROD.replace("slack:U9", "feishu:ou_9"),
                "chats \"slack:C1\".approvers: \"feishu:ou_9\" is on another platform than the chat",
            ),
            (format!("{PROD}model = \"m\"\n"), "unknown field `model`"),
        ];
        for (chats, expected) in cases {
            let error = format!("{:#}", checked_chats(&chats).unwrap_err());
            assert!(error.contains(expected), "{chats}: expected {expected:?} in {error}");
        }
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
