//! `send_file`: the tool of each IM session that posts a file in its conversation, as people
//! post an attachment. Each session gets its own, for the conversation its labels name.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use zx_agent::ai::ToolDef;
use zx_agent::builtin::tools::{parse_args, unless_cancelled};
use zx_agent::tools::{Tool, ToolContext, ToolOutput};
use zx_agent::transcript::SessionHeader;
use zx_app::SessionTools;

use crate::{Attachment, Destination, ImChannel, SessionKey};

const SEND_FILE: &str = "send_file";
/// The largest file posted: within what both platforms take (Feishu: 30 MB a file).
const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
/// How long a post may take, the upload included: a large file over a slow link.
const POST_TIMEOUT: Duration = Duration::from_secs(120);

/// Makes `send_file` for each session of a conversation on one of the channels, by platform.
pub(super) struct SendFiles {
    channels: HashMap<String, Arc<dyn ImChannel>>,
}

impl SendFiles {
    pub(super) fn new(channels: &[Arc<dyn ImChannel>]) -> Self {
        let channels = channels
            .iter()
            .map(|channel| (channel.platform().to_string(), Arc::clone(channel)))
            .collect();
        Self { channels }
    }
}

impl SessionTools for SendFiles {
    fn names(&self) -> Vec<String> {
        vec![SEND_FILE.into()]
    }

    /// `send_file` for the session of a conversation on a channel; none for another session.
    fn tools(&self, header: &SessionHeader) -> Vec<Arc<dyn Tool>> {
        let Some(key) = SessionKey::of_labels(&header.labels) else {
            return Vec::new();
        };
        let Some(channel) = self.channels.get(&key.platform) else {
            return Vec::new();
        };
        vec![Arc::new(SendFile {
            channel: Arc::clone(channel),
            to: key.destination(),
        })]
    }
}

/// Posts a file in the conversation `to` on `channel`.
struct SendFile {
    channel: Arc<dyn ImChannel>,
    to: Destination,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    name: Option<String>,
    content: Option<String>,
    path: Option<String>,
}

#[async_trait]
impl Tool for SendFile {
    fn name(&self) -> &str {
        SEND_FILE
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: SEND_FILE.into(),
            description: "Post a file in this conversation, where people can open or download it: text you \
                write (content, with name), or a file under the working directory (path) as it is. Use it \
                for a long report, a table or an image; your final answer still goes in your reply. A file \
                from the working directory is posted unchanged: do not post one that may hold credentials."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The file's name as people see it, with its extension, such as report.md. Required with content; the file's own name with path.",
                    },
                    "content": {"type": "string", "description": "The text of the file."},
                    "path": {
                        "type": "string",
                        "description": "A file under the working directory, relative to it or absolute.",
                    },
                },
                "additionalProperties": false,
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = parse_args(&args)?;
        let file = match (args.content, args.path) {
            (Some(content), None) => written(args.name, content)?,
            (None, Some(path)) => from_disk(args.name, &path, &ctx.cwd).await?,
            _ => return Err("give content (with name) or path, not both".into()),
        };
        let size = file.bytes.len();
        let posted = tokio::time::timeout(POST_TIMEOUT, self.channel.send_file(&self.to, &file));
        match unless_cancelled(ctx, posted).await {
            None => Err(format!("the post of {} was cancelled", file.name)),
            Some(Err(_)) => Err(format!(
                "posting {} took longer than {} seconds; it may have been posted or not",
                file.name,
                POST_TIMEOUT.as_secs()
            )),
            Some(Ok(Err(error))) => Err(format!("cannot post {}: {error}", file.name)),
            Some(Ok(Ok(()))) => Ok(ToolOutput::ok(format!(
                "Posted {} ({size} bytes) in the conversation.",
                file.name
            ))),
        }
    }
}

/// The file of the text `content`, named `name`.
fn written(name: Option<String>, content: String) -> Result<Attachment, String> {
    let name = name.ok_or("name is required with content")?;
    checked_name(&name)?;
    within_size(content.len())?;
    Ok(Attachment {
        name,
        bytes: content.into_bytes(),
    })
}

/// The file at `path`, under the working directory `cwd`, named `name` or its own name.
async fn from_disk(name: Option<String>, path: &str, cwd: &Path) -> Result<Attachment, String> {
    let wanted = cwd.join(path);
    let real = tokio::fs::canonicalize(&wanted)
        .await
        .map_err(|error| format!("cannot open {path}: {error}"))?;
    let root = tokio::fs::canonicalize(cwd)
        .await
        .map_err(|error| format!("cannot find the working directory: {error}"))?;
    if !real.starts_with(&root) {
        return Err(format!(
            "{path} is not under the working directory {}; only files there can be posted",
            root.display()
        ));
    }
    let metadata = tokio::fs::metadata(&real)
        .await
        .map_err(|error| format!("cannot open {path}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{path} is not a file"));
    }
    // A size beyond this platform's addresses is too large anyway.
    within_size(usize::try_from(metadata.len()).unwrap_or(usize::MAX))?;
    let name = match name {
        Some(name) => name,
        None => real
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{path} has no name to post it under; give name"))?
            .to_string(),
    };
    checked_name(&name)?;
    let bytes = tokio::fs::read(&real)
        .await
        .map_err(|error| format!("cannot read {path}: {error}"))?;
    // Checked again: the file may have grown meanwhile.
    within_size(bytes.len())?;
    Ok(Attachment { name, bytes })
}

/// Refuse a name that is empty, names a directory too, or holds a control character.
fn checked_name(name: &str) -> Result<(), String> {
    let bad = name.trim().is_empty() || name.contains(['/', '\\']) || name.chars().any(char::is_control);
    if bad {
        return Err(format!(
            "{name:?} is not a file name: give one like report.md, without directories"
        ));
    }
    Ok(())
}

