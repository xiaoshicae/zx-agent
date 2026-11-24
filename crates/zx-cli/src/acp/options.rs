//! The session's configuration options, which clients show as selectors: the model, one of
//! those config.toml lists or the one the session uses, and the reasoning level. They come
//! with the response that opens a session, and `session/set_config_option` changes one while
//! the session is idle, as chat's `/model` and `/reasoning` do.

use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
};
use agent_client_protocol::{Error, ErrorCode};
use anyhow::Result;
use zx_agent::Session;
use zx_agent::ai::{Provider, Reasoning};
use zx_agent::session::SessionError;

use super::models::Models;

/// The option that picks the model.
const MODEL: &str = "model";
/// The option that sets the reasoning level.
const REASONING: &str = "reasoning";
/// The reasoning level that leaves the level to the server.
const SERVER_DEFAULT: &str = "default";

/// The options of `session` as they are now; an error when it is running or closed.
pub(super) fn of(session: &Session, models: &Models) -> Result<Vec<SessionConfigOption>, SessionError> {
    Ok(vec![
        model_option(&session.model()?, models),
        reasoning_option(session.reasoning()?),
    ])
}

/// A change the client asks for: the option, and the value it picked.
pub(super) struct Change<'a> {
    pub(super) option: &'a str,
    pub(super) value: &'a SessionConfigOptionValue,
}

/// Make `change` to `session`, whose model is asked through `provider`, and return the
/// options as they are then. A value that is not one of the option's choices, an option the
/// agent does not have and a running session are refused, with nothing changed.
pub(super) fn set(
    session: &Session,
    provider: &Arc<dyn Provider>,
    models: &Models,
    change: &Change<'_>,
) -> Result<Vec<SessionConfigOption>> {
    let Some(value) = change.value.as_value_id().map(|value| value.0.as_ref()) else {
        return Err(invalid(format!("option {:?} takes one of its choices", change.option)).into());
    };
    match change.option {
        MODEL => {
            if !models.lists(value) && session.model().map_err(refusal)? != value {
                return Err(invalid(format!("unknown model {value:?}: config.toml lists no such model")).into());
            }
            session.set_model(Arc::clone(provider), value).map_err(refusal)?;
        }
        REASONING => {
            let level = if value == SERVER_DEFAULT {
                None
            } else {
                Some(value.parse::<Reasoning>().map_err(invalid)?)
            };
            session.set_reasoning(level).map_err(refusal)?;
        }
        other => return Err(invalid(format!("unknown option {other:?}")).into()),
    }
    of(session, models).map_err(refusal)
}

/// The model option: `current` among the models config.toml lists.
fn model_option(current: &str, models: &Models) -> SessionConfigOption {
    let mut ids: Vec<&str> = models.ids().collect();
    if !models.lists(current) {
        ids.push(current);
        ids.sort_unstable();
    }
    let choices: Vec<SessionConfigSelectOption> = ids
        .into_iter()
        .map(|id| SessionConfigSelectOption::new(id.to_string(), id))
        .collect();
    SessionConfigOption::select(MODEL, "Model", current.to_string(), choices)
        .description("The model the next prompts ask".to_string())
        .category(SessionConfigOptionCategory::Model)
}

/// The reasoning option: each level, and the server's default.
fn reasoning_option(current: Option<Reasoning>) -> SessionConfigOption {
    let default = SessionConfigSelectOption::new(SERVER_DEFAULT, SERVER_DEFAULT)
        .description("Send no reasoning setting; the server decides".to_string());
    let choices: Vec<SessionConfigSelectOption> = std::iter::once(default)
        .chain(
            Reasoning::ALL
                .into_iter()
                .map(|level| SessionConfigSelectOption::new(level.as_str(), level.as_str())),
        )
        .collect();
    let current = current.map_or(SERVER_DEFAULT, Reasoning::as_str);
    SessionConfigOption::select(REASONING, "Reasoning", current, choices)
        .description("How much the model reasons before it answers".to_string())
        .category(SessionConfigOptionCategory::ThoughtLevel)
}

/// A request whose parameters the agent cannot use.
fn invalid(message: String) -> Error {
    Error::new(ErrorCode::INVALID_PARAMS, message)
}

/// The error for a session that cannot change now: a turn of it is running, or it closed.
fn refusal(error: SessionError) -> anyhow::Error {
    match error {
        SessionError::Busy => invalid("a turn is running; change the option once it has ended".to_string()).into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_client_protocol::v1::schema::{SessionConfigKind, SessionConfigSelectOptions};
    use zx_agent::ai::ModelMeta;

    use super::*;

    /// The current value and the choices of a select option.
    fn select(option: &SessionConfigOption) -> (String, Vec<String>) {
        let SessionConfigKind::Select(select) = &option.kind else {
            panic!("not a select: {option:?}");
        };
        let SessionConfigSelectOptions::Ungrouped(choices) = &select.options else {
            panic!("grouped: {option:?}");
        };
        (
            select.current_value.0.to_string(),
            choices.iter().map(|choice| choice.value.0.to_string()).collect(),
        )
    }

    #[test]
    fn the_model_option_offers_the_listed_models_and_the_one_in_use() {
        let models = Models::new(&HashMap::from([
            ("b".to_string(), ModelMeta::default()),
            ("a".to_string(), ModelMeta::default()),
        ]));
        let listed = model_option("b", &models);
        assert_eq!(listed.category, Some(SessionConfigOptionCategory::Model));
        assert_eq!(
            select(&listed),
            ("b".to_string(), vec!["a".to_string(), "b".to_string()])
        );
        let unlisted = model_option("aa", &models);
        assert_eq!(select(&unlisted).1, ["a", "aa", "b"]);
    }

    #[test]
    fn the_reasoning_option_offers_every_level_and_the_server_default() {
        let option = reasoning_option(None);
        assert_eq!(option.category, Some(SessionConfigOptionCategory::ThoughtLevel));
        let (current, choices) = select(&option);
        assert_eq!(current, "default");
        assert_eq!(choices, ["default", "off", "minimal", "low", "medium", "high", "xhigh"]);
        assert_eq!(select(&reasoning_option(Some(Reasoning::High))).0, "high");
    }
}
