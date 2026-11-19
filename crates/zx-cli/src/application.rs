//! The application a subcommand runs on, read and connected once for the process, and the
//! sessions of its agents, with what opening them has to say on stderr.

use anyhow::Result;

use zx_agent::Session;
use zx_agent::transcript::Transcript;
use zx_app::config::Builtin;
use zx_app::{Application, ConfiguredAgent, NoModel, Overrides};

use crate::process::eprint_line;
use crate::render::visible;

/// What the process shares, read and connected once for sessions of `builtin`, with its
/// startup notices written to stderr. Without a model, the error names the program's ways to
/// give one.
pub(crate) async fn connect(overrides: Overrides, builtin: Builtin) -> Result<Application> {
    let app = Application::from_environment(overrides, builtin)
        .await
        .map_err(with_model_hint)?;
    // A notice may quote an MCP server, such as the protocol version it settled on.
    for notice in app.notices() {
        eprint_line(&visible(notice));
    }
    Ok(app)
}

/// `error`, or, when no model was given, the ways to give one: `--model`, over `ZX_MODEL`,
/// which the application reads, over config.toml.
fn with_model_hint(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<NoModel>() {
        Some(no_model) => anyhow::anyhow!(
            "no model: pass --model, set ZX_MODEL, or set model in {}",
            no_model.config_file
        ),
        None => error,
    }
}

/// The stored session `id` of `agent`, opened. A cut-short final record, which the next
/// record replaces, is reported on stderr.
pub(crate) async fn open_session(agent: &ConfiguredAgent, id: &str) -> Result<Session> {
    let session = agent.open(id).await?;
    let cut_off = session.with_transcript(Transcript::incomplete_tail_bytes)?;
    if cut_off > 0 {
        eprint_line(&format!(
            "[session] recovered incomplete final record; its {cut_off} bytes are cut off before the next record"
        ));
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_names_the_programs_ways_to_give_one() {
        let no_model = NoModel {
            config_file: "/home/me/.zx-agent/config.toml".into(),
        };
        assert_eq!(
            format!("{:#}", with_model_hint(no_model.into())),
            "no model: pass --model, set ZX_MODEL, or set model in /home/me/.zx-agent/config.toml"
        );
        let other = with_model_hint(anyhow::anyhow!("something else"));
        assert_eq!(format!("{other:#}"), "something else");
    }
}
