//! System prompt: instructions followed by generated sections (pi: coding-agent system-prompt.ts).

use crate::ai::{PREAMBLE, SystemPrompt};

/// Where commands run, as the executor that runs them describes it.
pub const ENVIRONMENT: &str = "environment";
/// Repository instructions such as AGENTS.md.
pub const PROJECT_CONTEXT: &str = "project_context";
/// The catalog of skills the model can read.
pub const AVAILABLE_SKILLS: &str = "available_skills";
/// The sections zx-agent generates after the preamble, in this order. Extensions may not
/// use these names, present or not, nor the preamble's.
pub const GENERATED_SECTIONS: [&str; 3] = [ENVIRONMENT, PROJECT_CONTEXT, AVAILABLE_SKILLS];

/// `instructions` as the preamble, followed by every non-empty section. Sections are wrapped
/// in tags of their names, so the model can tell generated facts from instructions.
pub fn build_system_prompt(instructions: &str, sections: &[(&str, &str)]) -> SystemPrompt {
    sections.iter().fold(
        SystemPrompt::default().with_section(PREAMBLE, instructions),
        |prompt, (name, content)| prompt.with_section(name, content),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_non_empty_sections_in_tags() {
        let prompt = build_system_prompt(
            "Do the task.\n",
            &[("environment", "OS: linux"), ("project_context", "")],
        );
        assert_eq!(
            prompt.render(),
            "Do the task.\n\n<environment>\nOS: linux\n</environment>"
        );
    }
}
