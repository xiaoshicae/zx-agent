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
    /// List the sessions in the session directory.
    Sessions,
    /// Switch to the session whose ID starts with this prefix.
    Resume(String),
    Quit,
    Help,
    Invalid(String),
}

pub(super) fn parse(line: String) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    if !trimmed.starts_with('/') {
        return Command::Prompt(line);
    }
    let (name, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
    let rest = rest.trim();
    match name {
        "/new" if rest.is_empty() => Command::New,
        "/quit" if rest.is_empty() => Command::Quit,
        "/help" if rest.is_empty() => Command::Help,
        "/compact" if rest.is_empty() => Command::Compact,
        "/continue" if rest.is_empty() => Command::Continue,
        "/status" if rest.is_empty() => Command::Status,
        "/sessions" if rest.is_empty() => Command::Sessions,
        "/resume" if rest.is_empty() => {
            Command::Invalid("give a session ID: /resume <ID>; /sessions lists them".into())
        }
        "/resume" => Command::Resume(rest.to_string()),
        "/model" => Command::Model((!rest.is_empty()).then(|| rest.to_string())),
        "/reasoning" => Command::Reasoning((!rest.is_empty()).then(|| rest.to_string())),
        _ => Command::Invalid(format!("unknown command or arguments: {name}; use /help")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_input_keeps_its_unicode_and_whitespace() {
        assert!(matches!(parse("  你好  ".into()), Command::Prompt(text) if text == "  你好  "));
        assert!(matches!(parse("第一行\n/quit".into()), Command::Prompt(text) if text == "第一行\n/quit"));
        assert!(matches!(parse(" \t".into()), Command::Empty));
    }

    #[test]
    fn slash_commands_validate_arguments_instead_of_forwarding_them_to_the_model() {
        for invalid in [
            "/new extra",
            "/quit extra",
            "/compact extra",
            "/continue extra",
            "/status extra",
            "/sessions extra",
            "/resume",
            "/unknown",
        ] {
            assert!(matches!(parse(invalid.into()), Command::Invalid(_)), "{invalid}");
        }
        assert!(matches!(parse("/model next".into()), Command::Model(Some(model)) if model == "next"));
        assert!(matches!(parse("/model".into()), Command::Model(None)));
        assert!(matches!(parse("/reasoning high".into()), Command::Reasoning(Some(level)) if level == "high"));
        assert!(matches!(parse("/reasoning".into()), Command::Reasoning(None)));
        assert!(matches!(parse("/status".into()), Command::Status));
        assert!(matches!(parse("/resume 0b7c".into()), Command::Resume(id) if id == "0b7c"));
    }
}
