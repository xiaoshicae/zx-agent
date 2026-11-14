//! Slash commands stay in the frontend and never become model input.

pub(super) enum Command {
    Empty,
    Prompt(String),
    New,
    Model(Option<String>),
    /// `/reasoning`: show; `/reasoning <level>`: change; `/reasoning default`: leave it to the
    /// server.
    Reasoning(Option<String>),
    Compact,
    /// Continue without new input.
    Continue,
    Status,
    /// A command an extension registered, with the text after its name.
    Extension {
        name: String,
        args: String,
    },
    Quit,
    Help,
    Invalid(String),
}

/// Parse one line. `extensions` are the names of the commands extensions registered, tried
/// after the built-in ones (pi: built-in commands are matched first).
pub(super) fn parse(line: String, extensions: &[(String, String)]) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    if !trimmed.starts_with('/') {
        return Command::Prompt(line);
    }
    let (name, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
    let rest = rest.trim();
    if let Some(extension) = extensions
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|registered| !is_builtin(name) && name[1..] == **registered)
    {
        return Command::Extension {
            name: extension.to_string(),
            args: rest.to_string(),
        };
    }
    match name {
        "/new" if rest.is_empty() => Command::New,
        "/quit" if rest.is_empty() => Command::Quit,
        "/help" if rest.is_empty() => Command::Help,
        "/compact" if rest.is_empty() => Command::Compact,
        "/continue" if rest.is_empty() => Command::Continue,
        "/status" if rest.is_empty() => Command::Status,
        "/model" => Command::Model((!rest.is_empty()).then(|| rest.to_string())),
        "/reasoning" => Command::Reasoning((!rest.is_empty()).then(|| rest.to_string())),
        _ => Command::Invalid(format!("unknown command or arguments: {name}; use /help")),
    }
}

/// The commands chat handles itself, which an extension's command cannot shadow.
const BUILTIN: [&str; 8] = [
    "/new",
    "/quit",
    "/help",
    "/compact",
    "/continue",
    "/status",
    "/model",
    "/reasoning",
];

fn is_builtin(name: &str) -> bool {
    BUILTIN.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Command {
        super::parse(line.into(), &[])
    }

    #[test]
    fn ordinary_input_keeps_its_unicode_and_whitespace() {
        assert!(matches!(parse("  你好  "), Command::Prompt(text) if text == "  你好  "));
        assert!(matches!(parse("第一行\n/quit"), Command::Prompt(text) if text == "第一行\n/quit"));
        assert!(matches!(parse(" \t"), Command::Empty));
    }

    #[test]
    fn extension_commands_come_after_the_built_in_ones_and_keep_their_arguments() {
        let registered = [
            ("jira".to_string(), "Fetch an issue".to_string()),
            ("quit".to_string(), "Cannot shadow /quit".to_string()),
        ];
        let parse = |line: &str| super::parse(line.into(), &registered);
        assert!(matches!(parse("/jira ABC-1  two words"),
            Command::Extension { name, args } if name == "jira" && args == "ABC-1  two words"));
        assert!(matches!(parse("/jira"), Command::Extension { name, args } if name == "jira" && args.is_empty()));
        assert!(matches!(parse("/quit"), Command::Quit));
        assert!(matches!(parse("/jiraa"), Command::Invalid(_)));
    }

    #[test]
    fn slash_commands_validate_arguments_instead_of_forwarding_them_to_the_model() {
        for invalid in [
            "/new extra",
            "/quit extra",
            "/compact extra",
            "/continue extra",
            "/status extra",
            "/sessions",
            "/resume 0b7c",
            "/fork",
            "/tree",
            "/branch 1a2b",
            "/queue all",
            "/unknown",
        ] {
            assert!(matches!(parse(invalid), Command::Invalid(_)), "{invalid}");
        }
        assert!(matches!(parse("/model next"), Command::Model(Some(model)) if model == "next"));
        assert!(matches!(parse("/model"), Command::Model(None)));
        assert!(matches!(parse("/reasoning high"), Command::Reasoning(Some(level)) if level == "high"));
        assert!(matches!(parse("/reasoning"), Command::Reasoning(None)));
        assert!(matches!(parse("/status"), Command::Status));
    }
}
