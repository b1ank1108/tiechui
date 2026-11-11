use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// Accepted only in user configuration; the CLI `--mode` overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    /// Omission uses the built-in defaults; an explicit empty list asks for everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRule>>,
    #[serde(skip)]
    #[schemars(skip)]
    pub project_rules: Vec<PermissionRule>,
    #[serde(skip)]
    #[schemars(skip)]
    pub project_invalid: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Rules decide; unmatched actions ask.
    #[default]
    Rules,
    /// Workspace edits pass; a model classifier reviews the remaining asks.
    Auto,
    /// Approves every ask except fail-closed project rules, which are denied without prompting.
    Yolo,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub surface: String,
    pub pattern: String,
    pub action: PermissionAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PermissionRule {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.surface.is_empty()
            || self.surface.trim() != self.surface
            || self.surface.chars().any(|c| c < ' ')
        {
            return Err("surface must be a nonempty name");
        }
        if ["path_", "external_directory_"]
            .iter()
            .any(|prefix| self.surface.starts_with(prefix))
            && !matches!(
                self.surface.as_str(),
                "path_read" | "path_write" | "external_directory_read" | "external_directory_write"
            )
        {
            return Err("surface has an unknown path direction");
        }
        if self.pattern.is_empty() || self.pattern.chars().any(|c| c < ' ') {
            return Err("pattern must be a nonempty pattern");
        }
        if self.surface == "*" && self.pattern != "*" {
            return Err("the universal surface requires pattern = \"*\"");
        }
        if let Some(reason) = &self.reason
            && (self.action != PermissionAction::Deny || reason.trim().is_empty())
        {
            return Err("reason requires a deny rule and nonempty text");
        }
        Ok(())
    }
}