/// Refuse a file of more than [`MAX_FILE_BYTES`] bytes.
fn within_size(bytes: usize) -> Result<(), String> {
    if bytes > MAX_FILE_BYTES {
        return Err(format!(
            "the file has {bytes} bytes; at most {MAX_FILE_BYTES} can be posted"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{ConversationId, ImError, Inlet, MessageRef, Post, Replacement, ThreadId};

    /// A channel that keeps what is posted as files, or refuses them for `refusal`.
    #[derive(Default)]
    struct Files {
        posted: Mutex<Vec<(Destination, Attachment)>>,
        refusal: Option<String>,
    }

    #[async_trait]
    impl ImChannel for Files {
        fn platform(&self) -> &str {
            "slack"
        }

        async fn run(&self, _inlet: Inlet, _cancel: &CancellationToken) -> Result<(), ImError> {
            Ok(())
        }

        async fn send(&self, _to: &Destination, _message: &Post) -> Result<MessageRef, ImError> {
            Err(ImError::Transport("no text here".into()))
        }

        async fn update(&self, _posted: &MessageRef, _replacement: &Replacement) -> Result<(), ImError> {
            Ok(())
        }

        async fn send_file(&self, to: &Destination, file: &Attachment) -> Result<(), ImError> {
            if let Some(refusal) = &self.refusal {
                return Err(ImError::Transport(refusal.clone()));
            }
            self.posted.lock().unwrap().push((to.clone(), file.clone()));
            Ok(())
        }
    }

    /// The thread t1 of slack:C1.
    fn thread() -> Destination {
        Destination {
            conversation: ConversationId("C1".into()),
            thread: Some(ThreadId("t1".into())),
        }
    }

    /// `send_file` of a session in the thread t1 of slack:C1 on `channel`.
    fn tool(channel: &Arc<Files>) -> SendFile {
        let channel: Arc<dyn ImChannel> = channel.clone();
        SendFile { channel, to: thread() }
    }

    async fn call(tool: &SendFile, args: Value, cwd: &Path) -> Result<ToolOutput, String> {
        tool.execute(args, &ToolContext::new(cwd.to_path_buf(), CancellationToken::new()))
            .await
    }

    #[tokio::test]
    async fn text_it_writes_is_posted_in_the_conversation_under_its_name() {
        let channel = Arc::new(Files::default());
        let dir = tempfile::tempdir().unwrap();
        let output = call(
            &tool(&channel),
            json!({"name": "report.md", "content": "# Report"}),
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(output.content, "Posted report.md (8 bytes) in the conversation.");
        let posted = channel.posted.lock().unwrap();
        assert_eq!(
            posted.as_slice(),
            [(
                thread(),
                Attachment {
                    name: "report.md".into(),
                    bytes: b"# Report".to_vec()
                }
            )]
        );
    }

    #[tokio::test]
    async fn a_file_of_the_working_directory_is_posted_as_it_is_and_no_other() {
        let channel = Arc::new(Files::default());
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work");
        std::fs::create_dir_all(cwd.join("out")).unwrap();
        std::fs::write(cwd.join("out/chart.png"), [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(root.path().join("secret.txt"), "key").unwrap();
        let sender = tool(&channel);
        call(&sender, json!({"path": "out/chart.png"}), &cwd).await.unwrap();
        let outside = call(&sender, json!({"path": "../secret.txt"}), &cwd).await.unwrap_err();
        assert!(outside.contains("is not under the working directory"), "{outside}");
        let posted = channel.posted.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].1.name, "chart.png");
        assert_eq!(posted[0].1.bytes, [0x89, b'P', b'N', b'G']);
    }

    #[tokio::test]
    async fn what_cannot_be_posted_is_said_to_the_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let channel = Arc::new(Files::default());
        let sender = tool(&channel);
        let cases = [
            (json!({"name": "a.md"}), "give content (with name) or path"),
            (json!({"name": "a.md", "content": "x", "path": "a.md"}), "not both"),
            (json!({"content": "x"}), "name is required with content"),
            (json!({"name": "../a.md", "content": "x"}), "is not a file name"),
            (json!({"path": "sub"}), "is not a file"),
            (json!({"path": "missing.txt"}), "cannot open missing.txt"),
            (json!({"name": "a.md", "text": "x"}), "unknown field `text`"),
        ];
        for (args, expected) in cases {
            let error = call(&sender, args.clone(), dir.path()).await.unwrap_err();
            assert!(error.contains(expected), "{args}: {error}");
        }
        assert!(channel.posted.lock().unwrap().is_empty());

        let refusing = Arc::new(Files {
            refusal: Some("slack files.getUploadURLExternal: missing_scope".into()),
            ..Files::default()
        });
        let error = call(&tool(&refusing), json!({"name": "a.md", "content": "x"}), dir.path())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "cannot post a.md: slack files.getUploadURLExternal: missing_scope"
        );
    }

    #[test]
    fn each_session_of_a_conversation_gets_the_tool_and_no_other_session_does() {
        let channel: Arc<dyn ImChannel> = Arc::new(Files::default());
        let tools = SendFiles::new(&[channel]);
        let mut header = SessionHeader::new(Path::new("/w"), "m");
        assert!(tools.tools(&header).is_empty());
        let key = SessionKey {
            platform: "slack".into(),
            conversation: ConversationId("C1".into()),
            thread: Some(ThreadId("t1".into())),
        };
        header.labels = key.labels();
        assert_eq!(tools.tools(&header).len(), 1);
        header.labels = SessionKey {
            platform: "teams".into(),
            ..key
        }
        .labels();
        assert!(tools.tools(&header).is_empty());
        assert_eq!(tools.names(), ["send_file"]);
    }
}
