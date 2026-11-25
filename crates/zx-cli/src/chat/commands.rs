//! Slash commands stay in chat and never become model input.

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
    Quit,
    Help,
    Invalid(String),
}

/// Parse one line.
pub(super) fn parse(line: String) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    if !trimmed.starts_with('/') {
        return Command::Prompt(line);
    }
    let (name, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
    builtin(name, rest.trim()).unwrap_or_else(|| unknown(name))
}

/// The command chat handles itself that `name` names, with `rest`, the text after it: an
/// invalid one for text a command does not take. `None` for a name that is no command.
fn builtin(name: &str, rest: &str) -> Option<Command> {
    let bare = |command| if rest.is_empty() { command } else { unknown(name) };
    let text = || (!rest.is_empty()).then(|| rest.to_string());
    Some(match name {
        "/new" => bare(Command::New),
        "/quit" => bare(Command::Quit),
        "/help" => bare(Command::Help),
        "/compact" => bare(Command::Compact),
        "/continue" => bare(Command::Continue),
        "/status" => bare(Command::Status),
        "/model" => Command::Model(text()),
        "/reasoning" => Command::Reasoning(text()),
        _ => return None,
    })
}

fn unknown(name: &str) -> Command {
    Command::Invalid(format!("unknown command or arguments: {name}; use /help"))
}

/// Whether `text` is `/quit` or `/new`, which end the operation that runs; any other line
/// answers the approval a call waits for.
pub(super) fn ends_the_operation(text: &str) -> bool {
    matches!(builtin(text.trim(), ""), Some(Command::Quit | Command::New))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Command {
        super::parse(line.into())
    }

    #[test]
    fn ordinary_input_keeps_its_unicode_and_whitespace() {
        assert!(matches!(parse("  你好  "), Command::Prompt(text) if text == "  你好  "));
        assert!(matches!(parse("第一行\n/quit"), Command::Prompt(text) if text == "第一行\n/quit"));
        assert!(matches!(parse(" \t"), Command::Empty));
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
