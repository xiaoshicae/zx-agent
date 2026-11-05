//! Slash commands stay in the frontend and never become model input.

pub(super) enum Command {
    Empty,
    Prompt(String),
    New,
    Model(Option<String>),
    Compact,
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
        "/model" => Command::Model((!rest.is_empty()).then(|| rest.to_string())),
        _ => Command::Invalid(format!("unknown command or arguments: {name}; use /help")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_input_keeps_its_unicode_and_whitespace() {
        assert!(matches!(parse("  你好  ".into()), Command::Prompt(text) if text == "  你好  "));
        assert!(matches!(parse(" \t".into()), Command::Empty));
    }

    #[test]
    fn slash_commands_validate_arguments_instead_of_forwarding_them_to_the_model() {
        for invalid in ["/new extra", "/quit extra", "/compact extra", "/unknown"] {
            assert!(matches!(parse(invalid.into()), Command::Invalid(_)), "{invalid}");
        }
        assert!(matches!(parse("/model next".into()), Command::Model(Some(model)) if model == "next"));
        assert!(matches!(parse("/model".into()), Command::Model(None)));
    }
}
