//! Slash commands stay in the frontend and never become model input.

use super::interaction::RequestId;
use zx_agent::interaction::Approval;

pub(super) enum Command {
    Empty,
    Prompt(String),
    New,
    Model(Option<String>),
    Compact,
    Quit,
    Help,
    Approval { id: RequestId, decision: Approval },
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
        "/approve" => {
            let mut parts = rest.split_whitespace();
            let id = parts.next().and_then(|id| id.parse::<u64>().ok());
            let choice = parts.next();
            match (id, choice, parts.next()) {
                (Some(id), Some("once"), None) => Command::Approval {
                    id: RequestId(id),
                    decision: Approval::AllowOnce,
                },
                (Some(id), Some("session"), None) => Command::Approval {
                    id: RequestId(id),
                    decision: Approval::AllowSession,
                },
                _ => Command::Invalid("usage: /approve <id> once|session".into()),
            }
        }
        "/deny" => {
            let (id, reason) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            match id.parse::<u64>() {
                Ok(id) => Command::Approval {
                    id: RequestId(id),
                    decision: Approval::Deny(if reason.trim().is_empty() {
                        "declined by the user".into()
                    } else {
                        reason.trim().to_string()
                    }),
                },
                Err(_) => Command::Invalid("usage: /deny <id> [reason]".into()),
            }
        }
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
    fn approval_commands_require_an_id_and_an_explicit_scope() {
        assert!(matches!(
            parse("/approve 7 once".into()),
            Command::Approval {
                id: RequestId(7),
                decision: Approval::AllowOnce
            }
        ));
        assert!(matches!(
            parse("/approve 8 session".into()),
            Command::Approval {
                id: RequestId(8),
                decision: Approval::AllowSession
            }
        ));
        for invalid in [
            "/approve once",
            "/approve no once",
            "/approve 7 maybe",
            "/approve 7 once extra",
        ] {
            assert!(matches!(parse(invalid.into()), Command::Invalid(_)), "{invalid}");
        }
    }

    #[test]
    fn denial_preserves_the_users_reason_and_cannot_be_confused_with_a_prompt() {
        assert!(
            matches!(parse("/deny 9 不允许 修改".into()), Command::Approval { id: RequestId(9), decision: Approval::Deny(reason) } if reason == "不允许 修改")
        );
        assert!(matches!(parse("/deny".into()), Command::Invalid(_)));
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
