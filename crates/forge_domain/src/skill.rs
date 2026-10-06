use std::path::PathBuf;

use derive_setters::Setters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Represents a reusable skill with a name, file path, and prompt content
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Setters, JsonSchema)]
#[setters(strip_option, into)]
pub struct Skill {
    /// Name of the skill
    pub name: String,

    /// File path to the skill markdown file
    pub path: Option<PathBuf>,

    /// Content/prompt loaded from the markdown file
    pub command: String,

    /// Description of the skill
    pub description: String,

    /// List of resource files in the skill directory
    pub resources: Vec<PathBuf>,

    /// Hidden from the model's skill listing; the user invokes it (T21).
    #[serde(default)]
    pub disable_model_invocation: bool,

    /// Tools the skill activates when loaded (intersected with the agent's
    /// policy).
    #[serde(default)]
    pub tools: Vec<String>,

    /// Globs over workspace-relative paths; the first time a matching file is
    /// read or edited in a conversation, the model is offered the skill.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Skill {
    /// Creates a new Skill with required fields
    ///
    /// # Arguments
    ///
    /// * `name` - The name identifier for the skill
    /// * `prompt` - The skill prompt content
    /// * `description` - A brief description of the skill
    pub fn new(
        name: impl Into<String>,
        prompt: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            path: None,
            command: prompt.into(),
            description: description.into(),
            resources: Vec::new(),
            disable_model_invocation: false,
            tools: Vec::new(),
            paths: Vec::new(),
        }
    }

    /// Whether `relative` (a workspace-relative path) matches one of the
    /// skill's `paths` globs.
    pub fn matches_path(&self, relative: &str) -> bool {
        self.paths.iter().any(|glob| {
            glob::Pattern::new(glob)
                .map(|pattern| {
                    pattern.matches_with(
                        relative,
                        glob::MatchOptions {
                            require_literal_separator: false,
                            ..Default::default()
                        },
                    )
                })
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_skill_creation() {
        // Fixture
        let fixture = Skill::new(
            "code_review",
            "Review this code",
            "A skill for reviewing code quality",
        )
        .path("/skills/code_review.md");

        // Act
        let actual = (
            fixture.name.clone(),
            fixture.path.clone(),
            fixture.command.clone(),
            fixture.description.clone(),
        );

        // Assert
        let expected = (
            "code_review".to_string(),
            Some("/skills/code_review.md".into()),
            "Review this code".to_string(),
            "A skill for reviewing code quality".to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_skill_with_setters() {
        // Fixture
        let fixture = Skill::new("test", "prompt", "desc")
            .path("/path")
            .name("updated_name")
            .path("/updated/path")
            .command("updated prompt")
            .description("updated description");

        // Act
        let actual = fixture;

        // Assert
        let expected = Skill::new("updated_name", "updated prompt", "updated description")
            .path("/updated/path");
        assert_eq!(actual, expected);
    }
}
